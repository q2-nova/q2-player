//! The Wi-Fi radio's intent (`net/radio.rs`): what the owner saved, the
//! temporary Offline and doze suppressions, and which request is current.
//!
//! Every change of intent takes a new generation. Helper threads carry the
//! generation they were started for; a completion or failure for an older
//! generation changes nothing, so a slow helper never undoes a newer choice.
//!
//! # Invariants
//!
//! * The generation never decreases, and changes exactly when the intent
//!   (requested, offline, doze) is replaced by a request or a doze change.
//! * `doze` implies `requested && !offline`: only a saved-on, non-Offline
//!   radio dozes, and any explicit request ends a doze.
//! * A fault always has a message ([`Intent::fault`] implies
//!   [`Intent::error`]), so the UI's `take_fault` cannot fail.
//! * An event carrying a stale generation leaves the intent unchanged.
//! * A failure never turns the saved request off: it rolls back only the
//!   temporary suppression (doze, else Offline).

/// Why the last radio operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum Fault {
    /// Turning the radio down for Offline Mode failed.
    Offline,
    /// A Wi-Fi on/off change failed.
    Change,
    /// Bringing the radio back after a failed Offline or doze failed.
    Restore,
}

impl Fault {
    /// The notice shown to the owner.
    pub fn message(self) -> &'static str {
        match self {
            Fault::Offline => "Offline failed. Check Wi-Fi and try again.",
            Fault::Change => "Wi-Fi change failed. Try again in Wi-Fi settings.",
            Fault::Restore => "Wi-Fi restore failed. Turn Wi-Fi on to retry.",
        }
    }
}

/// The radio's intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub struct Intent {
    /// Identifies the current request; helpers for older ones are stale.
    pub generation: u64,
    /// The owner's saved Wi-Fi choice.
    pub requested: bool,
    /// Offline Mode suppresses the radio.
    pub offline: bool,
    /// The automatic doze suppresses the radio.
    pub doze: bool,
    /// The last failure, kept for display until the next request.
    pub error: Option<Fault>,
    /// The failure has not been shown yet.
    pub fault: bool,
}

impl Intent {
    /// The intent at start: the saved choice, nothing suppressed.
    pub fn new(requested: bool) -> Self {
        Intent { generation: 0, requested, offline: false, doze: false, error: None, fault: false }
    }
    /// The radio should be up.
    pub fn enabled(self) -> bool { self.requested && !self.offline && !self.doze }
    /// The invariant every transition preserves.
    pub fn valid(self) -> bool { (!self.doze || (self.requested && !self.offline)) && (!self.fault || self.error.is_some()) }
}

/// What happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum Event {
    /// An explicit Wi-Fi or Offline choice.
    Request {
        /// The saved Wi-Fi choice.
        requested: bool,
        /// Offline Mode.
        offline: bool,
    },
    /// The doze policy starts or ends a doze.
    Doze(bool),
    /// The helper for `generation` failed.
    Failed(u64),
    /// Bringing the radio back for `generation` failed too.
    RestoreFailed(u64),
    /// The UI collects an unseen fault.
    TakeFault,
}

/// What the Wi-Fi code does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Effect {
    /// Nothing: the event did not apply.
    None,
    /// Run the helper for the returned intent.
    Apply,
    /// The failure belonged to the current request: the returned intent is
    /// the rolled-back one (restore the radio if it is enabled).
    RolledBack,
    /// Show this notice; `offline` is whether Offline Mode is still on.
    Show {
        /// Offline Mode after the rollback.
        offline: bool,
        /// The failure.
        fault: Fault,
    },
}

/// The intent's response to `event`. `None` when the generation counter is
/// exhausted (2^64 requests), which the caller treats as fatal.
pub fn transition(i: Intent, event: Event) -> Option<(Intent, Effect)> {
    Some(match event {
        Event::Request { requested, offline } => (Intent {
            generation: i.generation.checked_add(1)?, requested, offline, doze: false, error: None, fault: false,
        }, Effect::Apply),
        Event::Doze(on) => {
            if i.doze == on || (on && (!i.requested || i.offline)) { return Some((i, Effect::None)); }
            (Intent { generation: i.generation.checked_add(1)?, doze: on, error: None, fault: false, ..i }, Effect::Apply)
        }
        Event::Failed(generation) => {
            if i.generation != generation { return Some((i, Effect::None)); }
            // A failed automatic doze is rolled back silently: the owner
            // asked for nothing, so there is nothing to report.
            if i.doze { return Some((Intent { doze: false, ..i }, Effect::RolledBack)); }
            let fault = if i.offline { Fault::Offline } else { Fault::Change };
            (Intent { offline: false, error: Some(fault), fault: true, ..i }, Effect::RolledBack)
        }
        Event::RestoreFailed(generation) => {
            if i.generation != generation { return Some((i, Effect::None)); }
            (Intent { error: Some(Fault::Restore), fault: true, ..i }, Effect::None)
        }
        Event::TakeFault => match (i.fault, i.error) {
            (true, Some(fault)) => (Intent { fault: false, ..i }, Effect::Show { offline: i.offline, fault }),
            _ => (Intent { fault: false, ..i }, Effect::None),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::explore;

    /// Every intent reachable within a bounded number of generations, with
    /// events for the current and the previous generation.
    #[test]
    fn every_reachable_intent_is_valid_and_stale_events_change_nothing() {
        const GENERATIONS: u64 = 6;
        let states = explore([Intent::new(false), Intent::new(true)], 100_000, |i| {
            let mut events = vec![Event::Doze(false), Event::Doze(true), Event::TakeFault];
            for a in [false, true] { for b in [false, true] { events.push(Event::Request { requested: a, offline: b }); } }
            for g in [i.generation, i.generation.wrapping_sub(1), i.generation + 1] {
                events.push(Event::Failed(g));
                events.push(Event::RestoreFailed(g));
            }
            events.into_iter().filter_map(|event| {
                let (next, effect) = transition(*i, event).unwrap();
                assert!(next.valid(), "{i:?} --{event:?}--> {next:?}");
                assert!(next.generation >= i.generation);
                if let Event::Failed(g) | Event::RestoreFailed(g) = event {
                    if g != i.generation { assert_eq!((next, effect), (*i, Effect::None)); }
                    assert_eq!(next.requested, i.requested, "a failure changed the saved choice");
                }
                if next.generation != i.generation { assert_eq!(effect, Effect::Apply); }
                if (next.requested, next.offline, next.doze) != (i.requested, i.offline, i.doze) && effect != Effect::RolledBack {
                    assert!(next.generation > i.generation, "intent changed without a generation: {event:?}");
                }
                (next.generation <= GENERATIONS).then_some(next)
            }).collect()
        }, |i| assert!(i.valid()));
        assert!(states > 50, "{states}");
    }

    #[test]
    fn a_failed_doze_rolls_back_silently_and_a_failed_offline_reports() {
        let on = Intent::new(true);
        let (dozing, _) = transition(on, Event::Doze(true)).unwrap();
        assert!(dozing.doze && !dozing.enabled());
        let (back, effect) = transition(dozing, Event::Failed(dozing.generation)).unwrap();
        assert!(back.enabled() && !back.fault && effect == Effect::RolledBack);
        let (offline, _) = transition(on, Event::Request { requested: true, offline: true }).unwrap();
        let (failed, _) = transition(offline, Event::Failed(offline.generation)).unwrap();
        assert_eq!(transition(failed, Event::TakeFault).unwrap().1, Effect::Show { offline: false, fault: Fault::Offline });
        assert_eq!(transition(Intent::new(false), Event::Doze(true)).unwrap().1, Effect::None);
        assert!(transition(Intent { generation: u64::MAX, ..on }, Event::Doze(true)).is_none());
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    fn any_intent() -> Intent {
        let i: Intent = kani::any();
        kani::assume(i.valid());
        i
    }

    /// Unbounded: all 2^64 generations.
    #[kani::proof]
    fn transitions_preserve_validity_and_generations_never_decrease() {
        let i = any_intent();
        if let Some((next, _)) = transition(i, kani::any()) {
            assert!(next.valid());
            assert!(next.generation >= i.generation);
        }
    }

    #[kani::proof]
    fn a_stale_generation_changes_nothing() {
        let i = any_intent();
        let g: u64 = kani::any();
        kani::assume(g != i.generation);
        assert!(transition(i, Event::Failed(g)) == Some((i, Effect::None)));
        assert!(transition(i, Event::RestoreFailed(g)) == Some((i, Effect::None)));
    }

    #[kani::proof]
    fn doze_starts_only_on_a_saved_on_radio_and_any_request_ends_it() {
        let i = any_intent();
        let event: Event = kani::any();
        if let Some((next, _)) = transition(i, event) {
            if next.doze && !i.doze { assert!(event == Event::Doze(true) && i.requested && !i.offline); }
            if let Event::Request { .. } = event { assert!(!next.doze && next.generation == i.generation + 1); }
            if let Event::Failed(_) | Event::RestoreFailed(_) | Event::TakeFault = event {
                assert!(next.requested == i.requested && next.generation == i.generation);
            }
        }
    }

    #[kani::proof]
    fn a_shown_fault_always_has_its_message() {
        let i = any_intent();
        if let Some((next, effect)) = transition(i, Event::TakeFault) {
            assert!(!next.fault);
            assert!(matches!(effect, Effect::Show { .. }) == i.fault);
        }
    }
}
