//! Anthropic Messages <-> canonical (`POST /v1/messages`, `POST /v1/messages/count_tokens`).

use std::fmt;

use futures::StreamExt;
use futures::stream::BoxStream;
use serde::de::{Deserializer, MapAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::{Map, Value, json};

use super::common::{
    MEDIA_TYPES, RequestInfo, Stop, check_named_choice, decode, ignored_params, is_true, object_fields, placeholder, positive, stops,
    tool_params,
};
use crate::canonical::{CanonicalRequest, CanonicalResponse, Event, Finish, ToolCall, ToolChoice, ToolResult, ToolSpec, Usage, hex_id};
use crate::errors::{ClientError, Error};
use crate::json;

/// Blocks with no equivalent for a text backend: dropped from the history.
const SKIPPED_BLOCKS: [&str; 4] = ["thinking", "redacted_thinking", "server_tool_use", "web_search_tool_result"];

#[derive(Deserialize)]
struct Request {
    model: Option<String>,
    stream: Option<Box<RawValue>>,
    messages: Option<Vec<Message>>,
    system: Option<Blocks>,
    tools: Option<Vec<Tool>>,
    tool_choice: Option<Choice>,
    stop_sequences: Option<Stop>,
    max_tokens: Option<i64>,
    output_config: Option<OutputConfig>,
    metadata: Option<Box<RawValue>>,
    // accepted without effect
    temperature: Option<Box<RawValue>>,
    top_p: Option<Box<RawValue>>,
    top_k: Option<Box<RawValue>>,
    thinking: Option<Box<RawValue>>,
    service_tier: Option<Box<RawValue>>,
    cache_control: Option<Box<RawValue>>,
    container: Option<Box<RawValue>>,
    mcp_servers: Option<Box<RawValue>>,
    context_management: Option<Box<RawValue>>,
}

impl Request {
    fn ignored(&self) -> Vec<String> {
        ignored_params(&[
            ("temperature", self.temperature.as_deref()),
            ("top_p", self.top_p.as_deref()),
            ("top_k", self.top_k.as_deref()),
            ("metadata", self.metadata.as_deref()),
            ("thinking", self.thinking.as_deref()),
            ("service_tier", self.service_tier.as_deref()),
            ("cache_control", self.cache_control.as_deref()),
            ("container", self.container.as_deref()),
            ("mcp_servers", self.mcp_servers.as_deref()),
            ("context_management", self.context_management.as_deref()),
        ])
    }

    /// Claude Code's session: `metadata.user_id` is a JSON string holding `{"session_id": ...}`.
    fn session(&self) -> Option<String> {
        #[derive(Deserialize)]
        struct Metadata {
            user_id: Option<String>,
        }
        #[derive(Deserialize)]
        struct UserId {
            session_id: Option<String>,
        }
        let meta: Metadata = serde_json::from_str(self.metadata.as_deref()?.get()).ok()?;
        let user: UserId = serde_json::from_str(&meta.user_id?).ok()?;
        user.session_id.filter(|s| !s.is_empty())
    }
}

#[derive(Deserialize)]
#[serde(expecting = "a message object")]
struct Message {
    role: Option<String>,
    content: Option<Blocks>,
}

/// A string or a list of content blocks (a message's content, the system prompt, a tool result's content).
enum Blocks {
    Text(String),
    List(Vec<Block>),
}

impl<'de> Deserialize<'de> for Blocks {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Blocks;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a string or a list of content blocks")
            }
            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<Blocks, E> {
                Ok(Blocks::Text(s.to_string()))
            }
            fn visit_string<E: serde::de::Error>(self, s: String) -> Result<Blocks, E> {
                Ok(Blocks::Text(s))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Blocks, A::Error> {
                let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0));
                while let Some(b) = seq.next_element()? {
                    out.push(b);
                }
                Ok(Blocks::List(out))
            }
        }
        d.deserialize_any(V)
    }
}

impl Serialize for Blocks {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Blocks::Text(t) => t.serialize(s),
            Blocks::List(l) => l.serialize(s),
        }
    }
}

/// A content block: the fields Midir reads, and the others in order (an unknown block is shown as its JSON).
#[derive(Default, Serialize)]
struct Block {
    #[serde(rename = "type", skip_serializing_if = "String::is_empty")]
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_use_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<Blocks>,
    #[serde(skip_serializing_if = "Option::is_none")]
    is_error: Option<bool>,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

impl<'de> Deserialize<'de> for Block {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Block;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a content block object")
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Block, A::Error> {
                let mut b = Block::default();
                let mut kind = None;
                let extra = object_fields(map, "a content block object", |key, map| {
                    match key {
                        "type" => kind = map.next_value::<Option<String>>()?,
                        "text" => b.text = map.next_value()?,
                        "id" => b.id = map.next_value()?,
                        "name" => b.name = map.next_value()?,
                        "input" => b.input = map.next_value()?,
                        "tool_use_id" => b.tool_use_id = map.next_value()?,
                        "content" => b.content = map.next_value()?,
                        "is_error" => b.is_error = map.next_value()?,
                        _ => return Ok(false),
                    }
                    Ok(true)
                })?;
                (b.kind, b.extra) = (kind.unwrap_or_default(), extra);
                Ok(b)
            }
        }
        d.deserialize_map(V)
    }
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
    disable_parallel_tool_use: Option<bool>,
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
    fn into_text(self, place: &str) -> String {
        match self.kind.as_str() {
            "text" => self.text.unwrap_or_default(),
            k if MEDIA_TYPES.contains(&k) => placeholder(k, place),
            _ => json::readable(&self),
        }
    }
}

fn blocks_text(blocks: Vec<Block>, place: &str) -> String {
    blocks.into_iter().filter(|b| !SKIPPED_BLOCKS.contains(&b.kind.as_str())).map(|b| b.into_text(place)).collect::<Vec<_>>().join("\n")
}

/// The request as a canonical request (`max_tokens` included) and what the HTTP layer needs from it.
pub fn to_canonical(body: &[u8]) -> Result<(CanonicalRequest, RequestInfo), ClientError> {
    let r: Request = decode(body)?;
    let mut req = CanonicalRequest { ignored: r.ignored(), ..Default::default() };
    let info = RequestInfo {
        model: r.model.clone().unwrap_or_default(),
        stream: is_true(r.stream.as_deref()),
        session: r.session(),
        ..Default::default()
    };
    let messages = r
        .messages
        .filter(|m| !m.is_empty())
        .ok_or_else(|| ClientError::new("'messages' is required and must be a non-empty list", "invalid_request_error"))?;
    match r.system {
        Some(Blocks::Text(s)) if !s.is_empty() => req.system = vec![s].into(),
        Some(Blocks::List(blocks)) if !blocks.is_empty() => req.system = vec![blocks_text(blocks, "system")].into(),
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
                    let content = match b.content {
                        None => String::new(),
                        Some(Blocks::Text(s)) => s,
                        Some(Blocks::List(inner)) => blocks_text(inner, "tool_result"),
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
                    let text = b.into_text(role);
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
    let mut tools = vec![];
    for t in r.tools.into_iter().flatten() {
        // client tools have an input_schema (or type "custom"); server tools (web_search, ...) have a type and no schema
        let kind = t.kind.filter(|k| !k.is_empty()).unwrap_or_else(|| "custom".into());
        if t.input_schema.is_none() && kind != "custom" {
            req.ignored.push(format!("tool:{kind}"));
            continue;
        }
        tools.push(ToolSpec::new(t.name.unwrap_or_default(), t.description.unwrap_or_default(), tool_params(t.input_schema), false));
    }
    if let Some(c) = &r.tool_choice {
        req.parallel_tool_calls = !c.disable_parallel_tool_use.unwrap_or(false);
    }
    req.tool_choice = match r.tool_choice {
        None => ToolChoice::Auto,
        Some(c) => match c.kind.as_deref().unwrap_or("auto") {
            "any" => ToolChoice::Required,
            "none" => ToolChoice::None,
            "tool" => match c.name.filter(|n| !n.is_empty()) {
                Some(n) => {
                    check_named_choice(&n, &tools)?;
                    ToolChoice::Named(n)
                }
                None => ToolChoice::Required,
            },
            _ => ToolChoice::Auto,
        },
    };
    req.tools = tools.into();
    req.stop = stops(r.stop_sequences);
    req.max_tokens = positive("max_tokens", r.max_tokens)?;
    if let Some(Format { kind: Some(k), schema }) = r.output_config.and_then(|o| o.format)
        && k == "json_schema"
    {
        req.json_schema =
            Some(schema.filter(|s| s.as_object().is_some_and(|m| !m.is_empty())).unwrap_or_else(|| json!({"type": "object"})));
    }
    Ok((req, info))
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
    if call_id.starts_with("toolu_") { call_id.to_string() } else { format!("toolu_{}", call_id.strip_prefix("call_").unwrap_or(call_id)) }
}

/// CAVEAT: arguments that are not a JSON object arrive as {"_raw": ...}.
pub fn tool_input(c: &ToolCall) -> Value {
    if c.arguments.is_object() { c.arguments.clone() } else { json!({"_raw": c.arguments}) }
}

pub fn content_blocks(r: &CanonicalResponse) -> Value {
    let mut blocks = vec![];
    // an empty reply is an empty content list, as Anthropic's API answers it (never an empty text block)
    if !r.text.is_empty() {
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

/// The stream starts once the prompt is rendered, so `message_start` can report its estimated input tokens (clients
/// track context usage from it); the real count follows in `message_delta`.
pub fn stream(events: BoxStream<'static, Result<Event, Error>>, mid: String, model: String) -> BoxStream<'static, Result<String, Error>> {
    Box::pin(async_stream::try_stream! {
        let mut events = events;
        let first = events.next().await;
        let input_tokens = match &first {
            Some(Ok(Event::Prompt { tokens })) => *tokens,
            _ => 0,
        };
        yield ev("message_start", json!({"message": {"id": mid, "type": "message", "role": "assistant", "model": model, "content": [], "stop_reason": null,
                 "stop_sequence": null, "usage": {"input_tokens": input_tokens, "output_tokens": 0, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}}}));
        yield ev("ping", json!({}));
        let mut index = 0usize;
        let mut text_open = false;
        let mut done: Option<CanonicalResponse> = None;
        let mut pending = first;
        loop {
            let next = match pending.take() {
                Some(e) => Some(e),
                None => events.next().await,
            };
            let Some(e) = next else { break };
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
                Event::Prompt { .. } => {}
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
