//! The gateway: configured backends, the runner that serves each model, and the Responses store.

use std::sync::Arc;

use indexmap::IndexMap;
use serde_json::{json, Map, Value};

use crate::backends::{create_backend, StackSpotBackend};
use crate::config::{Config, ConfigError, ModelSpec};
use crate::emulation::engine::EmulationEngine;
use crate::errors::Error;
use crate::py::text;
use crate::store::ResponseStore;
use crate::telemetry::Telemetry;

pub struct Gateway {
    pub config: Arc<Config>,
    pub telemetry: Arc<Telemetry>,
    pub backends: IndexMap<String, Arc<StackSpotBackend>>,
    pub runners: IndexMap<String, Arc<EmulationEngine>>,
    pub store: Arc<ResponseStore>,
}

impl Gateway {
    pub fn new(config: Arc<Config>, telemetry: Arc<Telemetry>) -> Result<Self, ConfigError> {
        let mut backends = IndexMap::new();
        for (name, s) in &config.backends {
            backends.insert(name.clone(), create_backend(s, &config.env, telemetry.clone(), config.server.retry_backoff_s)?);
        }
        let runners = backends.iter().map(|(n, b)| (n.clone(), Arc::new(EmulationEngine::new(b.clone(), config.clone())))).collect();
        let s = &config.server;
        let store = Arc::new(ResponseStore::new(s.responses_dir.clone(), s.responses_retention_days, s.responses_max_mb));
        Ok(Gateway { config, telemetry, backends, runners, store })
    }

    /// Startup check: every backend that serves a model has what it needs (credentials, ...).
    pub fn validate(&self) -> Result<(), ConfigError> {
        let mut used: Vec<String> = vec![];
        for m in self.config.exposed_models() {
            if !used.contains(&m.backend) {
                used.push(m.backend.clone());
            }
        }
        for name in used {
            if let Some(b) = self.backends.get(&name) {
                b.validate()?;
            }
        }
        Ok(())
    }

    pub fn route(&self, model: &str) -> (Arc<ModelSpec>, Arc<EmulationEngine>) {
        let spec = self.config.resolve(model);
        let runner = self.runners.get(&spec.backend).or_else(|| self.runners.values().next()).cloned().expect("a runner per backend");
        (spec, runner)
    }

    pub fn backend_of(&self, name: &str) -> Option<&Arc<StackSpotBackend>> {
        self.backends.get(name)
    }

    /// Per backend: None when ready, else the reason.
    pub async fn ready(&self) -> IndexMap<String, Option<String>> {
        let mut out = IndexMap::new();
        for (name, b) in &self.backends {
            let why = match b.ready().await {
                Ok(()) => None,
                Err(Error::Backend(e)) => Some(text::head(&e.message(), 500).to_string()),
                Err(Error::Net(e)) => Some(text::head(&format!("{name}: {}", e.repr()), 500).to_string()),
                Err(e) => Some(text::head(&format!("{name}: {}", e.describe()), 500).to_string()),
            };
            out.insert(name.clone(), why);
        }
        out
    }

    pub fn describe_models(&self) -> Value {
        let mut out = Map::new();
        for m in self.config.exposed_models() {
            let target = self.backends.get(&m.backend).map_or(m.target.clone(), |b| b.describe_target(&m.target));
            out.insert(
                m.name.clone(),
                json!({"backend": m.backend, "target": target, "description": m.description, "aliases": m.aliases, "match": m.match_pattern}),
            );
        }
        Value::Object(out)
    }
}
