//! Replay of real client requests (Copilot, Claude Code, Codex, OpenCode, aider; 237 captured with an earlier version).
//! They hold real conversations, so they are kept out of the repository: point MIDIR_CAPTURES at the folder (one JSON
//! file per request: {"protocol": "chat"|"responses"|"messages", "body": {...}}) and run
//!
//!     MIDIR_CAPTURES=.internal/dev/captures cargo test --profile ci --test replay -- --ignored
//!
//! Every request goes through its protocol adapter, the prompt renderer and the response/stream writer with a text
//! reply and with a tool-call reply, streaming and not. Any 4xx/5xx or malformed SSE is a regression.

mod common;

use common::*;
use serde_json::{Value, json};

fn first_tool(body: &Value) -> Option<String> {
    for t in body["tools"].as_array().into_iter().flatten() {
        let name = t["name"].as_str().or_else(|| t["function"]["name"].as_str());
        let kind = t["type"].as_str().unwrap_or("function");
        if let Some(name) = name
            && (matches!(kind, "function" | "custom") || t.get("input_schema").is_some())
        {
            return Some(name.to_string());
        }
    }
    None
}

#[test]
#[ignore = "needs the local captures: MIDIR_CAPTURES=<dir> cargo test --profile ci --test replay -- --ignored"]
fn captured_requests_replay_cleanly() {
    let dir = std::env::var("MIDIR_CAPTURES").expect("MIDIR_CAPTURES=<folder with the captures>");
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(dir);
    let mut paths: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "json").unwrap_or(false))
        .collect();
    paths.sort();
    assert!(!paths.is_empty(), "no captures in {}", dir.display());
    let rig = Rig::new();
    let mut failures = vec![];
    for path in &paths {
        let cap: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let endpoint = match s(&cap["protocol"]) {
            "chat" => "/v1/chat/completions",
            "responses" => "/v1/responses",
            "messages" => "/v1/messages",
            other => panic!("{}: protocol {other:?}", path.display()),
        };
        let mut body = cap["body"].clone();
        body.as_object_mut().unwrap().remove("previous_response_id"); // the captured chains are not in this store
        let reply = match first_tool(&body) {
            Some(tool) if body["tool_choice"] != "none" => format!("Working on it.\n{}", tool_call_text(&tool, json!({}))),
            _ => "plain answer".to_string(),
        };
        for stream in [false, true] {
            rig.upstream.add(reply.as_str());
            body["stream"] = json!(stream);
            let r = rig.http.post(endpoint, &body);
            let name = path.file_name().unwrap().to_string_lossy();
            if r.status != 200 {
                failures.push(format!("{name} stream={stream}: {} {}", r.status, &r.text[..r.text.len().min(300)]));
            } else if stream {
                let evs = r.events();
                let error = evs.iter().any(|(n, d)| n.as_deref() == Some("error") || (cap["protocol"] == "chat" && d["error"].is_object()));
                if evs.is_empty() || error {
                    failures.push(format!("{name} stream: {}", &r.text[r.text.len().saturating_sub(300)..]));
                }
            } else if !r.json().is_object() {
                failures.push(format!("{name}: not a JSON object"));
            }
        }
    }
    assert!(failures.is_empty(), "{} of {} captures failed:\n{}", failures.len(), paths.len(), failures.join("\n"));
}

/// The tools block of a prompt: from the listing's header to the end of the system part.
fn tools_block(prompt: &str) -> usize {
    let start = prompt.find("Available tools").unwrap_or(prompt.len());
    let end = prompt[start..].find("</system>").map_or(prompt.len(), |e| start + e);
    end - start
}

#[test]
#[ignore = "needs the local captures: MIDIR_CAPTURES=<dir> cargo test --profile ci --test replay -- --ignored"]
fn the_compact_tool_listing_is_shorter() {
    // review 2026-10-05 (N01): the raw JSON Schemas of Copilot's 75 tools were ~113k characters on every turn
    let dir = std::env::var("MIDIR_CAPTURES").expect("MIDIR_CAPTURES=<folder with the captures>");
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(dir);
    let mut paths: Vec<_> = std::fs::read_dir(&dir).unwrap().filter_map(|e| e.ok()).map(|e| e.path()).collect();
    paths.sort();
    let json_rig = Rig::new();
    let compact_rig = Rig::with(MIDIR_TOML, &[("MIDIR_TOOL_SCHEMA", "compact")]);
    let (mut json_total, mut compact_total, mut largest) = (0, 0, (0, 0, String::new()));
    for path in paths.iter().filter(|p| p.extension().is_some_and(|x| x == "json")) {
        let cap: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let mut body = cap["body"].clone();
        if body["tools"].as_array().is_none_or(|t| t.is_empty()) || body["tool_choice"] == "none" {
            continue;
        }
        let endpoint = match s(&cap["protocol"]) {
            "chat" => "/v1/chat/completions",
            "responses" => "/v1/responses",
            _ => "/v1/messages",
        };
        body.as_object_mut().unwrap().remove("previous_response_id");
        body["stream"] = json!(false);
        let mut sizes = vec![];
        for rig in [&json_rig, &compact_rig] {
            rig.upstream.add("plain answer");
            let before = rig.upstream.calls().len();
            assert_eq!(rig.http.post(endpoint, &body).status, 200, "{}", path.display());
            sizes.push(tools_block(&rig.upstream.prompt(before)));
        }
        json_total += sizes[0];
        compact_total += sizes[1];
        if sizes[0] > largest.0 {
            largest = (sizes[0], sizes[1], path.file_name().unwrap().to_string_lossy().into_owned());
        }
    }
    let saved = |a: usize, b: usize| 100.0 * (a as f64 - b as f64) / a as f64;
    println!(
        "tool listings: json {json_total} chars, compact {compact_total} ({:.1}% fewer); largest {}: {} -> {} ({:.1}% fewer)",
        saved(json_total, compact_total),
        largest.2,
        largest.0,
        largest.1,
        saved(largest.0, largest.1)
    );
    assert!(compact_total < json_total);
    assert!(saved(largest.0, largest.1) >= 12.0, "the largest tool set should shrink by an eighth at least");
}
