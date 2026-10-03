"""Black-box mode of the regression suite: the implementation under test runs as a separate process (Python, Go or
Rust) and is reached only over HTTP; the StackSpot stand-in is a real HTTP server in this process with the same
scripting interface as the in-process mock (Reply, add, prompts, calls, token_status).

    MIDIR_IMPL=python uv run pytest -q              # the reference implementation, as a process
    MIDIR_IMPL=go     uv run pytest -q              # .internal/ports/go/bin/midir
    MIDIR_IMPL=rust   uv run pytest -q              # .internal/ports/rust/target/release/midir
    MIDIR_IMPL_CMD="/path/to/midir" MIDIR_IMPL=x    # any other build

Tests that look inside the Python process (unit tests, captured logs, store internals) carry the `inprocess` marker
and are skipped in this mode.
"""
from __future__ import annotations

import json
import os
import shlex
import socket
import subprocess
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import httpx

ROOT = Path(__file__).resolve().parents[1]
IMPLS = {
    "python": [sys.executable, "-m", "midir"],
    "go": [str(ROOT / ".internal" / "ports" / "go" / "bin" / "midir")],
    "rust": [str(ROOT / ".internal" / "ports" / "rust" / "target" / "release" / "midir")],
}


def impl_command(name: str) -> list[str]:
    custom = os.environ.get("MIDIR_IMPL_CMD")
    return shlex.split(custom) if custom else IMPLS[name]


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class HttpUpstream:
    """The StackSpot stand-in over real HTTP: idm token endpoint and Agent API chat endpoint, scripted like the
    in-process mock. `Reply` comes from conftest (same dataclass)."""

    def __init__(self, reply_cls) -> None:
        from collections import deque
        self.Reply = reply_cls
        self.script = deque()
        self.default = reply_cls("ok")
        self.calls: list[dict] = []
        self.token_calls = 0
        self.token_status = 200
        self.expires_in = 1200
        upstream = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *a) -> None:
                pass

            def _send(self, status: int, body: bytes, ctype: str = "application/json", headers: dict | None = None) -> None:
                self.send_response(status)
                self.send_header("Content-Type", ctype)
                self.send_header("Content-Length", str(len(body)))
                for k, v in (headers or {}).items():
                    self.send_header(k, v)
                self.end_headers()
                self.wfile.write(body)

            def do_POST(self) -> None:
                raw = self.rfile.read(int(self.headers.get("Content-Length") or 0))
                if self.path.endswith("/oidc/oauth/token"):
                    upstream.token_calls += 1
                    if upstream.token_status != 200:
                        return self._send(upstream.token_status, json.dumps({"error": "invalid_client"}).encode())
                    return self._send(200, json.dumps({"access_token": f"tok{upstream.token_calls}", "expires_in": upstream.expires_in}).encode())
                body = json.loads(raw)
                agent = self.path.rstrip("/").split("/")[-2]
                upstream.calls.append({"agent": agent, "prompt": body["user_prompt"], "auth": self.headers.get("authorization"), "body": body})
                r = upstream.script.popleft() if upstream.script else upstream.default
                if r.delay:
                    time.sleep(r.delay)
                if r.status != 200:
                    data = r.body.encode() if isinstance(r.body, str) else json.dumps(r.body).encode()
                    return self._send(r.status, data, "text/plain" if isinstance(r.body, str) else "application/json", r.headers)
                if r.raw is not None:
                    return self._send(200, r.raw.encode(), "text/event-stream")
                events = r.events
                if events is None:
                    events = [{"message": r.text[i:i + r.chunk]} for i in range(0, len(r.text), r.chunk)]
                    events.append(r.final if r.final is not None else {"stop_reason": "stop", "message_id": "up-msg-1",
                                                                      "tokens": {"input": len(body["user_prompt"]) // 4, "output": max(1, len(r.text) // 4)}})
                parts = [f"data: {json.dumps(e) if not isinstance(e, str) else e}\r\n\r\n".encode() for e in events]
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Connection", "close")
                self.end_headers()
                self.close_connection = True
                if r.break_after is not None:
                    for p in parts[: r.break_after]:
                        self.wfile.write(p)
                        self.wfile.flush()
                    self.connection.shutdown(socket.SHUT_RDWR)  # dropped mid-stream
                    return
                for p in parts:
                    self.wfile.write(p)
                    self.wfile.flush()

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def add(self, *replies):
        for r in replies:
            self.script.append(self.Reply(r) if isinstance(r, str) else r)
        return self

    @property
    def prompts(self) -> list[str]:
        return [c["prompt"] for c in self.calls]

    def close(self) -> None:
        self.server.shutdown()
        self.server.server_close()


class ServerProcess:
    """One implementation under test, started in its own working directory with config/midir.toml pointing at the
    stand-in. `url` is its base URL; `log` its stderr; `restart()` keeps the configuration and data."""

    def __init__(self, impl: str, toml: str, upstream: HttpUpstream, workdir: Path, env: dict | None = None) -> None:
        self.impl, self.upstream, self.workdir = impl, upstream, workdir
        (workdir / "config").mkdir(parents=True, exist_ok=True)
        text = (toml.replace("{responses_dir}", str(workdir / "responses")).replace("http://idm.mock", upstream.url)
                .replace("http://agent.mock/v1/agent", upstream.url + "/v1/agent"))
        (workdir / "config" / "midir.toml").write_text(text)
        self.extra_env = env or {}
        self.port = free_port()
        self.url = f"http://127.0.0.1:{self.port}"
        self.log = workdir / "server.log"
        self.proc: subprocess.Popen | None = None
        self.start()

    def start(self) -> None:
        env = {k: v for k, v in os.environ.items() if not k.startswith(("STACKSPOT_", "MIDIR_", "OTEL_"))}
        env.update({"MIDIR_PORT": str(self.port), "MIDIR_NO_BANNER": "1", "MIDIR_RETRY_BACKOFF_S": "0.001", "PYTHONUNBUFFERED": "1"}, **self.extra_env)
        self.proc = subprocess.Popen(impl_command(self.impl) + ["--host", "127.0.0.1", "--port", str(self.port)], cwd=self.workdir, env=env,
                                     stdout=open(self.log, "ab"), stderr=subprocess.STDOUT)
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            if self.proc.poll() is not None:
                raise RuntimeError(f"{self.impl} exited with {self.proc.returncode}:\n{self.log.read_text()[-3000:]}")
            try:
                if httpx.get(self.url + "/health", timeout=0.5).status_code == 200:
                    return
            except httpx.HTTPError:
                time.sleep(0.05)
        raise RuntimeError(f"{self.impl} did not answer /health in 20 s:\n{self.log.read_text()[-3000:]}")

    def stop(self) -> None:
        if self.proc and self.proc.poll() is None:
            self.proc.terminate()
            try:
                self.proc.wait(5)
            except subprocess.TimeoutExpired:
                self.proc.kill()
                self.proc.wait()

    def restart(self) -> None:
        self.stop()
        self.start()

    def logs(self) -> str:
        return self.log.read_text(errors="replace")
