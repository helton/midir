//! OpenAI Chat Completions <-> canonical (`POST /v1/chat/completions`).

use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::{json, Map, Value};

use super::common::{arguments_str, ignored_params, parse_arguments, text_of, tool_params, Adapted};
use crate::canonical::{new_call_id, CanonicalRequest, CanonicalResponse, Event, ToolCall, ToolChoice, ToolResult, ToolSpec, Usage};
use crate::errors::{ClientError, Error};
use crate::py::json as pyjson;
use crate::py::obj::{self, PyResult};
use crate::py::text::{self, truthy};

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

fn str_or(v: Option<&Value>, default: &str) -> String {
    match v {
        None | Some(Value::Null) => default.to_string(),
        Some(x) => text::str_of(x),
    }
}

pub fn to_canonical(body: &Value) -> PyResult<Result<Adapted, ClientError>> {
    if text::truthy_opt(body.get("logprobs")) || text::truthy_opt(body.get("top_logprobs")) {
        return Ok(Err(ClientError::new("logprobs are not available from StackSpot", "unsupported_parameter")));
    }
    if let Some(n) = body.get("n") {
        if !n.is_null() && !text::eq(n, &json!(1)) {
            return Ok(Err(ClientError::new("n > 1 is not supported (one generation per request)", "unsupported_parameter")));
        }
    }
    let messages = match body.get("messages") {
        Some(Value::Array(a)) if !a.is_empty() => a,
        _ => return Ok(Err(ClientError::new("'messages' is required and must be a non-empty list", "missing_messages"))),
    };
    let mut req = CanonicalRequest { ignored: ignored_params(body, &IGNORED), ..Default::default() };
    for m in messages {
        let role = obj::get(m, "role")?.cloned().unwrap_or(json!("user"));
        let role_s = role.as_str().unwrap_or("");
        if role.is_string() && (role_s == "system" || role_s == "developer") {
            req.system.push(text_of(obj::get(m, "content")?, "system")?);
        } else if role.is_string() && role_s == "assistant" {
            let mut calls = vec![];
            for tc in obj::iter_or_empty(obj::get(m, "tool_calls")?)? {
                let id = obj::get(&tc, "id")?;
                let f = obj::get_truthy(&tc, "function")?.cloned().unwrap_or(json!({}));
                let name = str_or(obj::get(&f, "name")?, "");
                let id = if text::truthy_opt(id) { text::str_of(id.unwrap_or(&Value::Null)) } else { new_call_id() };
                calls.push(ToolCall { id, name, arguments: parse_arguments(obj::get(&f, "arguments")?) });
            }
            if let Some(fc) = obj::get_truthy(m, "function_call")? {
                let name = str_or(obj::get(fc, "name")?, "");
                calls.push(ToolCall { id: new_call_id(), name, arguments: parse_arguments(obj::get(fc, "arguments")?) });
            }
            let t = text_of(obj::get(m, "content")?, "assistant")?;
            req.add("assistant", &t, calls, vec![]);
        } else if role.is_string() && (role_s == "tool" || role_s == "function") {
            let name = str_or(obj::get(m, "name")?, "");
            let call_id = match obj::get(m, "tool_call_id")? {
                Some(v) if truthy(v) => text::str_of(v),
                _ => obj::get(m, "name")?.map(text::str_of).unwrap_or_default(),
            };
            let content = text_of(obj::get(m, "content")?, "tool")?;
            req.add("user", "", vec![], vec![ToolResult { call_id, content, name, is_error: false }]);
        } else {
            let t = text_of(obj::get(m, "content")?, &text::str_of(&role))?;
            req.add("user", &t, vec![], vec![]);
        }
    }
    for t in obj::iter_or_empty(body.get("tools"))? {
        let typ = obj::get(&t, "type")?.cloned().unwrap_or(json!("function"));
        if !obj::is_str(Some(&typ), "function") {
            req.ignored.push(format!("tool:{}", text::str_of(obj::get(&t, "type")?.unwrap_or(&Value::Null))));
            continue;
        }
        let f = obj::get_truthy(&t, "function")?.cloned().unwrap_or(json!({}));
        let name = obj::get(&f, "name")?.cloned().unwrap_or(json!(""));
        req.tools.push(ToolSpec::new(Some(&name), obj::get(&f, "description")?, tool_params(obj::get(&f, "parameters")?), false));
    }
    for f in obj::iter_or_empty(body.get("functions"))? {
        let name = obj::get(&f, "name")?.cloned().unwrap_or(json!(""));
        let desc = obj::get(&f, "description")?.cloned().unwrap_or(json!(""));
        req.tools.push(ToolSpec::new(Some(&name), Some(&desc), tool_params(obj::get(&f, "parameters")?), false));
    }
    let choice = match body.get("tool_choice") {
        Some(c) => c.clone(),
        None => body.get("function_call").cloned().unwrap_or(json!("auto")),
    };
    match &choice {
        Value::Object(_) => {
            let f = obj::get_truthy(&choice, "function")?.cloned().unwrap_or(json!({}));
            let name = match obj::get(&f, "name")? {
                Some(n) if truthy(n) => Some(n.clone()),
                _ => choice.get("name").cloned(),
            };
            req.tool_choice = match name {
                Some(n) if truthy(&n) => ToolChoice::Named(n),
                _ => ToolChoice::Auto,
            };
        }
        Value::String(s) if s == "none" => req.tool_choice = ToolChoice::None,
        Value::String(s) if s == "required" => req.tool_choice = ToolChoice::Required,
        Value::String(s) if s == "auto" => req.tool_choice = ToolChoice::Auto,
        _ => {}
    }
    if let Some(fmt @ Value::Object(_)) = body.get("response_format") {
        if obj::is_str(fmt.get("type"), "json_object") {
            req.json_schema = Some(json!({"type": "object"}));
        } else if obj::is_str(fmt.get("type"), "json_schema") {
            let js = obj::get_truthy(fmt, "json_schema")?.cloned().unwrap_or(json!({}));
            req.json_schema = Some(match obj::get(&js, "schema")? {
                Some(s) if truthy(s) => s.clone(),
                _ => json!({"type": "object"}),
            });
        }
    }
    req.stop = match body.get("stop") {
        Some(Value::String(s)) => vec![s.clone()],
        other => obj::iter_or_empty(other)?.into_iter().filter_map(|s| s.as_str().map(String::from)).collect(),
    };
    let max_tokens = match body.get("max_completion_tokens") {
        Some(v) if truthy(v) => Some(v.clone()),
        _ => body.get("max_tokens").cloned(),
    };
    Ok(Ok(Adapted { req, max_tokens }))
}

pub fn finish_reason(r: &CanonicalResponse) -> &'static str {
    match r.finish.as_str() {
        "tool_calls" => "tool_calls",
        "length" => "length",
        _ => "stop",
    }
}

/// Same shape streaming (last chunk) and not.
pub fn usage(u: &Usage) -> Value {
    let mut m = match u.to_json() {
        Value::Object(m) => m,
        _ => Map::new(),
    };
    m.insert("prompt_tokens_details".into(), json!({"cached_tokens": 0}));
    m.insert("completion_tokens_details".into(), json!({"reasoning_tokens": 0}));
    Value::Object(m)
}

fn tool_call(c: &ToolCall, index: Option<usize>) -> Value {
    let mut d = json!({"id": c.id, "type": "function", "function": {"name": c.name, "arguments": arguments_str(&c.arguments)}});
    if let (Some(i), Value::Object(m)) = (index, &mut d) {
        m.insert("index".into(), json!(i));
    }
    d
}

pub fn response(r: &CanonicalResponse, cid: &str, created: i64, model: &Value) -> Value {
    let content = if !r.text.is_empty() || r.tool_calls.is_empty() { json!(r.text) } else { Value::Null };
    let mut message = json!({"role": "assistant", "content": content, "refusal": null});
    if !r.tool_calls.is_empty() {
        message["tool_calls"] = Value::Array(r.tool_calls.iter().map(|c| tool_call(c, None)).collect());
    }
    json!({"id": cid, "object": "chat.completion", "created": created, "model": model,
           "choices": [{"index": 0, "message": message, "finish_reason": finish_reason(r), "logprobs": null}],
           "usage": usage(&r.usage), "system_fingerprint": r.message_id})
}

pub fn stream(
    events: BoxStream<'static, Result<Event, Error>>,
    cid: String,
    created: i64,
    model: Value,
    include_usage: bool,
) -> BoxStream<'static, Result<String, Error>> {
    let chunk = move |delta: Value, finish: Option<&str>, usage: Option<Value>| -> String {
        let mut d = json!({"id": cid, "object": "chat.completion.chunk", "created": created, "model": model,
                           "choices": [{"index": 0, "delta": delta, "finish_reason": finish, "logprobs": null}]});
        if let Some(u) = usage {
            d["usage"] = u;
        }
        format!("data: {}\n\n", pyjson::dumps(&d, pyjson::DEFAULT))
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
                Event::Done(r) => {
                    let u = if include_usage { Some(usage(&r.usage)) } else { None };
                    yield chunk(json!({}), Some(finish_reason(&r)), u);
                }
            }
        }
        yield "data: [DONE]\n\n".to_string();
    })
}
