//! Anonymous transfer/refill facts, independent of optional playback tracing.
//! Durations are server metadata, not decoded or listened audio measurements.
use crate::telemetry::Log;
use std::path::PathBuf;
use std::sync::Arc;

pub(super) enum Event {
    Request { burst: u64, duration_ms: u64, playback: bool },
    /// kind: 1 new network transfer, 2 join, 3 hit, 4 not configured,
    /// 5 offline, 6 authentication refusal, 7 other preparation failure.
    Resolved { burst: u64, request: u64, transfer: u64, kind: u64 },
    /// outcome: 1 published, 2 rename failed, 3 index failed, 4 cancelled,
    /// 5 transport/length failure, 6 offline, 7 authentication refusal.
    /// Only 1 adds new cached metadata duration.
    Finished { burst: u64, transfer: u64, duration_ms: u64, bytes: u64, outcome: u64 },
    BurstBegin { targets: usize },
    /// reason: 1 exhausted targets, 2 replaced, 3 offline. Exhausted targets
    /// does not imply that every attempted transfer succeeded.
    BurstEnd { burst: u64, reason: u64 },
}

pub(super) struct Journal {
    session: u64,
    pub(super) output: Arc<Log>,
}

impl Journal {
    pub fn new(path: PathBuf) -> Self {
        Self { session: fastrand::u64(1..), output: Log::new(path) }
    }

    pub fn record(&self, event: Event) -> u64 {
        let (kind, burst, request, transfer, duration_ms, bytes, value) = match event {
            Event::Request { burst, duration_ms, playback } => (1, burst, 0, 0, duration_ms, 0, if playback { 1 } else { 2 }),
            Event::Resolved { burst, request, transfer, kind } => (2, burst, request, transfer, 0, 0, kind),
            Event::Finished { burst, transfer, duration_ms, bytes, outcome } => (3, burst, 0, transfer, duration_ms, bytes, outcome),
            Event::BurstBegin { targets } => (4, 0, 0, 0, 0, 0, targets as u64),
            Event::BurstEnd { burst, reason } => (5, burst, 0, 0, 0, 0, reason),
        };
        self.output.record(|sequence| format!("refill 1 {} {sequence} {} {kind} {burst} {request} {transfer} {duration_ms} {bytes} {value}", self.session, crate::playtrace::boottime_us()))
    }
}
