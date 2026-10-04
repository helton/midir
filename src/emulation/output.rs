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
        let hold = stops.iter().map(|s| char_len(s)).max().unwrap_or(1).saturating_sub(1);
        OutputLimiter { stops, max_chars, hold, pending: String::new(), emitted: 0, finish: Finish::Stop, stop_sequence: None }
    }

    /// Returns (text to emit, stop now?).
    pub fn apply(&mut self, chunk: &str, flush: bool) -> (String, bool) {
        self.pending.push_str(chunk);
        let first_stop = self.stops.iter().filter_map(|s| self.pending.find(s.as_str()).map(|k| (k, s))).min_by_key(|(k, _)| *k);
        if let Some((k, s)) = first_stop {
            let out = self.pending[..k].to_string();
            self.stop_sequence = Some(s.clone());
            self.pending.clear();
            self.finish = Finish::StopSequence;
            self.emitted += char_len(&out);
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
        if let Some(max) = self.max_chars {
            if self.emitted > max {
                let over = self.emitted - max;
                self.emitted = max;
                self.finish = Finish::Length;
                return (prefix(&out, out_len.saturating_sub(over)).to_string(), true);
            }
        }
        (out, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
