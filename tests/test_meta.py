"""Telemetry labels: which client sent a request and which session it belongs to (labels only, no behavior)."""
from __future__ import annotations

import pytest

from midir.canonical import CanonicalRequest, ToolResult
from midir.config import ModelSpec
from midir.telemetry import client_of, request_meta

ROUTE = ModelSpec("gpt-5.1", "stackspot", "AGENT")


@pytest.mark.parametrize("headers,client", [
    ({"user-agent": "claude-cli/2.1.283 (external, cli)"}, "claude-code"),
    ({"user-agent": "claude-cli/2.1 (external, claude-vscode)"}, "claude-code-vscode"),
    ({"user-agent": "codex_cli_rs/0.50"}, "codex"),
    ({"user-agent": "GitHubCopilotChat/0.32"}, "copilot"),
    ({"user-agent": "OpenAI/JS 5.0", "x-initiator": "user"}, "copilot"),
    ({"user-agent": "opencode/1.0"}, "opencode"),
    ({"user-agent": "deepseek-harness/0.2.0-rc.2"}, "deepseek-harness"),
    ({"user-agent": "OpenAI/Python 2.24.0"}, "openai"),
    ({}, "unknown"),
])
def test_client_detection(headers, client):
    assert client_of(headers)[0] == client


@pytest.mark.parametrize("ua,system,client", [  # real openings captured 2026-10-03
    ("OpenAI/Python 2.24.0", "You are Hermes Agent, built by Nous Research. Be direct", "hermes"),
    ("OpenAI/JS 7.20.0", "<!-- openclaw:attempt:STABLE -->\nYou are a personal assistant running inside OpenClaw.", "openclaw"),
    ("deepseek-harness/0.2.0-rc.2 (+https://github.com/deepseek-ai/deepseek-harness)", "You are an AI agent powered by DeepSeek Harness.", "deepseek-harness"),
    ("claude-cli/2.1.283 (external, cli)", "mentions OpenClaw somewhere", "claude-code"),
])
def test_client_detection_by_system_prompt(ua, system, client):
    assert client_of({"user-agent": ua}, system)[0] == client


def _req(text="hello"):
    r = CanonicalRequest()
    r.add("user", text)
    return r


def test_session_sources():
    assert request_meta({"x-claude-code-session-id": "S1"}, {}, "messages", "m", ROUTE, _req(), "msg_1")["session"] == "S1"
    assert request_meta({}, {"metadata": {"user_id": '{"session_id": "S2"}'}}, "messages", "m", ROUTE, _req(), "msg_1")["session"] == "S2"
    assert request_meta({}, {"prompt_cache_key": "S3"}, "responses", "m", ROUTE, _req(), "resp_1")["session"] == "S3"
    a = request_meta({}, {}, "chat", "m", ROUTE, _req("same first message"), "c1")["session"]
    b = request_meta({}, {}, "chat", "m", ROUTE, _req("same first message"), "c2")["session"]
    assert a == b and a.startswith("conv-")


def test_initiator():
    r = _req()
    r.add("user", tool_results=[ToolResult("c", "out")])
    assert request_meta({}, {}, "chat", "m", ROUTE, r, "c")["initiator"] == "agent"
    assert request_meta({"x-initiator": "user"}, {}, "chat", "m", ROUTE, r, "c")["initiator"] == "user"


def test_startup_banner():
    from midir.cli import banner
    text = banner("9.9.9")
    lines = [ln for ln in text.splitlines() if ln.strip()]
    assert "\t" not in text and len(lines) == 3 and lines[0].endswith("v9.9.9") and "M  I  D  I  R" in lines[1]
    assert len({ln.index("│") for ln in lines}) == 1  # one straight vertical rule
