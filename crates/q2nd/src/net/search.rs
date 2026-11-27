//! When to look for a saved network while Wi-Fi is on but not associated.
//!
//! Left alone, wpa_supplicant rescans every few seconds for as long as no
//! saved network is in range, which on a portable player is most of the time
//! away from home; the 8733bs build has no scheduled-scan (PNO) offload to do
//! it cheaply. Product feedback (2026-11-08): scan when the screen wakes, then at a
//! foreground and a background rate, backing off from about 30 s–1 min to
//! about 10 min with the screen on and 30 min with it off.
//!
//! Between searches the supplicant is told to `DISCONNECT`, which stops its
//! own scanning; a search is `RECONNECT`, which scans and joins any saved
//! network it finds. A search is one scan round plus the start of a join, not
//! several rescans. The backoff restarts only when an association is lost or
//! Wi-Fi comes up; a screen wake buys one search, at most every two minutes,
//! so frequent glances cannot keep the radio searching. Owner network
//! actions (join, the Wi-Fi page's scan) open a longer search so a join is
//! never parked mid-way. Pure policy: the caller sends the commands.
use std::time::{Duration, Instant};

/// First gap after a search finds nothing; doubles up to the cap.
pub const FIRST: Duration = Duration::from_secs(45);
pub const CAP_SCREEN_ON: Duration = Duration::from_secs(10 * 60);
pub const CAP_SCREEN_OFF: Duration = Duration::from_secs(30 * 60);
/// One search: a dual-band scan (2–4 s) and time for a found network's join
/// to reach AUTHENTICATING, after which `Connecting` holds the search open.
pub const WINDOW: Duration = Duration::from_secs(12);
/// After losing an association, the supplicant's own reconnect attempts run
/// this long before the first park (roaming, a brief drop).
pub const LOST_WINDOW: Duration = Duration::from_secs(60);
/// An owner join or Wi-Fi page scan: the supplicant may scan for several
/// seconds before it starts authenticating.
pub const ACTION_WINDOW: Duration = Duration::from_secs(60);
/// A screen wake searches only if the last search began at least this long ago.
pub const WAKE_SPACING: Duration = Duration::from_secs(120);

/// What the supplicant is doing, from its `wpa_state`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Link {
    Associated,
    /// Authenticating, associating or in a handshake: never park mid-join.
    Connecting,
    /// Scanning, disconnected or inactive.
    Idle,
}

impl Link {
    pub fn from_wpa_state(state: &str) -> Link {
        match state {
            "COMPLETED" => Link::Associated,
            "AUTHENTICATING" | "ASSOCIATING" | "ASSOCIATED" | "4WAY_HANDSHAKE" | "GROUP_HANDSHAKE" => Link::Connecting,
            _ => Link::Idle,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Step {
    /// Nothing to send.
    Hold,
    /// Stop the supplicant's own scanning (`DISCONNECT`).
    Park,
    /// Scan and join a saved network if one is in range (`RECONNECT`).
    Seek,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    Associated,
    /// The supplicant is searching until `until`.
    Seeking { until: Instant },
    /// Parked; the next search is due at `next`.
    Parked { next: Instant },
}

#[derive(Debug, Clone)]
pub struct Search {
    state: State,
    gap: Duration,
    screen_on: bool,
    last_seek: Instant,
}

impl Search {
    /// Wi-Fi has just come up: the supplicant's start-up scan is the first search.
    pub fn new(now: Instant, screen_on: bool) -> Self {
        Search { state: State::Seeking { until: now + WINDOW }, gap: FIRST, screen_on, last_seek: now }
    }

    fn cap(&self) -> Duration { if self.screen_on { CAP_SCREEN_ON } else { CAP_SCREEN_OFF } }

    fn seek(&mut self, now: Instant, window: Duration) -> Step {
        self.state = State::Seeking { until: now + window };
        self.last_seek = now;
        Step::Seek
    }

    /// The owner joined a network or asked for a scan: hold any search open
    /// long enough for the join, or start one if parked.
    pub fn action(&mut self, now: Instant) -> Step {
        match self.state {
            State::Associated => Step::Hold,
            State::Seeking { until } => {
                self.state = State::Seeking { until: until.max(now + ACTION_WINDOW) };
                Step::Hold
            }
            State::Parked { .. } => self.seek(now, ACTION_WINDOW),
        }
    }

    /// One observation of the supplicant and the screen. Returns the command
    /// to send now.
    pub fn step(&mut self, now: Instant, link: Link, screen_on: bool) -> Step {
        let woke = screen_on && !self.screen_on;
        self.screen_on = screen_on;
        match link {
            Link::Associated => {
                self.state = State::Associated;
                self.gap = FIRST;
                return Step::Hold;
            }
            Link::Connecting => {
                // A join in progress (the owner's, or one this search found)
                // keeps the search open for another window.
                if let State::Seeking { until } = self.state {
                    self.state = State::Seeking { until: until.max(now + WINDOW) };
                } else if matches!(self.state, State::Parked { .. }) {
                    self.state = State::Seeking { until: now + WINDOW };
                }
                return Step::Hold;
            }
            Link::Idle => {}
        }
        match self.state {
            State::Associated => {
                self.state = State::Seeking { until: now + LOST_WINDOW };
                self.last_seek = now;
                Step::Hold
            }
            State::Parked { .. } if woke && now.duration_since(self.last_seek) >= WAKE_SPACING => self.seek(now, WINDOW),
            State::Seeking { until } if now >= until => {
                let gap = self.gap.min(self.cap());
                self.state = State::Parked { next: now + gap };
                self.gap = (gap * 2).min(self.cap());
                Step::Park
            }
            State::Parked { next } if now >= next => self.seek(now, WINDOW),
            State::Parked { next } if screen_on && next > now + CAP_SCREEN_ON => {
                // Screen on during a long background gap: the foreground rate applies.
                self.state = State::Parked { next: now + CAP_SCREEN_ON };
                self.gap = self.gap.min(CAP_SCREEN_ON);
                Step::Hold
            }
            _ => Step::Hold,
        }
    }

    /// When the next `step` could change something, so the caller can sleep
    /// no longer than that.
    pub fn deadline(&self) -> Option<Instant> {
        match self.state {
            State::Associated => None,
            State::Seeking { until } => Some(until),
            State::Parked { next } => Some(next),
        }
    }

    pub fn parked(&self) -> bool { matches!(self.state, State::Parked { .. }) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(secs: u64, t0: Instant) -> Instant { t0 + Duration::from_secs(secs) }
    const W: u64 = 12;

    /// Runs searches that find nothing and returns the gaps between them.
    fn gaps(p: &mut Search, t0: Instant, mut t: u64, screen: bool, n: usize) -> (Vec<u64>, u64) {
        let mut out = vec![];
        for _ in 0..n {
            let gap = (p.deadline().unwrap() - s(t, t0)).as_secs();
            out.push(gap);
            t += gap;
            assert_eq!(p.step(s(t - 1, t0), Link::Idle, screen), Step::Hold);
            assert_eq!(p.step(s(t, t0), Link::Idle, screen), Step::Seek);
            t += W;
            assert_eq!(p.step(s(t, t0), Link::Idle, screen), Step::Park);
        }
        (out, t)
    }

    #[test]
    fn backs_off_to_the_dark_cap() {
        let t0 = Instant::now();
        let mut p = Search::new(t0, false);
        assert_eq!(p.step(s(5, t0), Link::Idle, false), Step::Hold);
        assert_eq!(p.step(s(W, t0), Link::Idle, false), Step::Park);
        let (g, _) = gaps(&mut p, t0, W, false, 8);
        assert_eq!(g, vec![45, 90, 180, 360, 720, 1440, 1800, 1800]);
    }

    #[test]
    fn screen_on_caps_at_ten_minutes_and_clamps_a_background_gap() {
        let t0 = Instant::now();
        let mut p = Search::new(t0, true);
        assert_eq!(p.step(s(W, t0), Link::Idle, true), Step::Park);
        let (g, _) = gaps(&mut p, t0, W, true, 7);
        assert_eq!(*g.last().unwrap(), 600);
        // In the dark the gap grows to 30 min; turning the screen on mid-gap
        // brings the next search within 10 min without an extra one now.
        let t0 = Instant::now();
        let mut p = Search::new(t0, false);
        assert_eq!(p.step(s(W, t0), Link::Idle, false), Step::Park);
        let (_, t) = gaps(&mut p, t0, W, false, 7);
        assert_eq!((p.deadline().unwrap() - s(t, t0)).as_secs(), 1800);
        // The wake itself searches (the last search began over 2 min ago).
        assert_eq!(p.step(s(t + 150, t0), Link::Idle, true), Step::Seek);
        assert_eq!(p.step(s(t + 150 + W, t0), Link::Idle, true), Step::Park);
        assert!((p.deadline().unwrap() - s(t + 150 + W, t0)).as_secs() <= 600);
    }

    #[test]
    fn a_wake_searches_once_and_does_not_restart_the_backoff() {
        let t0 = Instant::now();
        let mut p = Search::new(t0, false);
        assert_eq!(p.step(s(W, t0), Link::Idle, false), Step::Park);
        let (_, t) = gaps(&mut p, t0, W, false, 4); // next gap 720 s
        // Wake 200 s after the last search: one search, gap unchanged.
        assert_eq!(p.step(s(t + 200 - W, t0), Link::Idle, true), Step::Seek);
        assert_eq!(p.step(s(t + 200, t0), Link::Idle, true), Step::Park);
        assert_eq!((p.deadline().unwrap() - s(t + 200, t0)).as_secs(), 600); // screen on: clamped
        // Screen off and on again within 2 min: no search.
        assert_eq!(p.step(s(t + 230, t0), Link::Idle, false), Step::Hold);
        assert_eq!(p.step(s(t + 260, t0), Link::Idle, true), Step::Hold);
        assert!(p.parked());
    }

    #[test]
    fn association_resets_and_a_loss_gets_a_longer_first_window() {
        let t0 = Instant::now();
        let mut p = Search::new(t0, false);
        assert_eq!(p.step(s(5, t0), Link::Associated, false), Step::Hold);
        assert_eq!(p.deadline(), None);
        assert_eq!(p.step(s(100, t0), Link::Idle, false), Step::Hold);
        assert_eq!(p.step(s(159, t0), Link::Idle, false), Step::Hold);
        assert_eq!(p.step(s(160, t0), Link::Idle, false), Step::Park);
        assert_eq!((p.deadline().unwrap() - s(160, t0)).as_secs(), 45);
        assert_eq!(p.step(s(205, t0), Link::Idle, false), Step::Seek);
        assert_eq!(p.step(s(210, t0), Link::Associated, false), Step::Hold);
        assert!(!p.parked());
    }

    #[test]
    fn a_join_in_progress_is_never_parked() {
        let t0 = Instant::now();
        let mut p = Search::new(t0, false);
        assert_eq!(p.step(s(10, t0), Link::Connecting, false), Step::Hold);
        assert_eq!(p.step(s(21, t0), Link::Connecting, false), Step::Hold);
        // The handshake failed: park one window after the last progress.
        assert_eq!(p.step(s(32, t0), Link::Idle, false), Step::Hold);
        assert_eq!(p.step(s(33, t0), Link::Idle, false), Step::Park);
        assert_eq!(Link::from_wpa_state("4WAY_HANDSHAKE"), Link::Connecting);
        assert_eq!(Link::from_wpa_state("SCANNING"), Link::Idle);
        assert_eq!(Link::from_wpa_state("COMPLETED"), Link::Associated);
    }

    #[test]
    fn an_owner_join_scans_without_being_parked() {
        let t0 = Instant::now();
        let mut p = Search::new(t0, true);
        assert_eq!(p.step(s(W, t0), Link::Idle, true), Step::Park);
        // Join from the Wi-Fi page while parked: the supplicant scans
        // (SCANNING = Idle) for several seconds before authenticating.
        assert_eq!(p.action(s(20, t0)), Step::Seek);
        for t in [23, 26, 29, 50, 79] { assert_eq!(p.step(s(t, t0), Link::Idle, true), Step::Hold); }
        assert_eq!(p.step(s(80, t0), Link::Idle, true), Step::Park);
        // During a search, an action extends it without a second command.
        let next = p.deadline().unwrap();
        assert_eq!(p.step(next, Link::Idle, true), Step::Seek);
        assert_eq!(p.action(next + Duration::from_secs(5)), Step::Hold);
        assert_eq!(p.deadline(), Some(next + Duration::from_secs(65)));
    }
}
