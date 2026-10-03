"""Backend interface. A backend turns one request into a response stream; the gateway routes each model to one.

Today there is one kind: :class:`TextBackend`, for APIs that take a single text prompt and stream text back (StackSpot
AI agents). The emulation layer (:mod:`midir.emulation`) builds roles, tools, structured output and stop sequences on
top of it. A backend with a native chat API (tools and messages of its own) would implement the same runner contract
as :class:`midir.emulation.engine.EmulationEngine` (``run`` and ``complete`` over canonical requests) and skip the
emulation entirely; see docs/architecture.md.
"""
from __future__ import annotations

import logging
from abc import ABC, abstractmethod
from dataclasses import dataclass
from typing import Any, AsyncIterator

import httpx
from tenacity import RetryCallState, retry_if_exception, stop_after_attempt, wait_exponential

from midir.config import BackendSettings
from midir.errors import BackendError
from midir.limiter import UpstreamLimiter
from midir.telemetry import Telemetry

log = logging.getLogger(__name__)

TIMEOUT = httpx.Timeout(connect=20.0, read=600.0, write=60.0, pool=20.0)


@dataclass
class Completion:
    """The end of a text stream. `usage` is None when the backend reported nothing (the engine then estimates it)."""

    usage: dict | None = None
    message_id: str | None = None
    stop_reason: str | None = None


def is_retryable(e: BaseException) -> bool:
    if isinstance(e, BackendError):
        return e.retryable
    return isinstance(e, (httpx.TimeoutException, httpx.ConnectError, httpx.RemoteProtocolError))


class TextBackend(ABC):
    """A text-in, text-out backend with its own queue (:class:`UpstreamLimiter`) and retry policy (1, 2, 4 s on 429,
    5xx and connection errors, only until the response starts)."""

    type: str = ""

    def __init__(self, settings: BackendSettings, env: dict[str, str] | None = None, telemetry: Telemetry | None = None, backoff_s: float = 1.0) -> None:
        self.name = settings.name
        self.settings = settings
        self.env = env or {}
        self.telemetry = telemetry or Telemetry()
        self.limiter = UpstreamLimiter(settings.limits, self.telemetry, backend=self.name)
        self.retry: dict[str, Any] = dict(retry=retry_if_exception(is_retryable), stop=stop_after_attempt(4), wait=wait_exponential(multiplier=backoff_s, min=backoff_s, max=4 * backoff_s),
                                          before_sleep=self._log_retry, reraise=True)

    def _log_retry(self, state: RetryCallState) -> None:
        e = state.outcome.exception() if state.outcome else None
        log.warning("%s attempt %s failed (%r); waiting %.1fs", self.name, state.attempt_number, e, state.next_action.sleep if state.next_action else 0)
        self.telemetry.upstream_retry(getattr(e, "status", None) or type(e).__name__)

    def validate(self) -> None:
        """Called at startup: raise midir.config.ConfigError when required settings are missing."""

    @abstractmethod
    def stream(self, prompt: str, target: str, meta: dict | None = None) -> AsyncIterator[str | Completion]:
        """Yield text deltas, then exactly one Completion. Raise BackendError for errors answered by the backend."""

    @abstractmethod
    async def ready(self) -> None:
        """Readiness without spending model quota (credentials, network, TLS); raise BackendError or httpx.HTTPError."""

    def input_limit_exceeded(self, error: BackendError) -> tuple[int, int] | None:
        """(limit, actual) input tokens when `error` is the backend refusing a prompt for its size; None otherwise."""
        return None

    def describe_target(self, target: str) -> str:
        """How /health shows a model's target (abbreviated when it is an id)."""
        return target

    async def aclose(self) -> None:
        """Release connections."""
