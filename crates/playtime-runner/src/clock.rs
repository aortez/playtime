use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use playtime_core::UtcTime;

/// Time is supplied by the worker owner. `idle` must return promptly so an I/O
/// receipt or cancellation is not hidden behind a long timer sleep.
pub trait DriverClock {
    fn now_ms(&self) -> u64;
    fn utc(&self) -> UtcTime;
    fn idle(&mut self, next_ms: u64, storage_pending: bool);
}

/// Deterministic scripted time. Filesystem work consumes no simulated time by
/// default; a cloned handle can explicitly advance time during fault tests.
/// UTC is independently injected and may stay fixed; sequence orders events.
#[derive(Clone)]
pub struct VirtualClock {
    now: Arc<AtomicU64>,
    utc: UtcTime,
}
impl VirtualClock {
    pub fn new(utc: UtcTime) -> Self {
        Self {
            now: Arc::new(AtomicU64::new(0)),
            utc,
        }
    }
    pub fn advance_to(&self, now_ms: u64) {
        self.now.fetch_max(now_ms, Ordering::SeqCst);
    }
}
impl DriverClock for VirtualClock {
    fn now_ms(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }
    fn utc(&self) -> UtcTime {
        self.utc.clone()
    }
    fn idle(&mut self, next_ms: u64, storage_pending: bool) {
        if storage_pending {
            std::thread::sleep(Duration::from_millis(1));
        } else {
            self.advance_to(next_ms);
        }
    }
}

#[derive(Clone, Default)]
pub struct Cancellation(Arc<AtomicBool>);
impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}
