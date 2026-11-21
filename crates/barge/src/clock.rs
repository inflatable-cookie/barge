//! Time as a seam, so expiry and last-used order are testable without waiting.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// The current time in Unix milliseconds.
pub trait Clock: fmt::Debug + Send + Sync {
    fn now_unix_ms(&self) -> u64;
}

/// The real clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl SystemClock {
    pub fn new() -> Self {
        Self
    }
}

impl Clock for SystemClock {
    fn now_unix_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0)
    }
}

/// A clock that only moves when a test moves it.
#[derive(Debug, Default)]
pub struct ManualClock {
    millis: AtomicU64,
}

impl ManualClock {
    pub fn new(unix_ms: u64) -> Self {
        Self {
            millis: AtomicU64::new(unix_ms),
        }
    }

    pub fn set(&self, unix_ms: u64) {
        self.millis.store(unix_ms, Ordering::SeqCst);
    }

    pub fn advance(&self, by_ms: u64) {
        let mut current = self.millis.load(Ordering::SeqCst);
        loop {
            let next = current.saturating_add(by_ms);
            match self.millis.compare_exchange_weak(
                current,
                next,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => return,
                Err(observed) => current = observed,
            }
        }
    }
}

impl Clock for ManualClock {
    fn now_unix_ms(&self) -> u64 {
        self.millis.load(Ordering::SeqCst)
    }
}
