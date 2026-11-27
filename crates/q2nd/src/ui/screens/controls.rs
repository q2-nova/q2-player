//! Shared choice and confirmation screens. Merely opening or cancelling a
//! screen never changes a setting; callbacks run only on explicit selection.
use crate::gfx::{Rect, Weight};
use crate::input::Event;
use crate::ui::list::{Badge, ListView, Row, RowAction};
use crate::ui::{Action, Ctx, Draw, Screen};
#[cfg(target_arch = "wasm32")]
use crate::clock::Instant;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;

type Selection = Box<dyn Fn(usize, &mut Ctx) -> Action>;

pub struct Choice {
    title: String,
    list: ListView,
    apply: Selection,
}

impl Choice {
    pub fn new(title: impl Into<String>, labels: Vec<String>, current: Option<usize>, apply: Selection) -> Self {
        let mut list = ListView::settings(false);
        list.set_rows(labels.into_iter().enumerate().map(|(i, label)| {
            let row = Row::new(label, RowAction::Cmd(i as u32));
            if current == Some(i) { row.badge(Badge::Check) } else { row }
        }).collect());
        list.select(current.unwrap_or(0));
        Self { title: title.into(), list, apply }
    }

    /// A second line under each choice, in order.
    pub fn details(mut self, details: Vec<String>) -> Self {
        let rows = std::mem::take(&mut self.list.rows).into_iter().zip(details).map(|(row, detail)| row.detail(detail)).collect();
        let selected = self.list.selected;
        self.list.set_rows(rows);
        self.list.select(selected);
        self
    }
}

impl Screen for Choice {
    #[cfg(target_arch = "wasm32")]
    fn selected_label(&self) -> Option<&str> {
        self.list.selected_row().map(|row| row.title.as_str())
    }
    fn clickable(&self, x: i32, y: i32) -> bool { self.list.hit(x, y).is_some() }
    fn click(&mut self, x: i32, y: i32, ctx: &mut Ctx) -> Action {
        if self.list.select_at(x, y) { self.event(Event::Center, ctx) } else { Action::None }
    }

    #[cfg(test)]
    fn audit(&self) -> crate::ui::hierarchy::Surface { crate::ui::hierarchy::Surface::list(&self.list) }
    fn title(&self) -> String { self.title.clone() }
    fn event(&mut self, ev: Event, ctx: &mut Ctx) -> Action {
        match ev {
            Event::Scroll(t) => { self.list.wheel(t, ctx.now); }
            Event::WheelEnd => self.list.end_spin(),
            Event::Center => {
                if let Some(RowAction::Cmd(i)) = self.list.selected_row().map(|r| &r.action) {
                    return (self.apply)(*i as usize, ctx);
                }
            }
            _ => {}
        }
        Action::None
    }
    fn draw(&mut self, d: &mut Draw, area: Rect) { self.list.draw(d, area); }
    fn animate(&mut self, now: Instant) -> bool { self.list.animate(now) }
}

pub struct Confirm {
    title: String,
    message: String,
    list: ListView,
    apply: Box<dyn Fn(&mut Ctx) -> Action>,
}

impl Confirm {
    pub fn clear_cache(apply: impl Fn(&mut Ctx) -> Action + 'static) -> Self {
        Self::new("Clear Song Cache?", "Remove temporarily cached songs.\nYour downloads will be kept.", "Clear Cache", apply)
    }

    pub fn reset_bands(apply: impl Fn(&mut Ctx) -> Action + 'static) -> Self {
        Self::new("Reset EQ Bands?", "Set all graphic bands to 0 dB.\nHeadphone correction is kept.", "Reset Bands", apply)
    }

    pub fn forget_network(apply: impl Fn(&mut Ctx) -> Action + 'static) -> Self {
        Self::new("Forget Network?", "Disconnect and remove this\nnetwork’s saved login.", "Forget Network", apply)
    }

    pub fn forget_device(apply: impl Fn(&mut Ctx) -> Action + 'static) -> Self {
        Self::new("Forget This Device", "Sign this player out of\nTailscale. Signing in\nagain needs approval.", "Forget", apply)
    }

    pub fn new(title: &str, message: impl Into<String>, accept: &str, apply: impl Fn(&mut Ctx) -> Action + 'static) -> Self {
        let mut list = ListView::settings(false);
        list.set_rows(vec![Row::new("Cancel", RowAction::Cmd(0)), Row::new(accept, RowAction::Cmd(1))]);
        Self { title: title.into(), message: message.into(), list, apply: Box::new(apply) }
    }
}

impl Screen for Confirm {
    #[cfg(target_arch = "wasm32")]
    fn selected_label(&self) -> Option<&str> {
        self.list.selected_row().map(|row| row.title.as_str())
    }
    fn clickable(&self, x: i32, y: i32) -> bool { self.list.hit(x, y).is_some() }
    fn click(&mut self, x: i32, y: i32, ctx: &mut Ctx) -> Action {
        if self.list.select_at(x, y) { self.event(Event::Center, ctx) } else { Action::None }
    }
    #[cfg(test)]
    fn audit(&self) -> crate::ui::hierarchy::Surface { crate::ui::hierarchy::Surface::list(&self.list) }
    fn title(&self) -> String { self.title.clone() }
    fn event(&mut self, ev: Event, ctx: &mut Ctx) -> Action {
        match ev {
            Event::Scroll(t) => { self.list.wheel(t, ctx.now); }
            Event::WheelEnd => self.list.end_spin(),
            Event::Center => return if self.list.selected == 1 { (self.apply)(ctx) } else { Action::Pop },
            _ => {}
        }
        Action::None
    }
    fn draw(&mut self, d: &mut Draw, area: Rect) {
        // Short, explicitly wrapped copy keeps the consequences visible above
        // the choices on the Q2's rounded display.
        for (i, line) in self.message.lines().enumerate() {
            d.fonts.draw_centered(d.c, line, area.x + area.w / 2, area.y + 38 + i as i32 * 25,
                17, Weight::Regular, d.theme.text);
        }
        self.list.draw(d, Rect::new(area.x, area.y + 166, area.w, 88));
    }
    fn animate(&mut self, now: Instant) -> bool { self.list.animate(now) }
}
