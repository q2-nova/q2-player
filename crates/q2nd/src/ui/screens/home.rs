//! The main menu, as the iPod classic's: the menu on the left half, and on
//! the right a slow slideshow of album art panning across, one cover
//! fading into the next.

use crate::gfx::{Color, Image, Rect, WIDTH};
use crate::input::Event;
use crate::library::Policy;
use crate::ui::list::{ListView, Row, RowAction};
use crate::ui::{Action, Ctx, Dest, Draw, Request, Screen};
use std::cell::Cell;
use std::sync::Arc;
use std::time::Duration;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use crate::clock::Instant;
use subsonic::{AlbumListType, calls};

/// The menu's width: half the screen, as the classic's.
const SPLIT_W: i32 = 187;
/// The covers are loaded larger than the panel, to pan across.
const SLIDE_ART: u32 = 320;
const SLIDE_TIME: Duration = Duration::from_secs(7);
const FADE: Duration = Duration::from_millis(900);
/// The pan moves a pixel about this often.
const FRAME: Duration = Duration::from_millis(60);

const CMD_NOW_PLAYING: u32 = 1;
const CMD_SHUFFLE: u32 = 2;
const AVAILABILITY: u64 = 1;

pub struct Home {
    list: ListView,
    /// Cover art ids for the slideshow.
    covers: Vec<String>,
    slides: Slides,
    candidates: Vec<String>,
    playing_cover: Option<String>,
    preload_cursor: usize,
    preload_at: Instant,
    last_frame: Cell<Instant>,
    loaded: bool,
    disk_books: bool,
    disk_podcasts: bool,
    availability_pending: bool,
    availability_again: bool,
}

impl Home {
    pub fn new() -> Home {
        Home {
            list: ListView::new(false),
            covers: Vec::new(),
            slides: Slides::new(),
            candidates: Vec::new(),
            playing_cover: None,
            preload_cursor: 0,
            preload_at: Instant::now() - Duration::from_secs(2),
            last_frame: Cell::new(Instant::now()),
            loaded: false,
            disk_books: false,
            disk_podcasts: false,
            availability_pending: false,
            availability_again: false,
        }
    }

    fn rows(&self, ctx: &Ctx) -> Vec<Row> {
        let mut rows = Vec::new();
        if !ctx.lib.is_configured() {
            rows.push(Row::link("Set Up Server", Dest::SettingsPage(super::settings::Page::Server)));
        }
        rows.push(Row::link("Music", Dest::Music));
        // Setup is independent of network/authentication state. A temporarily
        // unreachable configured server must not remove its navigation.
        let shelf = !ctx.config.audiobookshelf.url.trim().is_empty()
            && !ctx.config.audiobookshelf.token.trim().is_empty();
        if shelf || self.disk_books {
            rows.push(Row::link("Books", Dest::Spoken(super::spoken::Page::Books)));
        }
        if shelf || self.disk_podcasts {
            rows.push(Row::link("Podcasts", Dest::Spoken(super::spoken::Page::Podcasts)));
        }
        rows.push(Row::link("Radios", Dest::RadioStations { featured: false }));
        rows.push(Row::link("Downloads", Dest::Downloads));
        rows.push(Row::link("Settings", Dest::Settings));
        rows.push(Row::new("Shuffle Songs", RowAction::Cmd(CMD_SHUFFLE)));
        if ctx.status.current().is_some() {
            rows.push(Row { chevron: true, ..Row::new("Now Playing", RowAction::Cmd(CMD_NOW_PLAYING)) });
        }
        rows
    }

    fn check_availability(&mut self, ctx: &mut Ctx) {
        if self.availability_pending {
            self.availability_again = true;
            return;
        }
        self.availability_pending = true;
        let lib = ctx.lib.clone();
        ctx.task(AVAILABILITY, move |reply| async move { reply.send(lib.spoken_availability_async().await); });
    }

    fn apply_availability(&mut self, availability: crate::library::spoken::Availability, ctx: &Ctx) {
        if let Some(books) = availability.books { self.disk_books = books; }
        if let Some(podcasts) = availability.podcasts { self.disk_podcasts = podcasts; }
        self.list.set_rows(self.rows(ctx));
    }

    /// Covers for the slideshow: a page of random albums, cached as any.
    fn load_covers(&mut self, ctx: &mut Ctx) {
        let lib = ctx.lib.clone();
        ctx.task(0, move |reply| async move {
            let mut ids = Vec::new();
            for call in [calls::album_list(AlbumListType::Random, 30, 0), calls::album_list(AlbumListType::AlphabeticalByName, 500, 0)] {
                if let Ok(f) = lib.clone().fetch_async(call, Policy::CacheOnly).await {
                    ids.extend(f.value.album.into_iter().filter_map(|a| a.cover_art));
                }
                if !ids.is_empty() { break; }
            }
            if ids.is_empty() {
                if let Ok(sets) = lib.offline_sets_async().await { ids.extend(sets.into_iter().filter_map(|a| a.cover_art)); }
            }
            if ids.is_empty() && lib.is_online() {
                if let Ok(f) = lib.fetch_async(calls::album_list(AlbumListType::Random, 30, 0), Policy::NetworkFirst).await {
                    ids.extend(f.value.album.into_iter().filter_map(|a| a.cover_art));
                }
            }
            let mut unique = Vec::new();
            for id in ids { if !id.is_empty() && !unique.contains(&id) { unique.push(id); } if unique.len() >= 30 { break; } }
            reply.send(unique);
        });
    }

    fn panel() -> Rect {
        let top = crate::ui::theme::STATUS_H;
        Rect::new(SPLIT_W, top, WIDTH - SPLIT_W, crate::gfx::HEIGHT - top)
    }

    /// Where a slide is shown `t` (0-1) of the way through: it pans across
    /// the cover, alternating direction.
    fn offset(slide: usize, t: f32, panel: Rect) -> (i32, i32) {
        let dx = (SLIDE_ART as i32 - panel.w).max(0) as f32;
        let dy = (SLIDE_ART as i32 - panel.h).max(0) as f32;
        let t = if slide % 2 == 0 { t } else { 1.0 - t };
        ((dx * t) as i32, (dy * (1.0 - t)) as i32)
    }
}

impl Screen for Home {
    fn script_ready(&self) -> bool { !self.availability_pending }
    #[cfg(test)]
    fn audit_settle(&mut self, ctx: &mut Ctx) {
        self.availability_pending = false;
        self.availability_again = false;
        self.apply_availability(ctx.lib.spoken_availability(), ctx);
    }
    #[cfg(test)]
    fn audit(&self) -> crate::ui::hierarchy::Surface { crate::ui::hierarchy::Surface::list(&self.list) }
    #[cfg(target_arch = "wasm32")]
    fn selected_label(&self) -> Option<&str> { self.list.selected_row().map(|row| row.title.as_str()) }

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

    fn title(&self) -> String {
        "Q2".into()
    }

    fn enter(&mut self, ctx: &mut Ctx) {
        let rows = self.rows(ctx);
        self.list.set_rows(rows);
        self.check_availability(ctx);
        if !self.loaded {
            self.loaded = true;
            self.load_covers(ctx);
        }
    }

    fn event(&mut self, ev: Event, ctx: &mut Ctx) -> Action {
        match ev {
            Event::WheelEnd => { self.list.end_spin(); Action::None }
            Event::Scroll(t) => {
                self.list.wheel(t, ctx.now);
                Action::None
            }
            Event::Center => {
                let Some(row) = self.list.selected_row().cloned() else { return Action::None };
                match row.action {
                    RowAction::Open(dest) => Action::Push(super::open(dest, ctx)),
                    RowAction::Cmd(CMD_NOW_PLAYING) => Action::NowPlaying,
                    RowAction::Cmd(CMD_SHUFFLE) => super::sources::shuffle_from(ctx, None),
                    _ => Action::None,
                }
            }
            // Hold the centre on the main menu: the power menu.
            Event::CenterLong => {
                let menu = super::popup::Popup::new("Power")
                    .add("Sleep", |ctx| {
                        log::info!("sleep requested from the Power menu");
                        ctx.requests.push(Request::Sleep);
                        Action::None
                    })
                    .add("Power Off", |ctx| {
                        ctx.requests.push(Request::PowerOff);
                        Action::None
                    });
                Action::Push(Box::new(menu))
            }
            Event::Back => Action::Stay,
            _ => Action::None,
        }
    }

    fn loaded(&mut self, token: u64, data: Box<dyn std::any::Any + Send>, ctx: &mut Ctx) {
        if token == AVAILABILITY {
            self.availability_pending = false;
            if let Ok(availability) = data.downcast::<crate::library::spoken::Availability>() {
                // An intervening download mutation makes this result stale.
                // Retain known rows until the coalesced follow-up completes.
                if !self.availability_again { self.apply_availability(*availability, ctx); }
            }
            if std::mem::take(&mut self.availability_again) { self.check_availability(ctx); }
            return;
        }
        if let Ok(ids) = data.downcast::<Vec<String>>() {
            self.covers = *ids;
            self.candidates.clear();
        }
    }

    fn refresh(&mut self, ev: &crate::library::LibEvent, ctx: &mut Ctx) {
        if !matches!(ev, crate::library::LibEvent::ArtReady) {
            let rows = self.rows(ctx);
            self.list.set_rows(rows);
        }
        if matches!(ev, crate::library::LibEvent::Downloads) { self.check_availability(ctx); }
        if matches!(ev, crate::library::LibEvent::Online(true)) && self.covers.is_empty() {
            self.load_covers(ctx);
        }
    }

    fn tick(&mut self, ctx: &mut Ctx) -> bool {
        // "Now Playing" comes and goes with the queue.
        let has = self.list.rows.last().is_some_and(|r| r.title == "Now Playing");
        if has != ctx.status.current().is_some() {
            let rows = self.rows(ctx);
            self.list.set_rows(rows);
        }
        if !ctx.screen_on { return has != ctx.status.current().is_some(); }
        let playing = ctx.status.current().and_then(|s| s.cover_art.clone());
        if playing != self.playing_cover || self.candidates.is_empty() {
            self.playing_cover = playing;
            self.candidates = self.playing_cover.clone().into_iter().collect();
            for id in &self.covers {
                if !self.candidates.contains(id) { self.candidates.push(id.clone()); }
            }
        }
        // Pin the displayed images and advance only to a fully decoded one.
        // Cache eviction or a failed request cannot expose a loading image.
        let changed = self.slides.advance(&self.candidates, ctx.now, |id| ctx.lib.art.peek_cover(id, SLIDE_ART));
        if changed { self.preload_cursor = self.slides.next; self.preload_at = ctx.now - Duration::from_secs(2); }
        if !self.candidates.is_empty() && ctx.now.saturating_duration_since(self.preload_at) >= Duration::from_secs(1) {
            self.preload_at = ctx.now;
            // Try alternatives when a provider is missing/offline; don't let
            // one bad image prevent all cached images from appearing.
            let waiting = self.slides.current.is_none() || ctx.now.saturating_duration_since(self.slides.started) >= SLIDE_TIME;
            if !waiting { self.preload_cursor = self.slides.next; }
            for _ in 0..2 {
                let i = self.preload_cursor % self.candidates.len();
                ctx.lib.art.prefetch_cover(&self.candidates[i], SLIDE_ART);
                self.preload_cursor = (i + 1) % self.candidates.len();
            }
        }
        changed || has != ctx.status.current().is_some()
    }

    fn next_frame(&self, now: Instant) -> Option<Duration> {
        if self.slides.current.is_none() || now.saturating_duration_since(self.slides.started) >= SLIDE_TIME {
            return None;
        }
        Some(FRAME.saturating_sub(now.duration_since(self.last_frame.get())))
    }

    fn draw(&mut self, d: &mut Draw, area: Rect) {
        let menu = Rect::new(area.x, area.y, SPLIT_W, area.h);
        self.list.draw(d, menu);

        let now = d.now;
        self.last_frame.set(now);
        let panel = Home::panel();
        let old = d.c.set_clip(panel);
        let elapsed = now.saturating_duration_since(self.slides.started);
        let t = (elapsed.as_secs_f32() / SLIDE_TIME.as_secs_f32()).min(1.0);
        match &self.slides.current {
            Some((_, img)) => {
                let (ox, oy) = Home::offset(self.slides.sequence, t, panel);
                d.c.draw_image(img, panel.x - ox, panel.y - oy);
                if elapsed < FADE {
                    if let Some(p) = &self.slides.previous {
                        let (px, py) = Home::offset(self.slides.sequence.wrapping_sub(1), 1.0, panel);
                        let alpha = 255 - (255.0 * elapsed.as_secs_f32() / FADE.as_secs_f32()) as u32;
                        d.c.draw_image_alpha(p, panel.x - px, panel.y - py, alpha.min(255));
                    }
                }
            }
            None => {
                let ph = d.placeholder(SLIDE_ART);
                d.c.draw_image(&ph, panel.x - 66, panel.y - 15);
            }
        }
        // The menu's soft shadow onto the art.
        for i in 0..8 {
            d.c.fill_rect_alpha(Rect::new(panel.x + i, panel.y, 1, panel.h), Color::BLACK, 70 - i as u32 * 9);
        }
        d.c.set_clip(old);
    }

    fn animate(&mut self, now: Instant) -> bool {
        self.list.animate(now)
    }
}

/// Selection is independent of I/O, so failed/late loads are testable.
struct Slides {
    current: Option<(String, Arc<Image>)>,
    previous: Option<Arc<Image>>,
    next: usize,
    sequence: usize,
    started: Instant,
}
impl Slides {
    fn new() -> Self { Self { current: None, previous: None, next: 0, sequence: 0, started: Instant::now() } }
    fn advance(&mut self, ids: &[String], now: Instant, mut ready: impl FnMut(&str) -> Option<Arc<Image>>) -> bool {
        let elapsed = now.saturating_duration_since(self.started);
        if elapsed >= FADE { self.previous = None; }
        if ids.is_empty() || (self.current.is_some() && elapsed < SLIDE_TIME) { return false; }
        for offset in 0..ids.len() {
            let index = (self.next + offset) % ids.len();
            let id = &ids[index];
            if self.current.as_ref().is_some_and(|(current, _)| current == id) { continue; }
            if let Some(image) = ready(id) {
                self.previous = self.current.replace((id.clone(), image)).map(|(_, image)| image);
                self.next = (index + 1) % ids.len();
                self.sequence = self.sequence.wrapping_add(1);
                self.started = now;
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_next_cover_never_replaces_current_with_placeholder() {
        let mut slides = Slides::new();
        let now = Instant::now();
        let ids = vec!["first".into(), "missing".into(), "cached".into()];
        let image = Arc::new(Image::placeholder(320));
        assert!(slides.advance(&ids, now, |id| (id=="first").then(||image.clone())));
        assert!(!slides.advance(&ids, now + SLIDE_TIME, |_|None));
        assert!(Arc::ptr_eq(&slides.current.as_ref().unwrap().1, &image));
        assert!(slides.advance(&ids, now + SLIDE_TIME + Duration::from_secs(1), |id| (id=="cached").then(||image.clone())));
        assert_eq!(slides.current.as_ref().unwrap().0, "cached");
        assert!(slides.previous.is_some());
        assert!(!slides.advance(&ids, now + SLIDE_TIME + Duration::from_secs(2), |_|None));
        assert!(slides.previous.is_none());
    }
}
