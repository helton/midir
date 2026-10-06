//! Telemetry: with OTEL_EXPORTER_OTLP_ENDPOINT set, Midir exports a span per request (SERVER, the child of the client's
//! traceparent) with a CLIENT child per backend call, and the midir.* metrics, over OTLP/HTTP (protobuf), and flushes
//! them when it is stopped (SIGTERM). The collector here decodes what it receives with the official OTLP protobuf
//! definitions. Without a collector, /metrics can serve the metrics as Prometheus text.

mod common;

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::Uri;
use axum::routing::any;
use common::*;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::KeyValue;
use opentelemetry_proto::tonic::common::v1::any_value::Value as AnyValue;
use prost::Message;
use serde_json::json;

type Bodies = Arc<Mutex<HashMap<String, Vec<Vec<u8>>>>>;

/// Records OTLP/HTTP protobuf exports by path (/v1/traces, /v1/metrics).
struct Collector {
    bodies: Bodies,
    url: String,
    _rt: tokio::runtime::Runtime,
}

impl Collector {
    fn new() -> Self {
        let bodies: Bodies = Arc::default();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
        let app = Router::new()
            .fallback(any(|State(b): State<Bodies>, uri: Uri, body: Bytes| async move {
                b.lock().unwrap().entry(uri.path().to_string()).or_default().push(body.to_vec());
                ([("content-type", "application/x-protobuf")], Bytes::new())
            }))
            .with_state(bodies.clone());
        rt.spawn(async move { axum::serve(tokio::net::TcpListener::from_std(listener).unwrap(), app).await.unwrap() });
        Collector { bodies, url, _rt: rt }
    }

    fn exports(&self, path: &str) -> Vec<Vec<u8>> {
        self.bodies.lock().unwrap().get(path).cloned().unwrap_or_default()
    }
}

fn value(kv: &KeyValue) -> serde_json::Value {
    match kv.value.as_ref().and_then(|v| v.value.as_ref()) {
        Some(AnyValue::StringValue(s)) => json!(s),
        Some(AnyValue::IntValue(i)) => json!(i),
        Some(AnyValue::DoubleValue(d)) => json!(d),
        Some(AnyValue::BoolValue(b)) => json!(b),
        _ => serde_json::Value::Null,
    }
}

fn attrs(kvs: &[KeyValue]) -> HashMap<String, serde_json::Value> {
    kvs.iter().map(|kv| (kv.key.clone(), value(kv))).collect()
}

type Attributes = HashMap<String, serde_json::Value>;

/// The resource attributes and the spans the collector received (name, kind, ids and status mixed into the attributes).
fn spans_of(collector: &Collector) -> (Attributes, Vec<Attributes>) {
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    let mut resource = HashMap::new();
    let mut spans = vec![];
    for body in collector.exports("/v1/traces") {
        let req = ExportTraceServiceRequest::decode(body.as_slice()).unwrap();
        for rs in req.resource_spans {
            resource.extend(attrs(&rs.resource.map(|r| r.attributes).unwrap_or_default()));
            for ss in rs.scope_spans {
                for span in ss.spans {
                    let mut a = attrs(&span.attributes);
                    a.insert("name".into(), json!(span.name));
                    a.insert("kind".into(), json!(span.kind));
                    a.insert("trace".into(), json!(hex(&span.trace_id)));
                    a.insert("id".into(), json!(hex(&span.span_id)));
                    a.insert("parent".into(), json!(hex(&span.parent_span_id)));
                    a.insert("status".into(), json!(span.status.map_or(0, |s| s.code)));
                    spans.push(a);
                }
            }
        }
    }
    (resource, spans)
}

const TRACE: &str = "4bf92f3577b34da6a3ce929d0e0e4736";
const PARENT: &str = "00f067aa0ba902b7";

#[test]
fn one_span_per_request_and_the_metrics() {
    let collector = Collector::new();
    let mut rig = Rig::with(MIDIR_TOML, &[("OTEL_EXPORTER_OTLP_ENDPOINT", collector.url.as_str())]);
    rig.upstream.add("Hello.");
    let traceparent = format!("00-{TRACE}-{PARENT}-01");
    let r = rig.http.post_with_headers(
        "/v1/chat/completions",
        &json!({"model": "claude-haiku-4-5", "messages": user("hi")}),
        &[("user-agent", "claude-cli/2.1.283 (external, cli)"), ("traceparent", traceparent.as_str())],
    );
    assert_eq!(r.status, 200);
    let rid = s(&r.json()["id"]).to_string();
    assert_eq!(r.header("x-request-id"), Some(rid.as_str()));
    rig.http.get("/health"); // never a span
    rig.server.stop(); // SIGTERM: everything pending must be flushed

    let (resource, spans) = spans_of(&collector);
    assert_eq!(resource.get("service.name"), Some(&json!("midir")));
    assert!(resource.contains_key("service.version"));
    assert_eq!(spans.len(), 2, "{spans:?}"); // the request and its backend call; nothing for /health
    let (span, call) = if spans[0]["kind"] == 2 { (&spans[0], &spans[1]) } else { (&spans[1], &spans[0]) };
    assert_eq!((span["kind"].clone(), call["kind"].clone()), (json!(2), json!(3))); // SERVER, CLIENT
    assert_eq!((span["trace"].clone(), span["parent"].clone()), (json!(TRACE), json!(PARENT))); // the client's trace
    assert_eq!((call["trace"].clone(), call["parent"].clone()), (json!(TRACE), span["id"].clone()));
    assert_eq!((span["status"].clone(), call["status"].clone()), (json!(0), json!(0))); // neither ended in error
    assert_eq!(call["name"], "chat gpt-4.1");
    assert_eq!(call["gen_ai.provider.name"], "stackspot");
    assert_eq!(call["midir.call"], "first");
    assert!(call["gen_ai.usage.output_tokens"].as_i64().unwrap() > 0);
    assert_eq!(span["midir.request_id"], json!(rid));
    assert_eq!(span["name"], "chat claude-haiku-4-5"); // {operation} {model}
    assert_eq!(span["http.request.method"], "POST");
    assert_eq!(span["http.route"], "/v1/chat/completions");
    assert_eq!(span["gen_ai.provider.name"], "stackspot");
    assert_eq!(span["midir.backend"], "stackspot"); // the configured name; the provider is the backend's type
    assert_eq!(span["gen_ai.request.model"], "claude-haiku-4-5");
    assert_eq!(span["gen_ai.response.model"], "gpt-4.1");
    assert_eq!(span["client.name"], "claude-code");
    assert_eq!(span["midir.protocol"], "chat");
    assert_eq!(span["midir.finish"], "stop");
    assert!(span["gen_ai.usage.input_tokens"].as_i64().unwrap() > 0 && span["gen_ai.usage.output_tokens"].as_i64().unwrap() > 0);
    assert_eq!(span["midir.upstream_calls"], 1);

    let mut names = BTreeSet::new();
    for body in collector.exports("/v1/metrics") {
        let req = ExportMetricsServiceRequest::decode(body.as_slice()).unwrap();
        for rm in req.resource_metrics {
            for sm in rm.scope_metrics {
                names.extend(sm.metrics.into_iter().map(|m| m.name));
            }
        }
    }
    for metric in [
        "midir.requests",
        "gen_ai.client.token.usage",
        "midir.request.duration",
        "gen_ai.server.request.duration", // in seconds; time to first token only for streams
    ] {
        assert!(names.contains(metric), "{metric} not in {names:?}");
    }
}

#[test]
fn prometheus_metrics_without_a_collector() {
    let rig = Rig::with(MIDIR_TOML, &[("MIDIR_PROMETHEUS", "1")]);
    rig.upstream.add("x");
    assert_eq!(rig.http.post("/v1/chat/completions", &json!({"model": "gpt-5.1", "messages": user("hi")})).status, 200);
    let m = rig.http.get("/metrics");
    assert_eq!(m.status, 200);
    assert!(
        m.text.contains("# TYPE midir_requests_total counter") && m.text.contains("midir_request_duration_milliseconds_bucket{"),
        "{}",
        m.text
    );
    assert_eq!(Rig::new().http.get("/metrics").status, 404); // off by default
}

#[test]
fn no_endpoint_means_no_export_attempts() {
    let rig = Rig::new();
    rig.upstream.add("x");
    assert_eq!(rig.http.post("/v1/chat/completions", &json!({"model": "gpt-5.1", "messages": user("hi")})).status, 200);
    assert!(!rig.server.logs().to_lowercase().contains("otlp"), "{}", rig.server.logs());
}

#[test]
fn stateless_requests_do_not_mint_a_metric_series_each() {
    // review 2026-10-05 (F13): a Responses request with no session and no chain got its own session label, so a
    // stateless client created a set of series per request
    let rig = Rig::with(MIDIR_TOML, &[("MIDIR_PROMETHEUS", "1")]);
    for i in 0..5 {
        rig.upstream.add("ok");
        let r = rig.http.post("/v1/responses", &json!({"model": "gpt-5.1", "input": format!("q{i}"), "store": false}));
        assert_eq!(r.status, 200);
    }
    let text = rig.http.get("/metrics").text;
    let sessions: std::collections::HashSet<&str> = text
        .lines()
        .filter(|l| l.starts_with("midir_requests_total{"))
        .filter_map(|l| l.split("session_id=\"").nth(1).and_then(|r| r.split('"').next()))
        .collect();
    assert_eq!(sessions.len(), 1, "{sessions:?}");
    assert!(sessions.contains("-"));
    assert!(!text.contains("gen_ai_request_model")); // client-sent text is not a metric label
}

#[test]
fn an_unsampled_trace_gets_no_spans() {
    // review 2026-10-05 (F27): a traceparent with the sampled flag off still got recorded children (orphans in the
    // collector, whose parent was never exported)
    let collector = Collector::new();
    let mut rig = Rig::with(MIDIR_TOML, &[("OTEL_EXPORTER_OTLP_ENDPOINT", collector.url.as_str())]);
    rig.upstream.add("Hello.");
    let traceparent = format!("00-{TRACE}-{PARENT}-00");
    let r = rig.http.post_with_headers(
        "/v1/chat/completions",
        &json!({"model": "gpt-5.1", "messages": user("hi")}),
        &[("traceparent", traceparent.as_str())],
    );
    assert_eq!(r.status, 200);
    rig.server.stop();
    let (_, spans) = spans_of(&collector);
    assert!(spans.is_empty(), "{spans:?}");
    assert!(!collector.exports("/v1/metrics").is_empty()); // the metrics are still recorded
}

#[test]
fn the_service_name_follows_the_otel_variables() {
    // review 2026-10-05 (F27): the configuration always overrode OTEL_RESOURCE_ATTRIBUTES' service.name, and an
    // OTLP protocol Midir does not speak was ignored in silence
    let collector = Collector::new();
    let mut rig = Rig::with(
        MIDIR_TOML,
        &[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", collector.url.as_str()),
            ("OTEL_RESOURCE_ATTRIBUTES", "service.name=gateway-a,deployment.environment=lab"),
            ("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc"),
        ],
    );
    rig.upstream.add("Hello.");
    assert_eq!(rig.http.post("/v1/chat/completions", &json!({"model": "gpt-5.1", "messages": user("hi")})).status, 200);
    rig.server.stop();
    let (resource, _) = spans_of(&collector);
    assert_eq!(resource.get("service.name"), Some(&json!("gateway-a")));
    assert_eq!(resource.get("deployment.environment"), Some(&json!("lab")));
    assert_eq!(resource.get("telemetry.sdk.name"), Some(&json!("midir")));
    assert!(rig.server.logs().contains("OTEL_EXPORTER_OTLP_PROTOCOL=grpc is not supported"), "{}", rig.server.logs());

    let collector = Collector::new();
    let mut rig = Rig::with(
        MIDIR_TOML,
        &[
            ("OTEL_EXPORTER_OTLP_ENDPOINT", collector.url.as_str()),
            ("OTEL_RESOURCE_ATTRIBUTES", "service.name=gateway-a"),
            ("OTEL_SERVICE_NAME", "gateway-b"),
        ],
    );
    rig.upstream.add("Hello.");
    assert_eq!(rig.http.post("/v1/chat/completions", &json!({"model": "gpt-5.1", "messages": user("hi")})).status, 200);
    rig.server.stop();
    assert_eq!(spans_of(&collector).0.get("service.name"), Some(&json!("gateway-b")));
}

#[test]
fn metrics_behind_the_api_key_and_escaped_labels() {
    // review 2026-10-05 (F28): /metrics under an API key and label values a client controls
    let rig = Rig::with(MIDIR_TOML, &[("MIDIR_PROMETHEUS", "1"), ("MIDIR_API_KEY", "s3cret")]);
    rig.upstream.add("x");
    let r = rig.http.post_with_headers(
        "/v1/chat/completions",
        &json!({"model": "gpt-5.1", "messages": user("hi")}),
        &[("authorization", "Bearer s3cret"), ("x-session-id", r#"a"b\c"#)],
    );
    assert_eq!(r.status, 200, "{}", r.text);
    assert_eq!(rig.http.get("/metrics").status, 401);
    let m = rig.http.get_with_headers("/metrics", &[("authorization", "Bearer s3cret")]);
    assert_eq!(m.status, 200);
    assert!(m.text.contains(r#"session_id="a\"b\\c""#), "{}", m.text);
    assert!(m.text.contains("gen_ai_server_request_duration_seconds_bucket{"), "{}", m.text);
}
