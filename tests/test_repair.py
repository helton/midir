"""Invalid tool-call JSON and the /repair follow-up (bug seen with Hermes on 2026-10-03: a salvaged call with one extra
"}" still triggered /repair, whose reply re-emitted every call of the turn, double-escaped; the client got 8 writes)."""
from __future__ import annotations

import json

from conftest import Reply, sse_events
from midir.canonical import ToolSpec
from midir.emulation.parser import ToolCallParser

WRITE = {"type": "function", "function": {"name": "write_file", "description": "Write a file", "parameters": {"type": "object", "properties": {"path": {"type": "string"}, "content": {"type": "string"}}, "required": ["path", "content"]}}}
TOOLS = [WRITE]


def block(obj_text: str, cid: str = "call_1") -> str:
    return f'<tool_call id="{cid}">\n{obj_text}\n</tool_call>'


def w(path: str, content: str) -> str:
    return json.dumps({"name": "write_file", "arguments": {"path": path, "content": content}})


def stream_chat(app_client, upstream, *replies):
    upstream.add(*replies)
    r = app_client.post("/v1/chat/completions", json={"model": "gpt-5.1", "stream": True, "tools": TOOLS, "messages": [{"role": "user", "content": "write the files"}]})
    evs = [d for _, d in sse_events(r.text) if isinstance(d, dict)]
    calls = [tc for d in evs for tc in d["choices"][0]["delta"].get("tool_calls", [])] if evs else []
    finish = [d["choices"][0]["finish_reason"] for d in evs if d["choices"] and d["choices"][0]["finish_reason"]]
    return calls, finish, r.text


HERMES_TURN = "\n".join([block(w("a.py", "print(1)\n")), block(w("b.py", "print(2+3)\n"), "call_2"),
                         block(w("c.py", "x = 1\n"), "call_3"), block(w("notes.txt", "done\n") + "}", "call_4")])


def test_trailing_extra_brace_is_salvaged_without_repair(app_client, upstream):
    calls, finish, _ = stream_chat(app_client, upstream, HERMES_TURN)
    assert [json.loads(c["function"]["arguments"])["path"] for c in calls] == ["a.py", "b.py", "c.py", "notes.txt"]
    assert json.loads(calls[3]["function"]["arguments"])["content"] == "done\n"
    assert len(upstream.calls) == 1  # no /repair
    assert finish == ["tool_calls"]


def test_repair_asks_only_for_the_broken_block(app_client, upstream):
    broken = block('{"name": "write_file", "arguments": {"path": "d.py", "content": "print("x")"}}', "call_2")
    fixed = block(w("d.py", 'print("x")'))
    calls, finish, _ = stream_chat(app_client, upstream, block(w("a.py", "1")) + "\n" + broken, fixed)
    assert [json.loads(c["function"]["arguments"])["path"] for c in calls] == ["a.py", "d.py"]
    repair_prompt = upstream.prompts[1]
    tail = repair_prompt.split("could not be parsed as JSON", 1)[1]
    assert '"d.py"' in tail and "escape" not in tail.lower()
    assert finish == ["tool_calls"]


def test_repair_reply_that_re_emits_every_call_is_deduplicated(app_client, upstream):
    broken = block('{"name": "write_file", "arguments": {"path": "d.py", "content": "print("x")"}}', "call_2")
    everything = "\n".join([block(w("a.py", "1")), block(w("d.py", 'print("x")'), "call_2")])
    calls, finish, text = stream_chat(app_client, upstream, block(w("a.py", "1")) + "\n" + broken, everything)
    assert [json.loads(c["function"]["arguments"])["path"] for c in calls] == ["a.py", "d.py"]
    ids = [c["id"] for c in calls]
    assert len(set(ids)) == len(ids) and [c["index"] for c in calls] == [0, 1]
    assert finish == ["tool_calls"] and text.rstrip().endswith("[DONE]")


def test_double_escaped_re_emission_of_a_streamed_target_is_dropped(app_client, upstream):
    broken = block('{"name": "write_file", "arguments": {"path": "e.py", "content": "say("hi")"}}', "call_2")
    reply = "\n".join([block(w("a.py", "print(2+3)\\n")), block(w("e.py", 'say("hi")'), "call_2"), block(w("f.py", "extra"), "call_3")])
    calls, _, _ = stream_chat(app_client, upstream, block(w("a.py", "print(2+3)\n")) + "\n" + broken, reply)
    args = [json.loads(c["function"]["arguments"]) for c in calls]
    assert [a["path"] for a in args] == ["a.py", "e.py"]  # a.py re-emitted double-escaped: dropped; f.py beyond the 1 requested: dropped
    assert args[0]["content"] == "print(2+3)\n"


def test_second_edit_of_the_same_file_can_be_repaired(app_client, upstream):
    """The duplicate rule must not eat a legitimate repair that targets a file already written in this turn."""
    broken = block('{"name": "write_file", "arguments": {"path": "a.py", "content": "v2 "quoted""}}', "call_2")
    calls, _, _ = stream_chat(app_client, upstream, block(w("a.py", "v1")) + "\n" + broken, block(w("a.py", 'v2 "quoted"')))
    assert [json.loads(c["function"]["arguments"])["content"] for c in calls] == ["v1", 'v2 "quoted"']


def test_no_repair_when_nothing_was_lost():
    p = ToolCallParser([ToolSpec("write_file")])
    for junk in ("}", "]", "}}", " ,", "```"):
        p.rejected.clear()
        p.feed(block(w("x", "y") + junk))
        p.finish()
        assert p.rejected == [], junk
