#![forbid(unsafe_code)]
//! The player's state machines, each as an explicit state type and a pure
//! `transition(state, event) -> (state, effect)` function.
//!
//! Nothing here performs I/O, reads a clock, allocates or takes a lock. The
//! imperative code in `q2nd` (the player thread, the library, the Wi-Fi
//! worker, the UI loop) builds an event from what it observed, calls the
//! transition, stores the returned state and carries out the returned effect.
//!
//! Each module states its invariants and proves them twice:
//!
//! * `#[cfg(kani)]` harnesses (`cargo kani -p q2-machines`) over symbolic
//!   states and events;
//! * ordinary `#[test]`s that enumerate every reachable state of a finite
//!   abstraction ([`model::explore`]) or every input of a bounded domain and
//!   check the same invariants, so `cargo test` covers them without Kani.
//!
//! docs/state-machines.md lists states, events, transitions, invariants, and
//! which method proves which property with what bound.
//!
//! The Q2 target has no 64-bit atomics; this crate uses no atomics at all.
#![forbid(unsafe_code)]
#![warn(missing_docs)]

pub mod doze;
pub mod doze_cycle;
pub mod handoff;
pub mod model;
pub mod output;
pub mod power;
pub mod queue;
pub mod radio;
pub mod server;
pub mod transfer;
pub mod usb;
