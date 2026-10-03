"""Shared fixtures for the regression suite: the real Midir app, a scripted StackSpot stand-in (httpx.MockTransport,
nothing leaves the machine) and the official OpenAI/Anthropic SDKs pointed at the app.

    uv run poe test                        # in-process (the default)
    MIDIR_IMPL=go uv run pytest -q         # black-box: the same tests against a running implementation (tests/blackbox.py)
"""
from __future__ import annotations

import asyncio
import json
import os
import time
from collections import deque
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

import httpx
import pytest

from midir.app import build_app
from midir.config import Config
from midir.gateway import Gateway

ROOT = Path(__file__).resolve().parents[1]
IMPL = os.environ.get("MIDIR_IMPL", "").strip()  # set: black-box mode against that implementation


def pytest_configure(config):
    config.addinivalue_line("markers", "inprocess: looks inside the Python process (skipped in black-box mode)")
    config.addinivalue_line("markers", "blackbox: runs only in black-box mode, with its own processes")


HTTP_FIXTURES = {"app_client", "openai_client", "anthropic_client"}
INTERNAL_FIXTURES = {"caplog", "monkeypatch", "stackspot"}  # fixtures that only exist inside the Python process


def pytest_collection_modifyitems(config, items):
    """In black-box mode only tests that talk HTTP and do not look inside the process run: the `inprocess` marker, or no
    HTTP client fixture, or an internals fixture (captured logs, patched objects, config objects)."""
    if not IMPL:
        return
    skip = pytest.mark.skip(reason=f"in-process test; black-box mode ({IMPL})")
    for item in items:
        names = set(getattr(item, "fixturenames", ()))
        if "blackbox" in item.keywords:
            continue
        if "inprocess" in item.keywords or not names & HTTP_FIXTURES or names & INTERNAL_FIXTURES:
            item.add_marker(skip)

MIDIR_TOML = """
default_model = "gpt-5.1"

[server]
responses_dir = "{responses_dir}"

[backends.stackspot]
type = "stackspot"
realm = "acme"
client_id = "cid"
client_secret = "secret"
idm_base_url = "http://idm.mock"
agent_base_url = "http://agent.mock/v1/agent"

[backends.stackspot.limits]
max_concurrent = 4
requests_per_minute = 0
cooldown_on_429_s = 0

[[models]]
name = "gpt-5.1"
backend = "stackspot"
target = "AGENT51"
aliases = ["claude-opus-4-5"]

[[models]]
name = "gpt-4.1"
target = "AGENT41"
aliases = ["claude-haiku-4-5"]

[[models]]
name = "flex"
target = "AGENTFLEX"
match = "sonnet"
"""


# ---------------------------------------------------------------------------------------------------------------------
# scripted upstream
# ---------------------------------------------------------------------------------------------------------------------

@dataclass
class Reply:
    """One upstream answer. `text` is streamed in `chunk`-sized `message` deltas, then the final event.
    `status` != 200 answers an HTTP error with `body`; `events` replaces the generated SSE events entirely;
    `raw` replaces the whole SSE body; `break_after` drops the connection after that many events."""

    text: str = ""
    chunk: int = 7
    final: dict | None = None
    status: int = 200
    body: Any = None
    headers: dict = field(default_factory=dict)
    events: list | None = None
    raw: str | None = None
    break_after: int | None = None
    delay: float = 0.0  # seconds before the response (model latency)


class _Broken(httpx.AsyncByteStream):
    def __init__(self, parts: list[bytes]) -> None:
        self.parts = parts

    async def __aiter__(self):
        for p in self.parts:
            yield p
        raise httpx.ReadError("connection dropped by the mock")

    async def aclose(self) -> None:
        pass


class Upstream:
    """Stand-in for idm (token) + Agent API. Replies are consumed in order; `default` answers when the script is empty."""

    def __init__(self) -> None:
        self.script: deque[Reply] = deque()
        self.default = Reply("ok")
        self.calls: list[dict] = []  # {"agent": id, "prompt": str, "auth": header}
        self.token_calls = 0
        self.token_status = 200
        self.expires_in = 1200

    def add(self, *replies: Reply | str) -> "Upstream":
        for r in replies:
            self.script.append(Reply(r) if isinstance(r, str) else r)
        return self

    @property
    def prompts(self) -> list[str]:
        return [c["prompt"] for c in self.calls]

    async def handler(self, request: httpx.Request) -> httpx.Response:
        if request.url.path.endswith("/oidc/oauth/token"):
            self.token_calls += 1
            if self.token_status != 200:
                return httpx.Response(self.token_status, json={"error": "invalid_client"})
            return httpx.Response(200, json={"access_token": f"tok{self.token_calls}", "expires_in": self.expires_in})
        body = json.loads(request.content)
        agent = request.url.path.split("/")[-2]
        self.calls.append({"agent": agent, "prompt": body["user_prompt"], "auth": request.headers.get("authorization"), "body": body})
        r = self.script.popleft() if self.script else self.default
        if r.delay:
            await asyncio.sleep(r.delay)  # the loop keeps running, as with a real slow backend
        if r.status != 200:
            return httpx.Response(r.status, json=r.body, headers=r.headers) if not isinstance(r.body, str) else httpx.Response(r.status, text=r.body, headers=r.headers)
        if r.raw is not None:
            return httpx.Response(200, headers={"content-type": "text/event-stream"}, content=r.raw.encode())
        events = r.events
        if events is None:
            events = [{"message": r.text[i:i + r.chunk]} for i in range(0, len(r.text), r.chunk)]
            events.append(r.final if r.final is not None else {"stop_reason": "stop", "message_id": "up-msg-1", "tokens": {"input": len(body["user_prompt"]) // 4, "output": max(1, len(r.text) // 4)}})
        parts = [f"data: {json.dumps(e) if not isinstance(e, str) else e}\r\n\r\n".encode() for e in events]
        if r.break_after is not None:
            return httpx.Response(200, headers={"content-type": "text/event-stream"}, stream=_Broken(parts[: r.break_after]))
        return httpx.Response(200, headers={"content-type": "text/event-stream"}, content=b"".join(parts))


# ---------------------------------------------------------------------------------------------------------------------
# fixtures
# ---------------------------------------------------------------------------------------------------------------------

@pytest.fixture
def upstream():
    if IMPL:
        from blackbox import HttpUpstream
        up = HttpUpstream(Reply)
        yield up
        up.close()
    else:
        yield Upstream()


@pytest.fixture
def sleeps() -> list[float]:
    """Retry backoff waits, recorded instead of slept (the gateway fixture installs the fake sleep)."""
    return []


@pytest.fixture
def make_cfg(tmp_path):
    def make(toml: str = MIDIR_TOML, env: dict | None = None) -> Config:
        f = tmp_path / "midir.toml"
        f.write_text(toml.replace("{responses_dir}", str(tmp_path / "responses")))
        return Config(env=env or {}, config_file=f, root=tmp_path)
    return make


@pytest.fixture
def gateway(request, make_cfg, upstream, sleeps, tmp_path):
    """The real Gateway on the mock upstream (in black-box mode: the implementation as a process on the HTTP stand-in).
    Parametrize indirectly with a TOML string to change the configuration."""
    toml = getattr(request, "param", MIDIR_TOML)
    if IMPL:
        from blackbox import ServerProcess
        server = ServerProcess(IMPL, toml, upstream, tmp_path / "server")
        yield server
        server.stop()
        return
    gw = Gateway(make_cfg(toml))

    async def fake_sleep(s: float) -> None:
        sleeps.append(s)

    for backend in gw.backends.values():
        backend._http = httpx.AsyncClient(transport=httpx.MockTransport(upstream.handler))
        backend.retry["sleep"] = fake_sleep
    yield gw


@pytest.fixture
def stackspot(gateway):
    if IMPL:
        pytest.skip("in-process only")
    return gateway.backends["stackspot"]


@pytest.fixture
def app_client(gateway):
    if IMPL:
        with httpx.Client(base_url=gateway.url, timeout=120) as c:
            yield c
        return
    from fastapi.testclient import TestClient
    with TestClient(build_app(gateway), raise_server_exceptions=False) as c:
        yield c


@pytest.fixture
def openai_client(app_client):
    import openai
    if IMPL:
        return openai.OpenAI(base_url=f"{app_client.base_url}".rstrip("/") + "/v1", api_key="x", max_retries=0, timeout=120)
    return openai.OpenAI(base_url="http://testserver/v1", api_key="x", http_client=app_client, max_retries=0)


@pytest.fixture
def anthropic_client(app_client):
    import anthropic
    if IMPL:
        return anthropic.Anthropic(base_url=f"{app_client.base_url}".rstrip("/"), api_key="x", max_retries=0, timeout=120)
    return anthropic.Anthropic(base_url="http://testserver", api_key="x", http_client=app_client, max_retries=0)


# ---------------------------------------------------------------------------------------------------------------------
# helpers
# ---------------------------------------------------------------------------------------------------------------------

def sse_events(text: str) -> list[tuple[str | None, Any]]:
    """Parse an SSE body into (event name, decoded data) pairs; `[DONE]` stays a string."""
    out = []
    for block in text.replace("\r\n", "\n").split("\n\n"):
        name, data = None, []
        for line in block.split("\n"):
            if line.startswith("event:"):
                name = line[6:].strip()
            elif line.startswith("data:"):
                data.append(line[5:].strip())
        if data:
            raw = "\n".join(data)
            out.append((name, raw if raw == "[DONE]" else json.loads(raw)))
    return out


def tool_call_text(name: str, args: dict, cid: str = "call_1") -> str:
    return f'<tool_call id="{cid}">\n{json.dumps({"name": name, "arguments": args})}\n</tool_call>'


READ = {"type": "function", "function": {"name": "read_file", "description": "Read a file from disk", "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}}}
RUN = {"type": "function", "function": {"name": "run_command", "description": "Run a shell command in the terminal", "parameters": {"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}}}
CHAT_TOOLS = [READ, RUN]
RESP_TOOLS = [{"type": "function", **t["function"]} for t in CHAT_TOOLS]
ANTH_TOOLS = [{"name": t["function"]["name"], "description": t["function"]["description"], "input_schema": t["function"]["parameters"]} for t in CHAT_TOOLS]
