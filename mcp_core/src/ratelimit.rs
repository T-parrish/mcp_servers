//! Outbound request pacing, shared by every server's HTTP client.

use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::{Semaphore, SemaphorePermit};

/// A small jitter in `[0, span/2]`, using the clock as cheap entropy so paced
/// requests don't land on an exact grid.
fn jitter(span: Duration) -> Duration {
    let n = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    span.mul_f64((n as f64 / 1_000_000_000.0) * 0.5)
}

/// Bounds outbound requests: at most `max_concurrent` in flight, and request
/// *starts* spaced at least `min_interval` (plus jitter) apart.
pub struct RateLimiter {
    semaphore: Semaphore,
    min_interval: Duration,
    /// Earliest instant the next request may start.
    next_slot: Mutex<Instant>,
}

impl RateLimiter {
    pub fn new(max_concurrent: usize, min_interval: Duration) -> Self {
        Self {
            semaphore: Semaphore::new(max_concurrent),
            min_interval,
            next_slot: Mutex::new(Instant::now()),
        }
    }

    /// Build a limiter from `{PREFIX}_MAX_CONCURRENT_REQUESTS` and
    /// `{PREFIX}_MIN_REQUEST_INTERVAL_MS`, falling back to the given defaults.
    pub fn from_env(prefix: &str, default_concurrency: usize, default_interval_ms: u64) -> Self {
        let max_concurrent = std::env::var(format!("{prefix}_MAX_CONCURRENT_REQUESTS"))
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| n >= 1)
            .unwrap_or(default_concurrency);
        let min_interval_ms = std::env::var(format!("{prefix}_MIN_REQUEST_INTERVAL_MS"))
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(default_interval_ms);
        tracing::info!(
            max_concurrent,
            min_interval_ms,
            "configured request rate limiter"
        );
        Self::new(max_concurrent, Duration::from_millis(min_interval_ms))
    }

    /// Wait for a concurrency slot and the paced start time, returning a permit
    /// that must be held for the duration of the request.
    pub async fn acquire(&self) -> SemaphorePermit<'_> {
        let waited_from = Instant::now();
        let permit = self
            .semaphore
            .acquire()
            .await
            .expect("rate-limiter semaphore is never closed");

        // Reserve a paced start slot. The std mutex is dropped before awaiting.
        let start_at = {
            let mut slot = self.next_slot.lock().unwrap();
            let start_at = (*slot).max(Instant::now());
            *slot = start_at + self.min_interval + jitter(self.min_interval);
            start_at
        };
        if let Some(delay) = start_at.checked_duration_since(Instant::now()) {
            tokio::time::sleep(delay).await;
        }
        crate::metrics::record_rate_limiter_wait(waited_from.elapsed());
        permit
    }
}
