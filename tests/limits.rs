//! The backend queue as clients see it: a request the rate window or the concurrency cap cannot start within
//! `limits.queue_timeout_s` gets a 429 with `Retry-After` and never reaches the backend.

mod common;

use std::time::{Duration, Instant};

use common::*;
use serde_json::json;

fn ask(http: &Http) -> Resp {
    http.post("/v1/chat/completions", &json!({"model": "gpt-5.1", "messages": user("hi")}))
}

#[test]
fn the_rate_window_refuses_what_it_cannot_start_in_time() {
    let rig = Rig::with(MIDIR_TOML, &[("MIDIR_REQUESTS_PER_MINUTE", "2"), ("MIDIR_QUEUE_TIMEOUT", "1")]);
    rig.upstream.add_all(["one", "two", "three"]);
    assert_eq!(ask(&rig.http).status, 200);
    assert_eq!(ask(&rig.http).status, 200);
    let t0 = Instant::now();
    let r = ask(&rig.http);
    assert_eq!(r.status, 429, "{}", r.text);
    assert!(t0.elapsed() < Duration::from_secs(2), "a wait of a minute is refused at once, not after the timeout");
    assert!(r.header("retry-after").and_then(|v| v.parse::<u64>().ok()).is_some_and(|s| s > 1), "{:?}", r.headers);
    assert!(s(&r.json()["error"]["message"]).contains("queue_timeout_s"), "{}", r.text);
    assert_eq!(rig.upstream.calls().len(), 2);
}

#[test]
fn a_request_waiting_past_the_queue_timeout_gets_a_429() {
    let rig = Rig::with(MIDIR_TOML, &[("MIDIR_MAX_CONCURRENT", "1"), ("MIDIR_QUEUE_TIMEOUT", "1")]);
    rig.upstream.add(Reply::text("slow").delay(3.0));
    let http = Http::new(&rig.server.url);
    let first = std::thread::spawn(move || ask(&http));
    wait_until("the first request to reach the backend", || rig.upstream.calls().len() == 1);
    let t0 = Instant::now();
    let r = ask(&rig.http);
    assert_eq!(r.status, 429, "{}", r.text);
    let waited = t0.elapsed();
    assert!(waited >= Duration::from_millis(900) && waited < Duration::from_millis(2500), "{waited:?}");
    assert!(r.header("retry-after").is_some(), "{:?}", r.headers);
    assert_eq!(first.join().unwrap().status, 200);
    assert_eq!(rig.upstream.calls().len(), 1);
}
