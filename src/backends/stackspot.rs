//! StackSpot AI backend: agents of the StackSpot AI platform through the Agent API
//! (one text prompt in, SSE text out). Client-credentials token (cached, renewed 60 s before expiry; a 401 forces one
//! renewal), retries (b, 2b, 4b on 429, 5xx and connection errors, only until the response starts) and the account's
//! queue.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use futures::StreamExt;
use regex::Regex;
use serde_json::{json, Value};

use super::{Completion, Item, ItemStream};
use crate::canonical::{SharedMeta, Usage};
use crate::config::{BackendSettings, ConfigError};
use crate::errors::{BackendError, Error, NetError};
use crate::limiter::{monotonic, UpstreamLimiter};
use crate::py::json as pyjson;
use crate::py::text;
use crate::store::time_now;
use crate::telemetry::Telemetry;

const LOG: &str = "midir.backends.stackspot";
const LOG_BASE: &str = "midir.backends.base";
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

/// The final event's `tokens` ({"input", "output"}) as usage; None when absent or zero.
fn usage_from(tokens: Option<&Value>) -> Option<Usage> {
    let t = tokens.and_then(Value::as_object);
    let num = |k: &str| -> i64 {
        let v = t.and_then(|m| m.get(k)).cloned().unwrap_or(Value::Null);
        let v = if text::truthy(&v) { v } else { json!(0) };
        let n = match &v {
            Value::String(s) => text::parse_int(s),
            Value::Number(_) | Value::Bool(_) => text::int_of(&v),
            _ => None,
        };
        n.unwrap_or(0).max(0)
    };
    let (p, c) = (num("input"), num("output"));
    if p == 0 && c == 0 {
        None
    } else {
        Some(Usage { prompt_tokens: p, completion_tokens: c, total_tokens: p + c })
    }
}

fn headers_of(r: &reqwest::Response) -> Vec<(String, String)> {
    r.headers().iter().map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned())).collect()
}

async fn body_of(r: reqwest::Response) -> Value {
    let bytes = r.bytes().await.unwrap_or_default();
    let s = String::from_utf8_lossy(&bytes).into_owned();
    pyjson::loads(&s).unwrap_or(Value::String(s))
}

impl StackSpotBackend {
    pub fn new(settings: &BackendSettings, env: &indexmap::IndexMap<String, String>, telemetry: Arc<Telemetry>, backoff_s: f64) -> Self {
        let o = &settings.options;
        let get = |key: &str, env_name: &str| -> Option<Value> {
            let v = match env.get(env_name).filter(|v| !v.is_empty()) {
                Some(e) => Some(Value::String(e.clone())),
                None => o.get(key).cloned(),
            };
            v.filter(|v| !v.is_null() && v.as_str() != Some(""))
        };
        let s = |key: &str, env_name: &str, default: &str| -> String {
            get(key, env_name).map(|v| text::str_of(&v)).unwrap_or_else(|| default.to_string())
        };
        let limiter = Arc::new(UpstreamLimiter::new(&settings.limits, telemetry.clone(), &settings.name));
        let ca_bundle = get("ca_bundle", "STACKSPOT_CA_BUNDLE").map(|v| text::str_of(&v));
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
            realm: text::strip(&s("realm", "STACKSPOT_REALM", "")).to_string(),
            client_id: text::strip(&s("client_id", "STACKSPOT_CLIENT_ID", "")).to_string(),
            client_secret: text::strip(&s("client_secret", "STACKSPOT_CLIENT_SECRET", "")).to_string(),
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
            return Err(ConfigError(format!("backend {} (stackspot): missing {}", text::repr_str(&self.name), missing.join(", "))));
        }
        if let Some(e) = &self.http_error {
            return Err(ConfigError(format!("backend {} (stackspot): {e}", text::repr_str(&self.name))));
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
        format!("{}...", text::head(target, 6))
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
            let headers = headers_of(&r);
            return Err(BackendError::new(status, body_of(r).await, headers, "idm", &self.name).into());
        }
        let bytes = r.bytes().await.map_err(|e| NetError::from_reqwest(&e, true))?;
        let j = pyjson::loads(&String::from_utf8_lossy(&bytes))
            .map_err(|e| Error::Internal(format!("JSONDecodeError({})", text::repr_str(&e.text))))?;
        let access = j.get("access_token").ok_or_else(|| Error::Internal("KeyError('access_token')".into()))?;
        let expires = j.get("expires_in").cloned().unwrap_or(json!(300));
        let expires_n = text::int_of(&expires).ok_or_else(|| Error::Internal(format!("ValueError({})", text::repr(&expires))))?;
        tok.value = text::str_of(access);
        tok.expires_at = time_now() + expires_n as f64;
        *self.token_cache.lock().unwrap_or_else(|e| e.into_inner()) = (tok.value.clone(), tok.expires_at);
        crate::info!(LOG, "{} token renewed, expires in {}s", self.name, j.get("expires_in").map_or("None".into(), text::str_of));
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
            other => pyjson::dumps(other, pyjson::ASCII),
        };
        let c = TOO_LONG_RE.captures(&body)?;
        Some((c.get(1)?.as_str().parse().ok()?, c.get(2)?.as_str().parse().ok()?))
    }

    async fn open_once(&self, target: &str, prompt: &str, deadline: f64) -> Result<reqwest::Response, Error> {
        let body = pyjson::dumps(
            &json!({"streaming": true, "user_prompt": prompt, "stackspot_knowledge": false, "return_ks_in_response": false}),
            pyjson::COMPACT,
        );
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
                crate::warn!(LOG, "{}: 401 from the agent; renewing the token", self.name);
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
            let headers = headers_of(&r);
            let bytes = r.bytes().await.unwrap_or_default();
            let raw = String::from_utf8_lossy(&bytes).into_owned();
            let body = pyjson::loads(&raw).unwrap_or(Value::String(raw));
            return Err(BackendError::new(status, body, headers, "agent", &self.name).into());
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
            crate::info!(LOG, "queued {queued:.1}s for a {} slot ({})", self.name, pyjson::dumps(&self.limiter.state(), pyjson::DEFAULT));
        }
        let mut attempt = 0;
        let r = loop {
            attempt += 1;
            match self.open_once(target, prompt, deadline).await {
                Ok(r) => break r,
                Err(e) if Self::retryable(&e) && attempt < 4 => {
                    let b = self.backoff_s;
                    let wait = (b * 2f64.powi(attempt - 1)).min(4.0 * b).max(b).max(0.0);
                    let what = match &e {
                        Error::Backend(be) => format!("BackendError({})", text::repr_str(&be.describe())),
                        Error::Net(n) => n.repr(),
                        other => other.describe(),
                    };
                    crate::warn!(LOG_BASE, "{} attempt {attempt} failed ({what}); waiting {wait:.1}s", self.name);
                    let status = match &e {
                        Error::Backend(be) => be.status.to_string(),
                        Error::Net(n) => n.kind.clone(),
                        _ => "Exception".into(),
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

/// httpx's LineDecoder: `str.splitlines()` boundaries over a stream, a trailing "\r" waits for a possible "\n".
#[derive(Default)]
struct Lines {
    pending: Vec<u8>,
    buf: String,
}

impl Lines {
    fn decode(&mut self, chunk: &[u8]) -> Vec<String> {
        self.pending.extend_from_slice(chunk);
        let valid_up_to = match std::str::from_utf8(&self.pending) {
            Ok(_) => self.pending.len(),
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            Err(_) => self.pending.len(),
        };
        let rest = self.pending.split_off(valid_up_to);
        let text_ = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending = rest;
        self.buf.push_str(&text_);
        self.take_lines(false)
    }

    fn take_lines(&mut self, flush: bool) -> Vec<String> {
        let mut out = vec![];
        let mut start = 0;
        let b = self.buf.clone();
        let mut it = b.char_indices().peekable();
        while let Some((i, c)) = it.next() {
            if matches!(c, '\n' | '\r' | '\u{b}' | '\u{c}' | '\u{1c}' | '\u{1d}' | '\u{1e}' | '\u{85}' | '\u{2028}' | '\u{2029}') {
                if c == '\r' {
                    match it.peek() {
                        None if !flush => break, // wait: it may be "\r\n"
                        Some(&(_, '\n')) => {
                            out.push(b[start..i].to_string());
                            it.next();
                            start = i + 2;
                            continue;
                        }
                        _ => {}
                    }
                }
                out.push(b[start..i].to_string());
                start = i + c.len_utf8();
            }
        }
        self.buf = b[start..].to_string();
        if flush && !self.buf.is_empty() {
            out.push(std::mem::take(&mut self.buf));
        }
        out
    }

    fn flush(&mut self) -> Vec<String> {
        if !self.pending.is_empty() {
            let t = String::from_utf8_lossy(&self.pending).into_owned();
            self.pending.clear();
            self.buf.push_str(&t);
        }
        self.take_lines(true)
    }
}

enum Line {
    Item(Item),
    Done(Completion),
    Skip,
}

fn parse_line(line: &str) -> Line {
    let line = line.trim_end_matches('\r');
    let Some(payload) = line.strip_prefix("data:") else { return Line::Skip };
    let payload = text::strip(payload);
    if payload.is_empty() {
        return Line::Skip;
    }
    let ev = match pyjson::loads(payload) {
        Ok(v) => v,
        Err(_) => {
            crate::warn!(LOG, "ignoring non-JSON SSE event: {}", text::repr_str(text::head(payload, 200)));
            return Line::Skip;
        }
    };
    let Value::Object(ev) = ev else {
        crate::warn!(LOG, "ignoring SSE event that is not an object: {}", text::repr_str(text::head(payload, 200)));
        return Line::Skip;
    };
    if ev.contains_key("stop_reason") || ev.contains_key("tokens") {
        return Line::Done(Completion {
            usage: usage_from(ev.get("tokens")),
            message_id: ev.get("message_id").cloned().unwrap_or(Value::Null),
            stop_reason: ev.get("stop_reason").cloned().unwrap_or(Value::Null),
        });
    }
    match ev.get("message") {
        Some(Value::String(m)) if !m.is_empty() => Line::Item(Item::Text(m.clone())),
        Some(Value::String(_)) | None | Some(Value::Null) => Line::Skip,
        Some(_) => {
            crate::warn!(LOG, "ignoring SSE event with a non-text message: {}", text::repr_str(text::head(payload, 200)));
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
            for line in lines.decode(&chunk) {
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
        for line in lines.flush() {
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
            crate::warn!(LOG, "stream ended without a final event");
            yield Item::Completion(Completion { usage: None, message_id: Value::Null, stop_reason: json!("stop") });
        }
    })
}
