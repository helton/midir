//! OpenAI Responses <-> canonical (`POST /v1/responses`, `GET /v1/responses/{id}`).
//!
//! Output item ids are derived from the response id and the item's number among its kind (`fc_<response><n>`: the
//! n-th call), so a stored response answers the same call ids later and an `item_reference` to one of its items can
//! be resolved.

use std::sync::Arc;

use futures::StreamExt;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::{Map, Value, json};

use super::common::{
    Content, ModeOr, RequestInfo, arguments_text, check_named_choice, decode, field, ignored_params, is_true, parse_arguments, positive,
    text_of, tool_params,
};
use crate::canonical::{
    CanonicalRequest, CanonicalResponse, Event, Finish, ToolCall, ToolChoice, ToolResult, ToolSpec, Usage, custom_tool_params, new_call_id,
};
use crate::errors::{ClientError, Error};
use crate::json;
use crate::store::{Echo, ResponseStore, Stored};
use crate::text::skip_chars;

/// The request fields a Response object echoes.
const ECHOED: [&str; 12] = [
    "instructions",
    "max_output_tokens",
    "metadata",
    "parallel_tool_calls",
    "previous_response_id",
    "store",
    "temperature",
    "text",
    "tool_choice",
    "tools",
    "top_p",
    "truncation",
];

/// A Responses request as sent. Fields the Response object echoes are kept as raw JSON and decoded on their own.
#[derive(Deserialize)]
pub struct Request {
    model: Option<String>,
    stream: Option<Box<RawValue>>,
    input: Option<Input>,
    instructions: Option<Box<RawValue>>,
    max_output_tokens: Option<Box<RawValue>>,
    metadata: Option<Box<RawValue>>,
    parallel_tool_calls: Option<Box<RawValue>>,
    previous_response_id: Option<Box<RawValue>>,
    store: Option<Box<RawValue>>,
    temperature: Option<Box<RawValue>>,
    text: Option<Box<RawValue>>,
    tool_choice: Option<Box<RawValue>>,
    tools: Option<Box<RawValue>>,
    top_p: Option<Box<RawValue>>,
    truncation: Option<Box<RawValue>>,
    // accepted without effect, not echoed
    reasoning: Option<Box<RawValue>>,
    service_tier: Option<Box<RawValue>>,
    user: Option<Box<RawValue>>,
    include: Option<Box<RawValue>>,
    prompt_cache_key: Option<Box<RawValue>>,
    safety_identifier: Option<Box<RawValue>>,
    background: Option<Box<RawValue>>,
}

impl Request {
    fn ignored(&self) -> Vec<String> {
        ignored_params(&[
            ("temperature", self.temperature.as_deref()),
            ("top_p", self.top_p.as_deref()),
            ("reasoning", self.reasoning.as_deref()),
            ("service_tier", self.service_tier.as_deref()),
            ("metadata", self.metadata.as_deref()),
            ("truncation", self.truncation.as_deref()),
            ("user", self.user.as_deref()),
            ("include", self.include.as_deref()),
            ("prompt_cache_key", self.prompt_cache_key.as_deref()),
            ("safety_identifier", self.safety_identifier.as_deref()),
            ("background", self.background.as_deref()),
        ])
    }

    /// The echoed fields, on one line each (they go into SSE `data:` lines).
    fn take_echo(&mut self) -> Echo {
        let fields = [
            self.instructions.take(),
            self.max_output_tokens.take(),
            self.metadata.take(),
            self.parallel_tool_calls.take(),
            self.previous_response_id.take(),
            self.store.take(),
            self.temperature.take(),
            self.text.take(),
            self.tool_choice.take(),
            self.tools.take(),
            self.top_p.take(),
            self.truncation.take(),
        ];
        ECHOED.iter().zip(fields).filter_map(|(k, v)| Some((k.to_string(), json::one_line(v?)))).collect()
    }
}

/// A string field of the body (a non-string is treated as absent: these only feed labels).
fn string(raw: Option<&RawValue>) -> Option<String> {
    serde_json::from_str::<String>(raw?.get()).ok().filter(|s| !s.is_empty())
}

pub fn decode_request(body: &[u8]) -> Result<Request, ClientError> {
    decode(body)
}

enum Input {
    Text(String),
    Items(Vec<Item>),
}

impl<'de> Deserialize<'de> for Input {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Input;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a string or a list of input items")
            }
            fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<Input, E> {
                Ok(Input::Text(s.to_string()))
            }
            fn visit_string<E: serde::de::Error>(self, s: String) -> Result<Input, E> {
                Ok(Input::Text(s))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Input, A::Error> {
                let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0));
                while let Some(item) = seq.next_element()? {
                    out.push(item);
                }
                Ok(Input::Items(out))
            }
        }
        d.deserialize_any(V)
    }
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
struct NamedChoice {
    name: Option<String>,
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

fn add_tool(tools: &mut Vec<ToolSpec>, ignored: &mut Vec<String>, t: Tool) {
    match t.kind.as_deref().unwrap_or("function") {
        "function" => {
            tools.push(ToolSpec::new(t.name.unwrap_or_default(), t.description.unwrap_or_default(), tool_params(t.parameters), false))
        }
        // CAVEAT: free-form text tool (e.g. Codex apply_patch); its grammar/format is not enforced
        "custom" => {
            let description = format!(
                "{} Free-form text tool: put the entire raw input in the single string argument \"input\".",
                t.description.unwrap_or_default()
            );
            tools.push(ToolSpec::new(t.name.unwrap_or_default(), description.trim(), Some(custom_tool_params()), true));
        }
        "namespace" => t.tools.into_iter().flatten().for_each(|inner| add_tool(tools, ignored, inner)),
        // CAVEAT: built-in tool (web_search, file_search, ...) omitted
        other => ignored.push(format!("tool:{other}")),
    }
}

/// The request as a canonical request (`max_tokens` included), what the HTTP layer needs from it and the settings the
/// Response object echoes. A `previous_response_id` brings back the stored conversation, its instructions and its
/// tools (unless the request sets its own); an `item_reference` brings back an output item of a stored response.
pub async fn to_canonical(mut r: Request, store: &Arc<ResponseStore>) -> Result<(CanonicalRequest, RequestInfo, Echo), ClientError> {
    let instructions: Option<Content> = field(r.instructions.as_deref(), "instructions")?;
    let tools_field: Option<Vec<Tool>> = field(r.tools.as_deref(), "tools")?;
    let choice: Option<ModeOr<NamedChoice>> = field(r.tool_choice.as_deref(), "tool_choice")?;
    let text: Option<TextConfig> = field(r.text.as_deref(), "text")?;
    let max_output_tokens: Option<i64> = field(r.max_output_tokens.as_deref(), "max_output_tokens")?;
    let parallel: Option<bool> = field(r.parallel_tool_calls.as_deref(), "parallel_tool_calls")?;
    let previous: Option<String> = field::<Option<String>>(r.previous_response_id.as_deref(), "previous_response_id")?.flatten();
    let previous = previous.filter(|p| !p.is_empty());
    let mut req = CanonicalRequest { ignored: r.ignored(), ..Default::default() };
    let info = RequestInfo {
        model: r.model.take().unwrap_or_default(),
        stream: is_true(r.stream.as_deref()),
        session: string(r.prompt_cache_key.as_deref()).or_else(|| string(r.user.as_deref())),
        previous_response_id: previous.clone(),
        include_usage: false,
    };
    let prev = match &previous {
        None => None,
        Some(id) => Some(store.load(id).await.ok_or_else(|| {
            ClientError::with_status(
                format!("previous_response_id '{id}' is unknown (responses are {})", store.describe()),
                "previous_response_not_found",
                404,
            )
        })?),
    };
    let instructions = instructions.map(|c| c.into_text("instructions")).filter(|t| !t.is_empty());
    let mut system: Vec<String> = vec![];
    if let Some(p) = &prev {
        req.turns = p.req.turns.clone();
        req.add("assistant", &p.resp.text, p.resp.tool_calls.clone(), vec![]);
        {
            let mut meta = req.meta();
            meta.prev_turns = req.turns.len();
            meta.prev_id = previous.clone();
        }
        if instructions.is_none() {
            system.extend(p.req.system.iter().cloned());
        }
    }
    system.extend(instructions);
    let tools_given = tools_field.is_some();
    let mut tools: Vec<ToolSpec> = vec![];
    for t in tools_field.into_iter().flatten() {
        add_tool(&mut tools, &mut req.ignored, t);
    }
    let items = match r.input.take() {
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
                let text = text_of(it.content, &role);
                match role.as_str() {
                    "system" | "developer" => system.push(text),
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
                    content: text_of(it.output, kind),
                    name: String::new(),
                    is_error: false,
                };
                req.add("user", "", vec![], vec![result]);
            }
            "item_reference" => {
                let id = it.id.unwrap_or_default();
                match resolve_reference(&id, store).await {
                    Some(Referenced::Text(t)) => req.add_text("assistant", &t),
                    Some(Referenced::Call(c)) => req.add("assistant", "", vec![c], vec![]),
                    None => {
                        return Err(ClientError::new(
                            format!("item_reference '{id}' cannot be resolved: only output items of stored responses can be referenced"),
                            "unresolved_item_reference",
                        ));
                    }
                }
            }
            "reasoning" => {} // no equivalent for a text backend
            other => return Err(ClientError::new(format!("input item '{other}' is not supported"), "unsupported_input")),
        }
    }
    // inherited instructions and tools stay shared with the stored response when they did not change
    req.system = match &prev {
        Some(p) if *p.req.system == *system => p.req.system.clone(),
        _ => system.into(),
    };
    req.tools = match (&prev, tools_given) {
        (Some(p), false) => p.req.tools.clone(),
        (Some(p), true) if *p.req.tools == *tools => p.req.tools.clone(),
        _ => tools.into(),
    };
    req.tool_choice = match choice {
        Some(ModeOr::Mode(m)) => match m.as_str() {
            "none" => ToolChoice::None,
            "required" => ToolChoice::Required,
            _ => ToolChoice::Auto,
        },
        Some(ModeOr::Object(NamedChoice { name: Some(n) })) if !n.is_empty() => {
            check_named_choice(&n, &req.tools)?;
            ToolChoice::Named(n)
        }
        _ => ToolChoice::Auto,
    };
    req.parallel_tool_calls = parallel.unwrap_or(true);
    if let Some(Format { kind: Some(k), schema }) = text.and_then(|t| t.format) {
        match k.as_str() {
            "json_object" => req.json_schema = Some(json!({"type": "object"})),
            "json_schema" => {
                req.json_schema =
                    Some(schema.filter(|s| s.as_object().is_some_and(|m| !m.is_empty())).unwrap_or_else(|| json!({"type": "object"})))
            }
            _ => {}
        }
    }
    req.max_tokens = positive("max_output_tokens", max_output_tokens)?;
    Ok((req, info, r.take_echo()))
}

// ---------------------------------------------------------------------------------------------------------------------
// output items
// ---------------------------------------------------------------------------------------------------------------------

/// The id of an output item of response `rid`: `<prefix>_<the response's 24 hex digits><n, 4 hex>`, where `n` counts
/// the items of that kind (messages, calls) in the order they were streamed, so a call keeps its id whatever text
/// comes around it.
pub fn item_id(prefix: &str, rid: &str, n: usize) -> String {
    format!("{prefix}_{}{n:04x}", skip_chars(rid, 5))
}

/// (prefix, response id, n) of an id made by `item_id`.
fn parse_item_id(id: &str) -> Option<(&str, String, usize)> {
    let (prefix, rest) = id.split_once('_')?;
    if rest.len() != 28 || !rest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    Some((prefix, format!("resp_{}", &rest[..24]), usize::from_str_radix(&rest[24..], 16).ok()?))
}

/// What a stored response's output item holds.
enum Referenced {
    Text(String),
    Call(ToolCall),
}

/// A stored response's item: its first message (the whole text as stored) or one of its calls, by kind and number.
async fn resolve_reference(id: &str, store: &Arc<ResponseStore>) -> Option<Referenced> {
    let (prefix, rid, n) = parse_item_id(id)?;
    let stored = store.load(&rid).await?;
    match prefix {
        "msg" if n == 0 && has_message(&stored.resp) => Some(Referenced::Text(stored.resp.text.clone())),
        "fc" | "ctc" => stored.resp.tool_calls.get(n).cloned().map(Referenced::Call),
        _ => None,
    }
}

/// Whether a response's output has a message item: when there is text, or nothing else.
fn has_message(r: &CanonicalResponse) -> bool {
    !r.text.is_empty() || r.tool_calls.is_empty()
}

pub fn usage(u: &Usage) -> Value {
    json!({"input_tokens": u.prompt_tokens, "output_tokens": u.completion_tokens, "total_tokens": u.total_tokens,
           "input_tokens_details": {"cached_tokens": 0}, "output_tokens_details": {"reasoning_tokens": 0}})
}

/// A call's output item; `n` is the call's number among the response's calls.
pub fn call_item(c: &ToolCall, rid: &str, n: usize, custom_names: &[String], status: &str) -> Value {
    if custom_names.contains(&c.name) {
        let input = match &c.arguments {
            Value::Object(m) => m.get("input").cloned().unwrap_or(json!("")),
            other => json!(arguments_text(other)),
        };
        return json!({"type": "custom_tool_call", "id": item_id("ctc", rid, n), "call_id": c.id, "name": c.name, "input": input, "status": status});
    }
    json!({"type": "function_call", "id": item_id("fc", rid, n), "call_id": c.id, "name": c.name, "arguments": arguments_text(&c.arguments), "status": status})
}

pub fn message_item(msg_id: &str, text: &str, status: &str) -> Value {
    json!({"type": "message", "id": msg_id, "status": status, "role": "assistant", "content": [{"type": "output_text", "text": text, "annotations": []}]})
}

/// The output of a stored or non-streamed response: its message (all its text), then its calls.
pub fn output_items(r: &CanonicalResponse, rid: &str, custom_names: &[String]) -> Vec<Value> {
    let message = has_message(r).then(|| message_item(&item_id("msg", rid, 0), &r.text, "completed"));
    message.into_iter().chain(r.tool_calls.iter().enumerate().map(|(n, c)| call_item(c, rid, n, custom_names, "completed"))).collect()
}

// ---------------------------------------------------------------------------------------------------------------------
// the Response object
// ---------------------------------------------------------------------------------------------------------------------

/// An echoed field: the client's raw JSON, or the default when the client did not set it.
enum Echoed<'a> {
    Raw(&'a RawValue),
    Default(Value),
}

impl Serialize for Echoed<'_> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            Echoed::Raw(r) => r.serialize(s),
            Echoed::Default(v) => v.serialize(s),
        }
    }
}

#[derive(Serialize)]
pub struct ResponseObject<'a> {
    id: &'a str,
    object: &'static str,
    created_at: i64,
    status: &'a str,
    error: Option<()>,
    incomplete_details: Value,
    instructions: Echoed<'a>,
    max_output_tokens: Echoed<'a>,
    model: &'a str,
    output: Vec<Value>,
    parallel_tool_calls: Echoed<'a>,
    previous_response_id: Echoed<'a>,
    reasoning: Value,
    store: Echoed<'a>,
    temperature: Echoed<'a>,
    text: Echoed<'a>,
    tool_choice: Echoed<'a>,
    tools: Echoed<'a>,
    top_p: Echoed<'a>,
    truncation: Echoed<'a>,
    usage: Value,
    user: Option<()>,
    metadata: Echoed<'a>,
}

/// The Response object around `output`, echoing the request's settings.
pub struct Envelope<'a> {
    pub rid: &'a str,
    pub created: i64,
    pub model: &'a str,
    pub echo: &'a Echo,
}

impl Envelope<'_> {
    pub fn render<'s>(&'s self, status: &'s str, output: Vec<Value>, usage: Value, r: Option<&CanonicalResponse>) -> ResponseObject<'s> {
        let incomplete = r.is_some_and(|r| r.finish == Finish::Length);
        let echo = |key: &str, default: Value| self.echo.get(key).map_or(Echoed::Default(default), |raw| Echoed::Raw(raw));
        let echo_nonempty = |key: &str, default: Value| match self.echo.get(key) {
            Some(raw) if json::is_meaningful(raw) => Echoed::Raw(raw),
            _ => Echoed::Default(default),
        };
        ResponseObject {
            id: self.rid,
            object: "response",
            created_at: self.created,
            status: if incomplete { "incomplete" } else { status },
            error: None,
            incomplete_details: if incomplete { json!({"reason": "max_output_tokens"}) } else { Value::Null },
            instructions: echo("instructions", Value::Null),
            max_output_tokens: echo("max_output_tokens", Value::Null),
            model: self.model,
            output,
            parallel_tool_calls: echo("parallel_tool_calls", json!(true)),
            previous_response_id: echo("previous_response_id", Value::Null),
            reasoning: json!({"effort": null, "summary": null}),
            store: echo("store", json!(true)),
            temperature: echo("temperature", json!(1.0)),
            text: echo_nonempty("text", json!({"format": {"type": "text"}})),
            tool_choice: echo("tool_choice", json!("auto")),
            tools: echo_nonempty("tools", json!([])),
            top_p: echo("top_p", json!(1.0)),
            truncation: echo("truncation", json!("disabled")),
            usage,
            user: None,
            metadata: echo_nonempty("metadata", json!({})),
        }
    }
}

/// Whether the client asked for the response to be kept (`store`, default true).
fn wants_store(echo: &Echo) -> bool {
    echo.get("store").is_none_or(|raw| raw.get().trim() != "false")
}

/// The non-streaming answer, stored for `previous_response_id` unless the client said `store: false`.
pub async fn complete_response(mut stored: Stored, r: CanonicalResponse, store: &Arc<ResponseStore>) -> String {
    let body = {
        let env = Envelope { rid: &stored.id, created: stored.created as i64, model: &stored.model, echo: &stored.echo };
        let items = output_items(&r, env.rid, &stored.req.custom_tool_names());
        serde_json::to_string(&env.render("completed", items, usage(&r.usage), Some(&r))).unwrap_or_default()
    };
    if wants_store(&stored.echo) {
        stored.resp = Arc::new(r);
        store.remember(stored).await;
    }
    body
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

    /// An event that carries the whole Response object.
    fn response(&mut self, name: &str, response: ResponseObject) -> String {
        #[derive(Serialize)]
        struct Data<'a> {
            #[serde(rename = "type")]
            kind: &'a str,
            sequence_number: u64,
            response: ResponseObject<'a>,
        }
        self.0 += 1;
        let data = serde_json::to_string(&Data { kind: name, sequence_number: self.0, response }).unwrap_or_default();
        format!("event: {name}\ndata: {data}\n\n")
    }
}

/// Each output item keeps one identity: a message that resumes after a tool call is a NEW message item with its own id
/// and only its own text. The final event (`response.completed`, or `response.incomplete` after a max_output_tokens
/// cut) lists the items in the order they were streamed.
pub fn stream(
    events: BoxStream<'static, Result<Event, Error>>,
    stored: Stored,
    store: Arc<ResponseStore>,
) -> BoxStream<'static, Result<String, Error>> {
    Box::pin(async_stream::try_stream! {
        let mut stored = stored;
        let rid = stored.id.clone();
        let echo = stored.echo.clone();
        let model = stored.model.clone();
        let env = Envelope { rid: &rid, created: stored.created as i64, model: &model, echo: &echo };
        let mut seq = Seq(0);
        let custom_names = stored.req.custom_tool_names();
        yield seq.response("response.created", env.render("in_progress", vec![], Value::Null, None));
        yield seq.response("response.in_progress", env.render("in_progress", vec![], Value::Null, None));
        let mut items: Vec<Value> = vec![];
        let (mut n_messages, mut n_calls) = (0usize, 0usize);
        let mut msg_id = String::new();
        let mut msg_text = String::new();
        let mut all_text = String::new();
        let mut done: Option<CanonicalResponse> = None;
        let mut events = events;
        while let Some(e) = events.next().await {
            match e? {
                Event::Text(t) => {
                    if msg_id.is_empty() {
                        msg_id = item_id("msg", &rid, n_messages);
                        n_messages += 1;
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
                    let index = items.len();
                    let item = call_item(&c, &rid, n_calls, &custom_names, "in_progress");
                    n_calls += 1;
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
                Event::Prompt { .. } => {}
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
            items.push(message_item(&item_id("msg", &rid, 0), "", "completed"));
        }
        let name = if r.finish == Finish::Length { "response.incomplete" } else { "response.completed" };
        let last = seq.response(name, env.render("completed", items, usage(&r.usage), Some(&r)));
        if wants_store(&echo) {
            stored.resp = Arc::new(r);
            store.remember(stored).await;
        }
        yield last;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_ids_round_trip() {
        let rid = format!("resp_{}", "ab12".repeat(6));
        let id = item_id("fc", &rid, 3);
        assert_eq!(id, format!("fc_{}0003", "ab12".repeat(6)));
        assert_eq!(parse_item_id(&id), Some(("fc", rid, 3)));
        assert_eq!(parse_item_id("msg_123"), None);
    }
}
