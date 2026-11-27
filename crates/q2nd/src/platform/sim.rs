//! The desktop simulator: a window the size of the Q2's screen (doubled),
//! keys for the buttons, the mouse wheel or arrow keys for the wheel.
//!
//! | Q2           | Keyboard                       |
//! | ------------ | ------------------------------ |
//! | Wheel        | Up / Down, mouse wheel         |
//! | Centre       | Enter, Right                   |
//! | Return       | Backspace, Escape, Left        |
//! | Play/Pause   | Space                          |
//! | Prev / Next  | `[` / `]` (hold to seek)       |

use super::{Battery, InputSink, Platform};
use crate::audio::alsa::{AudioHw, SoftHw};
use crate::gfx::{Canvas, HEIGHT, WIDTH};
use crate::input::{Key, Raw};
use minifb::{Key as K, Scale, Window, WindowOptions};
use std::time::{Duration, Instant};

// minifb exposes scroll deltas but no mouse-wheel contact edge. Use the
// same idle interval as the browser demo for that input source.
const MOUSE_WHEEL_IDLE: Duration = Duration::from_millis(120);
const WHEEL_KEYS: [K; 4] = [K::Down, K::Up, K::PageDown, K::PageUp];

pub struct Sim {
    window: Window,
    input: InputSink,
    held: Vec<Key>,
    last_mouse_scroll: Option<Instant>,
    screen_on: bool,
    frame: Vec<u32>,
    pending_present: bool,
}

impl Sim {
    pub fn open(input: InputSink) -> Result<Sim, String> {
        let opts = WindowOptions { scale: Scale::X2, ..WindowOptions::default() };
        let mut window =
            Window::new("Q2 Navidrome", WIDTH as usize, HEIGHT as usize, opts).map_err(|e| e.to_string())?;
        window.set_target_fps(0);
        Ok(Sim {
            window,
            input,
            held: Vec::new(),
            last_mouse_scroll: None,
            screen_on: true,
            frame: vec![0; (WIDTH * HEIGHT) as usize],
            pending_present: true,
        })
    }

    fn key_of(k: K) -> Option<Key> {
        match k {
            K::Enter | K::NumPadEnter | K::Right => Some(Key::Center),
            K::Backspace | K::Escape | K::Left => Some(Key::Back),
            K::Space => Some(Key::PlayPause),
            K::LeftBracket | K::Comma => Some(Key::Prev),
            K::RightBracket | K::Period => Some(Key::Next),
            _ => None,
        }
    }

}

impl Platform for Sim {
    fn present<'a>(&'a mut self, frame: &'a Canvas) -> super::PresentFuture<'a> {
        Box::pin(async move {
            if self.screen_on { self.frame.copy_from_slice(&frame.pixels); }
            else { self.frame.fill(0); }
            crate::gfx::mask_corners(&mut self.frame);
            self.pending_present = true;
            Ok(())
        })
    }

    fn pump(&mut self) -> bool {
        if !self.window.is_open() {
            return false;
        }
        let mouse_ended = self.last_mouse_scroll.is_some_and(|at| at.elapsed() >= MOUSE_WHEEL_IDLE);
        if mouse_ended { self.last_mouse_scroll = None; }
        let wheel_key_down = WHEEL_KEYS.iter().any(|&key| self.window.is_key_down(key));
        let key_ended = WHEEL_KEYS.iter().any(|&key| self.window.is_key_released(key));
        if !wheel_key_down && self.last_mouse_scroll.is_none() && (key_ended || mouse_ended) {
            (self.input)(Raw::WheelEnd);
        }
        let now: Vec<Key> = self.window.get_keys().into_iter().filter_map(Self::key_of).collect();
        for k in &now {
            if !self.held.contains(k) {
                (self.input)(Raw::Key { key: *k, down: true });
            }
        }
        for k in &self.held {
            if !now.contains(k) {
                (self.input)(Raw::Key { key: *k, down: false });
            }
        }
        self.held = now;
        for k in self.window.get_keys_pressed(minifb::KeyRepeat::Yes) {
            match k {
                K::Down => (self.input)(Raw::WheelTicks(1)),
                K::Up => (self.input)(Raw::WheelTicks(-1)),
                K::PageDown => (self.input)(Raw::WheelTicks(5)),
                K::PageUp => (self.input)(Raw::WheelTicks(-5)),
                _ => {}
            }
        }
        if let Some((_, dy)) = self.window.get_scroll_wheel() {
            if dy != 0.0 {
                self.last_mouse_scroll = Some(Instant::now());
                (self.input)(Raw::WheelTicks(if dy < 0.0 { 1 } else { -1 }));
            }
        }
        if self.pending_present {
            self.pending_present = false;
            let _ = self.window.update_with_buffer(&self.frame, WIDTH as usize, HEIGHT as usize);
        } else {
            self.window.update();
        }
        true
    }

    fn pump_interval(&self) -> Option<Duration> {
        Some(Duration::from_millis(16))
    }

    fn set_screen(&mut self, on: bool) {
        self.screen_on = on;
        if !on {
            self.frame.fill(0);
            self.pending_present = true;
        }
    }

    fn set_brightness(&mut self, _level: u8) {}

    fn battery(&mut self) -> Battery {
        Battery { level: Some(76), charging: false, plugged: false }
    }

    fn headphones(&mut self) -> Option<bool> {
        Some(true)
    }

    fn audio_hw(&mut self) -> Box<dyn AudioHw> {
        Box::new(SoftHw(std::env::var("Q2ND_ALSA_DEVICE").unwrap_or_else(|_| "default".into())))
    }

    fn suspend(&mut self) -> super::SuspendFuture<'_> {
        Box::pin(async { Err("Suspend unavailable in simulator".into()) })
    }

    fn power_off(&mut self) -> super::SuspendFuture<'_> {
        Box::pin(async { std::process::exit(0) })
    }

    fn is_device(&self) -> bool {
        false
    }
}
