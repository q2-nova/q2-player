mod glyph;
pub use glyph::{Badge, draw, select};

/// One bounded, change-only record of the inputs used to select a badge.
/// Bits0..6: enabled/associated/IP/doze/offline/TS-required/server-online;
/// bit7: TS status is Connected; bits8..11: Observation; bits12..14: Badge.
/// The status bit is separate: tests/legacy constructors can have Unknown provenance.
#[derive(Default)]
pub(super) struct SelectionTrace { last: Option<u32>, count: u32, limited: bool }
pub(super) const SELECTION_LIMIT: u32 = 1024;
#[derive(Debug, PartialEq)]
pub(super) enum SelectionRecord { Selected(u32), Limit }
impl SelectionTrace {
    pub(super) fn observe(&mut self, flags: [bool; 8], observation: crate::tailscale::Observation,
        badge: Badge) -> Option<SelectionRecord> {
        let bits = flags.into_iter().enumerate().fold(0, |bits, (i, yes)| bits | (u32::from(yes) << i));
        let value = bits | ((observation as u32) << 8) | ((badge as u32) << 12);
        if self.last == Some(value) { return None; }
        self.last = Some(value);
        if self.count == SELECTION_LIMIT {
            if self.limited { return None; }
            self.limited = true;
            return Some(SelectionRecord::Limit);
        }
        self.count += 1;
        Some(SelectionRecord::Selected(value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gfx::{Canvas, Color};
    #[test]
    fn selected_trace_distinguishes_same_badge_causes_and_radio_inputs_without_repeats() {
        use crate::tailscale::Observation as O;
        let mut trace = SelectionTrace::default();
        let mut flags = [true, true, true, false, false, true, true, false];
        let first = trace.observe(flags, O::Timeout, Badge::TailscaleUnavailable).unwrap();
        assert_eq!(first, SelectionRecord::Selected(0x2a67));
        assert_eq!(trace.observe(flags, O::Timeout, Badge::TailscaleUnavailable), None);
        assert_ne!(trace.observe(flags, O::NeedsLogin, Badge::TailscaleUnavailable), Some(first));
        flags[7] = true;
        assert_eq!(trace.observe(flags, O::Running, Badge::Healthy), Some(SelectionRecord::Selected(0x44e7)));
        flags[1] = false; flags[3] = true;
        assert_eq!(trace.observe(flags, O::Running, Badge::WifiUnavailable), Some(SelectionRecord::Selected(0x14ed)));
    }

    #[test]
    fn selected_trace_limit_is_explicit_once_and_never_timed_out() {
        use crate::tailscale::Observation as O;
        let mut trace = SelectionTrace::default();
        let mut flags = [false; 8];
        for i in 0..SELECTION_LIMIT {
            flags[3] = i % 2 == 1;
            assert!(matches!(trace.observe(flags, O::Unknown, Badge::WifiUnavailable), Some(SelectionRecord::Selected(_))));
        }
        assert_eq!(trace.observe(flags, O::Unknown, Badge::WifiUnavailable), None);
        flags[3] = !flags[3];
        assert_eq!(trace.observe(flags, O::Unknown, Badge::WifiUnavailable), Some(SelectionRecord::Limit));
        flags[3] = !flags[3];
        assert_eq!(trace.observe(flags, O::Unknown, Badge::WifiUnavailable), None);
    }

    #[test]
    fn deliberate_offline_and_failed_dependencies_choose_exactly_one_badge() {
        use Badge::*;
        for tail_enabled in [false, true] {
            for tail_online in [false, true] {
                for nav_online in [false, true] {
                    assert_eq!(select(true, true, tail_enabled, tail_online, nav_online), OfflineMode);
                    assert_eq!(select(true, false, tail_enabled, tail_online, nav_online), OfflineMode);
                    assert_eq!(select(false, false, tail_enabled, tail_online, nav_online), WifiUnavailable);
                }
            }
        }
        assert_eq!(select(false, true, false, false, true), Healthy, "disabled VPN is optional");
        assert_eq!(select(false, true, false, true, true), Healthy);
        assert_eq!(select(false, true, false, false, false), NavidromeUnavailable);
        assert_eq!(select(false, true, false, true, false), NavidromeUnavailable);
        assert_eq!(select(false, true, true, false, true), TailscaleUnavailable);
        assert_eq!(select(false, true, true, false, false), TailscaleUnavailable);
        assert_eq!(select(false, true, true, true, false), NavidromeUnavailable);
        assert_eq!(select(false, true, true, true, true), Healthy);
    }

    #[test]
    fn generated_connectivity_badge_preview() {
        use crate::gfx::{Fonts, Rect, Weight};
        let mut c = Canvas::new(375, 210); c.clear(Color::WHITE);
        let mut fonts = Fonts::bundled();
        for (index, (badge, label)) in [(Badge::OfflineMode,"Offline Mode"),
            (Badge::WifiUnavailable,"Wi-Fi unavailable"), (Badge::TailscaleUnavailable,"Tailscale unavailable"),
            (Badge::NavidromeUnavailable,"Navidrome unavailable"), (Badge::Healthy,"All required connections healthy")].iter().enumerate() {
            let y = 10 + index as i32 * 40;
            c.fill_rect(Rect::new(0, y, 375, 30), Color(0xEEEEEE));
            draw(&mut c, *badge, 18.0, (y + 8) as f32, 3, Color(0x222222), Color(0xEEEEEE));
            fonts.draw(&mut c, label, 53, y + 5, 14, Weight::Regular, Color(0x222222));
        }
        let pixels: Vec<u8> = c.pixels.iter().flat_map(|p| [(*p >> 16) as u8, (*p >> 8) as u8, *p as u8]).collect();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build/perf/connectivity-badges.png");
        image::save_buffer(path, &pixels, 375, 210, image::ColorType::Rgb8).unwrap();
    }
}
