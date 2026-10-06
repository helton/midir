//! The observability stack's mitmproxy must stream SSE through (docker/mitm/sse_stream.py): without the addon it
//! buffers whole bodies, so tokens and keepalives reach clients only when generation is over. Opt-in, because it needs
//! mitmproxy 12: `mitmdump` on PATH (or MIDIR_MITMDUMP=<path>; the standalone binaries from mitmproxy.org need nothing
//! else), or else the stack's image with Docker's host network (Linux):
//!
//!     cargo test --profile ci --test mitm -- --ignored

mod common;

use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::*;
use serde_json::json;

enum Runner {
    Process(std::process::Child),
    Container(String),
}

struct Mitm {
    runner: Runner,
    url: String,
}

fn local_mitmdump() -> Option<String> {
    if let Ok(path) = std::env::var("MIDIR_MITMDUMP") {
        return Some(path);
    }
    Command::new("mitmdump")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()
        .filter(|s| s.success())
        .map(|_| "mitmdump".into())
}

impl Mitm {
    /// `mitmdump --mode reverse:<midir>` with the addon.
    fn start(target: &str) -> Mitm {
        let port = free_port();
        let addon = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("docker/mitm/sse_stream.py");
        let mode = format!("reverse:{target}");
        let port_arg = port.to_string();
        let runner = match local_mitmdump() {
            Some(bin) => {
                let confdir = tempfile::tempdir().unwrap().keep();
                // its own process group: the standalone build forks, and the whole group must go at the end
                let child = Command::new(bin)
                    .args([
                        "-q",
                        "--set",
                        &format!("confdir={}", confdir.display()),
                        "--mode",
                        &mode,
                        "-p",
                        &port_arg,
                        "-s",
                        &addon.to_string_lossy(),
                    ])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .process_group(0)
                    .spawn()
                    .expect("mitmdump");
                Runner::Process(child)
            }
            None => {
                let name = format!("midir-test-mitm-{port}");
                let status = Command::new("docker")
                    .args([
                        "run",
                        "-d",
                        "--rm",
                        "--name",
                        &name,
                        "--network",
                        "host",
                        "-v",
                        &format!("{}:/addons/sse_stream.py:ro", addon.display()),
                        "mitmproxy/mitmproxy:12",
                    ])
                    .args(["mitmdump", "-q", "--mode", &mode, "-p", &port_arg, "-s", "/addons/sse_stream.py"])
                    .stdout(Stdio::null())
                    .status()
                    .expect("docker");
                assert!(status.success(), "docker run mitmproxy failed");
                Runner::Container(name)
            }
        };
        let mitm = Mitm { runner, url: format!("http://127.0.0.1:{port}") };
        let deadline = Instant::now() + Duration::from_secs(120); // first run pulls the image
        let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(1)).build().unwrap();
        while Instant::now() < deadline {
            if client.get(format!("{}/health", mitm.url)).send().map(|r| r.status().is_success()).unwrap_or(false) {
                return mitm;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        panic!("mitmdump did not start (Docker Desktop has no host network: put mitmdump on PATH or set MIDIR_MITMDUMP)");
    }
}

impl Drop for Mitm {
    fn drop(&mut self) {
        match &mut self.runner {
            Runner::Process(child) => {
                unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                child.wait().ok();
            }
            Runner::Container(name) => {
                let _ = Command::new("docker").args(["rm", "-f", name.as_str()]).stdout(Stdio::null()).stderr(Stdio::null()).status();
            }
        }
    }
}

#[test]
#[ignore = "needs mitmproxy 12 (mitmdump on PATH, MIDIR_MITMDUMP or Docker): cargo test --profile ci --test mitm -- --ignored"]
fn keepalive_reaches_the_client_through_mitm_before_content() {
    let rig = Rig::with(&toml_with_server("keepalive_s = 0.2"), &[]);
    let mitm = Mitm::start(&rig.server.url);
    let http = Http::new(&mitm.url);
    for (path, body) in [
        ("/v1/chat/completions", json!({"model": "gpt-5.1", "stream": true, "messages": user("hi")})),
        ("/v1/responses", json!({"model": "gpt-5.1", "stream": true, "input": "hi"})),
    ] {
        rig.upstream.add(Reply::text("Hello there.").delay(2.0));
        let lines = http.post_lines(path, &body);
        let keepalive = lines.iter().find(|(_, l)| l.starts_with(": keepalive")).map(|(t, _)| *t);
        let content = lines.iter().find(|(_, l)| l.contains("Hello")).map(|(t, _)| *t);
        let (keepalive, content) = (keepalive.expect("no keepalive"), content.expect("no content"));
        assert!(
            keepalive < 1.0 && content > 1.8,
            "{path}: keepalive at {keepalive:.2}s, content at {content:.2}s (buffered: both at the end)"
        );
    }
}
