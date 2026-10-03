//! Model text -> text and tool-call events, incrementally.

use std::collections::HashSet;
use std::sync::LazyLock;

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::canonical::{new_call_id, ToolCall, ToolSpec};
use crate::py::json as pyjson;
use crate::py::text;

const LOG: &str = "midir.emulation.parser";
pub const OPEN_TAG: &str = "<tool_call";
pub const CLOSE_TAG: &str = "</tool_call>";
static OPEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)^<tool_call(?:[\s\x1c-\x1f]+id[\s\x1c-\x1f]*=[\s\x1c-\x1f]*"?([\w.-]*)"?)?[\s\x1c-\x1f]*>"#).unwrap()
});
static FENCE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)^[\s\x1c-\x1f]*```(?:json)?[\s\x1c-\x1f]*|[\s\x1c-\x1f]*```[\s\x1c-\x1f]*$").unwrap());
static LOOKS_LIKE_CALL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""(name|arguments|parameters|input|tool|function)"\s*:"#).unwrap());
static SALVAGE_NAME_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""name"\s*:\s*"([^"]+)""#).unwrap());
static SALVAGE_ARGS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""(?:arguments|parameters|input)"\s*:\s*"#).unwrap());

/// `FENCE_RE.sub("", s)`.
pub fn strip_fences(s: &str) -> String {
    FENCE_RE.replace_all(s, "").into_owned()
}

#[derive(Debug, Clone)]
pub enum Parsed {
    Text(String),
    Call(ToolCall),
}

/// Feed text deltas; get text and tool-call events. Holds back any suffix that could be the start of "<tool_call".
pub struct ToolCallParser {
    buf: String,
    in_call: bool,
    pub errors: Vec<String>,
    /// raw content of calls dropped because their JSON could not be decoded
    pub rejected: Vec<String>,
    tools: Vec<ToolSpec>,
    saw_call: bool,
}

impl ToolCallParser {
    pub fn new(tools: Vec<ToolSpec>) -> Self {
        ToolCallParser { buf: String::new(), in_call: false, errors: vec![], rejected: vec![], tools, saw_call: false }
    }

    pub fn feed(&mut self, delta: &str) -> Vec<Parsed> {
        self.buf.push_str(delta);
        let mut out = vec![];
        loop {
            if !self.in_call {
                if let Some(i) = self.buf.find(OPEN_TAG) {
                    let before = &self.buf[..i];
                    if !text::is_blank(before) {
                        let t = if before.ends_with("\n\n") { before.to_string() } else { text::rstrip(before).to_string() };
                        out.push(Parsed::Text(t));
                    }
                    self.buf = self.buf[i..].to_string();
                    self.in_call = true;
                    continue;
                }
                let hold_from = match self.buf.rfind('<') {
                    Some(k) if OPEN_TAG.starts_with(&self.buf[k..]) => k,
                    _ => self.buf.len(),
                };
                if hold_from > 0 && text::is_blank(&self.buf[..hold_from]) {
                    return out; // whitespace alone waits for real text: it never becomes a text block of its own
                }
                if hold_from > 0 {
                    out.push(Parsed::Text(self.buf[..hold_from].to_string()));
                    self.buf = self.buf[hold_from..].to_string();
                }
                return out;
            }
            let Some(j) = self.buf.find(CLOSE_TAG) else { return out };
            let end = j + CLOSE_TAG.len();
            let block = self.buf[..end].to_string();
            self.buf = self.buf[end..].to_string();
            self.in_call = false;
            self.saw_call = true;
            for call in self.parse_block(&block) {
                out.push(Parsed::Call(call));
            }
        }
    }

    pub fn finish(&mut self) -> Vec<Parsed> {
        let mut out = vec![];
        if self.in_call && !text::is_blank(&self.buf) {
            let block = format!("{}{CLOSE_TAG}", self.buf);
            let calls = self.parse_block(&block); // CAVEAT: block without </tool_call>; parsed anyway
            if calls.is_empty() {
                out.push(Parsed::Text(self.buf.clone()));
            } else {
                out.extend(calls.into_iter().map(Parsed::Call));
            }
        } else if !self.buf.is_empty() && (!text::is_blank(&self.buf) || !self.saw_call) {
            out.push(Parsed::Text(self.buf.clone()));
        }
        self.buf.clear();
        self.in_call = false;
        out
    }

    /// One block may hold one call, a JSON array of calls, or several JSON objects in a row. Arguments that are not
    /// valid JSON are never passed on.
    fn parse_block(&mut self, block: &str) -> Vec<ToolCall> {
        let start = OPEN_RE.find(block).map_or(OPEN_TAG.len(), |m| m.end());
        let end = block.len().saturating_sub(CLOSE_TAG.len());
        let inner_raw = if start <= end { &block[start..end] } else { "" };
        let inner = strip_fences(text::strip(inner_raw));
        let objs: Vec<Value> = match pyjson::loads(&inner) {
            Ok(Value::Array(a)) => a,
            Ok(v) => vec![v],
            Err(e) => {
                let (mut objs, rest) = decode_sequence(&inner);
                if !text::is_blank(&rest) {
                    if let Some(salvaged) = salvage(&rest) {
                        objs.push(salvaged);
                    } else if !objs.is_empty() && !LOOKS_LIKE_CALL_RE.is_match(&rest) {
                        // trailing junk after a decoded call: nothing was lost, nothing to repair
                    } else {
                        self.rejected.push(text::head(text::strip(&rest), 4000).to_string());
                    }
                    self.errors.push(format!(
                        "invalid JSON in tool_call ({e}); kept {} call(s); raw block: {}",
                        objs.len(),
                        text::repr_str(text::head(&inner, 300))
                    ));
                    crate::debug!(LOG, "tool_call raw block: {}", text::repr_str(text::head(&inner, 4000)));
                } else if objs.len() > 1 {
                    self.errors.push(format!("tool_call block held {} JSON objects; split into {} calls", objs.len(), objs.len()));
                }
                objs
            }
        };
        objs.into_iter().filter_map(|o| self.make_call(o)).collect()
    }

    fn make_call(&mut self, obj: Value) -> Option<ToolCall> {
        let obj = match obj {
            Value::Object(m) if !m.contains_key("name") => self.infer_name(m),
            other => other,
        };
        let Some(map) = obj.as_object().filter(|m| m.contains_key("name")) else {
            self.errors.push("tool_call without 'name'".into());
            return None;
        };
        let name = text::str_of(&map["name"]);
        let mut args = map.get("arguments").or_else(|| map.get("parameters")).or_else(|| map.get("input")).cloned().unwrap_or(json!({}));
        if let Value::String(s) = &args {
            match pyjson::loads(s) {
                Ok(v) => args = v,
                Err(_) => {
                    self.errors.push(format!(
                        "tool_call {} dropped: arguments are not valid JSON: {}",
                        text::repr_str(&name),
                        text::repr_str(text::head(s, 300))
                    ));
                    self.rejected.push(text::head(&pyjson::dumps(&obj, pyjson::DEFAULT), 4000).to_string());
                    return None;
                }
            }
        }
        if !args.is_null() && !args.is_object() {
            self.errors.push(format!("tool_call {} dropped: arguments are not a JSON object", text::repr_str(&name)));
            return None;
        }
        Some(ToolCall { id: new_call_id(), name, arguments: if args.is_null() { json!({}) } else { args } })
    }

    /// Smaller models sometimes drop the envelope: accept {"function": {...}}, {"tool"|"tool_name"|"function_name": ...}
    /// and bare arguments when exactly one declared tool matches them (CAVEAT: inference).
    fn infer_name(&mut self, obj: Map<String, Value>) -> Value {
        if let Some(Value::Object(f)) = obj.get("function") {
            if let Some(name) = f.get("name") {
                return json!({"name": name, "arguments": f.get("arguments").cloned().unwrap_or(json!({}))});
            }
        }
        for key in ["tool", "tool_name", "function_name"] {
            if let Some(Value::String(name)) = obj.get(key) {
                let mut rest = obj.clone();
                rest.shift_remove(key);
                let args = rest.get("arguments").cloned().unwrap_or(Value::Object(rest.clone()));
                return json!({"name": name, "arguments": args});
            }
        }
        if obj.contains_key("arguments") && self.tools.len() == 1 {
            return json!({"name": self.tools[0].name, "arguments": obj["arguments"]});
        }
        let keys: HashSet<&str> = obj.keys().map(String::as_str).collect();
        let mut candidates: Vec<&ToolSpec> = self
            .tools
            .iter()
            .filter(|t| !keys.is_empty() && keys.iter().all(|k| key_set(t.parameters.get("properties")).contains(*k)))
            .collect();
        if candidates.len() > 1 {
            let complete: Vec<&ToolSpec> = candidates
                .iter()
                .copied()
                .filter(|t| key_set(t.parameters.get("required")).iter().all(|r| keys.contains(r.as_str())))
                .collect();
            if complete.len() == 1 {
                candidates = complete;
            }
        }
        if candidates.len() == 1 || (candidates.is_empty() && self.tools.len() == 1) {
            let tool = candidates.first().copied().unwrap_or(&self.tools[0]);
            let name = tool.name.clone();
            self.errors.push(format!("tool_call without 'name': inferred {} from the arguments", text::repr_str(&name)));
            return json!({"name": name, "arguments": Value::Object(obj)});
        }
        Value::Object(obj)
    }
}

/// `set(x or ...)` of a schema's properties (dict keys) or required list.
fn key_set(v: Option<&Value>) -> HashSet<String> {
    match v {
        Some(Value::Object(m)) => m.keys().cloned().collect(),
        Some(Value::Array(a)) => a.iter().map(text::str_of).collect(),
        Some(Value::String(s)) => s.chars().map(|c| c.to_string()).collect(),
        _ => HashSet::new(),
    }
}

/// Decode consecutive JSON values separated by whitespace or commas. Returns the decoded objects and the undecodable
/// remainder ("" when everything was consumed).
pub fn decode_sequence(s: &str) -> (Vec<Value>, String) {
    let b = s.as_bytes();
    let mut objs = vec![];
    let mut i = 0;
    loop {
        while i < b.len() && matches!(b[i], b' ' | b'\t' | b'\r' | b'\n' | b',') {
            i += 1;
        }
        if i >= b.len() {
            return (objs, String::new());
        }
        match pyjson::raw_decode(s, i) {
            Ok((v, end)) => {
                match v {
                    Value::Array(a) => objs.extend(a),
                    other => objs.push(other),
                }
                i = end;
            }
            Err(_) => return (objs, s[i..].to_string()),
        }
    }
}

/// Last resort for one broken object: the "name" string and the "arguments" value decoded on their own.
fn salvage(s: &str) -> Option<Value> {
    let name = SALVAGE_NAME_RE.captures(s)?.get(1)?.as_str().to_string();
    let Some(args) = SALVAGE_ARGS_RE.find(s) else {
        return Some(json!({"name": name, "arguments": {}}));
    };
    match pyjson::raw_decode(s, args.end()) {
        Ok((v @ Value::Object(_), _)) => Some(json!({"name": name, "arguments": v})),
        _ => None,
    }
}
