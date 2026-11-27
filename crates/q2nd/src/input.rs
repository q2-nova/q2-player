#![forbid(unsafe_code)]
//! Turns raw key and wheel input into the events screens handle.

use std::time::{Duration, Instant};

/// The Q2's buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Key {
    /// The wheel's centre (also the power key).
    Center,
    /// Return, above the wheel.
    Back,
    PlayPause,
    /// Side buttons.
    Prev,
    Next,
}

/// What a platform reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Raw {
    Key { key: Key, down: bool },
    /// Cancel held keys after evdev loss, without synthesizing short presses.
    KeysReset,
    /// Jack state may have changed; this is not a user gesture.
    HeadphonesChanged,
    /// Whole wheel ticks, positive clockwise.
    WheelTicks(i32),
    /// Timestamp at acquisition, before any slow UI frame.
    WheelMotion { ticks: i32, at: Instant },
    WheelEnd,
    /// A completed touchscreen tap in landscape UI coordinates, timed at contact start.
    Touch { start_x: u16, start_y: u16, x: u16, y: u16, at: Instant },
    /// The active touchscreen client lost input and shut down.
    TouchUnavailable { at: Instant },
    /// A Bluetooth headset's button press.
    Media(Media),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Media {
    PlayPause,
    Next,
    Prev,
    /// A volume key of the Bluetooth headphones at `addr`: one step of their volume.
    Volume { addr: q2_bt::host::Addr, up: bool },
}

impl Media {
    /// A button of the Bluetooth headphones at `addr`.
    pub fn from_bluetooth(addr: q2_bt::host::Addr, button: crate::bluetooth::Button) -> Self {
        use crate::bluetooth::Button;
        match button {
            Button::Play | Button::Pause | Button::Stop => Media::PlayPause,
            Button::Next => Media::Next,
            Button::Previous => Media::Prev,
            Button::VolumeUp => Media::Volume { addr, up: true },
            Button::VolumeDown => Media::Volume { addr, up: false },
        }
    }
    /// The flight-recorder code.
    pub fn code(self) -> u32 {
        match self { Media::PlayPause => 0, Media::Next => 1, Media::Prev => 2, Media::Volume { up: true, .. } => 3, Media::Volume { up: false, .. } => 4 }
    }
}

/// What screens receive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Wheel ticks, positive clockwise: down a list, volume up.
    Scroll(i32),
    WheelEnd,
    Center,
    CenterLong,
    Back,
    BackLong,
    PlayPause,
    PlayPauseLong,
    Prev,
    Next,
    /// Prev or Next held: seek, repeated while held.
    SeekBack,
    SeekForward,
}

pub const WHEEL_RANGE: i32 = 200;
pub const LONG_PRESS: Duration = Duration::from_millis(600);
const SEEK_REPEAT: Duration = Duration::from_millis(250);

/// The ring's positions to whole ticks, carrying the remainder so jitter at a
/// step's edge doesn't tick back (Rockbox's `wheel_ticks`).
#[derive(Debug, Clone)]
pub struct Wheel {
    anchor: Option<i32>,
    peak: i32,
    direction: i32,
    /// Ring units per tick.
    pub step: i32,
}

impl Wheel {
    pub fn new(step: i32) -> Wheel {
        Wheel { anchor: None, peak: 0, direction: 0, step: step.max(4) }
    }

    /// Ticks for a new position report, positive clockwise. On the Q2,
    /// clockwise lowers the position.
    pub fn sample(&mut self, pos: Option<i32>) -> i32 {
        let Some(x) = pos else {
            self.anchor = None;
            self.direction = 0;
            return 0;
        };
        let Some(anchor) = self.anchor else {
            self.anchor = Some(x);
            self.peak = x;
            return 0;
        };
        let delta = |a: i32, b: i32| {
            let d = a - b;
            if d > WHEEL_RANGE / 2 { d - WHEEL_RANGE }
            else if d < -WHEEL_RANGE / 2 { d + WHEEL_RANGE }
            else { d }
        };
        let wrap = |p: i32| (p - 1).rem_euclid(WHEEL_RANGE) + 1;
        if self.direction != 0 {
            let motion = delta(x, self.peak);
            if motion.signum() == self.direction {
                self.peak = x;
            } else if motion.signum() == -self.direction {
                // Retain the peak until a real reverse tick: jitter cannot
                // repeatedly move the anchor. Reduce only the first reverse
                // tick's dead travel by one quarter; normal spacing stays put.
                let reverse = -self.direction;
                let remaining = self.step + delta(anchor, self.peak) * reverse;
                let threshold = (remaining * 3 + 3) / 4;
                if motion.abs() >= threshold {
                    let ticks = 1 + (motion.abs() - threshold) / self.step;
                    self.anchor = Some(wrap(self.peak + reverse * (threshold + (ticks - 1) * self.step)));
                    self.direction = reverse;
                    self.peak = x;
                    return -reverse * ticks;
                }
            }
        }
        let d = delta(x, anchor);
        let t = d / self.step;
        if t != 0 {
            self.direction = t.signum();
            self.peak = x;
        }
        self.anchor = Some(wrap(anchor + t * self.step));
        -t
    }
}

#[derive(Debug, Clone, Copy)]
struct Held {
    key: Key,
    since: Instant,
    long_sent: bool,
    last_repeat: Instant,
}

/// Short press on release; a long press once held for [`LONG_PRESS`]; Prev
/// and Next held seek instead.
#[derive(Debug, Default)]
pub struct Keys {
    held: Vec<Held>,
}

impl Keys {
    pub fn key(&mut self, key: Key, down: bool, now: Instant, out: &mut Vec<Event>) {
        if down {
            if !self.held.iter().any(|h| h.key == key) {
                self.held.push(Held { key, since: now, long_sent: false, last_repeat: now });
            }
            return;
        }
        // A UI frame may have queued both edges before its next timer tick.
        // Honor the acquisition-time deadline before removing the held key.
        self.tick(now, out);
        let Some(i) = self.held.iter().position(|h| h.key == key) else { return };
        let h = self.held.remove(i);
        if !h.long_sent {
            out.push(match key {
                Key::Center => Event::Center,
                Key::Back => Event::Back,
                Key::PlayPause => Event::PlayPause,
                Key::Prev => Event::Prev,
                Key::Next => Event::Next,
            });
        }
    }

    /// Long presses and seek repeats due by `now`.
    pub fn tick(&mut self, now: Instant, out: &mut Vec<Event>) {
        for h in &mut self.held {
            let held = now.saturating_duration_since(h.since);
            if held < LONG_PRESS {
                continue;
            }
            match h.key {
                Key::Prev | Key::Next => {
                    if !h.long_sent || now.saturating_duration_since(h.last_repeat) >= SEEK_REPEAT {
                        h.long_sent = true;
                        h.last_repeat = now;
                        out.push(if h.key == Key::Prev { Event::SeekBack } else { Event::SeekForward });
                    }
                }
                _ if !h.long_sent => {
                    h.long_sent = true;
                    out.push(match h.key {
                        Key::Center => Event::CenterLong,
                        Key::Back => Event::BackLong,
                        _ => Event::PlayPauseLong,
                    });
                }
                _ => {}
            }
        }
    }

    /// When the next long press or repeat could fire, for the main loop's
    /// sleep.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.held
            .iter()
            .filter_map(|h| {
                if !h.long_sent { Some(h.since + LONG_PRESS) }
                else if matches!(h.key, Key::Prev | Key::Next) { Some(h.last_repeat + SEEK_REPEAT) }
                else { None }
            })
            .min()
    }

    pub fn cancel(&mut self) { self.held.clear(); }

    pub fn any_held(&self) -> bool {
        !self.held.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wheel_ticks_carry_and_wrap() {
        let mut w = Wheel::new(20);
        assert_eq!(w.sample(Some(100)), 0);
        assert_eq!(w.sample(Some(90)), 0, "half a step");
        assert_eq!(w.sample(Some(80)), 1, "clockwise lowers the position");
        assert_eq!(w.sample(Some(41)), 1, "remainder carries");
        assert_eq!(w.sample(Some(40)), 1);
        // Across the 1/200 seam, still clockwise.
        let mut w = Wheel::new(20);
        w.sample(Some(10));
        assert_eq!(w.sample(Some(190)), 1);
        assert_eq!(w.sample(Some(10)), -1, "back the other way");
        assert_eq!(w.sample(None), 0);
        assert_eq!(w.sample(Some(150)), 0, "a new touch starts again");
    }

    #[test]
    fn reversal_reduces_dead_travel_without_changing_forward_spacing() {
        let mut w = Wheel::new(20);
        w.sample(Some(100));
        assert_eq!(w.sample(Some(80)), 1);
        assert_eq!(w.sample(Some(65)), 0); // 15 units of carried travel
        assert_eq!(w.sample(Some(66)), 0);
        assert_eq!(w.sample(Some(65)), 0); // jitter cannot move the anchor
        assert_eq!(w.sample(Some(91)), 0);
        assert_eq!(w.sample(Some(92)), -1); // ceil(35 * .75), formerly 100
        assert_eq!(w.sample(Some(111)), 0);
        assert_eq!(w.sample(Some(112)), -1); // subsequent ticks remain 20 apart
        w.sample(None);
        assert_eq!(w.sample(Some(100)), 0);
        assert_eq!(w.sample(Some(81)), 0);
        assert_eq!(w.sample(Some(80)), 1);
    }

    #[test]
    fn reversal_crosses_the_wheel_seam_in_both_directions() {
        for (start, forward, turn, reverse) in [(10, 190, 185, 4), (190, 10, 15, 196)] {
            let mut w = Wheel::new(20);
            w.sample(Some(start));
            let direction = w.sample(Some(forward));
            assert_ne!(direction, 0);
            assert_eq!(w.sample(Some(turn)), 0);
            assert_eq!(w.sample(Some(reverse)), -direction);
        }
    }

    #[test]
    fn lost_input_cancels_without_click_or_repeat() {
        let now = Instant::now();
        let mut keys = Keys::default();
        let mut events = Vec::new();
        keys.key(Key::Next, true, now, &mut events);
        keys.cancel();
        keys.tick(now + Duration::from_secs(2), &mut events);
        keys.key(Key::Next, false, now + Duration::from_secs(2), &mut events);
        assert!(events.is_empty());
        assert!(!keys.any_held() && keys.next_deadline().is_none());
    }

    #[test]
    fn short_and_long_presses() {
        let t0 = Instant::now();
        let mut k = Keys::default();
        let mut out = Vec::new();
        k.key(Key::Center, true, t0, &mut out);
        k.key(Key::Center, false, t0 + Duration::from_millis(100), &mut out);
        assert_eq!(out, vec![Event::Center]);

        out.clear();
        k.key(Key::Back, true, t0, &mut out);
        k.tick(t0 + Duration::from_millis(700), &mut out);
        k.tick(t0 + Duration::from_millis(900), &mut out);
        k.key(Key::Back, false, t0 + Duration::from_millis(1000), &mut out);
        assert_eq!(out, vec![Event::BackLong], "one long press, no short one");
    }

    #[test]
    fn generated_coalesced_edges_honor_acquisition_deadlines_without_an_intermediate_tick() {
        let start = Instant::now();
        let mut random = fastrand::Rng::with_seed(0x5132_484f);
        for index in 0..4096 {
            let key = [Key::Center, Key::Back, Key::PlayPause, Key::Prev, Key::Next][index % 5];
            let held = Duration::from_millis(random.u64(0..=3000));
            let mut keys = Keys::default();
            let mut events = Vec::new();
            keys.key(key, true, start, &mut events);
            // Both queued edges are delivered together after a slow frame.
            keys.key(key, false, start + held, &mut events);
            let expected = match (key, held >= LONG_PRESS) {
                (Key::Center, false) => Event::Center, (Key::Center, true) => Event::CenterLong,
                (Key::Back, false) => Event::Back, (Key::Back, true) => Event::BackLong,
                (Key::PlayPause, false) => Event::PlayPause, (Key::PlayPause, true) => Event::PlayPauseLong,
                (Key::Prev, false) => Event::Prev, (Key::Prev, true) => Event::SeekBack,
                (Key::Next, false) => Event::Next, (Key::Next, true) => Event::SeekForward,
            };
            assert_eq!(events, [expected], "seed replay {index}; key={key:?}; held={held:?}");
            keys.tick(start + Duration::from_secs(10), &mut events);
            assert_eq!(events.len(), 1, "released keys cannot repeat or emit a short action after a hold");
        }
        let mut keys = Keys::default(); let mut events = Vec::new();
        keys.key(Key::Center, true, start + Duration::from_secs(1), &mut events);
        keys.tick(start, &mut events);
        assert!(events.is_empty(), "out-of-order acquisition from another producer cannot panic or invent a hold");
    }

    #[test]
    fn held_deadlines_stop_after_one_shot_actions_but_preserve_seek_repeats() {
        let start = Instant::now();
        for key in [Key::Center, Key::Back, Key::PlayPause, Key::Prev, Key::Next] {
            let mut keys = Keys::default();
            let mut events = Vec::new();
            keys.key(key, true, start, &mut events);
            assert_eq!(keys.next_deadline(), Some(start + LONG_PRESS));
            let fired = start + LONG_PRESS;
            keys.tick(fired, &mut events);
            assert_eq!(events.len(), 1);
            if matches!(key, Key::Prev | Key::Next) {
                assert_eq!(keys.next_deadline(), Some(fired + SEEK_REPEAT));
                for repeat in 1..=64 {
                    keys.tick(fired + SEEK_REPEAT * repeat, &mut events);
                    assert_eq!(events.len(), repeat as usize + 1);
                    assert_eq!(keys.next_deadline(), Some(fired + SEEK_REPEAT * (repeat + 1)));
                }
            } else {
                for later in 0..64 {
                    keys.tick(fired + Duration::from_secs(later), &mut events);
                    assert_eq!(events.len(), 1, "one-shot hold cannot repeat: {key:?}");
                    assert_eq!(keys.next_deadline(), None, "one-shot hold has no future action: {key:?}");
                }
            }
            keys.key(key, false, fired + Duration::from_secs(100), &mut events);
            assert_eq!(keys.next_deadline(), None);
        }
    }

    #[test]
    fn side_buttons_seek_while_held() {
        let t0 = Instant::now();
        let mut k = Keys::default();
        let mut out = Vec::new();
        k.key(Key::Next, true, t0, &mut out);
        for ms in [600, 700, 860, 1110] {
            k.tick(t0 + Duration::from_millis(ms), &mut out);
        }
        k.key(Key::Next, false, t0 + Duration::from_millis(1200), &mut out);
        assert_eq!(out, vec![Event::SeekForward, Event::SeekForward, Event::SeekForward]);
    }
}
