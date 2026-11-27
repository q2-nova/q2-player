//! One presentation for a page with nothing to show yet: loading, empty,
//! failed and offline (UX-006, UX project Phase 4). Every list and page screen
//! draws these states here, so their copy, layout and Retry affordance match.
//!
//! - Loading: "Loading…". Every loading state has [`DEADLINE`]; past it the
//!   page shows [`Code::Stuck`]'s message with Retry, and a late answer still
//!   replaces it.
//! - Empty: what is missing, then an optional hint on how to fill it.
//! - Failed: the fixed plain message, an optional hint (offline: what still
//!   works), and a selected Retry row: Center retries, as on any list.
//!
//! Back is never taken by these states: the app's Back always leaves the page.
use super::Draw;
use super::theme::{DETAIL_SIZE, ROW_H, TITLE_SIZE};
use crate::gfx::{Rect, Weight};
use crate::ux::Code;
use std::time::Duration;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use crate::clock::Instant;

/// A page with nothing to show after this says so and offers Retry (UX-006).
#[cfg(not(test))]
pub const DEADLINE: Duration = Duration::from_secs(15);
#[cfg(test)]
pub const DEADLINE: Duration = Duration::from_millis(200);

pub const LOADING: &str = "Loading…";
pub const RETRY: &str = "Retry";

/// What a page shows in place of its content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum View<'a> {
    Loading,
    /// `text` may carry a hint on a second line ("No playlists\nHint").
    Empty(&'a str),
    Failed { text: &'a str, retry: bool },
}

/// A failure's hint, from its message: offline and unreachable failures say
/// that what is on the card still plays.
pub fn hint(text: &str) -> Option<&'static str> {
    let offline = [Code::Offline, Code::NetworkUnreachable, Code::NavidromeUnreachable, Code::AbsUnreachable];
    offline.iter().any(|code| code.message() == text).then_some("Downloads still play")
}

/// The loading clock of a page: started when a load begins, expired once.
#[derive(Clone, Copy, Debug)]
pub struct Deadline { since: Instant, reported: bool }
impl Deadline {
    pub fn new(now: Instant) -> Self { Self { since: now, reported: false } }
    pub fn restart(&mut self, now: Instant) { *self = Self::new(now); }
    /// True once, the first time the load has waited [`DEADLINE`]; reports
    /// [`Code::Stuck`] for `journey`.
    pub fn expired(&mut self, now: Instant, journey: crate::ux::Journey) -> bool {
        if self.reported || now.saturating_duration_since(self.since) < DEADLINE { return false; }
        self.reported = true;
        crate::ux::fail(Code::Stuck, journey, 0);
        true
    }
}

/// Draws `view` centred in `area`: message, hint, then Retry as a selected row.
pub fn draw(d: &mut Draw, area: Rect, view: View) {
    let _ = draw_target(d, area, view);
}

/// Returns the clipped bounds of the Retry row, if one was drawn.
pub(super) fn draw_target(d: &mut Draw, area: Rect, view: View) -> Option<Rect> {
    let (text, retry, failed) = match view {
        View::Loading => (LOADING, false, false),
        View::Empty(text) => (text, false, false),
        View::Failed { text, retry } => (text, retry, true),
    };
    let mut lines = text.lines();
    let main = lines.next().unwrap_or_default();
    let extra: Vec<&str> = lines.collect();
    let hint = if failed && extra.is_empty() { hint(main) } else { None };
    let hints: Vec<&str> = extra.into_iter().chain(hint).collect();
    let width = area.w - 40;
    let main_h = d.fonts.line_height(TITLE_SIZE);
    let hint_h = d.fonts.line_height(DETAIL_SIZE);
    let gap = if retry { 16 } else { 0 };
    let height = main_h + hints.len() as i32 * hint_h + gap + if retry { ROW_H } else { 0 };
    let cx = area.x + area.w / 2;
    let mut y = area.y + (area.h - height) / 2;
    let (main_weight, main_color) = if failed { (Weight::Bold, d.theme.text) } else { (Weight::Regular, d.theme.muted) };
    let main = d.fonts.ellipsize(main, TITLE_SIZE, main_weight, width).into_owned();
    d.fonts.draw_centered(d.c, &main, cx, y, TITLE_SIZE, main_weight, main_color);
    y += main_h;
    for line in hints {
        let line = d.fonts.ellipsize(line, DETAIL_SIZE, Weight::Regular, width).into_owned();
        d.fonts.draw_centered(d.c, &line, cx, y, DETAIL_SIZE, Weight::Regular, d.theme.muted);
        y += hint_h;
    }
    if retry {
        let bar = Rect::new(area.x, y + gap, area.w, ROW_H);
        d.theme.fill_selection(d.c, bar);
        let th = d.fonts.line_height(TITLE_SIZE);
        d.fonts.draw_centered(d.c, RETRY, cx, bar.y + (ROW_H - th) / 2, TITLE_SIZE, Weight::Bold, d.theme.sel_text);
        return Some(bar.intersect(&d.c.clip()));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_failures_say_what_still_works_and_others_do_not() {
        assert_eq!(hint(Code::NavidromeUnreachable.message()), Some("Downloads still play"));
        assert_eq!(hint(Code::Offline.message()), Some("Downloads still play"));
        assert_eq!(hint(Code::ServerError.message()), None);
        assert_eq!(hint(Code::Stuck.message()), None);
    }

    #[test]
    fn a_deadline_expires_once_and_restarts() {
        crate::ux::reset_for_test();
        let start = Instant::now();
        let mut deadline = Deadline::new(start);
        assert!(!deadline.expired(start + DEADLINE / 2, crate::ux::Journey::OpenPage));
        assert!(deadline.expired(start + DEADLINE, crate::ux::Journey::OpenPage));
        assert!(!deadline.expired(start + DEADLINE * 2, crate::ux::Journey::OpenPage), "reported once");
        let sent = crate::ux::take_sent();
        assert_eq!(sent.iter().filter(|r| r.code == Code::Stuck as u16).count(), 1);
        deadline.restart(start + DEADLINE * 2);
        assert!(!deadline.expired(start + DEADLINE * 2, crate::ux::Journey::OpenPage), "restarted");
    }
}
