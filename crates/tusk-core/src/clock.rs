use chrono::{DateTime, Utc};
use std::sync::{Arc, Mutex};

/// All timestamps flow through this trait so bitemporal tests never sleep.
pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

/// Real wall clock.
#[derive(Debug, Clone, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

/// Settable clock for tests.
#[derive(Debug, Clone)]
pub struct FakeClock {
    now: Arc<Mutex<DateTime<Utc>>>,
}

impl FakeClock {
    pub fn new(start: DateTime<Utc>) -> Self {
        FakeClock {
            now: Arc::new(Mutex::new(start)),
        }
    }

    pub fn set(&self, t: DateTime<Utc>) {
        if let Ok(mut g) = self.now.lock() {
            *g = t;
        }
    }

    pub fn advance(&self, d: chrono::Duration) {
        if let Ok(mut g) = self.now.lock() {
            *g += d;
        }
    }
}

impl Clock for FakeClock {
    fn now(&self) -> DateTime<Utc> {
        self.now
            .lock()
            .map(|g| *g)
            .unwrap_or_else(|e| **e.get_ref())
    }
}
