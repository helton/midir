"""Backends: what serves a model. `create_backend` builds one from its [backends.<name>] settings by `type`."""
from __future__ import annotations

from midir.backends.base import Completion, TextBackend
from midir.backends.stackspot import StackSpotBackend
from midir.config import BackendSettings, ConfigError
from midir.telemetry import Telemetry

BACKEND_TYPES: dict[str, type[TextBackend]] = {StackSpotBackend.type: StackSpotBackend}


def create_backend(settings: BackendSettings, env: dict[str, str] | None = None, telemetry: Telemetry | None = None) -> TextBackend:
    cls = BACKEND_TYPES.get(settings.type)
    if cls is None:
        raise ConfigError(f"backend {settings.name!r}: unknown type {settings.type!r} (available: {', '.join(sorted(BACKEND_TYPES))})")
    return cls(settings, env, telemetry)


__all__ = ["BACKEND_TYPES", "Completion", "TextBackend", "StackSpotBackend", "create_backend"]
