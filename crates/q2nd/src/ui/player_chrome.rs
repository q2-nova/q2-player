//! The title strip and playback indicators shared by the device and browser.
use super::{connectivity, icons, theme::*};
use crate::audio::player::PlayState;
use crate::gfx::{Canvas, Color, Fonts, Rect, WIDTH, Weight};

pub struct Header<'a> {
    pub title: &'a str,
    pub play: PlayState,
    pub level: Option<u8>,
    pub charging: bool,
    pub badge: connectivity::Badge,
    pub bars: u8,
}

impl Header<'_> {
    pub fn draw(self, c: &mut Canvas, fonts: &mut Fonts, th: Theme) {
        c.vertical_gradient(Rect::new(0, 0, WIDTH, STATUS_H - 1), th.header_top, th.header_bottom);
        c.fill_rect(Rect::new(0, STATUS_H - 1, WIDTH, 1), th.header_line);
        let ink = th.header_text;
        let y = 9.0;
        match self.play {
            PlayState::Playing | PlayState::Buffering => icons::play(c, STATUS_MARGIN as f32, y, 12.0, ink),
            PlayState::Paused => icons::pause(c, STATUS_MARGIN as f32, y, 12.0, ink),
            PlayState::Stopped => {}
        }
        // Battery at the right, then Wi-Fi or offline, then downloads.
        let bx = WIDTH - STATUS_MARGIN - 24;
        let inside = if th.dark { Color(0x1A1A1A) } else { Color(0xF4F4F4) };
        icons::battery(c, bx, 9, self.level, self.charging, Color(0x6A6A6A), inside);
        let x = bx - 10;
        connectivity::draw(c, self.badge, (x - 20) as f32, 8.0, self.bars, ink, th.header_bottom);
        let left = STATUS_MARGIN + 22;
        let right = x - 20;
        let width = (right - left).min(WIDTH - 2 * left).max(40);
        let t = fonts.ellipsize(self.title, 16, Weight::Bold, width).into_owned();
        let lh = fonts.line_height(16);
        // Centred, unless that would run into the status icons.
        let w = fonts.measure(&t, 16, Weight::Bold);
        let cx = (WIDTH / 2).min(right - w / 2).max(left + w / 2);
        fonts.draw_centered(c, &t, cx, (STATUS_H - lh) / 2 - 1, 16, Weight::Bold, ink);
    }
}

/// The classic's volume bar: a small speaker, the bar, a loud speaker.
pub fn volume_bar(c: &mut Canvas, th: &Theme, r: Rect, volume: u8, ink: Color) {
    icons::speaker_plain(c, r.x as f32, (r.y + r.h / 2 - 6) as f32, 12.0, ink);
    icons::speaker(c, (r.right() - 16) as f32, (r.y + r.h / 2 - 7) as f32, 14.0, ink);
    let bar = Rect::new(r.x + 20, r.y + r.h / 2 - 4, r.w - 44, 8);
    progress_bar(c, th, bar, volume as u64, 100);
}

/// A progress or volume bar: the classic's inset track and a gradient fill.
pub fn progress_bar(c: &mut Canvas, th: &Theme, bar: Rect, value: u64, max: u64) {
    c.fill_rect(bar, th.track_line);
    c.fill_rect(bar.inset(1, 1), th.track);
    let w = ((bar.w - 2) as u64 * value.min(max) / max.max(1)) as i32;
    if w > 0 {
        th.fill_bar(c, Rect::new(bar.x + 1, bar.y + 1, w, bar.h - 2));
    }
}

/// "3:07" or "1:02:03".
pub fn duration(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

/// "1 hr 5 min" or "42 min", for album and playlist totals.
pub fn long_duration(secs: u64) -> String {
    let m = (secs + 30) / 60;
    if m >= 60 { format!("{} hr {} min", m / 60, m % 60) } else { format!("{m} min") }
}
