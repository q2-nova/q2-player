#![forbid(unsafe_code)]
//! When to hand the next media packet to the host ([`Pacer`]), and the LDAC
//! bit rate for the room the link has ([`Abr`]). Pure: no clock, no I/O.
//!
//! The headphones play at the stream's rate from a buffer they fill before
//! they start. Every millisecond the source runs behind its schedule takes a
//! millisecond out of that buffer; at zero the listener hears a gap. The
//! pacer therefore sends by the clock, catches up after a stall (bounded by
//! what the controller takes), and keeps a small lead so that a short stall
//! is absorbed. The ABR lowers LDAC's bit rate when packets pile up between
//! the encoder and the air, and raises it again after a quiet spell.

/// The lead over real time the pacer builds up, in microseconds.
pub const AHEAD_US: u64 = 60_000;
/// The lead grows by one part in this many of elapsed stream time (5 %), so the
/// headphones' buffer fills slowly rather than in a burst at the start.
const AHEAD_RAMP: u64 = 20;
/// Behind schedule by more than this, the headphones' buffer has run out:
/// the schedule restarts at the present instead of sending the gap in a burst.
pub const CATCH_UP_US: u64 = 300_000;
/// A packet handed over this late counts in [`Pacer::late`].
pub const LATE_US: u64 = 40_000;
/// A stall this long is reported once, with its length ([`Pacer::take_stall`]).
pub const STALL_US: u64 = 100_000;

#[derive(Debug, Default)]
pub struct Pacer {
    rate: u32,
    /// When the first frame was due (the schedule's origin), in microseconds.
    anchor_us: Option<u64>,
    /// PCM frames handed to the host since the anchor.
    sent: u64,
    /// Packets handed over more than [`LATE_US`] behind schedule.
    pub late: u32,
    /// Schedule restarts after falling more than [`CATCH_UP_US`] behind.
    pub restarts: u32,
    /// The most the pacer has been behind schedule, in microseconds.
    pub behind_max_us: u64,
    /// The stall in progress: its worst lateness so far.
    stall_us: Option<u64>,
    /// Stalls of [`STALL_US`] or more that ended: worst lateness (µs) and whether the schedule restarted.
    stalls: Vec<(u64, bool)>,
}

impl Pacer {
    pub fn new(rate: u32) -> Self { Self { rate: rate.max(1), ..Self::default() } }

    /// Audio stopped (pause, stop): the next packet starts a new schedule.
    pub fn reset(&mut self) { self.anchor_us = None; self.sent = 0; self.close_stall(false); }

    fn frames_us(&self, frames: u64) -> u64 { frames * 1_000_000 / u64::from(self.rate) }
    /// The lead wanted at `now`: up to [`AHEAD_US`], 5 % of elapsed stream time.
    fn ahead_us(&self, now_us: u64) -> u64 {
        self.anchor_us.map_or(0, |anchor| (now_us.saturating_sub(anchor) / AHEAD_RAMP).min(AHEAD_US))
    }
    /// When the next frame to send is due, lead included.
    fn due_us(&self, now_us: u64) -> u64 {
        match self.anchor_us { Some(anchor) => (anchor + self.frames_us(self.sent)).saturating_sub(self.ahead_us(now_us)), None => now_us }
    }
    /// How far the next frame is behind its time without the lead (negative: ahead), in microseconds.
    pub fn behind_us(&self, now_us: u64) -> i64 {
        match self.anchor_us { Some(anchor) => now_us as i64 - (anchor + self.frames_us(self.sent)) as i64, None => 0 }
    }

    /// Whether the next chunk should be handed over now. Restarts the
    /// schedule when it is too far behind (after a long stall).
    pub fn due(&mut self, now_us: u64) -> bool {
        self.anchor_us.get_or_insert(now_us);
        let behind = self.behind_us(now_us);
        if behind > CATCH_UP_US as i64 {
            self.restarts += 1;
            self.note_behind(behind as u64);
            self.close_stall(true);
            // The next frame is due now.
            self.anchor_us = Some(now_us.saturating_sub(self.frames_us(self.sent)));
            return true;
        }
        now_us >= self.due_us(now_us)
    }

    /// `frames` PCM frames were handed to the host at `now_us`.
    pub fn sent(&mut self, frames: u64, now_us: u64) {
        self.anchor_us.get_or_insert(now_us);
        let behind = self.behind_us(now_us);
        if behind > LATE_US as i64 { self.late += 1; }
        if behind > 0 { self.note_behind(behind as u64); }
        if behind < LATE_US as i64 { self.close_stall(false); }
        self.sent += frames;
    }

    fn note_behind(&mut self, behind: u64) {
        self.behind_max_us = self.behind_max_us.max(behind);
        if behind >= STALL_US { self.stall_us = Some(self.stall_us.unwrap_or(0).max(behind)); }
    }
    fn close_stall(&mut self, restarted: bool) {
        if let Some(worst) = self.stall_us.take() { if self.stalls.len() < 16 { self.stalls.push((worst, restarted)); } }
    }
    /// A stall that ended since the last call: its worst lateness in µs, and whether the schedule restarted.
    pub fn take_stall(&mut self) -> Option<(u64, bool)> { (!self.stalls.is_empty()).then(|| self.stalls.remove(0)) }

    /// When to look again: the next chunk's due time.
    pub fn next_us(&self, now_us: u64) -> u64 { self.due_us(now_us).max(now_us + 1000) }
}

/// Packets queued between the encoder and the air at or below which the link has room.
pub const SAFE_PACKETS: u32 = 3;
/// At or above this and growing, one level down.
pub const DANGER_PACKETS: u32 = 8;
/// At or above this for [`CRITICAL_MS`], straight to the lowest level. A single
/// H5 resend (120 ms) fills the queue for less than that and drains on its own.
pub const CRITICAL_PACKETS: u32 = 14;
const CRITICAL_MS: u64 = 200;
/// After a change the queue needs this long to show its effect.
const SETTLE_MS: u64 = 400;
/// The quiet spell before a step up; doubled when a step up is soon followed by a step down.
const HOLD_MS: u64 = 5_000;
const HOLD_MAX_MS: u64 = 60_000;
/// A step down this soon after a step up doubles the hold.
const BOUNCE_MS: u64 = 30_000;
/// No step down for this long resets the hold.
const CALM_MS: u64 = 120_000;
/// The growth test compares with the depth this long ago.
const TREND_MS: u64 = 200;

/// LDAC adaptive bit rate: levels of `q2_ldac::LEVELS`, 0 the highest rate.
#[derive(Debug)]
pub struct Abr {
    pub level: u8,
    /// The best level allowed (the `ldac_quality` setting) and the lowest.
    ceiling: u8,
    floor: u8,
    changed_at: Option<u64>,
    safe_since: Option<u64>,
    critical_since: Option<u64>,
    hold_ms: u64,
    up_at: Option<u64>,
    down_at: Option<u64>,
    /// Recent (ms, depth) samples for the growth test.
    trend: std::collections::VecDeque<(u64, u32)>,
    /// Level changes made.
    pub changes: u32,
}

impl Abr {
    /// Starts at `ceiling`; never goes above it or below `floor`.
    pub fn new(ceiling: u8, floor: u8) -> Self {
        Self { level: ceiling, ceiling, floor: floor.max(ceiling), changed_at: None, safe_since: None, critical_since: None, hold_ms: HOLD_MS, up_at: None, down_at: None,
            trend: Default::default(), changes: 0 }
    }

    /// One observation of the queue depth in packets (queued in the host, in H5,
    /// in the controller, plus the schedule's lateness in packets). Returns the
    /// new level when it changes.
    pub fn observe(&mut self, now_ms: u64, depth: u32) -> Option<u8> {
        while self.trend.front().is_some_and(|&(t, _)| now_ms.saturating_sub(t) > TREND_MS) { self.trend.pop_front(); }
        let earlier = self.trend.front().map(|&(_, d)| d);
        self.trend.push_back((now_ms, depth));
        if self.down_at.is_some_and(|t| now_ms.saturating_sub(t) >= CALM_MS) { self.hold_ms = HOLD_MS; }
        if depth > SAFE_PACKETS { self.safe_since = None; }
        let critical = if depth >= CRITICAL_PACKETS { now_ms.saturating_sub(*self.critical_since.get_or_insert(now_ms)) >= CRITICAL_MS } else { self.critical_since = None; false };
        if self.changed_at.is_some_and(|t| now_ms.saturating_sub(t) < SETTLE_MS) { return None }
        let target = if critical {
            self.floor
        } else if depth >= DANGER_PACKETS && earlier.is_some_and(|e| depth > e) {
            (self.level + 1).min(self.floor)
        } else if depth <= SAFE_PACKETS {
            let since = *self.safe_since.get_or_insert(now_ms);
            if now_ms.saturating_sub(since) >= self.hold_ms && self.level > self.ceiling { self.level - 1 } else { self.level }
        } else { self.level };
        if target == self.level { return None }
        if target > self.level {
            // Once per step up that did not last.
            if self.up_at.take().is_some_and(|t| now_ms.saturating_sub(t) < BOUNCE_MS) { self.hold_ms = (self.hold_ms * 2).min(HOLD_MAX_MS); }
            self.down_at = Some(now_ms);
        } else { self.up_at = Some(now_ms); }
        self.level = target;
        self.changed_at = Some(now_ms);
        self.safe_since = None;
        self.trend.clear();
        self.changes += 1;
        Some(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Sends 128-frame chunks at 44.1 kHz as often as `room` allows; returns µs behind at each send.
    fn drive(pacer: &mut Pacer, until_us: u64, mut room: impl FnMut(u64) -> bool) -> Vec<(u64, i64)> {
        let mut log = Vec::new();
        let mut now = 0;
        while now < until_us {
            while room(now) && pacer.due(now) { log.push((now, pacer.behind_us(now))); pacer.sent(128, now); }
            now += 1000;
        }
        log
    }

    #[test]
    fn sends_by_the_clock_with_a_lead_that_builds_slowly_to_its_target() {
        let mut pacer = Pacer::new(44_100);
        let log = drive(&mut pacer, 3_000_000, |_| true);
        // 3 s of audio plus the 40 ms lead, in whole chunks.
        let sent = log.len() as u64 * 128;
        let expected = (3_000_000 + AHEAD_US) * 44_100 / 1_000_000;
        assert!(sent.abs_diff(expected) <= 2 * 128, "{sent} vs {expected}");
        // After 0.4 s the lead is 20 ms, not 60: the ramp is 5 %.
        let at = |t: u64| log.iter().take_while(|(now, _)| *now <= t).last().unwrap().1;
        assert!((-21_000..=-17_000).contains(&at(400_000)), "{}", at(400_000));
        assert!((-61_000..=-57_000).contains(&at(2_000_000)), "{}", at(2_000_000));
        assert_eq!((pacer.late, pacer.restarts), (0, 0));
    }

    #[test]
    fn a_stall_is_caught_up_and_reported_and_a_long_one_restarts_the_schedule() {
        let mut pacer = Pacer::new(44_100);
        // Nothing is taken from 1.0 to 1.2 s, then from 2.0 to 2.5 s.
        let log = drive(&mut pacer, 4_000_000, |now| !(1_000_000..1_200_000).contains(&now) && !(2_000_000..2_500_000).contains(&now));
        // Lateness of the first packet sent at or after `t`.
        let behind = |t: u64| log.iter().find(|(now, _)| *now >= t).unwrap().1;
        // Right after the first stall the pacer is 150 ms late (200 ms less the 50 ms lead at 1 s) and catches up at once.
        assert!((140_000..=160_000).contains(&behind(1_200_000)), "{}", behind(1_200_000));
        assert!(behind(1_201_000) < 0, "caught up in a millisecond: {}", behind(1_201_000));
        // The second is longer than the catch-up limit: the schedule restarts at the present.
        assert_eq!(pacer.restarts, 1);
        assert!(behind(2_550_000) <= 0);
        let first = pacer.take_stall().unwrap();
        assert!((140_000..=160_000).contains(&first.0) && !first.1, "{first:?}");
        let second = pacer.take_stall().unwrap();
        assert!(second.0 > CATCH_UP_US && second.1, "{second:?}");
        assert_eq!(pacer.take_stall(), None);
        assert!(pacer.late > 0);
    }

    #[test]
    fn abr_steps_down_on_a_growing_queue_drops_to_the_floor_when_critical_and_climbs_back_after_quiet() {
        let mut abr = Abr::new(1, 4);
        let mut changes = Vec::new();
        let mut t = 0;
        let mut feed = |abr: &mut Abr, ms: u64, depth: u32, changes: &mut Vec<(u64, u8)>| {
            for _ in 0..ms / 20 { t += 20; if let Some(level) = abr.observe(t, depth) { changes.push((t, level)); } }
        };
        feed(&mut abr, 2_000, 2, &mut changes);
        assert!(changes.is_empty(), "a quiet link at the ceiling stays there");
        // A 100 ms spike (one H5 resend) is one step at most, not the floor.
        feed(&mut abr, 100, 20, &mut changes);
        assert!(abr.level <= 2, "{changes:?}");
        feed(&mut abr, 6_000, 2, &mut changes);
        assert_eq!(abr.level, 1);
        changes.clear();
        // A queue that grows: one step down per settle time while it grows.
        for depth in 8..12 { feed(&mut abr, 400, depth, &mut changes); }
        assert!(changes.len() >= 2 && changes.iter().all(|c| c.1 > 1), "{changes:?}");
        // Critical: the floor at once.
        feed(&mut abr, 300, 20, &mut changes);
        assert_eq!(abr.level, 4);
        // Quiet again: one step up per hold back to the ceiling, never above. The hold is
        // 10 s now: the step up after the first spike was followed by a step down.
        let before = changes.len();
        feed(&mut abr, 40_000, 1, &mut changes);
        assert_eq!(abr.level, 1, "{changes:?}");
        let ups: Vec<_> = changes[before..].iter().collect();
        assert_eq!(ups.len(), 3);
        assert!(ups.windows(2).all(|w| w[1].0 - w[0].0 >= 2 * HOLD_MS), "{ups:?}");
        // A step down right after a step up doubles the wait again (to 20 s).
        feed(&mut abr, 1_000, 20, &mut changes);
        let down = changes.last().unwrap().0;
        feed(&mut abr, 15_000, 1, &mut changes);
        assert_eq!(changes.last().unwrap().0, down, "15 s of quiet is not enough after a second bounce");
        feed(&mut abr, 6_000, 1, &mut changes);
        assert!(changes.last().unwrap().0 > down);
    }
}
