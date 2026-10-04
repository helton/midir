//! Character-based string helpers. Prompt sizes, description limits and the chars-per-token estimates count Unicode
//! scalar values (what a user calls characters), never bytes, so a Portuguese prompt is not cut short.

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
    if char_len(s) <= n { s.to_string() } else { format!("{}…", prefix(s, n).trim_end()) }
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
}
