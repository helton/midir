//! Python string and value semantics Midir's behavior is defined by: `str.strip()` whitespace, `splitlines()`, code-point
//! lengths and slices, truthiness, `==` across ints/floats/bools, `str()` and `repr()` of JSON values.

use serde_json::Value;

use super::json;

/// `str.isspace()` for one character (Rust's whitespace plus the ASCII separators \x1c-\x1f).
pub fn is_space(c: char) -> bool {
    c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)
}

pub fn strip(s: &str) -> &str {
    s.trim_matches(is_space)
}

pub fn rstrip(s: &str) -> &str {
    s.trim_end_matches(is_space)
}

pub fn is_blank(s: &str) -> bool {
    s.chars().all(is_space)
}

/// `len(s)`: code points.
pub fn len(s: &str) -> usize {
    s.chars().count()
}

/// Byte offset of code point `n` (or the end).
pub fn byte_at(s: &str, n: usize) -> usize {
    s.char_indices().nth(n).map_or(s.len(), |(i, _)| i)
}

/// `s[:n]`.
pub fn head(s: &str, n: usize) -> &str {
    &s[..byte_at(s, n)]
}

/// `s[n:]`.
pub fn tail_from(s: &str, n: usize) -> &str {
    &s[byte_at(s, n)..]
}

/// `s[a:b]` with non-negative bounds.
pub fn slice(s: &str, a: usize, b: usize) -> &str {
    let start = byte_at(s, a);
    let end = byte_at(s, b).max(start);
    &s[start..end]
}

fn is_line_break(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{b}' | '\u{c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

/// `str.splitlines()`.
pub fn splitlines(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        if is_line_break(c) {
            out.push(&s[start..i]);
            let mut next = i + c.len_utf8();
            if c == '\r' {
                if let Some(&(j, '\n')) = it.peek() {
                    it.next();
                    next = j + 1;
                }
            }
            start = next;
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// `bool(x)`.
pub fn truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map_or(true, |f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

pub fn truthy_opt(v: Option<&Value>) -> bool {
    v.map_or(false, truthy)
}

fn num_of(v: &Value) -> Option<f64> {
    match v {
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

/// Python `a == b` for JSON values (1 == 1.0 == True; dicts compare regardless of key order).
pub fn eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            if let (Some(i), Some(j)) = (x.as_i64(), y.as_i64()) {
                return i == j;
            }
            if let (Some(i), Some(j)) = (x.as_u64(), y.as_u64()) {
                return i == j;
            }
            x.as_f64() == y.as_f64()
        }
        (Value::Bool(_) | Value::Number(_), Value::Bool(_) | Value::Number(_)) => num_of(a) == num_of(b),
        (Value::Array(x), Value::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(p, q)| eq(p, q)),
        (Value::Object(x), Value::Object(y)) => x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).map_or(false, |w| eq(v, w))),
        _ => a == b,
    }
}

/// `type(x).__name__`.
pub fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(n) if n.is_i64() || n.is_u64() => "int",
        Value::Number(_) => "float",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

pub fn is_int(v: &Value) -> bool {
    matches!(v, Value::Number(n) if n.is_i64() || n.is_u64())
}

/// `str(x)`.
pub fn str_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => repr(other),
    }
}

/// `str(x or "")`: falsy values become "".
pub fn str_or_empty(v: Option<&Value>) -> String {
    match v {
        Some(v) if truthy(v) => str_of(v),
        _ => String::new(),
    }
}

/// `repr(x)`.
pub fn repr(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(n) if n.is_i64() || n.is_u64() => n.to_string(),
        Value::Number(n) => {
            let f = n.as_f64().unwrap_or(0.0);
            if f.is_nan() {
                "nan".into()
            } else if f.is_infinite() {
                if f > 0.0 {
                    "inf".into()
                } else {
                    "-inf".into()
                }
            } else {
                json::float_repr(f)
            }
        }
        Value::String(s) => repr_str(s),
        Value::Array(items) => format!("[{}]", items.iter().map(repr).collect::<Vec<_>>().join(", ")),
        Value::Object(map) => {
            format!("{{{}}}", map.iter().map(|(k, v)| format!("{}: {}", repr_str(k), repr(v))).collect::<Vec<_>>().join(", "))
        }
    }
}

fn printable(c: char) -> bool {
    let u = c as u32;
    if u < 0x20 || (0x7f..=0xa0).contains(&u) {
        return false;
    }
    !matches!(u,
        0xad | 0x34f | 0x61c | 0x6dd | 0x70f | 0x180e | 0x1680 | 0x2000..=0x200f | 0x2028..=0x202f | 0x205f..=0x206f
        | 0x3000 | 0xd800..=0xf8ff | 0xfeff | 0xfff9..=0xfffb | 0x600..=0x605 | 0xe0001 | 0xe0020..=0xe007f
        | 0xf0000..=0x10ffff)
}

/// `repr(str)`.
pub fn repr_str(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') { '"' } else { '\'' };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if !printable(c) => {
                let u = c as u32;
                if u <= 0xff {
                    out.push_str(&format!("\\x{u:02x}"));
                } else if u <= 0xffff {
                    out.push_str(&format!("\\u{u:04x}"));
                } else {
                    out.push_str(&format!("\\U{u:08x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// `repr(list_of_str)`.
pub fn repr_list(items: &[String]) -> String {
    format!("[{}]", items.iter().map(|s| repr_str(s)).collect::<Vec<_>>().join(", "))
}

/// `int(x)` on configuration values: ints, floats (truncated), bools and numeric strings.
pub fn int_of(v: &Value) -> Option<i64> {
    match v {
        Value::Bool(b) => Some(*b as i64),
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f.trunc() as i64)),
        Value::String(s) => parse_int(s),
        _ => None,
    }
}

/// `int(str)`: surrounding whitespace, a sign and underscores between digits are accepted.
pub fn parse_int(s: &str) -> Option<i64> {
    let t = strip(s);
    let (neg, digits) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    if digits.is_empty() || digits.starts_with('_') || digits.ends_with('_') || digits.contains("__") {
        return None;
    }
    let clean: String = digits.chars().filter(|c| *c != '_').collect();
    if !clean.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let n: i64 = clean.parse().ok()?;
    Some(if neg { -n } else { n })
}

/// `float(x)`.
pub fn float_of(v: &Value) -> Option<f64> {
    match v {
        Value::Bool(b) => Some(*b as i64 as f64),
        Value::Number(n) => n.as_f64(),
        Value::String(s) => {
            let t = strip(s).replace('_', "");
            let low = t.to_ascii_lowercase();
            match low.trim_start_matches(['+', '-']) {
                "inf" | "infinity" => Some(if low.starts_with('-') { f64::NEG_INFINITY } else { f64::INFINITY }),
                "nan" => Some(f64::NAN),
                _ => t.parse().ok(),
            }
        }
        _ => None,
    }
}

/// Python's `round(x, 1)` (decimal rounding of the binary value).
pub fn round1(x: f64) -> f64 {
    format!("{x:.1}").parse().unwrap_or(x)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn basics() {
        assert_eq!(splitlines("a\r\nb\rc\n\nd\u{2028}e\n"), vec!["a", "b", "c", "", "d", "e"]);
        assert_eq!(strip("\u{1c} x \n"), "x");
        assert_eq!(repr(&json!({"a": [1, "it's", null, true, 1.5]})), "{'a': [1, \"it's\", None, True, 1.5]}");
        assert!(eq(&json!(1), &json!(1.0)) && eq(&json!(true), &json!(1)) && !eq(&json!("1"), &json!(1)));
        assert_eq!(parse_int(" 1_000 "), Some(1000));
        assert_eq!(head("héllo", 2), "hé");
    }
}
