//! What the protocol adapters share: OpenAI-style content (a string or a list of parts) as text, tool arguments,
//! ignored parameters, `max_tokens` validation, and typed decoding with errors that name the offending field.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::errors::ClientError;
use crate::text::readable_json;

/// Content kinds a text-only backend cannot receive: they become a placeholder (CAVEAT).
pub const MEDIA_TYPES: [&str; 8] = ["image_url", "input_image", "image", "input_audio", "audio", "file", "input_file", "document"];

/// The request body as a protocol's typed request; a mismatch is a 400 that says where.
pub fn decode<T: DeserializeOwned>(body: &Value) -> Result<T, ClientError> {
    serde_path_to_error::deserialize(body).map_err(|e| {
        let path = e.path().to_string();
        let at = if path == "." { String::new() } else { format!("{path}: ") };
        ClientError::new(format!("invalid request: {at}{}", e.inner()), "invalid_request")
    })
}

/// OpenAI content: a string, or a list of parts (strings or typed objects).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged, expecting = "a string or a list of content parts")]
pub enum Content {
    Text(String),
    Parts(Vec<Part>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged, expecting = "a string or a content part object")]
pub enum Part {
    Text(String),
    Block(PartBlock),
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PartBlock {
    #[serde(rename = "type", default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<String>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

pub fn placeholder(kind: &str, place: &str) -> String {
    tracing::warn!("{place}: '{kind}' content replaced by a placeholder (the backend is text-only)");
    format!("[{kind} omitted: this model only receives text]")
}

impl PartBlock {
    /// Text parts as they are, refusals as their text, media as a placeholder, anything else as its JSON.
    pub fn as_text(&self, place: &str) -> String {
        match self.kind.as_str() {
            "text" | "input_text" | "output_text" => self.text.clone().unwrap_or_default(),
            "refusal" => self.refusal.clone().unwrap_or_default(),
            k if MEDIA_TYPES.contains(&k) => placeholder(k, place),
            _ => readable_json(self),
        }
    }
}

impl Content {
    /// The content as one text; parts are joined by newlines.
    pub fn as_text(&self, place: &str) -> String {
        match self {
            Content::Text(s) => s.clone(),
            Content::Parts(parts) => parts
                .iter()
                .map(|p| match p {
                    Part::Text(s) => s.clone(),
                    Part::Block(b) => b.as_text(place),
                })
                .collect::<Vec<_>>()
                .join("\n"),
        }
    }
}

pub fn text_of(content: &Option<Content>, place: &str) -> String {
    content.as_ref().map_or_else(String::new, |c| c.as_text(place))
}

/// One stop sequence or several.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged, expecting = "a string or a list of strings")]
pub enum Stop {
    One(String),
    Many(Vec<String>),
}

pub fn stops(stop: Option<Stop>) -> Vec<String> {
    match stop {
        None => vec![],
        Some(Stop::One(s)) => vec![s],
        Some(Stop::Many(v)) => v,
    }
}

/// Tool-call arguments as a JSON value: a JSON string is decoded (an empty one is `{}`); a string that is not JSON
/// stays a string (CAVEAT: invalid JSON in a client's history is passed on as text).
pub fn parse_arguments(raw: Option<Value>) -> Value {
    match raw {
        None | Some(Value::Null) => json!({}),
        Some(Value::String(s)) if s.trim().is_empty() => json!({}),
        Some(Value::String(s)) => serde_json::from_str(&s).unwrap_or(Value::String(s)),
        Some(other) => other,
    }
}

/// Arguments as the JSON text clients expect in `arguments` fields.
pub fn arguments_text(a: &Value) -> String {
    match a {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// Parameters accepted without effect: present and not null, false, empty or zero-length.
pub fn ignored_params(body: &Value, names: &[&str]) -> Vec<String> {
    names
        .iter()
        .filter(|k| match body.get(**k) {
            None | Some(Value::Null) | Some(Value::Bool(false)) => false,
            Some(Value::Array(a)) => !a.is_empty(),
            Some(Value::Object(o)) => !o.is_empty(),
            Some(_) => true,
        })
        .map(|k| k.to_string())
        .collect()
}

/// A JSON Schema for tool parameters: the client's object, else an empty object schema.
pub fn tool_params(p: Option<Value>) -> Option<Value> {
    p.filter(|v| v.as_object().is_some_and(|m| !m.is_empty()))
}

/// `max_tokens` and its equivalents must be positive.
pub fn positive(name: &str, v: Option<i64>) -> Result<Option<i64>, ClientError> {
    match v {
        Some(n) if n <= 0 => Err(ClientError::new(format!("{name} must be a positive integer, got {n}"), "invalid_request")),
        other => Ok(other),
    }
}
