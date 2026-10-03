//! Anthropic Messages <-> canonical (`POST /v1/messages`, `POST /v1/messages/count_tokens`).

use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::{json, Map, Value};

use super::common::{ignored_params, is_media, text_of, text_of_value, tool_params, Adapted};
use crate::canonical::{hex_id, CanonicalRequest, CanonicalResponse, Event, ToolCall, ToolChoice, ToolResult, ToolSpec, Usage};
use crate::errors::{ClientError, Error};
use crate::py::json as pyjson;
use crate::py::obj::{self, PyResult};
use crate::py::text::{self, truthy};

const IGNORED: [&str; 10] = [
    "temperature",
    "top_p",
    "top_k",
    "metadata",
    "thinking",
    "service_tier",
    "cache_control",
    "container",
    "mcp_servers",
    "context_management",
];
const SKIPPED_BLOCKS: [&str; 4] = ["thinking", "redacted_thinking", "server_tool_use", "web_search_tool_result"];

pub fn to_canonical(body: &Value) -> PyResult<Result<Adapted, ClientError>> {
    let messages = match body.get("messages") {
        Some(Value::Array(a)) if !a.is_empty() => a,
        _ => return Ok(Err(ClientError::new("'messages' is required and must be a non-empty list", "invalid_request_error"))),
    };
    let mut req = CanonicalRequest { ignored: ignored_params(body, &IGNORED), ..Default::default() };
    if let Some(system) = body.get("system").filter(|s| truthy(s)) {
        match system {
            Value::String(s) => req.system.push(s.clone()),
            other => {
                let mut parts = vec![];
                for b in obj::iter(other)? {
                    parts.push(text_of_value(&Value::Array(vec![b]), "system")?);
                }
                req.system.push(parts.join("\n"));
            }
        }
    }
    for m in messages {
        let role = if obj::is_str(obj::get(m, "role")?, "assistant") { "assistant" } else { "user" };
        let content = obj::get(m, "content")?;
        if let Some(Value::String(s)) = content {
            req.add(role, s, vec![], vec![]);
            continue;
        }
        let mut texts: Vec<String> = vec![];
        let mut after: Vec<String> = vec![];
        let mut calls: Vec<ToolCall> = vec![];
        let mut results: Vec<ToolResult> = vec![];
        for b in obj::iter_or_empty(content)? {
            let t = obj::get(&b, "type")?.cloned().unwrap_or(Value::Null);
            if obj::is_str(Some(&t), "text") {
                let s = text::str_of(obj::get(&b, "text")?.unwrap_or(&json!("")));
                if results.is_empty() {
                    texts.push(s)
                } else {
                    after.push(s)
                }
            } else if obj::is_str(Some(&t), "tool_use") {
                let id = match obj::get(&b, "id")? {
                    Some(v) if truthy(v) => text::str_of(v),
                    _ => format!("toolu_{}", hex_id(24)),
                };
                let name = match obj::get(&b, "name")? {
                    None | Some(Value::Null) => String::new(),
                    Some(v) => text::str_of(v),
                };
                let input = match obj::get(&b, "input")? {
                    None | Some(Value::Null) => json!({}),
                    Some(v) => v.clone(),
                };
                calls.push(ToolCall { id, name, arguments: input });
            } else if obj::is_str(Some(&t), "tool_result") {
                let c = obj::get(&b, "content")?;
                let txt = match c {
                    Some(Value::String(s)) => s.clone(),
                    other => {
                        let mut parts = vec![];
                        for p in obj::iter_or_empty(other)? {
                            parts.push(text_of_value(&Value::Array(vec![p]), "tool_result")?);
                        }
                        parts.join("\n")
                    }
                };
                let call_id = obj::get(&b, "tool_use_id")?.map(text::str_of).unwrap_or_default();
                results.push(ToolResult {
                    call_id,
                    content: txt,
                    name: String::new(),
                    is_error: text::truthy_opt(obj::get(&b, "is_error")?),
                });
            } else if is_media(&t)? {
                texts.push(text_of(Some(&Value::Array(vec![b.clone()])), role)?);
            } else if matches!(&t, Value::String(s) if SKIPPED_BLOCKS.contains(&s.as_str())) {
                continue;
            } else {
                texts.push(pyjson::dumps(&b, pyjson::DEFAULT));
            }
        }
        req.add(role, &texts.join("\n"), calls, results);
        if !after.is_empty() {
            req.add(role, &after.join("\n"), vec![], vec![]);
        }
    }
    for t in obj::iter_or_empty(body.get("tools"))? {
        let typ = match obj::get(&t, "type")? {
            Some(v) if truthy(v) => v.clone(),
            _ => json!("custom"),
        };
        if !text::truthy_opt(obj::get(&t, "input_schema")?) && !obj::is_str(Some(&typ), "custom") {
            req.ignored.push(format!("tool:{}", text::str_of(&typ)));
            continue;
        }
        let name = obj::get(&t, "name")?.cloned().unwrap_or(json!(""));
        let desc = obj::get(&t, "description")?.filter(|d| truthy(d)).cloned().unwrap_or(json!(""));
        req.tools.push(ToolSpec::new(Some(&name), Some(&desc), tool_params(obj::get(&t, "input_schema")?), false));
    }
    let choice = body.get("tool_choice").filter(|c| truthy(c)).cloned().unwrap_or(json!({}));
    let typ = match &choice {
        Value::Object(m) => m.get("type").cloned().unwrap_or(json!("auto")),
        _ => json!("auto"),
    };
    obj::hashable(&typ)?;
    req.tool_choice = match typ.as_str() {
        Some("auto") => ToolChoice::Auto,
        Some("any") => ToolChoice::Required,
        Some("none") => ToolChoice::None,
        Some("tool") => match choice.get("name") {
            Some(n) if truthy(n) => ToolChoice::Named(n.clone()),
            _ => ToolChoice::Required,
        },
        _ => ToolChoice::Auto,
    };
    req.stop = match body.get("stop_sequences") {
        Some(Value::String(s)) => vec![s.clone()],
        other => obj::iter_or_empty(other)?.into_iter().filter_map(|s| s.as_str().map(String::from)).collect(),
    };
    let max_tokens = body.get("max_tokens").cloned();
    let oc = body.get("output_config").filter(|c| truthy(c)).cloned().unwrap_or(json!({}));
    let fmt = obj::get_truthy(&oc, "format")?.cloned().unwrap_or(json!({}));
    if obj::is_str(obj::get(&fmt, "type")?, "json_schema") {
        req.json_schema = Some(match fmt.get("schema") {
            Some(s) if truthy(s) => s.clone(),
            _ => json!({"type": "object"}),
        });
    }
    Ok(Ok(Adapted { req, max_tokens }))
}

pub fn stop_reason(r: &CanonicalResponse) -> &'static str {
    match r.finish.as_str() {
        "tool_calls" => "tool_use",
        "length" => "max_tokens",
        "stop_sequence" => "stop_sequence",
        _ => "end_turn",
    }
}

pub fn usage(u: &Usage) -> Value {
    json!({"input_tokens": u.prompt_tokens, "output_tokens": u.completion_tokens, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0})
}

pub fn tool_use_id(call_id: &str) -> String {
    if call_id.starts_with("toolu_") {
        call_id.to_string()
    } else {
        format!("toolu_{}", call_id.strip_prefix("call_").unwrap_or(call_id))
    }
}

/// CAVEAT: invalid JSON arrives as {"_raw": ...}.
pub fn tool_input(c: &ToolCall) -> Value {
    if c.arguments.is_object() {
        c.arguments.clone()
    } else {
        json!({"_raw": c.arguments})
    }
}

pub fn content_blocks(r: &CanonicalResponse) -> Value {
    let mut blocks = vec![];
    if !r.text.is_empty() || r.tool_calls.is_empty() {
        blocks.push(json!({"type": "text", "text": r.text}));
    }
    for c in &r.tool_calls {
        blocks.push(json!({"type": "tool_use", "id": tool_use_id(&c.id), "name": c.name, "input": tool_input(c)}));
    }
    Value::Array(blocks)
}

pub fn response(r: &CanonicalResponse, mid: &str, model: &Value) -> Value {
    json!({"id": mid, "type": "message", "role": "assistant", "model": model, "content": content_blocks(r), "stop_reason": stop_reason(r),
           "stop_sequence": r.stop_sequence, "usage": usage(&r.usage)})
}

fn ev(name: &str, data: Value) -> String {
    let mut m = Map::new();
    m.insert("type".into(), json!(name));
    if let Value::Object(d) = data {
        m.extend(d);
    }
    format!("event: {name}\ndata: {}\n\n", pyjson::dumps(&Value::Object(m), pyjson::DEFAULT))
}

pub fn stream(events: BoxStream<'static, Result<Event, Error>>, mid: String, model: Value) -> BoxStream<'static, Result<String, Error>> {
    Box::pin(async_stream::try_stream! {
        yield ev("message_start", json!({"message": {"id": mid, "type": "message", "role": "assistant", "model": model, "content": [], "stop_reason": null,
                 "stop_sequence": null, "usage": {"input_tokens": 0, "output_tokens": 0, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}}}));
        yield ev("ping", json!({}));
        let mut index = 0usize;
        let mut text_open = false;
        let mut final_: Option<CanonicalResponse> = None;
        let mut events = events;
        while let Some(e) = events.next().await {
            match e? {
                Event::Text(t) => {
                    if !text_open {
                        yield ev("content_block_start", json!({"index": index, "content_block": {"type": "text", "text": ""}}));
                        text_open = true;
                    }
                    yield ev("content_block_delta", json!({"index": index, "delta": {"type": "text_delta", "text": t}}));
                }
                Event::ToolCall(c) => {
                    if text_open {
                        yield ev("content_block_stop", json!({"index": index}));
                        text_open = false;
                        index += 1;
                    }
                    yield ev("content_block_start", json!({"index": index, "content_block": {"type": "tool_use", "id": tool_use_id(&c.id), "name": c.name, "input": {}}}));
                    yield ev("content_block_delta", json!({"index": index, "delta": {"type": "input_json_delta", "partial_json": pyjson::dumps(&tool_input(&c), pyjson::DEFAULT)}}));
                    yield ev("content_block_stop", json!({"index": index}));
                    index += 1;
                }
                Event::Keepalive => yield ev("ping", json!({})),
                Event::Done(r) => final_ = Some(r),
            }
        }
        if text_open {
            yield ev("content_block_stop", json!({"index": index}));
        }
        let r = final_.unwrap_or_default();
        yield ev("message_delta", json!({"delta": {"stop_reason": stop_reason(&r), "stop_sequence": r.stop_sequence},
                 "usage": {"output_tokens": r.usage.completion_tokens, "input_tokens": r.usage.prompt_tokens}}));
        yield ev("message_stop", json!({}));
    })
}
