//! Optional local-time night tint. No additional wakeups; update on normal ticks.
use std::time::Instant;

pub struct Night {
    level: f32,
    last: Instant,
    quantized: u8,
    /// Green and blue scale factors as 24-bit fractions
    /// (`gfx::span::tint_multiplier`).
    green: u32,
    blue: u32,
}

/// One-hour ramps at 21:00–22:00 and 06:00–07:00, with no midnight discontinuity.
fn scheduled(seconds: u32) -> f32 {
    match seconds % 86400 {
        s @ 75600..=79199 => (s - 75600) as f32 / 3600.0,
        79200..=86399 | 0..=21599 => 1.0,
        s @ 21600..=25199 => (25200 - s) as f32 / 3600.0,
        _ => 0.0,
    }
}
impl Night {
    pub fn new() -> Self {
        Self {
            level: 0.0,
            last: Instant::now(),
            quantized: 0,
            green: 1 << 24,
            blue: 1 << 24,
        }
    }
    pub fn tick(&mut self, enabled: bool, now: Instant) -> bool {
        // Disabled scheduling still fades to neutral, without local-time work.
        if !enabled { return self.fade(0.0, now); }
        let tm=crate::platform::system::local_time();
        self.tick_at(enabled, (tm.tm_hour * 3600 + tm.tm_min * 60 + tm.tm_sec) as u32, now)
    }
    // Separate the local clock from the transition so generated checks can
    // exercise the production schedule without changing process TZ or time.
    pub(super) fn tick_at(&mut self, enabled: bool, seconds: u32, now: Instant) -> bool {
        self.fade(if enabled { scheduled(seconds) } else { 0.0 }, now)
    }
    pub(super) fn fade(&mut self, target: f32, now: Instant) -> bool {
        // Enabling/disabling also fades, over at least a minute for full range.
        let step = now.duration_since(self.last).as_secs_f32() / 60.0;
        self.last = now;
        self.level += (target - self.level).clamp(-step, step);
        let q = (self.level * 255.0).round() as u8;
        if q == self.quantized {
            return false;
        }
        self.set_quantized(q);
        true
    }
    /// The tables for warmth `q` (0 neutral, 255 full).
    pub(super) fn set_quantized(&mut self, q: u8) {
        self.quantized = q;
        // Green is value * (25500 - 18 q) / 25500, blue value * (25500 - 45 q) / 25500.
        self.green = crate::gfx::span::tint_multiplier(25500 - q as u32 * 18);
        self.blue = crate::gfx::span::tint_multiplier(25500 - q as u32 * 45);
    }
    pub fn apply(&self, pixels: &mut [u32]) {
        if self.quantized == 0 {
            return;
        }
        crate::gfx::span::tint(pixels, self.green, self.blue);
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn disabled_real_tick_fades_existing_warmth_without_a_schedule_read() {
        let mut n=Night::new(); let start=n.last;
        n.fade(1.0,start+std::time::Duration::from_secs(60));
        assert_eq!(n.quantized,255);
        assert!(n.tick(false,start+std::time::Duration::from_secs(90)));
        assert_eq!(n.quantized,128);
        assert!(n.tick(false,start+std::time::Duration::from_secs(120)));
        assert_eq!(n.quantized,0);
        assert!(!n.tick(false,start+std::time::Duration::from_secs(121)));
    }
    #[test]
    fn smooth_schedule_and_midnight() {
        assert_eq!(scheduled(20 * 3600), 0.0);
        assert_eq!(scheduled(21 * 3600 + 1800), 0.5);
        assert_eq!(scheduled(23 * 3600), 1.0);
        assert_eq!(scheduled(0), 1.0);
        assert_eq!(scheduled(6 * 3600 + 1800), 0.5);
        assert_eq!(scheduled(7 * 3600), 0.0);
        for s in 1..86400 {
            assert!((scheduled(s) - scheduled(s - 1)).abs() < 0.001);
        }
    }
    #[test]
    fn schedule_drives_exact_rgb_endpoints_at_normal_tick_cadence() {
        let mut n = Night::new();
        let start = n.last;
        for tick in 1..=240 {
            n.tick_at(true, 22 * 3600, start + std::time::Duration::from_millis(tick * 250));
        }
        let mut pixels = [0xffffff, 0xff0000, 0x00ff00, 0x0000ff, 0];
        n.apply(&mut pixels);
        assert_eq!(pixels, [0xffd18c, 0xff0000, 0x00d100, 0x00008c, 0]);
        assert!(!n.tick_at(true, 0, start + std::time::Duration::from_secs(61)));
        assert!(!n.tick_at(true, 6 * 3600, start + std::time::Duration::from_secs(62)));
        n.tick_at(true, 6 * 3600 + 1800, start + std::time::Duration::from_secs(122));
        assert_eq!(n.quantized, 128);
        n.tick_at(true, 7 * 3600, start + std::time::Duration::from_secs(182));
        assert_eq!(n.quantized, 0);
    }
    #[test]
    fn disabled_is_pixel_exact() {
        let n = Night::new();
        let mut p = [0xffffff, 0x123456, 0];
        let old = p;
        n.apply(&mut p);
        assert_eq!(p, old);
    }
    #[test]
    fn toggles_fade_and_do_not_change_red_or_black() {
        let mut n = Night::new();
        let now = n.last;
        n.fade(1.0, now + std::time::Duration::from_secs(30));
        assert_eq!(n.quantized, 128);
        n.fade(1.0, now + std::time::Duration::from_secs(60));
        assert_eq!(n.quantized, 255);
        let mut pixels = [0xffffff, 0xff0000, 0];
        n.apply(&mut pixels);
        assert_eq!(pixels[1], 0xff0000);
        assert_eq!(pixels[2], 0);
        assert!((pixels[0] & 255) < ((pixels[0] >> 8) & 255));
        n.fade(0.0, now + std::time::Duration::from_secs(90));
        assert_eq!(n.quantized, 128);
        n.fade(0.0, now + std::time::Duration::from_secs(120));
        assert_eq!(n.quantized, 0);
    }
}
