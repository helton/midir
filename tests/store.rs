//! Responses store (previous_response_id): chains on disk survive a restart, blobs are shared, permissions are
//! owner-only, purge by age, and broken chains answer 404 instead of a wrong history. The on-disk format is shared with
//! every earlier implementation, so upgrading keeps the chains.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::time::{Duration, SystemTime};

use common::*;
use serde_json::{json, Value};

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
    assert_eq!(blobs.len(), 2); // one system blob, one tools blob for the whole chain
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
