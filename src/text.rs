//! Text helpers. Prompt sizes, description limits and the chars-per-token estimates count Unicode scalar values (what
//! a user calls characters), never bytes, so a Portuguese prompt is not cut short. Also the one-line JSON style used
//! in prompts.

use std::io;

use serde::Serialize;
use serde_json::ser::{Formatter, Serializer};

/// One-line JSON with a space after `:` and `,` (`{"name": "x", "arguments": {"a": 1}}`): the form the tool protocol
/// shows the model, used for every JSON value written into a prompt (tool calls in the history, schemas).
pub fn readable_json<T: Serialize + ?Sized>(value: &T) -> String {
    struct Spaced;
    impl Formatter for Spaced {
        fn begin_array_value<W: ?Sized + io::Write>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
            if first {
                Ok(())
            } else {
                w.write_all(b", ")
            }
        }
        fn begin_object_key<W: ?Sized + io::Write>(&mut self, w: &mut W, first: bool) -> io::Result<()> {
            if first {
                Ok(())
            } else {
                w.write_all(b", ")
            }
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

/// Number of characters.
pub fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// The first `n` characters (all of `s` when it is shorter).
pub fn prefix(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

/// `s` without its first `n` characters.
pub fn skip_chars(s: &str, n: usize) -> &str {
    match s.char_indices().nth(n) {
        Some((i, _)) => &s[i..],
        None => "",
    }
}

/// At most `n` characters, with an ellipsis when something was cut (descriptions in prompts).
pub fn ellipsize(s: &str, n: usize) -> String {
    if char_len(s) <= n {
        s.to_string()
    } else {
        format!("{}…", prefix(s, n).trim_end())
    }
}

/// Byte offset of the character `n` characters before the end of `s` (for holding back a stop-sequence tail).
pub fn byte_offset_from_end(s: &str, n: usize) -> usize {
    if n == 0 {
        return s.len();
    }
    s.char_indices().rev().nth(n - 1).map_or(0, |(i, _)| i)
}

/// One decimal place, for durations and pauses in reports.
pub fn round1(x: f64) -> f64 {
    (x * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn characters_not_bytes() {
        assert_eq!(char_len("ação"), 4);
        assert_eq!(prefix("héllo", 2), "hé");
        assert_eq!(prefix("ab", 5), "ab");
        assert_eq!(skip_chars("resp_abc", 5), "abc");
        assert_eq!(ellipsize("uma descrição longa", 5), "uma d…");
        assert_eq!(ellipsize("curta", 10), "curta");
        assert_eq!(&"abçde"[byte_offset_from_end("abçde", 3)..], "çde");
        assert_eq!(byte_offset_from_end("ab", 0), 2);
        assert_eq!(byte_offset_from_end("ab", 5), 0);
        assert_eq!(round1(2.25000001), 2.3);
    }

    #[test]
    fn readable_json_spacing() {
        let v = serde_json::json!({"name": "write", "arguments": {"path": "a b", "n": [1, 2.5, null], "s": "x, y: z"}});
        assert_eq!(readable_json(&v), r#"{"name": "write", "arguments": {"path": "a b", "n": [1, 2.5, null], "s": "x, y: z"}}"#);
        assert_eq!(readable_json(&serde_json::json!({})), "{}");
    }
}
