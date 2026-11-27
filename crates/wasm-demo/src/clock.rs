//! Monotonic UI time supplied by the frame ABI. The base permits backdating.
use std::{cell::Cell, ops::Sub, time::Duration};

thread_local! {
    static NOW: Cell<Instant> = const { Cell::new(Instant(60_000)) };
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Instant(u64);

impl Instant {
    pub fn now() -> Self {
        NOW.get()
    }

    pub fn duration_since(self, earlier: Self) -> Duration {
        self.saturating_duration_since(earlier)
    }

    pub fn saturating_duration_since(self, earlier: Self) -> Duration {
        Duration::from_millis(self.0.saturating_sub(earlier.0))
    }
}

impl Sub<Duration> for Instant {
    type Output = Self;
    fn sub(self, delta: Duration) -> Self {
        Self(self.0.saturating_sub(delta.as_millis() as u64))
    }
}

pub fn set_elapsed(ms: u32) {
    NOW.set(Instant(60_000 + u64::from(ms)));
}

impl std::ops::Add<Duration> for Instant {
    type Output = Self;
    fn add(self, delta: Duration) -> Self {
        Self(self.0.saturating_add(delta.as_millis() as u64))
    }
}
