//! The lifecycle of one cache transfer slot (`library/mod.rs`: `Transfer`,
//! `TransferUse`, `start_fetch`, `cancel_orphans`).
//!
//! The library keeps one slot per (song, variant). A slot holds the transfer
//! readers may join: the player's streams and the fetch-ahead worker each
//! take a hold. When the last hold drops the transfer is orphaned and keeps
//! downloading; a sweep (the player opened another song, or fetch-ahead moved
//! on) cancels orphans that are not wanted and not nearly complete. A
//! cancelled transfer's thread stops at its next read and removes the slot
//! if the slot is still its own; meanwhile a new open replaces the slot with
//! a fresh transfer writing its own partial file.
//!
//! # Invariants
//!
//! Step properties of [`transition`] (all states and events):
//!
//! * a cancelled transfer is never joined;
//! * only an orphan (no holds) is ever cancelled, so a cancel never cuts a
//!   reader off;
//! * `orphaned` exactly when `users == 0`.
//!
//! System properties ([`tests`], every interleaving of opens, hold drops,
//! sweeps and thread exits):
//!
//! * at most one transfer per key is running without its cancel flag set;
//! * `users` equals the number of live holds on the slot's transfer;
//! * no hold ever refers to a cancelled transfer;
//! * every hold dropped and every thread finished leaves no slot behind (no
//!   leaked transfer).

/// The joinable transfer in a slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub struct Slot {
    /// Holds on the transfer.
    pub users: usize,
    /// The last hold dropped and none has joined since.
    pub orphaned: bool,
    /// The cancel flag is set: the thread stops at its next read.
    pub cancelled: bool,
    /// A player stream holds or held it (fetch-ahead yields to it).
    pub playback: bool,
}

impl Slot {
    /// The invariant every transition preserves.
    pub fn valid(self) -> bool { self.orphaned == (self.users == 0) && (!self.cancelled || self.users == 0) }
}

/// What happened to the slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum Event {
    /// A reader wants the song (`start_fetch`).
    Open {
        /// The reader is the player.
        playback: bool,
    },
    /// No joinable transfer and no cached copy: a new transfer was created.
    Started {
        /// It was started for the player.
        playback: bool,
    },
    /// A hold dropped. `own`: the hold belongs to the slot's transfer (a
    /// hold on a replaced transfer changes nothing).
    Released {
        /// The hold is on this slot's transfer.
        own: bool,
    },
    /// An orphan sweep reached the slot.
    Sweep {
        /// The sweeper still wants this song.
        keep: bool,
        /// The transfer is complete or at least 80 % downloaded.
        retain: bool,
    },
    /// A transfer thread ended. `own`: it is the slot's transfer.
    Finished {
        /// The thread is this slot's transfer.
        own: bool,
    },
}

/// What the library does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Effect {
    /// Nothing.
    None,
    /// Hand the reader a hold on the existing transfer. `rejoined`: it was
    /// orphaned.
    Join {
        /// The transfer had no holds.
        rejoined: bool,
    },
    /// Nothing to join: use the cached copy, or create a transfer and report
    /// [`Event::Started`].
    StartNew,
    /// Spawn the transfer thread and hand the reader the first hold.
    Spawn,
    /// The last hold dropped (trace it).
    Orphaned,
    /// Set the cancel flag.
    Cancel,
    /// Remove the slot.
    Remove,
}

/// The slot's response to `event`.
pub fn transition(slot: Option<Slot>, event: Event) -> (Option<Slot>, Effect) {
    match (slot, event) {
        (Some(s), Event::Open { playback }) if !s.cancelled => match s.users.checked_add(1) {
            Some(users) => (Some(Slot { users, orphaned: false, cancelled: false, playback: s.playback | playback }),
                Effect::Join { rejoined: s.orphaned }),
            // More holds than addresses cannot exist; refuse rather than wrap.
            None => (slot, Effect::StartNew),
        },
        (_, Event::Open { .. }) => (slot, Effect::StartNew),
        (Some(s), Event::Started { .. }) if !s.cancelled => (slot, Effect::None),
        (_, Event::Started { playback }) =>
            (Some(Slot { users: 1, orphaned: false, cancelled: false, playback }), Effect::Spawn),
        (Some(s), Event::Released { own: true }) => {
            let users = s.users.saturating_sub(1);
            if users == 0 {
                (Some(Slot { users, orphaned: true, ..s }), if s.orphaned { Effect::None } else { Effect::Orphaned })
            } else { (Some(Slot { users, ..s }), Effect::None) }
        }
        (_, Event::Released { .. }) => (slot, Effect::None),
        (Some(s), Event::Sweep { keep, retain }) => {
            if s.orphaned && s.users == 0 && !keep && !s.cancelled && !retain {
                (Some(Slot { cancelled: true, ..s }), Effect::Cancel)
            } else { (slot, Effect::None) }
        }
        (None, Event::Sweep { .. }) => (None, Effect::None),
        (Some(_), Event::Finished { own: true }) => (None, Effect::Remove),
        (_, Event::Finished { .. }) => (slot, Effect::None),
    }
}

#[cfg(test)]
pub mod tests {
    //! Explicit-state model of one key: the slot, the transfer threads that
    //! are still running and the holds readers have, over every interleaving.
    use super::*;
    use crate::model::explore;

    /// A transfer thread: its identity and whether its cancel flag is set.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    struct Thread { id: u8, cancelled: bool }

    #[derive(Debug, Clone, PartialEq, Eq, Hash)]
    struct Key {
        /// The slot and the identity of its transfer.
        slot: Option<(u8, Slot)>,
        threads: Vec<Thread>,
        /// Identities of the transfers readers hold.
        holds: Vec<u8>,
        next_id: u8,
        /// A complete cached copy exists (a finished transfer left one).
        cached: bool,
    }

    const MAX_HOLDS: usize = 3;
    const MAX_TRANSFERS: u8 = 4;

    fn successors(k: &Key) -> Vec<Key> {
        let mut out = Vec::new();
        let slot = k.slot.map(|(_, s)| s);
        // A reader opens the song.
        if k.holds.len() < MAX_HOLDS {
            for playback in [false, true] {
                let (next, effect) = transition(slot, Event::Open { playback });
                let mut n = k.clone();
                match effect {
                    Effect::Join { .. } => {
                        let id = k.slot.unwrap().0;
                        n.slot = Some((id, next.unwrap()));
                        n.holds.push(id);
                        out.push(n);
                    }
                    Effect::StartNew if k.cached => out.push(n), // served from the cache: no hold
                    Effect::StartNew if k.next_id < MAX_TRANSFERS => {
                        let (started, effect) = transition(slot, Event::Started { playback });
                        assert_eq!(effect, Effect::Spawn);
                        n.slot = Some((k.next_id, started.unwrap()));
                        n.threads.push(Thread { id: k.next_id, cancelled: false });
                        n.holds.push(k.next_id);
                        n.next_id += 1;
                        out.push(n);
                    }
                    Effect::StartNew => {}
                    other => panic!("{other:?}"),
                }
            }
        }
        // A hold drops.
        for (i, held) in k.holds.iter().enumerate() {
            let own = k.slot.is_some_and(|(id, _)| id == *held);
            let (next, _) = transition(slot, Event::Released { own });
            let mut n = k.clone();
            n.holds.remove(i);
            n.slot = k.slot.and_then(|(id, _)| next.map(|s| (id, s)));
            out.push(n);
        }
        // A sweep.
        for keep in [false, true] { for retain in [false, true] {
            let (next, effect) = transition(slot, Event::Sweep { keep, retain });
            let mut n = k.clone();
            if effect == Effect::Cancel {
                let id = k.slot.unwrap().0;
                for t in &mut n.threads { if t.id == id { t.cancelled = true; } }
            }
            n.slot = k.slot.and_then(|(id, _)| next.map(|s| (id, s)));
            out.push(n);
        } }
        // A thread ends: done, failed, or stopped by its cancel flag.
        for (i, thread) in k.threads.iter().enumerate() {
            let own = k.slot.is_some_and(|(id, _)| id == thread.id);
            let (next, effect) = transition(slot, Event::Finished { own });
            for completed in [false, true] {
                if completed && thread.cancelled { continue; }
                let mut n = k.clone();
                n.threads.remove(i);
                n.slot = if effect == Effect::Remove { None } else { k.slot.and_then(|(id, _)| next.map(|s| (id, s))) };
                n.cached |= completed;
                out.push(n);
            }
        }
        out
    }

    #[test]
    fn one_key_never_has_two_live_transfers_and_no_reader_holds_a_cancelled_one() {
        let initial = Key { slot: None, threads: Vec::new(), holds: Vec::new(), next_id: 0, cached: false };
        let states = explore([initial], 200_000, successors, |k| {
            let live = k.threads.iter().filter(|t| !t.cancelled).count();
            assert!(live <= 1, "two live transfers: {k:?}");
            if let Some((id, slot)) = k.slot {
                assert!(slot.valid(), "{k:?}");
                assert_eq!(slot.users, k.holds.iter().filter(|h| **h == id).count(), "users miscounted: {k:?}");
                let thread = k.threads.iter().find(|t| t.id == id).expect("a slot's thread is running");
                assert_eq!(thread.cancelled, slot.cancelled, "{k:?}");
            }
            for held in &k.holds {
                assert!(!k.threads.iter().any(|t| t.id == *held && t.cancelled), "a reader holds a cancelled transfer: {k:?}");
            }
            // No leak: with every thread ended, no slot remains.
            if k.threads.is_empty() { assert!(k.slot.is_none(), "leaked slot: {k:?}"); }
        });
        assert!(states > 100, "{states}");
    }

    #[test]
    fn step_properties_hold_over_small_slots() {
        let mut slots = vec![None];
        for users in 0..4 { for cancelled in [false, true] { for playback in [false, true] {
            let s = Slot { users, orphaned: users == 0, cancelled, playback };
            if s.valid() { slots.push(Some(s)); }
        } } }
        let mut events = Vec::new();
        for a in [false, true] {
            events.extend([Event::Open { playback: a }, Event::Started { playback: a }, Event::Released { own: a }, Event::Finished { own: a }]);
            for b in [false, true] { events.push(Event::Sweep { keep: a, retain: b }); }
        }
        for slot in &slots { for event in &events {
            let (next, effect) = transition(*slot, *event);
            if let Some(n) = next { assert!(n.valid(), "{slot:?} {event:?}"); }
            if matches!(effect, Effect::Join { .. }) { assert!(!slot.unwrap().cancelled); }
            if effect == Effect::Cancel { assert!(slot.unwrap().users == 0 && !matches!(event, Event::Sweep { keep: true, .. } | Event::Sweep { retain: true, .. })); }
        } }
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    fn any_slot() -> Option<Slot> {
        let slot: Option<Slot> = kani::any();
        if let Some(s) = slot { kani::assume(s.valid()); }
        slot
    }

    /// Unbounded in `users`.
    #[kani::proof]
    fn transitions_preserve_slot_validity() {
        let (next, _) = transition(any_slot(), kani::any());
        if let Some(n) = next { assert!(n.valid()); }
    }

    #[kani::proof]
    fn a_cancelled_transfer_is_never_joined() {
        let slot = any_slot();
        let (next, effect) = transition(slot, kani::any());
        if let Effect::Join { .. } = effect {
            assert!(!slot.unwrap().cancelled && !next.unwrap().cancelled && next.unwrap().users == slot.unwrap().users + 1);
        }
        // Once cancelled, a slot's holds never grow until it is replaced.
        if let (Some(before), Some(after)) = (slot, next) {
            if before.cancelled && after.cancelled { assert!(after.users == 0); }
        }
    }

    #[kani::proof]
    fn only_an_unwanted_incomplete_orphan_is_cancelled() {
        let slot = any_slot();
        let event: Event = kani::any();
        let (next, effect) = transition(slot, event);
        if effect == Effect::Cancel {
            let s = slot.unwrap();
            assert!(s.users == 0 && s.orphaned && !s.cancelled);
            assert!(event == Event::Sweep { keep: false, retain: false });
            assert!(next.unwrap().cancelled);
        }
        // The cancel flag is never cleared; a cancelled slot is only replaced or removed.
        if let (Some(before), Some(after)) = (slot, next) {
            if before.cancelled && !after.cancelled { assert!(effect == Effect::Spawn); }
        }
    }

    #[kani::proof]
    fn a_new_transfer_starts_only_when_none_is_joinable() {
        let slot = any_slot();
        let event: Event = kani::any();
        let (_, effect) = transition(slot, event);
        if effect == Effect::Spawn { assert!(slot.is_none() || slot.unwrap().cancelled); }
        if effect == Effect::Remove { assert!(event == Event::Finished { own: true }); }
    }
}
