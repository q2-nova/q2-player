//! Wi-Fi doze while cached songs play (POWER-006, the original product "buffer a few songs
//! and then turn off Wi-Fi"). With the screen off and enough complete on-card
//! audio queued, the radio goes down until that audio runs low or the owner
//! needs the network; the prefetch worker then refills while it is up.
//!
//! Product feedback (2026-11-08): power the radio down while at least the next half
//! hour of the queue is playable offline, and power it up for the owner or
//! when more items need caching. A reconnect costs 9–52 J against a 5–16 mW
//! saving (docs/validation/radio-doze.md), so a doze must be long: the
//! threshold is 30 min of complete on-card audio, and the wake margin leaves
//! time to reconnect and download the next song before the local run ends.
//!
//! Product feedback (2026-11-22): "We should definitely doze the radio while paused,
//! but only while paused and screen off idle. If the screen is on, the user
//! may be browsing for new things to play." A pause dozes the radio once the
//! screen has been dark, untouched, for `power.doze_paused_minutes`
//! ([`PAUSED_DELAY_MINUTES`] by default); lighting the screen wakes it.
//!
//! The policy itself is the pure machine `q2_machines::doze` (proved there;
//! docs/state-machines.md); this module converts the UI's observations.
use q2_machines::doze as fsm;
use std::time::Duration;

/// Complete local audio needed ahead before the radio may doze.
#[cfg(test)]
pub const DOZE_AHEAD_MS: u64 = fsm::DOZE_AHEAD_MS;
/// Wake when less than this is left, so the next songs can download in time
/// (reconnect about 10 s, a lossless song at Wi-Fi speed under a minute).
#[cfg(test)]
pub const WAKE_AHEAD_MS: u64 = fsm::WAKE_AHEAD_MS;
/// No doze until the owner has left the device alone this long, so a quick
/// screen-off/on does not pay a reconnect.
#[cfg(test)]
pub const SETTLE: Duration = Duration::from_millis(fsm::SETTLE_MS);
/// How far ahead to look, in songs: enough short songs to reach the doze
/// threshold, bounded so the check stays cheap on long queues.
pub const LOOKAHEAD_SONGS: usize = 60;
/// How often to recompute the local audio ahead while nothing changes.
pub const RECHECK: Duration = Duration::from_millis(fsm::RECHECK_MS);

#[derive(Debug, Clone, Copy, Default)]
pub struct Inputs {
    /// `power.radio_doze` in config.toml.
    pub enabled: bool,
    /// The owner's saved Wi-Fi choice is On and Offline Mode is off.
    pub wifi_wanted: bool,
    /// Associated with an address (doze from a working link only).
    pub connected: bool,
    pub dozing: bool,
    pub playing: bool,
    /// The queue ended or was cleared (a pause is not stopped).
    pub stopped: bool,
    pub screen_on: bool,
    pub idle: Duration,
    /// How long the screen has been off; zero while it is on.
    pub dark: Duration,
    pub ahead_ms: u64,
    /// A fetch-ahead burst is running: never doze under it.
    pub fetching: bool,
    /// Paused-doze delay from `power.doze_paused_minutes`; None when 0 (off).
    pub paused_after: Option<Duration>,
}

/// The default `power.doze_paused_minutes`: one minute of dark screen (the
/// backlight goes off after 30 s untouched, so 90 s after the last input).
/// The key counts whole minutes and 0 turns the paused doze off, so one is
/// the shortest delay it can give. A doze pays a reconnect when the screen
/// next lights: 3–33 s to the link (V0.130 doze log `reconnect_ms`), and
/// tsrsd 0.1.2 reaches Navidrome about 14 s after the wake. The minute skips
/// that wait after a glance or a pocketing that lights the screen again
/// soon, and costs about 2 J of the 35 mW paused saving per pause
/// (docs/validation/power-proxies-2026-11-22.md), less than one reconnect
/// (9–52 J, docs/validation/radio-doze.md).
pub const PAUSED_DELAY_MINUTES: u32 = 1;

/// The paused-doze delay `power.doze_paused_minutes` asks for; None when 0.
pub fn paused_after(minutes: u32) -> Option<Duration> {
    (minutes > 0).then(|| Duration::from_secs(u64::from(minutes) * 60))
}

/// The thresholds and what the policy asks for are the machine's.
pub use fsm::{Effect, Thresholds};

/// Some(true): start dozing. Some(false): wake. None: leave it.
#[cfg(test)]
pub fn decide(i: Inputs) -> Option<bool> { decide_with(i, Thresholds::default()) }

/// As [`step`], without the wake reason.
#[cfg(test)]
pub fn decide_with(i: Inputs, t: Thresholds) -> Option<bool> {
    match step(i, t) {
        Effect::Doze => Some(true),
        Effect::Wake(_) => Some(false),
        Effect::None => None,
    }
}

/// One tick of the policy: what to do with the radio, and why it wakes.
/// `i.idle` is truncated to whole milliseconds (the settle time is whole
/// seconds, so the comparison is unchanged).
pub fn step(i: Inputs, t: Thresholds) -> Effect {
    fsm::transition(fsm::Inputs {
        enabled: i.enabled, wifi_wanted: i.wifi_wanted, connected: i.connected, dozing: i.dozing, playing: i.playing,
        stopped: i.stopped, screen_on: i.screen_on,
        // The machine's AirPlay input belonged to the deleted stock receiver.
        airplay: false,
        idle_ms: u64::try_from(i.idle.as_millis()).unwrap_or(u64::MAX), ahead_ms: i.ahead_ms, fetching: i.fetching,
        // Dark and untouched for the delay: lighting the screen restarts it,
        // and so does input that leaves it dark (the wheel's volume, Play).
        paused_doze: !i.screen_on && i.paused_after.is_some_and(|after| i.dark.min(i.idle) >= after),
    }, t).1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() -> Inputs {
        Inputs { enabled: true, wifi_wanted: true, connected: true, playing: true, idle: SETTLE, ahead_ms: DOZE_AHEAD_MS, ..Default::default() }
    }

    #[test]
    fn dozes_only_from_a_settled_dark_connected_playing_state_with_audio_ahead() {
        assert_eq!(decide(ready()), Some(true));
        let blockers: [(&str, fn(&mut Inputs)); 7] = [
            ("disabled", |i| i.enabled = false),
            ("wifi off or Offline", |i| i.wifi_wanted = false),
            ("not connected", |i| i.connected = false),
            ("paused or stopped", |i| i.playing = false),
            ("screen on", |i| i.screen_on = true),
            ("recent input", |i| i.idle = SETTLE - Duration::from_millis(1)),
            ("too little local audio", |i| i.ahead_ms = DOZE_AHEAD_MS - 1),
        ];
        for (name, block) in blockers {
            let mut i = ready();
            block(&mut i);
            assert_eq!(decide(i), None, "{name}");
        }
    }

    #[test]
    fn never_dozes_under_a_fetch_and_after_fetch_thresholds_follow_the_margin() {
        assert_eq!(decide(Inputs { fetching: true, ..ready() }), None);
        // The wake adds the reconnect allowance to the fetch margin.
        assert_eq!(Thresholds::after_fetch(3), Thresholds { doze_ms: 8 * 60_000, wake_ms: 4 * 60_000 });
        assert_eq!(Thresholds::after_fetch(0).wake_ms, 2 * 60_000, "a zero margin still wakes before the end");
    }

    #[test]
    fn configured_thresholds_apply_and_invalid_ones_fall_back() {
        let t = Thresholds::from_minutes(6, 2);
        assert_eq!(t, Thresholds { doze_ms: 360_000, wake_ms: 120_000 });
        assert_eq!(decide_with(Inputs { ahead_ms: 360_000, ..ready() }, t), Some(true));
        assert_eq!(decide_with(Inputs { ahead_ms: 359_999, ..ready() }, t), None);
        let dozing = Inputs { dozing: true, ahead_ms: 120_000, ..ready() };
        assert_eq!(decide_with(dozing, t), None);
        assert_eq!(decide_with(Inputs { ahead_ms: 119_999, ..dozing }, t), Some(false));
        assert_eq!(Thresholds::from_minutes(30, 5), Thresholds::default());
        for (d, w) in [(5, 5), (5, 0), (0, 0), (4, 6)] {
            assert_eq!(Thresholds::from_minutes(d, w), Thresholds::default(), "{d}/{w}");
        }
    }

    #[test]
    fn wakes_for_low_audio_screen_or_a_changed_choice_with_hysteresis() {
        let dozing = Inputs { dozing: true, ..ready() };
        // Between the thresholds a doze continues; pause keeps it too.
        assert_eq!(decide(Inputs { ahead_ms: WAKE_AHEAD_MS, ..dozing }), None);
        assert_eq!(decide(Inputs { playing: false, connected: false, ..dozing }), None);
        let wakers: [(&str, fn(&mut Inputs)); 5] = [
            ("queue ended", |i| { i.playing = false; i.stopped = true; }),
            ("audio running low", |i| i.ahead_ms = WAKE_AHEAD_MS - 1),
            ("screen on", |i| i.screen_on = true),
            ("disabled", |i| i.enabled = false),
            ("wifi off or Offline", |i| i.wifi_wanted = false),
        ];
        for (name, wake) in wakers {
            let mut i = dozing;
            wake(&mut i);
            assert_eq!(decide(i), Some(false), "{name}");
        }
        // Awake again with the audio between thresholds: no immediate re-doze.
        assert_eq!(decide(Inputs { ahead_ms: WAKE_AHEAD_MS, ..ready() }), None);
    }

    #[test]
    fn a_paused_doze_follows_the_configured_minutes_of_dark_screen() {
        assert_eq!(paused_after(0), None, "0 keeps the playing-only policy");
        assert_eq!(paused_after(2), Some(Duration::from_secs(120)));
        assert_eq!(paused_after(crate::config::Config::default().power.doze_paused_minutes), Some(Duration::from_secs(60)), "on by default");
        let paused = Inputs { playing: false, ahead_ms: WAKE_AHEAD_MS, idle: Duration::from_secs(150), dark: Duration::from_secs(120),
            paused_after: paused_after(2), ..ready() };
        assert_eq!(decide(paused), Some(true));
        assert_eq!(decide(Inputs { paused_after: None, ..paused }), None, "0: off");
        assert_eq!(decide(Inputs { dark: Duration::from_secs(119), ..paused }), None, "not dark long enough");
        assert_eq!(decide(Inputs { idle: Duration::from_secs(119), ..paused }), None, "input in the dark restarts the delay");
        assert_eq!(decide(Inputs { ahead_ms: WAKE_AHEAD_MS - 1, ..paused }), None, "a resume would stream");
        assert_eq!(decide(Inputs { stopped: true, ..paused }), None);
        // Once down, the pause keeps it; the screen wakes it.
        let dozing = Inputs { dozing: true, ..paused };
        assert_eq!(decide(dozing), None);
        assert_eq!(decide(Inputs { screen_on: true, ..dozing }), Some(false));
    }

    #[test]
    fn paused_dozes_only_dark_and_playing_keeps_its_rules() {
        let after = paused_after(PAUSED_DELAY_MINUTES);
        let long = Duration::from_secs(3600);
        let paused = Inputs { playing: false, ahead_ms: WAKE_AHEAD_MS, idle: long, dark: long, paused_after: after, ..ready() };
        // Paused and dark: down once the screen has been off for the delay.
        assert_eq!(decide(Inputs { dark: Duration::from_secs(59), idle: Duration::from_secs(89), ..paused }), None);
        assert_eq!(decide(Inputs { dark: Duration::from_secs(60), idle: Duration::from_secs(90), ..paused }), Some(true));
        // Paused and lit, however long untouched (a lit screen has no dark time,
        // but the policy does not rely on that).
        assert_eq!(decide(Inputs { screen_on: true, ..paused }), None);
        assert_eq!(decide(Inputs { screen_on: true, dark: Duration::ZERO, ..paused }), None);
        // Paused and dozed, then the screen comes on: up.
        assert_eq!(step(Inputs { dozing: true, screen_on: true, dark: Duration::ZERO, idle: Duration::ZERO, ..paused }, Thresholds::default()),
            Effect::Wake(fsm::WakeReason::Screen));
        // Stopped (queue ended or cleared): never down, and a doze ends.
        let stopped = Inputs { stopped: true, ..paused };
        assert_eq!(decide(stopped), None);
        assert_eq!(decide(Inputs { ahead_ms: u64::MAX, ..stopped }), None);
        assert_eq!(decide(Inputs { dozing: true, ..stopped }), Some(false));
        // Playing: the paused delay and the dark time change nothing.
        for (paused_after, dark) in [(None, Duration::ZERO), (after, Duration::ZERO), (after, long)] {
            for ahead_ms in [0, WAKE_AHEAD_MS, DOZE_AHEAD_MS - 1, DOZE_AHEAD_MS] {
                for dozing in [false, true] {
                    let base = Inputs { ahead_ms, dozing, ..ready() };
                    assert_eq!(decide(Inputs { paused_after, dark, ..base }), decide(base), "{paused_after:?} {dark:?} {ahead_ms} {dozing}");
                }
            }
        }
    }
}
