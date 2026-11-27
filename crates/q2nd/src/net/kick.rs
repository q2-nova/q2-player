//! One early full scan after a hinted start misses (fast reassociation).
//!
//! The network helper limits the supplicant's first scan to the last
//! associated channel (`initial_freq_list`). When the AP is no longer there,
//! the supplicant reports `CTRL-EVENT-NETWORK-NOT-FOUND` and waits its 5 s
//! `scan_interval` before a full scan: address at 12.4 s instead of 3.1 s on a
//! hit. On the first
//! NOT-FOUND of a radio generation, early on and while the search policy is
//! not parked, the app asks for the full scan at once. Counted as sent only
//! when the supplicant accepts it (a `FAIL-BUSY` keeps it armed).

use std::time::{Duration, Instant};

/// How long after a generation's supplicant first appears a kick may be sent.
/// The search policy's window starts earlier (its first iteration, often
/// before `up` returns), so this alone does not keep the kick out of a park:
/// the caller also passes the policy's parked state.
pub const WINDOW: Duration = Duration::from_secs(8);
/// The helper writes this overlay only for a hinted start (it removes a stale
/// one otherwise); an unhinted first scan already covered every channel.
pub const HINT: &str = "/run/q2-network/initial.conf";
/// A NOT-FOUND counts as the hinted scan's only this soon after the overlay
/// was written (just before the supplicant started). The supplicant's own
/// full scan follows its 5 s backoff plus a scan, so it cannot arrive this
/// soon; a later generation over the same supplicant is past it too.
pub const HINT_AGE: Duration = Duration::from_secs(5);

/// The hint file's age from its modification time; None without the file.
pub fn hint_age(_blocking: &crate::rt::Blocking) -> Option<Duration> {
    std::fs::metadata(HINT).and_then(|m| m.modified()).ok()?.elapsed().ok()
}

#[derive(Default)]
pub struct Kick {
    /// The generation, when its supplicant was first seen, and whether sent.
    current: Option<(u64, Option<Instant>, bool)>,
    /// A NOT-FOUND arrived and the scan is not yet accepted (FAIL-BUSY retries).
    pending: bool,
}

impl Kick {
    /// Notes an observation; `supplicant` is false while the state is OFF.
    pub fn observe(&mut self, now: Instant, generation: u64, supplicant: bool) {
        match self.current {
            Some((g, _, _)) if g == generation => {}
            _ => { self.current = Some((generation, None, false)); self.pending = false; }
        }
        if let Some((_, first @ None, _)) = &mut self.current {
            if supplicant { *first = Some(now); }
        }
    }

    /// True when a `SCAN` should be sent now: the hinted scan's NOT-FOUND
    /// arrived in this generation's window and no scan has been accepted yet.
    /// `idle`: not associated or joining (a join in progress, or done, ends it).
    pub fn wanted(&mut self, now: Instant, generation: u64, not_found: bool, hinted: bool, idle: bool, parked: bool) -> bool {
        let Some((g, Some(first), sent)) = self.current else { return false };
        if g != generation || sent || !idle || parked || now.saturating_duration_since(first) > WINDOW {
            self.pending = false;
            return false;
        }
        if not_found && hinted { self.pending = true; }
        self.pending
    }

    /// A kick is waiting for the supplicant to accept `SCAN` (FAIL-BUSY).
    pub fn pending(&self) -> bool { self.pending }

    /// The supplicant accepted the scan.
    pub fn sent(&mut self, generation: u64) {
        if let Some((g, _, sent)) = &mut self.current {
            if *g == generation { *sent = true; self.pending = false; }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kicks_once_on_the_first_not_found_of_a_generation() {
        let mut k = Kick::default();
        let t = Instant::now();
        k.observe(t, 1, false);
        assert!(!k.wanted(t, 1, true, true, true, false), "no supplicant yet: no window");
        k.observe(t + Duration::from_millis(200), 1, true);
        assert!(!k.wanted(t + Duration::from_secs(1), 1, false, true, true, false), "only on NOT-FOUND");
        assert!(k.wanted(t + Duration::from_secs(1), 1, true, true, true, false));
        // FAIL-BUSY: not marked sent; stays pending without a second event.
        assert!(k.wanted(t + Duration::from_secs(2), 1, false, true, true, false));
        k.sent(1);
        assert!(!k.wanted(t + Duration::from_secs(3), 1, true, true, true, false), "once per generation");
    }

    #[test]
    fn never_kicks_when_parked_late_or_for_another_generation() {
        let mut k = Kick::default();
        let t = Instant::now();
        k.observe(t, 1, true);
        assert!(!k.wanted(t + Duration::from_secs(1), 1, true, false, true, false), "an unhinted first scan already covered every channel");
        assert!(!k.wanted(t + Duration::from_secs(1), 1, true, true, true, true), "the policy parked the radio");
        assert!(!k.wanted(t + Duration::from_secs(2), 1, false, true, true, false), "parking cleared the pending kick");
        assert!(k.wanted(t + Duration::from_secs(2), 1, true, true, true, false));
        assert!(!k.wanted(t + Duration::from_secs(3), 1, false, true, false, false), "a join started: the busy scan found a network");
        assert!(!k.pending());
        assert!(!k.wanted(t + WINDOW + Duration::from_secs(1), 1, true, true, true, false), "past the window");
        assert!(!k.wanted(t + Duration::from_secs(1), 2, true, true, true, false), "a different generation");
        k.observe(t + Duration::from_secs(20), 2, true);
        assert!(k.wanted(t + Duration::from_secs(21), 2, true, true, true, false), "a new generation has its own window");
    }
}
