//! OpenAI Responses <-> canonical (`POST /v1/responses`, `GET /v1/responses/{id}`).

use std::sync::Arc;

use futures::stream::BoxStream;
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{json, Map, Value};

use super::common::{arguments_text, decode, ignored_params, parse_arguments, positive, text_of, tool_params, Content};
use crate::canonical::{
    custom_tool_params, hex_id, new_call_id, CanonicalRequest, CanonicalResponse, Event, Finish, ToolCall, ToolChoice, ToolResult,
    ToolSpec, Usage,
};
use crate::errors::{ClientError, Error};
use crate::store::ResponseStore;
use crate::text::skip_chars;

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

#[derive(Deserialize)]
struct Request {
    previous_response_id: Option<String>,
    instructions: Option<Content>,
    input: Option<Input>,
    tools: Option<Vec<Tool>>,
    tool_choice: Option<Choice>,
    text: Option<TextConfig>,
    max_output_tokens: Option<i64>,
}

#[derive(Deserialize)]
#[serde(untagged, expecting = "a string or a list of input items")]
enum Input {
    Text(String),
    Items(Vec<Item>),
}

#[derive(Deserialize)]
#[serde(expecting = "an input item object")]
struct Item {
    #[serde(rename = "type")]
    kind: Option<String>,
    role: Option<String>,
    content: Option<Content>,
    id: Option<String>,
    call_id: Option<String>,
    name: Option<String>,
    arguments: Option<Value>,
    input: Option<String>,
    output: Option<Content>,
}

#[derive(Deserialize)]
#[serde(expecting = "a tool object")]
struct Tool {
    #[serde(rename = "type")]
    kind: Option<String>,
    name: Option<String>,
    description: Option<String>,
    parameters: Option<Value>,
    /// a namespace's own tools
    tools: Option<Vec<Tool>>,
}

#[derive(Deserialize)]
#[serde(untagged, expecting = "\"auto\", \"none\", \"required\" or an object naming a tool")]
enum Choice {
    Mode(String),
    Named { name: Option<String> },
}

#[derive(Deserialize)]
#[serde(expecting = "a text configuration object")]
struct TextConfig {
    format: Option<Format>,
}

#[derive(Deserialize)]
struct Format {
    #[serde(rename = "type")]
    kind: Option<String>,
    schema: Option<Value>,
}

fn add_tool(req: &mut CanonicalRequest, t: Tool) {
    match t.kind.as_deref().unwrap_or("function") {
        "function" => {
            req.tools.push(ToolSpec::new(t.name.unwrap_or_default(), t.description.unwrap_or_default(), tool_params(t.parameters), false))
        }
        // CAVEAT: free-form text tool (e.g. Codex apply_patch); its grammar/format is not enforced
        "custom" => {
            let description = format!(
                "{} Free-form text tool: put the entire raw input in the single string argument \"input\".",
                t.description.unwrap_or_default()
            );
            req.tools.push(ToolSpec::new(t.name.unwrap_or_default(), description.trim(), Some(custom_tool_params()), true));
        }
        "namespace" => t.tools.into_iter().flatten().for_each(|inner| add_tool(req, inner)),
        // CAVEAT: built-in tool (web_search, file_search, ...) omitted
        other => req.ignored.push(format!("tool:{other}")),
    }
}

/// The request as a canonical request (`max_tokens` included). A `previous_response_id` brings back the stored
/// conversation, its instructions and its tools (unless the request sets its own).
pub fn to_canonical(body: &Value, store: &ResponseStore) -> Result<CanonicalRequest, ClientError> {
    let r: Request = decode(body)?;
    let mut req = CanonicalRequest { ignored: ignored_params(body, &IGNORED), ..Default::default() };
    let instructions = r.instructions.as_ref().map(|c| c.as_text("instructions")).filter(|t| !t.is_empty());
    if let Some(prev) = r.previous_response_id.filter(|p| !p.is_empty()) {
        let Some((_, prev_req, prev_resp)) = store.load(&prev) else {
            return Err(ClientError::with_status(
                format!("previous_response_id '{prev}' is unknown (responses are {})", store.describe()),
                "previous_response_not_found",
                404,
            ));
        };
        req.turns = prev_req.turns.clone();
        req.add("assistant", &prev_resp.text, prev_resp.tool_calls.clone(), vec![]);
        {
            let mut meta = req.meta();
            meta.prev_turns = req.turns.len();
            meta.prev_id = Some(prev);
        }
        if instructions.is_none() {
            req.system = prev_req.system.clone();
        }
        if r.tools.is_none() {
            req.tools = prev_req.tools.clone();
        }
    }
    req.system.extend(instructions);
    let items = match r.input {
        None => vec![],
        Some(Input::Text(s)) => {
            req.add_text("user", &s);
            vec![]
        }
        Some(Input::Items(items)) => items,
    };
    for it in items {
        let kind = it.kind.as_deref().unwrap_or("message");
        match kind {
            "message" => {
                let role = it.role.unwrap_or_else(|| "user".into());
                let text = text_of(&it.content, &role);
                match role.as_str() {
                    "system" | "developer" => req.system.push(text),
                    "assistant" => req.add_text("assistant", &text),
                    _ => req.add_text("user", &text),
                }
            }
            "function_call" | "custom_tool_call" => {
                let id = it.call_id.or(it.id).filter(|i| !i.is_empty()).unwrap_or_else(new_call_id);
                let arguments =
                    if kind == "custom_tool_call" { json!({"input": it.input.unwrap_or_default()}) } else { parse_arguments(it.arguments) };
                req.add("assistant", "", vec![ToolCall { id, name: it.name.unwrap_or_default(), arguments }], vec![]);
            }
            "function_call_output" | "custom_tool_call_output" => {
                let result = ToolResult {
                    call_id: it.call_id.unwrap_or_default(),
                    content: text_of(&it.output, kind),
                    name: String::new(),
                    is_error: false,
                };
                req.add("user", "", vec![], vec![result]);
            }
            "reasoning" | "item_reference" => {} // no equivalent for a text backend
            other => return Err(ClientError::new(format!("input item '{other}' is not supported"), "unsupported_input")),
        }
    }
    for t in r.tools.into_iter().flatten() {
        add_tool(&mut req, t);
    }
    req.tool_choice = match r.tool_choice {
        Some(Choice::Mode(m)) => match m.as_str() {
            "none" => ToolChoice::None,
            "required" => ToolChoice::Required,
            _ => ToolChoice::Auto,
        },
        Some(Choice::Named { name: Some(n) }) if !n.is_empty() => ToolChoice::Named(n),
        _ => ToolChoice::Auto,
    };
    if let Some(Format { kind: Some(k), schema }) = r.text.and_then(|t| t.format) {
        match k.as_str() {
            "json_object" => req.json_schema = Some(json!({"type": "object"})),
            "json_schema" => {
                req.json_schema =
                    Some(schema.filter(|s| s.as_object().is_some_and(|m| !m.is_empty())).unwrap_or_else(|| json!({"type": "object"})))
            }
            _ => {}
        }
    }
    req.max_tokens = positive("max_output_tokens", r.max_output_tokens)?;
    Ok(req)
}

// ---- response ----

pub fn usage(u: &Usage) -> Value {
    json!({"input_tokens": u.prompt_tokens, "output_tokens": u.completion_tokens, "total_tokens": u.total_tokens,
           "input_tokens_details": {"cached_tokens": 0}, "output_tokens_details": {"reasoning_tokens": 0}})
}

pub fn call_item(c: &ToolCall, custom_names: &[String], status: &str) -> Value {
    let suffix = skip_chars(&c.id, 5);
    if custom_names.contains(&c.name) {
        let input = match &c.arguments {
            Value::Object(m) => m.get("input").cloned().unwrap_or(json!("")),
            other => json!(arguments_text(other)),
        };
        return json!({"type": "custom_tool_call", "id": format!("ctc_{suffix}"), "call_id": c.id, "name": c.name, "input": input, "status": status});
    }
    json!({"type": "function_call", "id": format!("fc_{suffix}"), "call_id": c.id, "name": c.name, "arguments": arguments_text(&c.arguments), "status": status})
}

pub fn message_item(msg_id: &str, text: &str, status: &str) -> Value {
    json!({"type": "message", "id": msg_id, "status": status, "role": "assistant", "content": [{"type": "output_text", "text": text, "annotations": []}]})
}

pub fn output_items(r: &CanonicalResponse, msg_id: &str, custom_names: &[String]) -> Vec<Value> {
    let mut items = vec![];
    if !r.text.is_empty() || r.tool_calls.is_empty() {
        items.push(message_item(msg_id, &r.text, "completed"));
    }
    items.extend(r.tool_calls.iter().map(|c| call_item(c, custom_names, "completed")));
    items
}

/// The Response object around `output`, echoing the request's settings.
pub struct Envelope<'a> {
    pub body: &'a Value,
    pub rid: &'a str,
    pub created: i64,
    pub model: &'a str,
}

impl Envelope<'_> {
    pub fn render(&self, status: &str, output: Vec<Value>, usage: Value, r: Option<&CanonicalResponse>) -> Value {
        let incomplete = r.is_some_and(|r| r.finish == Finish::Length);
        let echo = |key: &str, default: Value| self.body.get(key).filter(|v| !v.is_null()).cloned().unwrap_or(default);
        let echo_nonempty = |key: &str, default: Value| {
            self.body.get(key).filter(|v| !v.is_null() && v != &&json!({}) && v != &&json!([])).cloned().unwrap_or(default)
        };
        json!({
            "id": self.rid, "object": "response", "created_at": self.created, "status": if incomplete { "incomplete" } else { status }, "error": null,
            "incomplete_details": if incomplete { json!({"reason": "max_output_tokens"}) } else { Value::Null },
            "instructions": echo("instructions", Value::Null), "max_output_tokens": echo("max_output_tokens", Value::Null), "model": self.model,
            "output": output, "parallel_tool_calls": true, "previous_response_id": echo("previous_response_id", Value::Null),
            "reasoning": {"effort": null, "summary": null}, "store": echo("store", json!(true)), "temperature": echo("temperature", json!(1.0)),
            "text": echo_nonempty("text", json!({"format": {"type": "text"}})), "tool_choice": echo("tool_choice", json!("auto")),
            "tools": echo_nonempty("tools", json!([])), "top_p": echo("top_p", json!(1.0)), "truncation": echo("truncation", json!("disabled")),
            "usage": usage, "user": null, "metadata": echo_nonempty("metadata", json!({}))
        })
    }
}

fn wants_store(body: &Value) -> bool {
    body.get("store").and_then(Value::as_bool).unwrap_or(true)
}

pub fn complete_response(env: &Envelope, req: Arc<CanonicalRequest>, r: CanonicalResponse, store: &ResponseStore) -> Value {
    let items = output_items(&r, &format!("msg_{}", hex_id(24)), &req.custom_tool_names());
    let out = env.render("completed", items, usage(&r.usage), Some(&r));
    if wants_store(env.body) {
        store.remember(env.rid, req, Arc::new(r));
    }
    out
}

/// Numbered events: every event carries the next `sequence_number`.
struct Seq(u64);

impl Seq {
    fn ev(&mut self, name: &str, data: Value) -> String {
        self.0 += 1;
        let mut m = Map::from_iter([("type".to_string(), json!(name)), ("sequence_number".to_string(), json!(self.0))]);
        if let Value::Object(d) = data {
            m.extend(d);
        }
        format!("event: {name}\ndata: {}\n\n", Value::Object(m))
    }
}

/// Each output item keeps one identity: a message that resumes after a tool call is a NEW message item with its own id
/// and only its own text. response.completed lists the items in the order they were streamed.
pub fn stream(
    events: BoxStream<'static, Result<Event, Error>>,
    body: Arc<Value>,
    rid: String,
    created: i64,
    model: String,
    req: Arc<CanonicalRequest>,
    store: Arc<ResponseStore>,
) -> BoxStream<'static, Result<String, Error>> {
    Box::pin(async_stream::try_stream! {
        let env = Envelope { body: &body, rid: &rid, created, model: &model };
        let mut seq = Seq(0);
        let custom_names = req.custom_tool_names();
        yield seq.ev("response.created", json!({"response": env.render("in_progress", vec![], Value::Null, None)}));
        yield seq.ev("response.in_progress", json!({"response": env.render("in_progress", vec![], Value::Null, None)}));
        let mut items: Vec<Value> = vec![];
        let mut msg_id = String::new();
        let mut msg_text = String::new();
        let mut all_text = String::new();
        let mut done: Option<CanonicalResponse> = None;
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
                    let mut added = item.clone();
                    if item["type"] == "custom_tool_call" {
                        added["input"] = json!("");
                        yield seq.ev("response.output_item.added", json!({"output_index": index, "item": added}));
                        yield seq.ev("response.custom_tool_call_input.delta", json!({"item_id": item_id, "output_index": index, "delta": item["input"]}));
                        yield seq.ev("response.custom_tool_call_input.done", json!({"item_id": item_id, "output_index": index, "input": item["input"]}));
                    } else {
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
                Event::Done(r) => done = Some(r),
            }
        }
        if !msg_id.is_empty() {
            for chunk in close_text(&mut seq, &mut items, &msg_id, &msg_text) {
                yield chunk;
            }
        }
        let mut r = done.unwrap_or_default();
        r.text = if r.tool_calls.is_empty() { all_text } else { all_text.trim().to_string() };
        if items.is_empty() {
            items.push(message_item(&format!("msg_{}", hex_id(24)), "", "completed"));
        }
        let completed = env.render("completed", items, usage(&r.usage), Some(&r));
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
