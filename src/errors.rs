//! Errors that cross module boundaries. Each protocol renders them in its own error format (app.rs).

use serde_json::{json, Value};

use crate::py::json as pyjson;
use crate::py::text;

/// An error answered by a backend, with its raw status, headers and body.
#[derive(Debug, Clone)]
pub struct BackendError {
    pub status: u16,
    pub body: Value,
    #[allow(dead_code)] // the backend's raw headers, kept for diagnostics
    pub headers: Vec<(String, String)>,
    pub where_: String,
    pub backend: String,
    /// QueueTimeout: a 429 of our own queue, never retried.
    pub queue_timeout: bool,
}

impl BackendError {
    pub fn new(status: u16, body: Value, headers: Vec<(String, String)>, where_: &str, backend: &str) -> Self {
        BackendError { status, body, headers, where_: where_.into(), backend: backend.into(), queue_timeout: false }
    }

    /// `QueueTimeout(timeout, needed, backend)`.
    pub fn queue_timeout(timeout: f64, needed: Option<f64>, backend: &str) -> Self {
        let why = match needed {
            Some(n) => format!("the next slot is {n:.0}s away"),
            None => format!("no slot within {timeout:.0}s"),
        };
        let msg = format!("queue: {why}, above limits.queue_timeout_s={timeout:.0} (requests_per_minute / max_concurrent)");
        BackendError {
            status: 429,
            body: json!({"message": msg}),
            headers: vec![],
            where_: "queue".into(),
            backend: backend.into(),
            queue_timeout: true,
        }
    }

    pub fn retryable(&self) -> bool {
        !self.queue_timeout && (self.status == 429 || self.status >= 500)
    }

    /// The status the client gets: the backend's own for the ones clients handle, 502 for everything else.
    pub fn http_status(&self) -> u16 {
        if matches!(self.status, 400 | 401 | 403 | 404 | 429) {
            self.status
        } else {
            502
        }
    }

    fn body_text(&self) -> String {
        match &self.body {
            Value::String(s) => s.clone(),
            other => pyjson::dumps(other, pyjson::DEFAULT),
        }
    }

    pub fn message(&self) -> String {
        format!("{} {} HTTP {}: {}", self.backend, self.where_, self.status, text::head(&self.body_text(), 2000))
    }

    /// `str(e)`.
    pub fn describe(&self) -> String {
        format!("{} {} HTTP {}: {}", self.backend, self.where_, self.status, text::head(&text::str_of(&self.body), 300))
    }
}

/// Invalid request or unsupported feature; rendered as 400/404 in the protocol's error format.
#[derive(Debug, Clone)]
pub struct ClientError {
    pub message: String,
    pub code: String,
    pub status: u16,
}

impl ClientError {
    pub fn new(message: impl Into<String>, code: &str) -> Self {
        ClientError { message: message.into(), code: code.into(), status: 400 }
    }

    pub fn with_status(message: impl Into<String>, code: &str, status: u16) -> Self {
        ClientError { message: message.into(), code: code.into(), status }
    }
}

/// A network error talking to a backend (connection, timeout, protocol).
#[derive(Debug, Clone)]
pub struct NetError {
    /// httpx-style exception name: ConnectError, ReadTimeout, RemoteProtocolError, ReadError, ...
    pub kind: String,
    pub detail: String,
    pub timeout: bool,
    pub retryable: bool,
}

impl NetError {
    /// `repr(e)`.
    pub fn repr(&self) -> String {
        format!("{}({})", self.kind, text::repr_str(&self.detail))
    }

    pub fn from_reqwest(e: &reqwest::Error, reading_body: bool) -> Self {
        let mut detail = e.to_string();
        let mut src: Option<&dyn std::error::Error> = std::error::Error::source(e);
        while let Some(s) = src {
            detail = format!("{detail}: {s}");
            src = s.source();
        }
        let low = detail.to_lowercase();
        let (kind, timeout, retryable) = if e.is_timeout() {
            (if reading_body { "ReadTimeout" } else { "ConnectTimeout" }, true, true)
        } else if e.is_connect() {
            ("ConnectError", false, true)
        } else if low.contains("closed before message completed")
            || low.contains("connection reset")
            || low.contains("incomplete")
            || low.contains("unexpected eof")
            || low.contains("connection closed")
        {
            ("RemoteProtocolError", false, !reading_body)
        } else if reading_body {
            ("ReadError", false, false)
        } else {
            ("ConnectError", false, true)
        };
        NetError { kind: kind.into(), detail, timeout, retryable }
    }
}

#[derive(Debug, Clone)]
pub enum Error {
    Client(ClientError),
    Backend(BackendError),
    Net(NetError),
    /// A bug or an unexpected input shape inside the engine (500 / "midir internal error" mid-stream).
    Internal(String),
}

impl From<ClientError> for Error {
    fn from(e: ClientError) -> Self {
        Error::Client(e)
    }
}

impl From<BackendError> for Error {
    fn from(e: BackendError) -> Self {
        Error::Backend(e)
    }
}

impl From<NetError> for Error {
    fn from(e: NetError) -> Self {
        Error::Net(e)
    }
}

impl Error {
    /// The telemetry `error.type` label.
    pub fn telemetry_type(&self) -> String {
        match self {
            Error::Backend(e) => format!("upstream_{}", e.status),
            Error::Client(_) => "ClientError".into(),
            Error::Net(n) => n.kind.clone(),
            Error::Internal(_) => "Exception".into(),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Error::Backend(e) => e.describe(),
            Error::Client(e) => e.message.clone(),
            Error::Net(n) => n.repr(),
            Error::Internal(m) => m.clone(),
        }
    }
}
