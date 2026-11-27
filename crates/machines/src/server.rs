//! Navidrome reachability (`library/health.rs`, docs/server-status.md).
//!
//! Requests report their outcome with a priority; a separate transport input
//! says whether the link to the server can work at all; a serialized health
//! probe (Subsonic `ping`) settles doubt. The library is offline only on
//! evidence: a failed probe after a failure, [`OFFLINE_AFTER`] foreground
//! failures in a row, or a transport that is down.
//!
//! # Invariants
//!
//! * From `Online`, no single request failure reaches `Offline`.
//! * A low-priority failure (cover prefetch, album-page refresh) never
//!   changes the state.
//! * Any success, including a successful probe, reaches `Online` with no
//!   failures counted.
//! * At most one probe runs: a probe starts only when none is running.
//! * `Checking` always has a probe running, so it ends with that probe.
//! * `Offline` is entered only by a failed probe, the [`OFFLINE_AFTER`]th
//!   consecutive foreground failure, a transport going down, or a reset.

/// Foreground failures in a row, with no success between them, that go
/// offline without waiting for the probe.
pub const OFFLINE_AFTER: u8 = 3;

/// What the listener is told about the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum Status {
    /// The last evidence was a success.
    Online,
    /// A request failed, or the listener asked for a recheck; the probe
    /// decides. Requests still go to the server.
    Checking,
    /// Confirmed unreachable, or not yet reached. Requests fall back to the
    /// card; the shell probes on its offline cadence.
    Offline,
}

/// The machine's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub struct Health {
    /// What is published.
    pub status: Status,
    /// Foreground failures since the last success, at most [`OFFLINE_AFTER`].
    pub failures: u8,
    /// A probe is running.
    pub probing: bool,
    /// The transport is known down (no Wi-Fi address, Tailscale stopped).
    pub transport_down: bool,
}

impl Health {
    /// At start and after a reset: not yet reached.
    pub const INITIAL: Health = Health { status: Status::Offline, failures: 0, probing: false, transport_down: false };
}

impl Default for Health {
    fn default() -> Self { Self::INITIAL }
}

/// Who was waiting for a request that failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum Priority {
    /// The listener waits: a page with nothing cached, a song starting, an action.
    Foreground,
    /// Work the listener asked for but does not watch: downloads, fetch-ahead,
    /// queued plays and queue saves.
    Background,
    /// Speculative work: cover prefetch, album-page refresh behind a shown
    /// cached page, nearby-album warming. Never evidence.
    Low,
}

/// What the shell observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum Event {
    /// A response from the server (any priority).
    Success,
    /// A request failed by network, timeout, proxy or 5xx; never a
    /// cancellation the player made itself.
    Failure {
        /// Who was waiting.
        priority: Priority,
    },
    /// The probe ended.
    ProbeDone {
        /// The server answered `ping`.
        ok: bool,
    },
    /// The offline cadence is due: probe if nothing else is.
    ProbeDue,
    /// The listener looks at the server's status (Settings › Connections):
    /// probe unless one runs or the transport is down. An online server shows
    /// Checking until the probe ends, so a status nothing has tested since the
    /// server failed is never shown as Connected.
    Recheck,
    /// The transport changed.
    Transport {
        /// It can carry requests.
        up: bool,
    },
    /// The server or account changed: nothing is known about the new one.
    Reset,
}

/// What the shell does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Effect {
    /// Start the probe.
    pub probe: bool,
    /// The status changed to this: publish it.
    pub publish: Option<Status>,
}

const NONE: Effect = Effect { probe: false, publish: None };

/// The machine's response to `event`.
pub fn transition(state: Health, event: Event) -> (Health, Effect) {
    let mut next = state;
    let mut probe = false;
    match event {
        Event::Success => {
            next.status = Status::Online;
            next.failures = 0;
        }
        Event::ProbeDone { ok: true } => {
            next.status = Status::Online;
            next.failures = 0;
            next.probing = false;
        }
        Event::ProbeDone { ok: false } => {
            next.probing = false;
            match state.status {
                Status::Checking | Status::Offline => next.status = Status::Offline,
                // A probe started before a success ended badly: doubt again.
                Status::Online => {
                    next.status = Status::Checking;
                    next.probing = true;
                    probe = true;
                }
            }
        }
        Event::Failure { priority: Priority::Low } => {}
        Event::Failure { priority } => {
            if priority == Priority::Foreground { next.failures = state.failures.saturating_add(1).min(OFFLINE_AFTER); }
            match state.status {
                Status::Offline => {}
                Status::Online | Status::Checking if priority == Priority::Foreground && next.failures >= OFFLINE_AFTER => next.status = Status::Offline,
                Status::Online | Status::Checking => {
                    next.status = Status::Checking;
                    if !state.probing {
                        next.probing = true;
                        probe = true;
                    }
                }
            }
        }
        Event::ProbeDue => {
            if state.status == Status::Offline && !state.probing && !state.transport_down {
                next.probing = true;
                probe = true;
            }
        }
        Event::Recheck => {
            if !state.probing && !state.transport_down {
                next.probing = true;
                probe = true;
                if state.status == Status::Online { next.status = Status::Checking; }
            }
        }
        Event::Transport { up: false } => {
            next.transport_down = true;
            next.status = Status::Offline;
        }
        Event::Transport { up: true } => {
            next.transport_down = false;
            if state.status == Status::Offline && !state.probing {
                next.probing = true;
                probe = true;
            }
        }
        Event::Reset => {
            next = Health { transport_down: state.transport_down, ..Health::INITIAL };
        }
    }
    // A probe that ran before a reset is not this machine's; the shell drops
    // its result by generation, so the flag goes with the reset.
    let publish = (next.status != state.status).then_some(next.status);
    (next, if probe || publish.is_some() { Effect { probe, publish } } else { NONE })
}

impl Health {
    /// The invariants every reachable state keeps.
    pub fn valid(self) -> bool {
        self.failures <= OFFLINE_AFTER
            && (self.status != Status::Checking || (self.probing && self.failures < OFFLINE_AFTER))
            && (self.status != Status::Online || self.failures == 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn states() -> Vec<Health> {
        let mut out = Vec::new();
        for status in [Status::Online, Status::Checking, Status::Offline] {
            for failures in 0..=OFFLINE_AFTER {
                for probing in [false, true] {
                    for transport_down in [false, true] {
                        out.push(Health { status, failures, probing, transport_down });
                    }
                }
            }
        }
        out
    }

    fn events() -> Vec<Event> {
        let mut out = vec![Event::Success, Event::ProbeDone { ok: true }, Event::ProbeDone { ok: false }, Event::ProbeDue,
            Event::Recheck, Event::Transport { up: true }, Event::Transport { up: false }, Event::Reset];
        for priority in [Priority::Foreground, Priority::Background, Priority::Low] { out.push(Event::Failure { priority }); }
        out
    }

    #[test]
    fn every_reachable_state_keeps_the_invariants() {
        // The shell delivers ProbeDone only while a probe runs.
        let n = crate::model::explore([Health::INITIAL], 1000, |state| {
            events().into_iter()
                .filter(|event| !matches!(event, Event::ProbeDone { .. }) || state.probing)
                .map(|event| transition(*state, event).0).collect()
        }, |state| assert!(state.valid(), "{state:?}"));
        assert!(n >= 20, "{n} states");
    }

    #[test]
    fn every_state_and_event_follows_the_rules() {
        for state in states().into_iter().filter(|s| s.valid()) {
            for event in events() {
                let (next, effect) = transition(state, event);
                assert_eq!(effect.publish.is_some(), next.status != state.status);
                if effect.probe { assert!(!state.probing || state.status == Status::Online && event == Event::ProbeDone { ok: false }); assert!(next.probing); }
                if matches!(event, Event::Success | Event::ProbeDone { ok: true }) { assert_eq!((next.status, next.failures), (Status::Online, 0)); }
                if event == (Event::Failure { priority: Priority::Low }) { assert_eq!(next, state); }
                if state.status == Status::Online && matches!(event, Event::Failure { .. }) { assert_ne!(next.status, Status::Offline, "{state:?}"); }
                if next.status == Status::Offline && state.status != Status::Offline {
                    assert!(matches!(event, Event::ProbeDone { ok: false } | Event::Transport { up: false } | Event::Reset)
                        || (event == Event::Failure { priority: Priority::Foreground } && next.failures == OFFLINE_AFTER), "{state:?} {event:?}");
                }
            }
        }
    }

    #[test]
    fn a_slow_moment_checks_and_returns_online() {
        let s = transition(Health { status: Status::Online, ..Health::INITIAL }, Event::Failure { priority: Priority::Foreground });
        assert_eq!(s.0.status, Status::Checking);
        assert_eq!(s.1, Effect { probe: true, publish: Some(Status::Checking) });
        let s = transition(s.0, Event::Failure { priority: Priority::Background });
        assert_eq!((s.0.status, s.1.probe), (Status::Checking, false), "one probe at a time");
        let s = transition(s.0, Event::ProbeDone { ok: true });
        assert_eq!(s.0, Health { status: Status::Online, ..Health::INITIAL });
    }

    #[test]
    fn a_dead_server_goes_offline_on_the_probe_or_the_third_failure() {
        let online = Health { status: Status::Online, ..Health::INITIAL };
        let s = transition(online, Event::Failure { priority: Priority::Foreground }).0;
        assert_eq!(transition(s, Event::ProbeDone { ok: false }).0.status, Status::Offline);
        let s = transition(s, Event::Failure { priority: Priority::Foreground }).0;
        assert_eq!(s.status, Status::Checking);
        let s = transition(s, Event::Failure { priority: Priority::Foreground });
        assert_eq!(s.0.status, Status::Offline);
        assert_eq!(s.1.publish, Some(Status::Offline));
        // The probe still running reports later; a failure keeps it offline.
        assert_eq!(transition(s.0, Event::ProbeDone { ok: false }).0.status, Status::Offline);
    }

    #[test]
    fn a_recheck_probes_an_untested_status_once() {
        // Online with nothing asked since: Checking until the probe answers.
        let online = Health { status: Status::Online, ..Health::INITIAL };
        let (checking, effect) = transition(online, Event::Recheck);
        assert_eq!(effect, Effect { probe: true, publish: Some(Status::Checking) });
        assert!(checking.valid() && checking.probing && checking.failures == 0);
        assert_eq!(transition(checking, Event::Recheck), (checking, NONE), "one probe at a time");
        assert_eq!(transition(checking, Event::ProbeDone { ok: true }).0, online);
        assert_eq!(transition(checking, Event::ProbeDone { ok: false }).0.status, Status::Offline);
        // Offline with a transport probes and stays Offline until it answers.
        let (offline, effect) = transition(Health::INITIAL, Event::Recheck);
        assert_eq!((offline.status, effect), (Status::Offline, Effect { probe: true, publish: None }));
        // No transport: nothing to ask.
        let down = Health { transport_down: true, ..Health::INITIAL };
        assert_eq!(transition(down, Event::Recheck), (down, NONE));
    }

    #[test]
    fn transport_goes_offline_at_once_and_probes_when_back() {
        let online = Health { status: Status::Online, ..Health::INITIAL };
        let (down, effect) = transition(online, Event::Transport { up: false });
        assert_eq!((down.status, effect.publish, effect.probe), (Status::Offline, Some(Status::Offline), false));
        assert_eq!(transition(down, Event::ProbeDue).1.probe, false, "no probing without a transport");
        let (up, effect) = transition(down, Event::Transport { up: true });
        assert!(effect.probe && up.probing && up.status == Status::Offline);
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// No single failure takes an online server offline.
    #[kani::proof]
    fn one_failure_never_goes_offline_from_online() {
        let state: Health = kani::any();
        kani::assume(state.valid() && state.status == Status::Online);
        let priority: Priority = kani::any();
        assert!(transition(state, Event::Failure { priority }).0.status != Status::Offline);
    }

    /// Low-priority failures are never evidence.
    #[kani::proof]
    fn low_priority_failures_change_nothing() {
        let state: Health = kani::any();
        assert!(transition(state, Event::Failure { priority: Priority::Low }) == (state, NONE));
    }

    /// Any success is online with nothing counted.
    #[kani::proof]
    fn any_success_is_online() {
        let state: Health = kani::any();
        let event = if kani::any::<bool>() { Event::Success } else { Event::ProbeDone { ok: true } };
        let next = transition(state, event).0;
        assert!(next.status == Status::Online && next.failures == 0);
    }

    /// Offline is entered only on evidence.
    #[kani::proof]
    fn offline_only_on_evidence() {
        let state: Health = kani::any();
        kani::assume(state.valid() && state.status != Status::Offline);
        let event: Event = kani::any();
        let next = transition(state, event).0;
        if next.status == Status::Offline {
            assert!(matches!(event, Event::ProbeDone { ok: false } | Event::Transport { up: false } | Event::Reset)
                || (event == Event::Failure { priority: Priority::Foreground } && next.failures == OFFLINE_AFTER));
        }
    }

    /// Bounded: over every sequence of 8 events, at most one probe runs,
    /// `Checking` always has one, and the invariants hold.
    #[kani::proof]
    #[kani::unwind(9)]
    fn one_probe_at_a_time_and_checking_always_has_one() {
        let mut state = Health::INITIAL;
        let mut running: u8 = 0;
        for _ in 0..8 {
            let event: Event = kani::any();
            // The shell delivers a probe's end only while one runs; a reset
            // drops the running probe's result by generation.
            kani::assume(!matches!(event, Event::ProbeDone { .. }) || running == 1);
            let (next, effect) = transition(state, event);
            if matches!(event, Event::ProbeDone { .. }) || event == Event::Reset { running = 0; }
            if effect.probe { running += 1; }
            assert!(running <= 1);
            assert!(next.probing == (running == 1));
            assert!(next.valid());
            state = next;
        }
    }
}
