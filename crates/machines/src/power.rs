//! Sleep and power-off preparation in the UI loop (`ui/mod.rs`:
//! `prepare_power`, `UiMsg::PowerPrepared`, the tick, `finish_power`).
//!
//! A power transaction saves settings, asks the player and the library for
//! a durable barrier on a worker thread, waits for the keys to be released,
//! enters suspend or power-off, and afterwards releases the barrier it took.
//!
//! # Invariants
//!
//! * Suspend or power-off is entered only from `Ready`, which is reached
//!   only by a successful preparation, and only with all keys released.
//! * While the state is not `Idle`, input is observed but not acted on and
//!   a second request starts nothing.
//! * Finishing cancels the player's barrier exactly when this transaction
//!   holds it (`Ready`, `Entering`, `AwaitingOff`): a refused preparation
//!   never cancels a barrier that belongs to someone else (USB export).
//! * Every state returns to `Idle` on `Finished`.

/// Where a power transaction is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum Power {
    /// No transaction.
    #[default]
    Idle,
    /// The worker is preparing the player and the library.
    Preparing,
    /// Prepared; waiting for the keys to be released.
    Ready {
        /// Power off rather than sleep.
        off: bool,
    },
    /// The main loop was asked to suspend or power off.
    Entering,
    /// Power-off was accepted; waiting for the power to go or an error.
    AwaitingOff,
}

impl Power {
    /// A transaction is under way: input is swallowed, timers are held.
    pub fn pending(self) -> bool { self != Power::Idle }
    /// This transaction holds the player's barrier.
    pub fn barrier(self) -> bool { matches!(self, Power::Ready { .. } | Power::Entering | Power::AwaitingOff) }
}

/// What happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum Event {
    /// Sleep or power-off was requested and the settings are saved.
    Request,
    /// The preparation worker could not be started.
    SpawnFailed,
    /// The worker's result.
    Prepared {
        /// Power off rather than sleep.
        off: bool,
        /// The player and the library acknowledged the barrier.
        ok: bool,
    },
    /// The UI tick.
    Tick {
        /// A key is still held.
        keys_held: bool,
    },
    /// Power-off was accepted by the platform; the app keeps running until
    /// the power goes.
    AwaitOff,
    /// Suspend returned, power-off was refused or failed, or the
    /// preparation failed: restore the UI.
    Finished,
}

/// What the UI loop does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Effect {
    /// Nothing.
    None,
    /// Stop AirPlay, show the notice and start the preparation worker.
    Prepare,
    /// Tell the owner preparation is unavailable.
    Unavailable,
    /// The preparation failed: restore the UI with its error (via
    /// [`Event::Finished`]).
    Failed,
    /// Ask the main loop to suspend or power off.
    Enter {
        /// Power off rather than sleep.
        off: bool,
    },
    /// Restore the UI; `cancel_barrier` when this transaction's barrier
    /// must be released.
    Restore {
        /// Release the player's barrier.
        cancel_barrier: bool,
    },
}

/// The transaction's response to `event`.
pub fn transition(state: Power, event: Event) -> (Power, Effect) {
    match (state, event) {
        (Power::Idle, Event::Request) => (Power::Preparing, Effect::Prepare),
        (_, Event::Request) => (state, Effect::None),
        (Power::Preparing, Event::SpawnFailed) => (Power::Idle, Effect::Unavailable),
        (_, Event::SpawnFailed) => (state, Effect::None),
        (_, Event::Prepared { off, ok: true }) => (Power::Ready { off }, Effect::None),
        (_, Event::Prepared { ok: false, .. }) => (state, Effect::Failed),
        (Power::Ready { off }, Event::Tick { keys_held: false }) => (Power::Entering, Effect::Enter { off }),
        (_, Event::Tick { .. }) => (state, Effect::None),
        (_, Event::AwaitOff) => (Power::AwaitingOff, Effect::None),
        (_, Event::Finished) => (Power::Idle, Effect::Restore { cancel_barrier: state.barrier() }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::explore;

    fn events() -> Vec<Event> {
        let mut e = vec![Event::Request, Event::SpawnFailed, Event::AwaitOff, Event::Finished];
        for a in [false, true] {
            e.push(Event::Tick { keys_held: a });
            for b in [false, true] { e.push(Event::Prepared { off: a, ok: b }); }
        }
        e
    }

    /// The protocol the UI loop follows: one worker result per preparation,
    /// `AwaitOff`/`Finished` only after entering (or `Finished` on failure).
    #[test]
    fn suspend_is_entered_only_after_a_successful_preparation_with_keys_released() {
        // (state, a worker result is outstanding)
        let states = explore([(Power::Idle, false)], 100, |(state, outstanding)| {
            let mut out = Vec::new();
            for event in events() {
                let allowed = match event {
                    Event::Prepared { .. } => *outstanding,
                    Event::SpawnFailed => *state == Power::Preparing && !*outstanding,
                    Event::AwaitOff => *state == Power::Entering,
                    Event::Finished => *state == Power::Entering || *state == Power::AwaitingOff,
                    _ => true,
                };
                if !allowed { continue; }
                let (next, effect) = transition(*state, event);
                let mut outstanding = *outstanding;
                match effect {
                    Effect::Prepare => { assert!(!outstanding, "two preparations"); outstanding = true; }
                    Effect::Enter { off } => assert_eq!((*state, event), (Power::Ready { off }, Event::Tick { keys_held: false })),
                    Effect::Failed => {
                        // The shell follows a failed preparation with Finished.
                        let (idle, restore) = transition(next, Event::Finished);
                        assert_eq!((idle, restore), (Power::Idle, Effect::Restore { cancel_barrier: false }));
                        out.push((idle, false));
                        continue;
                    }
                    _ => {}
                }
                if matches!(event, Event::Prepared { .. }) { outstanding = false; }
                if next != Power::Idle || !outstanding { out.push((next, outstanding)); }
            }
            out
        }, |(state, outstanding)| {
            if *outstanding { assert_eq!(*state, Power::Preparing); }
        });
        assert_eq!(states, 6);
    }

    #[test]
    fn finishing_releases_the_barrier_only_when_this_transaction_holds_it() {
        for (state, held) in [(Power::Idle, false), (Power::Preparing, false), (Power::Ready { off: false }, true),
            (Power::Ready { off: true }, true), (Power::Entering, true), (Power::AwaitingOff, true)] {
            assert_eq!(transition(state, Event::Finished), (Power::Idle, Effect::Restore { cancel_barrier: held }));
            assert_eq!(state.pending(), state != Power::Idle);
            if state != Power::Idle { assert_eq!(transition(state, Event::Request), (state, Effect::None)); }
        }
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    #[kani::proof]
    fn power_is_entered_only_from_ready_with_keys_released() {
        let state: Power = kani::any();
        let event: Event = kani::any();
        let (next, effect) = transition(state, event);
        if let Effect::Enter { off } = effect {
            assert!(state == Power::Ready { off } && event == Event::Tick { keys_held: false } && next == Power::Entering);
        }
        if next == (Power::Ready { off: true }) || next == (Power::Ready { off: false }) {
            assert!(matches!(event, Event::Prepared { ok: true, .. }) || next == state);
        }
        if effect == Effect::Prepare { assert!(state == Power::Idle); }
    }

    #[kani::proof]
    fn finishing_returns_to_idle_and_cancels_exactly_a_held_barrier() {
        let state: Power = kani::any();
        let (next, effect) = transition(state, Event::Finished);
        assert!(next == Power::Idle);
        assert!(effect == Effect::Restore { cancel_barrier: state.barrier() });
        assert!(!Power::Preparing.barrier() && !Power::Idle.barrier());
    }

    /// Bounded: every sequence of 6 events. The barrier is held only after
    /// a successful preparation or an accepted power-off since the last finish.
    #[kani::proof]
    #[kani::unwind(7)]
    fn a_barrier_is_claimed_only_after_a_successful_preparation() {
        let mut state = Power::Idle;
        let mut prepared = false;
        for _ in 0..6 {
            let event: Event = kani::any();
            kani::assume(event != Event::AwaitOff || state == Power::Entering);
            let (next, _) = transition(state, event);
            match event {
                Event::Prepared { ok: true, .. } => prepared = true,
                Event::Finished => prepared = false,
                _ => {}
            }
            state = next;
            if state.barrier() { assert!(prepared); }
        }
    }
}
