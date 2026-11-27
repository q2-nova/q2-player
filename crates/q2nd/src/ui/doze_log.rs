//! Per-cycle doze telemetry on the device, to show whether the doze policy
//! needs adjustment.
//!
//! One line per doze cycle, numbers and fixed words only (no song IDs,
//! titles or network names), appended to a bounded file in internal state
//! next to the library database, so it survives reboots and a rig can read
//! it. A cycle runs from the radio going down to the next doze (or the end
//! of playback): how much cached audio it started with, how long it dozed,
//! why it woke, how long the reconnect and the refill took, the least cached
//! audio while awake, and how much the next doze started with.
//!
//! The cycle itself is the pure machine `q2_machines::doze_cycle` (proved
//! there; docs/state-machines.md); this file converts clock readings,
//! formats closed cycles and owns the bounded file.
use q2_machines::doze_cycle::{self as fsm, Cycle, Event, Reason};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
#[cfg(test)]
use std::time::Duration;

pub use q2_machines::doze::WakeReason;

/// A wake with no next doze within this long closes the cycle anyway.
#[cfg(test)]
const CLOSE_AFTER: Duration = Duration::from_nanos(fsm::CLOSE_AFTER_NS);

/// The cycle state machine's shell; `poll`-style calls drive it from the
/// doze tick with explicit times so it can be tested without a clock.
#[derive(Debug)]
pub struct DozeLog {
    output: Option<Arc<crate::telemetry::Log>>,
    cycle: Cycle,
    /// Wall clock and boot time when the open cycle's doze began.
    stamp: (u64, f64),
    /// The first instant seen: the machine's clock counts from here.
    origin: Option<Instant>,
    pending: Vec<String>,
}

impl DozeLog {
    pub async fn settle(&self) -> bool {
        match &self.output { Some(output) => output.settle().await, None => true }
    }

    pub fn new(path: Option<PathBuf>) -> Self {
        DozeLog { output: path.map(crate::telemetry::Log::new), cycle: Cycle::Idle, stamp: (0, 0.0), origin: None, pending: Vec::new() }
    }

    /// Nanoseconds since the first instant this log saw (0 for an earlier one).
    fn ns(&mut self, now: Instant) -> u64 {
        let origin = *self.origin.get_or_insert(now);
        u64::try_from(now.saturating_duration_since(origin).as_nanos()).unwrap_or(u64::MAX)
    }

    fn feed(&mut self, event: Event) {
        let (next, record) = fsm::transition(self.cycle, event);
        self.cycle = next;
        if let Some(r) = record {
            let opt = |v: Option<u64>| v.map_or("na".to_string(), |v| v.to_string());
            self.pending.push(format!(
                "doze v=1 epoch_ms={} boot_s={:.1} ahead_ms={} dozed_s={} wake={} wake_ahead_ms={} reconnect_ms={} refill_ms={} min_awake_ahead_ms={} ran_out={} next_ahead_ms={}",
                self.stamp.0, self.stamp.1, r.ahead_ms, r.dozed_s, r.wake.map_or("none", |(reason, _)| reason.word()),
                opt(r.wake.map(|(_, ahead)| ahead)), opt(r.reconnect_ms), opt(r.refill_ms),
                opt(r.min_awake_ahead_ms), u8::from(r.ran_out), opt(r.next_ahead_ms)));
            self.flush();
        }
    }

    /// The radio went down with `ahead_ms` of cached audio queued.
    pub fn dozed(&mut self, now: Instant, epoch_ms: u64, boot_s: f64, ahead_ms: u64) {
        let now_ns = self.ns(now);
        self.feed(Event::Dozed { now_ns, ahead_ms });
        self.stamp = (epoch_ms, boot_s);
    }

    /// The radio was asked up.
    pub fn woke(&mut self, now: Instant, reason: WakeReason, ahead_ms: u64) {
        let now_ns = self.ns(now);
        self.feed(Event::Woke { now_ns, reason: Reason::of(reason), ahead_ms });
    }

    /// Each doze tick while awake after a doze: the link, the fetch-ahead
    /// and the cached audio now. Closes a cycle that has stayed awake long.
    pub fn awake(&mut self, now: Instant, connected: bool, fetching: bool, ahead_ms: u64, playing: bool) {
        if self.cycle == Cycle::Idle { return; }
        let now_ns = self.ns(now);
        self.feed(Event::Sample { now_ns, connected, fetching, ahead_ms, playing });
    }

    /// Playback ended or doze was turned off: close any open cycle.
    pub fn finish(&mut self, now: Instant) {
        if self.cycle == Cycle::Idle { return; }
        let now_ns = self.ns(now);
        self.feed(Event::Finish { now_ns });
    }

    fn flush(&mut self) {
        // No file (tests): keep the newest lines in memory.
        let Some(output) = &self.output else { let n = self.pending.len(); if n > 16 { self.pending.drain(..n - 16); } return };
        for line in self.pending.drain(..) { output.record(|_| line); }
    }

    #[cfg(test)]
    fn lines(&self) -> &[String] { &self.pending }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cycle_records_doze_wake_reconnect_refill_and_the_next_doze() {
        let t0 = Instant::now();
        let s = |n: u64| t0 + Duration::from_secs(n);
        let mut log = DozeLog::new(None);
        log.dozed(s(0), 1_000, 50.0, 600_000);
        log.awake(s(10), true, false, 590_000, true); // not woken yet: ignored
        log.woke(s(420), WakeReason::Low, 179_000);
        log.awake(s(424), false, false, 175_000, true);
        log.awake(s(428), true, false, 171_000, true);
        log.awake(s(430), true, true, 169_000, true);
        log.awake(s(445), true, false, 650_000, true);
        log.dozed(s(460), 461_000, 510.0, 640_000);
        log.finish(s(900));
        let l = log.lines();
        assert_eq!(l.len(), 2);
        assert_eq!(l[0], "doze v=1 epoch_ms=1000 boot_s=50.0 ahead_ms=600000 dozed_s=420 wake=low wake_ahead_ms=179000 reconnect_ms=8000 refill_ms=25000 min_awake_ahead_ms=169000 ran_out=0 next_ahead_ms=640000");
        assert!(l[1].starts_with("doze v=1 epoch_ms=461000 boot_s=510.0 ahead_ms=640000 dozed_s=440 wake=none "), "{}", l[1]);
    }

    #[test]
    fn running_out_of_cached_audio_and_a_long_wake_are_recorded() {
        let t0 = Instant::now();
        let mut log = DozeLog::new(None);
        log.dozed(t0, 1, 1.0, 420_000);
        log.woke(t0 + Duration::from_secs(300), WakeReason::Screen, 120_000);
        log.awake(t0 + Duration::from_secs(400), true, false, 0, true);
        log.awake(t0 + Duration::from_secs(300) + CLOSE_AFTER, true, false, 0, true);
        let l = log.lines();
        assert_eq!(l.len(), 1);
        assert!(l[0].contains(" wake=screen ") && l[0].contains(" ran_out=1 ") && l[0].ends_with(" next_ahead_ms=na"), "{}", l[0]);
    }

    #[test]
    fn the_file_stays_bounded_and_keeps_whole_newest_lines() {
        let dir = std::env::var_os("Q2_GENERATED_TEST_ROOT").map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build/perf/tests"))
            .join(format!("doze-log-{}-{}", std::process::id(), fastrand::u64(..)));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("doze-cycles.log");
        let line = "doze v=1 ".to_string() + &"x".repeat(200);
        let output = crate::telemetry::Log::new(path.clone());
        for _ in 0..20 {
            for _ in 0..100 { output.record(|_| line.clone()); }
            assert!(output.wait_idle());
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.len() as u64 <= crate::telemetry::MAX_BYTES + 100 * (line.len() as u64 + 1));
        assert!(text.lines().all(|l| l == line), "only whole lines remain");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
