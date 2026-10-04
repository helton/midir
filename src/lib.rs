//! Midir: an OpenAI- and Anthropic-compatible gateway for LLM backends (StackSpot AI agents today).
//!
//! The library holds everything but the command line (`main.rs`): protocol adapters, the gateway and its runners,
//! the emulation layer for text-only backends, the backends, the Responses store, telemetry and configuration.
//! docs/architecture.md describes how the pieces fit.

pub mod app;
pub mod backends;
pub mod buildinfo;
pub mod canonical;
pub mod config;
pub mod emulation;
pub mod errors;
pub mod gateway;
pub mod json;
pub mod limiter;
pub mod log;
pub mod otlp;
pub mod protocols;
pub mod store;
pub mod telemetry;
pub mod text;
