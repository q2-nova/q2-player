//! The power/storage barrier between the player thread and its callers
//! (`PlayerHandle::prepare_sleep`, `prepare_handoff`, `cancel_handoff`).
//!
//! Two small machines cooperate across the command channel:
//!
//! * [`Gate`], owned by the player thread: `Open`, or `Frozen` after a
//!   prepare request succeeded and its acknowledgement was delivered. While
//!   frozen every command except `Quit` and the barrier commands is dropped,
//!   so nothing reopens the card or the PCM during sleep or USB export.
//! * [`Claim`], owned by the handle: at most one caller holds the barrier.
//!   A caller that fails or times out queues a cancel before it gives the
//!   claim up, so a late success cannot leave the player frozen.
//!
//! # Invariants
//!
//! * The gate freezes only on a successful prepare.
//! * A frozen gate runs no command other than `Quit`, and `Cancel` always
//!   opens it.
//! * A prepare whose acknowledgement could not be delivered leaves the gate
//!   open.
//! * In every interleaving of two callers, timeouts and the channel
//!   ([`tests`]): when the system is quiescent and nobody holds the claim,
//!   the gate is open (no stranded freeze); a prepare never arrives at a
//!   frozen gate; and at most one caller believes it holds the barrier.

/// The player thread's side of the barrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum Gate {
    /// Commands run.
    #[default]
    Open,
    /// Only `Quit` and barrier commands run.
    Frozen,
}

/// What reached the player thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum Event {
    /// `PrepareSleep` or `PrepareHandoff` was taken from the channel.
    Prepare,
    /// The preparation the gate admitted finished: paused, saved (and for a
    /// handoff released), or not.
    Prepared {
        /// It succeeded.
        ok: bool,
    },
    /// The acknowledgement could not be delivered: the caller is gone.
    AckLost,
    /// `CancelHandoff`.
    Cancel,
    /// Any other command.
    Command {
        /// It is `Quit`.
        quit: bool,
    },
}

/// What the player thread does with the event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Effect {
    /// Nothing further.
    None,
    /// Carry out the preparation, then report it with [`Event::Prepared`].
    Attempt,
    /// Answer "a transition is already pending" without preparing.
    Refuse,
    /// Run the command.
    Run,
    /// Drop the command.
    Ignore,
}

/// The gate's response to `event`.
pub fn transition(gate: Gate, event: Event) -> (Gate, Effect) {
    match (gate, event) {
        (Gate::Open, Event::Prepare) => (Gate::Open, Effect::Attempt),
        (Gate::Frozen, Event::Prepare) => (Gate::Frozen, Effect::Refuse),
        (_, Event::Prepared { ok: true }) => (Gate::Frozen, Effect::None),
        (gate, Event::Prepared { ok: false }) => (gate, Effect::None),
        (_, Event::AckLost) | (_, Event::Cancel) => (Gate::Open, Effect::None),
        (Gate::Frozen, Event::Command { quit: false }) => (Gate::Frozen, Effect::Ignore),
        (gate, Event::Command { .. }) => (gate, Effect::Run),
    }
}

/// The handle's side: whether a caller holds (or is acquiring) the barrier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum Claim {
    /// Nobody.
    #[default]
    Free,
    /// One caller is acquiring or holds it.
    Held,
}

/// What a caller did or learned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum ClaimEvent {
    /// A caller asks for the barrier.
    Acquire,
    /// The player's answer arrived in time.
    Answer {
        /// The player prepared and froze.
        ok: bool,
    },
    /// No answer within the caller's bound, or the player is gone.
    Timeout,
    /// The holder releases the barrier (`cancel_handoff`).
    Release,
}

/// What the handle does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ClaimEffect {
    /// Nothing.
    None,
    /// Send the prepare command and wait for its answer.
    SendPrepare,
    /// Tell the caller a transition is already pending.
    Busy,
    /// Send `CancelHandoff` (before the claim is given up).
    SendCancel,
}

/// The claim's response to `event`.
pub fn claim(state: Claim, event: ClaimEvent) -> (Claim, ClaimEffect) {
    match (state, event) {
        (Claim::Free, ClaimEvent::Acquire) => (Claim::Held, ClaimEffect::SendPrepare),
        (Claim::Held, ClaimEvent::Acquire) => (Claim::Held, ClaimEffect::Busy),
        (Claim::Held, ClaimEvent::Answer { ok: true }) => (Claim::Held, ClaimEffect::None),
        (_, ClaimEvent::Answer { ok: false }) | (_, ClaimEvent::Timeout) | (_, ClaimEvent::Release) =>
            (Claim::Free, ClaimEffect::SendCancel),
        (Claim::Free, ClaimEvent::Answer { ok: true }) => (Claim::Free, ClaimEffect::SendCancel),
    }
}

#[cfg(test)]
pub mod tests {
    //! Explicit-state model of two callers, the FIFO command channel and the
    //! player thread, over every interleaving.
    use super::*;
    use crate::model::explore;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    enum Msg { Prepare { caller: u8 }, Cancel, Command }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    enum Caller {
        Idle,
        /// Sent a prepare and waits for its answer.
        Waiting,
        /// Was told the barrier is held for it.
        Holding,
    }

    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    struct System {
        gate: Gate,
        claim: Claim,
        channel: Vec<Msg>,
        callers: [Caller; 2],
        /// Answers the player sent that the caller has not read: Some(ok).
        answers: [Option<bool>; 2],
        /// Prepare attempts each caller may still make (bounds the model).
        budget: [u8; 2],
        commands: u8,
    }

    const CHANNEL: usize = 5;

    fn successors(s: &System) -> Vec<System> {
        let mut out = Vec::new();
        for c in 0..2usize {
            match s.callers[c] {
                Caller::Idle if s.budget[c] > 0 && s.channel.len() < CHANNEL => {
                    let mut n = s.clone();
                    n.budget[c] -= 1;
                    let (state, effect) = claim(s.claim, ClaimEvent::Acquire);
                    n.claim = state;
                    if effect == ClaimEffect::SendPrepare {
                        n.channel.push(Msg::Prepare { caller: c as u8 });
                        n.callers[c] = Caller::Waiting;
                        n.answers[c] = None;
                    }
                    out.push(n);
                }
                Caller::Waiting => {
                    // The answer arrives in time...
                    if let Some(ok) = s.answers[c] {
                        let mut n = s.clone();
                        n.answers[c] = None;
                        let (state, effect) = claim(s.claim, ClaimEvent::Answer { ok });
                        n.claim = state;
                        if effect == ClaimEffect::SendCancel { n.channel.push(Msg::Cancel); }
                        n.callers[c] = if ok { Caller::Holding } else { Caller::Idle };
                        if n.channel.len() <= CHANNEL { out.push(n); }
                    }
                    // ...or the caller's bound expires first (the receiver is dropped).
                    let mut n = s.clone();
                    n.answers[c] = None;
                    let (state, effect) = claim(s.claim, ClaimEvent::Timeout);
                    n.claim = state;
                    assert_eq!(effect, ClaimEffect::SendCancel);
                    n.channel.push(Msg::Cancel);
                    n.callers[c] = Caller::Idle;
                    if n.channel.len() <= CHANNEL { out.push(n); }
                }
                Caller::Holding if s.channel.len() < CHANNEL => {
                    let mut n = s.clone();
                    let (state, effect) = claim(s.claim, ClaimEvent::Release);
                    n.claim = state;
                    assert_eq!(effect, ClaimEffect::SendCancel);
                    n.channel.push(Msg::Cancel);
                    n.callers[c] = Caller::Idle;
                    out.push(n);
                }
                _ => {}
            }
        }
        if s.commands > 0 && s.channel.len() < CHANNEL {
            let mut n = s.clone();
            n.commands -= 1;
            n.channel.push(Msg::Command);
            out.push(n);
        }
        if let Some(first) = s.channel.first().copied() {
            let rest = s.channel[1..].to_vec();
            match first {
                Msg::Cancel => {
                    let mut n = s.clone();
                    n.channel = rest;
                    n.gate = transition(s.gate, Event::Cancel).0;
                    out.push(n);
                }
                Msg::Command => {
                    let mut n = s.clone();
                    n.channel = rest;
                    let (gate, effect) = transition(s.gate, Event::Command { quit: false });
                    assert_eq!(effect == Effect::Ignore, s.gate == Gate::Frozen);
                    n.gate = gate;
                    out.push(n);
                }
                Msg::Prepare { caller } => {
                    let caller = caller as usize;
                    let (gate, effect) = transition(s.gate, Event::Prepare);
                    assert_eq!(effect, Effect::Attempt, "a prepare reached a frozen gate: {s:?}");
                    // The preparation succeeds or fails; the caller is still
                    // waiting for this answer, or has given up (ack lost).
                    for ok in [false, true] {
                        let mut n = s.clone();
                        n.channel = rest.clone();
                        n.gate = transition(gate, Event::Prepared { ok }).0;
                        // A caller that timed out and re-acquired waits on a
                        // new channel: this answer cannot reach it. The model
                        // tracks that with the prepare still queued behind.
                        let superseded = rest.iter().any(|m| *m == Msg::Prepare { caller: caller as u8 });
                        if s.callers[caller] == Caller::Waiting && !superseded {
                            n.answers[caller] = Some(ok);
                        } else {
                            n.gate = transition(n.gate, Event::AckLost).0;
                        }
                        out.push(n);
                    }
                }
            }
        }
        out
    }

    #[test]
    fn no_interleaving_strands_a_frozen_player_or_grants_the_barrier_twice() {
        let initial = System { gate: Gate::Open, claim: Claim::Free, channel: Vec::new(),
            callers: [Caller::Idle; 2], answers: [None; 2], budget: [2, 2], commands: 1 };
        let states = explore([initial], 2_000_000, successors, |s| {
            let holding = s.callers.iter().filter(|c| **c == Caller::Holding).count();
            let busy = s.callers.iter().filter(|c| **c != Caller::Idle).count();
            assert!(holding <= 1 && busy <= 1, "two callers in the barrier: {s:?}");
            assert_eq!(s.claim == Claim::Held, busy == 1, "{s:?}");
            if s.callers.iter().any(|c| *c == Caller::Holding) {
                assert_eq!(s.gate, Gate::Frozen, "a holder's player is not frozen: {s:?}");
            }
            // Quiescent with the claim free: the player must be open.
            if s.channel.is_empty() && s.claim == Claim::Free {
                assert_eq!(s.gate, Gate::Open, "stranded freeze: {s:?}");
            }
        });
        assert!(states > 500, "{states}");
    }

    #[test]
    fn gate_rules_hold_for_every_state_and_event() {
        for gate in [Gate::Open, Gate::Frozen] {
            assert_eq!(transition(gate, Event::Cancel).0, Gate::Open);
            assert_eq!(transition(gate, Event::AckLost).0, Gate::Open);
            assert_eq!(transition(gate, Event::Prepared { ok: false }).0, gate);
            assert_eq!(transition(gate, Event::Prepared { ok: true }).0, Gate::Frozen);
            assert_eq!(transition(gate, Event::Command { quit: true }), (gate, Effect::Run));
        }
        assert_eq!(transition(Gate::Frozen, Event::Command { quit: false }), (Gate::Frozen, Effect::Ignore));
        assert_eq!(transition(Gate::Frozen, Event::Prepare), (Gate::Frozen, Effect::Refuse));
        assert_eq!(transition(Gate::Open, Event::Prepare), (Gate::Open, Effect::Attempt));
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    #[kani::proof]
    fn the_gate_freezes_only_on_a_successful_prepare_and_cancel_opens_it() {
        let gate: Gate = kani::any();
        let event: Event = kani::any();
        let (next, effect) = transition(gate, event);
        if gate == Gate::Open && next == Gate::Frozen { assert!(event == Event::Prepared { ok: true }); }
        if matches!(event, Event::Cancel | Event::AckLost) { assert!(next == Gate::Open); }
        if gate == Gate::Frozen {
            if let Event::Command { quit } = event { assert!((effect == Effect::Run) == quit); }
            if event == Event::Prepare { assert!(effect == Effect::Refuse); }
        }
        if gate == Gate::Open && matches!(event, Event::Command { .. }) { assert!(effect == Effect::Run); }
    }

    /// Bounded: every sequence of 6 gate events.
    #[kani::proof]
    #[kani::unwind(7)]
    fn no_command_runs_between_a_delivered_freeze_and_its_cancel() {
        let mut gate = Gate::Open;
        let mut frozen = false;
        for _ in 0..6 {
            let event: Event = kani::any();
            let (next, effect) = transition(gate, event);
            if frozen {
                if let Event::Command { quit: false } = event { assert!(effect == Effect::Ignore); }
            }
            match event {
                Event::Prepared { ok: true } => frozen = true,
                Event::Cancel | Event::AckLost => frozen = false,
                _ => {}
            }
            gate = next;
            assert!((gate == Gate::Frozen) == frozen);
        }
    }

    #[kani::proof]
    fn giving_up_a_claim_always_queues_a_cancel() {
        let state: Claim = kani::any();
        let event: ClaimEvent = kani::any();
        let (next, effect) = claim(state, event);
        if state == Claim::Held && next == Claim::Free { assert!(effect == ClaimEffect::SendCancel); }
        if state == Claim::Held && event == ClaimEvent::Acquire { assert!(effect == ClaimEffect::Busy && next == Claim::Held); }
    }
}
