//! The announce-and-stop detector against a labeled corpus of real model replies (with tools available and no tool
//! call: PROMISE when the reply announced work it did not do, FINAL, ASK or WAIT otherwise). The replies come from real
//! sessions, so the corpus is kept out of the repository: point MIDIR_PROMISE_CORPUS at its folder (to_label_<set>.jsonl
//! with {"id", "reply", ...} and labels_<set>.jsonl with {"id", "label"}) and run
//!
//!     MIDIR_PROMISE_CORPUS=.internal/dev/bench/promise cargo test --release --test followups_corpus -- --ignored --nocapture
//!
//! A change to the detector's rules ships only with no false alarm and no recall lost.

use std::collections::HashMap;

use midir::emulation::followups::announces_without_acting;
use serde_json::Value;

/// Promises the rules catch today (out of 64), and false alarms allowed (none).
const MIN_CAUGHT: usize = 55;

fn read(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path).unwrap().lines().filter(|l| !l.trim().is_empty()).map(|l| serde_json::from_str(l).unwrap()).collect()
}

#[test]
#[ignore = "needs the private corpus: MIDIR_PROMISE_CORPUS=<dir> cargo test --release --test followups_corpus -- --ignored"]
fn labeled_replies() {
    let dir = std::env::var("MIDIR_PROMISE_CORPUS").expect("MIDIR_PROMISE_CORPUS=<folder with the corpus>");
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(dir);
    let (mut caught, mut missed, mut false_alarms) = (vec![], vec![], vec![]);
    for set in ["mitm", "replay"] {
        let replies: HashMap<String, String> = read(&dir.join(format!("to_label_{set}.jsonl")))
            .into_iter()
            .map(|r| (r["id"].as_str().unwrap().to_string(), r["reply"].as_str().unwrap().to_string()))
            .collect();
        for label in read(&dir.join(format!("labels_{set}.jsonl"))) {
            let id = label["id"].as_str().unwrap();
            let promise = label["label"] == "PROMISE";
            let flagged = announces_without_acting(&replies[id]);
            let entry = format!("{set}/{id}: {}", replies[id].chars().take(120).collect::<String>().replace('\n', " / "));
            match (promise, flagged) {
                (true, true) => caught.push(entry),
                (true, false) => missed.push(entry),
                (false, true) => false_alarms.push(entry),
                (false, false) => {}
            }
        }
    }
    println!("caught {} of {} promises; missed:\n  {}", caught.len(), caught.len() + missed.len(), missed.join("\n  "));
    assert!(false_alarms.is_empty(), "false alarms:\n{}", false_alarms.join("\n"));
    assert!(caught.len() >= MIN_CAUGHT, "caught {} promises, at least {MIN_CAUGHT} expected", caught.len());
}
