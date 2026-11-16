//! Injected time, so the settle/timeout logic runs in virtual time against the fake transport.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub trait Clock: Send + Sync {
    /// Unix time in milliseconds.
    fn now_ms(&self) -> u64;
    fn sleep_ms(&self, ms: u64);
}

/// The real wall clock.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        crate::util::wall_ms()
    }
    fn sleep_ms(&self, ms: u64) {
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
}

/// Virtual time: `sleep_ms` advances it instantly.
pub struct FakeClock(AtomicU64);

impl FakeClock {
    pub fn new(start_ms: u64) -> Arc<Self> {
        Arc::new(FakeClock(AtomicU64::new(start_ms)))
    }
    pub fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
    fn sleep_ms(&self, ms: u64) {
        self.advance(ms);
    }
}

pub type SharedClock = Arc<dyn Clock>;
