//! StackSpot AI backend: agents of the StackSpot AI platform through the Agent API
//! (one text prompt in, SSE text out). Client-credentials token (cached, renewed 60 s before expiry; a 401 forces one
//! renewal), retries (b, 2b, 4b on 429, 5xx and connection errors, only until the response starts) and the account's
//! queue.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use futures::StreamExt;
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};

use super::{Completion, Item, ItemStream};
use crate::canonical::{SharedMeta, Usage};
use crate::config::{BackendSettings, ConfigError};
use crate::errors::{BackendError, Error, NetError};
use crate::limiter::{monotonic, UpstreamLimiter};
use crate::store::time_now;
use crate::telemetry::Telemetry;
use crate::text::prefix;
const DEFAULT_AGENT_BASE: &str = "https://genai-inference-app.stackspot.com/v1/agent";
const DEFAULT_IDM_BASE: &str = "https://idm.stackspot.com";
static TOO_LONG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)limit of (\d+) tokens.*?resulted in (\d+) tokens").unwrap());

struct Token {
    value: String,
    expires_at: f64,
}

pub struct StackSpotBackend {
    pub name: String,
    pub type_: String,
    realm: String,
    client_id: String,
    client_secret: String,
    idm_base: String,
    agent_base: String,
    http: reqwest::Client,
    http_error: Option<String>,
    token: tokio::sync::Mutex<Token>,
    token_cache: std::sync::Mutex<(String, f64)>,
    pub limiter: Arc<UpstreamLimiter>,
    pub telemetry: Arc<Telemetry>,
    backoff_s: f64,
}

/// A token count as the Agent API sends it: a number, a numeric string or null.
fn count(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)).unwrap_or(0),
        Some(Value::String(s)) => s.trim().parse().unwrap_or(0),
        _ => 0,
    }
    .max(0)
}

/// The final event's `tokens` ({"input", "output"}) as usage; None when absent or zero.
fn usage_from(tokens: Option<&Value>) -> Option<Usage> {
    let (p, c) = (count(tokens.and_then(|t| t.get("input"))), count(tokens.and_then(|t| t.get("output"))));
    (p != 0 || c != 0).then(|| Usage::new(p, c))
}

/// An error body: JSON when it is JSON, else its text.
async fn body_of(r: reqwest::Response) -> Value {
    let bytes = r.bytes().await.unwrap_or_default();
    serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<Value>,
}

impl StackSpotBackend {
    pub fn new(settings: &BackendSettings, env: &indexmap::IndexMap<String, String>, telemetry: Arc<Telemetry>, backoff_s: f64) -> Self {
        let o = &settings.options;
        // an environment variable wins over the [backends.<name>] option
        let get = |key: &str, env_name: &str| -> Option<String> {
            env.get(env_name).filter(|v| !v.is_empty()).cloned().or_else(|| match o.get(key) {
                Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
                Some(Value::Null) | None => None,
                Some(Value::String(_)) => None,
                Some(other) => Some(other.to_string()),
            })
        };
        let s = |key: &str, env_name: &str, default: &str| -> String { get(key, env_name).unwrap_or_else(|| default.to_string()) };
        let limiter = Arc::new(UpstreamLimiter::new(&settings.limits, telemetry.clone(), &settings.name));
        let ca_bundle = get("ca_bundle", "STACKSPOT_CA_BUNDLE");
        let n = (limiter.max_concurrent + 2).max(4) as usize;
        let mut builder = reqwest::Client::builder()
            .user_agent(format!("midir/{}", crate::buildinfo::VERSION))
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(600))
            .pool_idle_timeout(Duration::from_secs(60))
            .pool_max_idle_per_host(n)
            .tcp_nodelay(true);
        let mut http_error = None;
        if let Some(path) = &ca_bundle {
            match std::fs::read(path)
                .map_err(|e| e.to_string())
                .and_then(|pem| reqwest::Certificate::from_pem_bundle(&pem).map_err(|e| e.to_string()))
            {
                Ok(certs) => {
                    builder = builder.tls_built_in_root_certs(false);
                    for c in certs {
                        builder = builder.add_root_certificate(c);
                    }
                }
                Err(e) => http_error = Some(format!("cannot read ca_bundle {path}: {e}")),
            }
        }
        let http = builder.build().unwrap_or_default();
        StackSpotBackend {
            name: settings.name.clone(),
            type_: "stackspot".into(),
            realm: s("realm", "STACKSPOT_REALM", "").trim().to_string(),
            client_id: s("client_id", "STACKSPOT_CLIENT_ID", "").trim().to_string(),
            client_secret: s("client_secret", "STACKSPOT_CLIENT_SECRET", "").trim().to_string(),
            idm_base: s("idm_base_url", "STACKSPOT_IDM_BASE_URL", DEFAULT_IDM_BASE).trim_end_matches('/').to_string(),
            agent_base: s("agent_base_url", "STACKSPOT_AGENT_BASE_URL", DEFAULT_AGENT_BASE).trim_end_matches('/').to_string(),
            http,
            http_error,
            token: tokio::sync::Mutex::new(Token { value: String::new(), expires_at: 0.0 }),
            token_cache: std::sync::Mutex::new((String::new(), 0.0)),
            limiter,
            telemetry,
            backoff_s,
        }
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let missing: Vec<&str> = [
            ("realm (STACKSPOT_REALM)", &self.realm),
            ("client_id (STACKSPOT_CLIENT_ID)", &self.client_id),
            ("client_secret (STACKSPOT_CLIENT_SECRET)", &self.client_secret),
        ]
        .iter()
        .filter(|(_, v)| v.is_empty())
        .map(|(n, _)| *n)
        .collect();
        if !missing.is_empty() {
            return Err(ConfigError(format!("backend {:?} (stackspot): missing {}", self.name, missing.join(", "))));
        }
        if let Some(e) = &self.http_error {
            return Err(ConfigError(format!("backend {:?} (stackspot): {e}", self.name)));
        }
        Ok(())
    }

    pub fn idm_url(&self) -> String {
        format!("{}/{}/oidc/oauth/token", self.idm_base, self.realm)
    }

    pub fn agent_url(&self, agent_id: &str) -> String {
        format!("{}/{agent_id}/chat", self.agent_base)
    }

    pub fn describe_target(&self, target: &str) -> String {
        format!("{}...", prefix(target, 6))
    }

    fn cached_token(&self) -> Option<String> {
        let c = self.token_cache.lock().unwrap_or_else(|e| e.into_inner());
        (!c.0.is_empty() && c.1 - 60.0 > time_now()).then(|| c.0.clone())
    }

    /// Client-credentials token, cached and renewed 60 s before expiry (tokens last 20 min).
    pub async fn token(&self, force: bool) -> Result<String, Error> {
        if !force {
            if let Some(t) = self.cached_token() {
                return Ok(t);
            }
        }
        let mut tok = self.token.lock().await;
        if !force && !tok.value.is_empty() && tok.expires_at - 60.0 > time_now() {
            return Ok(tok.value.clone());
        }
        let form =
            [("grant_type", "client_credentials"), ("client_id", self.client_id.as_str()), ("client_secret", self.client_secret.as_str())];
        let r = self.http.post(self.idm_url()).form(&form).send().await.map_err(|e| NetError::from_reqwest(&e, false))?;
        let status = r.status().as_u16();
        if status != 200 {
            return Err(BackendError::new(status, body_of(r).await, "idm", &self.name).into());
        }
        let bytes = r.bytes().await.map_err(|e| NetError::from_reqwest(&e, true))?;
        let answer: TokenResponse =
            serde_json::from_slice(&bytes).map_err(|e| Error::Internal(format!("{} idm: unexpected token response ({e})", self.name)))?;
        let expires_in = match &answer.expires_in {
            None | Some(Value::Null) => 300,
            some => count(some.as_ref()).max(1),
        };
        tok.value = answer.access_token;
        tok.expires_at = time_now() + expires_in as f64;
        *self.token_cache.lock().unwrap_or_else(|e| e.into_inner()) = (tok.value.clone(), tok.expires_at);
        tracing::info!("{} token renewed, expires in {expires_in}s", self.name);
        Ok(tok.value.clone())
    }

    pub async fn ready(&self) -> Result<(), Error> {
        self.token(false).await.map(|_| ())
    }

    /// (limit, actual) input tokens when `error` is the backend refusing a prompt for its size.
    pub fn input_limit_exceeded(&self, error: &BackendError) -> Option<(i64, i64)> {
        if error.status != 400 {
            return None;
        }
        let body = match &error.body {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let c = TOO_LONG_RE.captures(&body)?;
        Some((c.get(1)?.as_str().parse().ok()?, c.get(2)?.as_str().parse().ok()?))
    }

    async fn open_once(&self, target: &str, prompt: &str, deadline: f64) -> Result<reqwest::Response, Error> {
        let body =
            json!({"streaming": true, "user_prompt": prompt, "stackspot_knowledge": false, "return_ks_in_response": false}).to_string();
        let mut resp = None;
        for attempt in 1..=2 {
            let token = self.token(attempt == 2).await?;
            self.limiter.start(deadline).await?;
            let r = self
                .http
                .post(self.agent_url(target))
                .header("Authorization", format!("Bearer {token}"))
                .header("Content-Type", "application/json")
                .header("Accept", "text/event-stream")
                .body(body.clone())
                .send()
                .await
                .map_err(|e| NetError::from_reqwest(&e, false))?;
            if r.status().as_u16() == 401 && attempt == 1 {
                drop(r);
                tracing::warn!("{}: 401 from the agent; renewing the token", self.name);
                continue;
            }
            resp = Some(r);
            break;
        }
        let r = resp.ok_or_else(|| Error::Internal("no response".into()))?;
        let status = r.status().as_u16();
        if status != 200 {
            if status == 429 {
                self.limiter.on_429();
            }
            return Err(BackendError::new(status, body_of(r).await, "agent", &self.name).into());
        }
        Ok(r)
    }

    fn retryable(e: &Error) -> bool {
        match e {
            Error::Backend(b) => b.retryable(),
            Error::Net(n) => n.retryable,
            _ => false,
        }
    }

    /// Text deltas, then one Completion. Waits in the queue first; retries only until the response headers arrive.
    pub async fn stream(self: &Arc<Self>, prompt: &str, target: &str, meta: Option<SharedMeta>) -> Result<ItemStream, Error> {
        let t_queue = monotonic();
        let deadline = t_queue + self.limiter.timeout;
        let slot = self.limiter.acquire_slot(deadline).await?;
        let queued = monotonic() - t_queue;
        if let Some(m) = &meta {
            let mut m = m.lock().unwrap_or_else(|e| e.into_inner());
            m.queue_wait_ms = Some(m.queue_wait_ms.unwrap_or(0.0) + queued * 1000.0);
        }
        self.telemetry.queue_wait(queued);
        if queued >= 1.0 {
            tracing::info!("queued {queued:.1}s for a {} slot ({})", self.name, self.limiter.state());
        }
        let mut attempt = 0;
        let r = loop {
            attempt += 1;
            match self.open_once(target, prompt, deadline).await {
                Ok(r) => break r,
                Err(e) if Self::retryable(&e) && attempt < 4 => {
                    let b = self.backoff_s;
                    let wait = (b * 2f64.powi(attempt - 1)).min(4.0 * b).max(b).max(0.0);
                    tracing::warn!("{} attempt {attempt} failed ({e}); waiting {wait:.1}s", self.name);
                    let status = match &e {
                        Error::Backend(be) => be.status.to_string(),
                        other => other.telemetry_type(),
                    };
                    self.telemetry.upstream_retry(&status);
                    tokio::time::sleep(Duration::from_secs_f64(wait)).await;
                }
                Err(e) => return Err(e),
            }
        };
        Ok(read_sse(r, slot))
    }
}

/// SSE lines from a byte stream: a line ends at "\n", "\r\n" or "\r" (a trailing "\r" waits for a possible "\n").
#[derive(Default)]
struct Lines {
    buf: Vec<u8>,
}

impl Lines {
    fn feed(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(chunk);
        self.take(false)
    }

    fn finish(&mut self) -> Vec<String> {
        self.take(true)
    }

    fn take(&mut self, flush: bool) -> Vec<String> {
        let mut out = vec![];
        let mut start = 0;
        let mut i = 0;
        while i < self.buf.len() {
            match self.buf[i] {
                b'\n' => {
                    out.push(String::from_utf8_lossy(&self.buf[start..i]).into_owned());
                    start = i + 1;
                }
                b'\r' if i + 1 == self.buf.len() && !flush => break, // wait: it may be "\r\n"
                b'\r' => {
                    out.push(String::from_utf8_lossy(&self.buf[start..i]).into_owned());
                    start = if self.buf.get(i + 1) == Some(&b'\n') { i + 2 } else { i + 1 };
                    i = start;
                    continue;
                }
                _ => {}
            }
            i += 1;
        }
        self.buf.drain(..start);
        if flush && !self.buf.is_empty() {
            out.push(String::from_utf8_lossy(&std::mem::take(&mut self.buf)).into_owned());
        }
        out
    }
}

enum Line {
    Item(Item),
    Done(Completion),
    Skip,
}

fn parse_line(line: &str) -> Line {
    let Some(payload) = line.strip_prefix("data:") else { return Line::Skip };
    let payload = payload.trim();
    if payload.is_empty() {
        return Line::Skip;
    }
    let Ok(ev) = serde_json::from_str::<Value>(payload) else {
        tracing::warn!("ignoring non-JSON SSE event: {:?}", prefix(payload, 200));
        return Line::Skip;
    };
    let Value::Object(ev) = ev else {
        tracing::warn!("ignoring SSE event that is not an object: {:?}", prefix(payload, 200));
        return Line::Skip;
    };
    if ev.contains_key("stop_reason") || ev.contains_key("tokens") {
        let message_id = ev.get("message_id").and_then(Value::as_str).map(String::from);
        return Line::Done(Completion { usage: usage_from(ev.get("tokens")), message_id });
    }
    match ev.get("message") {
        Some(Value::String(m)) if !m.is_empty() => Line::Item(Item::Text(m.clone())),
        Some(Value::String(_)) | None | Some(Value::Null) => Line::Skip,
        Some(_) => {
            tracing::warn!("ignoring SSE event with a non-text message: {:?}", prefix(payload, 200));
            Line::Skip
        }
    }
}

/// SSE events: deltas carry only `message`; the final one carries stop_reason, message_id and tokens. The slot is
/// released when the stream is dropped.
fn read_sse(r: reqwest::Response, slot: crate::limiter::SlotGuard) -> ItemStream {
    Box::pin(async_stream::try_stream! {
        let _slot = slot;
        let mut body = r.bytes_stream();
        let mut lines = Lines::default();
        let mut done = false;
        loop {
            let chunk = match body.next().await {
                Some(Ok(c)) => c,
                Some(Err(e)) => Err(Error::Net(NetError::from_reqwest(&e, true)))?,
                None => break,
            };
            for line in lines.feed(&chunk) {
                match parse_line(&line) {
                    Line::Item(i) => yield i,
                    Line::Done(c) => {
                        done = true;
                        yield Item::Completion(c);
                    }
                    Line::Skip => {}
                }
            }
        }
        for line in lines.finish() {
            match parse_line(&line) {
                Line::Item(i) => yield i,
                Line::Done(c) => {
                    done = true;
                    yield Item::Completion(c);
                }
                Line::Skip => {}
            }
        }
        if !done {
            tracing::warn!("stream ended without a final event");
            yield Item::Completion(Completion::default());
        }
    })
}
