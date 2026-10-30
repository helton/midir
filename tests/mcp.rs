//! `POST /mcp`: the JSON-RPC transport (initialize, tools/list, notifications, errors), `web_search` through the
//! scripted web agent, `web_fetch` of pages served by a local site (native) and of pages that need a browser (the
//! agent), the endpoint when `[mcp]` is off, and `midir check`.

mod common;

use std::io::{Read, Write};
use std::net::TcpListener;

use common::*;
use serde_json::{Value, json};

fn mcp_toml() -> String {
    format!("{MIDIR_TOML}\n[mcp]\nenabled = true\nweb_agent = \"AGENTWEB\"\nfetch_max_chars = 600\n")
}

fn rpc(rig: &Rig, method: &str, params: Value) -> Value {
    let r = rig.http.post("/mcp", &json!({"jsonrpc": "2.0", "id": 7, "method": method, "params": params}));
    assert_eq!(r.status, 200, "{}", r.text);
    let v = r.json();
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], 7);
    v
}

fn call(rig: &Rig, tool: &str, args: Value) -> (bool, String) {
    let v = rpc(rig, "tools/call", json!({"name": tool, "arguments": args}));
    assert!(v["error"].is_null(), "{v}");
    let r = &v["result"];
    assert_eq!(r["content"][0]["type"], "text");
    (r["isError"].as_bool().unwrap(), s(&r["content"][0]["text"]).to_string())
}

/// A site of fixed pages on a local port: (path, status, content type, body).
fn site(pages: Vec<(&'static str, u16, &'static str, String)>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut req = vec![];
            let mut buf = [0u8; 1024];
            while !req.windows(4).any(|w| w == b"\r\n\r\n") {
                match s.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => req.extend_from_slice(&buf[..n]),
                }
            }
            let line = String::from_utf8_lossy(&req);
            let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
            let (status, ctype, body) = pages.iter().find(|(p, ..)| *p == path).map(|(_, st, ct, b)| (*st, *ct, b.clone())).unwrap_or((
                404,
                "text/plain",
                "no such page".into(),
            ));
            let _ = write!(
                s,
                "HTTP/1.1 {status} X\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    url
}

const PAGE: &str = "<html><head><title>Mutex in std::sync</title><style>.x{}</style></head><body><nav><a href='/'>Home</a> <a>Docs</a></nav>\
<main><h1>Struct Mutex</h1><p>A mutual exclusion primitive useful for protecting shared data.</p>\
<ul><li>lock()</li><li>try_lock()</li></ul><pre>let m = Mutex::new(5);\nlet g = m.lock();</pre>\
<p>PADDING</p></main><footer>Rust 2026</footer><script>track()</script></body></html>";

// ---------------------------------------------------------------- the transport

#[test]
fn initialize_lists_the_tools_and_answers_the_transport() {
    let rig = Rig::with(&mcp_toml(), &[]);
    let v = rpc(
        &rig,
        "initialize",
        json!({"protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": {"name": "test", "version": "1"}}),
    );
    assert_eq!(v["result"]["protocolVersion"], "2025-03-26");
    assert_eq!(v["result"]["serverInfo"]["name"], "midir");
    assert!(v["result"]["capabilities"]["tools"].is_object());
    let v = rpc(&rig, "initialize", json!({"protocolVersion": "2099-01-01"}));
    assert_eq!(v["result"]["protocolVersion"], "2025-06-18");
    // a notification: accepted, no body
    let r = rig.http.post("/mcp", &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    assert_eq!((r.status, r.text.as_str()), (202, ""));
    assert_eq!(rpc(&rig, "ping", json!({}))["result"], json!({}));
    let tools = rpc(&rig, "tools/list", json!({}))["result"]["tools"].clone();
    let names: Vec<&str> = tools.as_array().unwrap().iter().map(|t| s(&t["name"])).collect();
    assert_eq!(names, ["web_search", "web_fetch"]);
    for t in tools.as_array().unwrap() {
        assert_eq!(t["inputSchema"]["type"], "object");
        assert!(s(&t["description"]).len() < 140, "descriptions are paid in every prompt: {t}");
    }
    assert!(s(&tools[1]["description"]).contains("600"));
    // errors in JSON-RPC form
    assert_eq!(rpc(&rig, "resources/list", json!({}))["error"]["code"], -32601);
    assert_eq!(rpc(&rig, "tools/call", json!({"name": "nope"}))["error"]["code"], -32602);
    assert_eq!(rpc(&rig, "tools/call", json!({"name": "web_fetch", "arguments": {"url": "ftp://x"}}))["error"]["code"], -32602);
    let r = rig.http.post_raw("/mcp", b"{not json", "application/json");
    assert_eq!((r.status, r.json()["error"]["code"].as_i64()), (400, Some(-32700)));
    // a batch: one reply per request, none for notifications
    let batch = json!([{"jsonrpc": "2.0", "id": 1, "method": "ping"}, {"jsonrpc": "2.0", "method": "notifications/x"}, {"jsonrpc": "2.0", "id": 2, "method": "ping"}]);
    let r = rig.http.post("/mcp", &batch);
    assert_eq!(r.json().as_array().map(Vec::len), Some(2), "{}", r.text);
    // the other methods of the transport
    assert_eq!(rig.http.get("/mcp").status, 405);
    let h = rig.http.get("/health").json();
    assert_eq!(h["mcp"], json!({"enabled": true, "tools": ["web_search", "web_fetch"]}));
    assert!(rig.server.logs().contains("/mcp (web tools)"));
}

#[test]
fn mcp_off_is_a_clear_404_and_a_key_still_guards_it() {
    let rig = Rig::new();
    let r = rig.http.post("/mcp", &json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}));
    assert_eq!(r.status, 404);
    assert!(s(&r.json()["error"]["message"]).contains("[mcp] enabled = true"), "{}", r.text);
    assert_eq!(rig.http.get("/health").json()["mcp"], json!({"enabled": false}));
    let rig = Rig::with(&mcp_toml(), &[("MIDIR_API_KEY", "k1")]);
    assert_eq!(rig.http.post("/mcp", &json!({"jsonrpc": "2.0", "id": 1, "method": "ping"})).status, 401);
    let r = rig.http.post_with_headers("/mcp", &json!({"jsonrpc": "2.0", "id": 1, "method": "ping"}), &[("authorization", "Bearer k1")]);
    assert_eq!(r.status, 200);
}

// ---------------------------------------------------------------- web_search

#[test]
fn web_search_asks_the_web_agent_and_cleans_its_results() {
    let rig = Rig::with(&mcp_toml(), &[]);
    rig.upstream.add(
        "Tokio releases | https://github.com/tokio-rs/tokio/releases?utm_source=stackspot | every version\n\
         Tokio releases | https://github.com/tokio-rs/tokio/releases/?utm_source=stackspot | again\n\
         docs.rs | https://docs.rs/tokio/1.54.0/tokio/?utm_source=stackspot | the API\n",
    );
    let (is_error, text) = call(&rig, "web_search", json!({"query": "tokio 1.54 release notes", "max_results": 3}));
    assert!(!is_error);
    assert_eq!(
        text,
        "Tokio releases | https://github.com/tokio-rs/tokio/releases | every version\ndocs.rs | https://docs.rs/tokio/1.54.0/tokio/ | the API"
    );
    let calls = rig.upstream.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].agent, "AGENTWEB");
    assert!(calls[0].prompt.starts_with("Search the web for: tokio 1.54 release notes\n"), "{}", calls[0].prompt);
    assert!(calls[0].prompt.contains("top 3 results with 3 DISTINCT URLs"), "{}", calls[0].prompt);
    assert!(rig.server.logs().contains("mcp web_search \"tokio 1.54 release notes\": 2 results"), "{}", rig.server.logs());
    // the agent failing is the tool's error, not the transport's
    rig.upstream.add(Reply::status(403, json!({"message": "agent not shared"})));
    let (is_error, text) = call(&rig, "web_search", json!({"query": "x"}));
    assert!(is_error && text.contains("web_search failed"), "{text}");
    // without a query: a JSON-RPC error
    assert_eq!(rpc(&rig, "tools/call", json!({"name": "web_search", "arguments": {}}))["error"]["code"], -32602);
}

#[test]
fn without_a_web_agent_only_web_fetch_is_offered() {
    let rig = Rig::with(&format!("{MIDIR_TOML}\n[mcp]\nenabled = true\n"), &[]);
    let tools = rpc(&rig, "tools/list", json!({}))["result"]["tools"].clone();
    assert_eq!(tools.as_array().unwrap().len(), 1);
    assert_eq!(tools[0]["name"], "web_fetch");
    assert_eq!(rpc(&rig, "tools/call", json!({"name": "web_search", "arguments": {"query": "x"}}))["error"]["code"], -32602);
    assert!(rig.server.logs().contains("no web_agent"), "{}", rig.server.logs());
}

// ---------------------------------------------------------------- web_fetch

#[test]
fn web_fetch_reads_a_page_natively_as_text() {
    let base = site(vec![
        ("/mutex", 200, "text/html; charset=utf-8", PAGE.replace("PADDING", &"More text. ".repeat(60))),
        ("/notes.txt", 200, "text/plain", "plain  notes\nline two".into()),
        ("/data.bin", 200, "application/octet-stream", "\u{0}\u{1}\u{2}binary".into()),
    ]);
    let rig = Rig::with(&mcp_toml(), &[]);
    let (is_error, text) = call(&rig, "web_fetch", json!({"url": format!("{base}/mutex")}));
    assert!(!is_error, "{text}");
    let head = format!(
        "Mutex in std::sync\n{base}/mutex\n\nStruct Mutex\n\nA mutual exclusion primitive useful for protecting shared data.\n- lock()\n- try_lock()\n\nlet m = Mutex::new(5);\nlet g = m.lock();"
    );
    assert!(text.starts_with(&head), "{text}");
    assert!(!text.contains("Home") && !text.contains("Rust 2026") && !text.contains("track()") && !text.contains(".x{}"), "{text}");
    assert!(text.contains("\n[cut at 600 of ") && text.ends_with(" characters]"), "{text}");
    assert!(text.rsplit_once('\n').is_some_and(|(body, _)| body.chars().count() <= 600), "{text}");
    assert!(rig.upstream.calls().is_empty(), "a page Midir can read never goes to the agent");
    // the call's own cut, and a plain-text page
    let (_, text) = call(&rig, "web_fetch", json!({"url": format!("{base}/notes.txt"), "max_chars": 1000}));
    assert_eq!(text, format!("{base}/notes.txt\n\nplain  notes\nline two"));
    // not text: an error for the model, no fallback
    let (is_error, text) = call(&rig, "web_fetch", json!({"url": format!("{base}/data.bin")}));
    assert!(is_error && text.contains("not text (content-type application/octet-stream"), "{text}");
    // a page that does not exist: an error, and the agent is not asked
    let (is_error, text) = call(&rig, "web_fetch", json!({"url": format!("{base}/nope")}));
    assert!(is_error && text.contains("HTTP 404"), "{text}");
    assert!(rig.upstream.calls().is_empty());
    assert!(rig.server.logs().contains("mcp web_fetch"), "{}", rig.server.logs());
}

#[test]
fn a_page_built_by_javascript_or_out_of_reach_is_read_by_the_web_agent() {
    let shell =
        format!("<html><head><title>App</title></head><body><div id=app></div><script>{}</script></body></html>", "x".repeat(12_000));
    let base = site(vec![("/app", 200, "text/html", shell), ("/private", 403, "text/html", "<h1>Forbidden by proxy</h1>".into())]);
    let rig = Rig::with(&mcp_toml(), &[]);
    rig.upstream.add("The app's content, as the agent read it.");
    let (is_error, text) = call(&rig, "web_fetch", json!({"url": format!("{base}/app")}));
    assert!(!is_error);
    assert_eq!(text, "(read by the web agent: the page is built by JavaScript)\n\nThe app's content, as the agent read it.");
    let calls = rig.upstream.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].agent, "AGENTWEB");
    assert!(calls[0].prompt.starts_with(&format!("Fetch {base}/app and return its main text content")), "{}", calls[0].prompt);
    assert!(calls[0].prompt.contains("at most 600 characters"), "{}", calls[0].prompt);
    // a page Midir cannot reach (a proxy's 403): the agent, and its own failure reported with the reason
    rig.upstream.add("Private page text.");
    let (is_error, text) = call(&rig, "web_fetch", json!({"url": format!("{base}/private")}));
    assert!(!is_error);
    assert!(text.starts_with("(read by the web agent: HTTP 403: Forbidden by proxy)\n\nPrivate page text."), "{text}");
    rig.upstream.add("");
    let (is_error, text) = call(&rig, "web_fetch", json!({"url": format!("{base}/private")}));
    assert!(is_error && text.contains("the web agent returned nothing"), "{text}");
    // a host that does not answer at all
    let (is_error, text) = call(&rig, "web_fetch", json!({"url": "http://127.0.0.1:9/x"}));
    rig.upstream.add("Agent could not read it either.");
    assert!(is_error || text.contains("read by the web agent"), "{text}");
}

#[test]
fn without_a_web_agent_a_javascript_page_says_so() {
    let shell = format!("<html><body><p>Loading</p><script>{}</script></body></html>", "x".repeat(12_000));
    let base = site(vec![("/app", 200, "text/html", shell)]);
    let rig = Rig::with(&format!("{MIDIR_TOML}\n[mcp]\nenabled = true\n"), &[]);
    let (is_error, text) = call(&rig, "web_fetch", json!({"url": format!("{base}/app")}));
    assert!(!is_error);
    assert!(text.starts_with("(the page is built by JavaScript; nothing to read without a browser)\n\n"), "{text}");
    assert!(text.ends_with("Loading"), "{text}");
}

// ---------------------------------------------------------------- midir check

#[test]
fn check_reports_the_mcp_settings() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("config")).unwrap();
    let toml = mcp_toml().replace("{responses_dir}", &dir.path().join("r").to_string_lossy()).replace("{upstream}", "http://127.0.0.1:9");
    std::fs::write(dir.path().join("config/midir.toml"), toml).unwrap();
    let o = midir_command(&[("MIDIR_MCP_TIMEOUT_S", "45")]).args(["check", "--no-network"]).current_dir(dir.path()).output().unwrap();
    let text = String::from_utf8_lossy(&o.stdout).to_string();
    assert!(o.status.success(), "{text}");
    assert!(text.contains("MCP (POST /mcp)"), "{text}");
    assert!(text.lines().any(|l| l.contains("enabled") && l.contains("true") && l.ends_with("file")), "{text}");
    assert!(text.lines().any(|l| l.contains("web_agent") && l.contains("stackspot:AGENTW...")), "{text}");
    assert!(text.lines().any(|l| l.contains("timeout_s") && l.contains("45") && l.contains("env MIDIR_MCP_TIMEOUT_S")), "{text}");
    // enabled without an agent: a warning
    let toml = format!("{MIDIR_TOML}\n[mcp]\nenabled = true\n")
        .replace("{responses_dir}", &dir.path().join("r").to_string_lossy())
        .replace("{upstream}", "http://127.0.0.1:9");
    std::fs::write(dir.path().join("config/midir.toml"), toml).unwrap();
    let o = midir_command(&[]).args(["check", "--no-network"]).current_dir(dir.path()).output().unwrap();
    let text = String::from_utf8_lossy(&o.stdout).to_string();
    assert!(text.contains("WARN  mcp: no web_agent"), "{text}");
    // a bad setting stops the start
    let toml = format!("{MIDIR_TOML}\n[mcp]\nenabled = true\nfetch_max_chars = 10\n")
        .replace("{responses_dir}", "r")
        .replace("{upstream}", "http://127.0.0.1:9");
    std::fs::write(dir.path().join("config/midir.toml"), toml).unwrap();
    let o = midir_command(&[]).args(["check", "--no-network"]).current_dir(dir.path()).output().unwrap();
    assert!(!o.status.success());
    assert!(String::from_utf8_lossy(&o.stdout).contains("[mcp] fetch_max_chars"), "{}", String::from_utf8_lossy(&o.stdout));
}
