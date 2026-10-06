//! StackSpot AI backend: agents of the StackSpot AI platform through the Agent API
//! (one text prompt in, SSE text out). Client-credentials token (cached, renewed 60 s before expiry; a 401 forces one
//! renewal), retries (b, 2b, 4b on 429, 5xx and connection errors, only until the response starts) and the account's
//! queue.

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use bytes::Bytes;
use futures::StreamExt;
use futures::future::BoxFuture;
use regex::Regex;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use super::{Completion, Item, ItemStream, TextBackend};
use crate::canonical::{SharedMeta, Usage};
use crate::config::{BackendSettings, ConfigError};
use crate::errors::{BackendError, Error, NetError};
use crate::json;
use crate::limiter::UpstreamLimiter;
use crate::store::time_now;
use crate::telemetry::Telemetry;
use crate::text::{char_len, prefix};

const DEFAULT_AGENT_BASE: &str = "https://genai-inference-app.stackspot.com/v1/agent";
const DEFAULT_IDM_BASE: &str = "https://idm.stackspot.com";
/// The options a [backends.<name>] table of type "stackspot" understands (anything else is reported at startup).
const OPTIONS: [&str; 7] = ["type", "realm", "client_id", "client_secret", "idm_base_url", "agent_base_url", "ca_bundle"];
/// The token call as a whole (connect, send, answer): a hung idm must not hold every request for minutes.
const TOKEN_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a failed token renewal is answered again before idm is asked anew.
const TOKEN_FAILURE_TTL: Duration = Duration::from_secs(5);
static TOO_LONG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)limit of (\d+) tokens.*?resulted in (\d+) tokens").unwrap());

struct Token {
    value: String,
    expires_at: f64,
    /// the last failed renewal, answered again for a few seconds instead of asking idm once per caller
    failed: Option<(std::time::Instant, Error)>,
}

pub struct StackSpotBackend {
    name: String,
    realm: String,
    client_id: String,
    client_secret: String,
    idm_base: String,
    agent_base: String,
    http: reqwest::Client,
    http_error: Option<String>,
    token: tokio::sync::Mutex<Token>,
    token_cache: std::sync::Mutex<(String, f64)>,
    limiter: UpstreamLimiter,
    telemetry: Arc<Telemetry>,
    backoff_s: f64,
}

/// The Agent API request: the prompt is serialized once, straight from the engine's string.
#[derive(Serialize)]
struct AgentRequest<'a> {
    streaming: bool,
    user_prompt: &'a str,
    stackspot_knowledge: bool,
    return_ks_in_response: bool,
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
        for key in o.keys().filter(|k| !OPTIONS.contains(&k.as_str())) {
            tracing::warn!("backend {:?}: unknown option {key:?} ignored (stackspot options: {})", settings.name, OPTIONS[1..].join(", "));
        }
        let limiter = UpstreamLimiter::new(&settings.limits, telemetry.clone(), &settings.name);
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
                // added to the system's trust store (a corporate CA next to the public roots)
                Ok(certs) => builder = builder.tls_certs_merge(certs),
                Err(e) => http_error = Some(format!("cannot read ca_bundle {path}: {e}")),
            }
        }
        let http = builder.build().unwrap_or_default();
        StackSpotBackend {
            name: settings.name.clone(),
            realm: s("realm", "STACKSPOT_REALM", "").trim().to_string(),
            client_id: s("client_id", "STACKSPOT_CLIENT_ID", "").trim().to_string(),
            client_secret: s("client_secret", "STACKSPOT_CLIENT_SECRET", "").trim().to_string(),
            idm_base: s("idm_base_url", "STACKSPOT_IDM_BASE_URL", DEFAULT_IDM_BASE).trim_end_matches('/').to_string(),
            agent_base: s("agent_base_url", "STACKSPOT_AGENT_BASE_URL", DEFAULT_AGENT_BASE).trim_end_matches('/').to_string(),
            http,
            http_error,
            token: tokio::sync::Mutex::new(Token { value: String::new(), expires_at: 0.0, failed: None }),
            token_cache: std::sync::Mutex::new((String::new(), 0.0)),
            limiter,
            telemetry,
            backoff_s,
        }
    }

    fn check(&self) -> Result<(), ConfigError> {
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

    fn cached_token(&self) -> Option<String> {
        let c = self.token_cache.lock().unwrap_or_else(|e| e.into_inner());
        (!c.0.is_empty() && c.1 - 60.0 > time_now()).then(|| c.0.clone())
    }

    /// Client-credentials token, cached and renewed 60 s before expiry (tokens last 20 min). `stale` is a token the
    /// agent refused (401): it is renewed unless another caller already did. A failed renewal is answered again for
    /// `TOKEN_FAILURE_TTL` instead of asking idm once per caller (an open `/ready` polled in a loop, a wave of 401s).
    pub async fn token(&self, stale: Option<&str>) -> Result<String, Error> {
        if stale.is_none()
            && let Some(t) = self.cached_token()
        {
            return Ok(t);
        }
        let mut tok = self.token.lock().await;
        let fresh = !tok.value.is_empty() && tok.expires_at - 60.0 > time_now();
        if fresh && stale.is_none_or(|s| s != tok.value) {
            return Ok(tok.value.clone()); // still valid, or renewed by another caller since `stale` was handed out
        }
        if let Some((at, e)) = &tok.failed
            && at.elapsed() < TOKEN_FAILURE_TTL
        {
            return Err(e.clone());
        }
        let renewed = self.renew_token(&mut tok).await;
        tok.failed = renewed.as_ref().err().map(|e| (std::time::Instant::now(), e.clone()));
        renewed
    }

    async fn renew_token(&self, tok: &mut Token) -> Result<String, Error> {
        let form =
            [("grant_type", "client_credentials"), ("client_id", self.client_id.as_str()), ("client_secret", self.client_secret.as_str())];
        let r = self
            .http
            .post(self.idm_url())
            .form(&form)
            .timeout(TOKEN_TIMEOUT)
            .send()
            .await
            .map_err(|e| NetError::from_reqwest(&e, false))?;
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

    async fn open_once(&self, target: &str, body: &Bytes, deadline: tokio::time::Instant) -> Result<reqwest::Response, Error> {
        let mut resp = None;
        let mut refused: Option<String> = None;
        for attempt in 1..=2 {
            let token = self.token(refused.as_deref()).await?;
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
                refused = Some(token);
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
                // Retry-After in seconds (an HTTP date is rare from APIs and ignored)
                let retry_after =
                    r.headers().get(reqwest::header::RETRY_AFTER).and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse::<f64>().ok());
                self.limiter.on_429(retry_after);
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
    async fn open(&self, prompt: &str, target: &str, meta: Option<SharedMeta>) -> Result<ItemStream, Error> {
        let body = Bytes::from(
            serde_json::to_vec(&AgentRequest {
                streaming: true,
                user_prompt: prompt,
                stackspot_knowledge: false,
                return_ks_in_response: false,
            })
            .map_err(|e| Error::Internal(format!("cannot encode the agent request: {e}")))?,
        );
        let t_queue = tokio::time::Instant::now();
        let deadline = self.limiter.deadline();
        let slot = self.limiter.acquire_slot(deadline).await?;
        let queued = t_queue.elapsed().as_secs_f64();
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
            match self.open_once(target, &body, deadline).await {
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

impl TextBackend for StackSpotBackend {
    fn name(&self) -> &str {
        &self.name
    }

    fn kind(&self) -> &'static str {
        "stackspot"
    }

    fn limiter(&self) -> &UpstreamLimiter {
        &self.limiter
    }

    fn validate(&self) -> Result<(), ConfigError> {
        self.check()
    }

    fn describe_target(&self, target: &str) -> String {
        format!("{}...", prefix(target, 6))
    }

    fn ready(&self) -> BoxFuture<'_, Result<(), Error>> {
        Box::pin(async move { self.token(None).await.map(|_| ()) })
    }

    fn stream<'a>(&'a self, prompt: &'a str, target: &'a str, meta: Option<SharedMeta>) -> BoxFuture<'a, Result<ItemStream, Error>> {
        Box::pin(self.open(prompt, target, meta))
    }

    fn input_limit_exceeded(&self, error: &BackendError) -> Option<(i64, i64)> {
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

/// One Agent API event: deltas carry `message`; the final one `stop_reason`, `message_id` and `tokens` (present, even
/// as null, is what marks it).
#[derive(Deserialize)]
struct AgentEvent {
    message: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    stop_reason: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    tokens: Option<Value>,
    message_id: Option<Value>,
}

/// Some(value) whenever the field is there, null included.
fn present<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

/// One SSE line. Model output is not logged at WARN (it may hold secrets): problems are described by size, the content
/// goes to DEBUG.
fn parse_line(line: &str) -> Line {
    let Some(payload) = line.strip_prefix("data:") else { return Line::Skip };
    let payload = payload.trim();
    if payload.is_empty() {
        return Line::Skip;
    }
    // a lone UTF-16 surrogate or a NaN would make the whole event, and its text, unreadable
    let ev: AgentEvent = match serde_json::from_slice(&json::sanitize(payload.as_bytes())) {
        Ok(ev) => ev,
        Err(e) => {
            tracing::warn!("ignoring an SSE event that is not a JSON object ({} chars: {e})", char_len(payload));
            tracing::debug!("ignored SSE event: {:?}", prefix(payload, 2000));
            return Line::Skip;
        }
    };
    if ev.stop_reason.is_some() || ev.tokens.is_some() {
        let message_id = ev.message_id.as_ref().and_then(Value::as_str).map(String::from);
        return Line::Done(Completion { usage: usage_from(ev.tokens.as_ref()), message_id });
    }
    match ev.message {
        Some(Value::String(m)) if !m.is_empty() => Line::Item(Item::Text(m)),
        Some(Value::String(_)) | None | Some(Value::Null) => Line::Skip,
        Some(other) => {
            tracing::warn!("ignoring an SSE event whose message is not text ({} chars)", char_len(payload));
            tracing::debug!("ignored SSE message: {:?}", prefix(&other.to_string(), 2000));
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

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn lines_are_the_same_in_any_chunking(body in "(data: [a-zé{}\":]{0,12}(\r\n|\n|\r){1,2}){0,8}", cuts in prop::collection::vec(0usize..200, 0..10)) {
            let bytes = body.as_bytes();
            let mut whole = Lines::default();
            let mut expected = whole.feed(bytes);
            expected.extend(whole.finish());
            let mut bounds: Vec<usize> = cuts.iter().map(|c| c % (bytes.len() + 1)).collect();
            bounds.extend([0, bytes.len()]);
            bounds.sort_unstable();
            bounds.dedup();
            let mut chunked = Lines::default();
            let mut got = vec![];
            for w in bounds.windows(2) {
                got.extend(chunked.feed(&bytes[w[0]..w[1]]));
            }
            got.extend(chunked.finish());
            prop_assert_eq!(got, expected);
        }
    }

    #[test]
    fn events_are_read_whatever_their_shape() {
        assert!(matches!(parse_line("data: {\"message\": \"hi\"}"), Line::Item(Item::Text(t)) if t == "hi"));
        assert!(matches!(parse_line("data: {\"stop_reason\": null}"), Line::Done(_)));
        assert!(
            matches!(parse_line("data: {\"tokens\": {\"input\": \"7\", \"output\": 2}}"), Line::Done(c) if c.usage == Some(Usage::new(7, 2)))
        );
        for skipped in ["data: [1]", "data: nope", "event: x", "data: {\"message\": {\"a\": 1}}", "data: {\"message\": null}"] {
            assert!(matches!(parse_line(skipped), Line::Skip), "{skipped}");
        }
    }
}
