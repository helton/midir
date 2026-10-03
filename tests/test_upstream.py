"""StackSpot side: token, retries and backoff, error mapping per protocol, odd SSE streams, routing by model."""
from __future__ import annotations

import json

import pytest

from conftest import Reply, sse_events
from midir.emulation.parser import ToolCallParser

USER = [{"role": "user", "content": "hi"}]
RATE = {"type": "TooManyRequests", "code": "INFERENCE_3008_CHAT_RATE_LIMIT_EXCEEDED", "details": "Maximum number of requests reached."}
TOO_LONG = {"type": "BadRequestError", "code": "INFERENCE_6001_LLM_MODEL_BAD_REQUEST", "details": "Input tokens exceed the configured limit of 272000 tokens. Your messages resulted in 340000 tokens. Please reduce the length of the messages."}


def chat(app_client, **kw):
    return app_client.post("/v1/chat/completions", json={"model": "gpt-5.1", "messages": USER, **kw})


# ---------------------------------------------------------------- request shape and token

def test_upstream_request_body_and_token_reuse(app_client, upstream):
    for _ in range(3):
        assert chat(app_client).status_code == 200
    assert upstream.token_calls == 1
    c = upstream.calls[0]
    assert c["auth"] == "Bearer tok1"
    assert c["body"]["streaming"] is True and c["body"]["stackspot_knowledge"] is False and c["body"]["return_ks_in_response"] is False


def test_401_from_agent_renews_token_once(app_client, upstream):
    upstream.add(Reply(status=401, body={"message": "expired"}), "after renewal")
    r = chat(app_client)
    assert r.json()["choices"][0]["message"]["content"] == "after renewal"
    assert upstream.token_calls == 2 and upstream.calls[1]["auth"] == "Bearer tok2"


def test_idm_failure_is_401_to_the_client(app_client, upstream):
    upstream.token_status = 401
    r = chat(app_client)
    assert r.status_code == 401 and "idm" in r.json()["error"]["message"]


# ---------------------------------------------------------------- retries

def test_retry_backoff_is_1_2_4_seconds(app_client, upstream, sleeps):
    upstream.add(*[Reply(status=503, body={"message": "busy"})] * 3, "finally")
    r = chat(app_client)
    assert r.json()["choices"][0]["message"]["content"] == "finally"
    assert sleeps == [1, 2, 4]


def test_persistent_5xx_is_502_with_the_upstream_message(app_client, upstream, sleeps):
    upstream.default = Reply(status=500, body={"message": "boom"})
    r = chat(app_client)
    assert r.status_code == 502 and "boom" in r.json()["error"]["message"]
    assert len(upstream.calls) == 4
    a = app_client.post("/v1/messages", json={"model": "gpt-5.1", "max_tokens": 5, "messages": USER})
    assert a.status_code == 502 and a.json()["error"]["type"] == "api_error"


def test_persistent_429_is_429_with_retry_after(app_client, upstream, sleeps):
    upstream.default = Reply(status=429, body=RATE)
    r = chat(app_client)
    assert r.status_code == 429 and "INFERENCE_3008" in r.json()["error"]["message"]
    assert int(r.headers["retry-after"]) > 0
    a = app_client.post("/v1/messages", json={"model": "gpt-5.1", "max_tokens": 5, "messages": USER})
    assert a.status_code == 429 and a.json()["error"]["type"] == "rate_limit_error" and "retry-after" in a.headers


def test_400_is_not_retried(app_client, upstream, sleeps):
    upstream.add(Reply(status=400, body={"message": "bad"}))
    assert chat(app_client).status_code == 400
    assert len(upstream.calls) == 1 and sleeps == []


def test_input_too_long_is_retried_once_with_a_smaller_prompt(app_client, upstream):
    msgs = []
    for i in range(20):
        msgs += [{"role": "user", "content": f"q{i} " + "y" * 3000}, {"role": "assistant", "content": f"a{i}"}]
    msgs.append({"role": "user", "content": "final question"})
    upstream.add(Reply(status=400, body=TOO_LONG), "short answer")
    r = app_client.post("/v1/chat/completions", json={"model": "gpt-5.1", "messages": msgs})
    assert r.status_code == 200 and r.json()["choices"][0]["message"]["content"] == "short answer"
    first, second = upstream.prompts
    assert len(second) < len(first) * 272000 / 340000 and second.rstrip().endswith("final question")


def test_input_too_long_twice_is_a_clear_400(app_client, upstream):
    upstream.default = Reply(status=400, body=TOO_LONG)
    r = chat(app_client)
    assert r.status_code == 400 and "272000" in r.json()["error"]["message"]
    assert len(upstream.calls) == 2


# ---------------------------------------------------------------- odd streams

@pytest.mark.parametrize("flavor,path,body", [
    ("openai", "/v1/chat/completions", {"model": "gpt-5.1", "messages": USER, "stream": True}),
    ("responses", "/v1/responses", {"model": "gpt-5.1", "input": "hi", "stream": True}),
    ("anthropic", "/v1/messages", {"model": "gpt-5.1", "max_tokens": 9, "messages": USER, "stream": True}),
])
def test_connection_drop_mid_stream_becomes_an_error_event(app_client, upstream, flavor, path, body):
    upstream.add(Reply("partial text that never ends", break_after=2))
    r = app_client.post(path, json=body)
    evs = sse_events(r.text)
    assert r.status_code == 200
    if flavor == "anthropic":
        assert evs[-1][0] == "error"
    elif flavor == "responses":
        assert evs[-1][0] == "error" and evs[-1][1]["type"] == "error"
    else:
        assert evs[-1][1] == "[DONE]" and "error" in evs[-2][1]


def test_non_text_message_fields_and_non_json_lines_are_ignored(app_client, upstream):
    upstream.add(Reply(events=["not json", {"message": {"nested": 1}}, {"message": ["a"]}, [1, 2], 7, {"message": "real "}, {"message": None}, {"message": "text"},
                               {"stop_reason": "stop", "tokens": {"input": 10, "output": 2}, "message_id": "m1"}]))
    r = chat(app_client)
    assert r.status_code == 200 and r.json()["choices"][0]["message"]["content"] == "real text"


def test_stream_without_final_event_estimates_usage(app_client, upstream):
    upstream.add(Reply(events=[{"message": "hello world"}]))
    u = chat(app_client).json()["usage"]
    assert u["prompt_tokens"] > 0 and u["completion_tokens"] > 0


def test_final_event_with_string_tokens(app_client, upstream):
    upstream.add(Reply("x", final={"stop_reason": "stop", "tokens": {"input": "12", "output": None}}))
    u = chat(app_client).json()["usage"]
    assert u["prompt_tokens"] == 12


def test_crash_inside_a_stream_is_an_error_event_not_a_dropped_connection(app_client, upstream, monkeypatch):
    def boom(*a, **k):
        raise RuntimeError("unexpected")
    monkeypatch.setattr(ToolCallParser, "feed", boom)
    upstream.add("text")
    r = app_client.post("/v1/messages", json={"model": "gpt-5.1", "max_tokens": 9, "messages": USER, "stream": True})
    assert sse_events(r.text)[-1][0] == "error"


# ---------------------------------------------------------------- routing

@pytest.mark.parametrize("model,agent", [("gpt-5.1", "AGENT51"), ("GPT-4.1", "AGENT41"), ("claude-opus-4-5", "AGENT51"), ("claude-haiku-4-5", "AGENT41"),
                                         ("claude-sonnet-4-6", "AGENTFLEX"), ("openai/gpt-4.1", "AGENT41"), ("stackspot-flex", "AGENTFLEX"),
                                         ("my-gpt-4.1-test", "AGENT41"), ("something-else", "AGENT51"), ("", "AGENT51")])
def test_routing(app_client, upstream, model, agent):
    app_client.post("/v1/chat/completions", json={"model": model, "messages": USER})
    assert upstream.calls[-1]["agent"] == agent


# ---------------------------------------------------------------- readiness

def test_ready_checks_the_token_without_agent_calls(app_client, upstream):
    r = app_client.get("/ready")
    assert r.status_code == 200 and r.json()["ok"] and upstream.calls == []
    app_client.get("/ready")
    assert upstream.token_calls == 1  # cached


def test_ready_reports_bad_credentials(app_client, upstream):
    upstream.token_status = 401
    r = app_client.get("/ready")
    assert r.status_code == 503 and "idm" in r.json()["error"]
