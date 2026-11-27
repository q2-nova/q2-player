//! Shared scrollable information text for the native player and browser demo.
use crate::gfx::{Fonts, Rect, Weight};
use crate::ui::Draw;

const FONT: u16 = 17;
const LINE: i32 = 23;
const FOOTER: i32 = 22;
const FOOTER_SIZE: u16 = 12;

struct TextLine {
    text: String,
    bold: bool,
}

pub(crate) struct InfoText {
    lines: Vec<TextLine>,
    scroll: i32,
    view_height: i32,
    layout_width: i32,
}

impl Default for InfoText {
    fn default() -> Self {
        Self {
            lines: Vec::new(),
            scroll: 0,
            view_height: 240,
            layout_width: 0,
        }
    }
}

impl InfoText {
    pub(crate) fn relayout(&mut self) {
        self.layout_width = 0;
    }

    pub(crate) fn reset_scroll(&mut self) {
        self.scroll = 0;
    }

    pub(crate) fn clear(&mut self) {
        self.lines.clear();
        self.relayout();
        self.reset_scroll();
    }

    #[cfg(test)]
    pub(super) fn audit(&self) -> (i32, Vec<&String>) {
        (
            self.scroll,
            self.lines.iter().map(|line| &line.text).collect(),
        )
    }

    // Only whole lines can be displayed or scrolled.
    fn visible_lines(&self) -> i32 {
        (self.view_height / LINE).max(1)
    }

    fn maximum_scroll(&self) -> i32 {
        (self.lines.len() as i32 - self.visible_lines()).max(0) * LINE
    }

    pub(crate) fn scroll_by(&mut self, ticks: i32) {
        self.scroll = self
            .scroll
            .saturating_add(ticks.saturating_mul(LINE))
            .clamp(0, self.maximum_scroll());
    }

    fn layout(&mut self, fonts: &mut Fonts, width: i32, paragraphs: Vec<(String, bool)>) {
        self.lines = paragraphs
            .into_iter()
            .flat_map(|(text, bold)| {
                wrap(&text, fonts, width)
                    .into_iter()
                    .map(move |text| TextLine { text, bold })
            })
            .collect();
        self.layout_width = width;
        self.scroll = self.scroll.clamp(0, self.maximum_scroll());
    }

    pub(crate) fn footer_target(fonts: &mut Fonts, area: Rect, footer: &str, label: &str) -> Option<Rect> {
        let (prefix, _) = footer.split_once(label)?;
        let left = area.x + area.w / 2 - fonts.measure(footer, FOOTER_SIZE, Weight::Regular) / 2;
        let start = fonts.measure(prefix, FOOTER_SIZE, Weight::Regular);
        let width = fonts.measure(label, FOOTER_SIZE, Weight::Regular);
        Some(Rect::new(left + start, area.bottom() - FOOTER, width, fonts.line_height(FOOTER_SIZE)))
    }

    pub(crate) fn draw(
        &mut self,
        d: &mut Draw,
        area: Rect,
        footer: &str,
        paragraphs: impl FnOnce() -> Vec<(String, bool)>,
    ) {
        // A gap above the footer: the last text line never runs into it.
        let body = Rect::new(area.x + 32, area.y + 8, area.w - 64, area.h - FOOTER - 16);
        self.view_height = body.h;
        if self.layout_width != body.w {
            self.layout(d.fonts, body.w, paragraphs());
        }
        self.scroll = self.scroll.clamp(0, self.maximum_scroll());
        let old = d.c.set_clip(body);
        let first = (self.scroll / LINE) as usize;
        let end = first + self.visible_lines() as usize;
        for (index, line) in self.lines.iter().enumerate().take(end).skip(first) {
            let y = body.y + index as i32 * LINE - self.scroll;
            let weight = if line.bold {
                Weight::Bold
            } else {
                Weight::Regular
            };
            d.fonts
                .draw(d.c, &line.text, body.x, y, FONT, weight, d.theme.text);
        }
        d.c.set_clip(old);
        if self.maximum_scroll() > 0 {
            let shown = self.visible_lines() * LINE;
            let height = (shown * shown / (self.lines.len() as i32 * LINE)).max(12);
            let y = body.y + (shown - height) * self.scroll / self.maximum_scroll();
            d.c.fill_rect(Rect::new(area.right() - 10, y, 3, height), d.theme.muted);
        }
        d.fonts.draw_centered(
            d.c,
            footer,
            area.x + area.w / 2,
            area.bottom() - FOOTER,
            FOOTER_SIZE,
            Weight::Regular,
            d.theme.muted,
        );
    }
}

fn wrap(text: &str, fonts: &mut Fonts, width: i32) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            let candidate = if line.is_empty() {
                word.into()
            } else {
                format!("{line} {word}")
            };
            if fonts.measure(&candidate, FONT, Weight::Bold) <= width {
                line = candidate;
                continue;
            }
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            for ch in word.chars() {
                let candidate = format!("{line}{ch}");
                if !line.is_empty() && fonts.measure(&candidate, FONT, Weight::Bold) > width {
                    lines.push(std::mem::take(&mut line));
                }
                line.push(ch);
            }
        }
        lines.push(line);
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn info_scrolls_and_stops_on_whole_lines() {
        let mut screen = InfoText::default();
        screen.lines = (0..30)
            .map(|n| TextLine {
                text: format!("Generated line {n}"),
                bold: false,
            })
            .collect();
        // The body height of a full page is not a whole number of lines.
        screen.view_height = 252;
        assert_eq!(screen.visible_lines(), 10);
        assert_eq!(
            screen.maximum_scroll(),
            20 * LINE,
            "the last line ends the page whole"
        );
        for ticks in [1, 1, 3, -1, 40, -2, -40] {
            screen.scroll_by(ticks);
            assert_eq!(screen.scroll % LINE, 0, "after {ticks}: {}", screen.scroll);
            assert!((0..=screen.maximum_scroll()).contains(&screen.scroll));
        }
        screen.scroll_by(1);
        assert_eq!(screen.scroll, LINE, "one detent is one line");
        screen.lines.truncate(4);
        assert_eq!(screen.maximum_scroll(), 0, "a short page does not scroll");
    }
    #[test]
    fn info_is_readable_and_wraps_long_unicode_without_losing_text() {
        let mut fonts = Fonts::bundled();
        for text in [
            "A description with ordinary words.",
            "音楽",
            "A".repeat(4096).as_str(),
        ] {
            let lines = wrap(text, &mut fonts, 280);
            assert_eq!(lines.concat().replace(' ', ""), text.replace(' ', ""));
            assert!(
                lines
                    .iter()
                    .all(|line| fonts.measure(line, FONT, Weight::Bold) <= 280)
            );
        }
    }
}
