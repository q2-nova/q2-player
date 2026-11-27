//! One doze cycle as the device's telemetry records it (`ui/doze_log.rs`):
//! from the radio going down to the next doze or the end of playback.
//!
//! Times are nanoseconds on a monotonic clock with an arbitrary origin; the
//! shell converts `Instant`s. Differences saturate, so a clock that steps
//! back yields zero rather than a wrapped value.
//!
//! # Invariants
//!
//! * A record is emitted exactly when an open cycle closes, and describes
//!   that cycle; at most one cycle is open.
//! * `reconnect_ms` is measured once per cycle. `refill_ms` measures the end
//!   of the latest observed fetch burst; another burst reopens it until its
//!   completion. Idle samples do not move it. `min_awake_ahead_ms` never increases.
//! * `ran_out` exactly when the least cached audio while awake was zero.
//! * A woken cycle is closed by the first `Awake` sample at or beyond
//!   [`CLOSE_AFTER_NS`] after the wake.

pub use crate::doze::WakeReason;

/// A wake with no next doze within this long closes the cycle anyway.
pub const CLOSE_AFTER_NS: u64 = 30 * 60 * 1_000_000_000;

/// What is known about a cycle after its wake.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub struct Woken {
    /// When the radio was asked up.
    pub at_ns: u64,
    /// Cached audio ahead at the wake.
    pub ahead_ms: u64,
    /// Wake to the link being back.
    pub reconnect_ms: Option<u64>,
    /// Wake to the end of the latest observed fetch-ahead burst; unknown
    /// while a subsequent burst is still running, or none was observed.
    pub refill_ms: Option<u64>,
    /// A fetch-ahead burst was seen since the wake.
    pub refill_seen: bool,
    /// Least cached audio ahead while awake and playing.
    pub min_awake_ahead_ms: Option<u64>,
}

/// The cycle's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum Cycle {
    /// No cycle is open.
    Idle,
    /// The radio is down.
    Dozing {
        /// When it went down.
        at_ns: u64,
        /// Cached audio ahead then.
        ahead_ms: u64,
    },
    /// The radio was asked up; the cycle stays open to measure the refill.
    Awake {
        /// When the radio went down.
        dozed_ns: u64,
        /// Cached audio ahead then.
        ahead_ms: u64,
        /// Why it woke. Not symbolic state: carried through unchanged.
        reason: Reason,
        /// The measurements since.
        woken: Woken,
    },
}

/// [`WakeReason`] as carried in the cycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub struct Reason(pub u8);

impl Reason {
    /// Encodes a reason.
    pub fn of(reason: WakeReason) -> Self {
        Reason(match reason {
            WakeReason::Low => 0, WakeReason::Screen => 1, WakeReason::AirPlay => 2,
            WakeReason::Stopped => 3, WakeReason::Disabled => 4, WakeReason::WifiOff => 5,
        })
    }
    /// The fixed word written to the log.
    pub fn word(self) -> &'static str {
        match self.0 { 0 => "low", 1 => "screen", 2 => "airplay", 3 => "stopped", 4 => "disabled", _ => "wifi-off" }
    }
}

/// What the doze tick reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum Event {
    /// The radio went down with `ahead_ms` cached.
    Dozed {
        /// Now.
        now_ns: u64,
        /// Cached audio ahead.
        ahead_ms: u64,
    },
    /// The radio was asked up.
    Woke {
        /// Now.
        now_ns: u64,
        /// Why.
        reason: Reason,
        /// Cached audio ahead.
        ahead_ms: u64,
    },
    /// A tick while the radio is not dozing.
    Sample {
        /// Now.
        now_ns: u64,
        /// The link is associated with an address.
        connected: bool,
        /// A fetch-ahead burst is running.
        fetching: bool,
        /// Cached audio ahead.
        ahead_ms: u64,
        /// Playing or buffering.
        playing: bool,
    },
    /// Playback ended or doze was turned off.
    Finish {
        /// Now.
        now_ns: u64,
    },
}

/// A closed cycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Record {
    /// Cached audio ahead when the radio went down.
    pub ahead_ms: u64,
    /// Whole seconds dozed.
    pub dozed_s: u64,
    /// The wake's reason and the cached audio ahead then; `None` when the
    /// cycle closed while still dozing.
    pub wake: Option<(Reason, u64)>,
    /// Wake to link, ms.
    pub reconnect_ms: Option<u64>,
    /// Wake to the end of the refill, ms.
    pub refill_ms: Option<u64>,
    /// Least cached audio while awake and playing.
    pub min_awake_ahead_ms: Option<u64>,
    /// The cached audio ran out while awake.
    pub ran_out: bool,
    /// Cached audio ahead at the next doze, when one closed this cycle.
    pub next_ahead_ms: Option<u64>,
}

fn close(cycle: Cycle, now_ns: u64, next_ahead_ms: Option<u64>) -> Option<Record> {
    match cycle {
        Cycle::Idle => None,
        Cycle::Dozing { at_ns, ahead_ms } => Some(Record {
            ahead_ms, dozed_s: now_ns.saturating_sub(at_ns) / 1_000_000_000, wake: None, reconnect_ms: None,
            refill_ms: None, min_awake_ahead_ms: None, ran_out: false, next_ahead_ms,
        }),
        Cycle::Awake { dozed_ns, ahead_ms, reason, woken } => Some(Record {
            ahead_ms, dozed_s: woken.at_ns.saturating_sub(dozed_ns) / 1_000_000_000, wake: Some((reason, woken.ahead_ms)),
            reconnect_ms: woken.reconnect_ms, refill_ms: woken.refill_ms, min_awake_ahead_ms: woken.min_awake_ahead_ms,
            ran_out: woken.min_awake_ahead_ms == Some(0), next_ahead_ms,
        }),
    }
}

/// The cycle's response to `event`: the next state and the record of a
/// cycle that closed.
pub fn transition(cycle: Cycle, event: Event) -> (Cycle, Option<Record>) {
    match event {
        Event::Dozed { now_ns, ahead_ms } =>
            (Cycle::Dozing { at_ns: now_ns, ahead_ms }, close(cycle, now_ns, Some(ahead_ms))),
        Event::Woke { now_ns, reason, ahead_ms } => match cycle {
            Cycle::Dozing { at_ns, ahead_ms: dozed_ahead } => (Cycle::Awake {
                dozed_ns: at_ns, ahead_ms: dozed_ahead, reason,
                woken: Woken { at_ns: now_ns, ahead_ms, reconnect_ms: None, refill_ms: None, refill_seen: false, min_awake_ahead_ms: None },
            }, None),
            _ => (cycle, None),
        },
        Event::Sample { now_ns, connected, fetching, ahead_ms, playing } => match cycle {
            Cycle::Awake { dozed_ns, ahead_ms: dozed_ahead, reason, woken: w } => {
                let since_ns = now_ns.saturating_sub(w.at_ns);
                let since_ms = since_ns / 1_000_000;
                let refill_seen = w.refill_seen || fetching;
                let woken = Woken {
                    reconnect_ms: w.reconnect_ms.or(connected.then_some(since_ms)),
                    // A superseded queue can leave an idle gap before the
                    // next burst. That gap is not the end of the whole refill.
                    refill_ms: if fetching { None } else { w.refill_ms.or(refill_seen.then_some(since_ms)) },
                    refill_seen,
                    min_awake_ahead_ms: if playing { Some(w.min_awake_ahead_ms.map_or(ahead_ms, |m| m.min(ahead_ms))) } else { w.min_awake_ahead_ms },
                    ..w
                };
                let next = Cycle::Awake { dozed_ns, ahead_ms: dozed_ahead, reason, woken };
                if since_ns >= CLOSE_AFTER_NS { (Cycle::Idle, close(next, now_ns, None)) } else { (next, None) }
            }
            _ => (cycle, None),
        },
        Event::Finish { now_ns } => (Cycle::Idle, close(cycle, now_ns, None)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: u64 = 1_000_000_000;

    #[test]
    fn a_single_burst_cycle_keeps_completed_measurements_and_closes_into_the_next_doze() {
        let mut c = Cycle::Idle;
        let mut records = Vec::new();
        let mut feed = |c: &mut Cycle, e: Event| { let (n, r) = transition(*c, e); *c = n; records.extend(r); };
        feed(&mut c, Event::Dozed { now_ns: 0, ahead_ms: 600_000 });
        feed(&mut c, Event::Sample { now_ns: 10 * S, connected: true, fetching: false, ahead_ms: 590_000, playing: true });
        feed(&mut c, Event::Woke { now_ns: 420 * S, reason: Reason::of(WakeReason::Low), ahead_ms: 179_000 });
        feed(&mut c, Event::Woke { now_ns: 421 * S, reason: Reason::of(WakeReason::Screen), ahead_ms: 1 });
        feed(&mut c, Event::Sample { now_ns: 424 * S, connected: false, fetching: false, ahead_ms: 175_000, playing: true });
        feed(&mut c, Event::Sample { now_ns: 428 * S, connected: true, fetching: false, ahead_ms: 171_000, playing: true });
        feed(&mut c, Event::Sample { now_ns: 430 * S, connected: true, fetching: true, ahead_ms: 169_000, playing: true });
        feed(&mut c, Event::Sample { now_ns: 445 * S, connected: true, fetching: false, ahead_ms: 650_000, playing: true });
        feed(&mut c, Event::Sample { now_ns: 450 * S, connected: true, fetching: false, ahead_ms: 640_000, playing: false });
        feed(&mut c, Event::Dozed { now_ns: 460 * S, ahead_ms: 640_000 });
        feed(&mut c, Event::Finish { now_ns: 900 * S });
        feed(&mut c, Event::Finish { now_ns: 901 * S });
        assert_eq!(records, [
            Record { ahead_ms: 600_000, dozed_s: 420, wake: Some((Reason(0), 179_000)), reconnect_ms: Some(8_000), refill_ms: Some(25_000),
                min_awake_ahead_ms: Some(169_000), ran_out: false, next_ahead_ms: Some(640_000) },
            Record { ahead_ms: 640_000, dozed_s: 440, wake: None, reconnect_ms: None, refill_ms: None, min_awake_ahead_ms: None, ran_out: false, next_ahead_ms: None },
        ]);
        assert_eq!(c, Cycle::Idle);
    }

    #[test]
    fn a_later_fetch_reopens_refill_measurement_until_its_own_completion() {
        for close_during_fetch in [false, true] {
            let (c, _) = transition(Cycle::Idle, Event::Dozed { now_ns: 0, ahead_ms: 700_000 });
            let (mut c, _) = transition(c, Event::Woke { now_ns: 500 * S, reason: Reason(0), ahead_ms: 175_000 });
            for (second, fetching) in [(532, true), (534, false), (540, false), (542, true)] {
                (c, _) = transition(c, Event::Sample { now_ns: second * S, connected: true, fetching, ahead_ms: 140_000, playing: true });
            }
            if !close_during_fetch {
                for second in [551, 560] {
                    (c, _) = transition(c, Event::Sample { now_ns: second * S, connected: true, fetching: false, ahead_ms: 570_000, playing: true });
                }
            }
            let (_, record) = transition(c, Event::Finish { now_ns: 561 * S });
            let record = record.unwrap();
            assert_eq!(record.refill_ms, if close_during_fetch { None } else { Some(51_000) });
            assert_eq!(record.reconnect_ms, Some(32_000));
            assert_eq!(record.min_awake_ahead_ms, Some(140_000));
        }
    }

    #[test]
    fn a_long_wake_closes_the_cycle_and_running_out_is_recorded() {
        let (c, _) = transition(Cycle::Idle, Event::Dozed { now_ns: 5, ahead_ms: 420_000 });
        let (c, _) = transition(c, Event::Woke { now_ns: 300 * S, reason: Reason::of(WakeReason::Screen), ahead_ms: 120_000 });
        let (c, r) = transition(c, Event::Sample { now_ns: 400 * S, connected: true, fetching: false, ahead_ms: 0, playing: true });
        assert!(r.is_none());
        let (c, r) = transition(c, Event::Sample { now_ns: 300 * S + CLOSE_AFTER_NS, connected: true, fetching: false, ahead_ms: 9, playing: true });
        let r = r.unwrap();
        assert_eq!((c, r.ran_out, r.wake.unwrap().0.word(), r.next_ahead_ms, r.reconnect_ms), (Cycle::Idle, true, "screen", None, Some(100_000)));
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// Unbounded: all `u64` times and amounts.
    #[kani::proof]
    fn a_record_is_emitted_exactly_when_an_open_cycle_closes() {
        let cycle: Cycle = kani::any();
        let event: Event = kani::any();
        let (next, record) = transition(cycle, event);
        let was_open = cycle != Cycle::Idle;
        match event {
            Event::Dozed { ahead_ms, .. } => {
                assert!(record.is_some() == was_open);
                assert!(matches!(next, Cycle::Dozing { ahead_ms: a, .. } if a == ahead_ms));
                if let Some(r) = record { assert!(r.next_ahead_ms == Some(ahead_ms)); }
            }
            Event::Finish { .. } => assert!(record.is_some() == was_open && next == Cycle::Idle),
            Event::Woke { .. } => assert!(record.is_none()),
            Event::Sample { .. } => if record.is_some() { assert!(matches!(cycle, Cycle::Awake { .. }) && next == Cycle::Idle); },
        }
        if let Some(r) = record { assert!(r.ran_out == (r.min_awake_ahead_ms == Some(0))); }
    }

    #[kani::proof]
    fn reconnect_is_taken_once_refill_reopens_only_on_fetch_and_the_minimum_never_rises() {
        let cycle: Cycle = kani::any();
        let event: Event = kani::any();
        kani::assume(matches!(event, Event::Sample { .. } | Event::Woke { .. }));
        let (next, _) = transition(cycle, event);
        if let (Cycle::Awake { woken: before, .. }, Cycle::Awake { woken: after, .. }) = (cycle, next) {
            if before.reconnect_ms.is_some() { assert!(after.reconnect_ms == before.reconnect_ms); }
            if matches!(event, Event::Sample { fetching: true, .. }) {
                assert!(after.refill_ms.is_none());
            } else if before.refill_ms.is_some() { assert!(after.refill_ms == before.refill_ms); }
            if let (Some(b), Some(a)) = (before.min_awake_ahead_ms, after.min_awake_ahead_ms) { assert!(a <= b); }
            if before.min_awake_ahead_ms.is_some() { assert!(after.min_awake_ahead_ms.is_some()); }
            assert!(after.at_ns == before.at_ns && after.ahead_ms == before.ahead_ms);
        }
    }

    #[kani::proof]
    fn a_woken_cycle_does_not_stay_open_past_the_close_deadline() {
        let cycle: Cycle = kani::any();
        let now_ns: u64 = kani::any();
        if let Cycle::Awake { woken, .. } = cycle {
            let (next, record) = transition(cycle, Event::Sample { now_ns, connected: kani::any(), fetching: kani::any(), ahead_ms: kani::any(), playing: kani::any() });
            let late = now_ns.saturating_sub(woken.at_ns) >= CLOSE_AFTER_NS;
            assert!((next == Cycle::Idle) == late && record.is_some() == late);
        }
    }
}
