"""SSE keepalive while a stream waits for the backend's first content: comments (OpenAI) or pings (Anthropic) every
keepalive_s, none once content flows, and streams that stay valid for the official SDK parsers."""
from __future__ import annotations

import asyncio

import pytest

from conftest import MIDIR_TOML, Reply, tool_call_text
from midir.app import with_keepalive
from midir.canonical import Event

FAST_KEEPALIVE = MIDIR_TOML.replace('responses_dir = "{responses_dir}"', 'responses_dir = "{responses_dir}"\nkeepalive_s = 0.05')
NO_KEEPALIVE = MIDIR_TOML.replace('responses_dir = "{responses_dir}"', 'responses_dir = "{responses_dir}"\nkeepalive_s = 0')
SLOW = 0.4  # backend silence, in seconds: about 8 keepalive intervals
USER = [{"role": "user", "content": "hi"}]
keepalive_config = pytest.mark.parametrize("gateway", [FAST_KEEPALIVE], indirect=True)


def split_at_content(text: str, content_marker: str) -> tuple[str, str]:
    i = text.index(content_marker)
    return text[:i], text[i:]


@keepalive_config
def test_chat_keepalive_before_content_only(app_client, upstream):
    upstream.add(Reply("Hello there, this is the answer.", delay=SLOW))
    text = app_client.post("/v1/chat/completions", json={"model": "gpt-5.1", "messages": USER, "stream": True}).text
    before, after = split_at_content(text, '"content": "Hello')
    assert before.count(": keepalive") >= 2
    assert ": keepalive" not in after and text.rstrip().endswith("data: [DONE]")


@keepalive_config
def test_responses_keepalive_before_content_only(app_client, upstream):
    upstream.add(Reply("Hello there.", delay=SLOW))
    text = app_client.post("/v1/responses", json={"model": "gpt-5.1", "input": "hi", "stream": True}).text
    before, after = split_at_content(text, "response.output_text.delta")
    assert before.count(": keepalive") >= 2 and ": keepalive" not in after


@keepalive_config
def test_messages_ping_before_content_only(app_client, upstream):
    upstream.add(Reply("Hello there.", delay=SLOW))
    text = app_client.post("/v1/messages", json={"model": "gpt-5.1", "max_tokens": 50, "messages": USER, "stream": True}).text
    before, after = split_at_content(text, "content_block_start")
    assert before.count("event: ping") >= 3  # the one message_start always sends, then the keepalives
    assert "event: ping" not in after


@keepalive_config
def test_sdks_parse_streams_with_keepalives(openai_client, anthropic_client, upstream):
    upstream.add(Reply("one two three", delay=SLOW), Reply(tool_call_text("read_file", {"path": "a.py"}), delay=SLOW), Reply("Olá.", delay=SLOW))
    chunks = list(openai_client.chat.completions.create(model="gpt-5.1", messages=USER, stream=True))
    assert "".join(c.choices[0].delta.content or "" for c in chunks if c.choices) == "one two three"
    tools = [{"type": "function", "name": "read_file", "parameters": {"type": "object", "properties": {"path": {"type": "string"}}}}]
    events = list(openai_client.responses.create(model="gpt-5.1", input="read a.py", tools=tools, stream=True))
    assert events[-1].type == "response.completed" and events[-1].response.output[-1].type == "function_call"
    with anthropic_client.messages.stream(model="gpt-5.1", max_tokens=50, messages=USER) as s:
        assert s.get_final_message().content[0].text == "Olá."


@keepalive_config
def test_json_mode_waits_with_keepalives(app_client, upstream):
    upstream.add(Reply('{"a": 1}', delay=SLOW))
    text = app_client.post("/v1/chat/completions", json={"model": "gpt-5.1", "messages": USER, "stream": True, "response_format": {"type": "json_object"}}).text
    assert text.count(": keepalive") >= 2 and '{\\"a\\": 1}' in text


@keepalive_config
def test_fast_backend_gets_no_keepalive(app_client, upstream):
    upstream.add("quick")
    text = app_client.post("/v1/chat/completions", json={"model": "gpt-5.1", "messages": USER, "stream": True}).text
    assert ": keepalive" not in text


@pytest.mark.parametrize("gateway", [NO_KEEPALIVE], indirect=True)
def test_keepalive_can_be_turned_off(app_client, upstream):
    upstream.add(Reply("late", delay=0.2))
    text = app_client.post("/v1/chat/completions", json={"model": "gpt-5.1", "messages": USER, "stream": True}).text
    assert ": keepalive" not in text


def test_client_leaving_while_waiting_closes_the_backend_stream():
    closed = []

    async def slow_events():
        try:
            await asyncio.sleep(10)
            yield Event("text", text="never")
        finally:
            closed.append(True)

    async def go():
        gen = with_keepalive(slow_events(), 0.01)
        first = await gen.__anext__()
        await gen.aclose()  # what Starlette does when the client disconnects
        return first

    assert asyncio.run(go()).kind == "keepalive" and closed == [True]
