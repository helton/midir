"""Differential parity: another implementation against the Python reference, side by side, on the real client requests
captured in .internal/dev/captures (local only). For each request and scripted reply both must send StackSpot the same
prompt, byte for byte, and answer the client with the same body or event sequence (ids, timestamps and keepalives
aside).

    MIDIR_IMPL=go uv run pytest -q tests/test_parity.py
"""
from __future__ import annotations

import json
import re
from pathlib import Path

import httpx
import pytest

from conftest import IMPL, MIDIR_TOML, ROOT, Reply, sse_events, tool_call_text

CAPTURES = sorted((ROOT / ".internal" / "dev" / "captures").glob("*.json"))
PATHS = {"chat": "/v1/chat/completions", "responses": "/v1/responses", "messages": "/v1/messages"}
ID_RE = re.compile(r"\b(chatcmpl-|resp_|msg_|call_|fc_|ctc_|toolu_)[0-9a-f]{20,32}\b")
VOLATILE_KEYS = {"created", "created_at", "system_fingerprint"}

pytestmark = [pytest.mark.blackbox, pytest.mark.skipif(not IMPL or IMPL == "python" or not CAPTURES, reason="needs MIDIR_IMPL=<other implementation> and the local captures")]


def normalize(value, ids: dict[str, str]):
    """Generated ids become ID1, ID2... in order of appearance; volatile keys are dropped."""
    if isinstance(value, dict):
        return {k: normalize(v, ids) for k, v in value.items() if k not in VOLATILE_KEYS}
    if isinstance(value, list):
        return [normalize(v, ids) for v in value]
    if isinstance(value, str):
        return ID_RE.sub(lambda m: ids.setdefault(m.group(0), f"ID{len(ids) + 1}"), value)
    return value


def client_view(r: httpx.Response) -> tuple[int, object]:
    ids: dict[str, str] = {}
    if r.headers.get("content-type", "").startswith("text/event-stream"):
        return r.status_code, [(name, normalize(data, ids)) for name, data in sse_events(r.text) if name != "ping" or data != {"type": "ping"}]
    try:
        return r.status_code, normalize(r.json(), ids)
    except ValueError:
        return r.status_code, r.text


@pytest.fixture(scope="module")
def pair(tmp_path_factory):
    from blackbox import HttpUpstream, ServerProcess
    toml = MIDIR_TOML.replace('responses_dir = "{responses_dir}"', 'responses_dir = "{responses_dir}"\nkeepalive_s = 0')
    servers = []
    for impl in ("python", IMPL):
        up = HttpUpstream(Reply)
        servers.append((ServerProcess(impl, toml, up, tmp_path_factory.mktemp(impl)), up))
    yield servers
    for server, up in servers:
        server.stop()
        up.close()


def first_tool(body: dict) -> str | None:
    for t in body.get("tools") or []:
        name = t.get("name") or (t.get("function") or {}).get("name")
        if name and (t.get("type", "function") in ("function", "custom") or "input_schema" in t):
            return name
    return None


def replies_for(body: dict) -> list[tuple[str, list[str]]]:
    tool = first_tool(body) if body.get("tool_choice") != "none" else None
    if not tool:
        return [("text", ["A plain answer, in two sentences. Done."])]
    return [("tool", ["Reading it.\n" + tool_call_text(tool, {})]),
            ("promise", ["Vou ler o arquivo agora.", tool_call_text(tool, {})])]  # announce-and-stop: one follow-up


CASES = [(p, stream) for p in CAPTURES for stream in (False, True)]


@pytest.mark.parametrize("capture,stream", CASES, ids=[f"{p.name[:34]}-{'sse' if s else 'json'}" for p, s in CASES])
def test_same_prompts_and_answers(pair, capture: Path, stream: bool):
    cap = json.loads(capture.read_text())
    body = {**cap["body"], "stream": stream}
    body.pop("previous_response_id", None)
    for kind, script in replies_for(body):
        views, prompts = [], []
        for server, up in pair:
            up.script.clear()
            up.calls.clear()
            up.add(*script)
            with httpx.Client(timeout=120) as c:
                views.append(client_view(c.post(server.url + PATHS[cap["protocol"]], json=body)))
            prompts.append(up.prompts)
        assert prompts[1] == prompts[0], f"{kind}: prompts sent to StackSpot differ"
        assert views[1] == views[0], f"{kind}: answers to the client differ"
