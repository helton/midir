//! Client input: what each protocol accepts and how it reaches the backend's prompt (legacy fields, media, built-in
//! tools, refusals), malformed input answered with 400 (never 500), and text that keeps its place around tool results.

mod common;

use common::*;
use serde_json::{json, Value};

fn chat_prompt(rig: &Rig, body: Value) -> String {
    rig.upstream.clear();
    rig.upstream.add("ok");
    let r = rig.http.post("/v1/chat/completions", &body);
    assert_eq!(r.status, 200, "{}", r.text);
    rig.upstream.prompt(0)
}

fn messages_prompt(rig: &Rig, body: Value) -> String {
    rig.upstream.clear();
    rig.upstream.add("ok");
    let r = rig.http.post("/v1/messages", &body);
    assert_eq!(r.status, 200, "{}", r.text);
    rig.upstream.prompt(0)
}

// ---------------------------------------------------------------- chat completions

#[test]
fn chat_stop_string_and_legacy_functions() {
    let rig = Rig::new();
    rig.upstream.add("abcENDdef");
    let r = rig
        .http
        .post("/v1/chat/completions", &json!({"model": "m", "messages": user("x"), "stop": "END", "functions": [{"name": "g", "description": "the g tool"}], "function_call": {"name": "g"}}))
        .json();
    assert_eq!(r["choices"][0]["message"]["content"], "abc");
    let p = rig.upstream.prompt(0);
    assert!(p.contains("\"g\"") && p.contains("MUST call the tool `g`"), "{p}");
}

#[test]
fn chat_response_format_text_is_plain() {
    let rig = Rig::new();
    let p = chat_prompt(&rig, json!({"model": "m", "messages": user("x"), "response_format": {"type": "text"}}));
    assert!(!p.to_lowercase().contains("json"), "{p}");
}

#[test]
fn chat_media_becomes_placeholder_and_builtin_tools_are_ignored() {
    let rig = Rig::new();
    let p = chat_prompt(
        &rig,
        json!({"model": "m", "messages": [{"role": "user", "content": [{"type": "text", "text": "see"}, {"type": "image_url", "image_url": {"url": "data:..."}}]}], "tools": [{"type": "web_search"}]}),
    );
    assert!(p.contains("see") && p.contains("[image_url omitted"), "{p}");
    assert!(!p.contains("# Tools"), "{p}");
}

#[test]
fn chat_refusals_are_400() {
    let rig = Rig::new();
    for body in [
        json!({"model": "m", "messages": user("x"), "n": 2}),
        json!({"model": "m", "messages": user("x"), "logprobs": true}),
        json!({"model": "m", "messages": []}),
        json!({"model": "m"}),
    ] {
        let r = rig.http.post("/v1/chat/completions", &body);
        assert_eq!(r.status, 400, "{body}: {}", r.text);
        assert!(r.json()["error"]["message"].is_string());
    }
}

#[test]
fn chat_null_description_and_empty_arguments() {
    let rig = Rig::new();
    let p = chat_prompt(
        &rig,
        json!({"model": "m", "messages": [{"role": "user", "content": "x"}, {"role": "assistant", "tool_calls": [{"id": "c", "function": {"name": "f", "arguments": ""}}]}, {"role": "tool", "tool_call_id": "c", "content": "r"}],
               "tools": [{"type": "function", "function": {"name": "f", "description": null, "parameters": null}}]}),
    );
    assert!(p.contains("\"name\": \"f\", \"arguments\": {}") || p.contains("\"arguments\": {}"), "{p}");
}

#[test]
fn chat_empty_assistant_message_is_not_a_turn() {
    // Hermes (#82924) can send an assistant message with empty content and no tool calls.
    let rig = Rig::new();
    let p = chat_prompt(
        &rig,
        json!({"model": "m", "messages": [{"role": "user", "content": "a"}, {"role": "assistant", "content": ""}, {"role": "user", "content": "b"}]}),
    );
    assert!(!p.contains("[assistant]: \n") && !p.trim_end().ends_with("[assistant]:"), "{p}");
}

#[test]
fn tool_with_null_description_does_not_crash_the_incapacity_check() {
    let rig = Rig::new();
    rig.upstream.add("Não tenho acesso à internet.");
    let tools = json!([{"type": "function", "function": {"name": "web_fetch", "description": null, "parameters": {"type": "object", "properties": {"url": {"type": "string"}}}}}]);
    let r = rig.http.post("/v1/chat/completions", &json!({"model": "m", "messages": user("x"), "tools": tools}));
    assert_eq!(r.status, 200);
}

// ---------------------------------------------------------------- responses

#[test]
fn responses_unknown_item_is_400() {
    let rig = Rig::new();
    let r = rig.http.post("/v1/responses", &json!({"model": "m", "input": [{"type": "computer_call_output"}]}));
    assert_eq!(r.status, 400);
}

#[test]
fn responses_instructions_and_function_call_history() {
    let rig = Rig::new();
    rig.upstream.add("ok");
    let input = json!([
        {"role": "user", "content": "read it"},
        {"type": "function_call", "call_id": "call_7", "name": "read_file", "arguments": "{\"path\": \"a.py\"}"},
        {"type": "function_call_output", "call_id": "call_7", "output": "print(7)"},
    ]);
    let r = rig
        .http
        .post("/v1/responses", &json!({"model": "m", "instructions": "BE BRIEF", "input": input, "tools": resp_tools(), "store": false}));
    assert_eq!(r.status, 200);
    let p = rig.upstream.prompt(0);
    assert!(
        p.contains("BE BRIEF")
            && p.contains("<tool_call id=\"call_7\">")
            && p.contains("<tool_result id=\"call_7\" name=\"read_file\">\nprint(7)\n</tool_result>"),
        "{p}"
    );
}

// ---------------------------------------------------------------- anthropic messages

#[test]
fn messages_full_request() {
    let rig = Rig::new();
    let body = json!({
        "model": "m", "system": [{"type": "text", "text": "SYSTEM-S", "cache_control": {"type": "ephemeral"}}], "max_tokens": 10, "stop_sequences": ["END"],
        "messages": [
            {"role": "user", "content": [{"type": "text", "text": "go"}, {"type": "image", "source": {}}]},
            {"role": "assistant", "content": [{"type": "thinking", "thinking": "hmm-secret"}, {"type": "text", "text": "ok"}, {"type": "tool_use", "id": "toolu_1", "name": "f", "input": {"a": 1}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "res", "is_error": true}]},
        ],
        "tools": [{"name": "f", "input_schema": {"type": "object"}}, {"type": "web_search_20250305", "name": "web_search"}], "tool_choice": {"type": "any"},
    });
    let p = messages_prompt(&rig, body);
    assert!(p.contains("SYSTEM-S") && p.contains("[image omitted") && !p.contains("hmm-secret"), "{p}");
    assert!(p.contains("MUST call at least one tool"), "{p}"); // tool_choice any = required
    assert!(p.contains("<tool_result id=\"toolu_1\" name=\"f\""), "{p}");
}

#[test]
fn messages_stop_sequences_as_a_string_is_one_sequence() {
    let rig = Rig::new();
    rig.upstream.add("abcENDdef");
    let r = rig.http.post("/v1/messages", &json!({"model": "m", "max_tokens": 50, "messages": user("x"), "stop_sequences": "END"})).json();
    assert_eq!(r["content"][0]["text"], "abc");
    assert_eq!(r["stop_sequence"], "END");
}

#[test]
fn messages_tool_choice() {
    let rig = Rig::new();
    let tools = json!([{"name": "f", "input_schema": {"type": "object"}}]);
    let cases = [
        (json!({"type": "tool", "name": "f"}), Some("MUST call the tool `f`")),
        (json!({"type": "tool"}), Some("MUST call at least one tool")),
        (json!({"type": "none"}), None),
        (json!({"type": "auto"}), None),
        (Value::Null, None),
    ];
    for (choice, expected) in cases {
        let mut body = json!({"model": "m", "max_tokens": 10, "messages": user("x"), "tools": tools});
        if !choice.is_null() {
            body["tool_choice"] = choice.clone();
        }
        let p = messages_prompt(&rig, body);
        assert!(!p.contains("`None`") && !p.contains("`null`"), "{choice}: {p}");
        match expected {
            Some(text) => assert!(p.contains(text), "{choice}: {p}"),
            None => assert!(!p.contains("In this response you MUST"), "{choice}: {p}"),
        }
        if choice["type"] == "none" {
            assert!(!p.contains("# Tools"), "{p}");
        }
    }
}

// ---------------------------------------------------------------- malformed input: always a 400, never a 500

#[test]
fn malformed_requests_are_400() {
    let rig = Rig::new();
    let cases = [
        ("/v1/chat/completions", json!({"model": "m", "messages": ["hello"]})),
        ("/v1/chat/completions", json!({"model": "m", "messages": [{"role": "user", "content": "x"}], "tools": ["f"]})),
        (
            "/v1/chat/completions",
            json!({"model": "m", "messages": [{"role": "user", "content": "x"}], "tools": [{"type": "function", "function": "f"}]}),
        ),
        ("/v1/chat/completions", json!({"model": "m", "messages": [{"role": "assistant", "tool_calls": ["x"]}]})),
        ("/v1/chat/completions", json!({"model": "m", "messages": [{"role": "user", "content": "x"}], "stop": 5})),
        ("/v1/chat/completions", json!({"model": "m", "messages": [{"role": "user", "content": "x"}], "max_tokens": "ten"})),
        ("/v1/responses", json!({"model": "m", "input": [{"type": "message", "role": "user", "content": "x"}], "tools": ["f"]})),
        ("/v1/responses", json!({"model": "m", "input": 5})),
        ("/v1/responses", json!({"model": "m", "input": "x", "text": "json"})),
        ("/v1/messages", json!({"model": "m", "max_tokens": 5, "messages": ["hello"]})),
        ("/v1/messages", json!({"model": "m", "max_tokens": 5, "messages": [{"role": "user", "content": ["x"]}]})),
        ("/v1/messages", json!({"model": "m", "max_tokens": 5, "messages": [{"role": "user", "content": "x"}], "tools": [null]})),
        ("/v1/messages", json!({"model": "m", "max_tokens": 5, "messages": [{"role": "user", "content": "x"}], "system": 7})),
        ("/v1/messages/count_tokens", json!({"model": "m", "messages": ["x"]})),
    ];
    for (path, body) in cases {
        let r = rig.http.post(path, &body);
        assert_eq!(r.status, 400, "{path} {body}: {}", r.text);
        let err = r.json();
        assert!(err["error"]["message"].is_string() || err["error"]["type"].is_string(), "{}", r.text);
    }
}

// ---------------------------------------------------------------- order of text around tool results

#[test]
fn text_after_tool_results_stays_after_them() {
    // OpenClaw appends a runtime-context user message after the tool messages; it must not jump ahead of the results.
    let rig = Rig::new();
    let p = chat_prompt(
        &rig,
        json!({"model": "m", "messages": [
            {"role": "user", "content": "go"},
            {"role": "assistant", "tool_calls": [{"id": "c1", "function": {"name": "read", "arguments": "{}"}}]},
            {"role": "tool", "tool_call_id": "c1", "content": "RESULT"},
            {"role": "user", "content": [{"type": "text", "text": "<<<BEGIN_OPENCLAW_INTERNAL_CONTEXT>>>ctx<<<END_OPENCLAW_INTERNAL_CONTEXT>>>"}]},
        ]}),
    );
    assert!(p.find("RESULT").unwrap() < p.find("BEGIN_OPENCLAW_INTERNAL_CONTEXT").unwrap(), "{p}");
}

#[test]
fn anthropic_text_block_after_tool_result_stays_after() {
    let rig = Rig::new();
    let p = messages_prompt(
        &rig,
        json!({"model": "m", "max_tokens": 10, "messages": [
            {"role": "user", "content": "go"},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "t1", "name": "f", "input": {}}]},
            {"role": "user", "content": [{"type": "text", "text": "BEFORE"}, {"type": "tool_result", "tool_use_id": "t1", "content": "RESULT"}, {"type": "text", "text": "<system-reminder>AFTER</system-reminder>"}]},
        ]}),
    );
    let (b, r, a) = (p.find("BEFORE").unwrap(), p.find("RESULT").unwrap(), p.find("AFTER").unwrap());
    assert!(b < r && r < a, "{p}");
}
