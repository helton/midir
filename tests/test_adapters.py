"""Protocol -> canonical translation: every field clients send, and malformed input answered with 400 (never 500)."""
from __future__ import annotations

import pytest

from midir.emulation.prompt import render_prompt
from midir.errors import ClientError
from midir.protocols import ChatCompletions as CC, Messages as MS, Responses as RS


# ---------------------------------------------------------------- chat completions

def test_chat_full_request():
    body = {"model": "m", "messages": [{"role": "system", "content": "S"}, {"role": "developer", "content": "D"}, {"role": "user", "content": "go"},
                                       {"role": "assistant", "content": None, "tool_calls": [{"id": "call_a", "type": "function", "function": {"name": "f", "arguments": "{\"x\":1}"}}]},
                                       {"role": "tool", "tool_call_id": "call_a", "content": "r1"}, {"role": "tool", "tool_call_id": "call_b", "content": [{"type": "text", "text": "r2"}]}],
            "tools": [{"type": "function", "function": {"name": "f", "parameters": {"type": "object", "properties": {"x": {"type": "integer"}}}}}],
            "tool_choice": {"type": "function", "function": {"name": "f"}}, "stop": ["END"], "max_completion_tokens": 9, "temperature": 0.2}
    req = CC.to_canonical(body)
    assert req.system == ["S", "D"]
    assert [t.role for t in req.turns] == ["user", "assistant", "user"]
    assert req.turns[1].tool_calls[0].arguments == {"x": 1}
    assert [r.content for r in req.turns[2].tool_results] == ["r1", "r2"]  # consecutive tool messages merge into one turn
    assert req.tool_choice == {"name": "f"} and req.stop == ["END"] and req.max_tokens == 9
    assert req.ignored == ["temperature"]


def test_temperature_zero_is_still_reported_as_ignored():
    assert "temperature" in CC.to_canonical({"messages": [{"role": "user", "content": "x"}], "temperature": 0}).ignored


def test_chat_stop_string_and_legacy_functions():
    req = CC.to_canonical({"messages": [{"role": "user", "content": "x"}], "stop": "END", "functions": [{"name": "g"}], "function_call": {"name": "g"}})
    assert req.stop == ["END"] and [t.name for t in req.tools] == ["g"] and req.tool_choice == {"name": "g"}


@pytest.mark.parametrize("fmt,schema", [({"type": "json_object"}, {"type": "object"}),
                                        ({"type": "json_schema", "json_schema": {"name": "x", "schema": {"type": "object", "required": ["a"]}}}, {"type": "object", "required": ["a"]}),
                                        ({"type": "text"}, None)])
def test_chat_response_format(fmt, schema):
    assert CC.to_canonical({"messages": [{"role": "user", "content": "x"}], "response_format": fmt}).json_schema == schema


def test_chat_media_becomes_placeholder_and_builtin_tools_are_ignored():
    req = CC.to_canonical({"messages": [{"role": "user", "content": [{"type": "text", "text": "see"}, {"type": "image_url", "image_url": {"url": "data:..."}}]}],
                           "tools": [{"type": "web_search"}]})
    assert "see" in req.turns[0].text and "[image_url omitted" in req.turns[0].text
    assert req.tools == [] and "tool:web_search" in req.ignored


@pytest.mark.parametrize("body", [{"messages": [{"role": "user", "content": "x"}], "n": 2}, {"messages": [{"role": "user", "content": "x"}], "logprobs": True}, {"messages": []}, {}])
def test_chat_refusals(body):
    with pytest.raises(ClientError):
        CC.to_canonical(body)


def test_chat_null_description_and_empty_arguments():
    req = CC.to_canonical({"messages": [{"role": "user", "content": "x"}, {"role": "assistant", "tool_calls": [{"id": "c", "function": {"name": "f", "arguments": ""}}]}],
                           "tools": [{"type": "function", "function": {"name": "f", "description": None, "parameters": None}}]})
    assert req.tools[0].description == "" and req.tools[0].parameters == {"type": "object", "properties": {}}
    assert req.turns[1].tool_calls[0].arguments == {}


def test_chat_empty_assistant_message_is_not_a_turn():
    """Hermes (#82924) can send an assistant message with empty content and no tool calls."""
    req = CC.to_canonical({"messages": [{"role": "user", "content": "a"}, {"role": "assistant", "content": ""}, {"role": "user", "content": "b"}]})
    prompt, _ = render_prompt(req, 10**6)
    assert "[assistant]: \n" not in prompt and not prompt.rstrip().endswith("[assistant]:")


# ---------------------------------------------------------------- responses

def test_responses_items():
    body = {"instructions": "I", "input": [{"type": "message", "role": "developer", "content": "D"}, {"role": "user", "content": [{"type": "input_text", "text": "go"}]},
                                           {"type": "reasoning", "summary": []}, {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"},
                                           {"type": "function_call_output", "call_id": "c1", "output": "ok"}, {"type": "custom_tool_call", "call_id": "c2", "name": "p", "input": "raw"},
                                           {"type": "custom_tool_call_output", "call_id": "c2", "output": [{"type": "input_text", "text": "applied"}]}],
            "tools": [{"type": "namespace", "tools": [{"type": "function", "name": "f"}]}, {"type": "custom", "name": "p"}, {"type": "web_search_preview"}],
            "text": {"format": {"type": "json_schema", "schema": {"type": "object"}}}, "max_output_tokens": 50}
    req = RS.to_canonical(body)
    assert req.system == ["I", "D"]
    assert [t.name for t in req.tools] == ["f", "p"] and req.tools[1].custom
    assert "tool:web_search_preview" in req.ignored
    assert [t.role for t in req.turns] == ["user", "assistant", "user", "assistant", "user"]
    assert req.turns[3].tool_calls[0].arguments == {"input": "raw"}
    assert [t.tool_results[0].content for t in req.turns[2::2]] == ["ok", "applied"]
    assert req.json_schema == {"type": "object"} and req.max_tokens == 50


def test_responses_unknown_item_is_400():
    with pytest.raises(ClientError):
        RS.to_canonical({"input": [{"type": "computer_call_output"}]})


# ---------------------------------------------------------------- anthropic messages

def test_messages_full_request():
    body = {"system": [{"type": "text", "text": "S", "cache_control": {"type": "ephemeral"}}], "max_tokens": 10, "stop_sequences": ["END"],
            "messages": [{"role": "user", "content": [{"type": "text", "text": "go"}, {"type": "image", "source": {}}]},
                         {"role": "assistant", "content": [{"type": "thinking", "thinking": "hmm"}, {"type": "text", "text": "ok"}, {"type": "tool_use", "id": "toolu_1", "name": "f", "input": {"a": 1}}]},
                         {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "res", "is_error": True}]}],
            "tools": [{"name": "f", "input_schema": {"type": "object"}}, {"type": "web_search_20250305", "name": "web_search"}], "tool_choice": {"type": "any"}}
    req = MS.to_canonical(body)
    assert req.system == ["S"] and req.stop == ["END"] and req.max_tokens == 10 and req.tool_choice == "required"
    assert "[image omitted" in req.turns[0].text and "hmm" not in req.turns[1].text
    assert req.turns[2].tool_results[0].is_error
    assert [t.name for t in req.tools] == ["f"]


def test_messages_stop_sequences_as_a_string_is_one_sequence():
    req = MS.to_canonical({"messages": [{"role": "user", "content": "x"}], "stop_sequences": "END"})
    assert req.stop == ["END"]


@pytest.mark.parametrize("choice,expected", [({"type": "tool", "name": "f"}, {"name": "f"}), ({"type": "tool"}, "required"), ({"type": "none"}, "none"), ({"type": "auto"}, "auto"), (None, "auto")])
def test_messages_tool_choice(choice, expected):
    req = MS.to_canonical({"messages": [{"role": "user", "content": "x"}], "tools": [{"name": "f", "input_schema": {"type": "object"}}], "tool_choice": choice})
    assert req.tool_choice == expected
    prompt, _ = render_prompt(req, 10**6)
    assert "`None`" not in prompt


# ---------------------------------------------------------------- malformed input: always a 400, never a 500

MALFORMED = [
    ("/v1/chat/completions", {"model": "m", "messages": ["hello"]}),
    ("/v1/chat/completions", {"model": "m", "messages": [{"role": "user", "content": "x"}], "tools": ["f"]}),
    ("/v1/chat/completions", {"model": "m", "messages": [{"role": "user", "content": "x"}], "tools": [{"type": "function", "function": "f"}]}),
    ("/v1/chat/completions", {"model": "m", "messages": [{"role": "assistant", "tool_calls": ["x"]}]}),
    ("/v1/chat/completions", {"model": "m", "messages": [{"role": "user", "content": "x"}], "stop": 5}),
    ("/v1/chat/completions", {"model": "m", "messages": [{"role": "user", "content": "x"}], "max_tokens": "ten"}),
    ("/v1/responses", {"model": "m", "input": [{"type": "message", "role": "user", "content": "x"}], "tools": ["f"]}),
    ("/v1/responses", {"model": "m", "input": 5}),
    ("/v1/responses", {"model": "m", "input": "x", "text": "json"}),
    ("/v1/messages", {"model": "m", "max_tokens": 5, "messages": ["hello"]}),
    ("/v1/messages", {"model": "m", "max_tokens": 5, "messages": [{"role": "user", "content": ["x"]}]}),
    ("/v1/messages", {"model": "m", "max_tokens": 5, "messages": [{"role": "user", "content": "x"}], "tools": [None]}),
    ("/v1/messages", {"model": "m", "max_tokens": 5, "messages": [{"role": "user", "content": "x"}], "system": 7}),
    ("/v1/messages/count_tokens", {"model": "m", "messages": ["x"]}),
]


@pytest.mark.parametrize("path,body", MALFORMED)
def test_malformed_requests_are_400(app_client, path, body):
    r = app_client.post(path, json=body)
    assert r.status_code == 400, r.text
    err = r.json()
    assert (err.get("error") or {}).get("message") or (err.get("error") or {}).get("type")


def test_tool_with_null_description_does_not_crash_the_incapacity_check(app_client, upstream):
    upstream.add("Não tenho acesso à internet.")
    tools = [{"type": "function", "function": {"name": "web_fetch", "description": None, "parameters": {"type": "object", "properties": {"url": {"type": "string"}}}}}]
    r = app_client.post("/v1/chat/completions", json={"model": "m", "messages": [{"role": "user", "content": "x"}], "tools": tools})
    assert r.status_code == 200


# ---------------------------------------------------------------- order of text around tool results

def test_text_after_tool_results_stays_after_them():
    """OpenClaw appends a runtime-context user message after the tool messages; it must not jump ahead of the results."""
    req = CC.to_canonical({"messages": [{"role": "user", "content": "go"}, {"role": "assistant", "tool_calls": [{"id": "c1", "function": {"name": "read", "arguments": "{}"}}]},
                                        {"role": "tool", "tool_call_id": "c1", "content": "RESULT"},
                                        {"role": "user", "content": [{"type": "text", "text": "<<<BEGIN_OPENCLAW_INTERNAL_CONTEXT>>>ctx<<<END_OPENCLAW_INTERNAL_CONTEXT>>>"}]}]})
    prompt, _ = render_prompt(req, 10**6)
    assert prompt.index("RESULT") < prompt.index("BEGIN_OPENCLAW_INTERNAL_CONTEXT")


def test_anthropic_text_block_after_tool_result_stays_after():
    req = MS.to_canonical({"messages": [{"role": "user", "content": "go"}, {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "f", "input": {}}]},
                                        {"role": "user", "content": [{"type": "text", "text": "BEFORE"}, {"type": "tool_result", "tool_use_id": "t1", "content": "RESULT"},
                                                                     {"type": "text", "text": "<system-reminder>AFTER</system-reminder>"}]}]})
    prompt, _ = render_prompt(req, 10**6)
    assert prompt.index("BEFORE") < prompt.index("RESULT") < prompt.index("AFTER")
