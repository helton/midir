//! Shared harness for the black-box regression suite. Every test runs Midir as a separate process (the binary Cargo
//! just built, `CARGO_BIN_EXE_midir`) in front of a scripted StackSpot stand-in (a real HTTP server in this process;
//! nothing leaves the machine), and talks to it over HTTP exactly as the clients do.
//!
//!     cargo test --release                  # the whole suite (the release profile: the same binary the image ships)
//!     cargo test --release --test protocols # one file
#![allow(dead_code)]

use std::collections::VecDeque;
use std::io::Write;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use serde_json::{Value, json};

pub const MIDIR_TOML: &str = r#"
default_model = "gpt-5.1"

[server]
responses_dir = "{responses_dir}"

[backends.stackspot]
type = "stackspot"
realm = "acme"
client_id = "cid"
client_secret = "secret"
idm_base_url = "{upstream}"
agent_base_url = "{upstream}/v1/agent"

[backends.stackspot.limits]
max_concurrent = 4
requests_per_minute = 0
cooldown_on_429_s = 0

[[models]]
name = "gpt-5.1"
backend = "stackspot"
target = "AGENT51"
aliases = ["claude-opus-4-5"]

[[models]]
name = "gpt-4.1"
target = "AGENT41"
aliases = ["claude-haiku-4-5"]

[[models]]
name = "flex"
target = "AGENTFLEX"
match = "sonnet"
"#;

/// MIDIR_TOML with extra `[server]` settings (`keepalive_s = 0.05`, ...).
pub fn toml_with_server(extra: &str) -> String {
    MIDIR_TOML.replace("responses_dir = \"{responses_dir}\"", &format!("responses_dir = \"{{responses_dir}}\"\n{extra}"))
}

// ---------------------------------------------------------------------------------------------------------------------
// scripted upstream
// ---------------------------------------------------------------------------------------------------------------------

/// One upstream answer. `text` is streamed in `chunk`-sized `message` deltas, then the final event. `status != 200`
/// answers an HTTP error with `body`; `events` replaces the generated SSE events entirely; `raw` replaces the whole
/// SSE body; `break_after` drops the connection after that many events; `delay` is the model's latency.
#[derive(Clone, Debug)]
pub struct Reply {
    pub text: String,
    pub chunk: usize,
    pub final_event: Option<Value>,
    pub status: u16,
    pub body: Value,
    pub headers: Vec<(String, String)>,
    pub events: Option<Vec<Value>>,
    pub raw: Option<String>,
    pub break_after: Option<usize>,
    pub delay: Duration,
}

impl Reply {
    pub fn text(text: &str) -> Self {
        Reply {
            text: text.into(),
            chunk: 7,
            final_event: None,
            status: 200,
            body: Value::Null,
            headers: vec![],
            events: None,
            raw: None,
            break_after: None,
            delay: Duration::ZERO,
        }
    }
    pub fn status(status: u16, body: Value) -> Self {
        Reply { status, body, ..Reply::text("") }
    }
    pub fn events(events: Vec<Value>) -> Self {
        Reply { events: Some(events), ..Reply::text("") }
    }
    pub fn chunk(mut self, n: usize) -> Self {
        self.chunk = n;
        self
    }
    pub fn final_event(mut self, v: Value) -> Self {
        self.final_event = Some(v);
        self
    }
    pub fn break_after(mut self, n: usize) -> Self {
        self.break_after = Some(n);
        self
    }
    pub fn delay(mut self, secs: f64) -> Self {
        self.delay = Duration::from_secs_f64(secs);
        self
    }
    pub fn raw(raw: &str) -> Self {
        Reply { raw: Some(raw.into()), ..Reply::text("") }
    }
    pub fn header(mut self, k: &str, v: &str) -> Self {
        self.headers.push((k.into(), v.into()));
        self
    }
}

impl From<&str> for Reply {
    fn from(s: &str) -> Self {
        Reply::text(s)
    }
}

impl From<String> for Reply {
    fn from(s: String) -> Self {
        Reply::text(&s)
    }
}

/// One Agent API call as the stand-in saw it.
#[derive(Clone, Debug)]
pub struct Call {
    pub agent: String,
    pub prompt: String,
    pub auth: Option<String>,
    pub body: Value,
}

#[derive(Default)]
struct UpstreamState {
    script: VecDeque<Reply>,
    default: Option<Reply>,
    calls: Vec<Call>,
    token_calls: usize,
    token_status: u16,
    expires_in: u64,
}

/// Stand-in for idm (token) + Agent API over real HTTP. Replies are consumed in order; `default` answers when the
/// script is empty.
pub struct Upstream {
    state: Arc<Mutex<UpstreamState>>,
    pub url: String,
    runtime: Option<tokio::runtime::Runtime>,
}

impl Upstream {
    pub fn new() -> Self {
        let state = Arc::new(Mutex::new(UpstreamState {
            default: Some(Reply::text("ok")),
            token_status: 200,
            expires_in: 1200,
            ..Default::default()
        }));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        listener.set_nonblocking(true).unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let app = Router::new().fallback(any(upstream_handler)).with_state(state.clone());
        runtime.spawn(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(listener, app).await.unwrap();
        });
        Upstream { state, url, runtime: Some(runtime) }
    }

    pub fn add<R: Into<Reply>>(&self, reply: R) -> &Self {
        self.state.lock().unwrap().script.push_back(reply.into());
        self
    }

    pub fn add_all<R: Into<Reply>>(&self, replies: impl IntoIterator<Item = R>) -> &Self {
        for r in replies {
            self.add(r);
        }
        self
    }

    pub fn set_default(&self, reply: Reply) {
        self.state.lock().unwrap().default = Some(reply);
    }

    pub fn set_token_status(&self, status: u16) {
        self.state.lock().unwrap().token_status = status;
    }

    pub fn set_expires_in(&self, secs: u64) {
        self.state.lock().unwrap().expires_in = secs;
    }

    pub fn calls(&self) -> Vec<Call> {
        self.state.lock().unwrap().calls.clone()
    }

    pub fn prompts(&self) -> Vec<String> {
        self.state.lock().unwrap().calls.iter().map(|c| c.prompt.clone()).collect()
    }

    pub fn prompt(&self, i: usize) -> String {
        self.prompts()[i].clone()
    }

    pub fn token_calls(&self) -> usize {
        self.state.lock().unwrap().token_calls
    }

    pub fn clear(&self) {
        let mut s = self.state.lock().unwrap();
        s.script.clear();
        s.calls.clear();
    }
}

impl Default for Upstream {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Upstream {
    fn drop(&mut self) {
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_timeout(Duration::from_secs(2));
        }
    }
}

async fn upstream_handler(State(state): State<Arc<Mutex<UpstreamState>>>, uri: Uri, headers: HeaderMap, raw: Bytes) -> Response {
    let path = uri.path().to_string();
    if path.ends_with("/oidc/oauth/token") {
        let mut s = state.lock().unwrap();
        s.token_calls += 1;
        if s.token_status != 200 {
            return json_response(s.token_status, &json!({"error": "invalid_client"}));
        }
        let body = json!({"access_token": format!("tok{}", s.token_calls), "expires_in": s.expires_in});
        return json_response(200, &body);
    }
    let body: Value = serde_json::from_slice(&raw).unwrap_or(Value::Null);
    let agent = path.trim_end_matches('/').rsplit('/').nth(1).unwrap_or("").to_string();
    let prompt = body["user_prompt"].as_str().unwrap_or("").to_string();
    let reply = {
        let mut s = state.lock().unwrap();
        s.calls.push(Call {
            agent,
            prompt: prompt.clone(),
            auth: headers.get("authorization").and_then(|v| v.to_str().ok()).map(String::from),
            body,
        });
        s.script.pop_front().or_else(|| s.default.clone()).unwrap()
    };
    if !reply.delay.is_zero() {
        tokio::time::sleep(reply.delay).await;
    }
    if reply.status != 200 {
        let mut resp = match &reply.body {
            Value::String(s) => (StatusCode::from_u16(reply.status).unwrap(), [("content-type", "text/plain")], s.clone()).into_response(),
            other => json_response(reply.status, other),
        };
        for (k, v) in &reply.headers {
            resp.headers_mut().insert(axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(), v.parse().unwrap());
        }
        return resp;
    }
    if let Some(raw) = &reply.raw {
        return (StatusCode::OK, [("content-type", "text/event-stream")], raw.clone()).into_response();
    }
    let events: Vec<Value> = reply.events.clone().unwrap_or_else(|| {
        let chars: Vec<char> = reply.text.chars().collect();
        let mut evs: Vec<Value> = chars.chunks(reply.chunk.max(1)).map(|c| json!({"message": c.iter().collect::<String>()})).collect();
        evs.push(reply.final_event.clone().unwrap_or_else(|| {
            json!({"stop_reason": "stop", "message_id": "up-msg-1", "tokens": {"input": prompt.chars().count() / 4, "output": (chars.len() / 4).max(1)}})
        }));
        evs
    });
    let parts: Vec<Bytes> = events
        .iter()
        .map(|e| {
            let data = match e {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            Bytes::from(format!("data: {data}\r\n\r\n"))
        })
        .collect();
    let stream = async_stream::stream! {
        let n = reply.break_after.unwrap_or(parts.len());
        for p in parts.into_iter().take(n) {
            yield Ok::<Bytes, std::io::Error>(p);
        }
        if reply.break_after.is_some() {
            tokio::time::sleep(Duration::from_millis(20)).await;  // the parts reach the client before the connection breaks
            yield Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "dropped by the stand-in"));
        }
    };
    Response::builder().status(200).header("content-type", "text/event-stream").body(Body::from_stream(stream)).unwrap()
}

fn json_response(status: u16, body: &Value) -> Response {
    (StatusCode::from_u16(status).unwrap(), [("content-type", "application/json")], body.to_string()).into_response()
}

// ---------------------------------------------------------------------------------------------------------------------
// the implementation under test, as a process
// ---------------------------------------------------------------------------------------------------------------------

pub fn midir_bin() -> &'static str {
    env!("CARGO_BIN_EXE_midir")
}

pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

/// The process environment without any Midir, StackSpot or OTel variable of the developer's shell, plus `extra`.
pub fn clean_env(extra: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut env: Vec<(String, String)> =
        std::env::vars().filter(|(k, _)| !k.starts_with("STACKSPOT_") && !k.starts_with("MIDIR_") && !k.starts_with("OTEL_")).collect();
    env.push(("MIDIR_NO_BANNER".into(), "1".into()));
    for (k, v) in extra {
        env.retain(|(ek, _)| ek != k);
        env.push((k.to_string(), v.to_string()));
    }
    env
}

/// A command for the binary with a clean environment (`env_clear` + `clean_env`).
pub fn midir_command(extra: &[(&str, &str)]) -> Command {
    let mut cmd = Command::new(midir_bin());
    cmd.env_clear().envs(clean_env(extra));
    cmd
}

/// Midir started in its own working directory with config/midir.toml pointing at the stand-in. `url` is its base
/// URL; `restart()` keeps the configuration and data; dropping it stops the process.
pub struct Server {
    pub url: String,
    pub port: u16,
    pub workdir: PathBuf,
    extra_env: Vec<(String, String)>,
    child: Option<Child>,
    retries: u32,
    _dir: tempfile::TempDir,
}

impl Server {
    pub fn start(toml: &str, upstream: &Upstream, extra_env: &[(&str, &str)]) -> Server {
        let dir = tempfile::Builder::new().prefix("midir-test-").tempdir().unwrap();
        let workdir = dir.path().to_path_buf();
        std::fs::create_dir_all(workdir.join("config")).unwrap();
        let text = toml.replace("{responses_dir}", &workdir.join("responses").to_string_lossy()).replace("{upstream}", &upstream.url);
        std::fs::write(workdir.join("config/midir.toml"), text).unwrap();
        let port = free_port();
        let mut server = Server {
            url: format!("http://127.0.0.1:{port}"),
            port,
            workdir,
            extra_env: extra_env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            child: None,
            retries: 0,
            _dir: dir,
        };
        server.spawn();
        server
    }

    fn spawn(&mut self) {
        let log = std::fs::OpenOptions::new().create(true).append(true).open(self.log_path()).unwrap();
        let port = self.port.to_string();
        let mut extra: Vec<(&str, &str)> = vec![("MIDIR_PORT", &port), ("MIDIR_RETRY_BACKOFF_S", "0.001")];
        extra.extend(self.extra_env.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        let child = midir_command(&extra)
            .args(["--host", "127.0.0.1", "--port", &port])
            .current_dir(&self.workdir)
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .spawn()
            .expect("midir binary");
        self.child = Some(child);
        let deadline = Instant::now() + Duration::from_secs(20);
        let client = reqwest::blocking::Client::builder().timeout(Duration::from_millis(500)).build().unwrap();
        while Instant::now() < deadline {
            if let Some(status) = self.child.as_mut().unwrap().try_wait().unwrap() {
                // free_port() can race with another test that got the same port: take another one
                if self.logs().contains("cannot listen") && self.retries < 3 {
                    self.retries += 1;
                    self.port = free_port();
                    self.url = format!("http://127.0.0.1:{}", self.port);
                    return self.spawn();
                }
                panic!("midir exited with {status}:\n{}", self.logs());
            }
            // another test's server may hold the same port (free_port races): only our own config counts
            let ours = self.workdir.join("config").join("midir.toml").to_string_lossy().into_owned();
            let health = client.get(format!("{}/health", self.url)).send().ok().filter(|r| r.status().as_u16() == 200);
            let body = health.and_then(|r| r.text().ok()).and_then(|t| serde_json::from_str::<Value>(&t).ok());
            if body.is_some_and(|h| h["config"] == ours.as_str()) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("midir did not answer /health in 20 s:\n{}", self.logs());
    }

    pub fn log_path(&self) -> PathBuf {
        self.workdir.join("server.log")
    }

    pub fn logs(&self) -> String {
        std::fs::read_to_string(self.log_path()).unwrap_or_default()
    }

    pub fn store_dir(&self) -> PathBuf {
        self.workdir.join("responses")
    }

    /// SIGTERM, then wait: everything pending (telemetry) must be flushed.
    pub fn stop(&mut self) {
        if let Some(mut child) = self.child.take()
            && child.try_wait().unwrap().is_none()
        {
            unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
            let deadline = Instant::now() + Duration::from_secs(8);
            while Instant::now() < deadline && child.try_wait().unwrap().is_none() {
                std::thread::sleep(Duration::from_millis(10));
            }
            if child.try_wait().unwrap().is_none() {
                child.kill().ok();
                child.wait().ok();
            }
        }
    }

    pub fn restart(&mut self) {
        self.stop();
        self.spawn();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// the client side
// ---------------------------------------------------------------------------------------------------------------------

pub struct Resp {
    pub status: u16,
    pub headers: reqwest::header::HeaderMap,
    pub text: String,
}

impl Resp {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.text).unwrap_or_else(|e| panic!("not JSON ({e}): {}", &self.text[..self.text.len().min(300)]))
    }
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
    /// The SSE body as (event name, decoded data) pairs; `[DONE]` stays a string.
    pub fn events(&self) -> Vec<(Option<String>, Value)> {
        sse_events(&self.text)
    }
    /// The decoded data of every SSE event that is a JSON object.
    pub fn objects(&self) -> Vec<Value> {
        self.events().into_iter().map(|(_, d)| d).filter(|d| d.is_object()).collect()
    }
    pub fn is_sse(&self) -> bool {
        self.header("content-type").map(|c| c.starts_with("text/event-stream")).unwrap_or(false)
    }
}

/// HTTP client bound to one server's base URL (a fresh connection pool per test, as a real client would have).
pub struct Http {
    pub base: String,
    client: reqwest::blocking::Client,
}

impl Http {
    pub fn new(base: &str) -> Self {
        Http { base: base.into(), client: reqwest::blocking::Client::builder().timeout(Duration::from_secs(120)).build().unwrap() }
    }
    fn wrap(r: reqwest::Result<reqwest::blocking::Response>) -> Resp {
        let r = r.expect("request");
        let status = r.status().as_u16();
        let headers = r.headers().clone();
        let text = r.text().unwrap_or_default();
        Resp { status, headers, text }
    }
    pub fn post(&self, path: &str, body: &Value) -> Resp {
        Self::wrap(
            self.client.post(format!("{}{path}", self.base)).header("content-type", "application/json").body(body.to_string()).send(),
        )
    }
    pub fn post_with_headers(&self, path: &str, body: &Value, headers: &[(&str, &str)]) -> Resp {
        let mut req = self.client.post(format!("{}{path}", self.base)).header("content-type", "application/json").body(body.to_string());
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        Self::wrap(req.send())
    }
    pub fn post_raw(&self, path: &str, body: &[u8], content_type: &str) -> Resp {
        Self::wrap(self.client.post(format!("{}{path}", self.base)).header("content-type", content_type).body(body.to_vec()).send())
    }
    pub fn get(&self, path: &str) -> Resp {
        Self::wrap(self.client.get(format!("{}{path}", self.base)).send())
    }
    pub fn get_with_headers(&self, path: &str, headers: &[(&str, &str)]) -> Resp {
        let mut req = self.client.get(format!("{}{path}", self.base));
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        Self::wrap(req.send())
    }
    /// A streaming POST read line by line: (seconds since the request, line) for every line of the body.
    pub fn post_lines(&self, path: &str, body: &Value) -> Vec<(f64, String)> {
        use std::io::{BufRead, BufReader};
        let t0 = Instant::now();
        let r = self
            .client
            .post(format!("{}{path}", self.base))
            .header("content-type", "application/json")
            .body(body.to_string())
            .send()
            .expect("request");
        let mut out = vec![];
        for line in BufReader::new(r).lines() {
            match line {
                Ok(l) => out.push((t0.elapsed().as_secs_f64(), l)),
                Err(_) => break,
            }
        }
        out
    }
}

/// Everything a test needs: the stand-in, Midir on it, and a client. Drop order: client, server (SIGTERM), stand-in.
pub struct Rig {
    pub upstream: Upstream,
    pub server: Server,
    pub http: Http,
}

impl Rig {
    pub fn new() -> Rig {
        Rig::with(MIDIR_TOML, &[])
    }
    pub fn with(toml: &str, env: &[(&str, &str)]) -> Rig {
        let upstream = Upstream::new();
        let server = Server::start(toml, &upstream, env);
        let http = Http::new(&server.url);
        Rig { upstream, server, http }
    }
    pub fn restart(&mut self) {
        self.server.restart();
    }
}

impl Default for Rig {
    fn default() -> Self {
        Rig::new()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.server.stop();
    }
}

// ---------------------------------------------------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------------------------------------------------

/// Parse an SSE body into (event name, decoded data) pairs; `[DONE]` stays a string. Comments (`: keepalive`) are
/// not events.
pub fn sse_events(text: &str) -> Vec<(Option<String>, Value)> {
    let mut out = vec![];
    for block in text.replace("\r\n", "\n").split("\n\n") {
        let (mut name, mut data) = (None, vec![]);
        for line in block.split('\n') {
            if let Some(n) = line.strip_prefix("event:") {
                name = Some(n.trim().to_string());
            } else if let Some(d) = line.strip_prefix("data:") {
                data.push(d.trim().to_string());
            }
        }
        if !data.is_empty() {
            let raw = data.join("\n");
            let value = if raw == "[DONE]" {
                Value::String(raw)
            } else {
                serde_json::from_str(&raw).unwrap_or_else(|e| panic!("bad SSE data ({e}): {raw}"))
            };
            out.push((name, value));
        }
    }
    out
}

pub fn tool_call_text(name: &str, args: Value) -> String {
    tool_call_text_id(name, args, "call_1")
}

pub fn tool_call_text_id(name: &str, args: Value, cid: &str) -> String {
    format!("<tool_call id=\"{cid}\">\n{}\n</tool_call>", json!({"name": name, "arguments": args}))
}

pub fn read_tool() -> Value {
    json!({"type": "function", "function": {"name": "read_file", "description": "Read a file from disk", "parameters": {"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}}})
}

pub fn run_tool() -> Value {
    json!({"type": "function", "function": {"name": "run_command", "description": "Run a shell command in the terminal", "parameters": {"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}}})
}

pub fn chat_tools() -> Value {
    json!([read_tool(), run_tool()])
}

pub fn resp_tools() -> Value {
    Value::Array(
        chat_tools()
            .as_array()
            .unwrap()
            .iter()
            .map(|t| {
                let mut f = t["function"].clone();
                f["type"] = json!("function");
                f
            })
            .collect(),
    )
}

pub fn anth_tools() -> Value {
    Value::Array(chat_tools().as_array().unwrap().iter().map(|t| json!({"name": t["function"]["name"], "description": t["function"]["description"], "input_schema": t["function"]["parameters"]})).collect())
}

pub fn user(text: &str) -> Value {
    json!([{"role": "user", "content": text}])
}

/// `&str` view of a JSON string (empty for anything else).
pub fn s(v: &Value) -> &str {
    v.as_str().unwrap_or("")
}

pub fn n(v: &Value) -> i64 {
    v.as_i64().unwrap_or_else(|| v.as_f64().map(|f| f as i64).unwrap_or(0))
}

/// Concatenated chat `delta.content` of a stream.
pub fn chat_stream_text(objects: &[Value]) -> String {
    objects.iter().filter_map(|d| d["choices"][0]["delta"]["content"].as_str()).collect()
}

/// Every chat `delta.tool_calls` entry of a stream, in order.
pub fn chat_stream_tool_calls(objects: &[Value]) -> Vec<Value> {
    objects.iter().flat_map(|d| d["choices"][0]["delta"]["tool_calls"].as_array().cloned().unwrap_or_default()).collect()
}

/// Every non-null chat `finish_reason` of a stream.
pub fn chat_stream_finish(objects: &[Value]) -> Vec<String> {
    objects.iter().filter_map(|d| d["choices"][0]["finish_reason"].as_str().map(String::from)).collect()
}

pub fn write_file(path: &Path, text: &str) {
    std::fs::File::create(path).unwrap().write_all(text.as_bytes()).unwrap();
}
