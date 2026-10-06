//! Errors that cross module boundaries. Each protocol renders them in its own error format (app.rs).

use std::fmt;

use serde_json::{Value, json};

use crate::text::prefix;

/// An error answered by a backend, with its raw status and body.
#[derive(Debug, Clone)]
pub struct BackendError {
    pub status: u16,
    pub body: Value,
    /// what was being called ("idm", "agent", "queue")
    pub stage: String,
    pub backend: String,
    /// A 429 of our own queue: never retried.
    pub queue_timeout: bool,
}

impl BackendError {
    pub fn new(status: u16, body: Value, stage: &str, backend: &str) -> Self {
        BackendError { status, body, stage: stage.into(), backend: backend.into(), queue_timeout: false }
    }

    /// Our queue could not start the call before `limits.queue_timeout_s`.
    pub fn queue_timeout(timeout: f64, needed: Option<f64>, backend: &str) -> Self {
        let why = match needed {
            Some(n) => format!("the next slot is {n:.0}s away"),
            None => format!("no slot within {timeout:.0}s"),
        };
        let msg = format!("queue: {why}, above limits.queue_timeout_s={timeout:.0} (requests_per_minute / max_concurrent)");
        BackendError { status: 429, body: json!({"message": msg}), stage: "queue".into(), backend: backend.into(), queue_timeout: true }
    }

    /// Our queue already holds `limits.max_waiting` requests: a new one is refused at once.
    pub fn queue_full(max_waiting: i64, backend: &str) -> Self {
        let msg = format!("queue: {max_waiting} requests are already waiting for a slot (limits.max_waiting); retry later");
        BackendError { status: 429, body: json!({"message": msg}), stage: "queue".into(), backend: backend.into(), queue_timeout: true }
    }

    pub fn retryable(&self) -> bool {
        !self.queue_timeout && (self.status == 429 || self.status >= 500)
    }

    /// The status the client gets. The backend's own 401 and 403 (Midir's credentials for it, its access to an agent)
    /// are a gateway problem, not the client's key: a 502 with the backend's message, so SDKs do not ask the user to
    /// log in again or disable the provider. 401 stays for Midir's own API key.
    pub fn http_status(&self) -> u16 {
        if matches!(self.status, 400 | 404 | 429) { self.status } else { 502 }
    }

    fn body_text(&self) -> String {
        match &self.body {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        }
    }

    /// The message clients get (the backend's body, up to 2000 characters).
    pub fn message(&self) -> String {
        format!("{} {} HTTP {}: {}", self.backend, self.stage, self.status, prefix(&self.body_text(), 2000))
    }
}

/// Short form for logs (the backend's body, up to 300 characters).
impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} HTTP {}: {}", self.backend, self.stage, self.status, prefix(&self.body_text(), 300))
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetErrorKind {
    /// could not connect (DNS, refused, TLS)
    Connect,
    /// no answer in time (connect or read)
    Timeout,
    /// the connection broke before the response was complete
    Protocol,
    /// reading the body failed for another reason
    Read,
}

impl NetErrorKind {
    /// The telemetry `error.type` label.
    pub fn label(self) -> &'static str {
        match self {
            NetErrorKind::Connect => "network_connect",
            NetErrorKind::Timeout => "network_timeout",
            NetErrorKind::Protocol => "network_protocol",
            NetErrorKind::Read => "network_read",
        }
    }
}

/// A network error talking to a backend.
#[derive(Debug, Clone)]
pub struct NetError {
    pub kind: NetErrorKind,
    pub detail: String,
    /// worth another attempt (only before the response started)
    pub retryable: bool,
}

impl NetError {
    pub fn from_reqwest(e: &reqwest::Error, reading_body: bool) -> Self {
        let mut detail = e.to_string();
        let mut src: Option<&dyn std::error::Error> = std::error::Error::source(e);
        while let Some(s) = src {
            detail = format!("{detail}: {s}");
            src = s.source();
        }
        let low = detail.to_lowercase();
        let broken = ["closed before message completed", "connection reset", "incomplete", "unexpected eof", "connection closed"]
            .iter()
            .any(|m| low.contains(m));
        let (kind, retryable) = if e.is_timeout() {
            (NetErrorKind::Timeout, true)
        } else if e.is_connect() {
            (NetErrorKind::Connect, true)
        } else if broken {
            (NetErrorKind::Protocol, !reading_body)
        } else if reading_body {
            (NetErrorKind::Read, false)
        } else {
            (NetErrorKind::Connect, true)
        };
        NetError { kind, detail, retryable }
    }
}

impl fmt::Display for NetError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let what = match self.kind {
            NetErrorKind::Connect => "connection failed",
            NetErrorKind::Timeout => "timed out",
            NetErrorKind::Protocol => "connection broken",
            NetErrorKind::Read => "read failed",
        };
        write!(f, "{what}: {}", self.detail)
    }
}

#[derive(Debug, Clone)]
pub enum Error {
    Client(ClientError),
    /// boxed: the backend's body makes it the largest variant, and errors travel through every request path
    Backend(Box<BackendError>),
    Net(NetError),
    /// A bug or an unexpected state inside Midir (500 / "midir internal error" mid-stream).
    Internal(String),
}

impl From<ClientError> for Error {
    fn from(e: ClientError) -> Self {
        Error::Client(e)
    }
}

impl From<BackendError> for Error {
    fn from(e: BackendError) -> Self {
        Error::Backend(Box::new(e))
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
            Error::Client(_) => "client_error".into(),
            Error::Net(n) => n.kind.label().into(),
            Error::Internal(_) => "internal".into(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Backend(e) => e.fmt(f),
            Error::Client(e) => f.write_str(&e.message),
            Error::Net(n) => n.fmt(f),
            Error::Internal(m) => f.write_str(m),
        }
    }
}
