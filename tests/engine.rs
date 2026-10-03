//! Engine behavior through the HTTP API: stop/max_tokens cuts, JSON mode, automatic follow-ups (promise, incapacity,
//! redundant confirmation, pending ordered action, tool_choice), assistant prefill, prompt shape.

mod common;

use common::*;
use serde_json::{json, Value};

const ASSISTANT_PREFILL_NUDGE: &str = "(continue your previous message exactly from where it stopped, without repeating it)";

fn task() -> Value {
    user("fix the bug in a.py and run the tests")
}

/// POST /v1/chat/completions with `extra` merged into the default body; the JSON body (or the SSE objects).
fn chat(rig: &Rig, extra: Value) -> Value {
    let mut body = json!({"model": "gpt-5.1", "messages": task()});
    for (k, v) in extra.as_object().unwrap() {
        body[k] = v.clone();
    }
    let r = rig.http.post("/v1/chat/completions", &body);
    assert_eq!(r.status, 200, "{}", r.text);
    if body["stream"] == true {
        Value::Array(r.objects())
    } else {
        r.json()
    }
}

fn content(r: &Value) -> &str {
    s(&r["choices"][0]["message"]["content"])
}

// ---------------------------------------------------------------- stop sequences and max_tokens

#[test]
fn stop_sequence_cut_keeps_real_usage() {
    let rig = Rig::new();
    rig.upstream.add(Reply::text("one two END three four").chunk(3));
    let r = chat(&rig, json!({"stop": ["END"]}));
    assert_eq!(content(&r), "one two ");
    assert_eq!(r["choices"][0]["finish_reason"], "stop");
    assert!(n(&r["usage"]["prompt_tokens"]) > 0 && n(&r["usage"]["completion_tokens"]) > 0);
    // the final event was cut off: estimated
}

#[test]
fn max_tokens_cut_in_every_protocol() {
    let rig = Rig::new();
    let long = "word ".repeat(200);
    rig.upstream.add(long.as_str()).add(long.as_str()).add(long.as_str());
    let c = chat(&rig, json!({"max_tokens": 5}));
    assert_eq!(c["choices"][0]["finish_reason"], "length");
    assert!(content(&c).len() <= 20);
    assert!(n(&c["usage"]["completion_tokens"]) > 0);
    let rs = rig.http.post("/v1/responses", &json!({"model": "gpt-5.1", "input": "x", "max_output_tokens": 5})).json();
    assert_eq!(rs["status"], "incomplete");
    assert_eq!(rs["incomplete_details"], json!({"reason": "max_output_tokens"}));
    let ms = rig.http.post("/v1/messages", &json!({"model": "gpt-5.1", "max_tokens": 5, "messages": task()})).json();
    assert_eq!(ms["stop_reason"], "max_tokens");
    assert!(n(&ms["usage"]["output_tokens"]) > 0);
}

#[test]
fn anthropic_stop_sequence_reported() {
    let rig = Rig::new();
    rig.upstream.add("alpha STOP beta");
    let ms = rig
        .http
        .post("/v1/messages", &json!({"model": "gpt-5.1", "max_tokens": 50, "messages": task(), "stop_sequences": ["STOP"]}))
        .json();
    assert_eq!(ms["stop_reason"], "stop_sequence");
    assert_eq!(ms["stop_sequence"], "STOP");
    assert_eq!(ms["content"][0]["text"], "alpha ");
}

#[test]
fn stop_split_across_chunks() {
    let rig = Rig::new();
    rig.upstream.add(Reply::text("abcXYZdef").chunk(4));
    assert_eq!(content(&chat(&rig, json!({"stop": ["XYZ"]}))), "abc");
}

// ---------------------------------------------------------------- JSON mode

#[test]
fn json_mode_valid_and_fenced() {
    let rig = Rig::new();
    rig.upstream.add("```json\n{\"a\": 1}\n```");
    let r = chat(&rig, json!({"response_format": {"type": "json_object"}}));
    assert_eq!(serde_json::from_str::<Value>(content(&r)).unwrap(), json!({"a": 1}));
    assert_eq!(rig.upstream.calls().len(), 1);
}

#[test]
fn json_mode_repair() {
    let rig = Rig::new();
    let schema = json!({"type": "object", "required": ["a"], "properties": {"a": {"type": "integer"}}});
    rig.upstream.add("{\"a\": \"x\"}").add("{\"a\": 2}");
    let r = chat(&rig, json!({"response_format": {"type": "json_schema", "json_schema": {"name": "s", "schema": schema}}}));
    assert_eq!(serde_json::from_str::<Value>(content(&r)).unwrap(), json!({"a": 2}));
    assert!(rig.upstream.prompt(1).contains("expected integer"));
}

#[test]
fn json_mode_with_required_tool_choice_does_not_retry_for_a_tool() {
    let rig = Rig::new();
    rig.upstream.add("{\"a\": 1}");
    chat(&rig, json!({"response_format": {"type": "json_object"}, "tools": chat_tools(), "tool_choice": "required"}));
    assert_eq!(rig.upstream.calls().len(), 1);
}

#[test]
fn json_mode_streaming_is_buffered() {
    let rig = Rig::new();
    rig.upstream.add("{\"ok\": true}");
    let objs = chat(&rig, json!({"response_format": {"type": "json_object"}, "stream": true}));
    let text = chat_stream_text(objs.as_array().unwrap());
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), json!({"ok": true}));
}

// ---------------------------------------------------------------- follow-ups

#[test]
fn promise_follow_up_appends_the_calls() {
    let rig = Rig::new();
    rig.upstream.add("Vou ler o arquivo a.py agora.").add(tool_call_text("read_file", json!({"path": "a.py"})));
    let r = chat(&rig, json!({"tools": chat_tools()}));
    let m = &r["choices"][0]["message"];
    assert_eq!(r["choices"][0]["finish_reason"], "tool_calls");
    assert_eq!(m["tool_calls"][0]["function"]["name"], "read_file");
    assert!(s(&m["content"]).contains("Vou ler"));
    assert!(rig.upstream.prompt(1).contains("You announced an action"));
    let expected: i64 = rig.upstream.prompts().iter().map(|p| (p.chars().count() / 4) as i64).sum();
    assert_eq!(n(&r["usage"]["prompt_tokens"]), expected);
}

#[test]
fn promise_follow_up_streaming() {
    let rig = Rig::new();
    rig.upstream.add("Let me check the tests.").add(tool_call_text("run_command", json!({"command": "pytest"})));
    let objs = chat(&rig, json!({"tools": chat_tools(), "stream": true}));
    let objs = objs.as_array().unwrap();
    assert_eq!(chat_stream_finish(objs), vec!["tool_calls"]);
    assert_eq!(chat_stream_tool_calls(objs).iter().map(|c| n(&c["index"])).collect::<Vec<_>>(), vec![0]);
}

#[test]
fn no_follow_up_for_a_final_report_or_a_question() {
    let rig = Rig::new();
    rig.upstream.add("Corrigi o bug e todos os testes passaram.").add("Qual arquivo você quer que eu leia?");
    chat(&rig, json!({"tools": chat_tools()}));
    chat(&rig, json!({"tools": chat_tools()}));
    assert_eq!(rig.upstream.calls().len(), 2);
}

#[test]
fn false_incapacity_follow_up() {
    let rig = Rig::new();
    let tools = json!([{"type": "function", "function": {"name": "web_fetch", "description": "Fetch a URL", "parameters": {"type": "object", "properties": {"url": {"type": "string"}}}}}]);
    rig.upstream.add("I don't have access to the internet.").add(tool_call_text("web_fetch", json!({"url": "https://example.com"})));
    let r = chat(&rig, json!({"tools": tools}));
    assert_eq!(r["choices"][0]["message"]["tool_calls"][0]["function"]["name"], "web_fetch");
    assert!(rig.upstream.prompt(1).contains("web_fetch"));
}

#[test]
fn redundant_confirmation_follow_up() {
    let rig = Rig::new();
    rig.upstream
        .add("Pronto para commit. Deseja que eu faça o commit?")
        .add(tool_call_text("run_command", json!({"command": "git commit -am fix"})));
    let r = chat(&rig, json!({"tools": chat_tools(), "messages": user("corrija o bug e faça um commit ao final")}));
    assert_eq!(r["choices"][0]["finish_reason"], "tool_calls");
    assert!(rig.upstream.prompt(1).contains("do not ask for confirmation"));
}

#[test]
fn tool_choice_none_disables_protocol_and_follow_ups() {
    let rig = Rig::new();
    rig.upstream.add("Vou ler o arquivo agora.");
    chat(&rig, json!({"tools": chat_tools(), "tool_choice": "none"}));
    assert_eq!(rig.upstream.calls().len(), 1);
    assert!(!rig.upstream.prompt(0).contains("# Tools"));
}

#[test]
fn tool_choice_required_retry_has_no_empty_assistant_turn() {
    let rig = Rig::new();
    rig.upstream.add("").add(tool_call_text("read_file", json!({"path": "a.py"})));
    let r = chat(&rig, json!({"tools": chat_tools(), "tool_choice": "required"}));
    assert_eq!(r["choices"][0]["finish_reason"], "tool_calls");
    let p = rig.upstream.prompt(1);
    assert!(!p.contains("[assistant]: \n") && !p.contains("[assistant]: <"));
    assert!(p.contains("MUST respond with a <tool_call>"));
}

#[test]
fn named_tool_choice_in_prompt() {
    let rig = Rig::new();
    rig.upstream.add(tool_call_text("run_command", json!({"command": "ls"})));
    chat(&rig, json!({"tools": chat_tools(), "tool_choice": {"type": "function", "function": {"name": "run_command"}}}));
    assert!(rig.upstream.prompt(0).contains("MUST call the tool `run_command`"));
}

// ---------------------------------------------------------------- prompt shape

#[test]
fn assistant_prefill_gets_a_continue_nudge() {
    let rig = Rig::new();
    rig.upstream.add("rest");
    let mut msgs = task().as_array().unwrap().clone();
    msgs.push(json!({"role": "assistant", "content": "The answer is"}));
    rig.http.post("/v1/messages", &json!({"model": "gpt-5.1", "max_tokens": 50, "messages": msgs}));
    assert!(rig.upstream.prompt(0).trim_end().ends_with(ASSISTANT_PREFILL_NUDGE));
}

#[test]
fn tail_reminder_only_with_tools() {
    let rig = Rig::new();
    rig.upstream.add("a").add("b");
    chat(&rig, json!({}));
    chat(&rig, json!({"tools": chat_tools()}));
    assert!(!rig.upstream.prompt(0).contains("<reminder>"));
    assert!(rig.upstream.prompt(1).trim_end().ends_with("</reminder>"));
}

#[test]
fn system_prompt_history_and_tools_are_rendered() {
    let rig = Rig::new();
    rig.upstream.add("ok");
    let msgs = json!([
        {"role": "system", "content": "You are terse."},
        {"role": "user", "content": "first"},
        {"role": "assistant", "content": "", "tool_calls": [{"id": "call_9", "type": "function", "function": {"name": "read_file", "arguments": "{\"path\": \"a.py\"}"}}]},
        {"role": "tool", "tool_call_id": "call_9", "content": "print(1)"},
        {"role": "user", "content": "second"},
    ]);
    chat(&rig, json!({"messages": msgs, "tools": chat_tools()}));
    let p = rig.upstream.prompt(0);
    assert!(p.contains("You are terse."));
    assert!(p.contains("# Tools") && p.contains("\"read_file\"") && p.contains("\"run_command\""));
    assert!(
        p.contains("<tool_call id=\"call_9\">") && p.contains("<tool_result id=\"call_9\" name=\"read_file\">\nprint(1)\n</tool_result>")
    );
    let (i1, i2) = (p.find("first").unwrap(), p.find("second").unwrap());
    assert!(i1 < i2);
}

// ---------------------------------------------------------------- ordered actions left pending

#[test]
fn ordered_action_reported_as_pending_gets_a_follow_up() {
    // Hermes on flex (battery 2026-10-03): '... Todos os 16 testes passaram. Pronto para commit. O que falta: apenas o commit.'
    let rig = Rig::new();
    let final_text = "Implementado median e pstdev em calc/stats.py. Todos os 16 testes passaram. Pronto para commit. O que mudou: novas funções, CLI expandido. O que falta: apenas o commit.";
    rig.upstream.add(final_text).add(tool_call_text("run_command", json!({"command": "git commit -am 'feat: median e pstdev'"})));
    let r = chat(
        &rig,
        json!({"tools": chat_tools(), "messages": user("Adicione median e pstdev, rode os testes até passar e faça um commit ao final.")}),
    );
    assert_eq!(r["choices"][0]["finish_reason"], "tool_calls");
    assert!(rig.upstream.prompt(1).contains("(commit)"));
}

#[test]
fn pending_wording_without_an_ordered_action_is_left_alone() {
    let rig = Rig::new();
    rig.upstream.add("Corrigi o bug e os testes passaram. Pronto para commit, se você quiser.").add("x");
    chat(&rig, json!({"tools": chat_tools(), "messages": user("corrija o bug")}));
    assert_eq!(rig.upstream.calls().len(), 1);
}

#[test]
fn done_report_saying_nothing_is_pending_is_left_alone() {
    let rig = Rig::new();
    let msgs = user("Adicione median, rode os testes e faça um commit ao final.");
    for final_text in [
        "Tudo implementado e commitado: median em stats.py, testes passando. Nada pendente.",
        "Commit realizado. Pronto para revisão, nada mais a fazer.",
        "Done: tests pass and the change is committed. Nothing left to do.",
    ] {
        rig.upstream.add(final_text);
        chat(&rig, json!({"tools": chat_tools(), "messages": msgs}));
    }
    assert_eq!(rig.upstream.calls().len(), 3);
}

#[test]
fn ignored_parameters_are_accepted() {
    let rig = Rig::new();
    rig.upstream.add("ok");
    let r = rig.http.post_with_headers(
        "/v1/chat/completions",
        &json!({"model": "gpt-5.1", "messages": task(), "temperature": 0, "top_p": 0.9, "seed": 7, "reasoning_effort": "high", "metadata": {"a": "b"}}),
        &[("user-agent", "hermes-agent/1.0")],
    );
    assert_eq!(r.status, 200);
    let log = rig.server.logs();
    assert!(log.contains("accepted parameters without effect") && log.contains("temperature"), "{log}");
}
