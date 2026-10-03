"""The gateway: configured backends, the runner that serves each model, and the Responses store."""
from __future__ import annotations

import logging
from typing import AsyncIterator, Protocol

from midir.backends import TextBackend, create_backend
from midir.canonical import CanonicalRequest, CanonicalResponse, Event
from midir.config import Config, ModelSpec
from midir.emulation import EmulationEngine
from midir.store import ResponseStore
from midir.telemetry import Telemetry

log = logging.getLogger(__name__)


class Runner(Protocol):
    """What the HTTP layer calls for a model. EmulationEngine implements it on top of a text backend; a backend with a
    native chat API (messages and tools of its own) would implement it directly."""

    def run(self, req: CanonicalRequest, rid: str) -> AsyncIterator[Event]: ...

    async def complete(self, req: CanonicalRequest, rid: str) -> CanonicalResponse: ...


class Gateway:
    def __init__(self, config: Config, telemetry: Telemetry | None = None, backends: dict[str, TextBackend] | None = None, store: ResponseStore | None = None) -> None:
        self.config = config
        self.telemetry = telemetry or Telemetry()
        self.backends = backends if backends is not None else {name: create_backend(s, config.env, self.telemetry, config.server.retry_backoff_s) for name, s in config.backends.items()}
        self.runners: dict[str, Runner] = {name: EmulationEngine(b, config) for name, b in self.backends.items()}
        s = config.server
        self.store = store if store is not None else ResponseStore(s.responses_dir, s.responses_retention_days, s.responses_max_mb)

    def validate(self) -> None:
        """Startup check: every backend that serves a model has what it needs (credentials, ...)."""
        used = {m.backend for m in self.config.exposed_models}
        for name in used:
            self.backends[name].validate()

    def route(self, model: str | None) -> tuple[ModelSpec, Runner]:
        spec = self.config.resolve(model)
        return spec, self.runners[spec.backend]

    def backend_of(self, name: str) -> TextBackend | None:
        return self.backends.get(name)

    async def ready(self) -> dict[str, str | None]:
        """Per backend: None when ready, else the reason."""
        from midir.errors import BackendError
        import httpx
        out: dict[str, str | None] = {}
        for name, b in self.backends.items():
            try:
                await b.ready()
                out[name] = None
            except BackendError as e:
                out[name] = e.message()[:500]
            except httpx.HTTPError as e:
                out[name] = f"{name}: {e!r}"[:500]
        return out

    def describe_models(self) -> dict:
        return {m.name: {"backend": m.backend, "target": self.backends[m.backend].describe_target(m.target), "description": m.description,
                         "aliases": m.aliases, "match": m.match.pattern if m.match else None} for m in self.config.exposed_models}

    async def aclose(self) -> None:
        for b in self.backends.values():
            await b.aclose()
