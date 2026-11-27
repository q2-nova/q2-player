//! The hardware under the app: the Q2 on the mainline image, or a desktop window.

pub mod drm_test;
mod display;
pub(crate) mod system;
pub(crate) mod process;
pub(crate) mod netlink;
pub(crate) mod resident;
pub(crate) mod memory;
mod evdev;
mod touchscreen;
mod input_ffi;
mod clicker;
mod beeper;
mod headphones;
mod controls;
#[cfg(feature = "sim")]
pub mod sim;

use crate::audio::alsa::{AudioHw, SoftHw};
use crate::gfx::Canvas;
use crate::input::Raw;
use std::sync::Arc;

/// Optional hardware the settings screen offers only when present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hardware {
    /// Wheel and Center clicks can sound.
    pub clicker: bool,
    /// Jack switches report unplugging (Pause on Unplug).
    pub headphone_switch: bool,
    /// A native touchscreen is present, including while held in reset.
    pub touchscreen: bool,
}

impl Default for Hardware {
    fn default() -> Self { Self { clicker: true, headphone_switch: true, touchscreen: false } }
}

/// Where raw input goes; platforms call it from any thread.
pub type InputSink = Arc<dyn Fn(Raw) + Send + Sync>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Battery {
    /// Percent, once read.
    pub level: Option<u8>,
    pub charging: bool,
    /// On a charger or a computer (charging or full).
    pub plugged: bool,
}

/// The device under the UI loop: screen, backlight, battery, jacks, suspend
/// and power-off, plus the audio controls handed to the player.
///
/// Called only from the main (UI) thread. `suspend` and `power_off` are
/// called only after the UI's power transaction reached `Ready`
/// (`q2_machines::power`): settings saved, the player's sleep barrier held
/// and all keys released.
pub type ControlFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + 'a>>;

pub type SuspendFuture<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + 'a>>;

pub type PresentFuture<'a> = std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + 'a>>;

pub trait Platform {
    /// Shows a 375x320 frame.
    fn present<'a>(&'a mut self, frame: &'a Canvas) -> PresentFuture<'a>;
    /// Handles window events (the simulator); false to quit.
    fn pump(&mut self) -> bool {
        true
    }
    /// How often `pump` wants calling, if at all.
    fn pump_interval(&self) -> Option<std::time::Duration> {
        None
    }
    /// Turns the panel and backlight on or off. Idempotent.
    fn set_screen(&mut self, on: bool);
    /// 1-100.
    fn set_brightness(&mut self, level: u8);
    /// The battery's level and charging state now; may read sysfs.
    fn battery(&mut self) -> Battery;
    /// Whether headphones are in either jack, where that can be told.
    fn headphones(&mut self) -> Option<bool>;
    /// A live input owner publishes jack changes; unknown state still retries.
    fn headphone_events(&self) -> bool { false }
    /// Own or release the optional touchscreen evdev client.
    fn set_touchscreen(&mut self, enabled: bool) -> ControlFuture<'_, Result<(), String>> {
        Box::pin(async move {
            if enabled { Err("Touchscreen unavailable".into()) } else { Ok(()) }
        })
    }
    // Default adapters are for memory-only simulator/headless platforms.
    // The device overrides every method with owned finite operations.
    fn set_screen_async(&mut self, on: bool) -> ControlFuture<'_, ()> {
        Box::pin(async move { self.set_screen(on); })
    }
    /// Darken the display before the privileged retained-sleep helper runs.
    fn blank_for_suspend(&mut self) -> ControlFuture<'_, ()> {
        self.set_screen_async(false)
    }
    fn set_brightness_async(&mut self, level: u8) -> ControlFuture<'_, ()> {
        Box::pin(async move { self.set_brightness(level); })
    }
    fn battery_async(&mut self) -> ControlFuture<'_, Battery> {
        Box::pin(async move { self.battery() })
    }
    /// An owned battery observation, independent of the platform borrow.
    /// Periodic refresh and screen restoration retain one finite device job
    /// across cancelled observers, without holding the input loop behind it.
    /// Memory-only adapters may use the immediate battery_async fallback.
    fn start_battery_refresh(&mut self) -> Option<ControlFuture<'static, Battery>> { None }
    fn headphones_async(&mut self) -> ControlFuture<'_, Option<bool>> {
        Box::pin(async move { self.headphones() })
    }
    /// What optional hardware opened; fixed after startup.
    fn hardware(&self) -> Hardware { Hardware::default() }
    /// The DAC and output controls, handed to the player.
    fn audio_hw(&mut self) -> Box<dyn AudioHw>;
    /// Suspends to RAM until a key wakes it. Returns after resume (`Ok`) or
    /// when suspend was refused (`Err`, with a message for the owner); in
    /// both cases the caller restores the UI and releases the barrier.
    fn suspend(&mut self) -> SuspendFuture<'_>;
    /// Asks the system to power off. `Ok` means the request was accepted
    /// and power will go: the app keeps running until then and polls
    /// [`power_off_error`](Self::power_off_error). `Err` means it was
    /// refused and the UI is restored.
    fn power_off(&mut self) -> SuspendFuture<'_>;
    /// A device with no restored input must return to canonical supervision,
    /// rather than keep a live but unusable UI reporting health.
    fn controls_ready(&self) -> bool { true }
    /// Actual helper-return boundary, captured before input/display recovery.
    fn take_resume_return(&mut self) -> Option<std::time::Instant> { None }
    /// A live player cannot count an exited shutdown helper as a completed off.
    fn power_off_error(&mut self) -> Option<String> { None }
    /// Whether this is the device (as opposed to a simulator).
    fn is_device(&self) -> bool;
    /// The Clicker's click, if the hardware can make one.
    fn click(&mut self) {}
    /// Brief pitched click, falling back to the ordinary click when unavailable.
    fn click_tone(&mut self, _hz: u16) { self.click(); }
}

/// No screen at all: renders into memory, for screenshots and tests.
pub struct Headless {
    pub last: Option<Vec<u32>>,
}

impl Platform for Headless {
    fn present<'a>(&'a mut self, frame: &'a Canvas) -> PresentFuture<'a> {
        Box::pin(async move { self.last = Some(frame.pixels.clone()); Ok(()) })
    }
    fn set_screen(&mut self, _on: bool) {}
    fn set_brightness(&mut self, _level: u8) {}
    fn battery(&mut self) -> Battery {
        Battery { level: Some(76), charging: false, plugged: false }
    }
    fn headphones(&mut self) -> Option<bool> {
        Some(true)
    }
    fn audio_hw(&mut self) -> Box<dyn AudioHw> {
        // Q2ND_ALSA_DEVICE plays through ALSA instead (e.g. `null`, to test
        // a libasound).
        Box::new(SoftHw(std::env::var("Q2ND_ALSA_DEVICE").unwrap_or_else(|_| crate::audio::alsa::FAKE.into())))
    }
    fn suspend(&mut self) -> SuspendFuture<'_> { Box::pin(async { Err("Suspend unavailable in headless mode".into()) }) }
    fn power_off(&mut self) -> SuspendFuture<'_> { Box::pin(async { Err("Power off unavailable in headless mode".into()) }) }
    fn is_device(&self) -> bool {
        false
    }
}
