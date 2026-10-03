//! Protocol-neutral request and response. Every protocol adapter translates to and from these types,
//! and every backend works on them, so neither side knows about the other.

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::config::ModelSpec;
use crate::py::text;

/// CAVEAT: heuristic for count_tokens, max_tokens and missing usage.
pub const CHARS_PER_TOKEN: f64 = 4.0;

pub fn custom_tool_params() -> Value {
    json!({"type": "object", "properties": {"input": {"type": "string", "description": "The raw text input for this tool (free-form, exactly as the tool expects it)"}}, "required": ["input"]})
}

/// Lowercase hex of some bytes.
pub fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// `uuid.uuid4().hex[:n]`.
pub fn hex_id(n: usize) -> String {
    let mut buf = [0u8; 16];
    let _ = getrandom::getrandom(&mut buf);
    hex(&buf)[..n.min(32)].to_string()
}

pub fn new_call_id() -> String {
    format!("call_{}", hex_id(24))
}

pub fn estimate_tokens(s: &str) -> i64 {
    ((text::len(s) as f64 / CHARS_PER_TOKEN) as i64).max(1)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
}

impl Usage {
    pub fn add(&self, o: &Usage) -> Usage {
        Usage {
            prompt_tokens: self.prompt_tokens + o.prompt_tokens,
            completion_tokens: self.completion_tokens + o.completion_tokens,
            total_tokens: self.total_tokens + o.total_tokens,
        }
    }

    pub fn to_json(self) -> Value {
        json!({"prompt_tokens": self.prompt_tokens, "completion_tokens": self.completion_tokens, "total_tokens": self.total_tokens})
    }

    pub fn repr(&self) -> String {
        format!(
            "{{'prompt_tokens': {}, 'completion_tokens': {}, 'total_tokens': {}}}",
            self.prompt_tokens, self.completion_tokens, self.total_tokens
        )
    }
}

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
    pub custom: bool,
}

impl ToolSpec {
    /// The dataclass with its __post_init__: names and descriptions as text, parameters always an object.
    pub fn new(name: Option<&Value>, description: Option<&Value>, parameters: Value, custom: bool) -> Self {
        let parameters = if parameters.is_object() { parameters } else { json!({"type": "object", "properties": {}}) };
        ToolSpec { name: text::str_or_empty(name), description: text::str_or_empty(description), parameters, custom }
    }

    pub fn to_json(&self) -> Value {
        json!({"name": self.name, "description": self.description, "parameters": self.parameters, "custom": self.custom})
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// A JSON value (an object); a raw string only when a client sent invalid JSON in its history (CAVEAT).
    pub arguments: Value,
}

impl ToolCall {
    pub fn to_json(&self) -> Value {
        json!({"id": self.id, "name": self.name, "arguments": self.arguments})
    }
}

#[derive(Debug, Clone)]
pub struct ToolResult {
    pub call_id: String,
    pub content: String,
    pub name: String,
    pub is_error: bool,
}

impl ToolResult {
    pub fn to_json(&self) -> Value {
        json!({"call_id": self.call_id, "content": self.content, "name": self.name, "is_error": self.is_error})
    }
}

#[derive(Debug, Clone)]
pub struct Turn {
    pub role: String,
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    pub tool_results: Vec<ToolResult>,
    /// text that came after the tool results (OpenClaw's runtime context, Claude Code's reminders)
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
    /// `{"name": ...}` (the value as the client sent it)
    Named(Value),
}

impl ToolChoice {
    /// How the log shows it (`choice=%s`).
    pub fn display(&self) -> String {
        match self {
            ToolChoice::Auto => "auto".into(),
            ToolChoice::None => "none".into(),
            ToolChoice::Required => "required".into(),
            ToolChoice::Named(n) => format!("{{'name': {}}}", text::repr(n)),
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
    pub labeled: bool,
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
    pub fn add(&mut self, role: &str, text_: &str, tool_calls: Vec<ToolCall>, tool_results: Vec<ToolResult>) {
        if role == "assistant" && text::is_blank(text_) && tool_calls.is_empty() && tool_results.is_empty() {
            return;
        }
        if let Some(t) = self.turns.last_mut().filter(|t| t.role == role) {
            if !text_.is_empty() && (!t.tool_results.is_empty() || !t.after.is_empty()) {
                t.after = if t.after.is_empty() { text_.to_string() } else { format!("{}\n{text_}", t.after) };
            } else if !text_.is_empty() {
                t.text = if t.text.is_empty() { text_.to_string() } else { format!("{}\n{text_}", t.text) };
            }
            t.tool_calls.extend(tool_calls);
            t.tool_results.extend(tool_results);
        } else {
            self.turns.push(Turn { role: role.into(), text: text_.into(), tool_calls, tool_results, after: String::new() });
        }
    }

    pub fn tool_name_for(&self, call_id: &str) -> String {
        for t in &self.turns {
            for c in &t.tool_calls {
                if c.id == call_id {
                    return c.name.clone();
                }
            }
        }
        String::new()
    }

    pub fn custom_tool_names(&self) -> Vec<String> {
        self.tools.iter().filter(|t| t.custom).map(|t| t.name.clone()).collect()
    }

    /// Copy for a follow-up call (retry, repair), keeping system and turns; meta and route are shared.
    pub fn derive(
        &self,
        tools: Option<Vec<ToolSpec>>,
        tool_choice: Option<ToolChoice>,
        json_schema: Option<Value>,
        max_tokens: Option<i64>,
    ) -> CanonicalRequest {
        CanonicalRequest {
            system: self.system.clone(),
            turns: self.turns.clone(),
            tools: tools.unwrap_or_else(|| self.tools.clone()),
            tool_choice: tool_choice.unwrap_or_else(|| self.tool_choice.clone()),
            json_schema,
            stop: self.stop.clone(),
            max_tokens,
            ignored: vec![],
            route: self.route.clone(),
            meta: self.meta.clone(),
        }
    }

    pub fn meta(&self) -> std::sync::MutexGuard<'_, Meta> {
        self.meta.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Debug, Clone)]
pub struct CanonicalResponse {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
    /// stop | tool_calls | length | stop_sequence
    pub finish: String,
    pub stop_sequence: Option<String>,
    pub usage: Usage,
    pub message_id: Value,
    /// raw <tool_call> content dropped as invalid JSON
    pub rejected_calls: Vec<String>,
}

impl Default for CanonicalResponse {
    fn default() -> Self {
        CanonicalResponse {
            text: String::new(),
            tool_calls: vec![],
            finish: "stop".into(),
            stop_sequence: None,
            usage: Usage::default(),
            message_id: Value::Null,
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
