//! OpenAI Responses <-> canonical (`POST /v1/responses`, `GET /v1/responses/{id}`).

use std::sync::Arc;

use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::{json, Map, Value};

use super::common::{arguments_str, ignored_params, parse_arguments, text_of, tool_params, Adapted};
use crate::canonical::{
    custom_tool_params, hex_id, new_call_id, CanonicalRequest, CanonicalResponse, Event, ToolCall, ToolChoice, ToolResult, ToolSpec, Usage,
};
use crate::errors::{ClientError, Error};
use crate::py::json as pyjson;
use crate::py::obj::{self, PyErr, PyResult};
use crate::py::text::{self, truthy};
use crate::store::ResponseStore;

const IGNORED: [&str; 12] = [
    "temperature",
    "top_p",
    "reasoning",
    "service_tier",
    "metadata",
    "parallel_tool_calls",
    "truncation",
    "user",
    "include",
    "prompt_cache_key",
    "safety_identifier",
    "background",
];

fn first_truthy(it: &Value, keys: &[&str]) -> PyResult<Option<Value>> {
    for k in keys {
        if let Some(v) = obj::get(it, k)? {
            if truthy(v) {
                return Ok(Some(v.clone()));
            }
        }
    }
    Ok(None)
}

fn add_tool(req: &mut CanonicalRequest, t: &Value) -> PyResult<()> {
    let typ = obj::get(t, "type")?.cloned().unwrap_or(json!("function"));
    if obj::is_str(Some(&typ), "function") {
        let name = obj::get(t, "name")?.cloned().unwrap_or(json!(""));
        let desc = obj::get(t, "description")?.filter(|d| truthy(d)).cloned().unwrap_or(json!(""));
        req.tools.push(ToolSpec::new(Some(&name), Some(&desc), tool_params(obj::get(t, "parameters")?), false));
    } else if obj::is_str(Some(&typ), "custom") {
        // CAVEAT: free-form text tool (e.g. Codex apply_patch); its grammar/format is not enforced
        let desc = match obj::get(t, "description")? {
            Some(Value::String(s)) => s.clone(),
            Some(v) if truthy(v) => {
                return Err(PyErr::type_err(format!("unsupported operand type(s) for +: '{}' and 'str'", text::type_name(v))))
            }
            _ => String::new(),
        };
        let description =
            text::strip(&(desc + " Free-form text tool: put the entire raw input in the single string argument \"input\".")).to_string();
        let name = obj::get(t, "name")?.cloned().unwrap_or(json!(""));
        req.tools.push(ToolSpec::new(Some(&name), Some(&json!(description)), custom_tool_params(), true));
    } else if obj::is_str(Some(&typ), "namespace") {
        for inner in obj::iter_or_empty(obj::get(t, "tools")?)? {
            add_tool(req, &inner)?;
        }
    } else {
        // CAVEAT: built-in tool (web_search, file_search, ...) omitted with a warning
        req.ignored.push(format!("tool:{}", text::str_of(&typ)));
    }
    Ok(())
}

pub fn to_canonical(body: &Value, store: &ResponseStore) -> PyResult<Result<Adapted, ClientError>> {
    let mut req = CanonicalRequest { ignored: ignored_params(body, &IGNORED), ..Default::default() };
    if let Some(prev) = body.get("previous_response_id").filter(|p| truthy(p)) {
        let Value::String(prev) = prev else {
            return Err(PyErr::type_err(format!("expected string or bytes-like object, got '{}'", text::type_name(prev))));
        };
        let Some((_, prev_req, prev_resp)) = store.load(prev) else {
            return Ok(Err(ClientError::with_status(
                format!("previous_response_id '{prev}' is unknown (responses are {})", store.describe()),
                "previous_response_not_found",
                404,
            )));
        };
        req.turns = prev_req.turns.clone();
        req.add("assistant", &prev_resp.text, prev_resp.tool_calls.clone(), vec![]);
        {
            let mut meta = req.meta();
            meta.prev_id = Some(prev.clone());
            meta.prev_turns = req.turns.len();
        }
        // instructions and tools are not inherited: the current request's values win; reuse the previous only when absent
        if !text::truthy_opt(body.get("instructions")) {
            req.system = prev_req.system.clone();
        }
        if body.get("tools").map_or(true, Value::is_null) {
            req.tools = prev_req.tools.clone();
        }
    }
    if let Some(instr) = body.get("instructions").filter(|i| truthy(i)) {
        req.system.push(text_of(Some(instr), "instructions")?);
    }
    let raw_input = body.get("input").cloned().unwrap_or(json!(""));
    let items = match &raw_input {
        Value::String(_) => vec![json!({"role": "user", "content": raw_input})],
        other => obj::iter_or_empty(Some(other))?,
    };
    for it in &items {
        if !it.is_object() {
            continue;
        }
        let t = obj::get(it, "type")?.cloned().unwrap_or(json!("message"));
        if obj::is_str(Some(&t), "message") || (t.is_null() && it.get("role").is_some()) {
            let role = obj::get(it, "role")?.cloned().unwrap_or(json!("user"));
            if obj::is_str(Some(&role), "system") || obj::is_str(Some(&role), "developer") {
                req.system.push(text_of(obj::get(it, "content")?, &text::str_of(&role))?);
            } else {
                let r = if obj::is_str(Some(&role), "assistant") { "assistant" } else { "user" };
                let content = text_of(obj::get(it, "content")?, &text::str_of(&role))?;
                req.add(r, &content, vec![], vec![]);
            }
        } else if obj::is_str(Some(&t), "function_call") {
            let id = first_truthy(it, &["call_id", "id"])?.map(|v| text::str_of(&v)).unwrap_or_else(new_call_id);
            let name = obj::get(it, "name")?.filter(|v| !v.is_null()).map(text::str_of).unwrap_or_default();
            req.add("assistant", "", vec![ToolCall { id, name, arguments: parse_arguments(obj::get(it, "arguments")?) }], vec![]);
        } else if obj::is_str(Some(&t), "custom_tool_call") {
            let id = first_truthy(it, &["call_id", "id"])?.map(|v| text::str_of(&v)).unwrap_or_else(new_call_id);
            let name = obj::get(it, "name")?.filter(|v| !v.is_null()).map(text::str_of).unwrap_or_default();
            let input = obj::get(it, "input")?.cloned().unwrap_or(json!(""));
            req.add("assistant", "", vec![ToolCall { id, name, arguments: json!({"input": input}) }], vec![]);
        } else if obj::is_str(Some(&t), "function_call_output") || obj::is_str(Some(&t), "custom_tool_call_output") {
            let call_id = obj::get(it, "call_id")?.map(text::str_of).unwrap_or_default();
            let content = text_of(obj::get(it, "output")?, t.as_str().unwrap_or(""))?;
            req.add("user", "", vec![], vec![ToolResult { call_id, content, name: String::new(), is_error: false }]);
        } else if obj::is_str(Some(&t), "reasoning") || obj::is_str(Some(&t), "item_reference") {
            continue; // no equivalent; ignored
        } else {
            return Ok(Err(ClientError::new(format!("input item '{}' is not supported", text::str_of(&t)), "unsupported_input")));
        }
    }
    for t in obj::iter_or_empty(body.get("tools"))? {
        add_tool(&mut req, &t)?;
    }
    let choice = body.get("tool_choice").cloned().unwrap_or(json!("auto"));
    match &choice {
        Value::Object(m) => {
            req.tool_choice = match m.get("name") {
                Some(n) if truthy(n) => ToolChoice::Named(n.clone()),
                _ => ToolChoice::Auto,
            }
        }
        Value::String(s) if s == "none" => req.tool_choice = ToolChoice::None,
        Value::String(s) if s == "required" => req.tool_choice = ToolChoice::Required,
        Value::String(s) if s == "auto" => req.tool_choice = ToolChoice::Auto,
        _ => {}
    }
    let text_cfg = body.get("text").filter(|t| truthy(t)).cloned().unwrap_or(json!({}));
    let fmt = obj::get_truthy(&text_cfg, "format")?.cloned().unwrap_or(json!({}));
    let ftype = obj::get(&fmt, "type")?;
    if obj::is_str(ftype, "json_object") {
        req.json_schema = Some(json!({"type": "object"}));
    } else if obj::is_str(ftype, "json_schema") {
        req.json_schema = Some(match fmt.get("schema") {
            Some(s) if truthy(s) => s.clone(),
            _ => json!({"type": "object"}),
        });
    }
    let max_tokens = body.get("max_output_tokens").cloned();
    Ok(Ok(Adapted { req, max_tokens }))
}

// ---- response ----

pub fn usage(u: &Usage) -> Value {
    json!({"input_tokens": u.prompt_tokens, "output_tokens": u.completion_tokens, "total_tokens": u.total_tokens,
           "input_tokens_details": {"cached_tokens": 0}, "output_tokens_details": {"reasoning_tokens": 0}})
}

pub fn call_item(c: &ToolCall, custom_names: &[String], status: &str) -> Value {
    let suffix = text::tail_from(&c.id, 5);
    if custom_names.contains(&c.name) {
        let raw = match &c.arguments {
            Value::Object(m) => m.get("input").cloned().unwrap_or(json!("")),
            other => json!(text::str_of(other)),
        };
        return json!({"type": "custom_tool_call", "id": format!("ctc_{suffix}"), "call_id": c.id, "name": c.name, "input": raw, "status": status});
    }
    json!({"type": "function_call", "id": format!("fc_{suffix}"), "call_id": c.id, "name": c.name, "arguments": arguments_str(&c.arguments), "status": status})
}

pub fn message_item(msg_id: &str, text_: &str, status: &str) -> Value {
    json!({"type": "message", "id": msg_id, "status": status, "role": "assistant", "content": [{"type": "output_text", "text": text_, "annotations": []}]})
}

pub fn output_items(r: &CanonicalResponse, msg_id: &str, custom_names: &[String]) -> Vec<Value> {
    let mut items = vec![];
    if !r.text.is_empty() || r.tool_calls.is_empty() {
        items.push(message_item(msg_id, &r.text, "completed"));
    }
    items.extend(r.tool_calls.iter().map(|c| call_item(c, custom_names, "completed")));
    items
}

fn get_or(body: &Value, key: &str, default: Value) -> Value {
    body.get(key).cloned().unwrap_or(default)
}

fn get_truthy_or(body: &Value, key: &str, default: Value) -> Value {
    body.get(key).filter(|v| truthy(v)).cloned().unwrap_or(default)
}

#[allow(clippy::too_many_arguments)] // the Response object's fields, as the spec lists them
pub fn envelope(
    body: &Value,
    rid: &str,
    created: i64,
    model: &Value,
    status: &str,
    output: Vec<Value>,
    usage: Value,
    r: Option<&CanonicalResponse>,
) -> Value {
    let incomplete = r.map_or(false, |r| r.finish == "length");
    json!({
        "id": rid, "object": "response", "created_at": created, "status": if incomplete { "incomplete" } else { status }, "error": null,
        "incomplete_details": if incomplete { json!({"reason": "max_output_tokens"}) } else { Value::Null },
        "instructions": get_or(body, "instructions", Value::Null), "max_output_tokens": get_or(body, "max_output_tokens", Value::Null), "model": model,
        "output": output, "parallel_tool_calls": true, "previous_response_id": get_or(body, "previous_response_id", Value::Null),
        "reasoning": {"effort": null, "summary": null}, "store": get_or(body, "store", json!(true)), "temperature": get_or(body, "temperature", json!(1.0)),
        "text": get_truthy_or(body, "text", json!({"format": {"type": "text"}})), "tool_choice": get_or(body, "tool_choice", json!("auto")),
        "tools": get_truthy_or(body, "tools", json!([])), "top_p": get_or(body, "top_p", json!(1.0)), "truncation": get_or(body, "truncation", json!("disabled")),
        "usage": usage, "user": null, "metadata": get_truthy_or(body, "metadata", json!({}))
    })
}

fn wants_store(body: &Value) -> bool {
    body.get("store").map_or(true, truthy)
}

pub fn complete_response(
    body: &Value,
    rid: &str,
    created: i64,
    model: &Value,
    req: Arc<CanonicalRequest>,
    r: CanonicalResponse,
    store: &ResponseStore,
) -> Value {
    let out = envelope(
        body,
        rid,
        created,
        model,
        "completed",
        output_items(&r, &format!("msg_{}", hex_id(24)), &req.custom_tool_names()),
        usage(&r.usage),
        Some(&r),
    );
    if wants_store(body) {
        store.remember(rid, req, Arc::new(r));
    }
    out
}

struct Seq(u64);

impl Seq {
    fn ev(&mut self, name: &str, data: Value) -> String {
        self.0 += 1;
        let mut m = Map::new();
        m.insert("type".into(), json!(name));
        m.insert("sequence_number".into(), json!(self.0));
        if let Value::Object(d) = data {
            m.extend(d);
        }
        format!("event: {name}\ndata: {}\n\n", pyjson::dumps(&Value::Object(m), pyjson::DEFAULT))
    }
}

/// Each output item keeps one identity: a message that resumes after a tool call is a NEW message item with its own id
/// and only its own text. response.completed lists the items in the order they were streamed.
#[allow(clippy::too_many_arguments)]
pub fn stream(
    events: BoxStream<'static, Result<Event, Error>>,
    body: Arc<Value>,
    rid: String,
    created: i64,
    model: Value,
    req: Arc<CanonicalRequest>,
    store: Arc<ResponseStore>,
) -> BoxStream<'static, Result<String, Error>> {
    Box::pin(async_stream::try_stream! {
        let mut seq = Seq(0);
        let custom_names = req.custom_tool_names();
        yield seq.ev("response.created", json!({"response": envelope(&body, &rid, created, &model, "in_progress", vec![], Value::Null, None)}));
        yield seq.ev("response.in_progress", json!({"response": envelope(&body, &rid, created, &model, "in_progress", vec![], Value::Null, None)}));
        let mut items: Vec<Value> = vec![];
        let mut msg_id = String::new();
        let mut msg_text = String::new();
        let mut all_text = String::new();
        let mut final_: Option<CanonicalResponse> = None;
        let mut events = events;
        while let Some(e) = events.next().await {
            match e? {
                Event::Text(t) => {
                    if msg_id.is_empty() {
                        msg_id = format!("msg_{}", hex_id(24));
                        msg_text.clear();
                        yield seq.ev("response.output_item.added", json!({"output_index": items.len(), "item": {"type": "message", "id": msg_id, "status": "in_progress", "role": "assistant", "content": []}}));
                        yield seq.ev("response.content_part.added", json!({"item_id": msg_id, "output_index": items.len(), "content_index": 0, "part": {"type": "output_text", "text": "", "annotations": []}}));
                    }
                    msg_text.push_str(&t);
                    all_text.push_str(&t);
                    yield seq.ev("response.output_text.delta", json!({"item_id": msg_id, "output_index": items.len(), "content_index": 0, "delta": t, "logprobs": []}));
                }
                Event::ToolCall(c) => {
                    if !msg_id.is_empty() {
                        for chunk in close_text(&mut seq, &mut items, &msg_id, &msg_text) {
                            yield chunk;
                        }
                        msg_id.clear();
                    }
                    let item = call_item(&c, &custom_names, "in_progress");
                    let index = items.len();
                    let item_id = item["id"].clone();
                    if item["type"] == "custom_tool_call" {
                        let mut added = item.clone();
                        added["input"] = json!("");
                        yield seq.ev("response.output_item.added", json!({"output_index": index, "item": added}));
                        yield seq.ev("response.custom_tool_call_input.delta", json!({"item_id": item_id, "output_index": index, "delta": item["input"]}));
                        yield seq.ev("response.custom_tool_call_input.done", json!({"item_id": item_id, "output_index": index, "input": item["input"]}));
                    } else {
                        let mut added = item.clone();
                        added["arguments"] = json!("");
                        yield seq.ev("response.output_item.added", json!({"output_index": index, "item": added}));
                        yield seq.ev("response.function_call_arguments.delta", json!({"item_id": item_id, "output_index": index, "delta": item["arguments"]}));
                        yield seq.ev("response.function_call_arguments.done", json!({"item_id": item_id, "output_index": index, "arguments": item["arguments"]}));
                    }
                    let mut done_item = item;
                    done_item["status"] = json!("completed");
                    items.push(done_item.clone());
                    yield seq.ev("response.output_item.done", json!({"output_index": index, "item": done_item}));
                }
                Event::Keepalive => yield ": keepalive\n\n".to_string(),
                Event::Done(r) => final_ = Some(r),
            }
        }
        if !msg_id.is_empty() {
            for chunk in close_text(&mut seq, &mut items, &msg_id, &msg_text) {
                yield chunk;
            }
        }
        let mut r = final_.unwrap_or_default();
        r.text = if r.tool_calls.is_empty() { all_text } else { text::strip(&all_text).to_string() };
        if items.is_empty() {
            items.push(message_item(&format!("msg_{}", hex_id(24)), "", "completed"));
        }
        let u = usage(&r.usage);
        let completed = envelope(&body, &rid, created, &model, "completed", items, u, Some(&r));
        if wants_store(&body) {
            store.remember(&rid, req.clone(), Arc::new(r));
        }
        yield seq.ev("response.completed", json!({"response": completed}));
    })
}

fn close_text(seq: &mut Seq, items: &mut Vec<Value>, msg_id: &str, msg_text: &str) -> Vec<String> {
    let item = message_item(msg_id, msg_text, "completed");
    items.push(item.clone());
    let index = items.len() - 1;
    vec![
        seq.ev("response.output_text.done", json!({"item_id": msg_id, "output_index": index, "content_index": 0, "text": msg_text, "logprobs": []})),
        seq.ev("response.content_part.done", json!({"item_id": msg_id, "output_index": index, "content_index": 0, "part": {"type": "output_text", "text": msg_text, "annotations": []}})),
        seq.ev("response.output_item.done", json!({"output_index": index, "item": item})),
    ]
}
