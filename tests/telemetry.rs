//! Telemetry: with OTEL_EXPORTER_OTLP_ENDPOINT set, Midir exports one span per request and the midir.* metrics over
//! OTLP/HTTP (protobuf), and flushes them when it is stopped (SIGTERM). The collector here decodes what it receives
//! with the official OTLP protobuf definitions.

mod common;

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::Uri;
use axum::routing::any;
use axum::Router;
use common::*;
use opentelemetry_proto::tonic::collector::metrics::v1::ExportMetricsServiceRequest;
use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
use opentelemetry_proto::tonic::common::v1::any_value::Value as AnyValue;
use opentelemetry_proto::tonic::common::v1::KeyValue;
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

#[test]
fn one_span_per_request_and_the_metrics() {
    let collector = Collector::new();
    let mut rig = Rig::with(MIDIR_TOML, &[("OTEL_EXPORTER_OTLP_ENDPOINT", collector.url.as_str())]);
    rig.upstream.add("Hello.");
    let r = rig.http.post_with_headers(
        "/v1/chat/completions",
        &json!({"model": "claude-haiku-4-5", "messages": user("hi")}),
        &[("user-agent", "claude-cli/2.1.283 (external, cli)")],
    );
    assert_eq!(r.status, 200);
    let rid = s(&r.json()["id"]).to_string();
    rig.http.get("/health"); // never a span
    rig.server.stop(); // SIGTERM: everything pending must be flushed

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
                    spans.push(a);
                }
            }
        }
    }
    assert_eq!(resource.get("service.name"), Some(&json!("midir")));
    assert!(resource.contains_key("service.version"));
    assert_eq!(spans.len(), 1, "{spans:?}"); // nothing for /health or internal steps
    let span = &spans[0];
    assert_eq!(span["midir.request_id"], json!(rid));
    assert_eq!(span["name"], "gen_ai.chat chat");
    assert_eq!(span["gen_ai.system"], "stackspot");
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
    for metric in ["midir.requests", "gen_ai.client.token.usage", "midir.request.duration"] {
        assert!(names.contains(metric), "{metric} not in {names:?}");
    }
}

#[test]
fn no_endpoint_means_no_export_attempts() {
    let rig = Rig::new();
    rig.upstream.add("x");
    assert_eq!(rig.http.post("/v1/chat/completions", &json!({"model": "gpt-5.1", "messages": user("hi")})).status, 200);
    assert!(!rig.server.logs().to_lowercase().contains("otlp"), "{}", rig.server.logs());
}
