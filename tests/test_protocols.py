"""The three protocols end to end (app + mock upstream), parsed by the official SDKs: streaming and not, text and tool
calls, ids, finish reasons, usage. If an SDK can parse it, real clients built on it can too."""
from __future__ import annotations

import json

import pytest

from conftest import ANTH_TOOLS, CHAT_TOOLS, RESP_TOOLS, Reply, sse_events, tool_call_text

TWO_CALLS = "Reading both.\n" + tool_call_text("read_file", {"path": "a.py"}) + "\n" + tool_call_text("read_file", {"path": "b.py"}, "call_2")
USER = [{"role": "user", "content": "read a.py and b.py"}]


# ---------------------------------------------------------------- chat completions

def test_chat_text_non_stream(openai_client, upstream):
    upstream.add("Hello there.")
    r = openai_client.chat.completions.create(model="gpt-5.1", messages=USER)
    assert r.choices[0].message.content == "Hello there."
    assert r.choices[0].finish_reason == "stop"
    assert r.usage.prompt_tokens > 0 and r.usage.completion_tokens > 0
    assert r.usage.total_tokens == r.usage.prompt_tokens + r.usage.completion_tokens
    assert r.model == "gpt-5.1" and r.id.startswith("chatcmpl-")


def test_chat_tool_calls_non_stream(openai_client, upstream):
    upstream.add(TWO_CALLS)
    r = openai_client.chat.completions.create(model="gpt-5.1", messages=USER, tools=CHAT_TOOLS)
    m = r.choices[0].message
    assert r.choices[0].finish_reason == "tool_calls"
    assert [json.loads(c.function.arguments) for c in m.tool_calls] == [{"path": "a.py"}, {"path": "b.py"}]
    assert m.content == "Reading both."
    assert len({c.id for c in m.tool_calls}) == 2


def test_chat_stream_text_and_tools(openai_client, upstream):
    upstream.add(TWO_CALLS)
    chunks = list(openai_client.chat.completions.create(model="gpt-5.1", messages=USER, tools=CHAT_TOOLS, stream=True, stream_options={"include_usage": True}))
    text = "".join(c.choices[0].delta.content or "" for c in chunks if c.choices)
    calls = [tc for c in chunks if c.choices for tc in (c.choices[0].delta.tool_calls or [])]
    assert text.strip() == "Reading both."
    assert [tc.index for tc in calls] == [0, 1]
    assert [c.choices[0].finish_reason for c in chunks if c.choices and c.choices[0].finish_reason] == ["tool_calls"]
    usage = [c.usage for c in chunks if c.usage]
    assert usage and usage[-1].prompt_tokens > 0


def test_chat_stream_usage_has_the_same_shape_as_non_stream(app_client, upstream):
    upstream.add("hi", "hi")
    body = {"model": "gpt-5.1", "messages": USER}
    plain = app_client.post("/v1/chat/completions", json=body).json()["usage"]
    streamed = [d for _, d in sse_events(app_client.post("/v1/chat/completions", json={**body, "stream": True}).text) if isinstance(d, dict) and d.get("usage")]
    assert set(streamed[-1]["usage"]) == set(plain)


def test_chat_stream_without_include_usage(app_client, upstream):
    upstream.add("hi")
    evs = sse_events(app_client.post("/v1/chat/completions", json={"model": "gpt-5.1", "messages": USER, "stream": True, "stream_options": {"include_usage": False}}).text)
    assert evs[-1][1] == "[DONE]"
    assert not any(isinstance(d, dict) and d.get("usage") for _, d in evs)


# ---------------------------------------------------------------- responses

def test_responses_text_non_stream(openai_client, upstream):
    upstream.add("Plain answer.")
    r = openai_client.responses.create(model="gpt-4.1", input="hi")
    assert r.output_text == "Plain answer."
    assert r.status == "completed" and r.usage.input_tokens > 0
    assert upstream.calls[0]["agent"] == "AGENT41"


def test_responses_tool_calls_non_stream(openai_client, upstream):
    upstream.add(TWO_CALLS)
    r = openai_client.responses.create(model="gpt-5.1", input="read both", tools=RESP_TOOLS)
    kinds = [o.type for o in r.output]
    assert kinds == ["message", "function_call", "function_call"]
    assert [json.loads(o.arguments) for o in r.output if o.type == "function_call"] == [{"path": "a.py"}, {"path": "b.py"}]


def test_responses_stream_item_identity_text_tool_text(openai_client, upstream):
    """A message that resumes after a tool call is a new item: ids never repeat across output items (OpenClaw aborts otherwise)."""
    upstream.add("Before.\n" + tool_call_text("read_file", {"path": "a.py"}) + "\nAfter the call.")
    events = list(openai_client.responses.create(model="gpt-5.1", input="x", tools=RESP_TOOLS, stream=True))
    added = [e for e in events if e.type == "response.output_item.added"]
    done = [e for e in events if e.type == "response.output_item.done"]
    assert [e.item.type for e in added] == ["message", "function_call", "message"]
    assert len({e.item.id for e in added}) == 3
    assert [e.item.id for e in added] == [e.item.id for e in done]
    for e in events:
        if e.type == "response.output_text.delta":
            assert e.output_index in (0, 2)
    final = events[-1]
    assert final.type == "response.completed"
    assert [o.id for o in final.response.output] == [e.item.id for e in done]
    seq = [e.sequence_number for e in events]
    assert seq == sorted(seq) and len(set(seq)) == len(seq)


def test_responses_custom_tool(openai_client, upstream):
    tools = [{"type": "custom", "name": "apply_patch", "description": "Apply a patch"}]
    upstream.add(tool_call_text("apply_patch", {"input": "*** Begin Patch\n*** End Patch"}))
    r = openai_client.responses.create(model="gpt-5.1", input="patch it", tools=tools)
    item = r.output[-1]
    assert item.type == "custom_tool_call" and item.input.startswith("*** Begin Patch")


def test_responses_previous_response_id_chain(openai_client, upstream):
    upstream.add(tool_call_text("read_file", {"path": "a.py"}), "It prints 1.")
    r1 = openai_client.responses.create(model="gpt-5.1", input="what does a.py print?", tools=RESP_TOOLS)
    call = r1.output[-1]
    r2 = openai_client.responses.create(model="gpt-5.1", previous_response_id=r1.id, input=[{"type": "function_call_output", "call_id": call.call_id, "output": "print(1)"}])
    assert r2.output_text == "It prints 1."
    p = upstream.prompts[1]
    assert "what does a.py print?" in p and "print(1)" in p and '"read_file"' in p  # history + tools inherited


def test_responses_get_stored(openai_client, upstream):
    upstream.add("stored")
    r = openai_client.responses.create(model="gpt-5.1", input="x")
    got = openai_client.responses.retrieve(r.id)
    assert got.output_text == "stored"


def test_responses_unknown_previous_id_is_404(openai_client):
    import openai
    with pytest.raises(openai.NotFoundError):
        openai_client.responses.create(model="gpt-5.1", previous_response_id="resp_" + "0" * 24, input="x")


# ---------------------------------------------------------------- anthropic messages

def test_messages_text_non_stream(anthropic_client, upstream):
    upstream.add("Olá.")
    r = anthropic_client.messages.create(model="claude-haiku-4-5", max_tokens=100, messages=USER)
    assert r.content[0].text == "Olá." and r.stop_reason == "end_turn"
    assert upstream.calls[0]["agent"] == "AGENT41"
    assert r.usage.input_tokens > 0


def test_messages_tool_use_non_stream(anthropic_client, upstream):
    upstream.add(TWO_CALLS)
    r = anthropic_client.messages.create(model="claude-opus-4-5", max_tokens=100, messages=USER, tools=ANTH_TOOLS)
    assert r.stop_reason == "tool_use"
    assert [b.type for b in r.content] == ["text", "tool_use", "tool_use"]
    assert all(b.id.startswith("toolu_") for b in r.content if b.type == "tool_use")


def test_messages_stream_blocks(anthropic_client, upstream):
    upstream.add(TWO_CALLS)
    with anthropic_client.messages.stream(model="claude-sonnet-4-6", max_tokens=100, messages=USER, tools=ANTH_TOOLS) as s:
        events = list(s)
        final = s.get_final_message()
    starts = [e for e in events if e.type == "content_block_start"]
    assert [e.index for e in starts] == [0, 1, 2]
    assert [b.type for b in final.content] == ["text", "tool_use", "tool_use"]
    assert final.content[1].input == {"path": "a.py"}
    assert final.stop_reason == "tool_use"
    assert final.usage.output_tokens > 0
    assert upstream.calls[0]["agent"] == "AGENTFLEX"  # "sonnet" regex


def test_messages_tool_result_round_trip(anthropic_client, upstream):
    upstream.add("Done.")
    msgs = USER + [{"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {"path": "a.py"}}]},
                   {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": [{"type": "text", "text": "print(1)"}]}]}]
    anthropic_client.messages.create(model="gpt-5.1", max_tokens=100, messages=msgs, tools=ANTH_TOOLS)
    assert '<tool_result id="toolu_1" name="read_file">\nprint(1)\n</tool_result>' in upstream.prompts[0]


def test_count_tokens(anthropic_client):
    r = anthropic_client.messages.count_tokens(model="gpt-5.1", messages=USER)
    assert r.input_tokens > 0


# ---------------------------------------------------------------- misc endpoints

def test_models_and_health(app_client):
    models = app_client.get("/v1/models").json()
    assert [m["id"] for m in models["data"]] == ["gpt-5.1", "gpt-4.1", "flex"]
    h = app_client.get("/health").json()
    assert h["ok"] and h["default"] == "gpt-5.1" and "AGENT51" not in json.dumps(h)  # ids are abbreviated


def test_embeddings_is_a_clear_404(openai_client):
    import openai
    with pytest.raises(openai.NotFoundError):
        openai_client.embeddings.create(model="x", input="hi")


@pytest.mark.parametrize("path", ["/v1/chat/completions", "/v1/responses", "/v1/messages"])
def test_body_not_json_is_400(app_client, path):
    r = app_client.post(path, content=b"{not json", headers={"content-type": "application/json"})
    assert r.status_code == 400
