"""Errors that cross module boundaries. Each protocol renders them in its own error format (:mod:`midir.app`)."""
from __future__ import annotations

import json
from typing import Any


class BackendError(Exception):
    """An error answered by a backend, with its raw status, headers and body. Never swallowed: the client gets the
    backend's message in its own protocol's error format."""

    def __init__(self, status: int, body: Any, headers: dict | None = None, where: str = "agent", backend: str = "backend") -> None:
        self.status, self.body, self.headers, self.where, self.backend = status, body, headers or {}, where, backend
        super().__init__(f"{backend} {where} HTTP {status}: {str(body)[:300]}")

    @property
    def retryable(self) -> bool:
        return self.status == 429 or self.status >= 500

    def http_status(self) -> int:
        """The status the client gets: the backend's own for the ones clients handle, 502 for everything else."""
        return self.status if self.status in (400, 401, 403, 404, 429) else 502

    def message(self) -> str:
        body = self.body if isinstance(self.body, str) else json.dumps(self.body, ensure_ascii=False)
        return f"{self.backend} {self.where} HTTP {self.status}: {body[:2000]}"


class QueueTimeout(BackendError):
    """The request waited longer than queue_timeout_s for a backend slot; surfaced as 429, never retried."""

    def __init__(self, timeout: float, needed: float | None = None, backend: str = "backend") -> None:
        why = f"the next slot is {needed:.0f}s away" if needed is not None else f"no slot within {timeout:.0f}s"
        super().__init__(429, {"message": f"queue: {why}, above limits.queue_timeout_s={timeout:.0f} (requests_per_minute / max_concurrent)"}, where="queue", backend=backend)

    @property
    def retryable(self) -> bool:
        return False


class ClientError(Exception):
    """Invalid request or unsupported feature; rendered as 400/404 in the protocol's error format."""

    def __init__(self, message: str, code: str = "invalid_request", status: int = 400) -> None:
        self.message, self.code, self.status = message, code, status
        super().__init__(message)
