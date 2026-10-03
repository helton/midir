"""Telemetry: one OpenTelemetry span and a few metrics per request, exported over OTLP/HTTP in background threads.
Off (and free) unless an OTLP endpoint is configured. Prompt content is never exported. Also the request labels
(client, session, initiator) that the span, the metrics and the log line share."""
from __future__ import annotations

import hashlib
import json
import logging
import os
import re
import time
from typing import Any, AsyncIterator, Callable

from midir.build import BUILD

VERSION = BUILD.full_version
from midir.canonical import CanonicalRequest, CanonicalResponse, Event
from midir.errors import BackendError

log = logging.getLogger(__name__)

SYSTEM_MARKERS = (("Hermes Agent", "hermes"), ("OpenClaw", "openclaw"), ("DeepSeek Harness", "deepseek-harness"))  # opening of their system prompts


def client_of(headers: Any, system: str = "") -> tuple[str, str]:
    """(client name, version) from the User-Agent and a few client headers; clients that only send their SDK's
    User-Agent (Hermes: OpenAI/Python, OpenClaw: OpenAI/JS) are recognized by the opening of their system prompt.
    Labels only: nothing here changes behavior."""
    ua = (headers.get("user-agent") or "").strip()
    low = ua.lower()
    m = re.search(r"/(\d[\w.\-]*)", ua)
    version = m.group(1) if m else ""
    if "claude-cli" in low or "claude-code" in low:
        return ("claude-code-vscode" if "claude-vscode" in low else "claude-code"), version
    if "codex" in low:
        return "codex", version
    if "opencode" in low:
        return "opencode", version
    if "aider" in low or "litellm" in low:
        return "aider", version
    if "copilot" in low or (low.startswith("openai/") and ("x-initiator" in headers or "x-interaction-type" in headers)):
        return "copilot", version
    head = system[:600]
    for marker, name in SYSTEM_MARKERS:
        if marker in head:
            return name, version
    if "python-httpx" in low or "openai-python" in low or "anthropic-python" in low:
        return "sdk-python", version
    return (ua.split("/")[0][:30].lower() or "unknown"), version


_SESSION_HEADERS = ("x-claude-code-session-id", "x-session-id", "session_id", "x-conversation-id", "conversation_id", "vscode-sessionid", "x-copilot-session-id")


def request_meta(headers: Any, body: dict, protocol: str, model: str, route: Any, req: CanonicalRequest, rid: str, previous_session: Callable[[str], str | None] | None = None) -> dict:
    """Labels for one request: client, session, initiator, model, agent. The session comes from a header when the client
    sends one (Claude Code), from `prompt_cache_key`/`previous_response_id` (Responses), from `metadata.user_id` (Messages),
    or, failing all that, from a hash of the first user message (Chat Completions): conversations of one client share it."""
    client, version = client_of(headers, req.system[0] if req.system else "")
    session = next((headers.get(h) for h in _SESSION_HEADERS if headers.get(h)), None)
    if not session and protocol == "messages":
        try:
            session = json.loads((body.get("metadata") or {}).get("user_id") or "{}").get("session_id")
        except Exception:
            session = None
    if not session and protocol == "responses":
        session = body.get("prompt_cache_key") or body.get("user")
        if not session and body.get("previous_response_id") and previous_session:
            session = previous_session(body["previous_response_id"])
        session = session or f"chain-{rid[5:15]}"
    if not session:
        first_user = next((t.text for t in req.turns if t.role == "user" and t.text), "")
        session = ("conv-" + hashlib.sha1(first_user[:500].encode()).hexdigest()[:10]) if first_user else rid
    initiator = headers.get("x-initiator") or ("agent" if req.turns and req.turns[-1].tool_results else "user")
    return {"client": client, "client_version": version, "session": str(session)[:64], "initiator": initiator, "protocol": protocol,
            "model": model, "agent": route.name if route else "default", "backend": route.backend if route else "", "stream": bool(body.get("stream")), "tools_declared": len(req.tools), "json_mode": req.json_schema is not None}


class Telemetry:
    """OTLP/HTTP exporter for spans and metrics, batched in background threads: the request path only sets attributes.
    Metric labels are low-cardinality except `session.id`, which is fine for one user and documented."""

    def __init__(self, endpoint: str = "", service_name: str = "midir") -> None:
        self.enabled = bool(endpoint)
        if not self.enabled:
            return
        os.environ.setdefault("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint)  # the OTLP exporters read it
        try:
            from opentelemetry.exporter.otlp.proto.http.metric_exporter import OTLPMetricExporter
            from opentelemetry.exporter.otlp.proto.http.trace_exporter import OTLPSpanExporter
            from opentelemetry.sdk.metrics import MeterProvider
            from opentelemetry.sdk.metrics.export import PeriodicExportingMetricReader
            from opentelemetry.sdk.resources import Resource
            from opentelemetry.sdk.trace import TracerProvider
            from opentelemetry.sdk.trace.export import BatchSpanProcessor
        except ImportError as e:  # the inline dependencies include them; this only happens with a hand-made environment
            log.warning("telemetry disabled: %r", e)
            self.enabled = False
            return
        resource = Resource.create({"service.name": service_name, "service.version": VERSION})
        self._tp = TracerProvider(resource=resource)
        self._tp.add_span_processor(BatchSpanProcessor(OTLPSpanExporter()))
        self._mp = MeterProvider(resource=resource, metric_readers=[PeriodicExportingMetricReader(OTLPMetricExporter(), export_interval_millis=5000)])
        self.tracer = self._tp.get_tracer("midir")
        meter = self._mp.get_meter("midir")
        self.m_requests = meter.create_counter("midir.requests", unit="1", description="LLM requests handled")
        self.m_tokens = meter.create_counter("gen_ai.client.token.usage", unit="{token}", description="Input and output tokens reported by the backend (estimated when it reports none)")
        self.m_duration = meter.create_histogram("midir.request.duration", unit="ms", description="Request duration, first byte in to last byte out")
        self.m_ttfb = meter.create_histogram("midir.request.ttfb", unit="ms", description="Time to the first text or tool-call event")
        self.m_tool_calls = meter.create_counter("midir.tool_calls", unit="1", description="Tool calls emitted to the client")
        self.m_followups = meter.create_counter("midir.followups", unit="1", description="Automatic follow-up calls (promise, incapacity, tool_choice retry)")
        self.m_retries = meter.create_counter("midir.upstream_retries", unit="1", description="Retried backend attempts, by status")
        self.m_queue_wait = meter.create_histogram("midir.queue.wait", unit="ms", description="Time waiting in the backend queue (concurrency and requests/minute)")
        self.m_queue_depth = meter.create_up_down_counter("midir.queue.waiting", unit="1", description="Requests currently waiting for a backend slot")
        self.m_errors = meter.create_counter("midir.errors", unit="1", description="Requests that ended in an error")
        self.m_truncations = meter.create_counter("midir.truncations", unit="1", description="Upstream calls whose prompt lost old turns to the size cap")
        self.m_dropped_turns = meter.create_counter("midir.dropped_turns", unit="{turn}", description="Old conversation turns dropped to fit the size cap")
        import atexit
        atexit.register(self.shutdown)
        log.info("telemetry on: OTLP/HTTP -> %s (service %s)", endpoint, service_name)

    def shutdown(self) -> None:
        """Flush and stop the exporters; safe to call twice (the app's shutdown, then atexit)."""
        if self.enabled and not getattr(self, "_closed", False):
            self._closed = True
            self._tp.shutdown()
            self._mp.shutdown()

    def queue_wait(self, seconds: float) -> None:
        if self.enabled:
            self.m_queue_wait.record(seconds * 1000)

    def queue_depth(self, delta: int) -> None:
        if self.enabled:
            self.m_queue_depth.add(delta)

    def truncated(self, meta: dict, turns: int) -> None:
        if self.enabled:
            self.m_truncations.add(1, self._labels(meta))
            self.m_dropped_turns.add(turns, self._labels(meta))

    def upstream_retry(self, status: Any) -> None:
        if self.enabled:
            self.m_retries.add(1, {"midir.upstream_status": str(status)})

    @staticmethod
    def _labels(meta: dict) -> dict:
        return {"client.name": meta["client"], "gen_ai.request.model": meta["model"], "gen_ai.response.model": meta["agent"], "midir.protocol": meta["protocol"], "midir.initiator": meta["initiator"], "session.id": meta["session"]}

    def begin(self, meta: dict, rid: str) -> Any:
        if not self.enabled:
            return None
        span = self.tracer.start_span(f"gen_ai.chat {meta['protocol']}")
        span.set_attributes({"gen_ai.system": meta.get("backend") or "unknown", "gen_ai.operation.name": "chat", "gen_ai.request.model": meta["model"], "gen_ai.response.model": meta["agent"], "client.name": meta["client"], "client.version": meta["client_version"], "session.id": meta["session"],
                             "midir.request_id": rid, "midir.protocol": meta["protocol"], "midir.initiator": meta["initiator"], "midir.stream": meta["stream"], "midir.tools_declared": meta["tools_declared"], "midir.json_mode": meta["json_mode"]})
        return span

    def end(self, span: Any, meta: dict, resp: CanonicalResponse | None, t0: float, ttfb: float | None, error: str | None = None) -> None:
        if not self.enabled:
            return
        from opentelemetry.trace import Status, StatusCode
        duration_ms = (time.perf_counter() - t0) * 1000
        labels = self._labels(meta)
        usage = resp.usage if resp else {}
        attrs = {"gen_ai.usage.input_tokens": usage.get("prompt_tokens", 0), "gen_ai.usage.output_tokens": usage.get("completion_tokens", 0), "midir.tool_calls": len(resp.tool_calls) if resp else 0,
                 "midir.finish": resp.finish if resp else "error", "midir.followups": meta.get("followups", 0), "midir.parse_errors": meta.get("parse_errors", 0), "midir.repairs": meta.get("repairs", 0),
                 "midir.upstream_calls": meta.get("upstream_calls", 0), "midir.queue_wait_ms": round(meta.get("queue_wait_ms", 0), 1), "midir.prompt_chars": meta.get("prompt_chars", 0), "midir.dropped_turns": meta.get("dropped_turns", 0), "midir.duration_ms": round(duration_ms, 1)}
        if ttfb is not None:
            attrs["midir.ttfb_ms"] = round(ttfb * 1000, 1)
        if error:
            attrs["error.type"] = error
            span.set_status(Status(StatusCode.ERROR, error))
            self.m_errors.add(1, {**labels, "error.type": error})
        span.set_attributes(attrs)
        span.end()
        self.m_requests.add(1, labels)
        self.m_duration.record(duration_ms, labels)
        if ttfb is not None:
            self.m_ttfb.record(ttfb * 1000, labels)
        if resp:
            self.m_tokens.add(usage.get("prompt_tokens", 0), {**labels, "gen_ai.token.type": "input"})
            self.m_tokens.add(usage.get("completion_tokens", 0), {**labels, "gen_ai.token.type": "output"})
            if resp.tool_calls:
                self.m_tool_calls.add(len(resp.tool_calls), labels)
        if meta.get("followups"):
            self.m_followups.add(meta["followups"], labels)

    async def observe(self, events: AsyncIterator[Event], req: CanonicalRequest, rid: str) -> AsyncIterator[Event]:
        """Pass-through for a streamed request: records TTFB, usage and outcome when the stream ends or breaks."""
        if not self.enabled:
            async for ev in events:
                yield ev
            return
        span, t0, ttfb, resp, error = self.begin(req.meta, rid), time.perf_counter(), None, None, None
        try:
            async for ev in events:
                if ttfb is None and ev.kind in ("text", "tool_call"):
                    ttfb = time.perf_counter() - t0
                if ev.kind == "done":
                    resp = ev.response
                yield ev
        except BaseException as e:  # backend error, client disconnect (CancelledError/GeneratorExit)
            error = type(e).__name__ if not isinstance(e, BackendError) else f"upstream_{e.status}"
            raise
        finally:
            if resp is None and error is None:
                error = "client_disconnect"
            self.end(span, req.meta, resp, t0, ttfb, error)

    async def observe_complete(self, coro: Any, req: CanonicalRequest, rid: str) -> CanonicalResponse:
        """Same for a non-streaming request (no TTFB: the client gets everything at once)."""
        if not self.enabled:
            return await coro
        span, t0 = self.begin(req.meta, rid), time.perf_counter()
        try:
            resp = await coro
        except BaseException as e:
            self.end(span, req.meta, None, t0, None, type(e).__name__ if not isinstance(e, BackendError) else f"upstream_{e.status}")
            raise
        self.end(span, req.meta, resp, t0, None)
        return resp


