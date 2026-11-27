//! Speed control presentation shared by the device and browser demo.
use crate::gfx::{Rect, WIDTH, Weight};
use crate::ui::{Draw, progress_bar};

pub const MIN: u16 = 50;
pub const MAX: u16 = 300;
pub const STEP: u16 = 5;

pub fn adjust(percent: u16, ticks: i32) -> u16 {
    i32::from(percent)
        .saturating_add(ticks.saturating_mul(i32::from(STEP)))
        .clamp(i32::from(MIN), i32::from(MAX)) as u16
}

pub fn label(percent: u16) -> String {
    format!("{}.{:02}×", percent / 100, percent % 100)
}

pub fn caption(setting_default: bool, custom: bool, podcast: bool) -> &'static str {
    if setting_default {
        "Books & podcasts default"
    } else if !custom {
        "Default speed"
    } else if podcast {
        "Custom for this podcast series"
    } else {
        "Custom for this book"
    }
}

pub fn bar(area: Rect) -> Rect {
    Rect::new(area.x + 45, area.y + 143, area.w - 90, 18)
}

pub fn draw_inline(d: &mut Draw, bar: Rect, percent: u16, custom: bool, error: Option<&str>) {
    let label = format!("Playback Speed  {}", label(percent));
    d.fonts.draw_centered(
        d.c,
        &label,
        WIDTH / 2,
        bar.y - 27,
        16,
        Weight::Bold,
        d.theme.text,
    );
    progress_bar(
        d.c,
        &d.theme,
        bar,
        u64::from(percent - MIN),
        u64::from(MAX - MIN),
    );
    let caption = error.unwrap_or(if custom {
        "Custom speed · Center: volume"
    } else {
        "Default speed · Center: volume"
    });
    d.fonts.draw_fit(
        d.c,
        caption,
        bar.x,
        bar.y + 14,
        bar.w,
        13,
        Weight::Regular,
        d.theme.muted,
    );
}

pub fn draw(
    d: &mut Draw,
    area: Rect,
    percent: u16,
    caption: &str,
    setting_default: bool,
    error: Option<&str>,
) {
    let cx = area.x + area.w / 2;
    d.fonts.draw_centered(
        d.c,
        &label(percent),
        cx,
        area.y + 65,
        30,
        Weight::Bold,
        d.theme.text,
    );
    d.fonts.draw_centered(
        d.c,
        caption,
        cx,
        area.y + 108,
        15,
        Weight::Regular,
        d.theme.muted,
    );
    progress_bar(
        d.c,
        &d.theme,
        bar(area),
        u64::from(percent - MIN),
        u64::from(MAX - MIN),
    );
    d.fonts.draw_centered(
        d.c,
        "Wheel: adjust · Center: done",
        cx,
        area.y + 186,
        16,
        Weight::Regular,
        d.theme.muted,
    );
    if !setting_default {
        d.fonts.draw_centered(
            d.c,
            "Hold Center: Use Default",
            cx,
            area.y + 216,
            16,
            Weight::Regular,
            d.theme.muted,
        );
    }
    if let Some(error) = error {
        d.fonts.draw_fit(
            d.c,
            error,
            area.x + 30,
            area.y + 251,
            area.w - 60,
            14,
            Weight::Regular,
            d.theme.text,
        );
    }
}
