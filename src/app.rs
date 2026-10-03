//! HTTP layer: the OpenAI and Anthropic endpoints, errors in each protocol's own format, health and
//! readiness, SSE keepalives and error events mid-stream. Routing follows Starlette's rules (404/405 bodies,
//! trailing-slash redirects).

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::Response;
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
use crate::protocols::{chat_completions, common, messages, responses};
use crate::py::json as pyjson;
use crate::py::text;
use crate::telemetry::{self, observe, observe_complete};

const LOG: &str = "midir.app";

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
    let mut r = Response::new(Body::from(pyjson::dumps(body, pyjson::COMPACT)));
    *r.status_mut() = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    r.headers_mut().insert("content-type", HeaderValue::from_static("application/json"));
    r
}

fn internal_error() -> Response {
    let mut r = Response::new(Body::from("Internal Server Error"));
    *r.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
    r.headers_mut().insert("content-type", HeaderValue::from_static("text/plain; charset=utf-8"));
    r
}

fn openai_error(status: u16, message: &str, typ: &str, code: Option<&str>) -> Response {
    json_response(status, &json!({"error": {"message": message, "type": typ, "code": code, "param": null}}))
}

fn anthropic_error(status: u16, message: &str, typ: &str) -> Response {
    json_response(status, &json!({"type": "error", "error": {"type": typ, "message": message}}))
}

/// An error in the middle of a stream becomes an error event in the protocol's format.
fn stream_error(flavor: &str, message: &str, typ: &str) -> String {
    match flavor {
        "anthropic" => format!(
            "event: error\ndata: {}\n\n",
            pyjson::dumps(&json!({"type": "error", "error": {"type": typ, "message": message}}), pyjson::DEFAULT)
        ),
        "responses" => format!(
            "event: error\ndata: {}\n\n",
            pyjson::dumps(&json!({"type": "error", "code": typ, "message": message, "param": null, "sequence_number": 0}), pyjson::DEFAULT)
        ),
        _ => format!(
            "data: {}\n\ndata: [DONE]\n\n",
            pyjson::dumps(&json!({"error": {"message": message, "type": typ, "code": null, "param": null}}), pyjson::DEFAULT)
        ),
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
    flavor: &'static str,
) -> BoxStream<'static, Result<Bytes, std::io::Error>> {
    Box::pin(async_stream::stream! {
        let mut gen = gen;
        while let Some(chunk) = gen.next().await {
            match chunk {
                Ok(s) => yield Ok(Bytes::from(s)),
                Err(e) => {
                    let ev = match &e {
                        Error::Backend(b) => {
                            crate::error!(LOG, "{rid} backend error mid-stream: {}", b.describe());
                            stream_error(flavor, &b.message(), "api_error")
                        }
                        Error::Client(c) => stream_error(flavor, &c.message, "invalid_request_error"),
                        Error::Net(n) => {
                            crate::error!(LOG, "{rid} network error mid-stream: {}", n.repr());
                            stream_error(flavor, &format!("error talking to the backend: {}", n.repr()), "api_error")
                        }
                        Error::Internal(m) => {
                            crate::error!(LOG, "{rid} internal error mid-stream: {m}");
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

pub struct App {
    pub gateway: Arc<Gateway>,
}

impl App {
    fn is_anthropic(path: &str) -> bool {
        path.starts_with("/v1/messages")
    }

    fn client_error(&self, path: &str, e: &ClientError) -> Response {
        crate::warn!(LOG, "{path} {}: {}", e.code, e.message);
        if Self::is_anthropic(path) {
            anthropic_error(e.status, &e.message, error_type(e.status, "invalid_request_error"))
        } else {
            openai_error(e.status, &e.message, error_type(e.status, "invalid_request_error"), Some(&e.code))
        }
    }

    fn error_response(&self, path: &str, e: Error) -> Response {
        match e {
            Error::Client(c) => self.client_error(path, &c),
            Error::Backend(b) => {
                crate::error!(LOG, "{} {} {}: {}", b.backend, b.where_, b.status, text::head(&text::str_of(&b.body), 500));
                let status = b.http_status();
                let typ = error_type(status, "api_error");
                let mut resp = if Self::is_anthropic(path) {
                    anthropic_error(status, &b.message(), typ)
                } else {
                    openai_error(status, &b.message(), typ, Some(&format!("upstream_{}", b.status)))
                };
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
                let status = if n.timeout { 504 } else { 502 };
                let message = format!("error talking to the backend: {}", n.repr());
                if Self::is_anthropic(path) {
                    anthropic_error(status, &message, "api_error")
                } else {
                    openai_error(status, &message, "api_error", Some("upstream_network"))
                }
            }
            Error::Internal(m) => {
                crate::error!(LOG, "Exception in ASGI application: {m}");
                internal_error()
            }
        }
    }

    fn read_json(body: &[u8]) -> Result<Value, ClientError> {
        match pyjson::loads_bytes(body) {
            None => Err(ClientError::new("request body is not valid JSON", "invalid_json")),
            Some(v @ Value::Object(_)) => Ok(v),
            Some(_) => Err(ClientError::new("request body must be a JSON object", "invalid_json")),
        }
    }

    /// Canonical request routed to its model: (request, runner, requested model name, stream?).
    fn prepare(
        &self,
        headers: &HeaderMap,
        body: &Value,
        mut req: CanonicalRequest,
        protocol: &str,
        rid: &str,
    ) -> Result<(Arc<CanonicalRequest>, Arc<EmulationEngine>, Value, bool), Error> {
        let model_name = match body.get("model") {
            Some(m) if text::truthy(m) => m.clone(),
            _ => json!(DEFAULT_MODEL_NAME),
        };
        let Value::String(model_str) = &model_name else {
            return Err(Error::Internal(format!("AttributeError(\"'{}' object has no attribute 'strip'\")", text::type_name(&model_name))));
        };
        let stream = body.get("stream").map_or(false, text::truthy);
        let (route, runner) = self.gateway.route(model_str);
        req.route = Some(route.clone());
        telemetry::request_meta(headers, body, protocol, model_str, &route, &req, rid, &self.gateway.store);
        {
            let m = req.meta();
            let prev = match body.get("previous_response_id") {
                Some(p) if text::truthy(p) => format!(" previous={}", text::str_of(p)),
                _ => String::new(),
            };
            crate::info!(
                LOG,
                "{rid} {protocol} model={model_str}->{}/{} stream={} tools={} choice={} json={}{prev} client={} session={}",
                route.backend,
                route.name,
                if stream { "True" } else { "False" },
                req.tools.len(),
                req.tool_choice.display(),
                if req.json_schema.is_some() { "True" } else { "False" },
                m.client,
                text::head(&m.session, 12)
            );
        }
        Ok((Arc::new(req), runner, model_name, stream))
    }

    fn keepalive_s(&self) -> f64 {
        self.gateway.config.server.keepalive_s
    }

    async fn health(&self) -> Response {
        let cfg = &self.gateway.config;
        let backends: Map<String, Value> =
            self.gateway.backends.iter().map(|(n, b)| (n.clone(), json!({"type": b.type_, "queue": b.limiter.state()}))).collect();
        json_response(
            200,
            &json!({"ok": true, "version": buildinfo::full_version(), "build": buildinfo::build().as_dict(), "config": cfg.source, "default": cfg.default.name,
                    "models": self.gateway.describe_models(), "backends": backends, "protocols": ["chat-completions", "responses", "messages"]}),
        )
    }

    async fn ready(&self) -> Response {
        let status = self.gateway.ready().await;
        let failed: Vec<String> = status.values().filter_map(|w| w.clone()).collect();
        let mut backends = Map::new();
        for (n, why) in &status {
            let mut b = Map::new();
            b.insert("ok".into(), json!(why.is_none()));
            if let Some(w) = why {
                b.insert("error".into(), json!(w));
            }
            b.insert("queue".into(), self.gateway.backends[n].limiter.state());
            backends.insert(n.clone(), Value::Object(b));
        }
        let mut body = json!({"ok": failed.is_empty(), "version": buildinfo::full_version(), "build": buildinfo::build().as_dict(), "backends": backends});
        if !failed.is_empty() {
            body["error"] = json!(failed.join("; "));
            return json_response(503, &body);
        }
        json_response(200, &body)
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

    async fn chat(&self, path: &str, headers: &HeaderMap, raw: &[u8]) -> Result<Response, Error> {
        let body = Self::read_json(raw)?;
        let cid = format!("chatcmpl-{}", hex_id(24));
        let created = now_s();
        let req = common::to_canonical("ChatCompletions", chat_completions::to_canonical(&body))?;
        let (req, runner, model, stream) = self.prepare(headers, &body, req, "chat", &cid)?;
        let tel = self.gateway.telemetry.clone();
        if !stream {
            let r = observe_complete(&tel, runner.complete(req.clone(), &cid), &req, &cid).await?;
            return Ok(json_response(200, &chat_completions::response(&r, &cid, created, &model)));
        }
        let include_usage = match body.get("stream_options") {
            Some(o) if text::truthy(o) => match o {
                Value::Object(m) => m.get("include_usage").map_or(true, text::truthy),
                other => {
                    return Err(Error::Internal(format!("AttributeError(\"'{}' object has no attribute 'get'\")", text::type_name(other))))
                }
            },
            _ => true,
        };
        let events = with_keepalive(observe(tel, runner.run(req.clone(), cid.clone()), &req, &cid), self.keepalive_s());
        let _ = path;
        Ok(sse_response(guarded(chat_completions::stream(events, cid.clone(), created, model, include_usage), cid, "openai")))
    }

    async fn responses(&self, headers: &HeaderMap, raw: &[u8]) -> Result<Response, Error> {
        let body = Self::read_json(raw)?;
        let rid = format!("resp_{}", hex_id(24));
        let created = now_s();
        let store = self.gateway.store.clone();
        let req = common::to_canonical("Responses", responses::to_canonical(&body, &store))?;
        let (req, runner, model, stream) = self.prepare(headers, &body, req, "responses", &rid)?;
        let tel = self.gateway.telemetry.clone();
        if !stream {
            let r = observe_complete(&tel, runner.complete(req.clone(), &rid), &req, &rid).await?;
            return Ok(json_response(200, &responses::complete_response(&body, &rid, created, &model, req, r, &store)));
        }
        let events = with_keepalive(observe(tel, runner.run(req.clone(), rid.clone()), &req, &rid), self.keepalive_s());
        Ok(sse_response(guarded(responses::stream(events, Arc::new(body), rid.clone(), created, model, req, store), rid, "responses")))
    }

    fn get_response(&self, rid: &str) -> Result<Response, Error> {
        let Some((ts, req, r)) = self.gateway.store.load(rid) else {
            return Err(ClientError::with_status(format!("response '{rid}' not found (expired or never existed)"), "not_found", 404).into());
        };
        let items = responses::output_items(&r, &format!("msg_{}", hex_id(24)), &req.custom_tool_names());
        Ok(json_response(
            200,
            &responses::envelope(
                &json!({}),
                rid,
                ts as i64,
                &json!(DEFAULT_MODEL_NAME),
                "completed",
                items,
                responses::usage(&r.usage),
                Some(&r),
            ),
        ))
    }

    async fn messages(&self, headers: &HeaderMap, raw: &[u8]) -> Result<Response, Error> {
        let body = Self::read_json(raw)?;
        let mid = format!("msg_{}", hex_id(24));
        let req = common::to_canonical("Messages", messages::to_canonical(&body))?;
        let (req, runner, model, stream) = self.prepare(headers, &body, req, "messages", &mid)?;
        let tel = self.gateway.telemetry.clone();
        if !stream {
            let r = observe_complete(&tel, runner.complete(req.clone(), &mid), &req, &mid).await?;
            return Ok(json_response(200, &messages::response(&r, &mid, &model)));
        }
        let events = with_keepalive(observe(tel, runner.run(req.clone(), mid.clone()), &req, &mid), self.keepalive_s());
        Ok(sse_response(guarded(messages::stream(events, mid.clone(), model), mid, "anthropic")))
    }

    fn count_tokens(&self, raw: &[u8]) -> Result<Response, Error> {
        let body = Self::read_json(raw)?;
        let req = common::to_canonical("Messages", messages::to_canonical(&body))?;
        let (prompt, _) = render_prompt(&req, 1_000_000_000, true, 0);
        Ok(json_response(200, &json!({"input_tokens": estimate_tokens(&prompt)})))
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Route {
    Health,
    Ready,
    Models,
    Model,
    Embeddings,
    Chat,
    Responses,
    GetResponse,
    Messages,
    CountTokens,
}

fn match_route(path: &str) -> Option<(Route, &'static [&'static str], String)> {
    const GET: &[&str] = &["GET", "HEAD"];
    const POST: &[&str] = &["POST"];
    let r = match path {
        "/health" => (Route::Health, GET, String::new()),
        "/ready" => (Route::Ready, GET, String::new()),
        "/v1/models" => (Route::Models, GET, String::new()),
        "/v1/embeddings" => (Route::Embeddings, POST, String::new()),
        "/v1/chat/completions" => (Route::Chat, POST, String::new()),
        "/v1/responses" => (Route::Responses, POST, String::new()),
        "/v1/messages" => (Route::Messages, POST, String::new()),
        "/v1/messages/count_tokens" => (Route::CountTokens, POST, String::new()),
        p => {
            if let Some(id) = p.strip_prefix("/v1/models/").filter(|s| !s.is_empty() && !s.contains('/')) {
                (Route::Model, GET, id.to_string())
            } else if let Some(id) = p.strip_prefix("/v1/responses/").filter(|s| !s.is_empty() && !s.contains('/')) {
                (Route::GetResponse, GET, id.to_string())
            } else {
                return None;
            }
        }
    };
    Some(r)
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = ((b[i + 1] as char).to_digit(16), (b[i + 2] as char).to_digit(16)) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

async fn handle(State(app): State<Arc<App>>, request: Request) -> Response {
    let (parts, body) = request.into_parts();
    let path = percent_decode(parts.uri.path());
    let Some((route, methods, param)) = match_route(&path) else {
        // Starlette's redirect_slashes: the same path with/without a trailing slash exists -> 307
        if path != "/" {
            let alt = if path.ends_with('/') { path.trim_end_matches('/').to_string() } else { format!("{path}/") };
            if match_route(&alt).is_some() {
                let host = parts.headers.get("host").and_then(|h| h.to_str().ok()).unwrap_or("127.0.0.1");
                let query = parts.uri.query().map_or(String::new(), |q| format!("?{q}"));
                let mut r = Response::new(Body::empty());
                *r.status_mut() = StatusCode::TEMPORARY_REDIRECT;
                if let Ok(v) = HeaderValue::from_str(&format!("http://{host}{alt}{query}")) {
                    r.headers_mut().insert("location", v);
                }
                return r;
            }
        }
        return json_response(404, &json!({"detail": "Not Found"}));
    };
    if !methods.contains(&parts.method.as_str()) {
        let mut r = json_response(405, &json!({"detail": "Method Not Allowed"}));
        if let Ok(v) = HeaderValue::from_str(&methods.join(", ")) {
            r.headers_mut().insert("allow", v);
        }
        return r;
    }
    let raw = if parts.method == Method::POST { axum::body::to_bytes(body, usize::MAX).await.unwrap_or_default() } else { Bytes::new() };
    let result = match route {
        Route::Health => Ok(app.health().await),
        Route::Ready => Ok(app.ready().await),
        Route::Models => Ok(app.models()),
        Route::Model => Ok(app.model(&param)),
        Route::Embeddings => Err(ClientError::with_status("no configured backend provides embeddings", "unsupported_endpoint", 404).into()),
        Route::Chat => app.chat(&path, &parts.headers, &raw).await,
        Route::Responses => app.responses(&parts.headers, &raw).await,
        Route::GetResponse => app.get_response(&param),
        Route::Messages => app.messages(&parts.headers, &raw).await,
        Route::CountTokens => app.count_tokens(&raw),
    };
    let mut resp = match result {
        Ok(r) => r,
        Err(e) => app.error_response(&path, e),
    };
    if parts.method == Method::HEAD {
        *resp.body_mut() = Body::empty();
    }
    resp
}

pub fn router(app: Arc<App>) -> Router {
    Router::new().fallback(handle).with_state(app)
}
