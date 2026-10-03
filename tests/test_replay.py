"""Replay of real client requests captured with an earlier version (237 requests; kept out of the repository because
they hold real conversations: the test is skipped without them). Source: .internal/dev/captures (Copilot, Claude Code, Codex, OpenCode, aider):
every one goes through its protocol adapter, the prompt renderer and the response/stream writer with a text reply
and with a tool-call reply, streaming and not. Any 4xx/5xx or malformed SSE is a regression."""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from conftest import ROOT, sse_events, tool_call_text

CAPTURES = sorted((ROOT / ".internal" / "dev" / "captures").glob("*.json"))
PATHS = {"chat": "/v1/chat/completions", "responses": "/v1/responses", "messages": "/v1/messages"}


def first_tool(body: dict) -> str | None:
    for t in body.get("tools") or []:
        name = t.get("name") or (t.get("function") or {}).get("name")
        if name and t.get("type", "function") in ("function", "custom", None) or (name and "input_schema" in t):
            return name
    return None


@pytest.mark.skipif(not CAPTURES, reason="no captures")
@pytest.mark.parametrize("path", CAPTURES, ids=lambda p: p.name[:40])
def test_capture(app_client, upstream, path: Path):
    cap = json.loads(path.read_text())
    body = dict(cap["body"])
    body.pop("previous_response_id", None)  # the captured chains are not in this store
    tool = first_tool(body)
    reply = ("Working on it.\n" + tool_call_text(tool, {})) if tool and body.get("tool_choice") != "none" else "plain answer"
    for stream in (False, True):
        upstream.add(reply)
        r = app_client.post(PATHS[cap["protocol"]], json={**body, "stream": stream})
        assert r.status_code == 200, r.text[:500]
        if stream:
            evs = sse_events(r.text)
            assert evs and not any(name == "error" or (isinstance(d, dict) and "error" in d and cap["protocol"] == "chat") for name, d in evs), r.text[-500:]
        else:
            assert isinstance(r.json(), dict)
