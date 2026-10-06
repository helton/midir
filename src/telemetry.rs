//! Telemetry: a span per request (kind SERVER, the child of the client's `traceparent` when it sends one) with a
//! CLIENT child span per backend call, and a few metrics. Spans and metrics go out over OTLP/HTTP (protobuf) from a
//! background task every 5 s and at shutdown; metrics can also be scraped at `/metrics` (Prometheus text). Off, and
//! free, unless one of them is configured. Prompt content is never exported. Also the request labels (client,
//! session, initiator) shared by the span, the metrics and the log line.
//!
//! Metric series carry the session id (the Grafana dashboard groups by it), so a series not updated for an hour is
//! dropped: long-running instances do not keep every session they ever served.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::http::HeaderMap;
use futures::StreamExt;
use futures::stream::BoxStream;
use regex::Regex;
use sha1::{Digest, Sha1};

use crate::canonical::{CanonicalRequest, CanonicalResponse, Event, Meta, SharedMeta, TraceContext};
use crate::config::ModelSpec;
use crate::errors::Error;
use crate::otlp::{self, AttrValue, Attrs, HistogramPoint, MetricData, Points, SpanData, SpanKind, SumPoint};
use crate::protocols::common::RequestInfo;
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
/// LLM calls take from a fraction of a second to minutes.
const LATENCY_BUCKETS: [f64; 12] = [100.0, 250.0, 500.0, 1000.0, 2000.0, 5000.0, 10000.0, 20000.0, 30000.0, 60000.0, 120000.0, 300000.0];
/// Queue waits are usually nothing, sometimes seconds.
const QUEUE_BUCKETS: [f64; 15] = [0.0, 5.0, 10.0, 25.0, 50.0, 75.0, 100.0, 250.0, 500.0, 750.0, 1000.0, 2500.0, 5000.0, 7500.0, 10000.0];
/// A series not updated for this long is dropped (its last value stays in the metrics backend).
const SERIES_TTL: Duration = Duration::from_secs(3600);
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

/// Where a request came from: its HTTP headers, what its body says about it, its protocol and id.
pub struct Origin<'a> {
    pub headers: &'a HeaderMap,
    pub info: &'a RequestInfo,
    pub protocol: &'a str,
    pub rid: &'a str,
}

/// The session a request belongs to: a client header, else what the body carries (Claude Code's metadata, Responses'
/// prompt_cache_key or user, the stored response it continues), else a hash of its first user message.
fn session_of(o: &Origin, req: &CanonicalRequest, store: &ResponseStore) -> String {
    let session = SESSION_HEADERS
        .iter()
        .find_map(|h| header(o.headers, h).filter(|v| !v.is_empty()))
        .or_else(|| o.info.session.clone())
        .or_else(|| {
            let prev = o.info.previous_response_id.as_deref().filter(|p| !p.is_empty())?;
            store.session_of(prev).filter(|s| !s.is_empty())
        })
        .or_else(|| (o.protocol == "responses").then(|| format!("chain-{}", prefix(skip_chars(o.rid, 5), 10))));
    session.unwrap_or_else(|| {
        let first_user = req.turns.iter().find(|t| t.role == "user" && !t.text.is_empty()).map(|t| t.text.as_str()).unwrap_or("");
        if first_user.is_empty() {
            o.rid.to_string()
        } else {
            let digest = Sha1::digest(prefix(first_user, 500).as_bytes());
            format!("conv-{}", &crate::canonical::hex(&digest)[..10])
        }
    })
}

/// Labels for one request: client, session, initiator, model, agent, and the client's trace.
pub fn request_meta(o: &Origin, route: &ModelSpec, req: &CanonicalRequest, store: &ResponseStore) {
    let (client, version) = client_of(o.headers, req.system.first().map(String::as_str).unwrap_or(""));
    let session = session_of(o, req, store);
    let initiator = header(o.headers, "x-initiator")
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| if req.turns.last().is_some_and(|t| !t.tool_results.is_empty()) { "agent".into() } else { "user".into() });
    let mut m = req.meta();
    m.client = client;
    m.client_version = version;
    m.session = prefix(&session, 64).to_string();
    m.initiator = initiator;
    m.protocol = o.protocol.into();
    m.model = o.info.model.clone();
    m.agent = route.name.clone();
    m.backend = route.backend.clone();
    m.stream = o.info.stream;
    m.tools_declared = req.tools.len() as i64;
    m.json_mode = req.json_schema.is_some();
    m.parent = header(o.headers, "traceparent").and_then(|t| TraceContext::parse_traceparent(&t));
}

fn now_ns() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64)
}

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    let _ = getrandom::fill(&mut b);
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

struct Instrument {
    name: &'static str,
    kind: Kind,
    unit: &'static str,
    description: &'static str,
    buckets: &'static [f64],
}

const fn instrument(name: &'static str, kind: Kind, unit: &'static str, description: &'static str) -> Instrument {
    Instrument { name, kind, unit, description, buckets: &[] }
}

const INSTRUMENTS: [Instrument; 12] = [
    instrument("midir.requests", Kind::Counter, "1", "LLM requests handled"),
    instrument(
        "gen_ai.client.token.usage",
        Kind::Counter,
        "{token}",
        "Input and output tokens reported by the backend (estimated when it reports none)",
    ),
    Instrument {
        buckets: &LATENCY_BUCKETS,
        ..instrument("midir.request.duration", Kind::Histogram, "ms", "Request duration, first byte in to last byte out")
    },
    Instrument {
        buckets: &LATENCY_BUCKETS,
        ..instrument("midir.request.ttfb", Kind::Histogram, "ms", "Time to the first text or tool-call event")
    },
    instrument("midir.tool_calls", Kind::Counter, "1", "Tool calls emitted to the client"),
    instrument("midir.followups", Kind::Counter, "1", "Automatic follow-up calls (promise, incapacity, tool_choice retry, repair)"),
    instrument("midir.upstream_retries", Kind::Counter, "1", "Retried backend attempts, by status"),
    Instrument {
        buckets: &QUEUE_BUCKETS,
        ..instrument("midir.queue.wait", Kind::Histogram, "ms", "Time waiting in the backend queue (concurrency and requests/minute)")
    },
    instrument("midir.queue.waiting", Kind::UpDown, "1", "Requests currently waiting for a backend slot"),
    instrument("midir.errors", Kind::Counter, "1", "Requests that ended in an error"),
    instrument(
        "midir.truncations",
        Kind::Counter,
        "1",
        "Upstream calls whose prompt was cut to the size cap (old turns dropped or texts cut)",
    ),
    instrument("midir.dropped_turns", Kind::Counter, "{turn}", "Old conversation turns dropped to fit the size cap"),
];

fn instrument_of(name: &str) -> &'static Instrument {
    INSTRUMENTS.iter().find(|i| i.name == name).unwrap_or(&INSTRUMENTS[0])
}

struct Series {
    attrs: Attrs,
    start_ns: u64,
    updated: Instant,
    int: i64,
    count: u64,
    sum: f64,
    buckets: Vec<u64>,
    min: f64,
    max: f64,
}

fn attrs_key(a: &Attrs) -> String {
    let mut parts: Vec<String> = a.iter().map(|(k, v)| format!("{k}={v:?}")).collect();
    parts.sort();
    parts.join("\u{1}")
}

/// The metric series, cumulative since each series appeared.
#[derive(Default)]
struct Metrics {
    series: Mutex<BTreeMap<&'static str, BTreeMap<String, Series>>>,
}

impl Metrics {
    fn record(&self, name: &'static str, attrs: Attrs, int: i64, val: f64) {
        let inst = instrument_of(name);
        let mut all = self.series.lock().unwrap_or_else(|e| e.into_inner());
        let series = all.entry(inst.name).or_default().entry(attrs_key(&attrs)).or_insert_with(|| Series {
            attrs,
            start_ns: now_ns(),
            updated: Instant::now(),
            int: 0,
            count: 0,
            sum: 0.0,
            buckets: vec![0; inst.buckets.len() + 1],
            min: f64::INFINITY,
            max: f64::NEG_INFINITY,
        });
        series.updated = Instant::now();
        match inst.kind {
            Kind::Histogram => {
                series.count += 1;
                series.sum += val;
                series.min = series.min.min(val);
                series.max = series.max.max(val);
                let i = inst.buckets.iter().position(|b| val <= *b).unwrap_or(inst.buckets.len());
                series.buckets[i] += 1;
            }
            _ => series.int += int,
        }
    }

    /// Every live series by instrument, after dropping the ones idle for longer than SERIES_TTL.
    fn snapshot(&self) -> Vec<MetricData> {
        self.snapshot_at(Instant::now())
    }

    fn snapshot_at(&self, now: Instant) -> Vec<MetricData> {
        let mut all = self.series.lock().unwrap_or_else(|e| e.into_inner());
        for series in all.values_mut() {
            series.retain(|_, s| now.saturating_duration_since(s.updated) < SERIES_TTL);
        }
        INSTRUMENTS
            .iter()
            .filter_map(|inst| {
                let series = all.get(inst.name).filter(|s| !s.is_empty())?;
                let points = match inst.kind {
                    Kind::Histogram => Points::Histogram {
                        bounds: inst.buckets,
                        points: series
                            .values()
                            .map(|s| HistogramPoint {
                                attrs: s.attrs.clone(),
                                start_ns: s.start_ns,
                                count: s.count,
                                sum: s.sum,
                                buckets: s.buckets.clone(),
                                min: s.min,
                                max: s.max,
                            })
                            .collect(),
                    },
                    k => Points::Sum {
                        monotonic: k == Kind::Counter,
                        points: series.values().map(|s| SumPoint { attrs: s.attrs.clone(), start_ns: s.start_ns, value: s.int }).collect(),
                    },
                };
                Some(MetricData { name: inst.name, description: inst.description, unit: inst.unit, points })
            })
            .collect()
    }

    /// The Prometheus text exposition of the snapshot (`midir.request.duration` in ms ->
    /// `midir_request_duration_milliseconds_bucket`, counters end in `_total`).
    fn prometheus(&self) -> String {
        fn name_of(m: &MetricData) -> String {
            let mut n = m.name.replace('.', "_");
            if m.unit == "ms" {
                n.push_str("_milliseconds");
            }
            n
        }
        fn labels(attrs: &Attrs, extra: Option<(&str, String)>) -> String {
            let escape = |v: &str| v.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n");
            let mut parts: Vec<String> = attrs
                .iter()
                .map(|(k, v)| {
                    let v = match v {
                        AttrValue::Str(s) => s.clone(),
                        AttrValue::Bool(b) => b.to_string(),
                        AttrValue::Int(i) => i.to_string(),
                        AttrValue::Double(d) => d.to_string(),
                    };
                    format!("{}=\"{}\"", k.replace(['.', '-'], "_"), escape(&v))
                })
                .collect();
            if let Some((k, v)) = extra {
                parts.push(format!("{k}=\"{v}\""));
            }
            if parts.is_empty() { String::new() } else { format!("{{{}}}", parts.join(",")) }
        }
        let mut out = String::new();
        for m in self.snapshot() {
            let name = name_of(&m);
            match &m.points {
                Points::Sum { monotonic, points } => {
                    let (full, kind) = if *monotonic { (format!("{name}_total"), "counter") } else { (name.clone(), "gauge") };
                    let _ = writeln!(out, "# HELP {full} {}\n# TYPE {full} {kind}", m.description);
                    for p in points {
                        let _ = writeln!(out, "{full}{} {}", labels(&p.attrs, None), p.value);
                    }
                }
                Points::Histogram { bounds, points } => {
                    let _ = writeln!(out, "# HELP {name} {}\n# TYPE {name} histogram", m.description);
                    for p in points {
                        let mut cumulative = 0;
                        for (i, count) in p.buckets.iter().enumerate() {
                            cumulative += count;
                            let le = bounds.get(i).map_or("+Inf".to_string(), |b| b.to_string());
                            let _ = writeln!(out, "{name}_bucket{} {cumulative}", labels(&p.attrs, Some(("le", le))));
                        }
                        let _ = writeln!(out, "{name}_sum{} {}", labels(&p.attrs, None), p.sum);
                        let _ = writeln!(out, "{name}_count{} {}", labels(&p.attrs, None), p.count);
                    }
                }
            }
        }
        out
    }
}

struct Exporter {
    resource: Attrs,
    traces_url: String,
    metrics_url: String,
    headers: Vec<(String, String)>,
    http: reqwest::Client,
    spans: Mutex<Vec<SpanData>>,
    closed: AtomicBool,
}

pub struct Telemetry {
    /// recorded when OTLP or /metrics is on
    metrics: Option<Arc<Metrics>>,
    /// OTLP/HTTP export (spans and metrics), when an endpoint is configured
    exp: Option<Arc<Exporter>>,
}

/// A span being recorded.
pub struct Span {
    ctx: TraceContext,
    parent: Option<[u8; 8]>,
    kind: SpanKind,
    name: String,
    start_ns: u64,
    attrs: Attrs,
}

fn signal_url(base: &str, specific: Option<String>, path: &str) -> String {
    if let Some(u) = specific.filter(|u| !u.is_empty()) {
        return u;
    }
    if base.ends_with('/') { format!("{base}{path}") } else { format!("{base}/{path}") }
}

/// `%XX` escapes decoded (the OTEL_EXPORTER_OTLP_HEADERS and OTEL_RESOURCE_ATTRIBUTES values are URL-encoded).
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(v) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `key1=value1,key2=value2` with URL-encoded values.
fn parse_pairs(raw: &str) -> Vec<(String, String)> {
    raw.split(',')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), percent_decode(v.trim())))
        .filter(|(k, _)| !k.is_empty())
        .collect()
}

impl Telemetry {
    pub fn disabled() -> Self {
        Telemetry { metrics: None, exp: None }
    }

    /// OTLP export when `endpoint` (or OTEL_EXPORTER_OTLP_ENDPOINT) is set; metrics kept for `/metrics` when
    /// `prometheus` is on.
    pub fn new(endpoint: &str, service_name: &str, prometheus: bool) -> Self {
        let metrics = (prometheus || !endpoint.is_empty()).then(|| Arc::new(Metrics::default()));
        if endpoint.is_empty() {
            return Telemetry { metrics, exp: None };
        }
        let env = |k: &str| std::env::var(k).ok();
        let base = env("OTEL_EXPORTER_OTLP_ENDPOINT").filter(|e| !e.is_empty()).unwrap_or_else(|| endpoint.to_string());
        let mut headers = parse_pairs(&env("OTEL_EXPORTER_OTLP_HEADERS").unwrap_or_default());
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
        for (k, v) in parse_pairs(&env("OTEL_RESOURCE_ATTRIBUTES").unwrap_or_default()) {
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
            closed: AtomicBool::new(false),
        });
        tracing::info!("telemetry on: OTLP/HTTP -> {endpoint} (service {service_name})");
        Telemetry { metrics, exp: Some(exp) }
    }

    /// Whether anything is recorded (OTLP or /metrics).
    pub fn enabled(&self) -> bool {
        self.metrics.is_some()
    }

    /// Whether /metrics has something to show.
    pub fn prometheus(&self) -> Option<String> {
        self.metrics.as_ref().map(|m| m.prometheus())
    }

    /// The periodic export (every 5 s); call once inside the runtime.
    pub fn spawn_exporter(&self) {
        if let (Some(exp), Some(metrics)) = (self.exp.clone(), self.metrics.clone()) {
            tokio::spawn(async move {
                let mut tick = tokio::time::interval(Duration::from_secs(5));
                tick.tick().await;
                loop {
                    tick.tick().await;
                    if exp.closed.load(Ordering::SeqCst) {
                        return;
                    }
                    exp.export(&metrics).await;
                }
            });
        }
    }

    /// Flush and stop the exporters; safe to call twice.
    pub async fn shutdown(&self) {
        if let (Some(exp), Some(metrics)) = (&self.exp, &self.metrics)
            && !exp.closed.swap(true, Ordering::SeqCst)
        {
            let _ = tokio::time::timeout(Duration::from_secs(4), exp.export(metrics)).await;
        }
    }

    fn record(&self, name: &'static str, attrs: Attrs, int: i64, val: f64) {
        if let Some(m) = &self.metrics {
            m.record(name, attrs, int, val);
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
            if turns > 0 {
                self.record("midir.dropped_turns", labels(meta), turns, 0.0);
            }
        }
    }

    pub fn upstream_retry(&self, status: &str) {
        self.record("midir.upstream_retries", vec![("midir.upstream_status".into(), s(status))], 1, 0.0);
    }

    /// The request span (kind SERVER): the child of the client's trace when it sent one. Its identity goes into the
    /// request's meta, so backend calls become its children.
    pub fn begin(&self, shared: &SharedMeta, rid: &str) -> Option<Span> {
        self.exp.as_ref()?;
        let mut meta = shared.lock().unwrap_or_else(|e| e.into_inner());
        let ctx = TraceContext { trace_id: meta.parent.map_or_else(random::<16>, |p| p.trace_id), span_id: random::<8>() };
        meta.span = Some(ctx);
        let provider = if meta.backend.is_empty() { "unknown" } else { meta.backend.as_str() };
        let attrs: Attrs = vec![
            ("gen_ai.provider.name".into(), s(provider)),
            ("gen_ai.system".into(), s(provider)),
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
            ctx,
            parent: meta.parent.map(|p| p.span_id),
            kind: SpanKind::Server,
            name: format!("gen_ai.chat {}", meta.protocol),
            start_ns: now_ns(),
            attrs,
        })
    }

    fn finish_span(&self, mut span: Span, attrs: Attrs, error: Option<String>) {
        let Some(exp) = &self.exp else { return };
        for (k, v) in attrs {
            span.attrs.retain(|(x, _)| *x != k);
            span.attrs.push((k, v));
        }
        exp.spans.lock().unwrap_or_else(|e| e.into_inner()).push(SpanData {
            trace_id: span.ctx.trace_id,
            span_id: span.ctx.span_id,
            parent_span_id: span.parent,
            kind: span.kind,
            name: span.name,
            start_ns: span.start_ns,
            end_ns: now_ns(),
            attrs: span.attrs,
            error,
        });
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
        if !self.enabled() {
            return;
        }
        let duration_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let lbl = labels(meta);
        let usage = resp.map(|r| r.usage).unwrap_or_default();
        if let Some(span) = span {
            let mut attrs: Attrs = vec![
                ("gen_ai.usage.input_tokens".into(), AttrValue::Int(usage.prompt_tokens)),
                ("gen_ai.usage.output_tokens".into(), AttrValue::Int(usage.completion_tokens)),
                ("midir.tool_calls".into(), AttrValue::Int(resp.map_or(0, |r| r.tool_calls.len() as i64))),
                ("midir.finish".into(), s(resp.map_or("error", |r| r.finish.as_str()))),
                ("midir.followups".into(), AttrValue::Int(meta.followups)),
                ("midir.followup_errors".into(), AttrValue::Int(meta.followup_errors)),
                ("midir.parse_errors".into(), AttrValue::Int(meta.parse_errors)),
                ("midir.repairs".into(), AttrValue::Int(meta.repairs)),
                ("midir.upstream_calls".into(), AttrValue::Int(meta.upstream_calls)),
                ("midir.queue_wait_ms".into(), meta.queue_wait_ms.map_or(AttrValue::Int(0), |q| AttrValue::Double(round1(q)))),
                ("midir.prompt_chars".into(), AttrValue::Int(meta.prompt_chars)),
                ("midir.dropped_turns".into(), AttrValue::Int(meta.dropped_turns)),
                ("midir.shrunk_parts".into(), AttrValue::Int(meta.shrunk_parts)),
                ("midir.duration_ms".into(), AttrValue::Double(round1(duration_ms))),
            ];
            if let Some(t) = ttfb {
                attrs.push(("midir.ttfb_ms".into(), AttrValue::Double(round1(t * 1000.0))));
            }
            if let Some(e) = &error {
                attrs.push(("error.type".into(), s(e)));
            }
            self.finish_span(span, attrs, error.clone());
        }
        if let Some(e) = &error {
            let mut el = lbl.clone();
            el.push(("error.type".into(), s(e)));
            self.record("midir.errors", el, 1, 0.0);
        }
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

    /// One backend call as a CLIENT span, the child of the request span: prompt size, time to first output, usage,
    /// how it ended. Without OTLP (or outside a traced request) the stream passes through untouched.
    pub fn trace_call(
        self: &Arc<Self>,
        events: BoxStream<'static, Result<Event, Error>>,
        shared: &SharedMeta,
        provider: &'static str,
        call: &str,
    ) -> BoxStream<'static, Result<Event, Error>> {
        if self.exp.is_none() {
            return events;
        }
        let this = self.clone();
        let shared = shared.clone();
        let call = call.to_string();
        Box::pin(async_stream::stream! {
            let (parent, model) = {
                let m = shared.lock().unwrap_or_else(|e| e.into_inner());
                (m.span, m.agent.clone())
            };
            let Some(parent) = parent else {
                let mut events = events;
                while let Some(ev) = events.next().await {
                    yield ev;
                }
                return;
            };
            let span = Span {
                ctx: TraceContext { trace_id: parent.trace_id, span_id: random::<8>() },
                parent: Some(parent.span_id),
                kind: SpanKind::Client,
                name: format!("chat {model}"),
                start_ns: now_ns(),
                attrs: vec![
                    ("gen_ai.operation.name".into(), s("chat")),
                    ("gen_ai.provider.name".into(), s(provider)),
                    ("gen_ai.request.model".into(), s(&model)),
                    ("midir.call".into(), s(&call)),
                ],
            };
            let mut obs = CallObservation { tel: this, span: Some(span), t0: Instant::now(), attrs: vec![], error: None, ended: false };
            let mut events = events;
            while let Some(ev) = events.next().await {
                match &ev {
                    Ok(Event::Prompt { tokens }) => obs.attrs.push(("midir.prompt_tokens_estimate".into(), AttrValue::Int(*tokens))),
                    Ok(Event::Text(_) | Event::ToolCall(_)) if !obs.attrs.iter().any(|(k, _)| k == "midir.ttfb_ms") => {
                        obs.attrs.push(("midir.ttfb_ms".into(), AttrValue::Double(round1(obs.t0.elapsed().as_secs_f64() * 1000.0))));
                    }
                    Ok(Event::Done(r)) => {
                        obs.attrs.push(("gen_ai.usage.input_tokens".into(), AttrValue::Int(r.usage.prompt_tokens)));
                        obs.attrs.push(("gen_ai.usage.output_tokens".into(), AttrValue::Int(r.usage.completion_tokens)));
                        obs.attrs.push(("midir.finish".into(), s(r.finish.as_str())));
                        obs.attrs.push(("midir.tool_calls".into(), AttrValue::Int(r.tool_calls.len() as i64)));
                        // the call is over: its reader may stop polling right after this event
                        obs.finish();
                    }
                    Err(e) => obs.error = Some(e.telemetry_type()),
                    _ => {}
                }
                yield ev;
            }
            obs.finish();
        })
    }
}

/// Ends a backend call's span when its stream ends or is dropped (the client went away).
struct CallObservation {
    tel: Arc<Telemetry>,
    span: Option<Span>,
    t0: Instant,
    attrs: Attrs,
    error: Option<String>,
    ended: bool,
}

impl CallObservation {
    fn finish(&mut self) {
        if std::mem::replace(&mut self.ended, true) {
            return;
        }
        let Some(span) = self.span.take() else { return };
        let mut attrs = std::mem::take(&mut self.attrs);
        if let Some(e) = &self.error {
            attrs.push(("error.type".into(), s(e)));
        }
        self.tel.finish_span(span, attrs, self.error.take());
    }
}

impl Drop for CallObservation {
    fn drop(&mut self) {
        if !self.ended && self.error.is_none() {
            self.error = Some("cancelled".into());
        }
        self.finish();
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

    async fn export(&self, metrics: &Metrics) {
        let spans: Vec<SpanData> = std::mem::take(&mut *self.spans.lock().unwrap_or_else(|e| e.into_inner()));
        if !spans.is_empty() {
            self.post(&self.traces_url, otlp::encode_traces(&self.resource, &spans)).await;
        }
        let snapshot = metrics.snapshot();
        if !snapshot.is_empty() {
            self.post(&self.metrics_url, otlp::encode_metrics(&self.resource, &snapshot, now_ns())).await;
        }
    }
}

/// Ends the request span when the observed stream ends or is dropped (client disconnect).
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
        let span = tel.begin(&meta, &rid);
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

/// Same for a non-streaming request (no TTFB). A client that goes away mid-request drops this future: the request is
/// still recorded, as a disconnect.
pub async fn observe_complete<F>(tel: &Arc<Telemetry>, fut: F, req: &CanonicalRequest, rid: &str) -> Result<CanonicalResponse, Error>
where
    F: std::future::Future<Output = Result<CanonicalResponse, Error>>,
{
    if !tel.enabled() {
        return fut.await;
    }
    let span = tel.begin(&req.meta, rid);
    let mut obs = Observation {
        tel: tel.clone(),
        span,
        meta: req.meta.clone(),
        t0: Instant::now(),
        ttfb: None,
        resp: None,
        error: None,
        ended: false,
    };
    let r = fut.await;
    match &r {
        Ok(resp) => obs.resp = Some(resp.clone()),
        Err(e) => obs.error = Some(e.telemetry_type()),
    }
    obs.finish();
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn otlp_headers_are_url_decoded() {
        assert_eq!(
            parse_pairs("Authorization=Basic%20abc%3D%3D, x-scope = tenant%2Fa"),
            vec![("Authorization".to_string(), "Basic abc==".to_string()), ("x-scope".to_string(), "tenant/a".to_string())]
        );
    }

    #[test]
    fn client_detection() {
        let h = |pairs: &[(&'static str, &str)]| {
            let mut m = HeaderMap::new();
            for (k, v) in pairs {
                m.insert(*k, v.parse().unwrap());
            }
            m
        };
        let cases: [(HeaderMap, &str, &str, &str); 9] = [
            (h(&[("user-agent", "claude-cli/2.1.283 (external, cli)")]), "", "claude-code", "2.1.283"),
            (h(&[("user-agent", "claude-cli/2.0.1 (external, claude-vscode)")]), "", "claude-code-vscode", "2.0.1"),
            (h(&[("user-agent", "codex_cli_rs/0.40.0")]), "", "codex", "0.40.0"),
            (h(&[("user-agent", "OpenAI/JS 5.12.2"), ("x-initiator", "agent")]), "", "copilot", ""),
            (h(&[("user-agent", "GitHubCopilotChat/0.31.0")]), "", "copilot", "0.31.0"),
            (h(&[("user-agent", "OpenAI/Python 1.99.0")]), "You are Hermes Agent, ...", "hermes", ""),
            (h(&[("user-agent", "OpenAI/JS 5.0")]), "You are a personal assistant running inside OpenClaw.", "openclaw", ""),
            (h(&[("user-agent", "python-httpx/0.28.1")]), "", "sdk-python", "0.28.1"),
            (h(&[]), "", "unknown", ""),
        ];
        for (headers, system, name, version) in cases {
            assert_eq!(client_of(&headers, system), (name.to_string(), version.to_string()), "{headers:?}");
        }
    }

    #[test]
    fn idle_series_are_dropped_and_prometheus_text() {
        let m = Metrics::default();
        m.record("midir.requests", vec![("session.id".into(), s("a"))], 1, 0.0);
        m.record("midir.request.duration", vec![], 0, 1500.0);
        let text = m.prometheus();
        assert!(text.contains("midir_requests_total{session_id=\"a\"} 1"), "{text}");
        assert!(text.contains("midir_request_duration_milliseconds_bucket{le=\"2000\"} 1"), "{text}");
        assert!(text.contains("midir_request_duration_milliseconds_bucket{le=\"1000\"} 0"), "{text}");
        assert!(text.contains("midir_request_duration_milliseconds_count 1"), "{text}");
        assert!(!m.snapshot_at(Instant::now() + SERIES_TTL - Duration::from_secs(1)).is_empty());
        assert!(m.snapshot_at(Instant::now() + SERIES_TTL + Duration::from_secs(1)).is_empty());
    }
}
