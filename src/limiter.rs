//! In-memory queue in front of one backend: concurrency, requests per minute, and a budget that adapts
//! to 429s. Waiters are served in arrival order (tokio's semaphore and mutex are fair).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::config::LimitSettings;
use crate::errors::BackendError;
use crate::telemetry::Telemetry;
use crate::text::round1;

static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Seconds on a monotonic clock (only differences matter).
pub fn monotonic() -> f64 {
    EPOCH.elapsed().as_secs_f64() + 1000.0
}

struct State {
    starts: VecDeque<f64>,
    paused_until: f64,
    budget: Option<i64>,
    budget_at: f64,
}

pub struct UpstreamLimiter {
    pub max_concurrent: i64,
    pub rpm: i64,
    pub timeout: f64,
    pub cooldown: f64,
    telemetry: Arc<Telemetry>,
    backend: String,
    slots: Arc<Semaphore>,
    rate_lock: tokio::sync::Mutex<()>,
    st: Mutex<State>,
    in_flight: Arc<AtomicI64>,
    waiting: AtomicI64,
}

/// One open backend call: released (slot and in_flight) when dropped.
pub struct SlotGuard {
    _permit: OwnedSemaphorePermit,
    in_flight: Arc<AtomicI64>,
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

impl UpstreamLimiter {
    pub fn new(limits: &LimitSettings, telemetry: Arc<Telemetry>, backend: &str) -> Self {
        UpstreamLimiter {
            max_concurrent: limits.max_concurrent,
            rpm: limits.requests_per_minute,
            timeout: limits.queue_timeout_s,
            cooldown: limits.cooldown_on_429_s,
            telemetry,
            backend: backend.into(),
            slots: Arc::new(Semaphore::new(limits.max_concurrent.max(1) as usize)),
            rate_lock: tokio::sync::Mutex::new(()),
            st: Mutex::new(State { starts: VecDeque::new(), paused_until: 0.0, budget: None, budget_at: 0.0 }),
            in_flight: Arc::new(AtomicI64::new(0)),
            waiting: AtomicI64::new(0),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The configured budget, halved by each backend 429 and grown back by one request per minute without one.
    fn effective(&self, st: &mut State) -> i64 {
        match st.budget {
            Some(b) if self.rpm > 0 => {
                let grown = b + ((monotonic() - st.budget_at) / 60.0).floor() as i64;
                if grown >= self.rpm {
                    st.budget = None;
                    self.rpm
                } else {
                    grown
                }
            }
            _ => self.rpm,
        }
    }

    pub fn state(&self) -> Value {
        let now = monotonic();
        let mut st = self.lock();
        while st.starts.front().map_or(false, |s| now - s >= 60.0) {
            st.starts.pop_front();
        }
        let eff = self.effective(&mut st);
        json!({"max_concurrent": self.max_concurrent, "requests_per_minute": self.rpm, "effective_rpm": eff, "in_flight": self.in_flight.load(Ordering::SeqCst),
               "waiting": self.waiting.load(Ordering::SeqCst), "starts_last_60s": st.starts.len(), "paused_s": round1((st.paused_until - now).max(0.0))})
    }

    /// Seconds a client should wait before trying again (Retry-After on our 429s).
    pub fn retry_after(&self) -> i64 {
        let now = monotonic();
        let mut st = self.lock();
        while st.starts.front().map_or(false, |s| now - s >= 60.0) {
            st.starts.pop_front();
        }
        let paused = round1((st.paused_until - now).max(0.0));
        let rpm = self.effective(&mut st);
        let window =
            if rpm > 0 && st.starts.len() as i64 >= rpm { st.starts[st.starts.len() - rpm as usize] + 60.0 - monotonic() } else { 0.0 };
        ((paused.max(window).max(self.cooldown) + 0.999) as i64).max(1)
    }

    pub fn on_429(&self) {
        let now = monotonic();
        let mut st = self.lock();
        st.paused_until = st.paused_until.max(now + self.cooldown);
        if self.rpm > 0 {
            let b = (self.effective(&mut st) / 2).max(10);
            st.budget = Some(b);
            st.budget_at = now;
            tracing::warn!(
                "{} 429: pausing {:.0}s and lowering the local budget to {b} requests/minute (recovers 1/min)",
                self.backend,
                self.cooldown
            );
        }
    }

    pub async fn acquire_slot(&self, deadline: f64) -> Result<SlotGuard, BackendError> {
        self.waiting.fetch_add(1, Ordering::SeqCst);
        self.telemetry.queue_depth(1);
        let wait = (deadline - monotonic()).max(0.001);
        let r = tokio::time::timeout(Duration::from_secs_f64(wait), self.slots.clone().acquire_owned()).await;
        self.waiting.fetch_sub(1, Ordering::SeqCst);
        self.telemetry.queue_depth(-1);
        match r {
            Ok(Ok(permit)) => {
                self.in_flight.fetch_add(1, Ordering::SeqCst);
                Ok(SlotGuard { _permit: permit, in_flight: self.in_flight.clone() })
            }
            _ => Err(BackendError::queue_timeout(self.timeout, None, &self.backend)),
        }
    }

    /// Wait for room in the 60 s window (and for a 429 pause to end), then record one request start.
    pub async fn start(&self, deadline: f64) -> Result<(), BackendError> {
        if self.rpm <= 0 {
            return Ok(());
        }
        let _order = self.rate_lock.lock().await;
        loop {
            let wait = {
                let now = monotonic();
                let mut st = self.lock();
                while st.starts.front().map_or(false, |s| now - s >= 60.0) {
                    st.starts.pop_front();
                }
                let rpm = self.effective(&mut st);
                let window = if st.starts.len() as i64 >= rpm && rpm > 0 {
                    st.starts[st.starts.len() - rpm as usize] + 60.0 - now
                } else if rpm <= 0 {
                    f64::INFINITY
                } else {
                    0.0
                };
                let wait = (st.paused_until - now).max(window);
                if wait <= 0.0 {
                    st.starts.push_back(now);
                    return Ok(());
                }
                if now + wait > deadline {
                    return Err(BackendError::queue_timeout(self.timeout, Some(wait), &self.backend));
                }
                wait
            };
            tokio::time::sleep(Duration::from_secs_f64(wait.min(5.0))).await;
        }
    }
}
