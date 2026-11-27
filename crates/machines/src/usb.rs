//! The USB drive handoff request (`usb.rs`, `UsbDrive::start`).
//!
//! The app asks the OS helper to export the card to a computer. One request
//! at a time: a worker first obtains the player's storage barrier, then runs
//! the helper, and the request is over when the worker returns (on success
//! the supervisor stops the app, so there is no "exported" state here).
//!
//! # Invariants
//!
//! * A worker starts only when a configured computer connection is present
//!   and no request is open, so two helpers never run together.
//! * Every refusal leaves the state unchanged.
//! * The request closes when its worker returns or could not be started.

/// Whether a handoff request is open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum Usb {
    /// No request.
    #[default]
    Idle,
    /// A worker is preparing the player or running the helper.
    Opening,
}

/// What happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum Event {
    /// The listener chose USB Drive.
    Start {
        /// The helper is installed and a computer has configured the port.
        available: bool,
    },
    /// The worker thread could not be created.
    SpawnFailed,
    /// The worker returned (refused preparation, helper failure, or the
    /// helper's success just before the app is stopped).
    WorkerDone,
}

/// What the app does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Effect {
    /// Nothing.
    None,
    /// Start the worker.
    Spawn,
    /// "Connect USB to a computer first".
    RefuseDisconnected,
    /// "USB drive is already opening".
    RefuseBusy,
    /// "USB drive could not start".
    ReportSpawnFailure,
}

/// The request's response to `event`.
pub fn transition(state: Usb, event: Event) -> (Usb, Effect) {
    match (state, event) {
        (_, Event::Start { available: false }) => (state, Effect::RefuseDisconnected),
        (Usb::Opening, Event::Start { available: true }) => (Usb::Opening, Effect::RefuseBusy),
        (Usb::Idle, Event::Start { available: true }) => (Usb::Opening, Effect::Spawn),
        (Usb::Opening, Event::SpawnFailed) => (Usb::Idle, Effect::ReportSpawnFailure),
        (Usb::Opening, Event::WorkerDone) => (Usb::Idle, Effect::None),
        (Usb::Idle, Event::SpawnFailed | Event::WorkerDone) => (Usb::Idle, Effect::None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_state_and_event_follows_the_table() {
        for state in [Usb::Idle, Usb::Opening] {
            for event in [Event::Start { available: false }, Event::Start { available: true }, Event::SpawnFailed, Event::WorkerDone] {
                let (next, effect) = transition(state, event);
                if effect == Effect::Spawn { assert_eq!((state, event, next), (Usb::Idle, Event::Start { available: true }, Usb::Opening)); }
                if matches!(effect, Effect::RefuseBusy | Effect::RefuseDisconnected) { assert_eq!(next, state); }
                if matches!(event, Event::SpawnFailed | Event::WorkerDone) { assert_eq!(next, Usb::Idle); }
            }
        }
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// Unbounded, by one step from any state: the three invariants in the
    /// module header hold for every state and event, so they hold after any
    /// number of events (the bounded proof below stops at six).
    #[kani::proof]
    fn any_step_from_any_state_keeps_the_request_invariants() {
        let state: Usb = kani::any();
        let event: Event = kani::any();
        let (next, effect) = transition(state, event);
        // A worker starts only for a connected computer with no request open.
        if effect == Effect::Spawn {
            assert!(state == Usb::Idle && next == Usb::Opening);
            assert!(matches!(event, Event::Start { available: true }));
        }
        // Every refusal leaves the state unchanged.
        if matches!(effect, Effect::RefuseBusy | Effect::RefuseDisconnected) { assert!(next == state); }
        // The request closes when its worker returns or could not be started,
        // and nothing else closes or opens it.
        if matches!(event, Event::SpawnFailed | Event::WorkerDone) { assert!(next == Usb::Idle); }
        if next != state {
            assert!(effect == Effect::Spawn || matches!(event, Event::SpawnFailed | Event::WorkerDone));
        }
        // A spawn failure is reported only for a request that was open.
        if effect == Effect::ReportSpawnFailure { assert!(state == Usb::Opening); }
    }

    /// Bounded: every sequence of 6 events never has two workers running.
    #[kani::proof]
    #[kani::unwind(7)]
    fn two_handoff_workers_never_run_together() {
        let mut state = Usb::Idle;
        let mut workers: u8 = 0;
        for _ in 0..6 {
            let event: Event = kani::any();
            // A worker can only end if one is running.
            kani::assume(!matches!(event, Event::SpawnFailed | Event::WorkerDone) || workers == 1);
            let (next, effect) = transition(state, event);
            if effect == Effect::Spawn { workers += 1; }
            if matches!(event, Event::SpawnFailed | Event::WorkerDone) { workers -= 1; }
            assert!(workers <= 1);
            assert!((next == Usb::Opening) == (workers == 1));
            if matches!(effect, Effect::RefuseBusy | Effect::RefuseDisconnected) { assert!(next == state); }
            state = next;
        }
    }
}
