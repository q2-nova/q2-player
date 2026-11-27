//! Executor stall detector: the longest single poll of any task spawned
//! through [`super::Runtime::spawn`]. A task that blocks (a sleep, a
//! synchronous read, long CPU work) holds every other task of its runtime for
//! the length of that poll, and the poll's length is what this records. It
//! adds two clock reads per poll and no timer, so an idle runtime stays idle.
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

/// A poll longer than this is logged (once per new maximum).
const WARN: Duration = Duration::from_millis(50);

pub struct Stall {
    name: &'static str,
    /// Microseconds, saturating (71 minutes).
    longest_us: AtomicU32,
    polls: AtomicU32,
}

impl Stall {
    pub(super) fn new(name: &'static str) -> Arc<Stall> {
        Arc::new(Stall { name, longest_us: AtomicU32::new(0), polls: AtomicU32::new(0) })
    }

    fn record(&self, elapsed: Duration, site: &'static std::panic::Location<'static>) {
        let us = u32::try_from(elapsed.as_micros()).unwrap_or(u32::MAX);
        self.polls.fetch_add(1, Ordering::Relaxed);
        let previous = self.longest_us.fetch_max(us, Ordering::Relaxed);
        if us > previous && elapsed >= WARN {
            log::warn!("runtime {}: a task held the executor for {} ms at {}", self.name, elapsed.as_millis(), site);
        }
    }

    /// The longest poll since the start or the last [`take_longest`](Self::take_longest).
    pub fn longest(&self) -> Duration {
        Duration::from_micros(self.longest_us.load(Ordering::Relaxed) as u64)
    }

    /// Returns the longest poll and starts a new observation window.
    #[cfg(test)]
    pub fn take_longest(&self) -> Duration {
        Duration::from_micros(self.longest_us.swap(0, Ordering::Relaxed) as u64)
    }

    /// Polls observed (wraps).
    pub fn polls(&self) -> u32 {
        self.polls.load(Ordering::Relaxed)
    }
}

/// Times every poll of `future`.
pub(super) struct Timed<F> {
    stall: Arc<Stall>,
    future: Pin<Box<F>>,
    task: u32,
    site: &'static std::panic::Location<'static>,
}

impl<F> Timed<F> {
    #[track_caller]
    pub(super) fn new(stall: Arc<Stall>, future: F, task: u32) -> Timed<F> {
        Timed { stall, future: Box::pin(future), task, site: std::panic::Location::caller() }
    }
}

impl<F: Future> Future for Timed<F> {
    type Output = F::Output;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let _capture = crate::diagnostics::PollScope::enter(self.task);
        let started = Instant::now();
        let result = self.future.as_mut().poll(cx);
        self.stall.record(started.elapsed(), self.site);
        result
    }
}
