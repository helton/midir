//! Telemetry: one span and a few metrics per request, exported over OTLP/HTTP (protobuf) by a
//! background task every 5 s and flushed at shutdown. Off (and free) unless an OTLP endpoint is configured. Prompt
//! content is never exported. Also the request labels (client, session, initiator) shared by the span, the metrics and
//! the log line.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::http::HeaderMap;
use futures::stream::BoxStream;
use futures::StreamExt;
use regex::Regex;
use serde_json::Value;
use sha1::{Digest, Sha1};

use crate::canonical::{CanonicalRequest, CanonicalResponse, Event, Meta, SharedMeta};
use crate::config::ModelSpec;
use crate::errors::Error;
use crate::otlp::{self, AttrValue, Attrs, MetricData, Points, SpanData};
use crate::store::ResponseStore;
use crate::text::{prefix, round1, skip_chars};
const SYSTEM_MARKERS: [(&str, &str); 3] = [("Hermes Agent", "hermes"), ("OpenClaw", "openclaw"), ("DeepSeek Harness", "deepseek-harness")];
const SESSION_HEADERS: [&str; 7] = [
    "x-claude-code-session-id",
    "x-session-id",
    "session_id",
    "x-conversation-id",
    "conversation_id",
    "vscode-sessionid",
    "x-copilot-session-id",
];
const BUCKETS: [f64; 15] = [0.0, 5.0, 10.0, 25.0, 50.0, 75.0, 100.0, 250.0, 500.0, 750.0, 1000.0, 2500.0, 5000.0, 7500.0, 10000.0];
static VERSION_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"/(\d[\w.\-]*)").unwrap());

/// A header's (first) value as text.
pub fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers.get(name).map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
}

/// (client name, version) from the User-Agent and a few client headers; clients that only send their SDK's
/// User-Agent are recognized by the opening of their system prompt. Labels only.
pub fn client_of(headers: &HeaderMap, system: &str) -> (String, String) {
    let ua = header(headers, "user-agent").unwrap_or_default().trim().to_string();
    let low = ua.to_lowercase();
    let version = VERSION_RE.captures(&ua).and_then(|c| c.get(1)).map(|m| m.as_str().to_string()).unwrap_or_default();
    if low.contains("claude-cli") || low.contains("claude-code") {
        return (if low.contains("claude-vscode") { "claude-code-vscode" } else { "claude-code" }.into(), version);
    }
    if low.contains("codex") {
        return ("codex".into(), version);
    }
    if low.contains("opencode") {
        return ("opencode".into(), version);
    }
    if low.contains("aider") || low.contains("litellm") {
        return ("aider".into(), version);
    }
    if low.contains("copilot")
        || (low.starts_with("openai/") && (headers.contains_key("x-initiator") || headers.contains_key("x-interaction-type")))
    {
        return ("copilot".into(), version);
    }
    let head = prefix(system, 600);
    for (marker, name) in SYSTEM_MARKERS {
        if head.contains(marker) {
            return (name.into(), version);
        }
    }
    if low.contains("python-httpx") || low.contains("openai-python") || low.contains("anthropic-python") {
        return ("sdk-python".into(), version);
    }
    let first = ua.split('/').next().unwrap_or("");
    let name = prefix(first, 30).to_lowercase();
    (if name.is_empty() { "unknown".into() } else { name }, version)
}

/// Labels for one request: client, session, initiator, model, agent.
#[allow(clippy::too_many_arguments)]
pub fn request_meta(
    headers: &HeaderMap,
    body: &Value,
    protocol: &str,
    model: &str,
    route: &ModelSpec,
    req: &CanonicalRequest,
    rid: &str,
    store: &ResponseStore,
) {
    let (client, version) = client_of(headers, req.system.first().map(String::as_str).unwrap_or(""));
    let mut session: Option<String> = SESSION_HEADERS.iter().find_map(|h| header(headers, h).filter(|v| !v.is_empty()));
    if session.is_none() && protocol == "messages" {
        // Claude Code: metadata.user_id is a JSON string holding the session id
        session = body
            .pointer("/metadata/user_id")
            .and_then(Value::as_str)
            .and_then(|uid| serde_json::from_str::<Value>(uid).ok())
            .and_then(|v| v.get("session_id").and_then(Value::as_str).map(String::from))
            .filter(|s| !s.is_empty());
    }
    if session.is_none() && protocol == "responses" {
        session = ["prompt_cache_key", "user"]
            .iter()
            .find_map(|k| body.get(*k).and_then(Value::as_str).filter(|v| !v.is_empty()).map(String::from));
        if session.is_none() {
            if let Some(prev) = body.get("previous_response_id").and_then(Value::as_str).filter(|p| !p.is_empty()) {
                session = store.session_of(prev).filter(|s| !s.is_empty());
            }
        }
        if session.is_none() {
            session = Some(format!("chain-{}", prefix(skip_chars(rid, 5), 10)));
        }
    }
    let session = session.unwrap_or_else(|| {
        let first_user = req.turns.iter().find(|t| t.role == "user" && !t.text.is_empty()).map(|t| t.text.as_str()).unwrap_or("");
        if first_user.is_empty() {
            rid.to_string()
        } else {
            let digest = Sha1::digest(prefix(first_user, 500).as_bytes());
            format!("conv-{}", &crate::canonical::hex(&digest)[..10])
        }
    });
    let initiator = header(headers, "x-initiator").filter(|v| !v.is_empty()).unwrap_or_else(|| {
        if req.turns.last().map_or(false, |t| !t.tool_results.is_empty()) {
            "agent".into()
        } else {
            "user".into()
        }
    });
    let mut m = req.meta();
    m.client = client;
    m.client_version = version;
    m.session = prefix(&session, 64).to_string();
    m.initiator = initiator;
    m.protocol = protocol.into();
    m.model = model.into();
    m.agent = route.name.clone();
    m.backend = route.backend.clone();
    m.stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
    m.tools_declared = req.tools.len() as i64;
    m.json_mode = req.json_schema.is_some();
}

fn now_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64)
}

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    let _ = getrandom::getrandom(&mut b);
    b
}

fn s(v: &str) -> AttrValue {
    AttrValue::Str(v.into())
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Counter,
    UpDown,
    Histogram,
}

const INSTRUMENTS: [(&str, Kind, &str, &str); 12] = [
    ("midir.requests", Kind::Counter, "1", "LLM requests handled"),
    (
        "gen_ai.client.token.usage",
        Kind::Counter,
        "{token}",
        "Input and output tokens reported by the backend (estimated when it reports none)",
    ),
    ("midir.request.duration", Kind::Histogram, "ms", "Request duration, first byte in to last byte out"),
    ("midir.request.ttfb", Kind::Histogram, "ms", "Time to the first text or tool-call event"),
    ("midir.tool_calls", Kind::Counter, "1", "Tool calls emitted to the client"),
    ("midir.followups", Kind::Counter, "1", "Automatic follow-up calls (promise, incapacity, tool_choice retry)"),
    ("midir.upstream_retries", Kind::Counter, "1", "Retried backend attempts, by status"),
    ("midir.queue.wait", Kind::Histogram, "ms", "Time waiting in the backend queue (concurrency and requests/minute)"),
    ("midir.queue.waiting", Kind::UpDown, "1", "Requests currently waiting for a backend slot"),
    ("midir.errors", Kind::Counter, "1", "Requests that ended in an error"),
    ("midir.truncations", Kind::Counter, "1", "Upstream calls whose prompt lost old turns to the size cap"),
    ("midir.dropped_turns", Kind::Counter, "{turn}", "Old conversation turns dropped to fit the size cap"),
];

#[derive(Default)]
struct Hist {
    count: u64,
    sum: f64,
    buckets: Vec<u64>,
    min: f64,
    max: f64,
}

#[derive(Default)]
struct Series {
    attrs: Attrs,
    int: i64,
    hist: Hist,
}

fn attrs_key(a: &Attrs) -> String {
    let mut parts: Vec<String> = a.iter().map(|(k, v)| format!("{k}={v:?}")).collect();
    parts.sort();
    parts.join("\u{1}")
}

struct Exporter {
    resource: Attrs,
    traces_url: String,
    metrics_url: String,
    headers: Vec<(String, String)>,
    http: reqwest::Client,
    spans: Mutex<Vec<SpanData>>,
    series: Mutex<BTreeMap<&'static str, BTreeMap<String, Series>>>,
    start_ns: u64,
    closed: AtomicBool,
}

pub struct Telemetry {
    exp: Option<Arc<Exporter>>,
}

pub struct Span {
    trace_id: [u8; 16],
    span_id: [u8; 8],
    name: String,
    start_ns: u64,
    attrs: Attrs,
}

fn signal_url(base: &str, specific: Option<String>, path: &str) -> String {
    if let Some(u) = specific.filter(|u| !u.is_empty()) {
        return u;
    }
    if base.ends_with('/') {
        format!("{base}{path}")
    } else {
        format!("{base}/{path}")
    }
}

fn parse_headers(raw: &str) -> Vec<(String, String)> {
    raw.split(',')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

impl Telemetry {
    pub fn disabled() -> Self {
        Telemetry { exp: None }
    }

    pub fn new(endpoint: &str, service_name: &str) -> Self {
        if endpoint.is_empty() {
            return Self::disabled();
        }
        let env = |k: &str| std::env::var(k).ok();
        let base = env("OTEL_EXPORTER_OTLP_ENDPOINT").filter(|e| !e.is_empty()).unwrap_or_else(|| endpoint.to_string());
        let mut headers = parse_headers(&env("OTEL_EXPORTER_OTLP_HEADERS").unwrap_or_default());
        let mut resource: Attrs = vec![
            ("telemetry.sdk.language".into(), s("rust")),
            ("telemetry.sdk.name".into(), s("opentelemetry")),
            ("telemetry.sdk.version".into(), s("midir-otlp")),
        ];
        let mut id = random::<16>();
        id[6] = (id[6] & 0x0f) | 0x40; // a random (version 4) UUID
        id[8] = (id[8] & 0x3f) | 0x80;
        let hex = crate::canonical::hex(&id);
        let uuid = format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..]);
        resource.push(("service.instance.id".into(), AttrValue::Str(uuid)));
        for (k, v) in parse_headers(&env("OTEL_RESOURCE_ATTRIBUTES").unwrap_or_default()) {
            resource.retain(|(x, _)| *x != k);
            resource.push((k, AttrValue::Str(v)));
        }
        for (k, v) in [("service.name", service_name.to_string()), ("service.version", crate::buildinfo::full_version())] {
            resource.retain(|(x, _)| x != k);
            resource.push((k.into(), AttrValue::Str(v)));
        }
        headers.push(("Content-Type".into(), "application/x-protobuf".into()));
        let http = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().unwrap_or_default();
        let exp = Arc::new(Exporter {
            resource,
            traces_url: signal_url(&base, env("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT"), "v1/traces"),
            metrics_url: signal_url(&base, env("OTEL_EXPORTER_OTLP_METRICS_ENDPOINT"), "v1/metrics"),
            headers,
            http,
            spans: Mutex::new(vec![]),
            series: Mutex::new(BTreeMap::new()),
            start_ns: now_ns(),
            closed: AtomicBool::new(false),
        });
        tracing::info!("telemetry on: OTLP/HTTP -> {endpoint} (service {service_name})");
        Telemetry { exp: Some(exp) }
    }

    pub fn enabled(&self) -> bool {
        self.exp.is_some()
    }

    /// The periodic export (every 5 s); call once inside the runtime.
    pub fn spawn_exporter(&self) {
        if let Some(exp) = self.exp.clone() {
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(5));
                tick.tick().await;
                loop {
                    tick.tick().await;
                    if exp.closed.load(Ordering::SeqCst) {
                        return;
                    }
                    exp.export().await;
                }
            });
        }
    }

    /// Flush and stop the exporters; safe to call twice.
    pub async fn shutdown(&self) {
        if let Some(exp) = &self.exp {
            if !exp.closed.swap(true, Ordering::SeqCst) {
                let _ = tokio::time::timeout(Duration::from_secs(4), exp.export()).await;
            }
        }
    }

    fn record(&self, name: &'static str, attrs: Attrs, int: i64, val: f64) {
        let Some(exp) = &self.exp else { return };
        let kind = INSTRUMENTS.iter().find(|i| i.0 == name).map_or(Kind::Counter, |i| i.1);
        let mut all = exp.series.lock().unwrap_or_else(|e| e.into_inner());
        let series = all.entry(name).or_default().entry(attrs_key(&attrs)).or_insert_with(|| Series { attrs, ..Default::default() });
        match kind {
            Kind::Histogram => {
                let h = &mut series.hist;
                if h.buckets.is_empty() {
                    h.buckets = vec![0; BUCKETS.len() + 1];
                    h.min = f64::INFINITY;
                    h.max = f64::NEG_INFINITY;
                }
                h.count += 1;
                h.sum += val;
                h.min = h.min.min(val);
                h.max = h.max.max(val);
                let i = BUCKETS.iter().position(|b| val <= *b).unwrap_or(BUCKETS.len());
                h.buckets[i] += 1;
            }
            _ => series.int += int,
        }
    }

    pub fn queue_wait(&self, seconds: f64) {
        self.record("midir.queue.wait", vec![], 0, seconds * 1000.0);
    }

    pub fn queue_depth(&self, delta: i64) {
        self.record("midir.queue.waiting", vec![], delta, 0.0);
    }

    pub fn truncated(&self, meta: &Meta, turns: i64) {
        if self.enabled() {
            self.record("midir.truncations", labels(meta), 1, 0.0);
            self.record("midir.dropped_turns", labels(meta), turns, 0.0);
        }
    }

    pub fn upstream_retry(&self, status: &str) {
        self.record("midir.upstream_retries", vec![("midir.upstream_status".into(), s(status))], 1, 0.0);
    }

    pub fn begin(&self, meta: &Meta, rid: &str) -> Option<Span> {
        self.exp.as_ref()?;
        let attrs: Attrs = vec![
            ("gen_ai.system".into(), s(if meta.backend.is_empty() { "unknown" } else { &meta.backend })),
            ("gen_ai.operation.name".into(), s("chat")),
            ("gen_ai.request.model".into(), s(&meta.model)),
            ("gen_ai.response.model".into(), s(&meta.agent)),
            ("client.name".into(), s(&meta.client)),
            ("client.version".into(), s(&meta.client_version)),
            ("session.id".into(), s(&meta.session)),
            ("midir.request_id".into(), s(rid)),
            ("midir.protocol".into(), s(&meta.protocol)),
            ("midir.initiator".into(), s(&meta.initiator)),
            ("midir.stream".into(), AttrValue::Bool(meta.stream)),
            ("midir.tools_declared".into(), AttrValue::Int(meta.tools_declared)),
            ("midir.json_mode".into(), AttrValue::Bool(meta.json_mode)),
        ];
        Some(Span {
            trace_id: random::<16>(),
            span_id: random::<8>(),
            name: format!("gen_ai.chat {}", meta.protocol),
            start_ns: now_ns(),
            attrs,
        })
    }

    pub fn end(
        &self,
        span: Option<Span>,
        meta: &Meta,
        resp: Option<&CanonicalResponse>,
        t0: Instant,
        ttfb: Option<f64>,
        error: Option<String>,
    ) {
        let (Some(exp), Some(mut span)) = (&self.exp, span) else { return };
        let duration_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let lbl = labels(meta);
        let usage = resp.map(|r| r.usage).unwrap_or_default();
        let mut attrs: Attrs = vec![
            ("gen_ai.usage.input_tokens".into(), AttrValue::Int(usage.prompt_tokens)),
            ("gen_ai.usage.output_tokens".into(), AttrValue::Int(usage.completion_tokens)),
            ("midir.tool_calls".into(), AttrValue::Int(resp.map_or(0, |r| r.tool_calls.len() as i64))),
            ("midir.finish".into(), s(resp.map_or("error", |r| r.finish.as_str()))),
            ("midir.followups".into(), AttrValue::Int(meta.followups)),
            ("midir.parse_errors".into(), AttrValue::Int(meta.parse_errors)),
            ("midir.repairs".into(), AttrValue::Int(meta.repairs)),
            ("midir.upstream_calls".into(), AttrValue::Int(meta.upstream_calls)),
            ("midir.queue_wait_ms".into(), meta.queue_wait_ms.map_or(AttrValue::Int(0), |q| AttrValue::Double(round1(q)))),
            ("midir.prompt_chars".into(), AttrValue::Int(meta.prompt_chars)),
            ("midir.dropped_turns".into(), AttrValue::Int(meta.dropped_turns)),
            ("midir.duration_ms".into(), AttrValue::Double(round1(duration_ms))),
        ];
        if let Some(t) = ttfb {
            attrs.push(("midir.ttfb_ms".into(), AttrValue::Double(round1(t * 1000.0))));
        }
        if let Some(e) = &error {
            attrs.push(("error.type".into(), s(e)));
            let mut el = lbl.clone();
            el.push(("error.type".into(), s(e)));
            self.record("midir.errors", el, 1, 0.0);
        }
        for (k, v) in attrs {
            span.attrs.retain(|(x, _)| *x != k);
            span.attrs.push((k, v));
        }
        exp.spans.lock().unwrap_or_else(|e| e.into_inner()).push(SpanData {
            trace_id: span.trace_id,
            span_id: span.span_id,
            name: span.name,
            start_ns: span.start_ns,
            end_ns: now_ns(),
            attrs: span.attrs,
            error,
        });
        self.record("midir.requests", lbl.clone(), 1, 0.0);
        self.record("midir.request.duration", lbl.clone(), 0, duration_ms);
        if let Some(t) = ttfb {
            self.record("midir.request.ttfb", lbl.clone(), 0, t * 1000.0);
        }
        if let Some(r) = resp {
            let mut i = lbl.clone();
            i.push(("gen_ai.token.type".into(), s("input")));
            self.record("gen_ai.client.token.usage", i, usage.prompt_tokens, 0.0);
            let mut o = lbl.clone();
            o.push(("gen_ai.token.type".into(), s("output")));
            self.record("gen_ai.client.token.usage", o, usage.completion_tokens, 0.0);
            if !r.tool_calls.is_empty() {
                self.record("midir.tool_calls", lbl.clone(), r.tool_calls.len() as i64, 0.0);
            }
        }
        if meta.followups != 0 {
            self.record("midir.followups", lbl, meta.followups, 0.0);
        }
    }
}

fn labels(meta: &Meta) -> Attrs {
    vec![
        ("client.name".into(), s(&meta.client)),
        ("gen_ai.request.model".into(), s(&meta.model)),
        ("gen_ai.response.model".into(), s(&meta.agent)),
        ("midir.protocol".into(), s(&meta.protocol)),
        ("midir.initiator".into(), s(&meta.initiator)),
        ("session.id".into(), s(&meta.session)),
    ]
}

impl Exporter {
    async fn post(&self, url: &str, body: Vec<u8>) {
        let mut rb = self.http.post(url).body(body);
        for (k, v) in &self.headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        match rb.send().await {
            Ok(r) if r.status().is_success() => {}
            Ok(r) => tracing::warn!("OTLP export to {url} failed: HTTP {}", r.status().as_u16()),
            Err(e) => tracing::warn!("OTLP export to {url} failed: {e}"),
        }
    }

    async fn export(&self) {
        let spans: Vec<SpanData> = std::mem::take(&mut *self.spans.lock().unwrap_or_else(|e| e.into_inner()));
        if !spans.is_empty() {
            self.post(&self.traces_url, otlp::encode_traces(&self.resource, &spans)).await;
        }
        let metrics: Vec<MetricData> = {
            let all = self.series.lock().unwrap_or_else(|e| e.into_inner());
            INSTRUMENTS
                .iter()
                .filter_map(|(name, kind, unit, description)| {
                    let series = all.get(name).filter(|s| !s.is_empty())?;
                    let points = match kind {
                        Kind::Histogram => Points::Histogram {
                            bounds: BUCKETS.to_vec(),
                            points: series
                                .values()
                                .map(|s| (s.attrs.clone(), s.hist.count, s.hist.sum, s.hist.buckets.clone(), s.hist.min, s.hist.max))
                                .collect(),
                        },
                        k => Points::Sum {
                            monotonic: *k == Kind::Counter,
                            points: series.values().map(|s| (s.attrs.clone(), s.int)).collect(),
                        },
                    };
                    Some(MetricData { name, description, unit, points })
                })
                .collect()
        };
        if !metrics.is_empty() {
            self.post(&self.metrics_url, otlp::encode_metrics(&self.resource, &metrics, self.start_ns, now_ns())).await;
        }
    }
}

/// Ends the span when the observed stream ends or is dropped (client disconnect).
struct Observation {
    tel: Arc<Telemetry>,
    span: Option<Span>,
    meta: SharedMeta,
    t0: Instant,
    ttfb: Option<f64>,
    resp: Option<CanonicalResponse>,
    error: Option<String>,
    ended: bool,
}

impl Observation {
    fn finish(&mut self) {
        if self.ended {
            return;
        }
        self.ended = true;
        if self.resp.is_none() && self.error.is_none() {
            self.error = Some("client_disconnect".into());
        }
        let meta = self.meta.lock().unwrap_or_else(|e| e.into_inner()).clone();
        self.tel.end(self.span.take(), &meta, self.resp.as_ref(), self.t0, self.ttfb, self.error.take());
    }
}

impl Drop for Observation {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Pass-through for a streamed request: records TTFB, usage and outcome when the stream ends or breaks.
pub fn observe(
    tel: Arc<Telemetry>,
    events: BoxStream<'static, Result<Event, Error>>,
    req: &CanonicalRequest,
    rid: &str,
) -> BoxStream<'static, Result<Event, Error>> {
    if !tel.enabled() {
        return events;
    }
    let meta = req.meta.clone();
    let rid = rid.to_string();
    Box::pin(async_stream::stream! {
        let span = { let m = meta.lock().unwrap_or_else(|e| e.into_inner()).clone(); tel.begin(&m, &rid) };
        let mut obs = Observation { tel: tel.clone(), span, meta: meta.clone(), t0: Instant::now(), ttfb: None, resp: None, error: None, ended: false };
        let mut events = events;
        while let Some(ev) = events.next().await {
            match &ev {
                Ok(Event::Text(_) | Event::ToolCall(_)) if obs.ttfb.is_none() => obs.ttfb = Some(obs.t0.elapsed().as_secs_f64()),
                Ok(Event::Done(r)) => obs.resp = Some(r.clone()),
                Err(e) => obs.error = Some(e.telemetry_type()),
                _ => {}
            }
            let is_err = ev.is_err();
            yield ev;
            if is_err {
                break;
            }
        }
        obs.finish();
    })
}

/// Same for a non-streaming request (no TTFB).
pub async fn observe_complete<F>(tel: &Telemetry, fut: F, req: &CanonicalRequest, rid: &str) -> Result<CanonicalResponse, Error>
where
    F: std::future::Future<Output = Result<CanonicalResponse, Error>>,
{
    if !tel.enabled() {
        return fut.await;
    }
    let span = tel.begin(&req.meta(), rid);
    let t0 = Instant::now();
    let r = fut.await;
    let meta = req.meta().clone();
    match &r {
        Ok(resp) => tel.end(span, &meta, Some(resp), t0, None, None),
        Err(e) => tel.end(span, &meta, None, t0, None, Some(e.telemetry_type())),
    }
    r
}
