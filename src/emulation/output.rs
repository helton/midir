//! Stop sequences and max_tokens for backends that support neither, applied to streamed text. Lengths are in
//! characters.

use crate::canonical::Finish;
use crate::text::{byte_offset_from_end, char_len, prefix};

pub struct OutputLimiter {
    stops: Vec<String>,
    max_chars: Option<usize>,
    /// characters held back: a stop sequence may be split across chunks
    hold: usize,
    pending: String,
    emitted: usize,
    pub finish: Finish,
    pub stop_sequence: Option<String>,
}

impl OutputLimiter {
    pub fn new(stops: Vec<String>, max_chars: Option<usize>) -> Self {
        let stops: Vec<String> = stops.into_iter().filter(|s| !s.is_empty()).collect();
        let hold = stops.iter().map(|s| char_len(s)).max().unwrap_or(1).saturating_sub(1);
        OutputLimiter { stops, max_chars, hold, pending: String::new(), emitted: 0, finish: Finish::Stop, stop_sequence: None }
    }

    /// Returns (text to emit, stop now?).
    pub fn apply(&mut self, chunk: &str, flush: bool) -> (String, bool) {
        self.pending.push_str(chunk);
        // the stop sequence a model writing this text would complete first (the earliest end), as in streaming
        let first_stop =
            self.stops.iter().filter_map(|s| self.pending.find(s.as_str()).map(|k| (k, s))).min_by_key(|(k, s)| (*k + s.len(), *k));
        if let Some((k, s)) = first_stop {
            let out = self.pending[..k].to_string();
            let s = s.clone();
            self.pending.clear();
            let out_len = char_len(&out);
            // the length limit can come before the stop sequence
            if let Some(max) = self.max_chars.filter(|max| self.emitted + out_len > *max) {
                let keep = max.saturating_sub(self.emitted);
                self.emitted = max;
                self.finish = Finish::Length;
                return (prefix(&out, keep).to_string(), true);
            }
            self.stop_sequence = Some(s);
            self.finish = Finish::StopSequence;
            self.emitted += out_len;
            return (out, true);
        }
        let out = if flush || self.hold == 0 {
            std::mem::take(&mut self.pending)
        } else {
            let cut = byte_offset_from_end(&self.pending, self.hold);
            let rest = self.pending.split_off(cut);
            std::mem::replace(&mut self.pending, rest)
        };
        let out_len = char_len(&out);
        self.emitted += out_len;
        if let Some(max) = self.max_chars
            && self.emitted > max
        {
            let over = self.emitted - max;
            self.emitted = max;
            self.finish = Finish::Length;
            return (prefix(&out, out_len.saturating_sub(over)).to_string(), true);
        }
        (out, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// (text out, finish, stop sequence) for a text fed in chunks cut at `cuts` (character positions).
    fn run(text: &str, stops: &[&str], max: Option<usize>, cuts: &[usize]) -> (String, Finish, Option<String>) {
        let mut l = OutputLimiter::new(stops.iter().map(|s| s.to_string()).collect(), max);
        let chars: Vec<char> = text.chars().collect();
        let mut bounds: Vec<usize> = cuts.iter().map(|c| c % (chars.len() + 1)).collect();
        bounds.extend([0, chars.len()]);
        bounds.sort_unstable();
        bounds.dedup();
        let mut out = String::new();
        let mut stopped = false;
        for w in bounds.windows(2) {
            let (o, stop) = l.apply(&chars[w[0]..w[1]].iter().collect::<String>(), false);
            out.push_str(&o);
            if stop {
                stopped = true;
                break;
            }
        }
        if !stopped {
            out.push_str(&l.apply("", true).0);
        }
        (out, l.finish, l.stop_sequence.clone())
    }

    proptest! {
        #[test]
        fn chunking_never_changes_the_cut(text in "[abXYZWç ]{0,40}", cuts in prop::collection::vec(0usize..60, 0..10), max in prop::option::of(0usize..30)) {
            prop_assert_eq!(run(&text, &["XYZ", "ç a", "Z", ""], max, &cuts), run(&text, &["XYZ", "ç a", "Z", ""], max, &[]));
        }
    }

    #[test]
    fn the_length_limit_can_come_before_a_stop_sequence() {
        // found by the chunking property: a stop sequence in one big chunk must not let the text exceed max_tokens
        let mut l = OutputLimiter::new(vec!["END".into()], Some(5));
        assert_eq!(l.apply("abcdefgh END more", true), ("abcde".into(), true));
        assert_eq!((l.finish, l.stop_sequence.clone()), (Finish::Length, None));
        let mut l = OutputLimiter::new(vec!["END".into()], Some(5));
        assert_eq!(l.apply("abcEND", true), ("abc".into(), true));
        assert_eq!(l.finish, Finish::StopSequence);
    }

    #[test]
    fn stop_split_across_chunks_and_length() {
        let mut l = OutputLimiter::new(vec!["XYZ".into()], None);
        assert_eq!(l.apply("abcX", false), ("ab".into(), false));
        assert_eq!(l.apply("YZdef", false), ("c".into(), true));
        assert_eq!(l.finish, Finish::StopSequence);
        let mut l = OutputLimiter::new(vec![], Some(5));
        assert_eq!(l.apply("ação ok", false), ("ação ".into(), true));
        assert_eq!(l.finish, Finish::Length);
    }
}
