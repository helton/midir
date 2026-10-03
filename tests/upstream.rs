//! StackSpot side: token, retries and backoff, error mapping per protocol, odd SSE streams, routing by model, readiness.

mod common;

use common::*;
use serde_json::{json, Value};

fn rate() -> Value {
    json!({"type": "TooManyRequests", "code": "INFERENCE_3008_CHAT_RATE_LIMIT_EXCEEDED", "details": "Maximum number of requests reached."})
}

fn too_long() -> Value {
    json!({"type": "BadRequestError", "code": "INFERENCE_6001_LLM_MODEL_BAD_REQUEST", "details": "Input tokens exceed the configured limit of 272000 tokens. Your messages resulted in 340000 tokens. Please reduce the length of the messages."})
}

fn chat(rig: &Rig) -> Resp {
    rig.http.post("/v1/chat/completions", &json!({"model": "gpt-5.1", "messages": user("hi")}))
}

fn messages(rig: &Rig) -> Resp {
    rig.http.post("/v1/messages", &json!({"model": "gpt-5.1", "max_tokens": 5, "messages": user("hi")}))
}

// ---------------------------------------------------------------- request shape and token

#[test]
fn upstream_request_body_and_token_reuse() {
    let rig = Rig::new();
    for _ in 0..3 {
        assert_eq!(chat(&rig).status, 200);
    }
    assert_eq!(rig.upstream.token_calls(), 1);
    let c = &rig.upstream.calls()[0];
    assert_eq!(c.auth.as_deref(), Some("Bearer tok1"));
    assert_eq!(c.body["streaming"], true);
    assert_eq!(c.body["stackspot_knowledge"], false);
    assert_eq!(c.body["return_ks_in_response"], false);
    assert_eq!(c.agent, "AGENT51");
}

#[test]
fn token_is_renewed_when_it_expires() {
    let rig = Rig::new();
    rig.upstream.set_expires_in(1); // renewed ahead of the expiry margin
    assert_eq!(chat(&rig).status, 200);
    assert_eq!(chat(&rig).status, 200);
    assert_eq!(rig.upstream.token_calls(), 2);
}

#[test]
fn a_401_from_the_agent_renews_the_token_once() {
    let rig = Rig::new();
    rig.upstream.add(Reply::status(401, json!({"message": "expired"}))).add("after renewal");
    let r = chat(&rig);
    assert_eq!(r.json()["choices"][0]["message"]["content"], "after renewal");
    assert_eq!(rig.upstream.token_calls(), 2);
    assert_eq!(rig.upstream.calls()[1].auth.as_deref(), Some("Bearer tok2"));
}

#[test]
fn idm_failure_is_401_to_the_client() {
    let rig = Rig::new();
    rig.upstream.set_token_status(401);
    let r = chat(&rig);
    assert_eq!(r.status, 401);
    assert!(s(&r.json()["error"]["message"]).contains("idm"));
}

// ---------------------------------------------------------------- retries

#[test]
fn persistent_5xx_is_502_with_the_upstream_message() {
    let rig = Rig::new();
    rig.upstream.set_default(Reply::status(500, json!({"message": "boom"})));
    let r = chat(&rig);
    assert_eq!(r.status, 502);
    assert!(s(&r.json()["error"]["message"]).contains("boom"));
    assert_eq!(rig.upstream.calls().len(), 4); // one call + 3 retries
    let a = messages(&rig);
    assert_eq!(a.status, 502);
    assert_eq!(a.json()["error"]["type"], "api_error");
}

#[test]
fn persistent_429_is_429_with_retry_after() {
    let rig = Rig::new();
    rig.upstream.set_default(Reply::status(429, rate()));
    let r = chat(&rig);
    assert_eq!(r.status, 429);
    assert!(s(&r.json()["error"]["message"]).contains("INFERENCE_3008"));
    assert!(r.header("retry-after").unwrap().parse::<i64>().unwrap() > 0);
    let a = messages(&rig);
    assert_eq!(a.status, 429);
    assert_eq!(a.json()["error"]["type"], "rate_limit_error");
    assert!(a.header("retry-after").is_some());
}

#[test]
fn a_400_is_not_retried() {
    let rig = Rig::new();
    rig.upstream.add(Reply::status(400, json!({"message": "bad"})));
    assert_eq!(chat(&rig).status, 400);
    assert_eq!(rig.upstream.calls().len(), 1);
}

#[test]
fn input_too_long_is_retried_once_with_a_smaller_prompt() {
    let rig = Rig::new();
    let mut msgs = vec![];
    for i in 0..20 {
        msgs.push(json!({"role": "user", "content": format!("q{i} {}", "y".repeat(3000))}));
        msgs.push(json!({"role": "assistant", "content": format!("a{i}")}));
    }
    msgs.push(json!({"role": "user", "content": "final question"}));
    rig.upstream.add(Reply::status(400, too_long())).add("short answer");
    let r = rig.http.post("/v1/chat/completions", &json!({"model": "gpt-5.1", "messages": msgs}));
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["choices"][0]["message"]["content"], "short answer");
    let prompts = rig.upstream.prompts();
    let (first, second) = (&prompts[0], &prompts[1]);
    assert!((second.chars().count() as f64) < first.chars().count() as f64 * 272000.0 / 340000.0);
    assert!(second.trim_end().ends_with("final question"));
}

#[test]
fn input_too_long_twice_is_a_clear_400() {
    let rig = Rig::new();
    rig.upstream.set_default(Reply::status(400, too_long()));
    let r = chat(&rig);
    assert_eq!(r.status, 400);
    assert!(s(&r.json()["error"]["message"]).contains("272000"));
    assert_eq!(rig.upstream.calls().len(), 2);
}

#[test]
fn retry_backoff_follows_the_configured_base() {
    // MIDIR_RETRY_BACKOFF_S=0.001 in the harness: three retries take milliseconds, not the default 1 + 2 + 4 s
    let rig = Rig::new();
    rig.upstream.set_default(Reply::status(500, json!({"message": "boom"})));
    let t0 = std::time::Instant::now();
    assert_eq!(chat(&rig).status, 502);
    assert!(t0.elapsed().as_secs_f64() < 2.0);
    assert_eq!(rig.upstream.calls().len(), 4);
}

// ---------------------------------------------------------------- odd streams

#[test]
fn connection_drop_mid_stream_becomes_an_error_event() {
    let rig = Rig::new();
    let cases = [
        ("openai", "/v1/chat/completions", json!({"model": "gpt-5.1", "messages": user("hi"), "stream": true})),
        ("responses", "/v1/responses", json!({"model": "gpt-5.1", "input": "hi", "stream": true})),
        ("anthropic", "/v1/messages", json!({"model": "gpt-5.1", "max_tokens": 9, "messages": user("hi"), "stream": true})),
    ];
    for (flavor, path, body) in cases {
        rig.upstream.add(Reply::text("partial text that never ends").break_after(2));
        let r = rig.http.post(path, &body);
        assert_eq!(r.status, 200, "{flavor}");
        let evs = r.events();
        let (last_name, last) = evs.last().unwrap();
        match flavor {
            "anthropic" => assert_eq!(last_name.as_deref(), Some("error"), "{flavor}: {}", r.text),
            "responses" => {
                assert_eq!(last_name.as_deref(), Some("error"), "{flavor}: {}", r.text);
                assert_eq!(last["type"], "error");
            }
            _ => {
                assert_eq!(*last, "[DONE]", "{flavor}: {}", r.text);
                assert!(evs[evs.len() - 2].1["error"].is_object(), "{flavor}: {}", r.text);
            }
        }
    }
}

#[test]
fn non_text_message_fields_and_non_json_lines_are_ignored() {
    let rig = Rig::new();
    rig.upstream.add(Reply::events(vec![
        json!("not json"),
        json!({"message": {"nested": 1}}),
        json!({"message": ["a"]}),
        json!([1, 2]),
        json!(7),
        json!({"message": "real "}),
        json!({"message": null}),
        json!({"message": "text"}),
        json!({"stop_reason": "stop", "tokens": {"input": 10, "output": 2}, "message_id": "m1"}),
    ]));
    let r = chat(&rig);
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["choices"][0]["message"]["content"], "real text");
}

#[test]
fn stream_without_final_event_estimates_usage() {
    let rig = Rig::new();
    rig.upstream.add(Reply::events(vec![json!({"message": "hello world"})]));
    let u = chat(&rig).json()["usage"].clone();
    assert!(n(&u["prompt_tokens"]) > 0 && n(&u["completion_tokens"]) > 0);
}

#[test]
fn final_event_with_string_tokens() {
    let rig = Rig::new();
    rig.upstream.add(Reply::text("x").final_event(json!({"stop_reason": "stop", "tokens": {"input": "12", "output": null}})));
    let u = chat(&rig).json()["usage"].clone();
    assert_eq!(n(&u["prompt_tokens"]), 12);
}

#[test]
fn raw_sse_with_lf_only_and_split_data_lines() {
    let rig = Rig::new();
    rig.upstream.add(Reply::raw("data: {\"message\": \"ab\"}\n\ndata: {\"message\": \"cd\"}\n\ndata: {\"stop_reason\": \"stop\", \"tokens\": {\"input\": 3, \"output\": 1}}\n\n"));
    let r = chat(&rig);
    assert_eq!(r.json()["choices"][0]["message"]["content"], "abcd");
}

// ---------------------------------------------------------------- routing

#[test]
fn routing() {
    let rig = Rig::new();
    let cases = [
        ("gpt-5.1", "AGENT51"),
        ("GPT-4.1", "AGENT41"),
        ("claude-opus-4-5", "AGENT51"),
        ("claude-haiku-4-5", "AGENT41"),
        ("claude-sonnet-4-6", "AGENTFLEX"),
        ("openai/gpt-4.1", "AGENT41"),
        ("stackspot-flex", "AGENTFLEX"),
        ("my-gpt-4.1-test", "AGENT41"),
        ("something-else", "AGENT51"),
        ("", "AGENT51"),
    ];
    for (model, agent) in cases {
        rig.http.post("/v1/chat/completions", &json!({"model": model, "messages": user("hi")}));
        assert_eq!(rig.upstream.calls().last().unwrap().agent, agent, "{model}");
    }
}

// ---------------------------------------------------------------- readiness

#[test]
fn ready_checks_the_token_without_agent_calls() {
    let rig = Rig::new();
    let r = rig.http.get("/ready");
    assert_eq!(r.status, 200);
    assert_eq!(r.json()["ok"], true);
    assert!(rig.upstream.calls().is_empty());
    rig.http.get("/ready");
    assert_eq!(rig.upstream.token_calls(), 1); // cached
}

#[test]
fn ready_reports_bad_credentials() {
    let rig = Rig::new();
    rig.upstream.set_token_status(401);
    let r = rig.http.get("/ready");
    assert_eq!(r.status, 503);
    assert!(s(&r.json()["error"]).contains("idm"));
}

#[test]
fn health_reports_queue_state() {
    let rig = Rig::new();
    let q = rig.http.get("/health").json()["backends"]["stackspot"]["queue"].clone();
    for key in ["in_flight", "waiting", "starts_last_60s", "paused_s", "requests_per_minute", "effective_rpm"] {
        assert!(q.get(key).is_some(), "{key}");
    }
}
