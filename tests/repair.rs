//! Invalid tool-call JSON and the repair follow-up (bug seen with Hermes on 2026-10-03: a salvaged call with one extra
//! "}" still triggered a repair, whose reply re-emitted every call of the turn, double-escaped; the client got 8 writes).
//! Also the parser's salvage paths: several calls in one block, broken objects with valid arguments, chunk splits.

mod common;

use common::*;
use serde_json::{json, Value};

fn write_tool() -> Value {
    json!([{"type": "function", "function": {"name": "write_file", "description": "Write a file", "parameters": {"type": "object", "properties": {"path": {"type": "string"}, "content": {"type": "string"}}, "required": ["path", "content"]}}}])
}

fn block(obj_text: &str, cid: &str) -> String {
    format!("<tool_call id=\"{cid}\">\n{obj_text}\n</tool_call>")
}

fn w(path: &str, content: &str) -> String {
    json!({"name": "write_file", "arguments": {"path": path, "content": content}}).to_string()
}

struct Turn {
    calls: Vec<Value>,
    finish: Vec<String>,
    text: String,
}

impl Turn {
    fn args(&self) -> Vec<Value> {
        self.calls.iter().map(|c| serde_json::from_str(s(&c["function"]["arguments"])).unwrap()).collect()
    }
    fn paths(&self) -> Vec<String> {
        self.args().iter().map(|a| s(&a["path"]).to_string()).collect()
    }
}

fn stream_chat(rig: &Rig, replies: &[Reply]) -> Turn {
    for r in replies {
        rig.upstream.add(r.clone());
    }
    let r = rig.http.post(
        "/v1/chat/completions",
        &json!({"model": "gpt-5.1", "stream": true, "tools": write_tool(), "messages": user("write the files")}),
    );
    let objs = r.objects();
    Turn { calls: chat_stream_tool_calls(&objs), finish: chat_stream_finish(&objs), text: r.text }
}

fn hermes_turn() -> String {
    [
        block(&w("a.py", "print(1)\n"), "call_1"),
        block(&w("b.py", "print(2+3)\n"), "call_2"),
        block(&w("c.py", "x = 1\n"), "call_3"),
        block(&(w("notes.txt", "done\n") + "}"), "call_4"),
    ]
    .join("\n")
}

const BROKEN_D: &str = r#"{"name": "write_file", "arguments": {"path": "d.py", "content": "print("x")"}}"#;

#[test]
fn trailing_extra_brace_is_salvaged_without_repair() {
    let rig = Rig::new();
    let t = stream_chat(&rig, &[hermes_turn().into()]);
    assert_eq!(t.paths(), vec!["a.py", "b.py", "c.py", "notes.txt"]);
    assert_eq!(t.args()[3]["content"], "done\n");
    assert_eq!(rig.upstream.calls().len(), 1); // no repair
    assert_eq!(t.finish, vec!["tool_calls"]);
}

#[test]
fn repair_asks_only_for_the_broken_block() {
    let rig = Rig::new();
    let first = format!("{}\n{}", block(&w("a.py", "1"), "call_1"), block(BROKEN_D, "call_2"));
    let t = stream_chat(&rig, &[first.into(), block(&w("d.py", "print(\"x\")"), "call_1").into()]);
    assert_eq!(t.paths(), vec!["a.py", "d.py"]);
    let repair_prompt = rig.upstream.prompt(1);
    let tail = repair_prompt.split_once("could not be parsed as JSON").unwrap().1;
    assert!(tail.contains("\"d.py\"") && !tail.to_lowercase().contains("escape"), "{tail}");
    assert_eq!(t.finish, vec!["tool_calls"]);
}

#[test]
fn repair_reply_that_re_emits_every_call_is_deduplicated() {
    let rig = Rig::new();
    let first = format!("{}\n{}", block(&w("a.py", "1"), "call_1"), block(BROKEN_D, "call_2"));
    let everything = format!("{}\n{}", block(&w("a.py", "1"), "call_1"), block(&w("d.py", "print(\"x\")"), "call_2"));
    let t = stream_chat(&rig, &[first.into(), everything.into()]);
    assert_eq!(t.paths(), vec!["a.py", "d.py"]);
    let ids: Vec<&str> = t.calls.iter().map(|c| s(&c["id"])).collect();
    let mut unique = ids.clone();
    unique.dedup();
    assert_eq!(unique.len(), ids.len());
    assert_eq!(t.calls.iter().map(|c| n(&c["index"])).collect::<Vec<_>>(), vec![0, 1]);
    assert_eq!(t.finish, vec!["tool_calls"]);
    assert!(t.text.trim_end().ends_with("[DONE]"));
}

#[test]
fn double_escaped_re_emission_of_a_streamed_target_is_dropped() {
    let rig = Rig::new();
    let broken = r#"{"name": "write_file", "arguments": {"path": "e.py", "content": "say("hi")"}}"#;
    let first = format!("{}\n{}", block(&w("a.py", "print(2+3)\n"), "call_1"), block(broken, "call_2"));
    let reply =
        [block(&w("a.py", "print(2+3)\\n"), "call_1"), block(&w("e.py", "say(\"hi\")"), "call_2"), block(&w("f.py", "extra"), "call_3")]
            .join("\n");
    let t = stream_chat(&rig, &[first.into(), reply.into()]);
    // a.py re-emitted double-escaped: dropped; f.py beyond the 1 requested: dropped
    assert_eq!(t.paths(), vec!["a.py", "e.py"]);
    assert_eq!(t.args()[0]["content"], "print(2+3)\n");
}

#[test]
fn second_edit_of_the_same_file_can_be_repaired() {
    // The duplicate rule must not eat a legitimate repair that targets a file already written in this turn.
    let rig = Rig::new();
    let broken = r#"{"name": "write_file", "arguments": {"path": "a.py", "content": "v2 "quoted""}}"#;
    let first = format!("{}\n{}", block(&w("a.py", "v1"), "call_1"), block(broken, "call_2"));
    let t = stream_chat(&rig, &[first.into(), block(&w("a.py", "v2 \"quoted\""), "call_1").into()]);
    assert_eq!(t.args().iter().map(|a| s(&a["content"]).to_string()).collect::<Vec<_>>(), vec!["v1", "v2 \"quoted\""]);
}

// ---------------------------------------------------------------- parser salvage paths

#[test]
fn several_calls_in_one_block() {
    let rig = Rig::new();
    let body = format!("[{}, {}]", w("a.py", "1"), w("b.py", "2"));
    let t = stream_chat(&rig, &[block(&body, "call_1").into()]);
    assert_eq!(t.paths(), vec!["a.py", "b.py"]);
    assert_eq!(rig.upstream.calls().len(), 1);
}

#[test]
fn streaming_split_across_chunks() {
    let rig = Rig::new();
    let text = format!("Writing.\n{}", block(&w("a.py", "print(1)\n"), "call_1"));
    for size in [1, 2, 5, 13] {
        rig.upstream.clear();
        let t = stream_chat(&rig, &[Reply::text(&text).chunk(size)]);
        assert_eq!(t.paths(), vec!["a.py"], "chunk {size}");
        assert_eq!(t.args()[0]["content"], "print(1)\n", "chunk {size}");
    }
}

#[test]
fn truly_broken_json_drops_the_call_without_crashing() {
    let rig = Rig::new();
    let t = stream_chat(&rig, &[block("{not json at all", "call_1").into(), block("{still broken", "call_1").into()]);
    assert!(t.calls.is_empty());
    assert!(t.text.trim_end().ends_with("[DONE]"));
    assert_eq!(rig.upstream.calls().len(), 2); // one repair, then give up
}

#[test]
fn arguments_are_always_a_json_object() {
    let rig = Rig::new();
    let t =
        stream_chat(&rig, &[block(r#"{"name": "write_file", "arguments": "{\"path\": \"s.py\", \"content\": \"x\"}"}"#, "call_1").into()]);
    assert_eq!(t.calls.len(), 1);
    assert!(t.args()[0].is_object());
    assert_eq!(t.args()[0]["path"], "s.py");
}

#[test]
fn tool_call_ids_and_finish_reason_in_every_protocol() {
    let rig = Rig::new();
    let text = block(&w("a.py", "1"), "call_1");
    rig.upstream.add(text.as_str()).add(text.as_str());
    let r = rig.http.post("/v1/responses", &json!({"model": "gpt-5.1", "input": "x", "tools": [{"type": "function", "name": "write_file", "parameters": {"type": "object"}}]})).json();
    assert_eq!(r["output"][0]["type"], "function_call");
    assert!(s(&r["output"][0]["call_id"]).starts_with("call_"));
    let m = rig.http.post("/v1/messages", &json!({"model": "gpt-5.1", "max_tokens": 50, "messages": user("x"), "tools": [{"name": "write_file", "input_schema": {"type": "object"}}]})).json();
    assert_eq!(m["stop_reason"], "tool_use");
    assert!(s(&m["content"][0]["id"]).starts_with("toolu_"));
}
