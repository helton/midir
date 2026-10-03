//! Helpers shared by the protocol adapters: content parts as text, tool arguments, ignored
//! parameters, and the guard that turns malformed input into a 400.

use serde_json::{json, Value};

use crate::canonical::CanonicalRequest;
use crate::errors::ClientError;
use crate::py::json as pyjson;
use crate::py::obj::{self, PyErr, PyResult};
use crate::py::text;

pub const MEDIA_TYPES: [&str; 8] = ["image_url", "input_image", "image", "input_audio", "audio", "file", "input_file", "document"];
const LOG: &str = "midir.protocols.common";

/// `t in MEDIA_TYPES` (a set: an unhashable `t` raises TypeError).
pub fn is_media(t: &Value) -> PyResult<bool> {
    obj::hashable(t)?;
    Ok(matches!(t, Value::String(s) if MEDIA_TYPES.contains(&s.as_str())))
}

/// OpenAI-style content (string or list of parts) as text. Media parts become a placeholder (CAVEAT: text-only API).
pub fn text_of(content: Option<&Value>, where_: &str) -> PyResult<String> {
    let content = match content {
        None | Some(Value::Null) => return Ok(String::new()),
        Some(Value::String(s)) => return Ok(s.clone()),
        Some(Value::Array(a)) => a,
        Some(other) => return Ok(text::str_of(other)),
    };
    let mut parts: Vec<String> = vec![];
    for p in content {
        match p {
            Value::String(s) => parts.push(s.clone()),
            Value::Object(m) => {
                let empty = json!("");
                let t = m.get("type").unwrap_or(&empty);
                if obj::is_str(Some(t), "text") || obj::is_str(Some(t), "input_text") || obj::is_str(Some(t), "output_text") {
                    parts.push(text::str_of(m.get("text").unwrap_or(&empty)));
                } else if obj::is_str(Some(t), "refusal") {
                    parts.push(text::str_of(m.get("refusal").unwrap_or(&empty)));
                } else if is_media(t)? {
                    let t = text::str_of(t);
                    crate::warn!(LOG, "{where_}: '{t}' content replaced by a placeholder (the StackSpot Agent API is text-only)");
                    parts.push(format!("[{t} omitted: this model only receives text]"));
                } else {
                    parts.push(pyjson::dumps(p, pyjson::DEFAULT));
                }
            }
            _ => continue,
        }
    }
    Ok(parts.join("\n"))
}

pub fn text_of_value(content: &Value, where_: &str) -> PyResult<String> {
    text_of(Some(content), where_)
}

pub fn parse_arguments(raw: Option<&Value>) -> Value {
    match raw {
        Some(Value::String(s)) => {
            if text::is_blank(s) {
                json!({})
            } else {
                pyjson::loads(s).unwrap_or_else(|_| Value::String(s.clone()))
            }
        }
        None | Some(Value::Null) => json!({}),
        Some(other) => other.clone(),
    }
}

pub fn arguments_str(a: &Value) -> String {
    match a {
        Value::String(s) => s.clone(),
        other => pyjson::dumps(other, pyjson::DEFAULT),
    }
}

/// Parameters accepted without effect: present and not None, False, [] or {}.
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

pub fn tool_params(p: Option<&Value>) -> Value {
    match p {
        Some(v) if text::truthy(v) => v.clone(),
        _ => json!({"type": "object", "properties": {}}),
    }
}

/// Raw `max_tokens`-style value from the body, validated by `to_canonical`.
pub struct Adapted {
    pub req: CanonicalRequest,
    pub max_tokens: Option<Value>,
}

/// Adapter call where malformed input is a 400, never a 500.
pub fn to_canonical(adapter: &str, result: PyResult<Result<Adapted, ClientError>>) -> Result<CanonicalRequest, ClientError> {
    let adapted = match result {
        Ok(Ok(a)) => a,
        Ok(Err(e)) => return Err(e),
        Err(PyErr { kind, msg }) => {
            crate::warn!(LOG, "malformed {adapter} request: {kind}({})", text::repr_str(&msg));
            return Err(ClientError::new(
                format!("malformed request ({kind}: {msg}); check the types of messages/input, content, tools and system"),
                "invalid_request",
            ));
        }
    };
    let mut req = adapted.req;
    match adapted.max_tokens {
        None | Some(Value::Null) => {}
        Some(v) => {
            let ok = match &v {
                Value::Number(n) if text::is_int(&v) => n.as_i64().map_or(n.as_u64().is_some(), |i| i > 0),
                _ => false,
            };
            if !ok {
                return Err(ClientError::new(format!("max_tokens must be a positive integer, got {}", text::repr(&v)), "invalid_request"));
            }
            req.max_tokens = Some(v.as_i64().unwrap_or(i64::MAX));
        }
    }
    Ok(req)
}
