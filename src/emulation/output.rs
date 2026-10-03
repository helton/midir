//! Stop sequences and max_tokens for backends that support neither, applied to streamed text.
//! Lengths are code points, like Python.

use crate::py::text;

pub struct OutputLimiter {
    stops: Vec<String>,
    max_chars: Option<i64>,
    hold: usize,
    pending: String,
    emitted: i64,
    pub finish: String,
    pub stop_sequence: Option<String>,
}

impl OutputLimiter {
    pub fn new(stops: Vec<String>, max_chars: Option<i64>) -> Self {
        let hold = stops.iter().map(|s| text::len(s) as i64).max().unwrap_or(1) - 1;
        OutputLimiter {
            stops,
            max_chars,
            hold: hold.max(0) as usize,
            pending: String::new(),
            emitted: 0,
            finish: "stop".into(),
            stop_sequence: None,
        }
    }

    /// Returns (text to emit, stop now?).
    pub fn apply(&mut self, chunk: &str, flush: bool) -> (String, bool) {
        self.pending.push_str(chunk);
        for s in &self.stops {
            if let Some(k) = self.pending.find(s.as_str()) {
                let out = self.pending[..k].to_string();
                self.pending.clear();
                self.finish = "stop_sequence".into();
                self.stop_sequence = Some(s.clone());
                self.emitted += text::len(&out) as i64;
                return (out, true);
            }
        }
        let out = if flush || self.hold == 0 {
            std::mem::take(&mut self.pending)
        } else {
            let n = text::len(&self.pending);
            let cut = text::byte_at(&self.pending, n.saturating_sub(self.hold));
            let out = self.pending[..cut].to_string();
            self.pending = self.pending[cut..].to_string();
            out
        };
        let out_len = text::len(&out) as i64;
        self.emitted += out_len;
        if let Some(max) = self.max_chars {
            if self.emitted > max {
                let over = self.emitted - max;
                self.emitted = max;
                self.finish = "length".into();
                let keep = (out_len - over).max(0) as usize;
                return (text::head(&out, keep).to_string(), true);
            }
        }
        (out, false)
    }
}
