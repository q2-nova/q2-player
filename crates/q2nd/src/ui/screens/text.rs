//! Typing with the wheel: a strip of characters along the bottom, the wheel
//! moves along it, the centre types the one in the middle.

use crate::gfx::{Color, Rect, WIDTH, Weight};
use crate::input::Event;
use crate::ui::{Action, Ctx, Draw, Screen};
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use crate::clock::Instant;
use zeroize::Zeroize;

const LOWER: &str = "abcdefghijklmnopqrstuvwxyz";
const UPPER: &str = "ABCDEFGHIJKLMNOPQRSTUVWXYZ";
const DIGITS: &str = "0123456789";
const SYMBOLS: &str = ".-_:/@&'!?#$%*+=,;()[]~\"\\^`{|}<>";

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Lower case and digits: what searching needs.
    Search,
    /// Anything: passwords, user names.
    Full,
    /// Addresses: lower case, digits and `:/.-_` first.
    Url,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Lower,
    Upper,
    Symbols,
}

#[derive(Clone, PartialEq)]
enum Cell {
    Done,
    Delete,
    Space,
    Mode(Mode),
    Char(char),
}

pub type OnDone = Box<dyn Fn(String, &mut Ctx) -> Action>;

pub(super) enum Edit {
    Editing,
    Done,
    Back,
}

pub struct TextEntry {
    title: String,
    text: String,
    kind: Kind,
    secret: bool,
    mode: Mode,
    cells: Vec<Cell>,
    sel: usize,
    /// The strip's drawn position, easing toward `sel`.
    shown: f32,
    on_done: Option<OnDone>,
    hit_cells: Vec<(usize, Rect)>,
}

impl TextEntry {
    pub fn new(title: impl Into<String>, initial: &str, kind: Kind, secret: bool, on_done: OnDone) -> TextEntry {
        Self::build_entry(title.into(), initial, kind, secret, Some(on_done))
    }

    pub(super) fn capture_secret(title: &str) -> Self {
        Self::build_entry(title.into(), "", Kind::Full, true, None)
    }

    fn build_entry(title: String, initial: &str, kind: Kind, secret: bool, on_done: Option<OnDone>) -> Self {
        let mut t = TextEntry {
            title,
            text: initial.to_string(),
            kind,
            secret,
            mode: Mode::Lower,
            cells: Vec::new(),
            sel: 0,
            shown: 0.0,
            on_done,
            hit_cells: Vec::new(),
        };
        t.build();
        // Start on the first letter, not on Done.
        t.sel = t.cells.iter().position(|c| matches!(c, Cell::Char(_))).unwrap_or(0);
        t.shown = t.sel as f32;
        t
    }

    fn build(&mut self) {
        let mut cells = vec![Cell::Done, Cell::Delete, Cell::Space];
        let chars: String = match (self.kind, self.mode) {
            (Kind::Search, _) => format!("{LOWER}{DIGITS}"),
            (Kind::Url, Mode::Lower) => format!(":/.-_{LOWER}{DIGITS}"),
            (_, Mode::Lower) => format!("{LOWER}{DIGITS}"),
            (_, Mode::Upper) => format!("{UPPER}{DIGITS}"),
            (_, Mode::Symbols) => format!("{SYMBOLS}{DIGITS}"),
        };
        if self.kind != Kind::Search {
            for m in [Mode::Lower, Mode::Upper, Mode::Symbols] {
                if m != self.mode {
                    cells.push(Cell::Mode(m));
                }
            }
        }
        cells.extend(chars.chars().map(Cell::Char));
        #[cfg(not(target_arch = "wasm32"))]
        self.hit_cells.clear();
        self.cells = cells;
        self.sel = self.sel.min(self.cells.len() - 1);
    }

    fn label(c: &Cell) -> String {
        match c {
            Cell::Done => "OK".into(),
            // Drawn as glyphs.
            Cell::Delete | Cell::Space => String::new(),
            Cell::Mode(Mode::Lower) => "abc".into(),
            Cell::Mode(Mode::Upper) => "ABC".into(),
            Cell::Mode(Mode::Symbols) => "#+=".into(),
            Cell::Char(ch) => ch.to_string(),
        }
    }

    fn cell_at(&self, x: i32, y: i32) -> Option<usize> {
        self.hit_cells.iter().rev().find_map(|&(index, r)| {
            ((r.x..r.right()).contains(&x) && (r.y..r.bottom()).contains(&y))
                .then_some(index)
        })
    }

    pub(super) fn select_cell(&mut self, x: i32, y: i32) -> bool {
        let Some(index) = self.cell_at(x, y) else { return false };
        self.sel = index;
        true
    }

    pub(super) fn edit(&mut self, ev: Event) -> Edit {
        match ev {
            Event::Scroll(t) => {
                let n = self.cells.len() as i32;
                self.sel = ((self.sel as i32 + t.rem_euclid(n)) % n) as usize;
            }
            Event::Center => match self.cells[self.sel].clone() {
                Cell::Done => return Edit::Done,
                Cell::Delete => { self.text.pop(); }
                Cell::Space => self.text.push(' '),
                Cell::Mode(m) => {
                    let pos = self.sel;
                    self.mode = m;
                    self.build();
                    self.sel = pos.min(self.cells.len() - 1);
                }
                Cell::Char(ch) => self.text.push(ch),
            },
            Event::CenterLong => return Edit::Done,
            Event::Back => return Edit::Back,
            _ => {}
        }
        Edit::Editing
    }

    pub(super) fn take_text(&mut self) -> String {
        std::mem::take(&mut self.text)
    }
}

impl Drop for TextEntry {
    fn drop(&mut self) {
        if self.secret {
            self.text.zeroize();
        }
    }
}

impl Screen for TextEntry {
    fn clickable(&self, x: i32, y: i32) -> bool {
        self.cell_at(x, y).is_some()
    }

    fn click(&mut self, x: i32, y: i32, ctx: &mut Ctx) -> Action {
        if self.select_cell(x, y) { self.event(Event::Center, ctx) } else { Action::None }
    }

    #[cfg(test)]
    fn audit(&self) -> crate::ui::hierarchy::Surface { crate::ui::hierarchy::Surface::widget("TextEntry", format!("cells={};selected={}", self.cells.iter().map(|cell| match cell { Cell::Delete => "Delete".into(), Cell::Space => "Space".into(), _ => Self::label(cell) }).collect::<Vec<_>>().join(" "), self.sel)) }
    #[cfg(target_arch = "wasm32")]
    fn selected_label(&self) -> Option<&str> {
        self.cells.get(self.sel).map(|cell| match cell {
            Cell::Done => "OK",
            Cell::Delete => "Delete",
            Cell::Space => "Space",
            Cell::Mode(Mode::Lower) => "Lowercase",
            Cell::Mode(Mode::Upper) => "Uppercase",
            Cell::Mode(Mode::Symbols) => "Symbols",
            Cell::Char(ch) => {
                let ascii = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789.-_:/@&'!?#$%*+=,;()[]~\"\\^`{|}<>";
                ascii.find(*ch).map(|i| &ascii[i..i + 1]).unwrap_or("Character")
            }
        })
    }

    fn title(&self) -> String {
        self.title.clone()
    }

    fn event(&mut self, ev: Event, ctx: &mut Ctx) -> Action {
        match self.edit(ev) {
            Edit::Done => self.on_done.as_ref().map_or(Action::None, |handler| handler(self.text.trim().to_string(), ctx)),
            Edit::Back => Action::Pop,
            Edit::Editing => Action::None,
        }
    }

    fn draw(&mut self, d: &mut Draw, area: Rect) {
        let th = d.theme;
        // The field.
        // A sunken white field, as the classic's search box.
        let field = Rect::new(20, area.y + 28, WIDTH - 40, 46);
        d.c.fill_round_rect(field, 8, th.track_line);
        d.c.fill_round_rect(field.inset(1, 1), 7, if th.dark { Color(0x1C1C1E) } else { Color::WHITE });
        d.c.vertical_gradient(Rect::new(field.x + 4, field.y + 1, field.w - 8, 3), th.track, if th.dark { Color(0x1C1C1E) } else { Color::WHITE });
        let shown: String = if self.secret { "•".repeat(self.text.chars().count()) } else { self.text.clone() };
        let size = 20;
        let lh = d.fonts.line_height(size);
        let maxw = field.w - 28;
        // Keep the end in view: cut from the front.
        let mut visible = shown.clone();
        while d.fonts.measure(&visible, size, Weight::Regular) > maxw {
            let mut cs = visible.chars();
            cs.next();
            visible = cs.collect();
        }
        let w = d.fonts.draw(d.c, &visible, field.x + 14, field.y + (field.h - lh) / 2, size, Weight::Regular, th.text);
        // The caret.
        d.c.fill_rect(Rect::new(field.x + 15 + w, field.y + 12, 2, field.h - 24), th.sel_mid);
        let hint = match self.kind {
            Kind::Search => "Hold Center to search · Return cancels",
            _ if self.on_done.is_none() => "Hold Center to continue · Return cancels",
            _ => "Hold Center to save · Return cancels",
        };
        d.fonts.draw_centered(d.c, hint, WIDTH / 2, field.bottom() + 12, 14, Weight::Regular, th.muted);

        // The strip: the selected cell in the middle, neighbours either side.
        let strip_y = area.bottom() - 92;
        let cell_w = 34;
        let cx = WIDTH / 2;
        let base = self.shown;
        let first = (base - 7.0).floor().max(0.0) as usize;
        let last = ((base + 8.0).ceil() as usize).min(self.cells.len());
        self.hit_cells.clear();
        for i in first..last {
            let x = cx + ((i as f32 - base) * cell_w as f32).round() as i32;
            let label = Self::label(&self.cells[i]);
            let selected = i == self.sel;
            let wide = label.chars().count() != 1;
            let size = if selected { 26 } else { 19 };
            let tw = if label.is_empty() { 20 } else { d.fonts.measure(&label, size, Weight::Regular) };
            let bw = (tw + 20).max(40);
            {
                let width = if selected { bw } else { cell_w.max(tw) };
                let hit = Rect::new(x - width / 2, strip_y - 6, width, 50).intersect(&area);
                self.hit_cells.push((i, hit));
            }
            if selected {
                let r = Rect::new(x - bw / 2, strip_y - 6, bw, 50);
                let old = d.c.set_clip(r.intersect(&area));
                d.c.fill_round_rect(r, 8, th.sel_bottom);
                d.c.set_clip(Rect::new(r.x, r.y, r.w, r.h - 1).intersect(&area));
                th.fill_selection(d.c, r);
                d.c.set_clip(old);
            }
            let col = if selected {
                th.sel_text
            } else if wide {
                th.muted
            } else {
                // Fade towards the edges.
                let dist = (i as f32 - base).abs();
                th.text.lerp(th.bg, (dist / 8.0).min(0.85))
            };
            let mid = (strip_y + 19) as f32;
            let fx = x as f32;
            match self.cells[i] {
                Cell::Delete => {
                    // A key with a left point, and a cross.
                    d.c.fill_polygon(&[(fx - 10.0, mid), (fx - 4.0, mid - 7.0), (fx + 10.0, mid - 7.0), (fx + 10.0, mid + 7.0), (fx - 4.0, mid + 7.0)], col);
                    let bg = if selected { th.sel_mid } else { th.bg };
                    d.c.fill_polygon(&[(fx - 1.0, mid - 4.0), (fx + 1.0, mid - 4.0), (fx + 7.0, mid + 3.0), (fx + 5.0, mid + 4.0)], bg);
                    d.c.fill_polygon(&[(fx + 5.0, mid - 4.0), (fx + 7.0, mid - 3.0), (fx + 1.0, mid + 4.0), (fx - 1.0, mid + 3.0)], bg);
                }
                Cell::Space => {
                    d.c.fill_rect(Rect::new(x - 9, strip_y + 24, 18, 2), col);
                    d.c.fill_rect(Rect::new(x - 9, strip_y + 18, 2, 7), col);
                    d.c.fill_rect(Rect::new(x + 7, strip_y + 18, 2, 7), col);
                }
                _ => {
                    let lh = d.fonts.line_height(size);
                    d.fonts.draw(d.c, &label, x - tw / 2, strip_y + 19 - lh / 2, size, Weight::Regular, col);
                }
            }
        }
    }

    fn animate(&mut self, _now: Instant) -> bool {
        let target = self.sel as f32;
        if (target - self.shown).abs() > 0.02 {
            // Wrapping from one end to the other jumps.
            if (target - self.shown).abs() > 10.0 {
                self.shown = target;
            } else {
                self.shown += (target - self.shown) * 0.45;
            }
            true
        } else {
            self.shown = target;
            false
        }
    }
}

/// Search: type, then hold the centre (or choose Done) for results.
pub fn search() -> Box<dyn Screen> {
    Box::new(TextEntry::new(
        "Search",
        "",
        Kind::Search,
        false,
        Box::new(|text, ctx| {
            if text.is_empty() {
                return Action::None;
            }
            Action::Push(super::open(crate::ui::Dest::SearchResults { query: text }, ctx))
        }),
    ))
}
