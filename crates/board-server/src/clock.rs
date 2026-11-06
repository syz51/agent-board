use std::sync::{Arc, Mutex};

use board_proto::Timestamp;
use chrono::{DurationRound, TimeDelta, Utc};

/// Source of the current time, injectable so expiry and staleness are testable.
pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> Timestamp;
}

/// The system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Utc::now()
    }
}

/// A clock that only moves when told to; clones share the same time.
#[derive(Debug, Clone)]
pub struct ManualClock(Arc<Mutex<Timestamp>>);

impl ManualClock {
    pub fn new(start: Timestamp) -> Self {
        Self(Arc::new(Mutex::new(start)))
    }

    pub fn set(&self, at: Timestamp) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) = at;
    }

    pub fn advance(&self, by: TimeDelta) {
        *self.0.lock().unwrap_or_else(|e| e.into_inner()) += by;
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Timestamp {
        *self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Times are stored with millisecond precision; truncating here keeps values round-trip
/// equal.
pub(crate) fn now_ms(clock: &dyn Clock) -> Timestamp {
    truncate(clock.now())
}

pub(crate) fn truncate(at: Timestamp) -> Timestamp {
    at.duration_trunc(TimeDelta::milliseconds(1)).unwrap_or(at)
}
