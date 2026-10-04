//! OpenAI Chat Completions <-> canonical (`POST /v1/chat/completions`).

use futures::stream::BoxStream;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};

use super::common::{arguments_text, decode, ignored_params, parse_arguments, positive, stops, text_of, tool_params, Content, Stop};
use crate::canonical::{
    new_call_id, CanonicalRequest, CanonicalResponse, Event, Finish, ToolCall, ToolChoice, ToolResult, ToolSpec, Usage,
};
use crate::errors::{ClientError, Error};

const IGNORED: [&str; 15] = [
    "temperature",
    "top_p",
    "seed",
    "presence_penalty",
    "frequency_penalty",
    "logit_bias",
    "reasoning_effort",
    "parallel_tool_calls",
    "service_tier",
    "store",
    "metadata",
    "prediction",
    "audio",
    "modalities",
    "verbosity",
];

#[derive(Deserialize)]
struct Request {
    messages: Option<Vec<Message>>,
    #[serde(default)]
    tools: Vec<Tool>,
    #[serde(default)]
    functions: Vec<Function>,
    tool_choice: Option<Choice>,
    function_call: Option<Choice>,
    response_format: Option<ResponseFormat>,
    stop: Option<Stop>,
    max_tokens: Option<i64>,
    max_completion_tokens: Option<i64>,
    n: Option<u64>,
    #[serde(default)]
    logprobs: Option<bool>,
    top_logprobs: Option<u64>,
}

#[derive(Deserialize)]
#[serde(expecting = "a message object")]
struct Message {
    role: Option<String>,
    content: Option<Content>,
    #[serde(default)]
    tool_calls: Vec<MessageToolCall>,
    function_call: Option<Function>,
    name: Option<String>,
    tool_call_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(expecting = "a tool call object")]
struct MessageToolCall {
    id: Option<String>,
    function: Option<Function>,
}

/// A function: its definition (tools, functions) or a call (name + arguments).
#[derive(Deserialize, Default)]
#[serde(expecting = "a function object")]
struct Function {
    name: Option<String>,
    description: Option<String>,
    parameters: Option<Value>,
    arguments: Option<Value>,
}

#[derive(Deserialize)]
#[serde(expecting = "a tool object")]
struct Tool {
    #[serde(rename = "type")]
    kind: Option<String>,
    function: Option<Function>,
}

#[derive(Deserialize)]
#[serde(untagged, expecting = "\"auto\", \"none\", \"required\" or an object naming a function")]
enum Choice {
    Mode(String),
    Named { function: Option<Function>, name: Option<String> },
}

#[derive(Deserialize)]
#[serde(expecting = "a response_format object")]
struct ResponseFormat {
    #[serde(rename = "type")]
    kind: String,
    json_schema: Option<JsonSchemaFormat>,
}

#[derive(Deserialize)]
struct JsonSchemaFormat {
    schema: Option<Value>,
}

/// The request as a canonical request (`max_tokens` included).
pub fn to_canonical(body: &Value) -> Result<CanonicalRequest, ClientError> {
    let r: Request = decode(body)?;
    if r.logprobs == Some(true) || r.top_logprobs.is_some_and(|n| n > 0) {
        return Err(ClientError::new("logprobs are not available from this backend", "unsupported_parameter"));
    }
    if r.n.is_some_and(|n| n != 1) {
        return Err(ClientError::new("n > 1 is not supported (one generation per request)", "unsupported_parameter"));
    }
    let messages = r
        .messages
        .filter(|m| !m.is_empty())
        .ok_or_else(|| ClientError::new("'messages' is required and must be a non-empty list", "missing_messages"))?;
    let mut req = CanonicalRequest { ignored: ignored_params(body, &IGNORED), ..Default::default() };
    for m in messages {
        let role = m.role.unwrap_or_else(|| "user".into());
        match role.as_str() {
            "system" | "developer" => req.system.push(text_of(&m.content, "system")),
            "assistant" => {
                let mut calls: Vec<ToolCall> = m
                    .tool_calls
                    .into_iter()
                    .map(|tc| {
                        let f = tc.function.unwrap_or_default();
                        ToolCall {
                            id: tc.id.filter(|i| !i.is_empty()).unwrap_or_else(new_call_id),
                            name: f.name.unwrap_or_default(),
                            arguments: parse_arguments(f.arguments),
                        }
                    })
                    .collect();
                if let Some(fc) = m.function_call {
                    calls.push(ToolCall { id: new_call_id(), name: fc.name.unwrap_or_default(), arguments: parse_arguments(fc.arguments) });
                }
                req.add("assistant", &text_of(&m.content, "assistant"), calls, vec![]);
            }
            "tool" | "function" => {
                let name = m.name.unwrap_or_default();
                let call_id = m.tool_call_id.filter(|i| !i.is_empty()).unwrap_or_else(|| name.clone());
                let content = text_of(&m.content, "tool");
                req.add("user", "", vec![], vec![ToolResult { call_id, content, name, is_error: false }]);
            }
            other => req.add_text("user", &text_of(&m.content, other)),
        }
    }
    for t in r.tools {
        match t.kind.as_deref().unwrap_or("function") {
            "function" => {
                let f = t.function.unwrap_or_default();
                req.tools.push(ToolSpec::new(
                    f.name.unwrap_or_default(),
                    f.description.unwrap_or_default(),
                    tool_params(f.parameters),
                    false,
                ));
            }
            other => req.ignored.push(format!("tool:{other}")),
        }
    }
    for f in r.functions {
        req.tools.push(ToolSpec::new(f.name.unwrap_or_default(), f.description.unwrap_or_default(), tool_params(f.parameters), false));
    }
    req.tool_choice = match r.tool_choice.or(r.function_call) {
        Some(Choice::Mode(m)) => match m.as_str() {
            "none" => ToolChoice::None,
            "required" => ToolChoice::Required,
            _ => ToolChoice::Auto,
        },
        Some(Choice::Named { function, name }) => match function.and_then(|f| f.name).or(name).filter(|n| !n.is_empty()) {
            Some(n) => ToolChoice::Named(n),
            None => ToolChoice::Auto,
        },
        None => ToolChoice::Auto,
    };
    if let Some(fmt) = r.response_format {
        match fmt.kind.as_str() {
            "json_object" => req.json_schema = Some(json!({"type": "object"})),
            "json_schema" => {
                let schema = fmt.json_schema.and_then(|j| j.schema).filter(|s| s.as_object().is_some_and(|m| !m.is_empty()));
                req.json_schema = Some(schema.unwrap_or_else(|| json!({"type": "object"})));
            }
            _ => {}
        }
    }
    req.stop = stops(r.stop);
    req.max_tokens = positive("max_tokens", r.max_completion_tokens.or(r.max_tokens))?;
    Ok(req)
}

/// Whether a streamed response should end with a usage chunk (`stream_options.include_usage`, default on).
pub fn include_usage(body: &Value) -> bool {
    body.get("stream_options").and_then(|o| o.get("include_usage")).and_then(Value::as_bool).unwrap_or(true)
}

pub fn finish_reason(r: &CanonicalResponse) -> &'static str {
    match r.finish {
        Finish::ToolCalls => "tool_calls",
        Finish::Length => "length",
        Finish::Stop | Finish::StopSequence => "stop",
    }
}

/// Same shape streaming (last chunk) and not.
pub fn usage(u: &Usage) -> Value {
    json!({"prompt_tokens": u.prompt_tokens, "completion_tokens": u.completion_tokens, "total_tokens": u.total_tokens,
           "prompt_tokens_details": {"cached_tokens": 0}, "completion_tokens_details": {"reasoning_tokens": 0}})
}

fn tool_call(c: &ToolCall, index: Option<usize>) -> Value {
    let mut d = json!({"id": c.id, "type": "function", "function": {"name": c.name, "arguments": arguments_text(&c.arguments)}});
    if let Some(i) = index {
        d["index"] = json!(i);
    }
    d
}

pub fn response(r: &CanonicalResponse, cid: &str, created: i64, model: &str) -> Value {
    let content = if !r.text.is_empty() || r.tool_calls.is_empty() { json!(r.text) } else { Value::Null };
    let mut message = json!({"role": "assistant", "content": content, "refusal": null});
    if !r.tool_calls.is_empty() {
        message["tool_calls"] = r.tool_calls.iter().map(|c| tool_call(c, None)).collect();
    }
    json!({"id": cid, "object": "chat.completion", "created": created, "model": model,
           "choices": [{"index": 0, "message": message, "finish_reason": finish_reason(r), "logprobs": null}],
           "usage": usage(&r.usage), "system_fingerprint": r.message_id})
}

pub fn stream(
    events: BoxStream<'static, Result<Event, Error>>,
    cid: String,
    created: i64,
    model: String,
    include_usage: bool,
) -> BoxStream<'static, Result<String, Error>> {
    let chunk = move |delta: Value, finish: Option<&str>, usage: Option<Value>| -> String {
        let mut d = json!({"id": cid, "object": "chat.completion.chunk", "created": created, "model": model,
                           "choices": [{"index": 0, "delta": delta, "finish_reason": finish, "logprobs": null}]});
        if let Some(u) = usage {
            d["usage"] = u;
        }
        format!("data: {d}\n\n")
    };
    Box::pin(async_stream::try_stream! {
        yield chunk(json!({"role": "assistant", "content": ""}), None, None);
        let mut n_calls = 0usize;
        let mut events = events;
        while let Some(ev) = events.next().await {
            match ev? {
                Event::Text(t) => yield chunk(json!({"content": t}), None, None),
                Event::ToolCall(c) => {
                    yield chunk(json!({"tool_calls": [tool_call(&c, Some(n_calls))]}), None, None);
                    n_calls += 1;
                }
                Event::Keepalive => yield ": keepalive\n\n".to_string(),
                Event::Done(r) => yield chunk(json!({}), Some(finish_reason(&r)), include_usage.then(|| usage(&r.usage))),
            }
        }
        yield "data: [DONE]\n\n".to_string();
    })
}
