"""Tool-call parser: blocks with several calls, broken JSON, streaming. Run: uv run poe test"""
from __future__ import annotations

import json
from pathlib import Path

import pytest

from midir.backends.base import Completion, TextBackend
from midir.canonical import CanonicalRequest, CanonicalResponse, Event, ToolSpec
from midir.config import BackendSettings, Config
from midir.emulation import EmulationEngine
from midir.emulation.parser import ToolCallParser
from midir.protocols import ChatCompletions

TOOLS = [ToolSpec("read_file", "d", {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
         ToolSpec("run", "d", {"type": "object", "properties": {"cmd": {"type": "string"}}, "required": ["cmd"]})]


def parse(text: str, chunk: int | None = None) -> tuple[list, list[str], ToolCallParser]:
    p = ToolCallParser(TOOLS)
    pieces = [text] if chunk is None else [text[i:i + chunk] for i in range(0, len(text), chunk)]
    events = [e for piece in pieces for e in p.feed(piece)] + p.finish()
    calls = [v for k, v in events if k == "tool_call"]
    texts = [v for k, v in events if k == "text"]
    return calls, texts, p


A = '{"name": "read_file", "arguments": {"path": "a.py"}}'
B = '{"name": "run", "arguments": {"cmd": "pytest -q"}}'


@pytest.mark.parametrize("inner", [f"{A}\n{B}", f"{A},\n{B}", f"[{A}, {B}]", f"{A} {B}", f"```json\n{A}\n{B}\n```"])
def test_several_calls_in_one_block(inner):
    calls, _, p = parse(f'<tool_call id="call_1">\n{inner}\n</tool_call>')
    assert [(c.name, c.arguments) for c in calls] == [("read_file", {"path": "a.py"}), ("run", {"cmd": "pytest -q"})]
    assert len({c.id for c in calls}) == 2


def test_single_valid_object_unchanged():
    calls, _, p = parse(f'Reading it.\n<tool_call id="call_1">\n{A}\n</tool_call>')
    assert [(c.name, c.arguments) for c in calls] == [("read_file", {"path": "a.py"})]
    assert p.errors == []


def test_valid_object_then_broken_tail_keeps_the_valid_one_and_logs_the_block():
    calls, _, p = parse(f'<tool_call id="call_1">\n{A}\n{{"name": "run", "arguments": {{"cmd": "ls"\n</tool_call>')
    assert [c.name for c in calls] == ["read_file"]
    assert any("raw block" in e for e in p.errors)


def test_broken_object_with_valid_arguments_is_salvaged():
    calls, _, p = parse('<tool_call id="call_1">\n{"name": "run", "arguments": {"cmd": "ls -la"}, oops}\n</tool_call>')
    assert [(c.name, c.arguments) for c in calls] == [("run", {"cmd": "ls -la"})]


def test_truly_broken_json_drops_the_call_without_crashing():
    calls, _, p = parse('<tool_call id="call_1">\n{"name": "run", "arguments": {cmd: ls}}\n</tool_call>')
    assert calls == []
    assert any("raw block" in e and "cmd: ls" in e for e in p.errors)


def test_arguments_never_a_non_json_fragment():
    calls, _, _ = parse('<tool_call id="call_1">\n{"name": "run", "arguments": "not json at all"}\n</tool_call>')
    assert calls == []


@pytest.mark.parametrize("chunk", [1, 3, 7, 40])
def test_streaming_split_across_chunks(chunk):
    calls, texts, _ = parse(f'Two calls:\n<tool_call id="call_1">\n{A}\n{B}\n</tool_call>', chunk=chunk)
    assert [c.name for c in calls] == ["read_file", "run"]
    assert "<tool_call" not in "".join(texts)


def test_finish_reason_and_ids_through_chat_stream():
    """Multi-call block through the Chat Completions adapter: two tool_calls with indexes 0 and 1, finish tool_calls."""
    import asyncio

    async def events():
        calls, _, _ = parse(f'<tool_call id="call_1">\n{A}\n{B}\n</tool_call>')
        for c in calls:
            yield Event("tool_call", call=c)
        yield Event("done", response=CanonicalResponse(tool_calls=calls, finish="tool_calls"))

    async def collect():
        return [c async for c in ChatCompletions.stream(events(), "chatcmpl-x", 0, "m", False)]

    chunks = [json.loads(c[5:]) for c in asyncio.run(collect()) if c.startswith("data: {")]
    tool_deltas = [tc for ch in chunks for tc in ch["choices"][0]["delta"].get("tool_calls", [])]
    assert [tc["index"] for tc in tool_deltas] == [0, 1]
    assert len({tc["id"] for tc in tool_deltas}) == 2
    assert chunks[-1]["choices"][0]["finish_reason"] == "tool_calls"


# ---------------------------------------------------------------- engine: repair follow-up for invalid tool-call JSON

class FakeBackend(TextBackend):
    """Stands in for a text backend: returns the scripted replies in order and records the prompts."""

    type = "fake"

    def __init__(self, replies: list[str]):
        super().__init__(BackendSettings("fake", "fake"))
        self.replies, self.prompts = list(replies), []

    async def stream(self, prompt, target, meta=None):
        self.prompts.append(prompt)
        yield self.replies.pop(0)
        yield Completion({"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15})

    async def ready(self):
        return None


def run_engine(replies: list[str]):
    import asyncio
    client = FakeBackend(replies)
    config = Config(env={"STACKSPOT_DEFAULT_AGENT_ID": "x"}, config_file=Path("/nonexistent"))
    req = CanonicalRequest(tools=TOOLS)
    req.add("user", "run the tests")

    async def go():
        return [e async for e in EmulationEngine(client, config).run(req, "t")]

    events = asyncio.run(go())
    done = events[-1].response
    return done, client


BROKEN = '<tool_call id="call_1">\n{"name": "run", "arguments": {"cmd": "echo "oops""}}\n</tool_call>'
FIXED = '<tool_call id="call_1">\n{"name": "run", "arguments": {"cmd": "echo \\"oops\\""}}\n</tool_call>'


def test_invalid_json_call_gets_one_repair_follow_up():
    done, client = run_engine([BROKEN, FIXED])
    assert [(c.name, c.arguments) for c in done.tool_calls] == [("run", {"cmd": 'echo "oops"'})]
    assert done.finish == "tool_calls"
    assert len(client.prompts) == 2 and "could not be parsed as JSON" in client.prompts[1]
    assert done.usage["prompt_tokens"] == 20  # both upstream calls are counted


def test_repair_that_fails_again_ends_without_a_broken_call():
    done, client = run_engine([BROKEN, BROKEN])
    assert done.tool_calls == [] and done.finish == "stop"
    assert len(client.prompts) == 2  # exactly one follow-up, no loop


def test_valid_calls_are_kept_and_only_the_broken_one_is_repaired():
    first = f'<tool_call id="call_1">\n{A}\n</tool_call>\n' + BROKEN.replace("call_1", "call_2")
    done, client = run_engine([first, FIXED])
    assert [c.name for c in done.tool_calls] == ["read_file", "run"]
    assert "read_file" in client.prompts[1] and "echo \"oops\"" in client.prompts[1]
