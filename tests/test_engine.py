"""Engine behavior through the app: stop/max_tokens cuts, JSON mode, automatic follow-ups (promise, incapacity,
tool_choice), assistant prefill, truncation, and what the Responses store keeps."""
from __future__ import annotations

import json

import pytest

from conftest import CHAT_TOOLS, RESP_TOOLS, Reply, sse_events, tool_call_text
from midir.canonical import CanonicalRequest
from midir.emulation.prompt import ASSISTANT_PREFILL_NUDGE, render_prompt

USER = [{"role": "user", "content": "fix the bug in a.py and run the tests"}]


def chat(app_client, **kw):
    body = {"model": "gpt-5.1", "messages": USER, **kw}
    r = app_client.post("/v1/chat/completions", json=body)
    assert r.status_code == 200, r.text
    return r.json() if not kw.get("stream") else [d for _, d in sse_events(r.text)]


# ---------------------------------------------------------------- stop sequences and max_tokens

def test_stop_sequence_cut_keeps_real_usage(app_client, upstream):
    upstream.add(Reply("one two END three four", chunk=3))
    r = chat(app_client, stop=["END"])
    assert r["choices"][0]["message"]["content"] == "one two "
    assert r["choices"][0]["finish_reason"] == "stop"
    assert r["usage"]["prompt_tokens"] > 0 and r["usage"]["completion_tokens"] > 0  # the final event was cut off: estimated


def test_max_tokens_cut_in_every_protocol(app_client, upstream):
    long = "word " * 200
    upstream.add(long, long, long)
    c = chat(app_client, max_tokens=5)
    assert c["choices"][0]["finish_reason"] == "length" and len(c["choices"][0]["message"]["content"]) <= 20
    assert c["usage"]["completion_tokens"] > 0
    rs = app_client.post("/v1/responses", json={"model": "gpt-5.1", "input": "x", "max_output_tokens": 5}).json()
    assert rs["status"] == "incomplete" and rs["incomplete_details"] == {"reason": "max_output_tokens"}
    ms = app_client.post("/v1/messages", json={"model": "gpt-5.1", "max_tokens": 5, "messages": USER}).json()
    assert ms["stop_reason"] == "max_tokens" and ms["usage"]["output_tokens"] > 0


def test_anthropic_stop_sequence_reported(app_client, upstream):
    upstream.add("alpha STOP beta")
    ms = app_client.post("/v1/messages", json={"model": "gpt-5.1", "max_tokens": 50, "messages": USER, "stop_sequences": ["STOP"]}).json()
    assert ms["stop_reason"] == "stop_sequence" and ms["stop_sequence"] == "STOP" and ms["content"][0]["text"] == "alpha "


def test_stop_split_across_chunks(app_client, upstream):
    upstream.add(Reply("abcXYZdef", chunk=4))
    assert chat(app_client, stop=["XYZ"])["choices"][0]["message"]["content"] == "abc"


# ---------------------------------------------------------------- JSON mode

def test_json_mode_valid_and_fenced(app_client, upstream):
    upstream.add('```json\n{"a": 1}\n```')
    r = chat(app_client, response_format={"type": "json_object"})
    assert json.loads(r["choices"][0]["message"]["content"]) == {"a": 1}
    assert len(upstream.calls) == 1


def test_json_mode_repair(app_client, upstream):
    schema = {"type": "object", "required": ["a"], "properties": {"a": {"type": "integer"}}}
    upstream.add('{"a": "x"}', '{"a": 2}')
    r = chat(app_client, response_format={"type": "json_schema", "json_schema": {"name": "s", "schema": schema}})
    assert json.loads(r["choices"][0]["message"]["content"]) == {"a": 2}
    assert "expected integer" in upstream.prompts[1]


def test_json_mode_with_required_tool_choice_does_not_retry_for_a_tool(app_client, upstream):
    upstream.add('{"a": 1}')
    chat(app_client, response_format={"type": "json_object"}, tools=CHAT_TOOLS, tool_choice="required")
    assert len(upstream.calls) == 1


def test_json_mode_streaming_is_buffered(app_client, upstream):
    upstream.add('{"ok": true}')
    evs = chat(app_client, response_format={"type": "json_object"}, stream=True)
    text = "".join(d["choices"][0]["delta"].get("content") or "" for d in evs if isinstance(d, dict) and d.get("choices"))
    assert json.loads(text) == {"ok": True}


# ---------------------------------------------------------------- follow-ups

def test_promise_follow_up_appends_the_calls(app_client, upstream):
    upstream.add("Vou ler o arquivo a.py agora.", tool_call_text("read_file", {"path": "a.py"}))
    r = chat(app_client, tools=CHAT_TOOLS)
    m = r["choices"][0]["message"]
    assert r["choices"][0]["finish_reason"] == "tool_calls" and m["tool_calls"][0]["function"]["name"] == "read_file"
    assert "Vou ler" in m["content"]
    assert "You announced an action" in upstream.prompts[1]
    assert r["usage"]["prompt_tokens"] == sum(len(p) // 4 for p in upstream.prompts)


def test_promise_follow_up_streaming(app_client, upstream):
    upstream.add("Let me check the tests.", tool_call_text("run_command", {"command": "pytest"}))
    evs = chat(app_client, tools=CHAT_TOOLS, stream=True)
    finish = [d["choices"][0]["finish_reason"] for d in evs if isinstance(d, dict) and d.get("choices") and d["choices"][0]["finish_reason"]]
    calls = [tc for d in evs if isinstance(d, dict) and d.get("choices") for tc in d["choices"][0]["delta"].get("tool_calls", [])]
    assert finish == ["tool_calls"] and [c["index"] for c in calls] == [0]


def test_no_follow_up_for_a_final_report_or_a_question(app_client, upstream):
    upstream.add("Corrigi o bug e todos os testes passaram.", "Qual arquivo você quer que eu leia?")
    chat(app_client, tools=CHAT_TOOLS)
    chat(app_client, tools=CHAT_TOOLS)
    assert len(upstream.calls) == 2


def test_false_incapacity_follow_up(app_client, upstream):
    tools = [{"type": "function", "function": {"name": "web_fetch", "description": "Fetch a URL", "parameters": {"type": "object", "properties": {"url": {"type": "string"}}}}}]
    upstream.add("I don't have access to the internet.", tool_call_text("web_fetch", {"url": "https://example.com"}))
    r = chat(app_client, tools=tools)
    assert r["choices"][0]["message"]["tool_calls"][0]["function"]["name"] == "web_fetch"
    assert "web_fetch" in upstream.prompts[1]


def test_redundant_confirmation_follow_up(app_client, upstream):
    msgs = [{"role": "user", "content": "corrija o bug e faça um commit ao final"}]
    upstream.add("Pronto para commit. Deseja que eu faça o commit?", tool_call_text("run_command", {"command": "git commit -am fix"}))
    r = chat(app_client, tools=CHAT_TOOLS, messages=msgs)
    assert r["choices"][0]["finish_reason"] == "tool_calls"
    assert "do not ask for confirmation" in upstream.prompts[1]


def test_tool_choice_none_disables_protocol_and_follow_ups(app_client, upstream):
    upstream.add("Vou ler o arquivo agora.")
    chat(app_client, tools=CHAT_TOOLS, tool_choice="none")
    assert len(upstream.calls) == 1 and "# Tools" not in upstream.prompts[0]


def test_tool_choice_required_retry_has_no_empty_assistant_turn(app_client, upstream):
    upstream.add("", tool_call_text("read_file", {"path": "a.py"}))
    r = chat(app_client, tools=CHAT_TOOLS, tool_choice="required")
    assert r["choices"][0]["finish_reason"] == "tool_calls"
    assert "[assistant]: \n" not in upstream.prompts[1] and "[assistant]: <" not in upstream.prompts[1]
    assert "MUST respond with a <tool_call>" in upstream.prompts[1]


def test_named_tool_choice_in_prompt(app_client, upstream):
    upstream.add(tool_call_text("run_command", {"command": "ls"}))
    chat(app_client, tools=CHAT_TOOLS, tool_choice={"type": "function", "function": {"name": "run_command"}})
    assert "MUST call the tool `run_command`" in upstream.prompts[0]


# ---------------------------------------------------------------- prompt shape

def test_assistant_prefill_gets_a_continue_nudge(app_client, upstream):
    upstream.add("rest")
    app_client.post("/v1/messages", json={"model": "gpt-5.1", "max_tokens": 50, "messages": USER + [{"role": "assistant", "content": "The answer is"}]})
    assert upstream.prompts[0].rstrip().endswith(ASSISTANT_PREFILL_NUDGE)


def test_tail_reminder_only_with_tools(app_client, upstream):
    upstream.add("a", "b")
    chat(app_client)
    chat(app_client, tools=CHAT_TOOLS)
    assert "<reminder>" not in upstream.prompts[0] and upstream.prompts[1].rstrip().endswith("</reminder>")


def test_truncation_keeps_system_and_recent_turns_and_tells_the_model():
    req = CanonicalRequest(system=["SYS"])
    for i in range(30):
        req.add("user" if i % 2 == 0 else "assistant", f"turn{i} " + "x" * 1000)
    req.add("user", "last question")
    prompt, info = render_prompt(req, 8000)
    assert info["dropped_turns"] > 0 and len(prompt) <= 8000
    assert prompt.startswith("<system>\nSYS") and prompt.rstrip().endswith("last question")
    assert "turn29" in prompt and "turn0 " not in prompt
    assert f"{info['dropped_turns']} earlier turns were omitted" in prompt


# ---------------------------------------------------------------- what the Responses store keeps

@pytest.mark.inprocess  # reads the store's memory
def test_stored_text_is_the_same_streaming_or_not(openai_client, upstream, gateway):
    reply = "Reading it.\n\n" + tool_call_text("read_file", {"path": "a.py"})
    upstream.add(reply, reply)
    r1 = openai_client.responses.create(model="gpt-5.1", input="x", tools=RESP_TOOLS)
    events = list(openai_client.responses.create(model="gpt-5.1", input="x", tools=RESP_TOOLS, stream=True))
    r2 = events[-1].response
    t1, t2 = gateway.store.memory[r1.id][2].text, gateway.store.memory[r2.id][2].text
    assert t1 == t2 == "Reading it."


def test_log_reports_real_ttfb(app_client, upstream, caplog):
    caplog.set_level("INFO", logger="midir")
    upstream.add(Reply("hello", delay=0.05))
    chat(app_client)
    ttfb = [r.getMessage() for r in caplog.records if " ttfb " in r.getMessage()]
    assert ttfb and float(ttfb[-1].split("ttfb ")[1].rstrip("s")) >= 0.04


def test_ordered_action_reported_as_pending_gets_a_follow_up(app_client, upstream):
    """Hermes on flex (battery 2026-10-03): '... Todos os 16 testes passaram. Pronto para commit. O que falta: apenas o commit.'"""
    msgs = [{"role": "user", "content": "Adicione median e pstdev, rode os testes até passar e faça um commit ao final."}]
    final = ("Implementado median e pstdev em calc/stats.py. Todos os 16 testes passaram. Pronto para commit. "
             "O que mudou: novas funções, CLI expandido. O que falta: apenas o commit.")
    upstream.add(final, tool_call_text("run_command", {"command": "git commit -am 'feat: median e pstdev'"}))
    r = chat(app_client, tools=CHAT_TOOLS, messages=msgs)
    assert r["choices"][0]["finish_reason"] == "tool_calls"
    assert "(commit)" in upstream.prompts[1]


def test_pending_wording_without_an_ordered_action_is_left_alone(app_client, upstream):
    upstream.add("Corrigi o bug e os testes passaram. Pronto para commit, se você quiser.", "x")
    chat(app_client, tools=CHAT_TOOLS, messages=[{"role": "user", "content": "corrija o bug"}])
    assert len(upstream.calls) == 1


def test_done_report_saying_nothing_is_pending_is_left_alone(app_client, upstream):
    msgs = [{"role": "user", "content": "Adicione median, rode os testes e faça um commit ao final."}]
    for final in ("Tudo implementado e commitado: median em stats.py, testes passando. Nada pendente.",
                  "Commit realizado. Pronto para revisão, nada mais a fazer.",
                  "Done: tests pass and the change is committed. Nothing left to do."):
        upstream.add(final)
        chat(app_client, tools=CHAT_TOOLS, messages=msgs)
    assert len(upstream.calls) == 3


def test_ignored_parameters_reported_once_per_client(app_client, upstream, caplog):
    """Hermes sends reasoning_effort on every request: one INFO line per client and set of parameters, then DEBUG only."""
    caplog.set_level("DEBUG", logger="midir")
    upstream.add("a", "b", "c")
    for _ in range(2):
        chat(app_client, reasoning_effort="medium")
    chat(app_client, reasoning_effort="medium", temperature=0.2)
    info = [r for r in caplog.records if "without effect" in r.getMessage() and r.levelname == "INFO"]
    debug = [r for r in caplog.records if "without effect" in r.getMessage() and r.levelname == "DEBUG"]
    warnings = [r for r in caplog.records if "without effect" in r.getMessage() and r.levelname == "WARNING"]
    assert len(info) == 2 and len(debug) == 1 and not warnings  # second set of parameters is new: reported once too
