//! The three protocols end to end: streaming and not, text and tool calls, ids, finish reasons, usage, the shapes the
//! official SDKs parse.

mod common;

use common::*;
use serde_json::{Value, json};

fn two_calls() -> String {
    format!(
        "Reading both.\n{}\n{}",
        tool_call_text("read_file", json!({"path": "a.py"})),
        tool_call_text_id("read_file", json!({"path": "b.py"}), "call_2")
    )
}

fn messages() -> Value {
    user("read a.py and b.py")
}

// ---------------------------------------------------------------- chat completions

#[test]
fn chat_text_non_stream() {
    let rig = Rig::new();
    rig.upstream.add("Hello there.");
    let r = rig.http.post("/v1/chat/completions", &json!({"model": "gpt-5.1", "messages": messages()})).json();
    assert_eq!(r["choices"][0]["message"]["content"], "Hello there.");
    assert_eq!(r["choices"][0]["finish_reason"], "stop");
    let u = &r["usage"];
    assert!(n(&u["prompt_tokens"]) > 0 && n(&u["completion_tokens"]) > 0);
    assert_eq!(n(&u["total_tokens"]), n(&u["prompt_tokens"]) + n(&u["completion_tokens"]));
    assert_eq!(r["model"], "gpt-5.1");
    assert!(s(&r["id"]).starts_with("chatcmpl-"));
    assert_eq!(r["object"], "chat.completion");
}

#[test]
fn chat_tool_calls_non_stream() {
    let rig = Rig::new();
    rig.upstream.add(two_calls());
    let r = rig.http.post("/v1/chat/completions", &json!({"model": "gpt-5.1", "messages": messages(), "tools": chat_tools()})).json();
    let m = &r["choices"][0]["message"];
    assert_eq!(r["choices"][0]["finish_reason"], "tool_calls");
    let calls = m["tool_calls"].as_array().unwrap();
    let args: Vec<Value> = calls.iter().map(|c| serde_json::from_str(s(&c["function"]["arguments"])).unwrap()).collect();
    assert_eq!(args, vec![json!({"path": "a.py"}), json!({"path": "b.py"})]);
    assert_eq!(m["content"], "Reading both.");
    assert!(calls.iter().all(|c| c["type"] == "function" && s(&c["id"]).starts_with("call_")));
    assert_ne!(calls[0]["id"], calls[1]["id"]);
}

#[test]
fn chat_stream_text_and_tools() {
    let rig = Rig::new();
    rig.upstream.add(two_calls());
    let r = rig.http.post("/v1/chat/completions", &json!({"model": "gpt-5.1", "messages": messages(), "tools": chat_tools(), "stream": true, "stream_options": {"include_usage": true}}));
    assert!(r.is_sse());
    let objs = r.objects();
    assert_eq!(chat_stream_text(&objs).trim(), "Reading both.");
    let calls = chat_stream_tool_calls(&objs);
    assert_eq!(calls.iter().map(|c| n(&c["index"])).collect::<Vec<_>>(), vec![0, 1]);
    assert_eq!(chat_stream_finish(&objs), vec!["tool_calls"]);
    let usage: Vec<&Value> = objs.iter().filter(|d| d["usage"].is_object()).collect();
    assert!(!usage.is_empty() && n(&usage.last().unwrap()["usage"]["prompt_tokens"]) > 0);
    assert!(objs.iter().all(|d| d["object"] == "chat.completion.chunk"));
    assert_eq!(r.events().last().unwrap().1, "[DONE]");
}

#[test]
fn chat_stream_usage_has_the_same_shape_as_non_stream() {
    let rig = Rig::new();
    rig.upstream.add("hi").add("hi");
    let body = json!({"model": "gpt-5.1", "messages": messages()});
    let plain = rig.http.post("/v1/chat/completions", &body).json()["usage"].clone();
    let mut streamed = body.clone();
    streamed["stream"] = json!(true);
    streamed["stream_options"] = json!({"include_usage": true});
    let objs = rig.http.post("/v1/chat/completions", &streamed).objects();
    let last = objs.iter().rev().find(|d| d["usage"].is_object()).unwrap();
    let keys = |v: &Value| v.as_object().unwrap().keys().cloned().collect::<std::collections::BTreeSet<_>>();
    assert_eq!(keys(&last["usage"]), keys(&plain));
}

#[test]
fn chat_stream_usage_is_a_last_chunk_without_choices() {
    // as OpenAI streams it: only with include_usage, every other chunk says "usage": null
    let rig = Rig::new();
    rig.upstream.add("hi");
    let objs = rig
        .http
        .post(
            "/v1/chat/completions",
            &json!({"model": "gpt-5.1", "messages": messages(), "stream": true, "stream_options": {"include_usage": true}}),
        )
        .objects();
    let (last, rest) = objs.split_last().unwrap();
    assert_eq!(last["choices"], json!([]));
    assert!(n(&last["usage"]["total_tokens"]) > 0);
    assert!(rest.iter().all(|d| d["usage"].is_null() && d.as_object().unwrap().contains_key("usage")));
    assert_eq!(chat_stream_finish(rest), vec!["stop"]);
}

#[test]
fn chat_stream_without_include_usage() {
    let rig = Rig::new();
    rig.upstream.add("hi");
    let r = rig.http.post(
        "/v1/chat/completions",
        &json!({"model": "gpt-5.1", "messages": messages(), "stream": true, "stream_options": {"include_usage": false}}),
    );
    let evs = r.events();
    assert_eq!(evs.last().unwrap().1, "[DONE]");
    assert!(!evs.iter().any(|(_, d)| d["usage"].is_object()));
}

// ---------------------------------------------------------------- responses

#[test]
fn responses_text_non_stream() {
    let rig = Rig::new();
    rig.upstream.add("Plain answer.");
    let r = rig.http.post("/v1/responses", &json!({"model": "gpt-4.1", "input": "hi"})).json();
    assert_eq!(r["output"][0]["type"], "message");
    assert_eq!(r["output"][0]["content"][0]["text"], "Plain answer.");
    assert_eq!(r["status"], "completed");
    assert!(n(&r["usage"]["input_tokens"]) > 0);
    assert!(s(&r["id"]).starts_with("resp_"));
    assert_eq!(rig.upstream.calls()[0].agent, "AGENT41");
}

#[test]
fn responses_tool_calls_non_stream() {
    let rig = Rig::new();
    rig.upstream.add(two_calls());
    let r = rig.http.post("/v1/responses", &json!({"model": "gpt-5.1", "input": "read both", "tools": resp_tools()})).json();
    let kinds: Vec<&str> = r["output"].as_array().unwrap().iter().map(|o| s(&o["type"])).collect();
    assert_eq!(kinds, vec!["message", "function_call", "function_call"]);
    let args: Vec<Value> = r["output"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|o| o["type"] == "function_call")
        .map(|o| serde_json::from_str(s(&o["arguments"])).unwrap())
        .collect();
    assert_eq!(args, vec![json!({"path": "a.py"}), json!({"path": "b.py"})]);
    for o in r["output"].as_array().unwrap().iter().filter(|o| o["type"] == "function_call") {
        assert!(s(&o["call_id"]).starts_with("call_") && s(&o["id"]).starts_with("fc_"));
    }
}

#[test]
fn responses_stream_item_identity_text_tool_text() {
    // A message that resumes after a tool call is a new item: ids never repeat across output items (OpenClaw aborts otherwise).
    let rig = Rig::new();
    rig.upstream.add(format!("Before.\n{}\nAfter the call.", tool_call_text("read_file", json!({"path": "a.py"}))));
    let evs = rig.http.post("/v1/responses", &json!({"model": "gpt-5.1", "input": "x", "tools": resp_tools(), "stream": true})).events();
    let of = |t: &str| evs.iter().filter(|(_, d)| d["type"] == t).map(|(_, d)| d.clone()).collect::<Vec<_>>();
    let added = of("response.output_item.added");
    let done = of("response.output_item.done");
    assert_eq!(added.iter().map(|e| s(&e["item"]["type"])).collect::<Vec<_>>(), vec!["message", "function_call", "message"]);
    let ids: std::collections::BTreeSet<&str> = added.iter().map(|e| s(&e["item"]["id"])).collect();
    assert_eq!(ids.len(), 3);
    assert_eq!(added.iter().map(|e| s(&e["item"]["id"])).collect::<Vec<_>>(), done.iter().map(|e| s(&e["item"]["id"])).collect::<Vec<_>>());
    for e in of("response.output_text.delta") {
        assert!(matches!(n(&e["output_index"]), 0 | 2));
    }
    let (name, last) = evs.last().unwrap();
    assert_eq!(last["type"], "response.completed");
    assert_eq!(name.as_deref(), Some("response.completed"));
    assert_eq!(
        last["response"]["output"].as_array().unwrap().iter().map(|o| s(&o["id"])).collect::<Vec<_>>(),
        done.iter().map(|e| s(&e["item"]["id"])).collect::<Vec<_>>()
    );
    let seq: Vec<i64> = evs.iter().map(|(_, d)| n(&d["sequence_number"])).collect();
    let mut sorted = seq.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(seq, sorted);
}

#[test]
fn responses_custom_tool() {
    let rig = Rig::new();
    let tools = json!([{"type": "custom", "name": "apply_patch", "description": "Apply a patch"}]);
    rig.upstream.add(tool_call_text("apply_patch", json!({"input": "*** Begin Patch\n*** End Patch"})));
    let r = rig.http.post("/v1/responses", &json!({"model": "gpt-5.1", "input": "patch it", "tools": tools})).json();
    let item = r["output"].as_array().unwrap().last().unwrap();
    assert_eq!(item["type"], "custom_tool_call");
    assert!(s(&item["input"]).starts_with("*** Begin Patch"));
}

#[test]
fn responses_previous_response_id_chain() {
    let rig = Rig::new();
    rig.upstream.add(tool_call_text("read_file", json!({"path": "a.py"}))).add("It prints 1.");
    let r1 = rig.http.post("/v1/responses", &json!({"model": "gpt-5.1", "input": "what does a.py print?", "tools": resp_tools()})).json();
    let call = r1["output"].as_array().unwrap().last().unwrap();
    let r2 = rig
        .http
        .post("/v1/responses", &json!({"model": "gpt-5.1", "previous_response_id": r1["id"], "input": [{"type": "function_call_output", "call_id": call["call_id"], "output": "print(1)"}]}))
        .json();
    assert_eq!(r2["output"][0]["content"][0]["text"], "It prints 1.");
    let p = rig.upstream.prompt(1);
    assert!(p.contains("what does a.py print?") && p.contains("print(1)") && p.contains("\"read_file\""));
    // history + tools inherited
}

#[test]
fn responses_get_stored() {
    let rig = Rig::new();
    rig.upstream.add("stored");
    let r = rig.http.post("/v1/responses", &json!({"model": "gpt-5.1", "input": "x"})).json();
    let got = rig.http.get(&format!("/v1/responses/{}", s(&r["id"])));
    assert_eq!(got.status, 200);
    assert_eq!(got.json()["output"][0]["content"][0]["text"], "stored");
}

#[test]
fn responses_unknown_previous_id_is_404() {
    let rig = Rig::new();
    let r = rig
        .http
        .post("/v1/responses", &json!({"model": "gpt-5.1", "previous_response_id": format!("resp_{}", "0".repeat(24)), "input": "x"}));
    assert_eq!(r.status, 404);
    assert!(r.json()["error"]["message"].is_string());
}

// ---------------------------------------------------------------- anthropic messages

#[test]
fn messages_text_non_stream() {
    let rig = Rig::new();
    rig.upstream.add("Olá.");
    let r = rig.http.post("/v1/messages", &json!({"model": "claude-haiku-4-5", "max_tokens": 100, "messages": messages()})).json();
    assert_eq!(r["content"][0]["text"], "Olá.");
    assert_eq!(r["stop_reason"], "end_turn");
    assert_eq!(r["type"], "message");
    assert_eq!(r["role"], "assistant");
    assert!(s(&r["id"]).starts_with("msg_"));
    assert_eq!(rig.upstream.calls()[0].agent, "AGENT41");
    assert!(n(&r["usage"]["input_tokens"]) > 0);
}

#[test]
fn messages_tool_use_non_stream() {
    let rig = Rig::new();
    rig.upstream.add(two_calls());
    let r = rig
        .http
        .post("/v1/messages", &json!({"model": "claude-opus-4-5", "max_tokens": 100, "messages": messages(), "tools": anth_tools()}))
        .json();
    assert_eq!(r["stop_reason"], "tool_use");
    let blocks = r["content"].as_array().unwrap();
    assert_eq!(blocks.iter().map(|b| s(&b["type"])).collect::<Vec<_>>(), vec!["text", "tool_use", "tool_use"]);
    assert!(blocks.iter().filter(|b| b["type"] == "tool_use").all(|b| s(&b["id"]).starts_with("toolu_") && b["input"].is_object()));
}

#[test]
fn messages_stream_blocks() {
    let rig = Rig::new();
    rig.upstream.add(two_calls());
    let evs = rig
        .http
        .post(
            "/v1/messages",
            &json!({"model": "claude-sonnet-4-6", "max_tokens": 100, "messages": messages(), "tools": anth_tools(), "stream": true}),
        )
        .events();
    let names: Vec<&str> = evs.iter().map(|(name, _)| name.as_deref().unwrap_or("")).collect();
    assert_eq!(names[0], "message_start");
    assert_eq!(*names.last().unwrap(), "message_stop");
    let starts: Vec<&Value> = evs.iter().filter(|(_, d)| d["type"] == "content_block_start").map(|(_, d)| d).collect();
    assert_eq!(starts.iter().map(|e| n(&e["index"])).collect::<Vec<_>>(), vec![0, 1, 2]);
    assert_eq!(starts.iter().map(|e| s(&e["content_block"]["type"])).collect::<Vec<_>>(), vec!["text", "tool_use", "tool_use"]);
    let stops = evs.iter().filter(|(_, d)| d["type"] == "content_block_stop").count();
    assert_eq!(stops, 3);
    // the tool input arrives as input_json_delta partial_json pieces that concatenate to the arguments
    let input: String = evs
        .iter()
        .filter(|(_, d)| d["type"] == "content_block_delta" && n(&d["index"]) == 1)
        .map(|(_, d)| s(&d["delta"]["partial_json"]).to_string())
        .collect();
    assert_eq!(serde_json::from_str::<Value>(&input).unwrap(), json!({"path": "a.py"}));
    let delta = evs.iter().find(|(_, d)| d["type"] == "message_delta").map(|(_, d)| d.clone()).unwrap();
    assert_eq!(delta["delta"]["stop_reason"], "tool_use");
    assert!(n(&delta["usage"]["output_tokens"]) > 0);
    assert_eq!(rig.upstream.calls()[0].agent, "AGENTFLEX"); // "sonnet" regex
}

#[test]
fn messages_tool_result_round_trip() {
    let rig = Rig::new();
    rig.upstream.add("Done.");
    let mut msgs = messages().as_array().unwrap().clone();
    msgs.push(
        json!({"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_1", "name": "read_file", "input": {"path": "a.py"}}]}),
    );
    msgs.push(json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": [{"type": "text", "text": "print(1)"}]}]}));
    let r = rig.http.post("/v1/messages", &json!({"model": "gpt-5.1", "max_tokens": 100, "messages": msgs, "tools": anth_tools()}));
    assert_eq!(r.status, 200);
    assert!(rig.upstream.prompt(0).contains("<tool_result id=\"toolu_1\" name=\"read_file\">\nprint(1)\n</tool_result>"));
}

#[test]
fn count_tokens() {
    let rig = Rig::new();
    let r = rig.http.post("/v1/messages/count_tokens", &json!({"model": "gpt-5.1", "messages": messages()})).json();
    assert!(n(&r["input_tokens"]) > 0);
}

// ---------------------------------------------------------------- misc endpoints

#[test]
fn models_and_health() {
    let rig = Rig::new();
    let models = rig.http.get("/v1/models").json();
    assert_eq!(models["data"].as_array().unwrap().iter().map(|m| s(&m["id"])).collect::<Vec<_>>(), vec!["gpt-5.1", "gpt-4.1", "flex"]);
    let h = rig.http.get("/health");
    assert_eq!(h.status, 200);
    let body = h.json();
    assert_eq!(body["ok"], true);
    assert_eq!(body["default"], "gpt-5.1");
    assert!(!h.text.contains("AGENT51")); // ids are abbreviated
}

#[test]
fn embeddings_is_a_clear_404() {
    let rig = Rig::new();
    let r = rig.http.post("/v1/embeddings", &json!({"model": "x", "input": "hi"}));
    assert_eq!(r.status, 404);
    assert!(s(&r.json()["error"]["message"]).contains("embeddings"));
}

#[test]
fn body_not_json_is_400() {
    let rig = Rig::new();
    for path in ["/v1/chat/completions", "/v1/responses", "/v1/messages"] {
        let r = rig.http.post_raw(path, b"{not json", "application/json");
        assert_eq!(r.status, 400, "{path}");
    }
}

// ---------------------------------------------------------------- what the official APIs do that clients rely on

#[test]
fn a_stored_response_answers_the_same_later() {
    // GET /v1/responses/{id}: the model asked for, the echoed settings and the same item ids
    let rig = Rig::new();
    rig.upstream.add(two_calls());
    let r = rig
        .http
        .post(
            "/v1/responses",
            &json!({"model": "gpt-4.1", "input": "x", "instructions": "be brief", "tools": resp_tools(), "temperature": 0.2}),
        )
        .json();
    let got = rig.http.get(&format!("/v1/responses/{}", s(&r["id"]))).json();
    for key in ["model", "instructions", "temperature", "tools", "output", "usage", "status"] {
        assert_eq!(got[key], r[key], "{key}");
    }
    assert_eq!(got["model"], "gpt-4.1");
    assert_eq!(got["instructions"], "be brief");
}

#[test]
fn responses_stream_cut_by_max_output_tokens_ends_incomplete() {
    let rig = Rig::new();
    rig.upstream.add("word ".repeat(100).as_str());
    let evs = rig.http.post("/v1/responses", &json!({"model": "gpt-5.1", "input": "x", "max_output_tokens": 5, "stream": true})).events();
    let (name, last) = evs.last().unwrap();
    assert_eq!(name.as_deref(), Some("response.incomplete"));
    assert_eq!(last["response"]["status"], "incomplete");
    assert_eq!(last["response"]["incomplete_details"]["reason"], "max_output_tokens");
}

#[test]
fn messages_stream_reports_input_tokens_from_the_start() {
    // clients track context use from message_start (the real count follows in message_delta)
    let rig = Rig::new();
    rig.upstream.add("hi");
    let r = rig.http.post("/v1/messages", &json!({"model": "gpt-5.1", "max_tokens": 9, "messages": messages(), "stream": true}));
    assert!(r.header("request-id").is_some_and(|id| id.starts_with("msg_")));
    let evs = r.events();
    assert_eq!(evs[0].0.as_deref(), Some("message_start"));
    assert!(n(&evs[0].1["message"]["usage"]["input_tokens"]) > 0, "{}", evs[0].1);
}

#[test]
fn models_in_the_anthropic_format_for_anthropic_clients() {
    let rig = Rig::new();
    let headers = [("anthropic-version", "2023-06-01")];
    let r = rig.http.get_with_headers("/v1/models", &headers).json();
    assert_eq!(r["has_more"], false);
    assert_eq!((r["first_id"].clone(), r["last_id"].clone()), (json!("gpt-5.1"), json!("flex")));
    let first = &r["data"][0];
    assert_eq!((first["type"].clone(), first["id"].clone()), (json!("model"), json!("gpt-5.1")));
    assert!(s(&first["created_at"]).ends_with('Z') && first["display_name"].is_string());
    let one = rig.http.get_with_headers("/v1/models/claude-opus-4-5", &headers).json();
    assert_eq!((one["type"].clone(), one["id"].clone()), (json!("model"), json!("claude-opus-4-5")));
    assert_eq!(rig.http.get("/v1/models").json()["object"], "list"); // OpenAI clients get the OpenAI format
}

#[test]
fn item_references_resolve_to_stored_output_items() {
    let rig = Rig::new();
    rig.upstream.add(tool_call_text("read_file", json!({"path": "a.py"}))).add("It prints 1.");
    let r1 = rig.http.post("/v1/responses", &json!({"model": "gpt-5.1", "input": "what does a.py print?", "tools": resp_tools()})).json();
    let call = r1["output"].as_array().unwrap().last().unwrap().clone();
    let input = json!([
        {"role": "user", "content": "what does a.py print?"},
        {"type": "item_reference", "id": call["id"]},
        {"type": "function_call_output", "call_id": call["call_id"], "output": "print(1)"},
    ]);
    let r2 = rig.http.post("/v1/responses", &json!({"model": "gpt-5.1", "input": input, "tools": resp_tools()}));
    assert_eq!(r2.status, 200, "{}", r2.text);
    let p = rig.upstream.prompt(1);
    assert!(p.contains(&format!("<tool_call id=\"{}\">", s(&call["call_id"]))) && p.contains("print(1)"), "{p}");
    let bad = rig.http.post("/v1/responses", &json!({"model": "gpt-5.1", "input": [{"type": "item_reference", "id": "msg_nope"}]}));
    assert_eq!(bad.status, 400);
    assert!(bad.text.contains("item_reference"), "{}", bad.text);
}

#[test]
fn a_call_keeps_its_id_whatever_text_comes_around_it() {
    // text, call, text: the call is the response's call 0 when streamed, stored and referenced
    let rig = Rig::new();
    rig.upstream.add(format!("Before.\n{}\nAfter the call.", tool_call_text("read_file", json!({"path": "a.py"})))).add("ok");
    let evs = rig.http.post("/v1/responses", &json!({"model": "gpt-5.1", "input": "x", "tools": resp_tools(), "stream": true})).events();
    let done = evs.last().unwrap().1["response"].clone();
    let streamed_call = done["output"].as_array().unwrap().iter().find(|o| o["type"] == "function_call").unwrap().clone();
    let got = rig.http.get(&format!("/v1/responses/{}", s(&done["id"]))).json();
    let stored_call = got["output"].as_array().unwrap().iter().find(|o| o["type"] == "function_call").unwrap().clone();
    assert_eq!(streamed_call["id"], stored_call["id"]);
    let input = json!([{"type": "item_reference", "id": streamed_call["id"]}, {"type": "function_call_output", "call_id": streamed_call["call_id"], "output": "print(1)"}]);
    assert_eq!(rig.http.post("/v1/responses", &json!({"model": "gpt-5.1", "input": input, "tools": resp_tools()})).status, 200);
}

#[test]
fn text_after_a_call_gives_the_same_items_streamed_and_stored() {
    // review 2026-10-05 (F10): the stream had msg 0, call 0, msg 1; the stored response one merged message, and an
    // item_reference to msg 1 was a 400
    let mut rig = Rig::new();
    let reply = format!("Reading.\n{}\nThen I will summarize the result.", tool_call_text("read_file", json!({"path": "a.py"})));
    rig.upstream.add(Reply::text(&reply).chunk(6));
    let evs = rig.http.post("/v1/responses", &json!({"model": "gpt-5.1", "input": "x", "tools": resp_tools(), "stream": true})).events();
    let done = evs.last().unwrap().1["response"].clone();
    let streamed: Vec<(String, String)> =
        done["output"].as_array().unwrap().iter().map(|o| (s(&o["type"]).to_string(), s(&o["id"]).to_string())).collect();
    assert_eq!(streamed.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(), ["message", "function_call", "message"]);
    let id = s(&done["id"]).to_string();
    for restart in [false, true] {
        if restart {
            rig.restart(); // the same from disk
        }
        let got = rig.http.get(&format!("/v1/responses/{id}")).json();
        let stored: Vec<(String, String)> =
            got["output"].as_array().unwrap().iter().map(|o| (s(&o["type"]).to_string(), s(&o["id"]).to_string())).collect();
        assert_eq!(stored, streamed, "restart={restart}");
        assert_eq!(got["output"][2]["content"][0]["text"], "Then I will summarize the result.");
    }
    rig.upstream.add("ok");
    let r = rig.http.post(
        "/v1/responses",
        &json!({"model": "gpt-5.1", "input": [{"type": "item_reference", "id": streamed[2].1}, {"role": "user", "content": "go on"}]}),
    );
    assert_eq!(r.status, 200, "{}", r.text);
    assert!(rig.upstream.prompt(1).contains("Then I will summarize the result."));
    // non-streaming: the same item order
    rig.upstream.add(reply.clone());
    let r = rig.http.post("/v1/responses", &json!({"model": "gpt-5.1", "input": "x", "tools": resp_tools()})).json();
    let types: Vec<&str> = r["output"].as_array().unwrap().iter().map(|o| s(&o["type"])).collect();
    assert_eq!(types, ["message", "function_call", "message"]);
}

#[test]
fn protocol_gaps_from_the_review() {
    let rig = Rig::new();
    // F43: Anthropic error bodies carry request_id
    let r = rig.http.post("/v1/messages", &json!({"model": "claude-opus-4-5", "max_tokens": 9, "messages": 5}));
    assert_eq!(r.status, 400);
    assert_eq!(r.json()["request_id"], json!(r.header("request-id").unwrap()));
    // F45: a forced tool choice with no tools is a 400, on every protocol, without a backend call
    for (path, body) in [
        ("/v1/chat/completions", json!({"model": "gpt-5.1", "messages": user("x"), "tool_choice": "required"})),
        ("/v1/responses", json!({"model": "gpt-5.1", "input": "x", "tool_choice": "required"})),
        ("/v1/messages", json!({"model": "claude-opus-4-5", "max_tokens": 9, "messages": user("x"), "tool_choice": {"type": "any"}})),
    ] {
        let r = rig.http.post(path, &body);
        assert_eq!(r.status, 400, "{path}: {}", r.text);
    }
    assert!(rig.upstream.calls().is_empty());
    // F46: `conversation` is refused (it would silently drop the server-side history); max_tool_calls is logged
    let r = rig.http.post("/v1/responses", &json!({"model": "gpt-5.1", "input": "x", "conversation": "conv_1"}));
    assert_eq!(r.status, 400);
    assert!(r.text.contains("conversation"));
    rig.upstream.add("ok");
    assert_eq!(rig.http.post("/v1/responses", &json!({"model": "gpt-5.1", "input": "x", "max_tool_calls": 3})).status, 200);
    assert!(rig.server.logs().contains("max_tool_calls"));
    // F47: GET /v1/models/{id} knows which ids exist
    assert_eq!(rig.http.get("/v1/models/definitely-not-a-model").status, 404);
    assert_eq!(rig.http.get("/v1/models/gpt-5.1").status, 200);
    assert_eq!(rig.http.get("/v1/models/claude-opus-4-5").status, 200); // an alias
    // F23: an empty reply is an empty content list for Anthropic clients
    rig.upstream.add("");
    let r = rig.http.post("/v1/messages", &json!({"model": "claude-opus-4-5", "max_tokens": 50, "messages": user("hi")})).json();
    assert_eq!(r["content"], json!([]));
}

#[test]
fn wrong_methods_and_trailing_slashes() {
    // review 2026-10-05 (F28): the 405 fallback and the path normalization had no black-box test
    let rig = Rig::new();
    let r = rig.http.get("/v1/chat/completions");
    assert_eq!(r.status, 405);
    assert_eq!(r.json()["error"]["code"], "method_not_allowed");
    let r = rig.http.get_with_headers("/v1/messages", &[("anthropic-version", "2023-06-01")]);
    assert_eq!(r.status, 405);
    assert_eq!(r.json()["type"], "error"); // in the Anthropic shape
    rig.upstream.add("ok");
    let r = rig.http.post("/v1/chat/completions/", &json!({"model": "gpt-5.1", "messages": user("hi")}));
    assert_eq!(r.status, 200, "{}", r.text);
    assert_eq!(rig.http.get("/health/").status, 200);
}
