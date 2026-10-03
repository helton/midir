"""Stop sequences and max_tokens for backends that support neither: applied to the streamed text."""
from __future__ import annotations

class OutputLimiter:
    """Applies stop sequences and the approximate max_tokens cut to streamed text.
    Holds back a tail as long as the longest stop sequence so a stop split across chunks is still caught."""

    def __init__(self, stops: list[str], max_chars: int | None) -> None:
        self.stops, self.max_chars = stops, max_chars
        self.hold = max((len(s) for s in stops), default=1) - 1
        self.pending = ""
        self.emitted = 0
        self.finish = "stop"
        self.stop_sequence: str | None = None

    def apply(self, chunk: str, flush: bool = False) -> tuple[str, bool]:
        """Returns (text to emit, stop now?)."""
        self.pending += chunk
        for s in self.stops:
            k = self.pending.find(s)
            if k >= 0:
                out, self.pending = self.pending[:k], ""
                self.finish, self.stop_sequence = "stop_sequence", s
                self.emitted += len(out)
                return out, True
        if flush or not self.hold:
            out, self.pending = self.pending, ""
        else:
            out, self.pending = self.pending[:-self.hold], self.pending[-self.hold:]
        self.emitted += len(out)
        if self.max_chars is not None and self.emitted > self.max_chars:
            over = self.emitted - self.max_chars
            self.emitted = self.max_chars
            self.finish = "length"
            return out[: max(len(out) - over, 0)], True
        return out, False
