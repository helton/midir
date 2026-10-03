"""In-memory queue in front of one backend: concurrency, requests per minute, and a budget that adapts to 429s."""
from __future__ import annotations

import asyncio
import logging
import time
from collections import deque

from midir.config import LimitSettings
from midir.errors import QueueTimeout
from midir.telemetry import Telemetry

log = logging.getLogger(__name__)


class UpstreamLimiter:
    """At most `max_concurrent` open backend calls and `requests_per_minute` request starts in any 60 s window (every
    POST counts: follow-ups, repairs, retries, token-renewal retries). Waiters are served in arrival order. A backend 429
    pauses new starts for `cooldown_on_429_s` and halves the local budget, which grows back by one request per minute:
    other clients of the same account (the backend's own UI, another machine) consume the quota too, and this absorbs
    them without any shared state. One process needs no external store."""

    def __init__(self, limits: LimitSettings, telemetry: Telemetry | None = None, backend: str = "backend") -> None:
        self.max_concurrent, self.rpm, self.timeout, self.cooldown = limits.max_concurrent, limits.requests_per_minute, limits.queue_timeout_s, limits.cooldown_on_429_s
        self.telemetry = telemetry or Telemetry()
        self.backend = backend
        self._slots = asyncio.Semaphore(max(1, self.max_concurrent))
        self._starts: deque[float] = deque()
        self._rate_lock = asyncio.Lock()
        self._paused_until = 0.0
        self._budget: int | None = None  # requests/minute after a 429 (None = the configured value)
        self._budget_at = 0.0
        self.in_flight = 0
        self.waiting = 0

    def state(self) -> dict:
        now = time.monotonic()
        while self._starts and now - self._starts[0] >= 60:
            self._starts.popleft()
        return {"max_concurrent": self.max_concurrent, "requests_per_minute": self.rpm, "effective_rpm": self.effective_rpm(), "in_flight": self.in_flight,
                "waiting": self.waiting, "starts_last_60s": len(self._starts), "paused_s": round(max(0.0, self._paused_until - now), 1)}

    def retry_after(self) -> int:
        """Seconds a client should wait before trying again (Retry-After on our 429s)."""
        st = self.state()
        rpm = self.effective_rpm()
        window = (self._starts[-rpm] + 60 - time.monotonic()) if rpm > 0 and len(self._starts) >= rpm else 0
        return max(1, int(max(st["paused_s"], window, self.cooldown) + 0.999))

    def effective_rpm(self) -> int:
        """The configured budget, halved by each backend 429 and grown back by one request per minute without one."""
        if self._budget is None or self.rpm <= 0:
            return self.rpm
        grown = self._budget + int((time.monotonic() - self._budget_at) // 60)
        if grown >= self.rpm:
            self._budget = None
            return self.rpm
        return grown

    def on_429(self) -> None:
        now = time.monotonic()
        self._paused_until = max(self._paused_until, now + self.cooldown)
        if self.rpm > 0:
            self._budget, self._budget_at = max(10, self.effective_rpm() // 2), now
            log.warning("%s 429: pausing %.0fs and lowering the local budget to %d requests/minute (recovers 1/min)", self.backend, self.cooldown, self._budget)

    async def acquire_slot(self, deadline: float) -> None:
        self.waiting += 1
        self.telemetry.queue_depth(+1)
        try:
            await asyncio.wait_for(self._slots.acquire(), max(0.001, deadline - time.monotonic()))
        except asyncio.TimeoutError:
            raise QueueTimeout(self.timeout, backend=self.backend) from None
        finally:
            self.waiting -= 1
            self.telemetry.queue_depth(-1)
        self.in_flight += 1

    def release_slot(self) -> None:
        self.in_flight -= 1
        self._slots.release()

    async def start(self, deadline: float) -> None:
        """Wait for room in the 60 s window (and for a 429 pause to end), then record one request start."""
        if self.rpm <= 0:
            return
        async with self._rate_lock:  # FIFO: the next start waits behind earlier ones
            while True:
                now = time.monotonic()
                while self._starts and now - self._starts[0] >= 60:
                    self._starts.popleft()
                rpm = self.effective_rpm()
                wait = max(self._paused_until - now, (self._starts[-rpm] + 60 - now) if len(self._starts) >= rpm else 0.0)
                if wait <= 0:
                    self._starts.append(now)
                    return
                if now + wait > deadline:
                    raise QueueTimeout(self.timeout, wait, backend=self.backend)
                await asyncio.sleep(min(wait, 5.0))
