//! Tailscale settings and a complete, wrapped browser approval link.
use crate::gfx::{Canvas, Color, Fonts, Rect, Weight, qr::Code};
use crate::input::Event;
use crate::ui::list::{ListView, Row, RowAction};
use crate::ui::{Action, Ctx, Draw, Request, Screen};
use std::time::Instant;

pub struct Tailscale { list: ListView }
impl Tailscale {
    pub fn new() -> Self { Self { list: ListView::settings(true) } }
    fn rows(ctx: &Ctx) -> Vec<Row> {
        let enabled = ctx.config.tailscale.enabled;
        let s = ctx.tailscale.state();
        let mut rows = vec![Row::new("Tailscale", RowAction::Cmd(1)).value(if enabled { "On" } else { "Off" })];
        if enabled {
            rows.push(Row::new("Status", RowAction::None).detail(s.status));
            if !s.address.is_empty() { rows.push(Row::new("Tailscale Address", RowAction::None).detail(s.address)); }
            rows.push(Row::screen("Sign In", 2).detail("Approve on your phone or computer"));
            if !s.health.is_empty() { rows.push(Row::screen("Connection Details", 3).detail(s.health)); }
            // The daemon is the image's: no choice here.
            rows.push(Row::screen("Forget This Device", 5).detail("Sign out; signing in again needs approval"));
        }
        rows
    }
}
impl Screen for Tailscale {

    fn clickable(&self, x: i32, y: i32) -> bool {
        self.list.hit(x, y).is_some()
    }

    fn click(&mut self, x: i32, y: i32, ctx: &mut Ctx) -> Action {
        if self.list.select_at(x, y) {
            self.event(Event::Center, ctx)
        } else {
            Action::None
        }
    }

    #[cfg(test)]
    fn audit(&self) -> crate::ui::hierarchy::Surface { crate::ui::hierarchy::Surface::list(&self.list) }
    fn title(&self) -> String { "Tailscale".into() }
    fn enter(&mut self, ctx: &mut Ctx) { self.list.set_rows(Self::rows(ctx)); }
    fn event(&mut self, ev: Event, ctx: &mut Ctx) -> Action {
        match ev {
            Event::Scroll(t) => { self.list.wheel(t, ctx.now); }
            Event::WheelEnd => self.list.end_spin(),
            Event::Center => match self.list.selected_row().map(|r| &r.action) {
                Some(RowAction::Cmd(1)) => {
                    ctx.config.tailscale.enabled = !ctx.config.tailscale.enabled;
                    if ctx.config.tailscale.enabled && ctx.lib.offline_mode() {
                        ctx.lib.set_offline_mode(false);
                        ctx.requests.push(Request::OfflineMode { offline: false, requested: ctx.config.wifi.enabled, networks: ctx.config.wifi.networks.clone() });
                    }
                    ctx.tailscale.set_enabled(ctx.config.tailscale.enabled);
                    *ctx.config_changed = true;
                    self.list.set_rows(Self::rows(ctx));
                }
                Some(RowAction::Cmd(2)) => {
                    ctx.tailscale.login();
                    return Action::Push(Box::new(Details::new(true)));
                }
                Some(RowAction::Cmd(3)) => return Action::Push(Box::new(Details::new(false))),
                Some(RowAction::Cmd(5)) => {
                    return Action::Push(Box::new(super::controls::Confirm::forget_device(|ctx| { ctx.tailscale.logout(); Action::Pop })));
                }
                Some(RowAction::Open(dest)) => return Action::Push(super::open(dest.clone(), ctx)),
                _ => {}
            },
            _ => {}
        }
        Action::None
    }
    fn tailscale_changed(&mut self, ctx: &mut Ctx) { self.list.set_rows(Self::rows(ctx)); }
    fn draw(&mut self, d: &mut Draw, area: Rect) { self.list.draw(d, area); }
    fn animate(&mut self, now: Instant) -> bool { self.list.animate(now) }
}

struct Details { login: bool, text: String, auth_url: String, qr: Option<Code>, scroll: usize, lines: usize }
impl Details {
    fn new(login: bool) -> Self { Self { login, text: String::new(), auth_url: String::new(), qr: None, scroll: 0, lines: 0 } }
    fn update(&mut self, ctx: &Ctx) -> bool {
        let s = ctx.tailscale.state();
        let text = if self.login && !s.auth_url.is_empty() {
            format!("Open on your phone or computer:\n\n{}\n\nReturn here after approval.", s.auth_url)
        } else if self.login && s.status == "Connected" {
            format!("Connected\n\n{}\n\nSet your Navidrome tailnet address in Settings > Connections > Navidrome Server.", s.address)
        } else {
            format!("{}\n\n{}", s.status, if s.health.is_empty() { "Connect Wi-Fi and wait for a sign-in link." } else { &s.health })
        };
        if text == self.text { return false; }
        let auth_url = if self.login { s.auth_url } else { String::new() };
        if auth_url != self.auth_url {
            self.qr = Code::for_url(&auth_url);
            self.auth_url = auth_url;
        }
        self.text = text;
        self.scroll = 0;
        true
    }
}
impl Screen for Details {
    #[cfg(test)]
    fn audit(&self) -> crate::ui::hierarchy::Surface { crate::ui::hierarchy::Surface::widget("Details", format!("login={};scroll={}", self.login, self.scroll)) }
    fn title(&self) -> String { if self.login { "Sign In" } else { "Connection Details" }.into() }
    fn enter(&mut self, ctx: &mut Ctx) { self.update(ctx); }
    fn tailscale_changed(&mut self, ctx: &mut Ctx) { self.update(ctx); }
    fn event(&mut self, ev: Event, _ctx: &mut Ctx) -> Action {
        if let Event::Scroll(n) = ev { self.scroll = (self.scroll as i32 + n).clamp(0, self.lines.saturating_sub(1) as i32) as usize; }
        Action::None
    }
    fn draw(&mut self, d: &mut Draw, area: Rect) {
        if let Some(qr) = &self.qr {
            if qr.side((area.h - 120).min(160)) > 0 {
                self.lines = draw_login(d.c, d.fonts, area, &self.auth_url, qr,
                    self.scroll, d.theme.text, d.theme.muted);
                return;
            }
        }
        let lines = wrap(d.fonts, &self.text, 17, area.w - 32);
        self.lines = lines.len();
        for (i, line) in lines.iter().skip(self.scroll).take(((area.h - 32) / 24) as usize).enumerate() {
            d.fonts.draw(d.c, line, area.x + 16, area.y + 12 + i as i32 * 24, 17, Weight::Regular, d.theme.text);
        }
        if lines.len() > ((area.h - 32) / 24) as usize {
            d.fonts.draw_centered(d.c, "Scroll for more", area.x + area.w / 2, area.y + area.h - 20, 12, Weight::Regular, d.theme.muted);
        }
    }
}

fn wrap(fonts: &mut Fonts, text: &str, size: u16, width: i32) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        for c in paragraph.chars() {
            let trial = format!("{line}{c}");
            if fonts.measure(&trial, size, Weight::Regular) > width && !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            line.push(c);
        }
        lines.push(line);
    }
    lines
}

fn draw_login(canvas: &mut Canvas, fonts: &mut Fonts, area: Rect, url: &str,
    qr: &Code, scroll: usize, text: Color, muted: Color) -> usize {
    let maximum = (area.h - 120).min(160);
    let side = qr.side(maximum);
    fonts.draw_centered(canvas, "Scan with your phone", area.x + area.w / 2,
        area.y + 8, 16, Weight::Regular, text);
    qr.draw(canvas, area.x + (area.w - side) / 2, area.y + 32, maximum);
    let top = area.y + 32 + side + 10;
    let footer = area.bottom() - 24;
    let rows = ((footer - top - 6) / 18).max(1) as usize;
    let lines = wrap(fonts, url, 14, area.w - 48);
    for (i, line) in lines.iter().skip(scroll).take(rows).enumerate() {
        fonts.draw(canvas, line, area.x + 24, top + i as i32 * 18, 14, Weight::Regular, text);
    }
    let hint = if lines.len() > rows { "Scroll for the full link" } else { "Or open the link above" };
    fonts.draw_centered(canvas, hint, area.x + area.w / 2, footer, 12, Weight::Regular, muted);
    lines.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    const URL: &str = "https://login.tailscale.com/a/example";

    #[test]
    fn login_details_follow_publication_and_preserve_scroll_for_unchanged_text() {
        let mut app = crate::ui::settings_tests::fixture();
        let id = app.stack.last().unwrap().0;
        let mut details = Details::new(true);
        app.tailscale.publish_generated(crate::tailscale::State {status: "Sign In Required".into(), auth_url: URL.into(), ..Default::default()});
        app.with_screen(id, |_, ctx| details.enter(ctx));
        assert_eq!(details.auth_url, URL); assert!(details.qr.is_some());
        details.scroll = 2;
        app.with_screen(id, |_, ctx| {details.tailscale_changed(ctx); assert!(!details.tick(ctx));});
        assert_eq!(details.scroll, 2);
        app.tailscale.publish_generated(crate::tailscale::State {status: "Connected".into(), address: "100.64.0.1".into(), ..Default::default()});
        app.with_screen(id, |_, ctx| details.tailscale_changed(ctx));
        assert!(details.text.starts_with("Connected"));
        assert!(details.auth_url.is_empty()); assert!(details.qr.is_none()); assert_eq!(details.scroll, 0);
        app.player.shutdown();
    }

    #[test]
    fn full_url_is_preserved_when_wrapped_and_preview_fits_the_screen() {
        let mut fonts = Fonts::bundled();
        let long = format!("{URL}?token={}", "generated-example-".repeat(16));
        assert_eq!(wrap(&mut fonts, &long, 14, 327).concat(), long);
        for (name, bg, fg) in [("light", Color::WHITE, Color::BLACK),
            ("dark", Color(0x151515), Color::WHITE)] {
            let mut canvas = Canvas::new(375, 320);
            canvas.clear(bg);
            fonts.draw_centered(&mut canvas, "Sign In", 187, 5, 17, Weight::Bold, fg);
            let qr = Code::for_url(URL).unwrap();
            let count = draw_login(&mut canvas, &mut fonts, Rect::new(0, 30, 375, 290),
                URL, &qr, 0, fg, Color(0x888888));
            assert!(count <= 3, "normal login link is fully visible beside the QR");
            crate::gfx::mask_corners(&mut canvas.pixels);
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join(format!("../../build/perf/qr-login-{name}.png"));
            let pixels: Vec<u8> = canvas.pixels.iter().flat_map(|p|
                [(*p >> 16) as u8, (*p >> 8) as u8, *p as u8]).collect();
            image::save_buffer(path, &pixels, 375, 320, image::ColorType::Rgb8).unwrap();
        }
    }
}
