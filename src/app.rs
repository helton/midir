//! HTTP layer: the OpenAI and Anthropic endpoints, errors in each protocol's own format, health, readiness and
//! metrics, the optional API key, SSE keepalives and error events mid-stream, and a 500 instead of a crash when a
//! request hits a bug.

use std::borrow::Cow;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use futures::StreamExt;
use futures::stream::BoxStream;
use serde::Serialize;
use serde_json::{Map, Value, json};
use tower_http::catch_panic::CatchPanicLayer;

use crate::buildinfo;
use crate::canonical::{CanonicalRequest, CanonicalResponse, Event, estimate_tokens, hex_id};
use crate::config::DEFAULT_MODEL_NAME;
use crate::emulation::engine::{EmulationEngine, EventStream};
use crate::emulation::prompt::render_prompt;
use crate::errors::{ClientError, Error};
use crate::gateway::Gateway;
use crate::json;
use crate::protocols::common::RequestInfo;
use crate::protocols::responses::Envelope;
use crate::protocols::{chat_completions, messages, responses};
use crate::store::Stored;
use crate::telemetry::{self, Origin, observe, observe_complete};
use crate::text::prefix;

/// Request bodies can be large (whole conversations with tool output); this is far above any backend's input limit.
const MAX_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Which error format a route answers in.
#[derive(Clone, Copy, PartialEq)]
enum Flavor {
    OpenAi,
    Responses,
    Anthropic,
}

impl Flavor {
    /// The header that carries the request id in this protocol.
    fn request_id_header(self) -> &'static str {
        if self == Flavor::Anthropic { "request-id" } else { "x-request-id" }
    }
}

fn error_type(status: u16, default: &'static str) -> &'static str {
    match status {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        429 => "rate_limit_error",
        _ => default,
    }
}

fn json_text(status: u16, body: String) -> Response {
    let mut r = Response::new(Body::from(body));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    r.headers_mut().insert("content-type", HeaderValue::from_static("application/json"));
    r
}

fn json_response<T: Serialize + ?Sized>(status: u16, body: &T) -> Response {
    json_text(status, serde_json::to_string(body).unwrap_or_default())
}

fn error_body(flavor: Flavor, status: u16, message: &str, typ: &str, code: Option<&str>) -> Response {
    match flavor {
        Flavor::Anthropic => json_response(status, &json!({"type": "error", "error": {"type": typ, "message": message}})),
        _ => json_response(status, &json!({"error": {"message": message, "type": typ, "code": code, "param": null}})),
    }
}

/// An error in the middle of a stream becomes an error event in the protocol's format.
fn stream_error(flavor: Flavor, message: &str, typ: &str) -> String {
    match flavor {
        Flavor::Anthropic => format!("event: error\ndata: {}\n\n", json!({"type": "error", "error": {"type": typ, "message": message}})),
        Flavor::Responses => {
            format!(
                "event: error\ndata: {}\n\n",
                json!({"type": "error", "code": typ, "message": message, "param": null, "sequence_number": 0})
            )
        }
        Flavor::OpenAi => {
            format!("data: {}\n\ndata: [DONE]\n\n", json!({"error": {"message": message, "type": typ, "code": null, "param": null}}))
        }
    }
}

/// Pass `events` through, adding a keepalive event whenever nothing was forwarded for `interval` seconds: while the
/// backend thinks, and while a follow-up call runs after content was streamed.
pub fn with_keepalive(events: EventStream, interval: f64) -> EventStream {
    if interval <= 0.0 {
        return events;
    }
    let every = Duration::from_secs_f64(interval);
    Box::pin(async_stream::stream! {
        let mut events = events;
        loop {
            match tokio::time::timeout(every, events.next()).await {
                Ok(Some(ev)) => yield ev,
                Ok(None) => break,
                Err(_) => yield Ok(Event::Keepalive),
            }
        }
    })
}

/// What a panic says, for the log.
fn panic_text(err: &(dyn std::any::Any + Send)) -> &str {
    err.downcast_ref::<String>().map(String::as_str).or_else(|| err.downcast_ref::<&str>().copied()).unwrap_or("unknown")
}

/// The protocol stream with errors turned into an error event (never a silently dropped connection); a panic while
/// streaming ends this stream with an error event and nothing else.
fn guarded(
    inner: BoxStream<'static, Result<String, Error>>,
    rid: String,
    flavor: Flavor,
) -> BoxStream<'static, Result<Bytes, std::io::Error>> {
    Box::pin(async_stream::stream! {
        let mut inner = AssertUnwindSafe(inner).catch_unwind();
        while let Some(chunk) = inner.next().await {
            let ev = match chunk {
                Ok(Ok(s)) => {
                    yield Ok(Bytes::from(s));
                    continue;
                }
                Ok(Err(e)) => match &e {
                    Error::Backend(b) => {
                        tracing::error!("{rid} backend error mid-stream: {b}");
                        stream_error(flavor, &b.message(), "api_error")
                    }
                    Error::Client(c) => stream_error(flavor, &c.message, "invalid_request_error"),
                    Error::Net(n) => {
                        tracing::error!("{rid} network error mid-stream: {n}");
                        stream_error(flavor, &format!("error talking to the backend: {n}"), "api_error")
                    }
                    Error::Internal(m) => {
                        tracing::error!("{rid} internal error mid-stream: {m}");
                        stream_error(flavor, &format!("midir internal error: {m}"), "api_error")
                    }
                },
                Err(panic) => {
                    tracing::error!("{rid} internal error mid-stream (panic): {}", panic_text(&*panic));
                    stream_error(flavor, "midir internal error (a bug): this response was cut; other requests are not affected", "api_error")
                }
            };
            yield Ok(Bytes::from(ev));
            break;
        }
    })
}

/// A request that hit a bug before its answer started: a 500 that both OpenAI and Anthropic SDKs read.
fn panic_response(err: Box<dyn std::any::Any + Send + 'static>) -> Response {
    tracing::error!("internal error (panic): {}", panic_text(&*err));
    let message = "midir internal error (a bug): this request failed; other requests are not affected";
    json_response(
        500,
        &json!({"type": "error", "error": {"type": "api_error", "message": message, "code": "internal_error", "param": null}}),
    )
}

fn sse_response(body: BoxStream<'static, Result<Bytes, std::io::Error>>) -> Response {
    let mut r = Response::new(Body::from_stream(body));
    let h = r.headers_mut();
    h.insert("content-type", HeaderValue::from_static("text/event-stream; charset=utf-8"));
    h.insert("cache-control", HeaderValue::from_static("no-cache"));
    h.insert("x-accel-buffering", HeaderValue::from_static("no"));
    r
}

fn now_s() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// RFC 3339 UTC time of a Unix timestamp (`2026-10-03T12:00:00Z`).
fn rfc3339(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // civil from days (H. Hinnant)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// A request routed to its model: the canonical request, its runner, the model name to answer with, stream or not.
struct Prepared {
    req: Arc<CanonicalRequest>,
    runner: Arc<EmulationEngine>,
    model: String,
    stream: bool,
}

pub struct App {
    pub gateway: Arc<Gateway>,
}

impl App {
    fn error_response(&self, flavor: Flavor, e: Error) -> Response {
        match e {
            Error::Client(c) => {
                tracing::warn!("{} {}: {}", c.status, c.code, prefix(&c.message, 300));
                error_body(flavor, c.status, &c.message, error_type(c.status, "invalid_request_error"), Some(&c.code))
            }
            Error::Backend(b) => {
                tracing::error!("{b}");
                let status = b.http_status();
                let mut resp =
                    error_body(flavor, status, &b.message(), error_type(status, "api_error"), Some(&format!("upstream_{}", b.status)));
                if status == 429
                    && let Some(backend) = self.gateway.backend_of(&b.backend)
                    && let Ok(v) = HeaderValue::from_str(&backend.limiter().retry_after().to_string())
                {
                    resp.headers_mut().insert("retry-after", v);
                }
                resp
            }
            Error::Net(n) => {
                tracing::error!("{n}");
                let status = if n.kind == crate::errors::NetErrorKind::Timeout { 504 } else { 502 };
                error_body(flavor, status, &format!("error talking to the backend: {n}"), "api_error", Some("upstream_network"))
            }
            Error::Internal(m) => {
                tracing::error!("internal error: {m}");
                error_body(flavor, 500, &format!("midir internal error: {m}"), "api_error", Some("internal_error"))
            }
        }
    }

    /// The body as the decoders read it: a lone UTF-16 surrogate escape (JavaScript clients write them when a string
    /// is cut in the middle of an emoji) or a NaN would otherwise make the whole conversation unreadable.
    fn body(raw: &[u8]) -> Cow<'_, [u8]> {
        json::sanitize(raw)
    }

    fn prepare(
        &self,
        headers: &HeaderMap,
        mut info: RequestInfo,
        mut req: CanonicalRequest,
        protocol: &str,
        rid: &str,
    ) -> Result<Prepared, Error> {
        if info.model.trim().is_empty() {
            info.model = DEFAULT_MODEL_NAME.to_string();
        }
        let (route, runner) = self.gateway.route(&info.model);
        req.route = Some(route.clone());
        telemetry::request_meta(&Origin { headers, info: &info, protocol, rid }, &route, &req, &self.gateway.store);
        {
            let m = req.meta();
            let prev = info.previous_response_id.as_deref().map_or(String::new(), |p| format!(" previous={p}"));
            tracing::info!(
                "{rid} {protocol} model={}->{}/{} stream={} tools={} choice={} json={}{prev} client={} session={}",
                info.model,
                route.backend,
                route.name,
                info.stream,
                req.tools.len(),
                req.tool_choice,
                req.json_schema.is_some(),
                m.client,
                prefix(&m.session, 12)
            );
        }
        Ok(Prepared { req: Arc::new(req), runner, model: info.model, stream: info.stream })
    }

    fn keepalive_s(&self) -> f64 {
        self.gateway.config.server.keepalive_s
    }

    fn health(&self) -> Response {
        let cfg = &self.gateway.config;
        let backends: Map<String, Value> =
            self.gateway.backends.iter().map(|(n, b)| (n.clone(), json!({"type": b.kind(), "queue": b.limiter().state()}))).collect();
        json_response(
            200,
            &json!({"ok": true, "version": buildinfo::full_version(), "build": buildinfo::build().as_json(), "config": cfg.source, "default": cfg.default.name,
                    "models": self.gateway.describe_models(), "backends": backends, "responses_cache": self.gateway.store.stats(),
                    "protocols": ["chat-completions", "responses", "messages"]}),
        )
    }

    async fn ready(&self) -> Response {
        let status = self.gateway.ready().await;
        let failed: Vec<&str> = status.values().filter_map(|w| w.as_deref()).collect();
        let backends: Map<String, Value> = status
            .iter()
            .map(|(n, why)| {
                let mut b = json!({"ok": why.is_none(), "queue": self.gateway.backends[n].limiter().state()});
                if let Some(w) = why {
                    b["error"] = json!(w);
                }
                (n.clone(), b)
            })
            .collect();
        let mut body = json!({"ok": failed.is_empty(), "version": buildinfo::full_version(), "build": buildinfo::build().as_json(), "backends": backends});
        if failed.is_empty() {
            return json_response(200, &body);
        }
        body["error"] = json!(failed.join("; "));
        json_response(503, &body)
    }

    /// The configured models, in the format of whoever asks: Anthropic clients send `anthropic-version`.
    fn models(&self, headers: &HeaderMap) -> Response {
        let now = now_s();
        let models = self.gateway.config.exposed_models();
        if headers.contains_key("anthropic-version") {
            let data: Vec<Value> = models.iter().map(|m| self.anthropic_model(&m.name, &m.description, now)).collect();
            let edge = |m: Option<&Arc<crate::config::ModelSpec>>| m.map_or(Value::Null, |m| json!(m.name));
            return json_response(
                200,
                &json!({"data": data, "has_more": false, "first_id": edge(models.first()), "last_id": edge(models.last())}),
            );
        }
        let data: Vec<Value> = models
            .iter()
            .map(|m| json!({"id": m.name, "object": "model", "created": now, "owned_by": m.backend, "description": m.description}))
            .collect();
        json_response(200, &json!({"object": "list", "data": data}))
    }

    fn anthropic_model(&self, id: &str, description: &str, created: i64) -> Value {
        let display = if description.is_empty() { id } else { description };
        json!({"type": "model", "id": id, "display_name": display, "created_at": rfc3339(created)})
    }

    fn model(&self, headers: &HeaderMap, model_id: &str) -> Response {
        let spec = self.gateway.config.resolve(model_id);
        if headers.contains_key("anthropic-version") {
            return json_response(200, &self.anthropic_model(model_id, &spec.description, now_s()));
        }
        json_response(200, &json!({"id": model_id, "object": "model", "created": now_s(), "owned_by": spec.backend}))
    }

    async fn chat(&self, headers: &HeaderMap, raw: &[u8], cid: &str) -> Result<Response, Error> {
        let created = now_s();
        let (req, info) = chat_completions::to_canonical(&Self::body(raw))?;
        let include_usage = info.include_usage;
        let p = self.prepare(headers, info, req, "chat", cid)?;
        let tel = self.gateway.telemetry.clone();
        if !p.stream {
            let r = observe_complete(&tel, p.runner.complete(p.req.clone(), cid), &p.req, cid).await?;
            return Ok(json_response(200, &chat_completions::response(&r, cid, created, &p.model)));
        }
        let events = with_keepalive(observe(tel, p.runner.run(p.req.clone(), cid.to_string()), &p.req, cid), self.keepalive_s());
        let stream = chat_completions::stream(events, cid.to_string(), created, p.model, include_usage);
        Ok(sse_response(guarded(stream, cid.to_string(), Flavor::OpenAi)))
    }

    async fn responses(&self, headers: &HeaderMap, raw: &[u8], rid: &str) -> Result<Response, Error> {
        let store = self.gateway.store.clone();
        let r = responses::decode_request(&Self::body(raw))?;
        let (req, info, echo) = responses::to_canonical(r, &store).await?;
        let p = self.prepare(headers, info, req, "responses", rid)?;
        let stored = Stored::new(rid, &p.model, Arc::new(echo), p.req.clone(), CanonicalResponse::default());
        let tel = self.gateway.telemetry.clone();
        if !p.stream {
            let r = observe_complete(&tel, p.runner.complete(p.req.clone(), rid), &p.req, rid).await?;
            return Ok(json_text(200, responses::complete_response(stored, r, &store).await));
        }
        let events = with_keepalive(observe(tel, p.runner.run(p.req.clone(), rid.to_string()), &p.req, rid), self.keepalive_s());
        let stream = responses::stream(events, stored, store);
        Ok(sse_response(guarded(stream, rid.to_string(), Flavor::Responses)))
    }

    async fn get_response(&self, rid: &str) -> Result<Response, Error> {
        let Some(stored) = self.gateway.store.load(rid).await else {
            return Err(ClientError::with_status(format!("response '{rid}' not found (expired or never existed)"), "not_found", 404).into());
        };
        let env = Envelope { rid, created: stored.created as i64, model: &stored.model, echo: &stored.echo };
        let items = responses::output_items(&stored.resp, rid, &stored.req.custom_tool_names());
        Ok(json_response(200, &env.render("completed", items, responses::usage(&stored.resp.usage), Some(&stored.resp))))
    }

    async fn messages(&self, headers: &HeaderMap, raw: &[u8], mid: &str) -> Result<Response, Error> {
        let (req, info) = messages::to_canonical(&Self::body(raw))?;
        let p = self.prepare(headers, info, req, "messages", mid)?;
        let tel = self.gateway.telemetry.clone();
        if !p.stream {
            let r = observe_complete(&tel, p.runner.complete(p.req.clone(), mid), &p.req, mid).await?;
            return Ok(json_response(200, &messages::response(&r, mid, &p.model)));
        }
        let events = with_keepalive(observe(tel, p.runner.run(p.req.clone(), mid.to_string()), &p.req, mid), self.keepalive_s());
        Ok(sse_response(guarded(messages::stream(events, mid.to_string(), p.model), mid.to_string(), Flavor::Anthropic)))
    }

    fn count_tokens(&self, raw: &[u8]) -> Result<Response, Error> {
        let (req, _) = messages::to_canonical(&Self::body(raw))?;
        let (prompt, _) = render_prompt(&req, i64::MAX, true, 0);
        Ok(json_response(200, &json!({"input_tokens": estimate_tokens(&prompt)})))
    }
}

type AppState = State<Arc<App>>;

/// The answer, or the error in the protocol's format; either way with the request id in the protocol's header.
fn answer(app: &App, flavor: Flavor, rid: &str, r: Result<Response, Error>) -> Response {
    let mut resp = r.unwrap_or_else(|e| app.error_response(flavor, e));
    if let Ok(v) = HeaderValue::from_str(rid) {
        resp.headers_mut().insert(flavor.request_id_header(), v);
    }
    resp
}

async fn health(State(app): AppState) -> Response {
    app.health()
}

async fn ready(State(app): AppState) -> Response {
    app.ready().await
}

async fn metrics(State(app): AppState) -> Response {
    match app.gateway.telemetry.prometheus() {
        Some(text) => {
            let mut r = Response::new(Body::from(text));
            r.headers_mut().insert("content-type", HeaderValue::from_static("text/plain; version=0.0.4; charset=utf-8"));
            r
        }
        None => error_body(
            Flavor::OpenAi,
            404,
            "metrics are off: set [telemetry] prometheus = true (or MIDIR_PROMETHEUS=1)",
            "not_found_error",
            Some("metrics_off"),
        ),
    }
}

async fn models(State(app): AppState, headers: HeaderMap) -> Response {
    app.models(&headers)
}

async fn model(State(app): AppState, headers: HeaderMap, Path(id): Path<String>) -> Response {
    app.model(&headers, &id)
}

async fn embeddings(State(app): AppState) -> Response {
    let e = ClientError::with_status(
        "embeddings are not available: no configured backend provides them (this gateway serves /v1/chat/completions, /v1/responses and /v1/messages)",
        "unsupported_endpoint",
        404,
    );
    app.error_response(Flavor::OpenAi, e.into())
}

async fn chat(State(app): AppState, headers: HeaderMap, raw: Bytes) -> Response {
    let cid = format!("chatcmpl-{}", hex_id(24));
    let r = app.chat(&headers, &raw, &cid).await;
    answer(&app, Flavor::OpenAi, &cid, r)
}

async fn create_response(State(app): AppState, headers: HeaderMap, raw: Bytes) -> Response {
    let rid = format!("resp_{}", hex_id(24));
    let r = app.responses(&headers, &raw, &rid).await;
    answer(&app, Flavor::Responses, &rid, r)
}

async fn get_response(State(app): AppState, Path(id): Path<String>) -> Response {
    let r = app.get_response(&id).await;
    answer(&app, Flavor::Responses, &id, r)
}

async fn create_message(State(app): AppState, headers: HeaderMap, raw: Bytes) -> Response {
    let mid = format!("msg_{}", hex_id(24));
    let r = app.messages(&headers, &raw, &mid).await;
    answer(&app, Flavor::Anthropic, &mid, r)
}

async fn count_tokens(State(app): AppState, raw: Bytes) -> Response {
    let rid = format!("req_{}", hex_id(24));
    answer(&app, Flavor::Anthropic, &rid, app.count_tokens(&raw))
}

fn flavor_of(path: &str) -> Flavor {
    if path.starts_with("/v1/messages") { Flavor::Anthropic } else { Flavor::OpenAi }
}

async fn not_found(request: Request) -> Response {
    let (method, path) = (request.method().clone(), request.uri().path().to_string());
    let message = format!("no endpoint {method} {path} (see GET /health for what this gateway serves)");
    error_body(flavor_of(&path), 404, &message, "not_found_error", Some("unknown_endpoint"))
}

async fn method_not_allowed(request: Request) -> Response {
    let (method, path) = (request.method().clone(), request.uri().path().to_string());
    let message = format!("{path} does not accept {method}");
    error_body(flavor_of(&path), 405, &message, "invalid_request_error", Some("method_not_allowed")).into_response()
}

/// Equal strings, compared in time that does not depend on where they differ.
fn same_secret(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// With `api_key` set, every endpoint but /health and /ready wants it, as `Authorization: Bearer <key>` (OpenAI
/// clients) or `x-api-key: <key>` (Anthropic clients).
async fn authorize(State(app): AppState, request: Request, next: Next) -> Response {
    let Some(key) = app.gateway.config.server.api_key.as_deref() else { return next.run(request).await };
    let path = request.uri().path();
    if path == "/health" || path == "/ready" {
        return next.run(request).await;
    }
    let headers = request.headers();
    let bearer =
        headers.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ").or(v.strip_prefix("bearer ")));
    let api_key = headers.get("x-api-key").and_then(|v| v.to_str().ok());
    // either header may carry it: SDKs configured with both send both
    if [bearer, api_key].into_iter().flatten().any(|g| same_secret(g.trim().as_bytes(), key.as_bytes())) {
        return next.run(request).await;
    }
    let message =
        "invalid or missing API key: this gateway wants its key (MIDIR_API_KEY) as `Authorization: Bearer <key>` or `x-api-key: <key>`";
    error_body(flavor_of(path), 401, message, "authentication_error", Some("invalid_api_key"))
}

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/metrics", get(metrics))
        .route("/v1/models", get(models))
        .route("/v1/models/{id}", get(model))
        .route("/v1/embeddings", post(embeddings))
        .route("/v1/chat/completions", post(chat))
        .route("/v1/responses", post(create_response))
        .route("/v1/responses/{id}", get(get_response))
        .route("/v1/messages", post(create_message))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(middleware::from_fn_with_state(app.clone(), authorize))
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .layer(CatchPanicLayer::custom(panic_response))
        .with_state(app)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_dates() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_791_115_200), "2026-10-04T12:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn secrets_compare_whole() {
        assert!(same_secret(b"abc", b"abc") && !same_secret(b"abc", b"abd") && !same_secret(b"abc", b"abcd"));
    }

    #[tokio::test]
    async fn a_panic_mid_stream_ends_that_stream_with_an_error_event() {
        let inner: BoxStream<'static, Result<String, Error>> = Box::pin(async_stream::stream! {
            yield Ok("event: message_start\ndata: {}\n\n".to_string());
            panic!("a bug");
        });
        let out: Vec<_> = guarded(inner, "msg_1".into(), Flavor::Anthropic).collect().await;
        assert_eq!(out.len(), 2);
        let last = String::from_utf8(out[1].as_ref().unwrap().to_vec()).unwrap();
        assert!(last.starts_with("event: error") && last.contains("internal error"), "{last}");
    }

    #[tokio::test]
    async fn a_panic_before_the_answer_is_a_500() {
        async fn bug() -> StatusCode {
            panic!("a bug")
        }
        let router = Router::new().route("/x", get(bug)).layer(CatchPanicLayer::custom(panic_response));
        let request = Request::get("/x").body(Body::empty()).unwrap();
        let response = tower::ServiceExt::oneshot(router, request).await.unwrap();
        assert_eq!(response.status(), 500);
    }
}
