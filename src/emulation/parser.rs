//! Model text -> text and tool-call events, incrementally.

use std::collections::HashSet;
use std::sync::{Arc, LazyLock};

use regex::Regex;
use serde_json::{Map, Value, json};

use crate::canonical::{ToolCall, ToolSpec, new_call_id};
use crate::json;
use crate::text::{char_len, prefix};

pub const OPEN_TAG: &str = "<tool_call";
pub const CLOSE_TAG: &str = "</tool_call>";
static OPEN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?s)^<tool_call(?:\s+id\s*=\s*"?([\w.-]*)"?)?\s*>"#).unwrap());
static FENCE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)^\s*```(?:json)?\s*|\s*```\s*$").unwrap());
static LOOKS_LIKE_CALL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#""(name|arguments|parameters|input|tool|function)"\s*:"#).unwrap());
static SALVAGE_NAME_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""name"\s*:\s*"([^"]+)""#).unwrap());
static SALVAGE_ARGS_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#""(?:arguments|parameters|input)"\s*:\s*"#).unwrap());

/// The text without a surrounding Markdown code fence (```json ... ```).
pub fn strip_fences(s: &str) -> String {
    FENCE_RE.replace_all(s, "").into_owned()
}

#[derive(Debug, Clone)]
pub enum Parsed {
    Text(String),
    Call(ToolCall),
}

/// Feed text deltas; get text and tool-call events. Holds back any suffix that could be the start of "<tool_call".
/// `errors` describe problems by their shape only (sizes, positions): they are logged at WARN, and model output may
/// hold secrets; the content itself goes to DEBUG.
pub struct ToolCallParser {
    buf: String,
    in_call: bool,
    pub errors: Vec<String>,
    /// raw content of calls dropped because their JSON could not be decoded
    pub rejected: Vec<String>,
    /// blocks whose JSON was decodable only after escaping raw control characters or dropping trailing commas
    pub repaired: usize,
    tools: Arc<[ToolSpec]>,
    saw_call: bool,
    /// inside a block: how far the search for its end got, and the JSON string state there
    scan: Scan,
    /// at the end of the stream: a block ends at its first `</tool_call>`, inside a string or not
    lenient: bool,
    /// text went out since the last call (a paragraph break before the next call belongs to it)
    text_since_call: bool,
    /// the block being read was opened with `</tool_call>` (a model's typo): if it is no call, it goes out as written
    stray_open: bool,
}

/// The search for the `</tool_call>` that ends a block, kept between deltas so a long call is scanned once.
#[derive(Default)]
struct Scan {
    /// byte offset in `buf` scanned so far (None until the open tag is complete)
    pos: Option<usize>,
    in_string: bool,
    escaped: bool,
}

impl ToolCallParser {
    pub fn new(tools: Arc<[ToolSpec]>) -> Self {
        ToolCallParser {
            buf: String::new(),
            in_call: false,
            errors: vec![],
            rejected: vec![],
            repaired: 0,
            tools,
            saw_call: false,
            scan: Scan::default(),
            lenient: false,
            text_since_call: false,
            stray_open: false,
        }
    }

    pub fn feed(&mut self, delta: &str) -> Vec<Parsed> {
        self.buf.push_str(delta);
        let mut out = vec![];
        loop {
            if !self.in_call {
                // `</tool_call>` where a call should open, followed by its JSON: the model wrote the wrong tag
                let open_at = self.buf.find(OPEN_TAG);
                let mut stray_hold = None;
                let mut stray_call = None;
                for (j, _) in self.buf.match_indices(CLOSE_TAG).take_while(|(j, _)| open_at.is_none_or(|i| *j < i)) {
                    let after = self.buf[j + CLOSE_TAG.len()..].trim_start();
                    if after.starts_with('{') {
                        stray_call = Some(j);
                        break;
                    }
                    if after.is_empty() {
                        stray_hold = Some(j); // what follows decides: wait for it
                    }
                }
                if let Some(j) = stray_call {
                    self.buf.replace_range(j..j + CLOSE_TAG.len(), "<tool_call>");
                    self.stray_open = true;
                    self.errors.push("a call opened with </tool_call> instead of <tool_call>".into());
                    continue;
                }
                if let Some(i) = open_at {
                    let before = &self.buf[..i];
                    if !before.trim().is_empty() {
                        let t = if before.ends_with("\n\n") { before.to_string() } else { before.trim_end().to_string() };
                        out.push(Parsed::Text(t));
                    } else if before.ends_with("\n\n") && self.text_since_call {
                        // the paragraph break after text already sent (held back as trailing whitespace)
                        out.push(Parsed::Text(before.to_string()));
                    }
                    self.buf = self.buf[i..].to_string();
                    self.in_call = true;
                    continue;
                }
                let hold_from = match self.buf.rfind('<') {
                    Some(k) if OPEN_TAG.starts_with(&self.buf[k..]) || CLOSE_TAG.starts_with(&self.buf[k..]) => k,
                    _ => self.buf.len(),
                };
                let hold_from = stray_hold.map_or(hold_from, |j| j.min(hold_from));
                if hold_from > 0 && self.buf[..hold_from].trim().is_empty() {
                    return out; // whitespace alone waits for real text: it never becomes a text block of its own
                }
                if hold_from > 0 {
                    // trailing whitespace waits: if a call follows, it is trimmed as when text and call come together
                    let text = self.buf[..hold_from].trim_end();
                    let sent = text.len();
                    out.push(Parsed::Text(text.to_string()));
                    self.text_since_call = true;
                    self.buf = self.buf[sent..].to_string();
                }
                return out;
            }
            let Some(end) = self.block_end() else { return out };
            let block = self.buf[..end].to_string();
            self.buf = self.buf[end..].to_string();
            self.in_call = false;
            self.scan = Scan::default();
            if is_prose(&block) {
                // the tag quoted in the model's text ("use the `<tool_call>` tag"): text, not a broken call
                out.push(Parsed::Text(block));
                self.text_since_call = true;
                continue;
            }
            self.saw_call = true;
            self.text_since_call = false;
            self.stray_open = false;
            for call in self.parse_block(&block) {
                out.push(Parsed::Call(call));
            }
        }
    }

    /// Where the block at the start of `buf` ends: after the first `</tool_call>` outside a JSON string, so a call
    /// whose arguments hold the tag (writing a file that documents the protocol) is not cut short. When the stream
    /// ends without one, the first `</tool_call>` anywhere (the JSON is broken then anyway).
    fn block_end(&mut self) -> Option<usize> {
        if self.lenient {
            return self.buf.find(CLOSE_TAG).map(|j| j + CLOSE_TAG.len());
        }
        let mut i = match self.scan.pos {
            Some(i) => i,
            None => {
                // the arguments start after the open tag (whose id="..." holds quotes of its own)
                let body = OPEN_RE.find(&self.buf).map(|m| m.end()).or_else(|| self.buf.find('>').map(|k| k + 1))?;
                self.scan.pos = Some(body);
                body
            }
        };
        let bytes = self.buf.as_bytes();
        while i < bytes.len() {
            let c = bytes[i];
            if self.scan.in_string {
                if self.scan.escaped {
                    self.scan.escaped = false;
                } else if c == b'\\' {
                    self.scan.escaped = true;
                } else if c == b'"' {
                    self.scan.in_string = false;
                }
            } else if c == b'"' {
                self.scan.in_string = true;
            } else if c == b'<' {
                let rest = &self.buf[i..];
                if rest.starts_with(CLOSE_TAG) {
                    return Some(i + CLOSE_TAG.len());
                }
                if CLOSE_TAG.starts_with(rest) {
                    break; // maybe the tag, cut by the delta: look again with more text
                }
            }
            i += 1;
        }
        self.scan.pos = Some(i);
        None
    }

    pub fn finish(&mut self) -> Vec<Parsed> {
        let mut out = vec![];
        if self.in_call && self.buf.contains(CLOSE_TAG) {
            // CAVEAT: no `</tool_call>` outside a string (an unescaped quote in the JSON): the first one ends the block
            self.lenient = true;
            out.extend(self.feed(""));
            self.lenient = false;
        }
        if self.in_call && !self.buf.trim().is_empty() {
            let block = format!("{}{CLOSE_TAG}", self.buf);
            let calls = if is_prose(&block) { vec![] } else { self.parse_block(&block) }; // CAVEAT: block without </tool_call>; parsed anyway
            if calls.is_empty() {
                let text = if self.stray_open { self.buf.replacen("<tool_call>", CLOSE_TAG, 1) } else { self.buf.clone() };
                out.push(Parsed::Text(text));
            } else {
                out.extend(calls.into_iter().map(Parsed::Call));
            }
        } else if !self.buf.is_empty() && (!self.buf.trim().is_empty() || !self.saw_call) {
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
        let inner = strip_fences(inner_raw.trim());
        let strict = serde_json::from_str(&inner).map_err(|e| {
            // what a model means more often than not: a literal newline or TAB inside a string, a trailing comma
            match json::decode_lenient(&inner) {
                Ok((v, true)) => {
                    self.repaired += 1;
                    tracing::debug!("tool_call JSON decoded after escaping raw control characters / dropping trailing commas");
                    Ok(v)
                }
                _ => Err(e),
            }
        });
        let objs: Vec<Value> = match strict {
            Ok(Value::Array(a)) | Err(Ok(Value::Array(a))) => a,
            Ok(v) | Err(Ok(v)) => vec![v],
            Err(Err(e)) => {
                // several objects in one block, one of them with a raw newline: repair, then read them in a row
                let repaired = json::repair_model_json(&inner);
                let (mut objs, rest) = match decode_sequence(&repaired) {
                    (objs, rest) if rest.trim().is_empty() && !objs.is_empty() => {
                        self.repaired += 1;
                        (objs, rest)
                    }
                    _ => decode_sequence(&inner),
                };
                if !rest.trim().is_empty() {
                    if let Some(salvaged) = salvage(&rest) {
                        objs.push(salvaged);
                    } else if !objs.is_empty() && !LOOKS_LIKE_CALL_RE.is_match(&rest) {
                        // trailing junk after a decoded call: nothing was lost, nothing to repair
                    } else {
                        self.rejected.push(prefix(rest.trim(), 4000).to_string());
                    }
                    self.errors.push(format!(
                        "invalid JSON in a tool_call block of {} chars ({e}); kept {} call(s)",
                        char_len(&inner),
                        objs.len()
                    ));
                    tracing::debug!("tool_call raw block: {:?}", prefix(&inner, 4000));
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
        let name = match &map["name"] {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let mut args = map.get("arguments").or_else(|| map.get("parameters")).or_else(|| map.get("input")).cloned().unwrap_or(json!({}));
        if let Value::String(s) = &args {
            match json::decode_lenient(s) {
                Ok((v, repaired)) => {
                    self.repaired += usize::from(repaired);
                    args = v;
                }
                Err(e) => {
                    self.errors.push(format!("tool_call {name:?} dropped: arguments are not valid JSON ({} chars: {e})", char_len(s)));
                    tracing::debug!("tool_call {name:?} arguments: {:?}", prefix(s, 4000));
                    self.rejected.push(prefix(&obj.to_string(), 4000).to_string());
                    return None;
                }
            }
        }
        if !args.is_null() && !args.is_object() {
            self.errors.push(format!("tool_call {name:?} dropped: arguments are not a JSON object"));
            return None;
        }
        Some(ToolCall { id: new_call_id(), name, arguments: if args.is_null() { json!({}) } else { args } })
    }

    /// Smaller models sometimes drop the envelope: accept {"function": {...}}, {"tool"|"tool_name"|"function_name": ...}
    /// and bare arguments when exactly one declared tool matches them (CAVEAT: inference).
    fn infer_name(&mut self, obj: Map<String, Value>) -> Value {
        if let Some(Value::Object(f)) = obj.get("function")
            && let Some(name) = f.get("name")
        {
            return json!({"name": name, "arguments": f.get("arguments").cloned().unwrap_or(json!({}))});
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
            self.errors.push(format!("tool_call without 'name': inferred {name:?} from the arguments"));
            return json!({"name": name, "arguments": Value::Object(obj)});
        }
        Value::Object(obj)
    }
}

/// The names in a schema's `properties` (object keys) or `required` (list of strings).
fn key_set(v: Option<&Value>) -> HashSet<String> {
    match v {
        Some(Value::Object(m)) => m.keys().cloned().collect(),
        Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).map(String::from).collect(),
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
        match decode_prefix(&s[i..]) {
            Some((v, used)) => {
                match v {
                    Value::Array(a) => objs.extend(a),
                    other => objs.push(other),
                }
                i += used;
            }
            None => return (objs, s[i..].to_string()),
        }
    }
}

/// The JSON value at the start of `s` and the bytes it used (anything may follow it).
fn decode_prefix(s: &str) -> Option<(Value, usize)> {
    let mut it = serde_json::Deserializer::from_str(s).into_iter::<Value>();
    let v = it.next()?.ok()?;
    Some((v, it.byte_offset()))
}

/// Last resort for one broken object: the "name" string and the "arguments" value decoded on their own.
fn salvage(s: &str) -> Option<Value> {
    let Some(args) = SALVAGE_ARGS_RE.find(s) else {
        let name = SALVAGE_NAME_RE.captures(s)?.get(1)?.as_str().to_string();
        return Some(json!({"name": name, "arguments": {}}));
    };
    let (v, used) = match decode_prefix(&s[args.end()..]) {
        Some((v @ Value::Object(_), used)) => (v, used),
        _ => return None,
    };
    // the call's name is the "name" outside its arguments (an argument may be called "name" too)
    let span = args.start()..args.end() + used;
    let name = SALVAGE_NAME_RE.captures_iter(s).filter_map(|c| c.get(1)).find(|m| !span.contains(&m.start()))?.as_str().to_string();
    Some(json!({"name": name, "arguments": v}))
}

/// A block that does not look like a call attempt: no JSON object or array, no call keys. The model quoted the tag in
/// its text; the block is text.
fn is_prose(block: &str) -> bool {
    let start = OPEN_RE.find(block).map_or(OPEN_TAG.len(), |m| m.end());
    let end = block.len().saturating_sub(CLOSE_TAG.len()).max(start.min(block.len()));
    let inner = strip_fences(block.get(start..end).unwrap_or("").trim());
    let inner = inner.trim_start();
    !inner.is_empty() && !inner.starts_with(['{', '[']) && !LOOKS_LIKE_CALL_RE.is_match(inner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// What a reply means, whatever the chunking: its texts (merged, trimmed) and calls (name and arguments) in order.
    fn meaning(text: &str, cuts: &[usize]) -> Vec<String> {
        let mut p = ToolCallParser::new(vec![ToolSpec::new("read_file", "", None, false)].into());
        let chars: Vec<char> = text.chars().collect();
        let mut bounds: Vec<usize> = cuts.iter().map(|c| c % (chars.len() + 1)).collect();
        bounds.extend([0, chars.len()]);
        bounds.sort_unstable();
        bounds.dedup();
        let mut events = vec![];
        for w in bounds.windows(2) {
            events.extend(p.feed(&chars[w[0]..w[1]].iter().collect::<String>()));
        }
        events.extend(p.finish());
        let mut out: Vec<String> = vec![];
        let mut text_run = String::new();
        // text is compared as sent, whitespace included (review 2026-10-05, F38): only a run of nothing but
        // whitespace after a call may be dropped, whatever the chunking
        let flush = |run: &mut String, out: &mut Vec<String>| {
            if !run.trim().is_empty() {
                out.push(format!("text:{run}"));
            }
            run.clear();
        };
        for e in events {
            match e {
                Parsed::Text(t) => text_run.push_str(&t),
                Parsed::Call(c) => {
                    flush(&mut text_run, &mut out);
                    out.push(format!("call:{}:{}", c.name, c.arguments));
                }
            }
        }
        flush(&mut text_run, &mut out);
        out
    }

    fn piece() -> impl Strategy<Value = String> {
        prop_oneof![
            "[a-zç🎉 <>/\\n.]{0,12}",
            "[a-z]{1,6}".prop_map(|v| format!(
                "<tool_call id=\"call_1\">\n{{\"name\": \"read_file\", \"arguments\": {{\"path\": \"{v}\"}}}}\n</tool_call>"
            )),
            Just("<tool_cal".to_string()),
            Just("</tool_call>\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"s\"}}\n</tool_call>".to_string()),
            Just("</tool_call>".to_string()),
            Just("<tool_call>\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"a\nb\"}}\n</tool_call>".to_string()),
            Just(
                "<tool_call>\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"doc </tool_call> \\\"q\\\" end\"}}\n</tool_call>"
                    .to_string()
            ),
        ]
    }

    proptest! {
        #[test]
        fn chunking_never_changes_the_meaning(pieces in prop::collection::vec(piece(), 0..8), cuts in prop::collection::vec(0usize..400, 0..12)) {
            let text: String = pieces.concat();
            prop_assert_eq!(meaning(&text, &cuts), meaning(&text, &[]));
        }
    }

    #[test]
    fn the_close_tag_inside_a_json_string_does_not_end_the_call() {
        // an agent writing a file that documents the protocol (Midir's own prompt.rs, agent-brief.md)
        let content = "Emit <tool_call id=\\\"call_1\\\">{...}</tool_call> blocks.";
        let reply = format!(
            "Writing it.\n<tool_call id=\"call_1\">\n{{\"name\": \"write_file\", \"arguments\": {{\"path\": \"brief.md\", \"content\": \"{content}\"}}}}\n</tool_call>\nDone."
        );
        let tools: Arc<[ToolSpec]> = vec![ToolSpec::new("write_file", "", None, false)].into();
        for step in [1, 3, reply.len()] {
            let mut p = ToolCallParser::new(tools.clone());
            let mut events = vec![];
            let chars: Vec<char> = reply.chars().collect();
            for piece in chars.chunks(step) {
                events.extend(p.feed(&piece.iter().collect::<String>()));
            }
            events.extend(p.finish());
            let calls: Vec<&ToolCall> = events.iter().filter_map(|e| if let Parsed::Call(c) = e { Some(c) } else { None }).collect();
            assert_eq!(calls.len(), 1, "step {step}: {events:?}");
            assert_eq!(calls[0].arguments["content"], "Emit <tool_call id=\"call_1\">{...}</tool_call> blocks.");
            let text: String = events.iter().filter_map(|e| if let Parsed::Text(t) = e { Some(t.as_str()) } else { None }).collect();
            assert!(text.contains("Writing it.") && text.contains("Done."), "step {step}: {text:?}");
            assert!(p.rejected.is_empty());
        }
    }

    #[test]
    fn an_unbalanced_quote_still_ends_the_call_at_its_close_tag() {
        // no </tool_call> outside a string: at the end of the stream the first one ends the block, as before
        let mut p = ToolCallParser::new(vec![ToolSpec::new("read_file", "", None, false)].into());
        let mut events =
            p.feed("<tool_call>\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"a\"b.py\"}}\n</tool_call>\nThen more text.");
        assert!(events.is_empty()); // held: the tag looked like part of a string
        events.extend(p.finish());
        let text: String = events.iter().filter_map(|e| if let Parsed::Text(t) = e { Some(t.as_str()) } else { None }).collect();
        assert!(text.contains("Then more text."), "{events:?}");
        assert!(!text.contains("</tool_call>"), "{events:?}");
    }

    #[test]
    fn broken_calls_are_described_without_their_content() {
        let mut p = ToolCallParser::new(Arc::new([]));
        p.feed("<tool_call>\n{\"name\": \"write\", \"arguments\": {\"content\": \"TOKEN=abc123\" \"x\"}}\n</tool_call>");
        p.finish();
        assert!(!p.errors.is_empty() && !p.errors.join(" ").contains("abc123"), "{:?}", p.errors);
        assert_eq!(p.rejected.len(), 1); // the repair follow-up still gets it
    }

    #[test]
    fn a_tag_quoted_in_prose_is_text() {
        // review 2026-10-05 (F34): the middle of the sentence was lost and a repair fired
        let reply = "Use the `<tool_call>` tag and close it with `</tool_call>`. That is all.";
        for step in [1, 3, reply.len()] {
            let mut p = ToolCallParser::new(Arc::new([]));
            let chars: Vec<char> = reply.chars().collect();
            let mut events = vec![];
            for piece in chars.chunks(step) {
                events.extend(p.feed(&piece.iter().collect::<String>()));
            }
            events.extend(p.finish());
            let text: String = events.iter().filter_map(|e| if let Parsed::Text(t) = e { Some(t.as_str()) } else { None }).collect();
            assert!(text.contains("tag and close it with") && text.contains("That is all."), "step {step}: {text:?}");
            assert!(p.rejected.is_empty() && events.iter().all(|e| matches!(e, Parsed::Text(_))), "step {step}");
        }
        // an unclosed quoted tag at the end of the reply is text too
        let mut p = ToolCallParser::new(Arc::new([]));
        let mut events = p.feed("Wrap calls in <tool_call> tags.");
        events.extend(p.finish());
        assert!(p.rejected.is_empty());
        assert!(matches!(events.last(), Some(Parsed::Text(t)) if t.contains("tags.")), "{events:?}");
    }

    #[test]
    fn text_before_a_call_is_the_same_whatever_the_chunking() {
        // review 2026-10-05 (F38): "Reading now.\n" or "Reading now." depending on where the deltas were cut
        for reply in [
            "Reading now.\n<tool_call>{\"name\": \"read_file\", \"arguments\": {}}</tool_call>",
            "Plan.\n\n<tool_call>{\"name\": \"read_file\", \"arguments\": {}}</tool_call>",
            "Hello there.\n",
        ] {
            let texts: Vec<String> = [1, 5, 13, reply.len()]
                .iter()
                .map(|step| {
                    let mut p = ToolCallParser::new(vec![ToolSpec::new("read_file", "", None, false)].into());
                    let chars: Vec<char> = reply.chars().collect();
                    let mut events = vec![];
                    for piece in chars.chunks(*step) {
                        events.extend(p.feed(&piece.iter().collect::<String>()));
                    }
                    events.extend(p.finish());
                    events.iter().filter_map(|e| if let Parsed::Text(t) = e { Some(t.as_str()) } else { None }).collect()
                })
                .collect();
            assert!(texts.windows(2).all(|w| w[0] == w[1]), "{reply:?}: {texts:?}");
        }
    }

    #[test]
    fn salvage_prefers_the_name_outside_the_arguments() {
        // review 2026-10-05 (F39)
        let v = salvage(r#"{"arguments": {"name": "John", "title": "x"}, "name": "create_issue", "labels": [broken"#).unwrap();
        assert_eq!(v["name"], "create_issue");
        assert_eq!(v["arguments"]["name"], "John");
        let v = salvage(r#"{"name": "read_file", "arguments": {"path": "a.py"}, "extra": [broken"#).unwrap();
        assert_eq!(v["name"], "read_file");
    }

    #[test]
    fn a_call_opened_with_the_close_tag_is_still_a_call() {
        // battery 2026-10-06 (dsh, flex): "Vou começar editando o CLI.\n\n</tool_call>\n{...}" reached the user as text
        let reply = "Vou começar editando o CLI.\n\n</tool_call>\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"calc/cli.py\"}}\n";
        let expected = vec!["text:Vou começar editando o CLI.\n\n".to_string(), r#"call:read_file:{"path":"calc/cli.py"}"#.to_string()];
        for cuts in [vec![], vec![30], vec![31, 33, 40], (0..90).collect()] {
            assert_eq!(meaning(reply, &cuts), expected, "{cuts:?}");
        }
        // closed properly after all
        assert_eq!(meaning(&format!("{reply}</tool_call>"), &[]), expected);
        // the tag quoted in prose stays text, as written
        assert_eq!(meaning("Close it with </tool_call> and stop.", &[20]), vec!["text:Close it with </tool_call> and stop."]);
        assert_eq!(meaning("Ends with </tool_call>", &[]), vec!["text:Ends with </tool_call>"]);
        // a stray tag before broken JSON goes out as written
        let broken = meaning("x </tool_call> {oops", &[]);
        assert!(broken.len() == 1 && broken[0].ends_with("</tool_call> {oops"), "{broken:?}");
    }
}
