//! In-memory queue in front of one backend: concurrency, requests per minute, and a budget that adapts
//! to 429s. Waiters are served in arrival order (tokio's semaphore and mutex are fair).

use std::collections::VecDeque;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::Instant;

use crate::config::LimitSettings;
use crate::errors::{BackendError, Error};
use crate::telemetry::Telemetry;
use crate::text::round1;

const WINDOW: Duration = Duration::from_secs(60);

/// Times are tokio's (`tokio::time::Instant`), so tests can pause and advance the clock.
struct State {
    starts: VecDeque<Instant>,
    paused_until: Option<Instant>,
    budget: Option<i64>,
    budget_at: Instant,
}

impl State {
    /// Forget the starts that left the 60 s window.
    fn slide(&mut self, now: Instant) {
        while self.starts.front().is_some_and(|s| now.duration_since(*s) >= WINDOW) {
            self.starts.pop_front();
        }
    }

    fn paused_for(&self, now: Instant) -> f64 {
        self.paused_until.map_or(0.0, |p| p.saturating_duration_since(now).as_secs_f64())
    }

    /// Seconds until the window has room for one more start at `rpm` per minute (0 when it has room now).
    fn window_wait(&self, now: Instant, rpm: i64) -> f64 {
        let n = self.starts.len();
        if rpm <= 0 || (n as i64) < rpm {
            return 0.0;
        }
        (self.starts[n - rpm as usize] + WINDOW).saturating_duration_since(now).as_secs_f64()
    }
}

/// The longest pause a backend's `Retry-After` can impose.
const MAX_RETRY_AFTER_S: f64 = 300.0;

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
#[derive(Debug)]
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
            st: Mutex::new(State { starts: VecDeque::new(), paused_until: None, budget: None, budget_at: Instant::now() }),
            in_flight: Arc::new(AtomicI64::new(0)),
            waiting: AtomicI64::new(0),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// When a request that starts waiting now must give up: `limits.queue_timeout_s` from now.
    pub fn deadline(&self) -> Instant {
        Instant::now() + Duration::from_secs_f64(self.timeout.max(0.0))
    }

    /// The configured budget, halved by each backend 429 and grown back by one request per minute without one.
    fn effective(&self, st: &mut State, now: Instant) -> i64 {
        match st.budget {
            Some(b) if self.rpm > 0 => {
                let grown = b + (now.duration_since(st.budget_at).as_secs_f64() / 60.0).floor() as i64;
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
        let now = Instant::now();
        let mut st = self.lock();
        st.slide(now);
        let eff = self.effective(&mut st, now);
        json!({"max_concurrent": self.max_concurrent, "requests_per_minute": self.rpm, "effective_rpm": eff, "in_flight": self.in_flight.load(Ordering::SeqCst),
               "waiting": self.waiting.load(Ordering::SeqCst), "starts_last_60s": st.starts.len(), "paused_s": round1(st.paused_for(now))})
    }

    /// Seconds a client should wait before trying again (Retry-After on our 429s).
    pub fn retry_after(&self) -> i64 {
        let now = Instant::now();
        let mut st = self.lock();
        st.slide(now);
        let rpm = self.effective(&mut st, now);
        let wait = round1(st.paused_for(now)).max(st.window_wait(now, rpm)).max(self.cooldown);
        ((wait + 0.999) as i64).max(1)
    }

    /// The backend answered 429: pause new requests for the cooldown (or the backend's `Retry-After`, up to five
    /// minutes, when longer) and halve the local budget, once per episode. Requests in flight that hit the same limit
    /// in the same window only extend the pause: one burst on a shared account is one halving, not one per attempt.
    pub fn on_429(&self, retry_after: Option<f64>) {
        let now = Instant::now();
        let mut st = self.lock();
        let pause = self.cooldown.max(0.0).max(retry_after.unwrap_or(0.0).clamp(0.0, MAX_RETRY_AFTER_S));
        let until = now + Duration::from_secs_f64(pause);
        st.paused_until = Some(st.paused_until.map_or(until, |p| p.max(until)));
        if self.rpm > 0 {
            let window = Duration::from_secs_f64(self.cooldown.max(1.0));
            if st.budget.is_some() && now < st.budget_at + window {
                tracing::debug!("{} 429 again within the same episode: pause extended to {pause:.0}s, budget unchanged", self.backend);
                return;
            }
            let b = (self.effective(&mut st, now) / 2).max(10);
            st.budget = Some(b);
            st.budget_at = now;
            tracing::warn!(
                "{} 429: pausing {pause:.0}s and lowering the local budget to {b} requests/minute (recovers 1/min)",
                self.backend
            );
        }
    }

    pub async fn acquire_slot(&self, deadline: Instant) -> Result<SlotGuard, Error> {
        /// Counts a waiter until it stops waiting, however it stops (a client that goes away drops the wait).
        struct Waiting<'a>(&'a UpstreamLimiter);
        impl Drop for Waiting<'_> {
            fn drop(&mut self) {
                self.0.waiting.fetch_sub(1, Ordering::SeqCst);
                self.0.telemetry.queue_depth(-1);
            }
        }
        self.waiting.fetch_add(1, Ordering::SeqCst);
        self.telemetry.queue_depth(1);
        let waiting = Waiting(self);
        let wait = deadline.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
        let r = tokio::time::timeout(wait, self.slots.clone().acquire_owned()).await;
        drop(waiting);
        match r {
            Ok(Ok(permit)) => {
                self.in_flight.fetch_add(1, Ordering::SeqCst);
                Ok(SlotGuard { _permit: permit, in_flight: self.in_flight.clone() })
            }
            _ => Err(BackendError::queue_timeout(self.timeout, None, &self.backend).into()),
        }
    }

    /// Wait for room in the 60 s window (and for a 429 pause to end), then record one request start.
    pub async fn start(&self, deadline: Instant) -> Result<(), Error> {
        if self.rpm <= 0 {
            return Ok(());
        }
        let _order = self.rate_lock.lock().await;
        loop {
            let wait = {
                let now = Instant::now();
                let mut st = self.lock();
                st.slide(now);
                let rpm = self.effective(&mut st, now);
                let wait = st.paused_for(now).max(st.window_wait(now, rpm));
                if wait <= 0.0 {
                    st.starts.push_back(now);
                    return Ok(());
                }
                if now + Duration::from_secs_f64(wait) > deadline {
                    return Err(BackendError::queue_timeout(self.timeout, Some(wait), &self.backend).into());
                }
                wait
            };
            tokio::time::sleep(Duration::from_secs_f64(wait.min(5.0))).await;
        }
    }
}

#[cfg(test)]
mod tests {
    //! The clock is tokio's, paused: sleeps advance it instantly, so these run in milliseconds.
    use super::*;

    fn limiter(rpm: i64, concurrent: i64, timeout: f64, cooldown: f64) -> UpstreamLimiter {
        let limits =
            LimitSettings { max_concurrent: concurrent, requests_per_minute: rpm, queue_timeout_s: timeout, cooldown_on_429_s: cooldown };
        UpstreamLimiter::new(&limits, Arc::new(Telemetry::disabled()), "test")
    }

    #[tokio::test(start_paused = true)]
    async fn requests_per_minute_window() {
        let lim = limiter(3, 2, 600.0, 15.0);
        let t0 = Instant::now();
        let mut starts = vec![];
        for _ in 0..5 {
            lim.start(lim.deadline()).await.unwrap();
            starts.push(t0.elapsed().as_secs_f64());
        }
        assert!(starts[..3].iter().all(|s| *s < 1.0) && starts[3] >= 60.0 && starts[4] >= 60.0, "{starts:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn queue_timeout_is_a_429_that_is_not_retried() {
        let lim = limiter(1, 2, 10.0, 15.0);
        lim.start(lim.deadline()).await.unwrap();
        let Error::Backend(e) = lim.start(lim.deadline()).await.unwrap_err() else { panic!("not a queue timeout") };
        assert!(e.status == 429 && e.queue_timeout && !e.retryable());
    }

    #[tokio::test(start_paused = true)]
    async fn concurrency_slots() {
        let lim = limiter(0, 2, 600.0, 15.0);
        let soon = |s: f64| Instant::now() + Duration::from_secs_f64(s);
        let a = lim.acquire_slot(soon(5.0)).await.unwrap();
        let _b = lim.acquire_slot(soon(5.0)).await.unwrap();
        assert_eq!(lim.state()["in_flight"], 2);
        assert!(lim.acquire_slot(soon(0.01)).await.is_err());
        drop(a);
        let _c = lim.acquire_slot(soon(5.0)).await.unwrap();
        assert_eq!(lim.state()["in_flight"], 2);
        assert_eq!(lim.state()["waiting"], 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_429_pauses_new_starts() {
        let lim = limiter(100, 2, 600.0, 15.0);
        lim.on_429(None);
        let t = Instant::now();
        lim.start(lim.deadline()).await.unwrap();
        assert!(t.elapsed().as_secs_f64() >= 15.0);
        assert!(lim.retry_after() >= 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_429_lowers_the_budget_and_it_recovers() {
        // another gateway or client on the same account shares the 100/min: after a 429 the local budget halves,
        // then grows back by one request per quiet minute
        let lim = limiter(90, 2, 600.0, 15.0);
        lim.on_429(None);
        assert_eq!(lim.state()["effective_rpm"], 45);
        tokio::time::advance(Duration::from_secs(16)).await; // a second episode
        lim.on_429(None);
        let low = lim.state()["effective_rpm"].as_i64().unwrap();
        assert_eq!(low, 22);
        tokio::time::advance(Duration::from_secs(600)).await; // ten quiet minutes
        assert_eq!(lim.state()["effective_rpm"].as_i64().unwrap(), (low + 10).min(90));
        tokio::time::advance(Duration::from_secs(3600)).await;
        assert_eq!(lim.state()["effective_rpm"], 90);
    }

    #[tokio::test(start_paused = true)]
    async fn a_waiter_that_goes_away_is_no_longer_counted() {
        let lim = Arc::new(limiter(0, 1, 600.0, 15.0));
        let _held = lim.acquire_slot(lim.deadline()).await.unwrap();
        let waiter = {
            let lim = lim.clone();
            tokio::spawn(async move { lim.acquire_slot(lim.deadline()).await.map(|_| ()) })
        };
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(lim.state()["waiting"], 1);
        waiter.abort(); // the client disconnected
        let _ = waiter.await;
        assert_eq!(lim.state()["waiting"], 0);
    }

    #[tokio::test(start_paused = true)]
    async fn queue_wait_is_bounded_by_the_deadline() {
        let lim = limiter(0, 1, 2.0, 15.0);
        let _held = lim.acquire_slot(lim.deadline()).await.unwrap();
        let t = Instant::now();
        let Error::Backend(e) = lim.acquire_slot(lim.deadline()).await.unwrap_err() else { panic!("not a queue timeout") };
        assert!(e.queue_timeout && (t.elapsed().as_secs_f64() - 2.0).abs() < 0.1);
    }

    #[tokio::test(start_paused = true)]
    async fn concurrent_429s_halve_the_budget_once() {
        // review 2026-10-05 (F02): eight requests in flight hit the account's limit together; one episode, one halving
        let lim = limiter(90, 8, 600.0, 15.0);
        for _ in 0..8 {
            lim.on_429(None);
        }
        assert_eq!(lim.state()["effective_rpm"], 45);
        tokio::time::advance(Duration::from_secs(16)).await;
        lim.on_429(None);
        assert_eq!(lim.state()["effective_rpm"], 22);
    }

    #[tokio::test(start_paused = true)]
    async fn the_backends_retry_after_sets_a_longer_pause() {
        // review 2026-10-05 (F03): the backend asks for 60 s; the cooldown is 15 s
        let lim = limiter(90, 8, 600.0, 15.0);
        lim.on_429(Some(60.0));
        assert!(lim.state()["paused_s"].as_f64().unwrap() > 59.0, "{}", lim.state());
        let lim = limiter(90, 8, 600.0, 15.0);
        lim.on_429(Some(3600.0)); // capped at five minutes
        assert!(lim.state()["paused_s"].as_f64().unwrap() <= 300.0);
    }
}
