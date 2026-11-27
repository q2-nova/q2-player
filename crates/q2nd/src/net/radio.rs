//! The radio's intent and its single owner. Offline and doze are temporary
//! effective requests, never a replacement for the saved Wi-Fi preference.
//! Doze is the automatic radio-down while cached songs play with the screen
//! off; any explicit Wi-Fi or Offline choice ends it.
//!
//! The intent and its transitions are `q2_machines::radio` (proved there;
//! docs/state-machines.md). [`Radio`] holds the intent and is the only caller
//! of `transition`: the monitor task owns one (owner.rs), or the handle does
//! when Wi-Fi is simulated. After every transition the intent is copied to
//! [`Shared`], which the UI and running operations read.
use super::WifiState;
use q2_machines::radio::{self as fsm, Effect, Event};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

/// The radio's intent as the Wi-Fi code reads it.
#[derive(Clone, Copy)]
pub(super) struct Intent {
    /// Identifies the request an operation works for.
    pub generation: u64,
    /// The owner's saved Wi-Fi choice.
    pub requested: bool,
    /// Offline Mode suppresses the radio.
    pub offline: bool,
    /// The automatic doze suppresses the radio.
    pub doze: bool,
    /// The last failure's notice, until the next request.
    pub error: Option<&'static str>,
}
impl Intent {
    /// The radio should be up.
    pub fn enabled(self) -> bool { self.requested && !self.offline && !self.doze }
}
impl From<fsm::Intent> for Intent {
    fn from(i: fsm::Intent) -> Self {
        Intent { generation: i.generation, requested: i.requested, offline: i.offline, doze: i.doze, error: i.error.map(fsm::Fault::message) }
    }
}

/// What the UI and running operations read. Written by the owner only; the
/// locks are held for a copy and never across an `.await`.
pub(super) struct Shared {
    intent: Mutex<fsm::Intent>,
    pub state: Mutex<WifiState>,
    /// The screen is on: status polls are frequent.
    visible: AtomicBool,
    /// A helper `down` succeeded and no `up` has succeeded since: the chip is off.
    powered_down: AtomicBool,
    /// Helper `up`s that powered the chip on after a `down` (see [`Shared::power_ups`]).
    power_ups: AtomicU32,
}
impl Shared {
    pub fn new(requested: bool, state: WifiState) -> Arc<Shared> {
        Arc::new(Shared { intent: Mutex::new(fsm::Intent::new(requested)), state: Mutex::new(state), visible: AtomicBool::new(true),
            powered_down: AtomicBool::new(false), power_ups: AtomicU32::new(0) })
    }
    /// A helper run succeeded: `up` powers the combo chip on, `down` off (q2-network/wifi).
    pub fn helper_succeeded(&self, up: bool) {
        if !up { self.powered_down.store(true, Ordering::Release); }
        else if self.powered_down.swap(false, Ordering::AcqRel) { self.power_ups.fetch_add(1, Ordering::AcqRel); }
    }
    /// How many times the chip has powered up again after a helper `down` (a doze
    /// wake, Wi-Fi Off then On, Offline Mode ended). The Bluetooth side, which shares
    /// the chip, starts over after each (bluetooth::Bluetooth::wifi_powered_up).
    pub fn power_ups(&self) -> u32 { self.power_ups.load(Ordering::Acquire) }
    pub fn intent(&self) -> Intent { (*self.intent.lock().unwrap()).into() }
    /// `generation` is still the latest request.
    pub fn current(&self, generation: u64) -> bool { self.intent.lock().unwrap().generation == generation }
    /// A fault of the latest request is waiting for the UI.
    pub fn fault_pending(&self) -> bool { self.intent.lock().unwrap().fault }
    pub fn visible(&self) -> bool { self.visible.load(Ordering::Relaxed) }
    /// Returns whether the value changed.
    pub fn set_visible(&self, on: bool) -> bool { self.visible.swap(on, Ordering::Relaxed) != on }
}

/// What follows the helper run of a radio request.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(super) enum AfterHelper {
    /// Nothing more.
    Done,
    /// The radio came up for the current request: synchronize the profiles.
    Configure,
    /// A radio-down for Offline Mode or a doze failed and was rolled back to
    /// a radio that should be up: bring it up again.
    RestoreUp,
}

pub(super) struct Radio {
    intent: fsm::Intent,
    shared: Arc<Shared>,
}
impl Radio {
    /// Takes ownership of the intent in `shared`.
    pub fn new(shared: Arc<Shared>) -> Self {
        let intent = *shared.intent.lock().unwrap();
        Self { intent, shared }
    }
    /// Applies one transition and publishes the result.
    fn step(&mut self, event: Event) -> (fsm::Intent, Effect) {
        let (next, effect) = fsm::transition(self.intent, event).expect("Wi-Fi generation exhausted");
        self.intent = next;
        *self.shared.intent.lock().unwrap() = next;
        (next, effect)
    }
    pub fn intent(&self) -> Intent { self.intent.into() }
    /// An explicit Wi-Fi or Offline choice: a new generation, any doze ended.
    pub fn request(&mut self, requested: bool, offline: bool) -> Intent {
        self.step(Event::Request { requested, offline }).0.into()
    }
    /// Starts or ends doze. Only a saved-on, non-Offline radio dozes; returns
    /// None (no new operation) when nothing would change.
    pub fn doze(&mut self, on: bool) -> Option<Intent> {
        let (next, effect) = self.step(Event::Doze(on));
        (effect == Effect::Apply).then(|| next.into())
    }
    pub fn current(&self, generation: u64) -> bool { self.intent.generation == generation }
    /// Runs `f` if `generation` is current and the radio is meant to be up;
    /// false (and `f` not run) otherwise. The owner is the only writer of
    /// the intent, so nothing can change it while `f` runs.
    pub fn publish(&self, generation: u64, f: impl FnOnce()) -> bool {
        if self.intent.generation != generation || !self.intent.enabled() { return false; }
        f();
        true
    }
    /// Failure is generation-bound. A failed Offline down is rolled back; a
    /// newer explicit choice must never be undone by an old helper completion.
    /// A failed automatic doze is rolled back silently.
    pub fn failed(&mut self, generation: u64) -> Option<Intent> {
        let (next, effect) = self.step(Event::Failed(generation));
        (effect == Effect::RolledBack).then(|| next.into())
    }
    pub fn restore_failed(&mut self, generation: u64) {
        self.step(Event::RestoreFailed(generation));
    }
    /// The unseen fault of the current request: whether Offline Mode is
    /// still on, and the notice. Each fault is returned once.
    pub fn take_fault(&mut self) -> Option<(bool, &'static str)> {
        match self.step(Event::TakeFault).1 {
            Effect::Show { offline, fault } => Some((offline, fault.message())),
            _ => None,
        }
    }
    /// The helper for `intent` (up when it is enabled, else down) returned
    /// `ok`. Only the newest request may roll back or go on to the profiles.
    pub fn helper_finished(&mut self, intent: Intent, ok: bool) -> AfterHelper {
        if ok {
            return if intent.enabled() && self.current(intent.generation) { AfterHelper::Configure } else { AfterHelper::Done };
        }
        match self.failed(intent.generation) {
            Some(rolled_back) if (intent.offline || intent.doze) && rolled_back.enabled() && self.current(intent.generation) => AfterHelper::RestoreUp,
            _ => AfterHelper::Done,
        }
    }
    /// The helper run asked for by [`AfterHelper::RestoreUp`] returned `ok`.
    pub fn restore_finished(&mut self, generation: u64, ok: bool) {
        if !ok { self.restore_failed(generation); }
    }
}

/// One radio request carried out in order, with the helper and the profile
/// synchronization supplied by the caller: the decisions of the monitor's
/// `Apply` operation without its I/O.
#[cfg(test)]
pub(super) fn apply_radio(radio: &mut Radio, intent: Intent, mut helper: impl FnMut(bool) -> bool, configure: impl FnOnce()) {
    if !radio.current(intent.generation) { return; }
    let ok = helper(intent.enabled());
    match radio.helper_finished(intent, ok) {
        AfterHelper::Configure => configure(),
        AfterHelper::RestoreUp => { let ok = helper(true); radio.restore_finished(intent.generation, ok); }
        AfterHelper::Done => {}
    }
}
