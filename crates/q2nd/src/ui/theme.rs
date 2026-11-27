//! Colours and metrics after the iPod classic (6th and 7th generation): a
//! silver title bar, white lists in bold black type, the blue gradient
//! selection bar, a grey scroll bar, translucent black pop-ups. Sizes are
//! the classic's 320x240 scaled to the Q2's 375x320 and its rounded glass.

use crate::config::{Accent, Look};
use crate::gfx::{Canvas, Color, Rect, WIDTH, corner_inset};

/// The title bar.
pub const STATUS_H: i32 = 30;
/// Title bar icons keep this far from the edges, clear of the glass corners
/// (Q2 Pod's `STATUS_MARGIN`).
pub const STATUS_MARGIN: i32 = 54;
/// Text rows: eight under the title bar, as the classic's menus.
pub const ROW_H: i32 = 36;
/// Text rows with a second line (settings, Wi-Fi).
pub const TALL_ROW_H: i32 = 48;
/// Rows with art: the classic's album list, thumbnail and two lines.
pub const MEDIA_ROW_H: i32 = 60;
pub const ART: i32 = crate::library::art::THUMBNAIL_SIZE as i32;
pub const ART_INSET: i32 = 6;
pub const SIDE: i32 = 12;
pub const SCROLLBAR_W: i32 = 8;

pub const TITLE_SIZE: u16 = 17;
pub const DETAIL_SIZE: u16 = 14;

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub dark: bool,
    pub bg: Color,
    pub text: Color,
    /// Secondary text: artists, values, times.
    pub muted: Color,
    pub faint: Color,
    pub header_top: Color,
    pub header_bottom: Color,
    pub header_line: Color,
    pub header_text: Color,
    pub sel_top: Color,
    pub sel_mid: Color,
    pub sel_bottom: Color,
    pub sel_text: Color,
    pub sel_muted: Color,
    pub chevron: Color,
    pub scroll_track: Color,
    pub scroll_line: Color,
    pub scroll_thumb_top: Color,
    pub scroll_thumb_bottom: Color,
    /// Progress and volume bars: the empty part, and its outline.
    pub track: Color,
    pub track_line: Color,
    /// Pop-ups and the fast-scroll letter: translucent black.
    pub hud: Color,
    pub hud_alpha: u32,
}

impl Theme {
    pub fn new(look: Look, accent: Accent) -> Theme {
        // The selection gradient: a lighter upper half over a deeper lower one.
        let (sel_top, sel_mid, sel_bottom) = match accent {
            Accent::Blue => (Color(0x75B8F6), Color(0x3D8DE6), Color(0x1C62C8)),
            Accent::Graphite => (Color(0xA9AAB0), Color(0x7C7D84), Color(0x55565C)),
            Accent::Green => (Color(0x7CCB72), Color(0x44A83B), Color(0x2C8526)),
            Accent::Orange => (Color(0xF8B866), Color(0xF08A24), Color(0xD06A0E)),
            Accent::Pink => (Color(0xF39BC5), Color(0xE85A9B), Color(0xC93C7E)),
            Accent::Purple => (Color(0xB797F0), Color(0x8E5CE6), Color(0x6E3EC6)),
            Accent::Red => (Color(0xF08A8C), Color(0xE5484D), Color(0xC22D33)),
        };
        let base = Theme {
            dark: false,
            bg: Color::WHITE,
            text: Color::BLACK,
            muted: Color(0x7A7A7A),
            faint: Color(0xB4B4B4),
            header_top: Color(0xFAFAFA),
            header_bottom: Color(0xCFD0D3),
            header_line: Color(0x8E8E92),
            header_text: Color::BLACK,
            sel_top,
            sel_mid,
            sel_bottom,
            sel_text: Color::WHITE,
            sel_muted: Color(0xE4EEFA),
            chevron: Color(0x6E6E6E),
            scroll_track: Color(0xEDEDED),
            scroll_line: Color(0xA5A5A5),
            scroll_thumb_top: Color(0x9C9C9C),
            scroll_thumb_bottom: Color(0x5E5E5E),
            track: Color(0xDCDCDC),
            track_line: Color(0x9A9A9A),
            hud: Color(0x101010),
            hud_alpha: 215,
        };
        match look {
            Look::Classic => base,
            Look::Dark => Theme {
                dark: true,
                bg: Color::BLACK,
                text: Color::WHITE,
                muted: Color(0x8C8C8C),
                faint: Color(0x4A4A4A),
                header_top: Color(0x3A3A3C),
                header_bottom: Color(0x161618),
                header_line: Color(0x000000),
                header_text: Color::WHITE,
                chevron: Color(0x8C8C8C),
                scroll_track: Color(0x1C1C1E),
                scroll_line: Color(0x3A3A3C),
                scroll_thumb_top: Color(0x8C8C8C),
                scroll_thumb_bottom: Color(0x5E5E5E),
                track: Color(0x3A3A3A),
                track_line: Color(0x555555),
                hud: Color(0x2A2A2C),
                hud_alpha: 235,
                ..base
            },
        }
    }

    /// The selection bar's gradient over `r`.
    pub fn fill_selection(&self, c: &mut Canvas, r: Rect) {
        let half = r.h / 2;
        c.vertical_gradient(Rect::new(r.x, r.y, r.w, half), self.sel_top, self.sel_mid);
        c.vertical_gradient(Rect::new(r.x, r.y + half, r.w, r.h - half), self.sel_mid, self.sel_bottom);
        // The classic's bright top edge.
        c.fill_rect_alpha(Rect::new(r.x, r.y, r.w, 1), Color::WHITE, 70);
    }

    /// A bar's filled part (progress, volume), in the selection's colours.
    pub fn fill_bar(&self, c: &mut Canvas, r: Rect) {
        c.vertical_gradient(r, self.sel_top, self.sel_bottom);
    }
}

/// The left and right text edges of a row centred on screen row `y`: the
/// glass's rounded corners take a bite out of the top and bottom rows.
pub fn row_edges(y: i32) -> (i32, i32) {
    let inset = corner_inset(y).saturating_sub(2).max(0);
    (SIDE.max(inset + 4), WIDTH - SIDE.max(inset + 4))
}
