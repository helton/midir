"""Configuration: config/midir.toml (or MIDIR_CONFIG) plus the environment.

Values may reference environment variables as ``${NAME}``; secrets live in .env and are referenced, never written in
the file. An environment variable with the same meaning always wins over the file (12-factor: Docker, CI and other
machines override without editing it). Format: config/midir.example.toml.

    default_model = "gpt-5.1"
    [server]    port, max_prompt_chars, tail_reminder, tool_desc_max, keepalive_s, responses_dir, responses_retention_days,
                responses_max_mb
    [telemetry] otlp_endpoint, service_name
    [backends.<name>]          type (default: the name) + the backend's own keys (see midir.backends)
    [backends.<name>.limits]   max_concurrent, requests_per_minute, queue_timeout_s, cooldown_on_429_s
    [[models]]  name, backend (optional with one backend), target, description, aliases, match,
                max_prompt_chars, tail_reminder, tool_desc_max

The pre-0.0.1 layout (`default`, `[stackspot]`, `[limits]`, `[[agents]]` with `agent_id`) is still read, with a warning.
"""
from __future__ import annotations

import logging
import os
import re
import tomllib
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

log = logging.getLogger(__name__)

DEFAULT_CONFIG = Path("config") / "midir.toml"
DEFAULT_MODEL_NAME = "default"
ENV_REF = re.compile(r"\$\{([A-Z0-9_]+)\}")
FALSE = ("0", "false", "no", "off")


@dataclass
class LimitSettings:
    """In-memory queue in front of one backend (its own quota)."""

    max_concurrent: int = 8
    requests_per_minute: int = 90  # StackSpot allows 100 per account; keep a margin
    queue_timeout_s: float = 600.0
    cooldown_on_429_s: float = 15.0


@dataclass
class BackendSettings:
    """One configured backend: its type, its own options (parsed by the backend class) and its queue limits."""

    name: str
    type: str
    options: dict = field(default_factory=dict)
    limits: LimitSettings = field(default_factory=LimitSettings)


@dataclass
class ModelSpec:
    """One model exposed to clients: the backend that serves it and the backend's target (StackSpot: the agent id).
    Knobs left as None inherit the [server] values."""

    name: str
    backend: str
    target: str
    description: str = ""
    aliases: list[str] = field(default_factory=list)  # extra exact names (case-insensitive)
    match: Any = None  # compiled regex tried against the requested model, in file order
    max_prompt_chars: int | None = None
    tail_reminder: bool | None = None
    tool_desc_max: int | None = None


@dataclass
class ServerSettings:
    port: int = 18880
    max_prompt_chars: int = 1_000_000  # StackSpot rejects inputs above 272k tokens (~1.1M chars of prose); keep a margin
    tail_reminder: bool = True  # one-line tool-protocol reminder after the last turn (measured: fewer follow-ups)
    tool_desc_max: int = 0  # truncate tool descriptions to N chars (0 = off; ~12% fewer input tokens, alters what clients send)
    responses_dir: Path | None = Path("data/gateway/responses")  # previous_response_id store; None = memory only
    responses_retention_days: float = 30.0
    responses_max_mb: float = 500.0
    keepalive_s: float = 15.0  # SSE keepalive while a stream waits for its first content (0 = off)
    retry_backoff_s: float = 1.0  # first backend retry wait; then doubled (1, 2, 4 s). Tests use a tiny value


@dataclass
class TelemetrySettings:
    otlp_endpoint: str = ""  # unset = nothing exported, no cost
    service_name: str = "midir"


class ConfigError(SystemExit):
    """Invalid configuration: the process stops with a message that names the file and the key."""


class Config:
    """The whole configuration, loaded once at startup."""

    def __init__(self, env: dict[str, str] | None = None, config_file: Path | None = None, root: Path | None = None) -> None:
        e = dict(env if env is not None else os.environ)
        self.env = e
        self.root = root or Path.cwd()
        self.config_file = config_file if config_file is not None else Path(e.get("MIDIR_CONFIG") or (self.root / DEFAULT_CONFIG))
        self.source = str(self.config_file) if self.config_file.is_file() else "env"
        data: dict = {}
        if self.config_file.is_file():
            try:
                data = tomllib.loads(self.config_file.read_text())
            except (OSError, tomllib.TOMLDecodeError) as err:
                raise ConfigError(f"{self.config_file}: cannot read: {err}")
            data = self._upgrade_legacy(data)
        self.server = self._server(data.get("server") or {})
        self.telemetry = TelemetrySettings(
            otlp_endpoint=str(self._get(data.get("telemetry"), "otlp_endpoint", "OTEL_EXPORTER_OTLP_ENDPOINT", "") or "").strip(),
            service_name=str(self._get(data.get("telemetry"), "service_name", "OTEL_SERVICE_NAME", "midir")))
        self.backends = self._backends(data.get("backends") or {})
        self.models: list[ModelSpec] = []
        self.default: ModelSpec
        if data.get("models"):
            self._models(data)
        else:
            self._models_from_env()

    # ---- loading ----
    def expand(self, v: Any) -> Any:
        """${NAME} references replaced by the environment (empty when unset)."""
        if isinstance(v, str):
            return ENV_REF.sub(lambda m: self.env.get(m.group(1), ""), v)
        if isinstance(v, dict):
            return {k: self.expand(x) for k, x in v.items()}
        if isinstance(v, list):
            return [self.expand(x) for x in v]
        return v

    def _get(self, section: dict | None, key: str, env_name: str, default: Any) -> Any:
        if self.env.get(env_name) not in (None, ""):
            return self.env[env_name]
        v = (section or {}).get(key)
        return default if v is None else self.expand(v)

    def _upgrade_legacy(self, data: dict) -> dict:
        """The pre-0.0.1 layout, translated: `default` -> default_model, [stackspot] + [limits] -> [backends.stackspot],
        [[agents]] (agent_id) -> [[models]] (target)."""
        if not (data.get("agents") or data.get("stackspot") or "default" in data):
            return data
        log.warning("%s uses the pre-0.0.1 layout ([stackspot], [[agents]], default); it still works, see config/midir.example.toml", self.config_file)
        out = {k: v for k, v in data.items() if k not in ("default", "stackspot", "limits", "agents")}
        if "default" in data:
            out.setdefault("default_model", data["default"])
        backend = {"type": "stackspot", **(data.get("stackspot") or {})}
        if data.get("limits"):
            backend["limits"] = data["limits"]
        out.setdefault("backends", {}).setdefault("stackspot", backend)
        if data.get("agents"):
            out["models"] = [{**{k: v for k, v in a.items() if k != "agent_id"}, "backend": "stackspot", "target": a.get("agent_id", "")} for a in data["agents"]]
        return out

    def _server(self, s: dict) -> ServerSettings:
        rdir = str(self._get(s, "responses_dir", "MIDIR_RESPONSES_DIR", "data/gateway/responses") or "").strip()
        return ServerSettings(
            port=int(self._get(s, "port", "MIDIR_PORT", 18880)),
            max_prompt_chars=int(self._get(s, "max_prompt_chars", "MIDIR_MAX_PROMPT_CHARS", 1_000_000)),
            tail_reminder=str(self._get(s, "tail_reminder", "MIDIR_TAIL_REMINDER", True)).lower() not in FALSE,
            tool_desc_max=int(self._get(s, "tool_desc_max", "MIDIR_TOOL_DESC_MAX", 0)),
            responses_dir=(Path(rdir) if Path(rdir).is_absolute() else self.root / rdir) if rdir else None,
            responses_retention_days=float(self._get(s, "responses_retention_days", "MIDIR_RESPONSES_RETENTION_DAYS", 30)),
            responses_max_mb=float(self._get(s, "responses_max_mb", "MIDIR_RESPONSES_MAX_MB", 500)),
            keepalive_s=float(self._get(s, "keepalive_s", "MIDIR_KEEPALIVE_S", 15)),
            retry_backoff_s=float(self._get(s, "retry_backoff_s", "MIDIR_RETRY_BACKOFF_S", 1)))

    def _backends(self, raw: dict) -> dict[str, BackendSettings]:
        """Every [backends.<name>] table; with none, one StackSpot backend configured from the environment alone."""
        raw = raw or {"stackspot": {"type": "stackspot"}}
        out: dict[str, BackendSettings] = {}
        for name, b in raw.items():
            if not isinstance(b, dict):
                raise ConfigError(f"{self.config_file}: [backends.{name}] must be a table")
            lim = b.get("limits") or {}
            limits = LimitSettings(  # MIDIR_* limits apply to every backend (one backend today)
                max_concurrent=int(self._get(lim, "max_concurrent", "MIDIR_MAX_CONCURRENT", 8)),
                requests_per_minute=int(self._get(lim, "requests_per_minute", "MIDIR_REQUESTS_PER_MINUTE", 90)),
                queue_timeout_s=float(self._get(lim, "queue_timeout_s", "MIDIR_QUEUE_TIMEOUT", 600)),
                cooldown_on_429_s=float(self._get(lim, "cooldown_on_429_s", "MIDIR_COOLDOWN_ON_429", 15)))
            options = {k: self.expand(v) for k, v in b.items() if k not in ("type", "limits")}
            out[name] = BackendSettings(name, str(b.get("type") or name), options, limits)
        return out

    def _models(self, data: dict) -> None:
        """[[models]] and default_model; the file is the single source of truth (environment shortcuts are ignored)."""
        names: set[str] = set()
        for i, m in enumerate(data["models"]):
            name = str(m.get("name", "")).strip().lower()
            where = f"{self.config_file}: models[{i}] ({name or '?'})"
            if not name or name in names:
                raise ConfigError(f"{where}: needs a unique 'name'")
            names.add(name)
            backend = str(m.get("backend") or (next(iter(self.backends)) if len(self.backends) == 1 else "")).strip()
            if backend not in self.backends:
                raise ConfigError(f"{where}: 'backend' must be one of {sorted(self.backends)} (got {backend!r})")
            target = str(self.expand(m.get("target", ""))).strip()
            if not target:
                raise ConfigError(f"{where}: 'target' is empty (unset environment variable?)")
            try:
                match = re.compile(m["match"], re.I) if m.get("match") else None
            except re.error as err:
                raise ConfigError(f"{where}: invalid 'match' regex: {err}")
            self.models.append(ModelSpec(name, backend, target, str(m.get("description", "")), [str(x).lower() for x in m.get("aliases", [])], match,
                                         m.get("max_prompt_chars"), m.get("tail_reminder"), m.get("tool_desc_max")))
        default_name = str(data.get("default_model", "")).strip().lower()
        if default_name:
            spec = next((x for x in self.models if x.name == default_name), None)
            if not spec:
                raise ConfigError(f"{self.config_file}: default_model = {default_name!r} is not one of the configured models")
            self.default = spec
        else:
            self.default = self.models[0]

    def _models_from_env(self) -> None:
        """Without [[models]]: STACKSPOT_<MODEL>_AGENT_ID defines model "<model>" (GPT_5_1 -> gpt-5.1) and
        STACKSPOT_DEFAULT_AGENT_ID the default, all on the StackSpot backend."""
        backend = "stackspot" if "stackspot" in self.backends else next(iter(self.backends))
        for k, v in self.env.items():
            m = re.fullmatch(r"STACKSPOT_(.+)_AGENT_ID", k)
            if m and m.group(1) != "DEFAULT" and v.strip():
                self.models.append(ModelSpec(self.model_name(m.group(1)), backend, v.strip()))
        default_id = str(self.env.get("STACKSPOT_DEFAULT_AGENT_ID") or "").strip()
        if default_id:
            self.default = ModelSpec(DEFAULT_MODEL_NAME, backend, default_id, "default agent (STACKSPOT_DEFAULT_AGENT_ID)")
        elif self.models:
            self.default = self.models[0]
        else:
            raise ConfigError(f"no models configured: create {self.config_file} (see config/midir.example.toml) or set STACKSPOT_DEFAULT_AGENT_ID")

    # ---- queries ----
    @staticmethod
    def model_name(env_part: str) -> str:
        """GPT_5_1 -> gpt-5.1, GPT_4_1_MINI -> gpt-4.1-mini, O3_MINI -> o3-mini (digit_digit becomes a dot)."""
        return re.sub(r"(?<=\d)_(?=\d)", ".", env_part.lower()).replace("_", "-")

    def resolve(self, model: str | None) -> ModelSpec:
        """The model spec for a requested model name. Order: exact name or alias (case-insensitive) > `match` regex in
        file order > exact after stripping a provider prefix ("openai/", "<backend>-", "midir-") > longest configured
        name contained in the requested one > default."""
        m = (model or "").strip().lower()
        for s in self.models:
            if m == s.name or m in s.aliases:
                return s
        for s in self.models:
            if s.match and s.match.search(m):
                return s
        prefixes = "|".join(re.escape(p + "-") for p in [*self.backends, "midir"])
        bare = re.sub(rf"^(?:[\w.-]+/)?(?:{prefixes})?", "", m)
        for s in self.models:
            if bare == s.name or bare in s.aliases:
                return s
        hits = sorted((s for s in self.models if s.name in m), key=lambda s: len(s.name), reverse=True)
        return hits[0] if hits else self.default

    @property
    def exposed_models(self) -> list[ModelSpec]:
        return self.models or [self.default]

    def knobs(self, spec: ModelSpec | None) -> tuple[int, bool, int]:
        """(max_prompt_chars, tail_reminder, tool_desc_max) for a model: its own values, else [server]'s."""
        s = self.server
        if spec is None:
            return s.max_prompt_chars, s.tail_reminder, s.tool_desc_max
        return (spec.max_prompt_chars or s.max_prompt_chars, s.tail_reminder if spec.tail_reminder is None else spec.tail_reminder,
                s.tool_desc_max if spec.tool_desc_max is None else spec.tool_desc_max)
