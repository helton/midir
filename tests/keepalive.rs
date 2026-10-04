//! SSE keepalive whenever a stream is idle for keepalive_s: comments (OpenAI) or pings (Anthropic) while the backend
//! thinks and while a follow-up runs, none while content flows, and streams that stay valid.

mod common;

use common::*;
use serde_json::json;

const SLOW: f64 = 0.4; // backend silence, in seconds: about 8 keepalive intervals

fn fast() -> Rig {
    Rig::with(&toml_with_server("keepalive_s = 0.05"), &[])
}

fn split_at<'a>(text: &'a str, marker: &str) -> (&'a str, &'a str) {
    let i = text.find(marker).unwrap_or_else(|| panic!("{marker} not in {text}"));
    text.split_at(i)
}

#[test]
fn chat_keepalive_while_waiting_none_while_content_flows() {
    let rig = fast();
    rig.upstream.add(Reply::text("Hello there, this is the answer.").delay(SLOW));
    let r = rig.http.post("/v1/chat/completions", &json!({"model": "gpt-5.1", "messages": user("hi"), "stream": true}));
    let (before, after) = split_at(&r.text, "\"content\":\"Hello");
    assert!(before.matches(": keepalive").count() >= 2, "{before}");
    assert!(!after.contains(": keepalive"));
    assert!(r.text.trim_end().ends_with("data: [DONE]"));
    assert_eq!(chat_stream_text(&r.objects()), "Hello there, this is the answer.");
}

#[test]
fn responses_keepalive_while_waiting_none_while_content_flows() {
    let rig = fast();
    rig.upstream.add(Reply::text("Hello there.").delay(SLOW));
    let r = rig.http.post("/v1/responses", &json!({"model": "gpt-5.1", "input": "hi", "stream": true}));
    let (before, after) = split_at(&r.text, "response.output_text.delta");
    assert!(before.matches(": keepalive").count() >= 2 && !after.contains(": keepalive"));
    assert_eq!(r.events().last().unwrap().1["type"], "response.completed");
}

#[test]
fn messages_ping_while_waiting_none_while_content_flows() {
    let rig = fast();
    rig.upstream.add(Reply::text("Hello there.").delay(SLOW));
    let r = rig.http.post("/v1/messages", &json!({"model": "gpt-5.1", "max_tokens": 50, "messages": user("hi"), "stream": true}));
    let (before, after) = split_at(&r.text, "content_block_start");
    assert!(before.matches("event: ping").count() >= 3); // the one message_start always sends, then the keepalives
    assert!(!after.contains("event: ping"));
    assert_eq!(r.events().last().unwrap().0.as_deref(), Some("message_stop"));
}

#[test]
fn tool_calls_after_keepalives_are_intact() {
    let rig = fast();
    rig.upstream.add(Reply::text(&tool_call_text("read_file", json!({"path": "a.py"}))).delay(SLOW));
    let r = rig.http.post("/v1/responses", &json!({"model": "gpt-5.1", "input": "read a.py", "tools": resp_tools(), "stream": true}));
    assert!(r.text.contains(": keepalive"));
    let last = r.events().last().unwrap().1.clone();
    assert_eq!(last["type"], "response.completed");
    assert_eq!(last["response"]["output"].as_array().unwrap().last().unwrap()["type"], "function_call");
}

#[test]
fn json_mode_waits_with_keepalives() {
    let rig = fast();
    rig.upstream.add(Reply::text("{\"a\": 1}").delay(SLOW));
    let r = rig.http.post(
        "/v1/chat/completions",
        &json!({"model": "gpt-5.1", "messages": user("hi"), "stream": true, "response_format": {"type": "json_object"}}),
    );
    assert!(r.text.matches(": keepalive").count() >= 2);
    assert!(r.text.contains(r#"{\"a\":1}"#), "{}", r.text);
}

#[test]
fn fast_backend_gets_no_keepalive() {
    let rig = Rig::with(&toml_with_server("keepalive_s = 1"), &[]);
    rig.upstream.add("quick");
    let r = rig.http.post("/v1/chat/completions", &json!({"model": "gpt-5.1", "messages": user("hi"), "stream": true}));
    assert!(!r.text.contains(": keepalive"));
}

#[test]
fn keepalive_can_be_turned_off() {
    let rig = Rig::with(&toml_with_server("keepalive_s = 0"), &[]);
    rig.upstream.add(Reply::text("late").delay(0.2));
    let r = rig.http.post("/v1/chat/completions", &json!({"model": "gpt-5.1", "messages": user("hi"), "stream": true}));
    assert!(!r.text.contains(": keepalive"));
}

#[test]
fn keepalives_arrive_while_waiting_not_at_the_end() {
    // the first keepalive must reach the client long before the content (no buffering anywhere); the margins are wide
    // because test runners can be slow
    let rig = Rig::with(&toml_with_server("keepalive_s = 0.1"), &[]);
    rig.upstream.add(Reply::text("Hello.").delay(2.0));
    let lines = rig.http.post_lines("/v1/chat/completions", &json!({"model": "gpt-5.1", "messages": user("hi"), "stream": true}));
    let first_keepalive = lines.iter().find(|(_, l)| l.starts_with(": keepalive")).map(|(t, _)| *t).unwrap();
    let first_content = lines.iter().find(|(_, l)| l.contains("Hello")).map(|(t, _)| *t).unwrap();
    assert!(first_keepalive < 1.2 && first_content > 1.9, "keepalive at {first_keepalive}, content at {first_content}");
}

#[test]
fn keepalives_continue_while_a_follow_up_runs() {
    // the model announces an action and stops; the follow-up that asks for the call takes a while: the stream must
    // stay alive after the first text too
    let rig = fast();
    rig.upstream.add("Vou ler o arquivo a.py agora.").add(Reply::text(&tool_call_text("read_file", json!({"path": "a.py"}))).delay(SLOW));
    let r = rig
        .http
        .post("/v1/chat/completions", &json!({"model": "gpt-5.1", "messages": user("leia a.py"), "tools": chat_tools(), "stream": true}));
    let (_, after) = split_at(&r.text, "Vou ler");
    let (between, _) = split_at(after, "read_file");
    assert!(between.matches(": keepalive").count() >= 2, "{between}");
    assert_eq!(chat_stream_finish(&r.objects()), vec!["tool_calls"]);
}
