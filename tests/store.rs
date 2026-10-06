//! Responses store (previous_response_id): chains on disk survive a restart, blobs are shared, permissions are
//! owner-only, purge by age, and broken chains answer 404 instead of a wrong history. The on-disk format is shared with
//! every earlier implementation, so upgrading keeps the chains.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, SystemTime};

use common::*;
use serde_json::{Value, json};

fn create(rig: &Rig, body: Value) -> Resp {
    rig.http.post("/v1/responses", &body)
}

fn chain(rig: &Rig, len: usize) -> Vec<String> {
    let (mut ids, mut prev): (Vec<String>, Option<String>) = (vec![], None);
    for i in 0..len {
        rig.upstream.add(format!("answer {i}"));
        let mut body = json!({"model": "gpt-5.1", "input": format!("question {i}")});
        match &prev {
            Some(p) => body["previous_response_id"] = json!(p),
            None => {
                body["instructions"] = json!("be brief");
                body["tools"] = resp_tools();
            }
        }
        let r = create(rig, body).json();
        let id = s(&r["id"]).to_string();
        ids.push(id.clone());
        prev = Some(id);
    }
    ids
}

fn mode(path: &std::path::Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn json_files(dir: &std::path::Path, prefix: &str) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    let name = p.file_name().unwrap().to_string_lossy();
                    name.starts_with(prefix) && name.ends_with(".json")
                })
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn chain_survives_a_restart() {
    let mut rig = Rig::new();
    let ids = chain(&rig, 3);
    rig.restart();
    rig.upstream.add("final");
    assert_eq!(create(&rig, json!({"model": "gpt-5.1", "previous_response_id": ids[2], "input": "question 3"})).status, 200);
    let p = rig.upstream.prompts().last().unwrap().clone();
    for i in 0..3 {
        assert!(p.contains(&format!("question {i}")) && p.contains(&format!("answer {i}")), "{p}");
    }
    assert!(p.contains("be brief") && p.contains("\"read_file\"")); // instructions and tools inherited through the chain
}

#[test]
fn files_are_owner_only_and_blobs_shared() {
    let rig = Rig::new();
    chain(&rig, 3);
    let d = rig.server.store_dir();
    let files = json_files(&d, "resp_");
    let blobs = json_files(&d.join("blobs"), "");
    assert_eq!(files.len(), 3);
    assert_eq!(blobs.len(), 3); // for the whole chain: the system prompt, the tools, and the tools as the client sent them
    assert_eq!(mode(&d), 0o700);
    for f in files.iter().chain(blobs.iter()) {
        assert_eq!(mode(f), 0o600, "{}", f.display());
    }
}

#[test]
fn new_instructions_and_tools_replace_the_inherited_ones() {
    let rig = Rig::new();
    let ids = chain(&rig, 1);
    rig.upstream.add("x");
    create(&rig, json!({"model": "gpt-5.1", "previous_response_id": ids[0], "input": "q", "instructions": "NEW RULES", "tools": []}));
    let p = rig.upstream.prompts().last().unwrap().clone();
    assert!(p.contains("NEW RULES") && !p.contains("be brief") && !p.contains("# Tools"), "{p}");
}

#[test]
fn tool_call_round_trip_through_the_store() {
    let mut rig = Rig::new();
    rig.upstream.add(tool_call_text("read_file", json!({"path": "a.py"}))).add("done");
    let r1 = create(&rig, json!({"model": "gpt-5.1", "input": "read a.py", "tools": resp_tools()})).json();
    rig.restart();
    let call = r1["output"].as_array().unwrap().last().unwrap().clone();
    let cid = s(&call["call_id"]).to_string();
    create(
        &rig,
        json!({"model": "gpt-5.1", "previous_response_id": r1["id"], "input": [{"type": "function_call_output", "call_id": cid, "output": "print(1)"}]}),
    );
    let p = rig.upstream.prompts().last().unwrap().clone();
    assert!(
        p.contains(&format!("<tool_call id=\"{cid}\">")) && p.contains(&format!("<tool_result id=\"{cid}\" name=\"read_file\">")),
        "{p}"
    );
}

#[test]
fn store_false_is_not_kept() {
    let rig = Rig::new();
    rig.upstream.add("x");
    let r = create(&rig, json!({"model": "gpt-5.1", "input": "x", "store": false})).json();
    let id = s(&r["id"]);
    assert!(!rig.server.store_dir().join(format!("{id}.json")).exists());
    assert_eq!(rig.http.get(&format!("/v1/responses/{id}")).status, 404);
}

#[test]
fn purge_by_age_breaks_the_chain_with_a_404() {
    let mut rig = Rig::new();
    let ids = chain(&rig, 3);
    let file = rig.server.store_dir().join(format!("{}.json", ids[0]));
    let old = SystemTime::now() - Duration::from_secs(30 * 86400 + 10); // older than the default 30-day retention
    std::fs::File::options().write(true).open(&file).unwrap().set_modified(old).unwrap();
    rig.restart(); // startup purges
    assert!(!file.exists());
    assert_eq!(create(&rig, json!({"model": "gpt-5.1", "previous_response_id": ids[2], "input": "next"})).status, 404);
}

#[test]
fn purge_by_size_removes_the_oldest() {
    let mut rig = Rig::with(&toml_with_server("responses_max_mb = 0.01"), &[]);
    let big = "z".repeat(3000);
    let mut ids = vec![];
    for i in 0..8 {
        rig.upstream.add(format!("{big} {i}"));
        ids.push(
            s(&create(&rig, json!({"model": "gpt-5.1", "input": format!("q{i}"), "instructions": format!("unique {i}")})).json()["id"])
                .to_string(),
        );
        std::thread::sleep(Duration::from_millis(15)); // distinct modification times
    }
    rig.restart();
    let d = rig.server.store_dir();
    let kept: Vec<bool> = ids.iter().map(|id| d.join(format!("{id}.json")).exists()).collect();
    assert!(!kept[0] && *kept.last().unwrap(), "{kept:?}");
}

#[test]
fn corrupt_file_is_a_404_not_a_500() {
    let mut rig = Rig::new();
    let ids = chain(&rig, 2);
    std::fs::write(rig.server.store_dir().join(format!("{}.json", ids[0])), "{broken").unwrap();
    rig.restart();
    assert_eq!(create(&rig, json!({"model": "gpt-5.1", "previous_response_id": ids[1], "input": "next"})).status, 404);
}

#[test]
fn ids_that_are_not_ours_never_touch_the_disk() {
    let rig = Rig::new();
    for bad in ["../../etc/passwd", "resp_xyz", &format!("resp_{}", "a".repeat(23))] {
        let status = create(&rig, json!({"model": "gpt-5.1", "previous_response_id": bad, "input": "x"})).status;
        assert!(status == 404 || status == 400, "{bad}: {status}");
    }
}

#[test]
fn memory_only_mode() {
    let rig = Rig::with(&MIDIR_TOML.replace("responses_dir = \"{responses_dir}\"", "responses_dir = \"\""), &[]);
    rig.upstream.add("one").add("two");
    let r1 = create(&rig, json!({"model": "gpt-5.1", "input": "q1"})).json();
    let r2 = create(&rig, json!({"model": "gpt-5.1", "input": "q2", "previous_response_id": r1["id"]}));
    assert_eq!(r2.status, 200);
    assert!(rig.upstream.prompt(1).contains("q1"));
    assert!(!rig.server.workdir.join("data").exists() && !rig.server.store_dir().exists());
}

#[test]
fn memory_grows_with_what_each_response_adds_not_with_the_conversation() {
    // a response shares its history with the one it continues: along a chain the cache grows linearly
    let rig = Rig::with(&MIDIR_TOML.replace("responses_dir = \"{responses_dir}\"", "responses_dir = \"\""), &[]);
    let chunk = "x".repeat(10_000);
    let mut prev: Option<String> = None;
    let mut sizes = vec![];
    for i in 0..60 {
        rig.upstream.add("ok");
        let mut body = json!({"model": "gpt-5.1", "input": format!("{i} {chunk}")});
        if let Some(p) = &prev {
            body["previous_response_id"] = json!(p);
        }
        prev = Some(s(&create(&rig, body).json()["id"]).to_string());
        if i % 20 == 19 {
            sizes.push(rig.http.get("/health").json()["responses_cache"]["bytes"].as_u64().unwrap());
        }
    }
    let (a, b, c) = (sizes[0] as f64, sizes[1] as f64, sizes[2] as f64);
    assert!((b - a) / a < 1.2 && (c - b) / a < 1.2, "{sizes:?}"); // each 20 steps add about the same
    assert!(c < 60.0 * 10_000.0 * 1.5, "{sizes:?}"); // not 60 copies of a growing conversation
}

#[test]
fn leftovers_and_unused_blobs_are_collected() {
    let mut rig = Rig::with(&toml_with_server("responses_max_mb = 0.05"), &[]);
    let ids = chain(&rig, 2);
    let d = rig.server.store_dir();
    let old = SystemTime::now() - Duration::from_secs(3600);
    let tmp = d.join(format!("{}.tmp", ids[0]));
    std::fs::write(&tmp, "{half").unwrap();
    std::fs::File::options().write(true).open(&tmp).unwrap().set_modified(old).unwrap();
    let unused = d.join("blobs").join(format!("{}.json", "f".repeat(32)));
    std::fs::write(&unused, "z".repeat(60_000)).unwrap(); // above the cap on its own
    std::fs::File::options().write(true).open(&unused).unwrap().set_modified(old).unwrap();
    rig.restart(); // startup purges
    assert!(!tmp.exists() && !unused.exists());
    for id in &ids {
        assert!(d.join(format!("{id}.json")).exists(), "{id}: the responses fit once the unused blob is gone");
    }
    rig.upstream.add("next");
    assert_eq!(create(&rig, json!({"model": "gpt-5.1", "previous_response_id": ids[1], "input": "q"})).status, 200);
}

#[test]
fn stored_text_is_the_same_streaming_or_not() {
    let rig = Rig::new();
    let text = format!("Before.\n{}", tool_call_text("read_file", json!({"path": "a.py"})));
    rig.upstream.add(text.as_str()).add(text.as_str());
    let plain = create(&rig, json!({"model": "gpt-5.1", "input": "x", "tools": resp_tools()})).json();
    let streamed =
        rig.http.post("/v1/responses", &json!({"model": "gpt-5.1", "input": "x", "tools": resp_tools(), "stream": true})).events();
    let streamed_id = s(&streamed.last().unwrap().1["response"]["id"]).to_string();
    let get = |id: &str| rig.http.get(&format!("/v1/responses/{id}")).json()["output"][0]["content"][0]["text"].clone();
    assert_eq!(get(s(&plain["id"])), json!("Before."));
    assert_eq!(get(&streamed_id), json!("Before."));
}

#[test]
fn expired_responses_are_not_served_from_disk_before_the_purge() {
    // review 2026-10-05 (F05): retention held only within an hour of the hourly purge
    let rig = Rig::with(&toml_with_server("responses_retention_days = 0.00002"), &[]); // about 1.7 s
    rig.upstream.add("first");
    let id = s(&create(&rig, json!({"model": "gpt-5.1", "input": "x"})).json()["id"]).to_string();
    std::thread::sleep(Duration::from_millis(2600));
    assert_eq!(rig.http.get(&format!("/v1/responses/{id}")).status, 404);
    rig.upstream.add("second");
    assert_eq!(create(&rig, json!({"model": "gpt-5.1", "input": "y", "previous_response_id": id})).status, 404);
}

#[test]
fn an_empty_blob_is_rewritten() {
    // review 2026-10-05 (F06): a crash between rename and flush can leave an empty blob that every chain shares
    let mut rig = Rig::new();
    rig.upstream.add("first");
    let first = s(&create(&rig, json!({"model": "gpt-5.1", "input": "x", "instructions": "be brief"})).json()["id"]).to_string();
    let blobs = json_files(&rig.server.store_dir().join("blobs"), "");
    assert!(!blobs.is_empty());
    for b in &blobs {
        std::fs::write(b, b"").unwrap();
    }
    rig.restart(); // memory gone: the next reads come from disk
    rig.upstream.add("second");
    assert_eq!(create(&rig, json!({"model": "gpt-5.1", "input": "y", "instructions": "be brief"})).status, 200);
    for b in &blobs {
        assert!(std::fs::metadata(b).unwrap().len() > 0, "{}", b.display());
    }
    assert_eq!(rig.http.get(&format!("/v1/responses/{first}")).status, 200);
}

#[test]
fn size_purge_removes_whole_chains() {
    // review 2026-10-05 (F07): the purge removed a chain's small root and left continuations nobody could rebuild
    let mut rig = Rig::with(&toml_with_server("responses_max_mb = 0.012"), &[]);
    let big = "z".repeat(3000);
    let mut prev: Option<String> = None;
    for answer in ["short".to_string(), format!("{big} 1"), format!("{big} 2")] {
        rig.upstream.add(answer);
        let mut body = json!({"model": "gpt-5.1", "input": "continue"});
        if let Some(p) = &prev {
            body["previous_response_id"] = json!(p);
        }
        prev = Some(s(&create(&rig, body).json()["id"]).to_string());
        std::thread::sleep(Duration::from_millis(15));
    }
    let mut standalone = vec![];
    for i in 0..2 {
        rig.upstream.add(format!("{big} standalone {i}"));
        standalone.push(s(&create(&rig, json!({"model": "gpt-5.1", "input": format!("q{i}")})).json()["id"]).to_string());
        std::thread::sleep(Duration::from_millis(15));
    }
    rig.restart(); // the purge runs at startup
    let d = rig.server.store_dir();
    for f in json_files(&d, "resp_") {
        let id = f.file_stem().unwrap().to_string_lossy().to_string();
        assert_eq!(rig.http.get(&format!("/v1/responses/{id}")).status, 200, "{id} was left without its chain");
    }
    assert!(d.join(format!("{}.json", standalone[1])).exists());
}
