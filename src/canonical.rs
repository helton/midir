//! Protocol-neutral request and response. Every protocol adapter translates to and from these types, and every
//! backend works on them, so neither side knows about the other. The serializable types are also the on-disk format
//! of the Responses store (store.rs), so their field names are stable.

use std::fmt;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config::ModelSpec;
use crate::text::char_len;

/// CAVEAT: heuristic for count_tokens, max_tokens and missing usage.
pub const CHARS_PER_TOKEN: f64 = 4.0;

pub fn custom_tool_params() -> Value {
    json!({"type": "object", "properties": {"input": {"type": "string", "description": "The raw text input for this tool (free-form, exactly as the tool expects it)"}}, "required": ["input"]})
}

pub fn empty_params() -> Value {
    json!({"type": "object", "properties": {}})
}

/// Lowercase hex of some bytes.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// `n` random hex digits (at most 32), for ids.
pub fn hex_id(n: usize) -> String {
    let mut buf = [0u8; 16];
    let _ = getrandom::getrandom(&mut buf);
    hex(&buf)[..n.min(32)].to_string()
}

pub fn new_call_id() -> String {
    format!("call_{}", hex_id(24))
}

pub fn estimate_tokens(s: &str) -> i64 {
    ((char_len(s) as f64 / CHARS_PER_TOKEN) as i64).max(1)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
}

impl Usage {
    pub fn new(prompt: i64, completion: i64) -> Self {
        Usage { prompt_tokens: prompt, completion_tokens: completion, total_tokens: prompt + completion }
    }

    pub fn add(&self, o: &Usage) -> Usage {
        Usage::new(self.prompt_tokens + o.prompt_tokens, self.completion_tokens + o.completion_tokens)
    }
}

impl fmt::Display for Usage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}+{} tokens", self.prompt_tokens, self.completion_tokens)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// A JSON Schema object (an empty object schema when the client sent none).
    #[serde(default = "empty_params")]
    pub parameters: Value,
    /// A free-form text tool (Responses `custom`): its single argument is `input`.
    #[serde(default)]
    pub custom: bool,
}

impl ToolSpec {
    pub fn new(name: impl Into<String>, description: impl Into<String>, parameters: Option<Value>, custom: bool) -> Self {
        let parameters = parameters.filter(Value::is_object).unwrap_or_else(empty_params);
        ToolSpec { name: name.into(), description: description.into(), parameters, custom }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// A JSON object; a raw string only when a client's history held invalid JSON (CAVEAT).
    pub arguments: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub call_id: String,
    pub content: String,
    /// the tool's name when the client sent it (else looked up from the call)
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Turn {
    /// "user" or "assistant"
    pub role: String,
    pub text: String,
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default)]
    pub tool_results: Vec<ToolResult>,
    /// text that came after the tool results (OpenClaw's runtime context, Claude Code's reminders)
    #[serde(default)]
    pub after: String,
}

impl Turn {
    pub fn new(role: &str, text: &str) -> Self {
        Turn { role: role.into(), text: text.into(), tool_calls: vec![], tool_results: vec![], after: String::new() }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum ToolChoice {
    Auto,
    None,
    Required,
    Named(String),
}

impl ToolChoice {
    /// The forced choices: the response must hold a tool call.
    pub fn forced(&self) -> bool {
        matches!(self, ToolChoice::Required | ToolChoice::Named(_))
    }
}

impl fmt::Display for ToolChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ToolChoice::Auto => f.write_str("auto"),
            ToolChoice::None => f.write_str("none"),
            ToolChoice::Required => f.write_str("required"),
            ToolChoice::Named(n) => write!(f, "tool:{n}"),
        }
    }
}

/// Telemetry labels and counters for one request, shared with its follow-up requests.
#[derive(Debug, Clone, Default)]
pub struct Meta {
    pub client: String,
    pub client_version: String,
    pub session: String,
    pub initiator: String,
    pub protocol: String,
    pub model: String,
    pub agent: String,
    pub backend: String,
    pub stream: bool,
    pub tools_declared: i64,
    pub json_mode: bool,
    pub prev_id: Option<String>,
    pub prev_turns: usize,
    pub followups: i64,
    pub parse_errors: i64,
    pub repairs: i64,
    pub upstream_calls: i64,
    pub queue_wait_ms: Option<f64>,
    pub prompt_chars: i64,
    pub dropped_turns: i64,
    pub usage_estimated: bool,
}

pub type SharedMeta = Arc<Mutex<Meta>>;

#[derive(Debug, Clone)]
pub struct CanonicalRequest {
    pub system: Vec<String>,
    pub turns: Vec<Turn>,
    pub tools: Vec<ToolSpec>,
    pub tool_choice: ToolChoice,
    /// `{"type": "object"}` alone means "any JSON object"
    pub json_schema: Option<Value>,
    pub stop: Vec<String>,
    pub max_tokens: Option<i64>,
    /// accepted parameters without effect (for the log)
    pub ignored: Vec<String>,
    pub route: Option<Arc<ModelSpec>>,
    pub meta: SharedMeta,
}

impl Default for CanonicalRequest {
    fn default() -> Self {
        CanonicalRequest {
            system: vec![],
            turns: vec![],
            tools: vec![],
            tool_choice: ToolChoice::Auto,
            json_schema: None,
            stop: vec![],
            max_tokens: None,
            ignored: vec![],
            route: None,
            meta: Arc::new(Mutex::new(Meta::default())),
        }
    }
}

impl CanonicalRequest {
    /// Append content, merging with the previous turn when the role repeats. An assistant message with no text and
    /// no calls adds nothing.
    pub fn add(&mut self, role: &str, text: &str, tool_calls: Vec<ToolCall>, tool_results: Vec<ToolResult>) {
        if role == "assistant" && text.trim().is_empty() && tool_calls.is_empty() && tool_results.is_empty() {
            return;
        }
        if let Some(t) = self.turns.last_mut().filter(|t| t.role == role) {
            let join = |a: &str, b: &str| if a.is_empty() { b.to_string() } else { format!("{a}\n{b}") };
            if !text.is_empty() && (!t.tool_results.is_empty() || !t.after.is_empty()) {
                t.after = join(&t.after, text);
            } else if !text.is_empty() {
                t.text = join(&t.text, text);
            }
            t.tool_calls.extend(tool_calls);
            t.tool_results.extend(tool_results);
        } else {
            self.turns.push(Turn { role: role.into(), text: text.into(), tool_calls, tool_results, after: String::new() });
        }
    }

    pub fn add_text(&mut self, role: &str, text: &str) {
        self.add(role, text, vec![], vec![]);
    }

    pub fn tool_name_for(&self, call_id: &str) -> String {
        self.turns.iter().flat_map(|t| &t.tool_calls).find(|c| c.id == call_id).map(|c| c.name.clone()).unwrap_or_default()
    }

    pub fn custom_tool_names(&self) -> Vec<String> {
        self.tools.iter().filter(|t| t.custom).map(|t| t.name.clone()).collect()
    }

    /// Whether the tool protocol is on for this request.
    pub fn tools_on(&self) -> bool {
        !self.tools.is_empty() && self.tool_choice != ToolChoice::None
    }

    /// Copy for a follow-up call (retry, repair), keeping system and turns; meta and route are shared.
    pub fn derive(&self) -> CanonicalRequest {
        CanonicalRequest { ignored: vec![], json_schema: None, max_tokens: None, ..self.clone() }
    }

    pub fn meta(&self) -> std::sync::MutexGuard<'_, Meta> {
        self.meta.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finish {
    Stop,
    ToolCalls,
    Length,
    StopSequence,
}

impl Finish {
    pub fn as_str(self) -> &'static str {
        match self {
            Finish::Stop => "stop",
            Finish::ToolCalls => "tool_calls",
            Finish::Length => "length",
            Finish::StopSequence => "stop_sequence",
        }
    }

    /// From the on-disk name (unknown names are a plain stop).
    pub fn parse(s: &str) -> Finish {
        match s {
            "tool_calls" => Finish::ToolCalls,
            "length" => Finish::Length,
            "stop_sequence" => Finish::StopSequence,
            _ => Finish::Stop,
        }
    }
}

impl fmt::Display for Finish {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct CanonicalResponse {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish: Finish,
    pub stop_sequence: Option<String>,
    pub usage: Usage,
    /// the backend's own id for the answer, when it sent one
    pub message_id: Option<String>,
    /// raw <tool_call> content dropped as invalid JSON
    pub rejected_calls: Vec<String>,
}

impl Default for CanonicalResponse {
    fn default() -> Self {
        CanonicalResponse {
            text: String::new(),
            tool_calls: vec![],
            finish: Finish::Stop,
            stop_sequence: None,
            usage: Usage::default(),
            message_id: None,
            rejected_calls: vec![],
        }
    }
}

/// One item of a response stream.
#[derive(Debug, Clone)]
pub enum Event {
    Text(String),
    ToolCall(ToolCall),
    Done(CanonicalResponse),
    /// nothing yet: the transport should send a keepalive
    Keepalive,
}
