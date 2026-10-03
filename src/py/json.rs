//! JSON exactly as Python's `json` module reads and writes it.
//!
//! Prompts sent to the backend must stay byte-identical to the Python implementation's, and some of them carry
//! Python's own decoder messages ("Expecting value: line 1 column 1 (char 0)" in the JSON-mode repair prompt), so both
//! directions are re-implemented here: a decoder with `json.loads`/`JSONDecoder.raw_decode` semantics and messages (CPython 3.12 C
//! scanner), and an encoder with `json.dumps` separators, `ensure_ascii`, `sort_keys` and float `repr`.
//! Values are `serde_json::Value` (insertion-ordered maps: the crate is built with `preserve_order`).
//! Deviations: integers outside 64 bits become floats; NaN/Infinity become null; lone surrogates become U+FFFD.

use serde_json::{Map, Number, Value};

/// `json.JSONDecodeError`: the message as `str(e)` prints it.
#[derive(Debug, Clone)]
pub struct JsonError {
    pub text: String,
}

impl std::fmt::Display for JsonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

fn err(msg: &'static str, doc: &str, pos: usize) -> JsonError {
    // pos is a byte offset; Python reports code points
    let before = &doc[..pos.min(doc.len())];
    let char_pos = before.chars().count();
    let lineno = before.matches('\n').count() + 1;
    let colno = match before.rfind('\n') {
        Some(nl) => before[nl + 1..].chars().count() + 1,
        None => char_pos + 1,
    };
    JsonError { text: format!("{msg}: line {lineno} column {colno} (char {char_pos})") }
}

fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r')
}

fn skip_ws(s: &[u8], mut i: usize) -> usize {
    while i < s.len() && is_ws(s[i]) {
        i += 1;
    }
    i
}

/// `json.loads(s)` for a str.
pub fn loads(doc: &str) -> Result<Value, JsonError> {
    if doc.starts_with('\u{feff}') {
        return Err(JsonError { text: "Unexpected UTF-8 BOM (decode using utf-8-sig): line 1 column 1 (char 0)".into() });
    }
    let b = doc.as_bytes();
    let start = skip_ws(b, 0);
    let (v, end) = raw_decode(doc, start)?;
    let end = skip_ws(b, end);
    if end != b.len() {
        return Err(err("Extra data", doc, end));
    }
    Ok(v)
}

/// `json.loads(bytes)`: UTF-8 (a BOM is accepted, as `utf-8-sig`).
pub fn loads_bytes(raw: &[u8]) -> Option<Value> {
    let raw = raw.strip_prefix(b"\xef\xbb\xbf").unwrap_or(raw);
    let s = std::str::from_utf8(raw).ok()?;
    loads(s).ok()
}

/// `JSONDecoder().raw_decode(s, idx)`: one value starting exactly at byte `idx` (no leading whitespace skipped);
/// returns the value and the byte offset after it.
pub fn raw_decode(doc: &str, idx: usize) -> Result<(Value, usize), JsonError> {
    let mut p = Parser { doc, s: doc.as_bytes() };
    match p.scan_once(idx) {
        Ok(r) => Ok(r),
        Err(Fail::Stop(pos)) => Err(err("Expecting value", doc, pos)),
        Err(Fail::Error(e)) => Err(e),
    }
}

enum Fail {
    Stop(usize),
    Error(JsonError),
}

struct Parser<'a> {
    doc: &'a str,
    s: &'a [u8],
}

impl<'a> Parser<'a> {
    fn starts(&self, idx: usize, lit: &str) -> bool {
        self.s.len() >= idx + lit.len() && &self.s[idx..idx + lit.len()] == lit.as_bytes()
    }

    fn scan_once(&mut self, idx: usize) -> Result<(Value, usize), Fail> {
        if idx >= self.s.len() {
            return Err(Fail::Stop(idx));
        }
        match self.s[idx] {
            b'"' => self.scan_string(idx + 1).map(|(s, e)| (Value::String(s), e)).map_err(Fail::Error),
            b'{' => self.parse_object(idx + 1),
            b'[' => self.parse_array(idx + 1),
            b'n' if self.starts(idx, "null") => Ok((Value::Null, idx + 4)),
            b't' if self.starts(idx, "true") => Ok((Value::Bool(true), idx + 4)),
            b'f' if self.starts(idx, "false") => Ok((Value::Bool(false), idx + 5)),
            b'N' if self.starts(idx, "NaN") => Ok((Value::Null, idx + 3)),
            b'I' if self.starts(idx, "Infinity") => Ok((Value::Null, idx + 8)),
            b'-' if self.starts(idx, "-Infinity") => Ok((Value::Null, idx + 9)),
            _ => self.match_number(idx),
        }
    }

    fn match_number(&self, start: usize) -> Result<(Value, usize), Fail> {
        let s = self.s;
        let n = s.len();
        let mut i = start;
        if i < n && s[i] == b'-' {
            i += 1;
            if i >= n {
                return Err(Fail::Stop(start));
            }
        }
        if i < n && (b'1'..=b'9').contains(&s[i]) {
            i += 1;
            while i < n && s[i].is_ascii_digit() {
                i += 1;
            }
        } else if i < n && s[i] == b'0' {
            i += 1;
        } else {
            return Err(Fail::Stop(start));
        }
        let mut is_float = false;
        if i + 1 < n && s[i] == b'.' && s[i + 1].is_ascii_digit() {
            is_float = true;
            i += 2;
            while i < n && s[i].is_ascii_digit() {
                i += 1;
            }
        }
        if i < n && (s[i] == b'e' || s[i] == b'E') {
            let e_start = i;
            i += 1;
            if i < n && (s[i] == b'-' || s[i] == b'+') {
                i += 1;
            }
            let d0 = i;
            while i < n && s[i].is_ascii_digit() {
                i += 1;
            }
            if i > d0 {
                is_float = true;
            } else {
                i = e_start;
            }
        }
        let text = &self.doc[start..i];
        Ok((number_value(text, is_float), i))
    }

    fn scan_string(&self, end0: usize) -> Result<(String, usize), JsonError> {
        let s = self.s;
        let n = s.len();
        let begin = end0 - 1;
        let mut out = String::new();
        let mut end = end0;
        loop {
            let mut next = end;
            let mut c = 0u8;
            while next < n {
                c = s[next];
                if c == b'"' || c == b'\\' {
                    break;
                }
                if c <= 0x1f {
                    return Err(err("Invalid control character at", self.doc, next));
                }
                next += 1;
            }
            if next >= n {
                return Err(err("Unterminated string starting at", self.doc, begin));
            }
            out.push_str(&self.doc[end..next]);
            if c == b'"' {
                return Ok((out, next + 1));
            }
            // backslash
            next += 1;
            if next >= n {
                return Err(err("Unterminated string starting at", self.doc, begin));
            }
            let e = s[next];
            if e != b'u' {
                let ch = match e {
                    b'"' => '"',
                    b'\\' => '\\',
                    b'/' => '/',
                    b'b' => '\u{8}',
                    b'f' => '\u{c}',
                    b'n' => '\n',
                    b'r' => '\r',
                    b't' => '\t',
                    _ => return Err(err("Invalid \\escape", self.doc, next - 1)),
                };
                out.push(ch);
                end = next + 1;
                continue;
            }
            let u_pos = next;
            let mut next = next + 1;
            let mut end_hex = next + 4;
            if end_hex >= n {
                return Err(err("Invalid \\uXXXX escape", self.doc, next - 1));
            }
            let mut code: u32 = 0;
            while next < end_hex {
                let d = (s[next] as char).to_digit(16).ok_or_else(|| err("Invalid \\uXXXX escape", self.doc, u_pos))?;
                code = (code << 4) | d;
                next += 1;
            }
            if (0xd800..0xdc00).contains(&code) && end_hex + 6 < n && s[next] == b'\\' && s[next + 1] == b'u' {
                let mut j = next + 2;
                let low_end = end_hex + 6;
                let mut c2: u32 = 0;
                while j < low_end {
                    let d = (s[j] as char).to_digit(16).ok_or_else(|| err("Invalid \\uXXXX escape", self.doc, low_end - 5))?;
                    c2 = (c2 << 4) | d;
                    j += 1;
                }
                if (0xdc00..0xe000).contains(&c2) {
                    code = 0x10000 + (((code - 0xd800) << 10) | (c2 - 0xdc00));
                    end_hex = low_end;
                }
            }
            out.push(char::from_u32(code).unwrap_or('\u{fffd}'));
            end = end_hex;
        }
    }

    fn parse_object(&mut self, start: usize) -> Result<(Value, usize), Fail> {
        let s = self.s;
        let n = s.len();
        let mut map = Map::new();
        let mut idx = skip_ws(s, start);
        if idx < n && s[idx] == b'}' {
            return Ok((Value::Object(map), idx + 1));
        }
        loop {
            if idx >= n || s[idx] != b'"' {
                return Err(Fail::Error(err("Expecting property name enclosed in double quotes", self.doc, idx)));
            }
            let (key, after) = self.scan_string(idx + 1).map_err(Fail::Error)?;
            idx = skip_ws(s, after);
            if idx >= n || s[idx] != b':' {
                return Err(Fail::Error(err("Expecting ':' delimiter", self.doc, idx)));
            }
            idx = skip_ws(s, idx + 1);
            let (val, after) = match self.scan_once(idx) {
                Ok(r) => r,
                Err(Fail::Stop(p)) => return Err(Fail::Error(err("Expecting value", self.doc, p))),
                Err(e) => return Err(e),
            };
            map.insert(key, val);
            idx = skip_ws(s, after);
            if idx < n && s[idx] == b'}' {
                return Ok((Value::Object(map), idx + 1));
            }
            if idx >= n || s[idx] != b',' {
                return Err(Fail::Error(err("Expecting ',' delimiter", self.doc, idx)));
            }
            idx = skip_ws(s, idx + 1);
        }
    }

    fn parse_array(&mut self, start: usize) -> Result<(Value, usize), Fail> {
        let s = self.s;
        let n = s.len();
        let mut items = Vec::new();
        let mut idx = skip_ws(s, start);
        if idx < n && s[idx] == b']' {
            return Ok((Value::Array(items), idx + 1));
        }
        loop {
            let (val, after) = match self.scan_once(idx) {
                Ok(r) => r,
                Err(Fail::Stop(p)) => return Err(Fail::Error(err("Expecting value", self.doc, p))),
                Err(e) => return Err(e),
            };
            items.push(val);
            idx = skip_ws(s, after);
            if idx < n && s[idx] == b']' {
                return Ok((Value::Array(items), idx + 1));
            }
            if idx >= n || s[idx] != b',' {
                return Err(Fail::Error(err("Expecting ',' delimiter", self.doc, idx)));
            }
            idx = skip_ws(s, idx + 1);
        }
    }
}

fn number_value(text: &str, is_float: bool) -> Value {
    if !is_float {
        if let Ok(i) = text.parse::<i64>() {
            return Value::Number(Number::from(i));
        }
        if let Ok(u) = text.parse::<u64>() {
            return Value::Number(Number::from(u));
        }
    }
    match text.parse::<f64>().ok().and_then(Number::from_f64) {
        Some(n) => Value::Number(n),
        None => Value::Null,
    }
}

// ---------------------------------------------------------------------------------------------------------------
// encoder
// ---------------------------------------------------------------------------------------------------------------

/// How `json.dumps` was called.
#[derive(Debug, Clone, Copy)]
pub struct Style {
    pub ensure_ascii: bool,
    pub compact: bool,
    pub sort_keys: bool,
}

/// `json.dumps(x, ensure_ascii=False)`: prompts, SSE payloads, the store.
pub const DEFAULT: Style = Style { ensure_ascii: false, compact: false, sort_keys: false };
/// `json.dumps(x, ensure_ascii=False, separators=(",", ":"))`: tool lines, HTTP bodies (Starlette's JSONResponse).
pub const COMPACT: Style = Style { ensure_ascii: false, compact: true, sort_keys: false };
/// `json.dumps(x)`.
pub const ASCII: Style = Style { ensure_ascii: true, compact: false, sort_keys: false };

pub fn dumps(v: &Value, style: Style) -> String {
    let mut out = String::new();
    write_value(v, style, &mut out);
    out
}

pub fn dumps_str(s: &str, ensure_ascii: bool) -> String {
    let mut out = String::new();
    write_string(s, ensure_ascii, &mut out);
    out
}

fn write_value(v: &Value, style: Style, out: &mut String) {
    match v {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                out.push_str(&n.to_string());
            } else {
                out.push_str(&float_repr(n.as_f64().unwrap_or(0.0)));
            }
        }
        Value::String(s) => write_string(s, style.ensure_ascii, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(if style.compact { "," } else { ", " });
                }
                write_value(item, style, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            if style.sort_keys {
                entries.sort_by(|a, b| a.0.cmp(b.0));
            }
            for (i, (k, item)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push_str(if style.compact { "," } else { ", " });
                }
                write_string(k, style.ensure_ascii, out);
                out.push_str(if style.compact { ":" } else { ": " });
                write_value(item, style, out);
            }
            out.push('}');
        }
    }
}

fn write_string(s: &str, ensure_ascii: bool, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => push_u(c as u32, out),
            c if ensure_ascii && (c as u32) > 0x7e => {
                let code = c as u32;
                if code <= 0xffff {
                    push_u(code, out);
                } else {
                    let v = code - 0x10000;
                    push_u(0xd800 | (v >> 10), out);
                    push_u(0xdc00 | (v & 0x3ff), out);
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

fn push_u(code: u32, out: &mut String) {
    use std::fmt::Write;
    let _ = write!(out, "\\u{code:04x}");
}

/// Python's `repr(float)`: shortest round-trip digits, fixed notation for exponents in -4..16, else scientific.
pub fn float_repr(f: f64) -> String {
    if f.is_nan() {
        return "NaN".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if f == 0.0 {
        return if f.is_sign_negative() { "-0.0" } else { "0.0" }.into();
    }
    let sci = format!("{f:e}");
    let (mantissa, exp) = sci.split_once('e').unwrap_or((sci.as_str(), "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let negative = mantissa.starts_with('-');
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let mut out = String::new();
    if negative {
        out.push('-');
    }
    if (-4..16).contains(&exp) {
        if exp >= 0 {
            let int_len = exp as usize + 1;
            if digits.len() <= int_len {
                out.push_str(&digits);
                out.push_str(&"0".repeat(int_len - digits.len()));
                out.push_str(".0");
            } else {
                out.push_str(&digits[..int_len]);
                out.push('.');
                out.push_str(&digits[int_len..]);
            }
        } else {
            out.push_str("0.");
            out.push_str(&"0".repeat((-exp - 1) as usize));
            out.push_str(&digits);
        }
    } else {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if exp < 0 { '-' } else { '+' });
        out.push_str(&format!("{:02}", exp.abs()));
    }
    out
}

/// A float as a JSON value (Python floats stay floats: 0.0 prints "0.0").
pub fn float(f: f64) -> Value {
    Number::from_f64(f).map(Value::Number).unwrap_or(Value::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages() {
        let cases = [
            ("", "Expecting value: line 1 column 1 (char 0)"),
            ("{", "Expecting property name enclosed in double quotes: line 1 column 2 (char 1)"),
            ("{\"a\"", "Expecting ':' delimiter: line 1 column 5 (char 4)"),
            ("{\"a\":1", "Expecting ',' delimiter: line 1 column 7 (char 6)"),
            ("[1,]", "Expecting value: line 1 column 4 (char 3)"),
            ("\"abc", "Unterminated string starting at: line 1 column 1 (char 0)"),
            ("\"a\\x\"", "Invalid \\escape: line 1 column 3 (char 2)"),
            ("\"a\u{1}\"", "Invalid control character at: line 1 column 3 (char 2)"),
            ("1 2", "Extra data: line 1 column 3 (char 2)"),
            ("\"\\u12\"", "Invalid \\uXXXX escape: line 1 column 3 (char 2)"),
            ("01", "Extra data: line 1 column 2 (char 1)"),
            (" ", "Expecting value: line 1 column 2 (char 1)"),
            ("\"\\", "Unterminated string starting at: line 1 column 1 (char 0)"),
        ];
        for (doc, msg) in cases {
            assert_eq!(loads(doc).unwrap_err().text, msg, "{doc:?}");
        }
    }

    #[test]
    fn values() {
        let v = loads(r#"{"a": [1, 2.5, "x\u00e9\ud83d\ude00"], "b": null}"#).unwrap();
        assert_eq!(dumps(&v, DEFAULT), "{\"a\": [1, 2.5, \"xé😀\"], \"b\": null}");
        assert_eq!(dumps(&v, ASCII), "{\"a\": [1, 2.5, \"x\\u00e9\\ud83d\\ude00\"], \"b\": null}");
        assert_eq!(dumps(&loads("1.0").unwrap(), DEFAULT), "1.0");
        assert_eq!(dumps(&loads("1e5").unwrap(), DEFAULT), "100000.0");
    }
}
