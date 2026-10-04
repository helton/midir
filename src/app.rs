//! HTTP layer: the OpenAI and Anthropic endpoints, errors in each protocol's own format, health and readiness, SSE
//! keepalives and error events mid-stream.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::extract::{DefaultBodyLimit, Path, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use futures::stream::BoxStream;
use futures::StreamExt;
use serde_json::{json, Map, Value};

use crate::buildinfo;
use crate::canonical::{estimate_tokens, hex_id, CanonicalRequest, Event};
use crate::config::DEFAULT_MODEL_NAME;
use crate::emulation::engine::{EmulationEngine, EventStream};
use crate::emulation::prompt::render_prompt;
use crate::errors::{ClientError, Error};
use crate::gateway::Gateway;
use crate::protocols::responses::Envelope;
use crate::protocols::{chat_completions, messages, responses};
use crate::telemetry::{self, observe, observe_complete};
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

fn json_response(status: u16, body: &Value) -> Response {
    let mut r = Response::new(Body::from(body.to_string()));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    r.headers_mut().insert("content-type", HeaderValue::from_static("application/json"));
    r
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

/// Pass `events` through, adding a keepalive event every `interval` seconds while no content has arrived yet.
pub fn with_keepalive(events: EventStream, interval: f64) -> EventStream {
    if interval <= 0.0 {
        return events;
    }
    let every = Duration::from_secs_f64(interval);
    Box::pin(async_stream::stream! {
        let mut events = events;
        let mut content = false;
        loop {
            let next = if content {
                events.next().await
            } else {
                match tokio::time::timeout(every, events.next()).await {
                    Ok(n) => n,
                    Err(_) => {
                        yield Ok(Event::Keepalive);
                        continue;
                    }
                }
            };
            let Some(ev) = next else { break };
            if matches!(ev, Ok(Event::Text(_) | Event::ToolCall(_))) {
                content = true;
            }
            yield ev;
        }
    })
}

/// The protocol stream with errors turned into an error event (never a silently dropped connection).
fn guarded(
    gen: BoxStream<'static, Result<String, Error>>,
    rid: String,
    flavor: Flavor,
) -> BoxStream<'static, Result<Bytes, std::io::Error>> {
    Box::pin(async_stream::stream! {
        let mut gen = gen;
        while let Some(chunk) = gen.next().await {
            match chunk {
                Ok(s) => yield Ok(Bytes::from(s)),
                Err(e) => {
                    let ev = match &e {
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
                    };
                    yield Ok(Bytes::from(ev));
                    break;
                }
            }
        }
    })
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
                tracing::warn!("{} {}: {}", c.status, c.code, c.message);
                error_body(flavor, c.status, &c.message, error_type(c.status, "invalid_request_error"), Some(&c.code))
            }
            Error::Backend(b) => {
                tracing::error!("{b}");
                let status = b.http_status();
                let mut resp =
                    error_body(flavor, status, &b.message(), error_type(status, "api_error"), Some(&format!("upstream_{}", b.status)));
                if status == 429 {
                    if let Some(backend) = self.gateway.backend_of(&b.backend) {
                        if let Ok(v) = HeaderValue::from_str(&backend.limiter.retry_after().to_string()) {
                            resp.headers_mut().insert("retry-after", v);
                        }
                    }
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

    fn read_json(raw: &[u8]) -> Result<Value, ClientError> {
        match serde_json::from_slice::<Value>(raw) {
            Ok(v @ Value::Object(_)) => Ok(v),
            Ok(_) => Err(ClientError::new("request body must be a JSON object", "invalid_json")),
            Err(e) => Err(ClientError::new(format!("request body is not valid JSON ({e})"), "invalid_json")),
        }
    }

    fn prepare(&self, headers: &HeaderMap, body: &Value, mut req: CanonicalRequest, protocol: &str, rid: &str) -> Result<Prepared, Error> {
        let model = match body.get("model") {
            None | Some(Value::Null) => DEFAULT_MODEL_NAME.to_string(),
            Some(Value::String(s)) if s.trim().is_empty() => DEFAULT_MODEL_NAME.to_string(),
            Some(Value::String(s)) => s.clone(),
            Some(_) => return Err(ClientError::new("invalid request: model: expected a string", "invalid_request").into()),
        };
        let stream = body.get("stream").and_then(Value::as_bool).unwrap_or(false);
        let (route, runner) = self.gateway.route(&model);
        req.route = Some(route.clone());
        telemetry::request_meta(headers, body, protocol, &model, &route, &req, rid, &self.gateway.store);
        {
            let m = req.meta();
            let prev = body.get("previous_response_id").and_then(Value::as_str).map_or(String::new(), |p| format!(" previous={p}"));
            tracing::info!(
                "{rid} {protocol} model={model}->{}/{} stream={stream} tools={} choice={} json={}{prev} client={} session={}",
                route.backend,
                route.name,
                req.tools.len(),
                req.tool_choice,
                req.json_schema.is_some(),
                m.client,
                prefix(&m.session, 12)
            );
        }
        Ok(Prepared { req: Arc::new(req), runner, model, stream })
    }

    fn keepalive_s(&self) -> f64 {
        self.gateway.config.server.keepalive_s
    }

    fn health(&self) -> Response {
        let cfg = &self.gateway.config;
        let backends: Map<String, Value> =
            self.gateway.backends.iter().map(|(n, b)| (n.clone(), json!({"type": b.type_, "queue": b.limiter.state()}))).collect();
        json_response(
            200,
            &json!({"ok": true, "version": buildinfo::full_version(), "build": buildinfo::build().as_json(), "config": cfg.source, "default": cfg.default.name,
                    "models": self.gateway.describe_models(), "backends": backends, "protocols": ["chat-completions", "responses", "messages"]}),
        )
    }

    async fn ready(&self) -> Response {
        let status = self.gateway.ready().await;
        let failed: Vec<&str> = status.values().filter_map(|w| w.as_deref()).collect();
        let backends: Map<String, Value> = status
            .iter()
            .map(|(n, why)| {
                let mut b = json!({"ok": why.is_none(), "queue": self.gateway.backends[n].limiter.state()});
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

    fn models(&self) -> Response {
        let now = now_s();
        let data: Vec<Value> = self
            .gateway
            .config
            .exposed_models()
            .iter()
            .map(|m| json!({"id": m.name, "object": "model", "created": now, "owned_by": m.backend, "description": m.description}))
            .collect();
        json_response(200, &json!({"object": "list", "data": data}))
    }

    fn model(&self, model_id: &str) -> Response {
        json_response(
            200,
            &json!({"id": model_id, "object": "model", "created": now_s(), "owned_by": self.gateway.config.resolve(model_id).backend}),
        )
    }

    async fn chat(&self, headers: &HeaderMap, raw: &[u8]) -> Result<Response, Error> {
        let body = Self::read_json(raw)?;
        let cid = format!("chatcmpl-{}", hex_id(24));
        let created = now_s();
        let req = chat_completions::to_canonical(&body)?;
        let p = self.prepare(headers, &body, req, "chat", &cid)?;
        let tel = self.gateway.telemetry.clone();
        if !p.stream {
            let r = observe_complete(&tel, p.runner.complete(p.req.clone(), &cid), &p.req, &cid).await?;
            return Ok(json_response(200, &chat_completions::response(&r, &cid, created, &p.model)));
        }
        let events = with_keepalive(observe(tel, p.runner.run(p.req.clone(), cid.clone()), &p.req, &cid), self.keepalive_s());
        let stream = chat_completions::stream(events, cid.clone(), created, p.model, chat_completions::include_usage(&body));
        Ok(sse_response(guarded(stream, cid, Flavor::OpenAi)))
    }

    async fn responses(&self, headers: &HeaderMap, raw: &[u8]) -> Result<Response, Error> {
        let body = Self::read_json(raw)?;
        let rid = format!("resp_{}", hex_id(24));
        let created = now_s();
        let store = self.gateway.store.clone();
        let req = responses::to_canonical(&body, &store)?;
        let p = self.prepare(headers, &body, req, "responses", &rid)?;
        let tel = self.gateway.telemetry.clone();
        if !p.stream {
            let r = observe_complete(&tel, p.runner.complete(p.req.clone(), &rid), &p.req, &rid).await?;
            let env = Envelope { body: &body, rid: &rid, created, model: &p.model };
            return Ok(json_response(200, &responses::complete_response(&env, p.req, r, &store)));
        }
        let events = with_keepalive(observe(tel, p.runner.run(p.req.clone(), rid.clone()), &p.req, &rid), self.keepalive_s());
        let stream = responses::stream(events, Arc::new(body), rid.clone(), created, p.model, p.req, store);
        Ok(sse_response(guarded(stream, rid, Flavor::Responses)))
    }

    fn get_response(&self, rid: &str) -> Result<Response, Error> {
        let Some((ts, req, r)) = self.gateway.store.load(rid) else {
            return Err(ClientError::with_status(format!("response '{rid}' not found (expired or never existed)"), "not_found", 404).into());
        };
        let items = responses::output_items(&r, &format!("msg_{}", hex_id(24)), &req.custom_tool_names());
        let env = Envelope { body: &json!({}), rid, created: ts as i64, model: DEFAULT_MODEL_NAME };
        Ok(json_response(200, &env.render("completed", items, responses::usage(&r.usage), Some(&r))))
    }

    async fn messages(&self, headers: &HeaderMap, raw: &[u8]) -> Result<Response, Error> {
        let body = Self::read_json(raw)?;
        let mid = format!("msg_{}", hex_id(24));
        let req = messages::to_canonical(&body)?;
        let p = self.prepare(headers, &body, req, "messages", &mid)?;
        let tel = self.gateway.telemetry.clone();
        if !p.stream {
            let r = observe_complete(&tel, p.runner.complete(p.req.clone(), &mid), &p.req, &mid).await?;
            return Ok(json_response(200, &messages::response(&r, &mid, &p.model)));
        }
        let events = with_keepalive(observe(tel, p.runner.run(p.req.clone(), mid.clone()), &p.req, &mid), self.keepalive_s());
        Ok(sse_response(guarded(messages::stream(events, mid.clone(), p.model), mid, Flavor::Anthropic)))
    }

    fn count_tokens(&self, raw: &[u8]) -> Result<Response, Error> {
        let body = Self::read_json(raw)?;
        let req = messages::to_canonical(&body)?;
        let (prompt, _) = render_prompt(&req, i64::MAX, true, 0);
        Ok(json_response(200, &json!({"input_tokens": estimate_tokens(&prompt)})))
    }
}

type AppState = State<Arc<App>>;

fn answer(app: &App, flavor: Flavor, r: Result<Response, Error>) -> Response {
    r.unwrap_or_else(|e| app.error_response(flavor, e))
}

async fn health(State(app): AppState) -> Response {
    app.health()
}

async fn ready(State(app): AppState) -> Response {
    app.ready().await
}

async fn models(State(app): AppState) -> Response {
    app.models()
}

async fn model(State(app): AppState, Path(id): Path<String>) -> Response {
    app.model(&id)
}

async fn embeddings(State(app): AppState) -> Response {
    let e = ClientError::with_status("no configured backend provides embeddings", "unsupported_endpoint", 404);
    app.error_response(Flavor::OpenAi, e.into())
}

async fn chat(State(app): AppState, headers: HeaderMap, raw: Bytes) -> Response {
    answer(&app, Flavor::OpenAi, app.chat(&headers, &raw).await)
}

async fn create_response(State(app): AppState, headers: HeaderMap, raw: Bytes) -> Response {
    answer(&app, Flavor::Responses, app.responses(&headers, &raw).await)
}

async fn get_response(State(app): AppState, Path(id): Path<String>) -> Response {
    answer(&app, Flavor::Responses, app.get_response(&id))
}

async fn create_message(State(app): AppState, headers: HeaderMap, raw: Bytes) -> Response {
    answer(&app, Flavor::Anthropic, app.messages(&headers, &raw).await)
}

async fn count_tokens(State(app): AppState, raw: Bytes) -> Response {
    answer(&app, Flavor::Anthropic, app.count_tokens(&raw))
}

fn flavor_of(path: &str) -> Flavor {
    if path.starts_with("/v1/messages") {
        Flavor::Anthropic
    } else {
        Flavor::OpenAi
    }
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

pub fn router(app: Arc<App>) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/v1/models", get(models))
        .route("/v1/models/:id", get(model))
        .route("/v1/embeddings", post(embeddings))
        .route("/v1/chat/completions", post(chat))
        .route("/v1/responses", post(create_response))
        .route("/v1/responses/:id", get(get_response))
        .route("/v1/messages", post(create_message))
        .route("/v1/messages/count_tokens", post(count_tokens))
        .fallback(not_found)
        .method_not_allowed_fallback(method_not_allowed)
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(app)
}
