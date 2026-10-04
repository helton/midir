//! OpenAI Chat Completions <-> canonical (`POST /v1/chat/completions`).

use futures::StreamExt;
use futures::stream::BoxStream;
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::{Value, json};

use super::common::{
    Content, ModeOr, RequestInfo, Stop, arguments_text, check_named_choice, decode, ignored_params, is_true, parse_arguments, positive,
    stops, text_of, tool_params,
};
use crate::canonical::{
    CanonicalRequest, CanonicalResponse, Event, Finish, ToolCall, ToolChoice, ToolResult, ToolSpec, Usage, new_call_id,
};
use crate::errors::{ClientError, Error};

#[derive(Deserialize)]
struct Request {
    model: Option<String>,
    stream: Option<Box<RawValue>>,
    stream_options: Option<StreamOptions>,
    messages: Option<Vec<Message>>,
    tools: Option<Vec<Tool>>,
    functions: Option<Vec<Function>>,
    tool_choice: Option<ModeOr<NamedChoice>>,
    function_call: Option<ModeOr<NamedChoice>>,
    parallel_tool_calls: Option<bool>,
    response_format: Option<ResponseFormat>,
    stop: Option<Stop>,
    max_tokens: Option<i64>,
    max_completion_tokens: Option<i64>,
    n: Option<u64>,
    logprobs: Option<bool>,
    top_logprobs: Option<u64>,
    // accepted without effect
    temperature: Option<Box<RawValue>>,
    top_p: Option<Box<RawValue>>,
    seed: Option<Box<RawValue>>,
    presence_penalty: Option<Box<RawValue>>,
    frequency_penalty: Option<Box<RawValue>>,
    logit_bias: Option<Box<RawValue>>,
    reasoning_effort: Option<Box<RawValue>>,
    service_tier: Option<Box<RawValue>>,
    store: Option<Box<RawValue>>,
    metadata: Option<Box<RawValue>>,
    prediction: Option<Box<RawValue>>,
    audio: Option<Box<RawValue>>,
    modalities: Option<Box<RawValue>>,
    verbosity: Option<Box<RawValue>>,
}

impl Request {
    fn ignored(&self) -> Vec<String> {
        ignored_params(&[
            ("temperature", self.temperature.as_deref()),
            ("top_p", self.top_p.as_deref()),
            ("seed", self.seed.as_deref()),
            ("presence_penalty", self.presence_penalty.as_deref()),
            ("frequency_penalty", self.frequency_penalty.as_deref()),
            ("logit_bias", self.logit_bias.as_deref()),
            ("reasoning_effort", self.reasoning_effort.as_deref()),
            ("service_tier", self.service_tier.as_deref()),
            ("store", self.store.as_deref()),
            ("metadata", self.metadata.as_deref()),
            ("prediction", self.prediction.as_deref()),
            ("audio", self.audio.as_deref()),
            ("modalities", self.modalities.as_deref()),
            ("verbosity", self.verbosity.as_deref()),
        ])
    }
}

#[derive(Deserialize)]
struct StreamOptions {
    include_usage: Option<bool>,
}

#[derive(Deserialize)]
#[serde(expecting = "a message object")]
struct Message {
    role: Option<String>,
    content: Option<Content>,
    tool_calls: Option<Vec<MessageToolCall>>,
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

/// `{"type": "function", "function": {"name": ...}}`, or the legacy `{"name": ...}` of `function_call`.
#[derive(Deserialize)]
struct NamedChoice {
    function: Option<Function>,
    name: Option<String>,
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

/// The request as a canonical request (`max_tokens` included) and what the HTTP layer needs from it.
pub fn to_canonical(body: &[u8]) -> Result<(CanonicalRequest, RequestInfo), ClientError> {
    let r: Request = decode(body)?;
    if r.logprobs == Some(true) || r.top_logprobs.is_some_and(|n| n > 0) {
        return Err(ClientError::new("logprobs are not available from this backend", "unsupported_parameter"));
    }
    if r.n.is_some_and(|n| n != 1) {
        return Err(ClientError::new("n > 1 is not supported (one generation per request)", "unsupported_parameter"));
    }
    let mut req = CanonicalRequest { ignored: r.ignored(), ..Default::default() };
    let info = RequestInfo {
        model: r.model.unwrap_or_default(),
        stream: is_true(r.stream.as_deref()),
        include_usage: r.stream_options.and_then(|o| o.include_usage).unwrap_or(false),
        ..Default::default()
    };
    let messages = r
        .messages
        .filter(|m| !m.is_empty())
        .ok_or_else(|| ClientError::new("'messages' is required and must be a non-empty list", "missing_messages"))?;
    let mut system = vec![];
    for m in messages {
        let role = m.role.unwrap_or_else(|| "user".into());
        match role.as_str() {
            "system" | "developer" => system.push(text_of(m.content, "system")),
            "assistant" => {
                let mut calls: Vec<ToolCall> = m
                    .tool_calls
                    .into_iter()
                    .flatten()
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
                req.add("assistant", &text_of(m.content, "assistant"), calls, vec![]);
            }
            "tool" | "function" => {
                let name = m.name.unwrap_or_default();
                let call_id = m.tool_call_id.filter(|i| !i.is_empty()).unwrap_or_else(|| name.clone());
                let content = text_of(m.content, "tool");
                req.add("user", "", vec![], vec![ToolResult { call_id, content, name, is_error: false }]);
            }
            other => req.add_text("user", &text_of(m.content, other)),
        }
    }
    req.system = system.into();
    let mut tools = vec![];
    for t in r.tools.into_iter().flatten() {
        match t.kind.as_deref().unwrap_or("function") {
            "function" => {
                let f = t.function.unwrap_or_default();
                tools.push(ToolSpec::new(f.name.unwrap_or_default(), f.description.unwrap_or_default(), tool_params(f.parameters), false));
            }
            other => req.ignored.push(format!("tool:{other}")),
        }
    }
    for f in r.functions.into_iter().flatten() {
        tools.push(ToolSpec::new(f.name.unwrap_or_default(), f.description.unwrap_or_default(), tool_params(f.parameters), false));
    }
    req.tool_choice = match r.tool_choice.or(r.function_call) {
        Some(ModeOr::Mode(m)) => match m.as_str() {
            "none" => ToolChoice::None,
            "required" => ToolChoice::Required,
            _ => ToolChoice::Auto,
        },
        Some(ModeOr::Object(NamedChoice { function, name })) => match function.and_then(|f| f.name).or(name).filter(|n| !n.is_empty()) {
            Some(n) => {
                check_named_choice(&n, &tools)?;
                ToolChoice::Named(n)
            }
            None => ToolChoice::Auto,
        },
        None => ToolChoice::Auto,
    };
    req.tools = tools.into();
    req.parallel_tool_calls = r.parallel_tool_calls.unwrap_or(true);
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
    Ok((req, info))
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

/// Chunks as OpenAI streams them. With `include_usage`, every chunk carries `"usage": null` and one last chunk with
/// no choices carries the usage; without it there is no usage at all.
pub fn stream(
    events: BoxStream<'static, Result<Event, Error>>,
    cid: String,
    created: i64,
    model: String,
    include_usage: bool,
) -> BoxStream<'static, Result<String, Error>> {
    let chunk = move |choices: Value, usage: Option<Value>| -> String {
        let mut d = json!({"id": cid, "object": "chat.completion.chunk", "created": created, "model": model, "choices": choices});
        if include_usage {
            d["usage"] = usage.unwrap_or(Value::Null);
        }
        format!("data: {d}\n\n")
    };
    let choice = |delta: Value, finish: Option<&str>| json!([{"index": 0, "delta": delta, "finish_reason": finish, "logprobs": null}]);
    Box::pin(async_stream::try_stream! {
        yield chunk(choice(json!({"role": "assistant", "content": ""}), None), None);
        let mut n_calls = 0usize;
        let mut events = events;
        while let Some(ev) = events.next().await {
            match ev? {
                Event::Text(t) => yield chunk(choice(json!({"content": t}), None), None),
                Event::ToolCall(c) => {
                    yield chunk(choice(json!({"tool_calls": [tool_call(&c, Some(n_calls))]}), None), None);
                    n_calls += 1;
                }
                Event::Keepalive => yield ": keepalive\n\n".to_string(),
                Event::Prompt { .. } => {}
                Event::Done(r) => {
                    yield chunk(choice(json!({}), Some(finish_reason(&r))), None);
                    if include_usage {
                        yield chunk(json!([]), Some(usage(&r.usage)));
                    }
                }
            }
        }
        yield "data: [DONE]\n\n".to_string();
    })
}
