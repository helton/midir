//! Backends (backends/): what serves a model. `create_backend` builds one from its `[backends.<name>]` settings by
//! `type`. Today there is one kind, a text backend (one prompt in, streamed text out): StackSpot AI agents. The
//! emulation engine and the gateway only see the `TextBackend` trait.

pub mod stackspot;

use std::sync::Arc;

use futures::future::BoxFuture;
use futures::stream::BoxStream;

use crate::canonical::{SharedMeta, Usage};
use crate::config::{BackendSettings, ConfigError};
use crate::errors::{BackendError, Error};
use crate::limiter::UpstreamLimiter;
use crate::telemetry::Telemetry;

pub use stackspot::StackSpotBackend;

/// The end of a text stream. `usage` is None when the backend reported nothing (the engine then estimates it).
#[derive(Debug, Clone, Default)]
pub struct Completion {
    pub usage: Option<Usage>,
    pub message_id: Option<String>,
}

#[derive(Debug, Clone)]
pub enum Item {
    Text(String),
    Completion(Completion),
}

pub type ItemStream = BoxStream<'static, Result<Item, Error>>;

/// A backend that takes one text prompt and streams text back.
pub trait TextBackend: Send + Sync {
    /// The `[backends.<name>]` name.
    fn name(&self) -> &str;
    /// The kind of backend (its `type`).
    fn kind(&self) -> &'static str;
    /// The queue in front of it (concurrency, requests per minute).
    fn limiter(&self) -> &UpstreamLimiter;
    /// Startup check: what the backend needs to serve (credentials, ...).
    fn validate(&self) -> Result<(), ConfigError>;
    /// A target as /health and the startup log show it (ids abbreviated).
    fn describe_target(&self, target: &str) -> String;
    /// Readiness: credentials and network, without a model call.
    fn ready(&self) -> BoxFuture<'_, Result<(), Error>>;
    /// Text deltas for one prompt, then one Completion. Waits in the queue first.
    fn stream<'a>(&'a self, prompt: &'a str, target: &'a str, meta: Option<SharedMeta>) -> BoxFuture<'a, Result<ItemStream, Error>>;
    /// The effective options, for `midir check`: (option, environment variable that overrides it, value), secrets
    /// masked.
    fn options(&self) -> Vec<(&'static str, &'static str, String)>;
    /// (limit, actual) input tokens when `error` is the backend refusing a prompt for its size.
    fn input_limit_exceeded(&self, error: &BackendError) -> Option<(i64, i64)>;
}

pub const BACKEND_TYPES: [&str; 1] = ["stackspot"];

pub fn create_backend(
    settings: &BackendSettings,
    env: &indexmap::IndexMap<String, String>,
    telemetry: Arc<Telemetry>,
    backoff_s: f64,
) -> Result<Arc<dyn TextBackend>, ConfigError> {
    if settings.type_ != "stackspot" {
        return Err(ConfigError(format!(
            "backend {:?}: unknown type {:?} (available: {})",
            settings.name,
            settings.type_,
            BACKEND_TYPES.join(", ")
        )));
    }
    Ok(Arc::new(StackSpotBackend::new(settings, env, telemetry, backoff_s)))
}
