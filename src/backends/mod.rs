//! Backends (backends/): what serves a model. `create_backend` builds one from its [backends.<name>] settings by
//! `type`. Today there is one kind, a text backend (one prompt in, streamed text out): StackSpot AI agents.

pub mod stackspot;

use std::sync::Arc;

use futures::stream::BoxStream;
use serde_json::Value;

use crate::canonical::Usage;
use crate::config::{BackendSettings, ConfigError};
use crate::errors::Error;
use crate::telemetry::Telemetry;

pub use stackspot::StackSpotBackend;

/// The end of a text stream. `usage` is None when the backend reported nothing (the engine then estimates it).
#[derive(Debug, Clone, Default)]
pub struct Completion {
    pub usage: Option<Usage>,
    pub message_id: Value,
    #[allow(dead_code)] // kept for diagnostics (Completion.stop_reason)
    pub stop_reason: Value,
}

#[derive(Debug, Clone)]
pub enum Item {
    Text(String),
    Completion(Completion),
}

pub type ItemStream = BoxStream<'static, Result<Item, Error>>;

pub const BACKEND_TYPES: [&str; 1] = ["stackspot"];

pub fn create_backend(
    settings: &BackendSettings,
    env: &indexmap::IndexMap<String, String>,
    telemetry: Arc<Telemetry>,
    backoff_s: f64,
) -> Result<Arc<StackSpotBackend>, ConfigError> {
    if settings.type_ != "stackspot" {
        let mut types = BACKEND_TYPES.to_vec();
        types.sort();
        return Err(ConfigError(format!(
            "backend {}: unknown type {} (available: {})",
            crate::py::text::repr_str(&settings.name),
            crate::py::text::repr_str(&settings.type_),
            types.join(", ")
        )));
    }
    Ok(Arc::new(StackSpotBackend::new(settings, env, telemetry, backoff_s)))
}
