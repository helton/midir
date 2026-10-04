//! Anthropic Messages <-> canonical (`POST /v1/messages`, `POST /v1/messages/count_tokens`).

use futures::stream::BoxStream;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use super::common::{decode, ignored_params, placeholder, positive, stops, tool_params, Stop, MEDIA_TYPES};
use crate::canonical::{hex_id, CanonicalRequest, CanonicalResponse, Event, Finish, ToolCall, ToolChoice, ToolResult, ToolSpec, Usage};
use crate::errors::{ClientError, Error};
use crate::text::readable_json;

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
/// Blocks with no equivalent for a text backend: dropped from the history.
const SKIPPED_BLOCKS: [&str; 4] = ["thinking", "redacted_thinking", "server_tool_use", "web_search_tool_result"];

#[derive(Deserialize)]
struct Request {
    messages: Option<Vec<Message>>,
    system: Option<Blocks>,
    #[serde(default)]
    tools: Vec<Tool>,
    tool_choice: Option<Choice>,
    stop_sequences: Option<Stop>,
    max_tokens: Option<i64>,
    output_config: Option<OutputConfig>,
}

#[derive(Deserialize)]
#[serde(expecting = "a message object")]
struct Message {
    role: Option<String>,
    content: Option<Blocks>,
}

/// A string or a list of content blocks.
#[derive(Deserialize)]
#[serde(untagged, expecting = "a string or a list of content blocks")]
enum Blocks {
    Text(String),
    List(Vec<Block>),
}

#[derive(Deserialize, Serialize)]
#[serde(expecting = "a content block object")]
struct Block {
    #[serde(rename = "type", default, skip_serializing_if = "String::is_empty")]
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    input: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_use_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    content: Option<BlockContent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    is_error: Option<bool>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

/// A tool result's content: a string or blocks.
#[derive(Deserialize, Serialize)]
#[serde(untagged, expecting = "a string or a list of content blocks")]
enum BlockContent {
    Text(String),
    List(Vec<Block>),
}

#[derive(Deserialize)]
#[serde(expecting = "a tool object")]
struct Tool {
    #[serde(rename = "type")]
    kind: Option<String>,
    name: Option<String>,
    description: Option<String>,
    input_schema: Option<Value>,
}

#[derive(Deserialize)]
#[serde(expecting = "a tool_choice object")]
struct Choice {
    #[serde(rename = "type")]
    kind: Option<String>,
    name: Option<String>,
}

#[derive(Deserialize)]
struct OutputConfig {
    format: Option<Format>,
}

#[derive(Deserialize)]
struct Format {
    #[serde(rename = "type")]
    kind: Option<String>,
    schema: Option<Value>,
}

impl Block {
    /// A non-tool block as text: text as it is, media as a placeholder, anything else as its JSON.
    fn as_text(&self, place: &str) -> String {
        match self.kind.as_str() {
            "text" => self.text.clone().unwrap_or_default(),
            k if MEDIA_TYPES.contains(&k) => placeholder(k, place),
            _ => readable_json(self),
        }
    }
}

fn blocks_text(blocks: &[Block], place: &str) -> String {
    blocks.iter().filter(|b| !SKIPPED_BLOCKS.contains(&b.kind.as_str())).map(|b| b.as_text(place)).collect::<Vec<_>>().join("\n")
}

/// The request as a canonical request (`max_tokens` included).
pub fn to_canonical(body: &Value) -> Result<CanonicalRequest, ClientError> {
    let r: Request = decode(body)?;
    let messages = r
        .messages
        .filter(|m| !m.is_empty())
        .ok_or_else(|| ClientError::new("'messages' is required and must be a non-empty list", "invalid_request_error"))?;
    let mut req = CanonicalRequest { ignored: ignored_params(body, &IGNORED), ..Default::default() };
    match r.system {
        Some(Blocks::Text(s)) if !s.is_empty() => req.system.push(s),
        Some(Blocks::List(blocks)) if !blocks.is_empty() => req.system.push(blocks_text(&blocks, "system")),
        _ => {}
    }
    for m in messages {
        let role = if m.role.as_deref() == Some("assistant") { "assistant" } else { "user" };
        let blocks = match m.content {
            None => vec![],
            Some(Blocks::Text(s)) => {
                req.add_text(role, &s);
                continue;
            }
            Some(Blocks::List(b)) => b,
        };
        // text before the tool results belongs to the turn; text after them stays after them in the prompt
        let (mut texts, mut after, mut calls, mut results) = (vec![], vec![], vec![], vec![]);
        for b in blocks {
            match b.kind.as_str() {
                "tool_use" => calls.push(ToolCall {
                    id: b.id.filter(|i| !i.is_empty()).unwrap_or_else(|| format!("toolu_{}", hex_id(24))),
                    name: b.name.unwrap_or_default(),
                    arguments: b.input.filter(|i| !i.is_null()).unwrap_or_else(|| json!({})),
                }),
                "tool_result" => {
                    let content = match &b.content {
                        None => String::new(),
                        Some(BlockContent::Text(s)) => s.clone(),
                        Some(BlockContent::List(inner)) => blocks_text(inner, "tool_result"),
                    };
                    results.push(ToolResult {
                        call_id: b.tool_use_id.unwrap_or_default(),
                        content,
                        name: String::new(),
                        is_error: b.is_error.unwrap_or(false),
                    });
                }
                k if SKIPPED_BLOCKS.contains(&k) => {}
                _ => {
                    let text = b.as_text(role);
                    if results.is_empty() {
                        texts.push(text);
                    } else {
                        after.push(text);
                    }
                }
            }
        }
        req.add(role, &texts.join("\n"), calls, results);
        if !after.is_empty() {
            req.add_text(role, &after.join("\n"));
        }
    }
    for t in r.tools {
        // client tools have an input_schema (or type "custom"); server tools (web_search, ...) have a type and no schema
        let kind = t.kind.filter(|k| !k.is_empty()).unwrap_or_else(|| "custom".into());
        if t.input_schema.is_none() && kind != "custom" {
            req.ignored.push(format!("tool:{kind}"));
            continue;
        }
        req.tools.push(ToolSpec::new(t.name.unwrap_or_default(), t.description.unwrap_or_default(), tool_params(t.input_schema), false));
    }
    req.tool_choice = match r.tool_choice {
        None => ToolChoice::Auto,
        Some(c) => match c.kind.as_deref().unwrap_or("auto") {
            "any" => ToolChoice::Required,
            "none" => ToolChoice::None,
            "tool" => c.name.filter(|n| !n.is_empty()).map_or(ToolChoice::Required, ToolChoice::Named),
            _ => ToolChoice::Auto,
        },
    };
    req.stop = stops(r.stop_sequences);
    req.max_tokens = positive("max_tokens", r.max_tokens)?;
    if let Some(Format { kind: Some(k), schema }) = r.output_config.and_then(|o| o.format) {
        if k == "json_schema" {
            req.json_schema =
                Some(schema.filter(|s| s.as_object().is_some_and(|m| !m.is_empty())).unwrap_or_else(|| json!({"type": "object"})));
        }
    }
    Ok(req)
}

pub fn stop_reason(r: &CanonicalResponse) -> &'static str {
    match r.finish {
        Finish::ToolCalls => "tool_use",
        Finish::Length => "max_tokens",
        Finish::StopSequence => "stop_sequence",
        Finish::Stop => "end_turn",
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

/// CAVEAT: arguments that are not a JSON object arrive as {"_raw": ...}.
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
    blocks
        .extend(r.tool_calls.iter().map(|c| json!({"type": "tool_use", "id": tool_use_id(&c.id), "name": c.name, "input": tool_input(c)})));
    Value::Array(blocks)
}

pub fn response(r: &CanonicalResponse, mid: &str, model: &str) -> Value {
    json!({"id": mid, "type": "message", "role": "assistant", "model": model, "content": content_blocks(r), "stop_reason": stop_reason(r),
           "stop_sequence": r.stop_sequence, "usage": usage(&r.usage)})
}

fn ev(name: &str, data: Value) -> String {
    let mut m = Map::from_iter([("type".to_string(), json!(name))]);
    if let Value::Object(d) = data {
        m.extend(d);
    }
    format!("event: {name}\ndata: {}\n\n", Value::Object(m))
}

pub fn stream(events: BoxStream<'static, Result<Event, Error>>, mid: String, model: String) -> BoxStream<'static, Result<String, Error>> {
    Box::pin(async_stream::try_stream! {
        yield ev("message_start", json!({"message": {"id": mid, "type": "message", "role": "assistant", "model": model, "content": [], "stop_reason": null,
                 "stop_sequence": null, "usage": {"input_tokens": 0, "output_tokens": 0, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}}}));
        yield ev("ping", json!({}));
        let mut index = 0usize;
        let mut text_open = false;
        let mut done: Option<CanonicalResponse> = None;
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
                    yield ev("content_block_delta", json!({"index": index, "delta": {"type": "input_json_delta", "partial_json": tool_input(&c).to_string()}}));
                    yield ev("content_block_stop", json!({"index": index}));
                    index += 1;
                }
                Event::Keepalive => yield ev("ping", json!({})),
                Event::Done(r) => done = Some(r),
            }
        }
        if text_open {
            yield ev("content_block_stop", json!({"index": index}));
        }
        let r = done.unwrap_or_default();
        yield ev("message_delta", json!({"delta": {"stop_reason": stop_reason(&r), "stop_sequence": r.stop_sequence},
                 "usage": {"output_tokens": r.usage.completion_tokens, "input_tokens": r.usage.prompt_tokens}}));
        yield ev("message_stop", json!({}));
    })
}
