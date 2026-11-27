//! The Wi-Fi doze policy (`ui/doze.rs`, POWER-006): with the screen off and
//! enough complete on-card audio queued the radio goes down, and comes back
//! when that audio runs low or the owner needs the network.
//!
//! The state is whether the radio is dozing; the UI loop samples the inputs
//! on its tick and applies the returned effect.
//!
//! # Invariants
//!
//! For all inputs and all thresholds built by [`Thresholds::after_fetch`],
//! [`Thresholds::from_minutes`] or `Default`:
//!
//! * a doze never starts while a fetch-ahead burst runs, with the screen on,
//!   under AirPlay, unconnected, disabled, with Wi-Fi off or Offline, or
//!   before the owner has been idle for [`SETTLE_MS`];
//! * while playing, a doze needs at least the doze threshold of local audio
//!   ahead; while paused, only with `paused_doze` (the screen dark and
//!   untouched for `power.doze_paused_minutes`), never with the queue
//!   stopped, and with at least the wake margin ahead, so a resume plays from
//!   the card and the doze is not woken for low audio on the next tick;
//! * a dozing radio is woken on the first tick that sees the screen on,
//!   AirPlay, the queue stopped, doze disabled, Wi-Fi off/Offline, or less
//!   local audio ahead than the wake margin;
//! * the wake margin is at least one minute and below the doze threshold, so
//!   a wake always comes before the cached audio runs out (the tick samples
//!   at most [`RECHECK_MS`] apart) and a doze that just started is not woken
//!   by the same inputs (hysteresis);
//! * the policy alone never flaps: after `Doze`, the same inputs give `None`.

/// Complete local audio needed ahead before the radio may doze.
pub const DOZE_AHEAD_MS: u64 = 30 * 60_000;
/// Wake when less than this is left, so the next songs can download in time.
pub const WAKE_AHEAD_MS: u64 = 5 * 60_000;
/// No doze until the owner has left the device alone this long.
pub const SETTLE_MS: u64 = 30_000;
/// How often the local audio ahead is recomputed while nothing changes.
pub const RECHECK_MS: u64 = 10_000;
/// With fetch by duration, the least doze worth a reconnect beyond the wake
/// margin.
pub const MIN_DOZE_MS: u64 = 4 * 60_000;
/// With fetch by duration, the radio wakes this much earlier than the fetch
/// margin, and fetch-ahead starts this much earlier too (library
/// `fetch_low_ms`), so the next songs land with the margin still left. In
/// production telemetry a wake took 18 s to reach Navidrome and 37 s to a
/// settled link, then fetch-ahead waits its 8 s settle; the song after a wake
/// still streamed at the boundary 64 s later.
pub const RECONNECT_MS: u64 = 60_000;

/// What the UI loop observed this tick.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub struct Inputs {
    /// `power.radio_doze` in config.toml.
    pub enabled: bool,
    /// The owner's saved Wi-Fi choice is On and Offline Mode is off.
    pub wifi_wanted: bool,
    /// Associated with an address (doze from a working link only).
    pub connected: bool,
    /// The radio is dozing now.
    pub dozing: bool,
    /// Playing or buffering.
    pub playing: bool,
    /// The queue ended or was cleared (a pause is not stopped).
    pub stopped: bool,
    /// The screen is on.
    pub screen_on: bool,
    /// The AirPlay receiver is active.
    pub airplay: bool,
    /// Time since the owner's last input.
    pub idle_ms: u64,
    /// Complete on-card audio ahead of the playing position.
    pub ahead_ms: u64,
    /// A fetch-ahead burst is running.
    pub fetching: bool,
    /// A doze while paused is allowed: the screen has been off, with no
    /// input, for `power.doze_paused_minutes` (default 1; false when 0).
    pub paused_doze: bool,
}

/// The local audio needed ahead to doze and the margin that wakes the radio.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Thresholds {
    /// Doze with at least this much local audio ahead.
    pub doze_ms: u64,
    /// Wake with less than this ahead.
    pub wake_ms: u64,
}

impl Default for Thresholds {
    fn default() -> Self { Thresholds { doze_ms: DOZE_AHEAD_MS, wake_ms: WAKE_AHEAD_MS } }
}

impl Thresholds {
    /// Fetch by duration: doze once the fetch ends with at least the wake
    /// margin plus [`MIN_DOZE_MS`] cached, wake at the margin plus
    /// [`RECONNECT_MS`]. A margin of zero minutes is taken as one.
    pub fn after_fetch(wake_minutes: u32) -> Self {
        let wake_ms = u64::from(wake_minutes.max(1)) * 60_000 + RECONNECT_MS;
        Thresholds { doze_ms: wake_ms + MIN_DOZE_MS, wake_ms }
    }
    /// From config minutes; the defaults unless the wake margin is positive
    /// and below the doze threshold (hysteresis must remain).
    pub fn from_minutes(doze: u32, wake: u32) -> Self {
        if wake == 0 || wake >= doze { return Self::default(); }
        Thresholds { doze_ms: u64::from(doze) * 60_000, wake_ms: u64::from(wake) * 60_000 }
    }
    /// The property every constructor establishes: a margin of at least a
    /// minute, below the doze threshold.
    pub fn valid(self) -> bool { self.wake_ms >= 60_000 && self.wake_ms < self.doze_ms }
}

/// Why a doze ended, in the order the causes are reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WakeReason {
    /// Doze was turned off in the configuration.
    Disabled,
    /// Wi-Fi was turned off or Offline Mode on.
    WifiOff,
    /// The screen came on.
    Screen,
    /// AirPlay started.
    AirPlay,
    /// The queue ended or was cleared.
    Stopped,
    /// The local audio ahead fell below the wake margin.
    Low,
}

/// What the UI loop does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Effect {
    /// Leave the radio as it is.
    None,
    /// Take the radio down.
    Doze,
    /// Bring the radio up.
    Wake(WakeReason),
}

/// The policy's response to one tick's inputs; `i.dozing` is the state.
/// Returns the next state (dozing or not) and the effect.
pub fn transition(i: Inputs, t: Thresholds) -> (bool, Effect) {
    if i.dozing {
        let reason = if !i.enabled { WakeReason::Disabled }
            else if !i.wifi_wanted { WakeReason::WifiOff }
            else if i.screen_on { WakeReason::Screen }
            else if i.airplay { WakeReason::AirPlay }
            else if i.stopped { WakeReason::Stopped }
            else if i.ahead_ms < t.wake_ms { WakeReason::Low }
            else { return (true, Effect::None) };
        return (false, Effect::Wake(reason));
    }
    let settled = i.enabled && i.wifi_wanted && i.connected && !i.screen_on && !i.airplay
        && !i.fetching && i.idle_ms >= SETTLE_MS;
    let playing = i.playing && i.ahead_ms >= t.doze_ms;
    // Paused: nothing is consumed, so the wake margin keeps a resume local
    // and the doze from waking for low audio (hysteresis holds).
    let paused = i.paused_doze && !i.playing && !i.stopped && i.ahead_ms >= t.wake_ms;
    let doze = settled && (playing || paused);
    if doze { (true, Effect::Doze) } else { (false, Effect::None) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exhaustive over the ten flags and boundary values of the three
    /// quantities, for boundary thresholds of each constructor.
    #[test]
    fn the_policy_table_holds_for_every_flag_combination_and_boundary_value() {
        let thresholds = [Thresholds::default(), Thresholds::after_fetch(0), Thresholds::after_fetch(1), Thresholds::after_fetch(u32::MAX),
            Thresholds::from_minutes(2, 1), Thresholds::from_minutes(u32::MAX, u32::MAX - 1), Thresholds::from_minutes(5, 5), Thresholds::from_minutes(0, 0)];
        let mut cases = 0u32;
        for t in thresholds {
            assert!(t.valid(), "{t:?}");
            let aheads = [0, 1, t.wake_ms - 1, t.wake_ms, t.doze_ms - 1, t.doze_ms, u64::MAX];
            let idles = [0, SETTLE_MS - 1, SETTLE_MS, u64::MAX];
            for bits in 0u32..1024 { for ahead_ms in aheads { for idle_ms in idles {
                let f = |n: u32| bits & (1 << n) != 0;
                let i = Inputs { enabled: f(0), wifi_wanted: f(1), connected: f(2), dozing: f(3), playing: f(4), stopped: f(5),
                    screen_on: f(6), airplay: f(7), fetching: f(8), paused_doze: f(9), idle_ms, ahead_ms };
                let (dozing, effect) = transition(i, t);
                cases += 1;
                match effect {
                    Effect::Doze => {
                        assert!(!i.dozing && dozing);
                        assert!(i.enabled && i.wifi_wanted && i.connected && !i.screen_on && !i.airplay
                            && !i.fetching && i.idle_ms >= SETTLE_MS, "{i:?}");
                        assert!((i.playing && i.ahead_ms >= t.doze_ms)
                            || (i.paused_doze && !i.playing && !i.stopped && i.ahead_ms >= t.wake_ms), "{i:?}");
                        // No flapping: the same inputs, now dozing, do not wake
                        // unless the queue is reported stopped while playing.
                        if !i.stopped { assert_eq!(transition(Inputs { dozing: true, ..i }, t), (true, Effect::None)); }
                    }
                    Effect::Wake(_) => assert!(i.dozing && !dozing),
                    Effect::None => assert_eq!(dozing, i.dozing),
                }
                // Without the owner's paused doze, the policy is the playing-only one.
                if !i.paused_doze && !i.dozing && effect == Effect::Doze { assert!(i.playing); }
                if i.dozing && (!i.enabled || !i.wifi_wanted || i.screen_on || i.airplay || i.stopped || i.ahead_ms < t.wake_ms) {
                    assert!(matches!(effect, Effect::Wake(_)), "{i:?}");
                }
            } } }
        }
        assert_eq!(cases, 8 * 1024 * 7 * 4);
    }

    #[test]
    fn a_paused_doze_needs_the_owners_choice_and_the_wake_margin() {
        let t = Thresholds::after_fetch(3);
        let paused = Inputs { enabled: true, wifi_wanted: true, connected: true, paused_doze: true,
            idle_ms: SETTLE_MS, ahead_ms: t.wake_ms, ..Default::default() };
        assert_eq!(transition(paused, t), (true, Effect::Doze));
        assert_eq!(transition(Inputs { paused_doze: false, ..paused }, t), (false, Effect::None), "not chosen");
        assert_eq!(transition(Inputs { ahead_ms: t.wake_ms - 1, ..paused }, t), (false, Effect::None), "resume would stream");
        assert_eq!(transition(Inputs { stopped: true, ..paused }, t), (false, Effect::None), "queue ended");
        assert_eq!(transition(Inputs { screen_on: true, ..paused }, t), (false, Effect::None));
        assert_eq!(transition(Inputs { fetching: true, ..paused }, t), (false, Effect::None));
        // Playing still needs the doze threshold, whatever the paused choice.
        assert_eq!(transition(Inputs { playing: true, ..paused }, t), (false, Effect::None));
        // Dozing while paused at the margin stays down; resume and play
        // below the margin wakes as before.
        let dozing = Inputs { dozing: true, ..paused };
        assert_eq!(transition(dozing, t), (true, Effect::None));
        assert_eq!(transition(Inputs { playing: true, ahead_ms: t.wake_ms - 1, ..dozing }, t), (false, Effect::Wake(WakeReason::Low)));
    }

    #[test]
    fn wake_reasons_are_reported_in_priority_order() {
        let dozing = Inputs { enabled: true, wifi_wanted: true, dozing: true, playing: true, ahead_ms: DOZE_AHEAD_MS, ..Default::default() };
        let t = Thresholds::default();
        assert_eq!(transition(dozing, t).1, Effect::None);
        assert_eq!(transition(Inputs { ahead_ms: WAKE_AHEAD_MS - 1, ..dozing }, t).1, Effect::Wake(WakeReason::Low));
        assert_eq!(transition(Inputs { stopped: true, ahead_ms: 0, ..dozing }, t).1, Effect::Wake(WakeReason::Stopped));
        assert_eq!(transition(Inputs { airplay: true, stopped: true, ..dozing }, t).1, Effect::Wake(WakeReason::AirPlay));
        assert_eq!(transition(Inputs { screen_on: true, airplay: true, ..dozing }, t).1, Effect::Wake(WakeReason::Screen));
        assert_eq!(transition(Inputs { wifi_wanted: false, screen_on: true, ..dozing }, t).1, Effect::Wake(WakeReason::WifiOff));
        assert_eq!(transition(Inputs { enabled: false, wifi_wanted: false, ..dozing }, t).1, Effect::Wake(WakeReason::Disabled));
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// Any thresholds with the property the constructors establish. The
    /// policy harnesses use these so no multiplication reaches the solver.
    fn any_thresholds() -> Thresholds {
        let t = Thresholds { doze_ms: kani::any(), wake_ms: kani::any() };
        kani::assume(t.valid());
        t
    }

    /// Bounded: minutes up to 65 535 (45 days); the constructors take `u32`
    /// and the `u32` boundaries are covered by the exhaustive test above.
    #[kani::proof]
    fn every_constructor_keeps_a_wake_margin_of_a_minute_below_the_doze_threshold() {
        let (a, b): (u16, u16) = (kani::any(), kani::any());
        let t = if kani::any() { Thresholds::after_fetch(u32::from(a)) } else { Thresholds::from_minutes(u32::from(a), u32::from(b)) };
        assert!(t.valid());
        assert!(Thresholds::default().valid());
    }

    /// Unbounded: all flags, all `u64` idle times, audio ahead and valid thresholds.
    #[kani::proof]
    fn a_doze_never_starts_under_a_fetch_or_while_the_owner_needs_the_radio() {
        let i: Inputs = kani::any();
        let t = any_thresholds();
        let (dozing, effect) = transition(i, t);
        if effect == Effect::Doze {
            assert!(dozing && !i.dozing);
            assert!(!i.fetching && !i.screen_on && !i.airplay && i.connected && i.enabled && i.wifi_wanted);
            assert!(i.idle_ms >= SETTLE_MS);
            assert!((i.playing && i.ahead_ms >= t.doze_ms) || (i.paused_doze && !i.playing && !i.stopped && i.ahead_ms >= t.wake_ms));
        }
    }

    #[kani::proof]
    fn a_dozing_radio_wakes_before_the_audio_runs_out_and_for_the_owner() {
        let i: Inputs = kani::any();
        kani::assume(i.dozing);
        let t = any_thresholds();
        let (dozing, effect) = transition(i, t);
        let must_wake = !i.enabled || !i.wifi_wanted || i.screen_on || i.airplay || i.stopped || i.ahead_ms < t.wake_ms;
        assert!(matches!(effect, Effect::Wake(_)) == must_wake);
        assert!(dozing == !must_wake);
        // Still dozing implies at least the wake margin (a minute or more,
        // several rechecks) of audio remains.
        if dozing { assert!(i.ahead_ms >= 60_000 && i.ahead_ms > RECHECK_MS); }
    }

    #[kani::proof]
    fn a_doze_that_just_started_is_not_woken_by_the_same_inputs() {
        let i: Inputs = kani::any();
        kani::assume(!i.stopped);
        let t = any_thresholds();
        if transition(i, t).1 == Effect::Doze {
            assert!(transition(Inputs { dozing: true, ..i }, t) == (true, Effect::None));
        }
    }
}
