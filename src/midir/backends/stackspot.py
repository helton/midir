"""StackSpot AI backend: agents of the StackSpot AI platform through the Agent API (one text prompt in, SSE text out).

    [backends.stackspot]
    type = "stackspot"
    realm = "${STACKSPOT_REALM}"
    client_id = "${STACKSPOT_CLIENT_ID}"
    client_secret = "${STACKSPOT_CLIENT_SECRET}"
    # ca_bundle = "/etc/ssl/certs/corporate-ca.pem"    corporate TLS interception
    # idm_base_url, agent_base_url                       only for local mocks and tests

A model's `target` is the agent id. Environment variables win over the file: STACKSPOT_REALM, STACKSPOT_CLIENT_ID,
STACKSPOT_CLIENT_SECRET, STACKSPOT_CA_BUNDLE, STACKSPOT_IDM_BASE_URL, STACKSPOT_AGENT_BASE_URL. The measured contract
(token lifetime, SSE events, limits, error codes) is in docs/backends/stackspot.md.
"""
from __future__ import annotations

import asyncio
import json
import logging
import re
import time
from typing import Any, AsyncIterator

import httpx
from tenacity import retry

from midir.backends.base import TIMEOUT, Completion, TextBackend
from midir.config import BackendSettings, ConfigError
from midir.errors import BackendError
from midir.telemetry import Telemetry

log = logging.getLogger(__name__)

DEFAULT_AGENT_BASE = "https://genai-inference-app.stackspot.com/v1/agent"
DEFAULT_IDM_BASE = "https://idm.stackspot.com"
TOO_LONG_RE = re.compile(r"limit of (\d+) tokens.*?resulted in (\d+) tokens", re.S)  # INFERENCE_6001


def body_of(r: httpx.Response) -> Any:
    try:
        return r.json()
    except Exception:
        return r.text


def usage_from(tokens_field: Any) -> dict | None:
    """The final event's `tokens` ({"input", "output"}) as usage; None when absent or zero."""
    t = tokens_field if isinstance(tokens_field, dict) else {}

    def num(v: Any) -> int:
        try:
            return max(0, int(v or 0))
        except (TypeError, ValueError):
            return 0

    prompt_tokens, completion_tokens = num(t.get("input")), num(t.get("output"))
    if not prompt_tokens and not completion_tokens:
        return None
    return {"prompt_tokens": prompt_tokens, "completion_tokens": completion_tokens, "total_tokens": prompt_tokens + completion_tokens}


class StackSpotBackend(TextBackend):
    """Client-credentials token (cached, renewed 60 s before expiry; a 401 forces one renewal), the Agent API SSE stream
    on one pooled keep-alive connection, retries and the account's queue."""

    type = "stackspot"

    def __init__(self, settings: BackendSettings, env: dict[str, str] | None = None, telemetry: Telemetry | None = None, backoff_s: float = 1.0) -> None:
        super().__init__(settings, env, telemetry, backoff_s)
        o, e = settings.options, self.env

        def get(key: str, env_name: str, default: Any) -> Any:
            v = e.get(env_name) if e.get(env_name) not in (None, "") else o.get(key)
            return default if v in (None, "") else v

        self.realm = str(get("realm", "STACKSPOT_REALM", "")).strip()
        self.client_id = str(get("client_id", "STACKSPOT_CLIENT_ID", "")).strip()
        self.client_secret = str(get("client_secret", "STACKSPOT_CLIENT_SECRET", "")).strip()
        self.ca_bundle = get("ca_bundle", "STACKSPOT_CA_BUNDLE", None)
        self.idm_base = str(get("idm_base_url", "STACKSPOT_IDM_BASE_URL", DEFAULT_IDM_BASE)).rstrip("/")
        self.agent_base = str(get("agent_base_url", "STACKSPOT_AGENT_BASE_URL", DEFAULT_AGENT_BASE)).rstrip("/")
        self._http: httpx.AsyncClient | None = None
        self._token = ""
        self._expires_at = 0.0
        self._lock = asyncio.Lock()

    # ---- settings ----
    def validate(self) -> None:
        missing = [n for n, v in (("realm (STACKSPOT_REALM)", self.realm), ("client_id (STACKSPOT_CLIENT_ID)", self.client_id),
                                  ("client_secret (STACKSPOT_CLIENT_SECRET)", self.client_secret)) if not v]
        if missing:
            raise ConfigError(f"backend {self.name!r} (stackspot): missing {', '.join(missing)}")

    @property
    def idm_url(self) -> str:
        return f"{self.idm_base}/{self.realm}/oidc/oauth/token"

    def agent_url(self, agent_id: str) -> str:
        return f"{self.agent_base}/{agent_id}/chat"

    def describe_target(self, target: str) -> str:
        return target[:6] + "..."

    # ---- connection and token ----
    @property
    def http(self) -> httpx.AsyncClient:
        """One pooled client for the process: keep-alive saves a TCP+TLS handshake per call (~25 ms direct, more behind
        corporate proxies). Created lazily inside the running event loop."""
        if self._http is None or self._http.is_closed:
            n = max(4, self.limiter.max_concurrent + 2)
            self._http = httpx.AsyncClient(timeout=TIMEOUT, verify=self.ca_bundle or True,
                                           limits=httpx.Limits(max_connections=n, max_keepalive_connections=n, keepalive_expiry=60))
        return self._http

    def _token_valid(self) -> bool:
        return bool(self._token) and self._expires_at - 60 > time.time()

    async def token(self, force: bool = False) -> str:
        """Client-credentials token, cached and renewed 60 s before expiry (tokens last 20 min)."""
        if not force and self._token_valid():
            return self._token
        async with self._lock:
            if not force and self._token_valid():
                return self._token
            form = {"grant_type": "client_credentials", "client_id": self.client_id, "client_secret": self.client_secret}
            r = await self.http.post(self.idm_url, data=form)
            if r.status_code != 200:
                raise BackendError(r.status_code, body_of(r), dict(r.headers), where="idm", backend=self.name)
            j = r.json()
            self._token = j["access_token"]
            self._expires_at = time.time() + int(j.get("expires_in", 300))
            log.info("%s token renewed, expires in %ss", self.name, j.get("expires_in"))
            return self._token

    async def ready(self) -> None:
        await self.token()

    @property
    def token_expires_in(self) -> int:
        return max(0, int(self._expires_at - time.time()))

    async def aclose(self) -> None:
        if self._http is not None:
            await self._http.aclose()

    # ---- chat ----
    @staticmethod
    def body(prompt: str, streaming: bool) -> dict:
        # stackspot_knowledge=true silently attaches a cross-account knowledge source (docs/backends/stackspot.md): always false.
        return {"streaming": streaming, "user_prompt": prompt, "stackspot_knowledge": False, "return_ks_in_response": False}

    def input_limit_exceeded(self, error: BackendError) -> tuple[int, int] | None:
        """StackSpot's "Input tokens exceed the configured limit of 272000 tokens. Your messages resulted in N tokens"
        (400 INFERENCE_6001): code is denser than 4 chars/token, so a prompt under the char cap can still exceed it."""
        if error.status != 400:
            return None
        m = TOO_LONG_RE.search(error.body if isinstance(error.body, str) else json.dumps(error.body))
        return (int(m.group(1)), int(m.group(2))) if m else None

    async def stream(self, prompt: str, target: str, meta: dict | None = None) -> AsyncIterator[str | Completion]:
        """Text deltas, then one Completion. Waits in the queue first; retries only until the response headers arrive."""
        limiter = self.limiter
        t_queue = time.monotonic()
        deadline = t_queue + limiter.timeout

        @retry(**self.retry)
        async def _open(c: httpx.AsyncClient) -> httpx.Response:
            for attempt in (1, 2):
                headers = {"Authorization": f"Bearer {await self.token(force=attempt == 2)}", "Content-Type": "application/json", "Accept": "text/event-stream"}
                await limiter.start(deadline)
                r = await c.send(c.build_request("POST", self.agent_url(target), json=self.body(prompt, True), headers=headers), stream=True)
                if r.status_code == 401 and attempt == 1:
                    await r.aclose()
                    log.warning("%s: 401 from the agent; renewing the token", self.name)
                    continue
                break
            if r.status_code != 200:
                if r.status_code == 429:
                    limiter.on_429()
                raw: Any = (await r.aread()).decode("utf-8", "replace")
                await r.aclose()
                try:
                    raw = json.loads(raw)
                except Exception:
                    pass
                raise BackendError(r.status_code, raw, dict(r.headers), backend=self.name)
            return r

        await limiter.acquire_slot(deadline)
        try:
            queued = time.monotonic() - t_queue
            if meta is not None:
                meta["queue_wait_ms"] = meta.get("queue_wait_ms", 0) + queued * 1000
            self.telemetry.queue_wait(queued)
            if queued >= 1:
                log.info("queued %.1fs for a %s slot (%s)", queued, self.name, limiter.state())
            r = await _open(self.http)
            async for item in self._read(r):
                yield item
        finally:
            limiter.release_slot()

    @staticmethod
    async def _read(r: httpx.Response) -> AsyncIterator[str | Completion]:
        """SSE events: deltas carry only `message`; the final one carries stop_reason, message_id and tokens. Events
        that are not JSON objects, or whose `message` is not text, are skipped with a warning."""
        try:
            done = False
            async for line in r.aiter_lines():
                line = line.rstrip("\r")
                if not line.startswith("data:"):
                    continue
                payload = line[5:].strip()
                if not payload:
                    continue
                try:
                    ev = json.loads(payload)
                except json.JSONDecodeError:
                    log.warning("ignoring non-JSON SSE event: %r", payload[:200])
                    continue
                if not isinstance(ev, dict):
                    log.warning("ignoring SSE event that is not an object: %r", payload[:200])
                    continue
                if "stop_reason" in ev or "tokens" in ev:
                    done = True
                    yield Completion(usage_from(ev.get("tokens")), ev.get("message_id"), ev.get("stop_reason"))
                elif isinstance(ev.get("message"), str):
                    if ev["message"]:
                        yield ev["message"]
                elif ev.get("message") is not None:
                    log.warning("ignoring SSE event with a non-text message: %r", payload[:200])
            if not done:
                log.warning("stream ended without a final event")
                yield Completion(None, None, "stop")
        finally:
            await r.aclose()
