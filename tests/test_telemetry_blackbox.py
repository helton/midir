"""Telemetry, black-box: with OTEL_EXPORTER_OTLP_ENDPOINT set, an implementation exports one span per request and the
midir.* metrics over OTLP/HTTP (protobuf), and flushes them when it is stopped (SIGTERM).

    MIDIR_IMPL=<python|go|rust> uv run pytest -q tests/test_telemetry_blackbox.py
"""
from __future__ import annotations

import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import httpx
import pytest

from conftest import IMPL, MIDIR_TOML, Reply

pytestmark = [pytest.mark.blackbox, pytest.mark.skipif(not IMPL, reason="black-box mode only (MIDIR_IMPL)")]


class Collector:
    """Records OTLP/HTTP protobuf exports by path (/v1/traces, /v1/metrics)."""

    def __init__(self) -> None:
        self.bodies: dict[str, list[bytes]] = {}
        coll = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *a) -> None:
                pass

            def do_POST(self) -> None:
                data = self.rfile.read(int(self.headers.get("Content-Length") or 0))
                coll.bodies.setdefault(self.path, []).append(data)
                self.send_response(200)
                self.send_header("Content-Type", "application/x-protobuf")
                self.send_header("Content-Length", "0")
                self.end_headers()

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def spans(self) -> tuple[dict, list]:
        from opentelemetry.proto.collector.trace.v1.trace_service_pb2 import ExportTraceServiceRequest
        resource, spans = {}, []
        for body in self.bodies.get("/v1/traces", []):
            req = ExportTraceServiceRequest()
            req.ParseFromString(body)
            for rs in req.resource_spans:
                resource.update({a.key: _value(a.value) for a in rs.resource.attributes})
                for ss in rs.scope_spans:
                    spans += [{"name": s.name, **{a.key: _value(a.value) for a in s.attributes}} for s in ss.spans]
        return resource, spans

    def metric_names(self) -> set[str]:
        from opentelemetry.proto.collector.metrics.v1.metrics_service_pb2 import ExportMetricsServiceRequest
        names = set()
        for body in self.bodies.get("/v1/metrics", []):
            req = ExportMetricsServiceRequest()
            req.ParseFromString(body)
            for rm in req.resource_metrics:
                for sm in rm.scope_metrics:
                    names |= {m.name for m in sm.metrics}
        return names

    def close(self) -> None:
        self.server.shutdown()


def _value(v):
    kind = v.WhichOneof("value")
    return getattr(v, kind) if kind else None


def test_one_span_per_request_and_the_metrics(tmp_path):
    from blackbox import HttpUpstream, ServerProcess
    collector, upstream = Collector(), HttpUpstream(Reply)
    upstream.add("Hello.")
    server = ServerProcess(IMPL, MIDIR_TOML, upstream, tmp_path / "server", env={"OTEL_EXPORTER_OTLP_ENDPOINT": collector.url})
    try:
        r = httpx.post(server.url + "/v1/chat/completions", json={"model": "claude-haiku-4-5", "messages": [{"role": "user", "content": "hi"}]},
                       headers={"user-agent": "claude-cli/2.1.283 (external, cli)"}, timeout=60)
        assert r.status_code == 200
        rid = r.json()["id"]
    finally:
        server.stop()  # SIGTERM: everything pending must be flushed
        upstream.close()
    resource, spans = collector.spans()
    collector.close()
    assert resource.get("service.name") == "midir" and resource.get("service.version")
    mine = [s for s in spans if s.get("midir.request_id") == rid]
    assert len(mine) == 1, spans
    s = mine[0]
    assert s["name"] == "gen_ai.chat chat"
    assert s["gen_ai.system"] == "stackspot" and s["gen_ai.request.model"] == "claude-haiku-4-5" and s["gen_ai.response.model"] == "gpt-4.1"
    assert s["client.name"] == "claude-code" and s["midir.protocol"] == "chat" and s["midir.finish"] == "stop"
    assert s["gen_ai.usage.input_tokens"] > 0 and s["gen_ai.usage.output_tokens"] > 0 and s["midir.upstream_calls"] == 1
    assert {"midir.requests", "gen_ai.client.token.usage", "midir.request.duration"} <= collector.metric_names()
