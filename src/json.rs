//! JSON helpers: what clients send is made decodable (lone UTF-16 surrogates, non-standard number literals), what models
//! write in tool calls is decoded tolerantly (raw control characters, trailing commas), and the two text forms Midir
//! writes itself (one-line readable JSON in prompts, key-sorted JSON for comparisons).

use std::borrow::Cow;
use std::io;

use serde::Serialize;
use serde_json::Value;
use serde_json::ser::{Formatter, Serializer};
use serde_json::value::RawValue;

// ---------------------------------------------------------------------------------------------------------------------
// client JSON: decodable whatever the client's JSON encoder did
// ---------------------------------------------------------------------------------------------------------------------

/// Make a JSON document decodable by `serde_json` without changing anything else:
///
/// - an escaped UTF-16 surrogate without its pair (half of an emoji) becomes the escaped replacement character U+FFFD.
///   JavaScript clients write these when a string was cut in the middle of an emoji (`JSON.stringify` escapes lone
///   surrogates), and serde_json rejects them;
/// - the non-standard literals `NaN`, `Infinity` and `-Infinity` (Python's `json.dumps` writes them) become `null`.
///
/// Valid documents come back borrowed and untouched.
pub fn sanitize(input: &[u8]) -> Cow<'_, [u8]> {
    if !needs_sanitizing(input) {
        return Cow::Borrowed(input);
    }
    let mut out = Vec::with_capacity(input.len());
    let mut changed = false;
    let mut i = 0;
    let mut in_string = false;
    while i < input.len() {
        let b = input[i];
        if in_string {
            match b {
                b'"' => {
                    in_string = false;
                    out.push(b);
                    i += 1;
                }
                b'\\' if i + 1 < input.len() && input[i + 1] == b'u' => {
                    match surrogate_at(input, i) {
                        Some(0xD800..=0xDBFF) if matches!(surrogate_at(input, i + 6), Some(0xDC00..=0xDFFF)) => {
                            out.extend_from_slice(&input[i..i + 12]); // a valid pair
                            i += 12;
                        }
                        Some(0xD800..=0xDFFF) => {
                            out.extend_from_slice(REPLACEMENT_ESCAPE); // half of a pair, alone
                            changed = true;
                            i += 6;
                        }
                        _ => {
                            out.extend_from_slice(&input[i..i + 2]);
                            i += 2;
                        }
                    }
                }
                b'\\' if i + 1 < input.len() => {
                    out.extend_from_slice(&input[i..i + 2]); // any other escape, including \\ and \"
                    i += 2;
                }
                _ => {
                    out.push(b);
                    i += 1;
                }
            }
        } else if b == b'"' {
            in_string = true;
            out.push(b);
            i += 1;
        } else if let Some(len) = non_standard_literal(&input[i..]) {
            out.extend_from_slice(b"null");
            changed = true;
            i += len;
        } else {
            out.push(b);
            i += 1;
        }
    }
    if changed { Cow::Owned(out) } else { Cow::Borrowed(input) }
}

/// `\ufffd`, the escaped U+FFFD that takes the place of a lone surrogate.
const REPLACEMENT_ESCAPE: &[u8] = b"\\ufffd";

fn needs_sanitizing(input: &[u8]) -> bool {
    let finder = |needle: &[u8]| memchr::memmem::find(input, needle).is_some();
    finder(br"\ud") || finder(br"\uD") || finder(b"NaN") || finder(b"Infinity")
}

/// The code unit of a `\uXXXX` escape starting at `i`, when it is one.
fn surrogate_at(input: &[u8], i: usize) -> Option<u16> {
    let esc = input.get(i..i + 6)?;
    if esc[0] != b'\\' || esc[1] != b'u' {
        return None;
    }
    u16::from_str_radix(std::str::from_utf8(&esc[2..]).ok()?, 16).ok()
}

/// Length of a `NaN`, `Infinity` or `-Infinity` literal at the start of `s` (outside strings), when it is one.
fn non_standard_literal(s: &[u8]) -> Option<usize> {
    [b"-Infinity".as_slice(), b"Infinity", b"NaN"].into_iter().find(|lit| s.starts_with(lit)).map(<[u8]>::len)
}

/// Raw JSON on one line: whitespace outside strings is dropped when the text has line breaks (a pretty-printed request
/// field echoed into an SSE `data:` line must not break the event); one-line text comes back as is.
pub fn one_line(raw: Box<RawValue>) -> Box<RawValue> {
    let text = raw.get();
    if !text.contains(['\n', '\r']) {
        return raw;
    }
    let mut out = String::with_capacity(text.len());
    let mut in_string = false;
    let mut escaped = false;
    for c in text.chars() {
        if in_string {
            out.push(c);
            match c {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
        } else if c == '"' {
            in_string = true;
            out.push(c);
        } else if !c.is_whitespace() {
            out.push(c);
        }
    }
    RawValue::from_string(out).unwrap_or(raw)
}

/// Whether a parameter's value means something: not null, false, `[]` or `{}`.
pub fn is_meaningful(raw: &RawValue) -> bool {
    let t = raw.get().trim();
    let empty =
        |open: char, close: char| t.strip_prefix(open).and_then(|r| r.strip_suffix(close)).is_some_and(|inner| inner.trim().is_empty());
    !(t == "null" || t == "false" || empty('[', ']') || empty('{', '}'))
}

// ---------------------------------------------------------------------------------------------------------------------
// model JSON: tolerant decoding of what a model wrote in a <tool_call>
// ---------------------------------------------------------------------------------------------------------------------

/// Decode JSON the way a model meant it: raw control characters inside strings (a literal newline or TAB in a file's
/// content, the most common mistake) are escaped, and trailing commas before `}` or `]` are dropped. Returns the value
/// and whether anything had to be fixed. Strict JSON is decoded as is.
pub fn decode_lenient(text: &str) -> Result<(Value, bool), serde_json::Error> {
    match serde_json::from_str(text) {
        Ok(v) => Ok((v, false)),
        Err(e) => {
            let fixed = repair_model_json(text);
            if fixed == text {
                return Err(e);
            }
            serde_json::from_str(&fixed).map(|v| (v, true)).map_err(|_| e)
        }
    }
}

/// Escape raw control characters inside strings and drop trailing commas, leaving everything else as written.
pub fn repair_model_json(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    let mut in_string = false;
    let mut escaped = false;
    let chars: Vec<char> = text.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        if in_string {
            if escaped {
                escaped = false;
                out.push(c);
                continue;
            }
            match c {
                '\\' => {
                    escaped = true;
                    out.push(c);
                }
                '"' => {
                    in_string = false;
                    out.push(c);
                }
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
                c => out.push(c),
            }
        } else {
            match c {
                '"' => {
                    in_string = true;
                    out.push(c);
                }
                ',' if next_significant(&chars, i + 1).is_some_and(|n| n == '}' || n == ']') => {}
                c => out.push(c),
            }
        }
    }
    out
}

fn next_significant(chars: &[char], from: usize) -> Option<char> {
    chars[from..].iter().copied().find(|c| !c.is_whitespace())
}

// ---------------------------------------------------------------------------------------------------------------------
// JSON text written by Midir
// ---------------------------------------------------------------------------------------------------------------------

/// One-line JSON with a space after `:` and `,` (`{"name": "x", "arguments": {"a": 1}}`): the form the tool protocol
/// shows the model, used for every JSON value written into a prompt (tool calls in the history, schemas).
pub fn readable<T: Serialize + ?Sized>(value: &T) -> String {
    struct Spaced;
    impl Formatter for Spaced {
        fn begin_array_value<W: ?Sized + io::Write>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
            if first { Ok(()) } else { w.write_all(b", ") }
        }
        fn begin_object_key<W: ?Sized + io::Write>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
            if first { Ok(()) } else { w.write_all(b", ") }
        }
        fn begin_object_value<W: ?Sized + io::Write>(&mut self, w: &mut W) -> io::Result<()> {
            w.write_all(b": ")
        }
    }
    let mut out = Vec::new();
    let mut ser = Serializer::with_formatter(&mut out, Spaced);
    match value.serialize(&mut ser) {
        Ok(()) => String::from_utf8(out).unwrap_or_default(),
        Err(_) => String::new(),
    }
}

/// JSON text with object keys sorted at every level: equal values give equal text (deduplication, content hashes).
pub fn sorted(v: &Value) -> String {
    fn sort(v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                Value::Object(keys.into_iter().map(|k| (k.clone(), sort(&m[k]))).collect())
            }
            Value::Array(a) => Value::Array(a.iter().map(sort).collect()),
            other => other.clone(),
        }
    }
    sort(v).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::json;

    proptest! {
        #[test]
        fn any_utf16_text_escaped_as_javascript_writes_it_decodes(units in prop::collection::vec(any::<u16>(), 0..40)) {
            // JSON.stringify escapes lone surrogates; here every code unit is escaped
            let doc: String = std::iter::once("\"".to_string()).chain(units.iter().map(|u| format!("{}u{u:04x}", char::from(92u8)))).chain(std::iter::once("\"".to_string())).collect();
            let decoded: String = serde_json::from_slice(&sanitize(doc.as_bytes())).unwrap();
            prop_assert_eq!(decoded, String::from_utf16_lossy(&units));
        }

        #[test]
        fn sanitize_never_panics_and_leaves_plain_json_alone(input in prop::collection::vec(any::<u8>(), 0..200), text in "[ -~ç]{0,40}") {
            let _ = sanitize(&input);
            let plain = serde_json::to_vec(&json!({"t": text, "n": [1, 2.5]})).unwrap();
            prop_assert!(matches!(sanitize(&plain), Cow::Borrowed(_)));
        }

        #[test]
        fn strict_json_is_decoded_as_is(text in "[ -~\n\tç]{0,30}", n in any::<i64>()) {
            let v = json!({"a": text, "b": [n, null, true], "c": {"d": "e,}"}});
            prop_assert_eq!(decode_lenient(&v.to_string()).unwrap(), (v, false));
        }
    }

    fn clean(s: &str) -> String {
        String::from_utf8(sanitize(s.as_bytes()).into_owned()).unwrap()
    }

    /// `\uXXXX` escape text (built at run time so no tool or editor turns it into the character).
    fn esc(hex: &str) -> String {
        format!("{}u{hex}", char::from(92u8))
    }

    #[test]
    fn lone_surrogates_become_replacement_characters() {
        let (high, low, fffd) = (esc("d83c"), esc("df89"), esc("fffd"));
        let doc = |s: &str| format!(r#"{{"a":"{s}"}}"#);
        assert_eq!(clean(&doc(&format!("x {high}"))), doc(&format!("x {fffd}")));
        assert_eq!(clean(&doc(&format!("{low} y"))), doc(&format!("{fffd} y")));
        assert_eq!(clean(&doc(&format!("{high}{low}"))), doc(&format!("{high}{low}"))); // a valid pair stays
        let escaped_backslash = format!("{0}{0}ud83c", char::from(92u8)); // an escaped backslash, then text
        assert_eq!(clean(&doc(&escaped_backslash)), doc(&escaped_backslash));
        assert_eq!(clean(&doc(&format!("{high}{}", esc("0041")))), doc(&format!("{fffd}{}", esc("0041"))));
        let v: Value = serde_json::from_slice(&sanitize(doc(&format!("cut {high}")).as_bytes())).unwrap();
        assert_eq!(v["a"], format!("cut {}", char::REPLACEMENT_CHARACTER));
    }

    #[test]
    fn non_standard_numbers_become_null_outside_strings_only() {
        assert_eq!(
            clean(r#"{"a":NaN,"b":-Infinity,"c":[Infinity],"d":"NaN Infinity"}"#),
            r#"{"a":null,"b":null,"c":[null],"d":"NaN Infinity"}"#
        );
    }

    #[test]
    fn valid_documents_are_borrowed() {
        assert!(matches!(sanitize(br#"{"a":1,"b":"text"}"#), Cow::Borrowed(_)));
    }

    #[test]
    fn lenient_model_json() {
        let (v, fixed) = decode_lenient("{\"path\": \"a.py\", \"content\": \"line1\nline2\ttab\"}").unwrap();
        assert!(fixed && v["content"] == "line1\nline2\ttab");
        let (v, fixed) = decode_lenient(r#"{"a": [1, 2,], "b": {"c": 3,},}"#).unwrap();
        assert!(fixed && v == json!({"a": [1, 2], "b": {"c": 3}}));
        let (_, fixed) = decode_lenient(r#"{"a": "x,}"}"#).unwrap();
        assert!(!fixed); // commas inside strings are text
        assert!(decode_lenient(r#"{"a": "x"#).is_err());
    }

    #[test]
    fn raw_values_on_one_line() {
        let raw = |s: &str| serde_json::from_str::<Box<RawValue>>(s).unwrap();
        assert_eq!(one_line(raw("[\n  {\"a\": \"x y\\n\"},\n  2\n]")).get(), "[{\"a\":\"x y\\n\"},2]");
        assert_eq!(one_line(raw("{\"a\": 1}")).get(), "{\"a\": 1}");
        assert!(!is_meaningful(&raw("{ }")) && !is_meaningful(&raw("false")) && is_meaningful(&raw("[0]")));
    }

    #[test]
    fn readable_and_sorted() {
        let v = json!({"name": "write", "arguments": {"path": "a b", "n": [1, 2.5, null], "s": "x, y: z"}});
        assert_eq!(readable(&v), r#"{"name": "write", "arguments": {"path": "a b", "n": [1, 2.5, null], "s": "x, y: z"}}"#);
        assert_eq!(readable(&json!({})), "{}");
        assert_eq!(sorted(&json!({"b": 1, "a": {"d": 2, "c": 3}})), r#"{"a":{"c":3,"d":2},"b":1}"#);
    }
}
