//! Now Playing, as the iPod classic's: "1 of 12", the cover with its
//! reflection, the title, artist and album beside it, the progress bar and
//! its times. The wheel is the volume; the centre cycles the wheel through
//! scrubbing and rating (spoken speed for books/podcasts), and back after idle.

use super::popup::Popup;
use crate::audio::player::{Cmd, Origin, PlayState, Repeat};
use crate::gfx::{Color, Image, Rect, WIDTH, Weight};
use crate::input::Event;
use crate::ui::{Action, Ctx, Dest, Draw, Screen, duration, icons, progress_bar, volume_bar};
use std::time::Duration;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use crate::clock::Instant;
use subsonic::ItemKind;

const MARGIN: i32 = 14;
const ART: i32 = crate::library::art::NOW_PLAYING_SIZE as i32;
const ART_Y: i32 = 28;
const REFLECTION: i32 = 42;
const TEXT_X: i32 = ART + 2 * MARGIN;
const BAR_Y: i32 = 236;
const BAR_X: i32 = 30;
const TIME_Y: i32 = 250;
const SLEEP_Y: i32 = 270;
/// A scrub seeks once the wheel rests this long.
const SCRUB_SETTLE: Duration = Duration::from_millis(500);
/// Scrubbing or rating returns to the volume after this long untouched.
const MODE_IDLE: Duration = Duration::from_secs(4);

/// Precision at low speed; deliberate sustained turns traverse long tracks.
/// Uses acquisition times, never frame timing, and has no momentum.
#[derive(Default)]
struct ScrubWheel {
    run: Option<(i32, Instant)>,
    last: Option<Instant>,
    speed: f32,
}

impl ScrubWheel {
    fn reset(&mut self) { *self = Self::default(); }

    fn delta(&mut self, ticks: i32, now: Instant, duration: u64) -> i64 {
        if ticks == 0 { return 0; }
        let continuing = self.run.is_some_and(|(dir, _)| dir == ticks.signum())
            && self.last.is_some_and(|t| now.saturating_duration_since(t) < Duration::from_millis(250));
        let mut step = 1000;
        if continuing {
            let dt = now.saturating_duration_since(self.last.unwrap()).as_secs_f32();
            if dt >= 0.001 {
                let measured = (36.0 * ticks.unsigned_abs() as f32 / dt).min(2160.0);
                // Decelerate immediately so slowing down restores precision.
                self.speed = if self.speed == 0.0 || measured < self.speed { measured }
                    else { (self.speed + measured) * 0.5 };
                if now.saturating_duration_since(self.run.unwrap().1) >= Duration::from_millis(200) {
                    let cap = (duration / 120).clamp(1000, 60_000);
                    step = ((self.speed / 180.0).powi(2).round() as u64 * 1000).clamp(1000, cap);
                }
            }
        } else {
            self.run = Some((ticks.signum(), now));
            self.speed = 0.0;
        }
        self.last = Some(now);
        i64::from(ticks) * step as i64
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Volume,
    Speed,
    /// The position chosen, and whether it is yet to be sought.
    Scrub { pos: u64, pending: bool },
    /// The rating chosen, 0-5.
    Rate(u8),
    Heart(bool),
}

pub struct NowPlaying {
    mode: Mode,
    generation: u64,
    seek_ack: Option<u64>,
    touched: Instant,
    /// The reflection, made once per cover.
    reflection: Option<(usize, Image)>,
    scrub_wheel: ScrubWheel,
    speed: std::rc::Rc<std::cell::RefCell<super::spoken::PlaybackSpeed>>,
    shown_sleep: Option<(u64, bool)>,
    /// Buffering since (generation, when) and the stage shown: 0 nothing
    /// yet, 1 "Buffering…", 2 past the deadline.
    waiting: Option<(u64, Instant, u8)>,
    playback_target: Option<Rect>,
}

/// A song that has not started shows that it is buffering after this.
const WAIT_SHOWN: Duration = Duration::from_secs(1);
/// After this the wait says so and is reported, like a page load (UX-006);
/// audio arriving later still plays.
const WAIT_DEADLINE: Duration = Duration::from_secs(15);

impl NowPlaying {
    pub fn new() -> NowPlaying {
        NowPlaying { seek_ack: None, generation: u64::MAX, mode: Mode::Volume, touched: Instant::now(), reflection: None, scrub_wheel: ScrubWheel::default(), speed: std::rc::Rc::new(std::cell::RefCell::new(super::spoken::PlaybackSpeed::current())), shown_sleep: None, waiting: None,
            playback_target: None,
        }
    }

    /// Times a buffering song (not a station); true when what is shown changes.
    fn tick_waiting(&mut self, ctx: &Ctx) -> bool {
        let st = &ctx.status;
        let buffering = st.state == PlayState::Buffering && st.current().is_some_and(|song| song.radio_url.is_none());
        if !buffering { return self.waiting.take().is_some_and(|(_, _, stage)| stage > 0); }
        let (generation, since, stage) = match self.waiting {
            Some(waiting) if waiting.0 == st.generation => waiting,
            _ => (st.generation, ctx.now, 0),
        };
        let waited = ctx.now.saturating_duration_since(since);
        let next = if waited >= WAIT_DEADLINE { 2 } else if waited >= WAIT_SHOWN { 1 } else { 0 };
        if next == 2 && stage < 2 {
            crate::ux::fail(crate::ux::Code::Stuck, crate::ux::pending_audio().unwrap_or(crate::ux::Journey::Buffering), 0);
        }
        self.waiting = Some((generation, since, next));
        next != stage
    }

    fn sync_track(&mut self, generation: u64) -> bool {
        if self.generation == generation { return false; }
        self.generation = generation;
        self.mode = Mode::Volume;
        self.seek_ack = None;
        self.scrub_wheel.reset();
        true
    }

    fn scrub(&mut self, ctx: &Ctx, position: Option<u64>, active: bool) {
        if position.is_some() { self.seek_ack = Some(ctx.status.seek_revision.wrapping_add(1)); }
        ctx.player.send(Cmd::Scrub { generation: self.generation, position, active });
    }

    /// Leaves scrubbing or rating, applying what was chosen.
    fn finish(&mut self, ctx: &mut Ctx) {
        self.sync_track(ctx.status.generation);
        match self.mode {
            Mode::Scrub { pos, pending } => self.scrub(ctx, pending.then_some(pos), false),
            Mode::Heart(on) => {
                if let Some(song) = ctx.status.current() {
                    if on != ctx.lib.loved(song) {
                        ctx.music_action(ctx.lib.star(ItemKind::Song, &song.id, on), None);
                    }
                }
            }
            Mode::Rate(r) => {
                if let Some(song) = ctx.status.current() {
                    if r != ctx.lib.rating(song) {
                        ctx.music_action(ctx.lib.rate(&song.id, r), None);
                    }
                }
            }
            _ => {}
        }
        self.mode = Mode::Volume;
        self.scrub_wheel.reset();
    }

    fn menu(&self, ctx: &mut Ctx) -> Popup {
        let st = ctx.status;
        if st.current().is_some_and(|s|s.spoken.is_some()){return super::spoken::playback_menu(ctx);}
        if st.current().is_some_and(|s| s.radio_url.is_some()) {
            return Popup::new("Radio").add("Brightness", |ctx| Action::Push(super::open(Dest::SettingsPage(super::settings::Page::Brightness),ctx)))
                .add("Equalizer", |ctx| Action::Push(super::open(Dest::Equalizer, ctx)))
                .add("Sleep Timer", |ctx| Action::Push(super::settings::sleep_timer_choice(ctx)))
                .add("Stations", |ctx| Action::Push(super::open(Dest::RadioStations { featured:false },ctx)))
                .report_problem();
        }
        let shuffle = st.shuffle;
        let repeat = st.repeat;
        let mut p = Popup::new("Now Playing")
            .add("Brightness", |ctx| Action::Push(super::open(Dest::SettingsPage(super::settings::Page::Brightness), ctx)))
            .add("Equalizer", |ctx| Action::Push(super::open(Dest::Equalizer, ctx)))
            .add("Sleep Timer", |ctx| Action::Push(super::settings::sleep_timer_choice(ctx)))
            .add("Up Next", |ctx| Action::Push(super::open(Dest::Queue, ctx)))
            .add(if shuffle { "Shuffle: Songs" } else { "Shuffle: Off" }, move |ctx| {
                ctx.player.send(Cmd::SetShuffle(!shuffle));
                Action::None
            })
            .add(
                match repeat {
                    Repeat::Off => "Repeat: Off",
                    Repeat::All => "Repeat: All",
                    Repeat::One => "Repeat: One",
                },
                move |ctx| {
                    let next = match repeat {
                        Repeat::Off => Repeat::One,
                        Repeat::One => Repeat::All,
                        Repeat::All => Repeat::Off,
                    };
                    ctx.player.send(Cmd::SetRepeat(next));
                    Action::None
                },
            );
        if let Some(song) = st.current().cloned() {
            let loved = ctx.lib.loved(&song);
            let id = song.id.clone();
            p = p.add(if loved { "Unlove" } else { "Love" }, move |ctx| {
                ctx.music_action(ctx.lib.star(ItemKind::Song, &id, !loved), Some(if loved { "Unloved" } else { "Loved" }.into()));
                Action::None
            });
            if let Some(album_id) = song.album_id.clone() {
                let name = song.album.clone();
                p = p.add("Browse Album", move |ctx| {
                    Action::Push(super::open(Dest::Album { id: album_id.clone(), name: name.clone() }, ctx))
                });
            }
            if let Some(artist_id) = song.artist_id.clone() {
                let name = song.artist.clone();
                p = p.add("Browse Artist", move |ctx| {
                    Action::Push(super::open(Dest::Artist { id: artist_id.clone(), name: name.clone() }, ctx))
                });
            }
            if ctx.lib.is_online() {
                let (id, title) = (song.id.clone(), song.title.clone());
                p = p.add("Similar Songs", move |ctx| {
                    Action::Push(super::open(Dest::Similar { id: id.clone(), title: title.clone() }, ctx))
                });
            }
        }
        p.report_problem()
    }

    /// The cover mirrored below itself, fading into the background.
    fn reflect(art: &Image, bg: Color) -> Image {
        let w = art.width as usize;
        let h = REFLECTION as usize;
        let mut pixels = Vec::with_capacity(w * h);
        for y in 0..h {
            let src = art.height as usize - 1 - y;
            let alpha = (110.0 * (1.0 - y as f32 / h as f32)) as u32;
            for x in 0..w {
                pixels.push(Color(art.pixels[src * w + x]).over(bg, alpha).0);
            }
        }
        Image { width: art.width, height: h as u32, pixels }
    }
}

impl Screen for NowPlaying {
    fn clickable(&self, x: i32, y: i32) -> bool {
        self.playback_target.is_some_and(|r| {
            (r.x..r.right()).contains(&x) && (r.y..r.bottom()).contains(&y)
        })
    }

    fn click(&mut self, x: i32, y: i32, ctx: &mut Ctx) -> Action {
        if self.clickable(x, y) && ctx.status.current().is_some() {
            Action::TogglePlayback
        } else {
            Action::None
        }
    }

    #[cfg(test)]
    fn audit(&self) -> crate::ui::hierarchy::Surface {
        let state = if self.mode == Mode::Speed { format!("Speed({})", self.speed.borrow().audit().state()) } else { format!("{:?}", self.mode) };
        crate::ui::hierarchy::Surface::widget("NowPlaying", state).contextual()
    }
    fn title(&self) -> String {
        "Now Playing".into()
    }

    fn enter(&mut self, ctx: &mut Ctx) { self.speed = ctx.spoken_speed.clone(); }

    fn is_now_playing(&self) -> bool {
        true
    }

    fn player_redraw(&self, old: &crate::audio::player::Status, next: &crate::audio::player::Status, volume_visible: bool) -> bool {
        // Keep transport acknowledgements and visible metadata immediate. Only
        // ordinary progress is paced by the visible-seconds requirement.
        if old.duration_ms != next.duration_ms || old.seek_revision != next.seek_revision
            || old.repeat != next.repeat || old.shuffle != next.shuffle
            || old.origin != next.origin || old.format != next.format
            || old.radio_title != next.radio_title || old.sample_rate != next.sample_rate
            || self.seek_ack.is_some() { return true; }
        let buffering = |s: &crate::audio::player::Status| s.state == PlayState::Buffering && s.position_ms > 0;
        if buffering(old) != buffering(next) { return true; }
        match self.mode {
            Mode::Scrub { .. } => return true,
            Mode::Volume if !volume_visible => {}
            // These controls replace the progress bar and its time labels.
            Mode::Volume | Mode::Rate(_) | Mode::Heart(_) | Mode::Speed => return false,
        }
        if next.current().is_some_and(|song| song.radio_url.is_some()) { return false; }
        old.position_ms / 1000 != next.position_ms / 1000
            || old.duration_ms.saturating_sub(old.position_ms) / 1000
                != next.duration_ms.saturating_sub(next.position_ms) / 1000
    }

    fn wheel_is_volume(&self) -> bool {
        self.mode == Mode::Volume
    }

    fn event(&mut self, ev: Event, ctx: &mut Ctx) -> Action {
        self.sync_track(ctx.status.generation);
        let Some(song) = ctx.status.current().cloned() else { return Action::None };
        match ev {
            Event::Scroll(t) => {
                self.touched = ctx.now;
                match &mut self.mode {
                    Mode::Scrub { pos, pending } => {
                        let dur = ctx.status.duration_ms;
                        let delta = self.scrub_wheel.delta(t, ctx.now, dur);
                        let p = pos.saturating_add_signed(delta);
                        *pos = crate::audio::player::seek_position(p, dur);
                        if *pos == 0 || *pos != p { self.scrub_wheel.reset(); }
                        *pending = true;
                    }
                    Mode::Heart(on) => { if t != 0 { *on = t > 0; } }
                    Mode::Rate(r) => *r = (*r as i32 + t).clamp(0, 5) as u8,
                    Mode::Speed => { self.speed.borrow_mut().event(Event::Scroll(t), ctx); }
                    Mode::Volume => {}
                }
                if matches!(self.mode, Mode::Scrub { .. }) { self.scrub(ctx, None, true); }
                Action::None
            }
            // Spoken titles use speed after seeking; music keeps rating controls.
            Event::WheelEnd => { self.scrub_wheel.reset(); Action::None }
            Event::Center if song.radio_url.is_some() => Action::Stay,
            Event::Center => {
                self.scrub_wheel.reset();
                self.touched = ctx.now;
                self.mode = match self.mode {
                    Mode::Volume => {
                        self.scrub(ctx, None, true);
                        Mode::Scrub { pos: ctx.status.position_ms, pending: false }
                    },
                    Mode::Scrub { pos, pending } => {
                        self.scrub(ctx, pending.then_some(pos), false);
                        if song.spoken.is_some() { self.speed.borrow_mut().enter(ctx); Mode::Speed }
                        else if ctx.config.display.rating_controls == crate::config::RatingControls::Hearts { Mode::Heart(ctx.lib.loved(&song)) }
                        else { Mode::Rate(ctx.lib.rating(&song)) }
                    }
                    Mode::Rate(_) => {
                        self.finish(ctx);
                        if ctx.config.display.rating_controls == crate::config::RatingControls::Both { Mode::Heart(ctx.lib.loved(&song)) }
                        else { Mode::Volume }
                    }
                    Mode::Heart(_) | Mode::Speed => { self.finish(ctx); Mode::Volume }
                };
                Action::None
            }
            Event::CenterLong => {
                self.finish(ctx);
                Action::Push(Box::new(self.menu(ctx)))
            }
            Event::Back if self.mode != Mode::Volume => {
                self.finish(ctx);
                Action::Stay
            }
            _ => Action::None,
        }
    }

    fn tick(&mut self, ctx: &mut Ctx) -> bool {
        if self.sync_track(ctx.status.generation) { return true; }
        if self.seek_ack.is_some_and(|rev| ctx.status.seek_revision >= rev) { self.seek_ack = None; }
        if self.seek_ack.is_none() {
            if let Mode::Scrub { pos, pending: false } = &mut self.mode { *pos = ctx.status.position_ms; }
        }
        let speed_changed = self.speed.borrow_mut().tick(ctx);
        let waiting_changed = self.tick_waiting(ctx);
        let sleep = sleep_display(*ctx.sleep_timer, *ctx.sleep_fading, ctx.now);
        let sleep_changed = ctx.screen_on && self.shown_sleep != sleep;
        if ctx.screen_on { self.shown_sleep = sleep; }
        let idle = ctx.now.duration_since(self.touched);
        if let Mode::Scrub { pos, pending: true } = self.mode {
            if idle > SCRUB_SETTLE {
                self.scrub(ctx, Some(pos), true);
                self.mode = Mode::Scrub { pos, pending: false };
            }
        }
        if self.mode != Mode::Volume && idle > MODE_IDLE {
            self.finish(ctx);
            return true;
        }
        speed_changed || sleep_changed || waiting_changed
    }

    fn draw(&mut self, d: &mut Draw, area: Rect) {
        { self.playback_target = None; }
        let st = d.status;
        self.sync_track(st.generation);
        let Some(song) = st.current().cloned() else {
            d.message(area, "Nothing playing");
            return;
        };
        let th = d.theme;
        let top = area.y;
        if let Some((seconds, fading)) = d.sleep_timer.and_then(|(deadline, fading)| sleep_display(Some(deadline), fading, d.now)) {
            let text = sleep_text(seconds, fading);
            d.fonts.draw_centered(d.c, &text, WIDTH / 2, top + SLEEP_Y, 14, Weight::Bold, th.text);
        }

        // Failures need the whole display width: a single line beside the
        // cover hides the useful part of transport/decoder/output errors.
        if let (Some(error), PlayState::Stopped | PlayState::Paused) = (&st.error, st.state) {
            let width = WIDTH - 64;
            d.fonts.draw_fit(d.c, &song.title, 32, top + 28, width, 19, Weight::Bold, th.text);
            d.fonts.draw(d.c, "Playback stopped", 32, top + 64, 16, Weight::Bold, th.muted);
            for (i, line) in wrap_lines(d, error, 17, width).iter().take(3).enumerate() {
                d.fonts.draw(d.c, line, 32, top + 96 + i as i32 * 21, 17, Weight::Bold, th.text);
            }
            let retry = "Press Play to retry";
            {
                let width = d.fonts.measure(retry, 15, Weight::Bold);
                self.playback_target = Some(Rect::new((WIDTH - width) / 2, top + BAR_Y, width, d.fonts.line_height(15)));
            }
            d.fonts.draw_centered(d.c, retry, WIDTH / 2, top + BAR_Y, 15, Weight::Bold, th.muted);
            return;
        }

        // "3 of 12", and what the decoder sees.
        let count = if song.radio_url.is_some() { "Live Radio".into() } else { format!("{} of {}", st.index + 1, st.queue.len()) };
        d.fonts.draw(d.c, &count, MARGIN + 4, top + 5, 14, Weight::Bold, th.muted);
        let mut right = WIDTH - MARGIN - 4;
        if let Some(f) = &st.format {
            let w = d.fonts.measure(f, 13, Weight::Regular);
            d.fonts.draw(d.c, f, right - w, top + 6, 13, Weight::Regular, th.muted);
            right -= w + 6;
        }
        match st.origin {
            Some(Origin::Offline) => icons::download(d.c, (right - 11) as f32, (top + 9) as f32, 11.0, th.muted),
            Some(Origin::Streaming) => icons::wifi(d.c, (right - 7) as f32, (top + 20) as f32, 3, th.muted, th.muted),
            _ => {}
        }

        // The cover and its reflection.
        let art = song.cover_art.as_deref().and_then(|id| d.lib.art.get(id, ART as u32));
        let img = art.unwrap_or_else(|| d.placeholder(ART as u32));
        let key = std::sync::Arc::as_ptr(&img) as usize;
        if self.reflection.as_ref().is_none_or(|(k, _)| *k != key) {
            self.reflection = Some((key, NowPlaying::reflect(&img, th.bg)));
        }
        {
            self.playback_target = Some(Rect::new(MARGIN, top + ART_Y, img.width as i32, img.height as i32).intersect(&area));
        }
        d.c.draw_image(&img, MARGIN, top + ART_Y);
        if let Some((_, r)) = &self.reflection {
            d.c.draw_image(r, MARGIN, top + ART_Y + ART + 1);
        }

        // Title, artist, album.
        let (title, artist, album) = track_text(&song, st.radio_title.as_deref());
        let tw = WIDTH - TEXT_X - MARGIN - 4;
        let lines = wrap2(d, title, 19, tw);
        let mut y = top + ART_Y + if lines.len() > 1 { 18 } else { 32 };
        for l in &lines {
            d.fonts.draw(d.c, l, TEXT_X, y, 19, Weight::Bold, th.text);
            y += 24;
        }
        y += 4;
        d.fonts.draw_fit(d.c, artist, TEXT_X, y, tw, 16, Weight::Regular, th.text);
        y += 22;
        d.fonts.draw_fit(d.c, album, TEXT_X, y, tw, 16, Weight::Regular, th.muted);
        y += 30;
        // Shuffle, repeat and love, small, when on.
        let mut ix = TEXT_X as f32;
        if st.shuffle {
            icons::shuffle(d.c, ix, y as f32, 16.0, th.muted);
            ix += 26.0;
        }
        if st.repeat != Repeat::Off {
            icons::repeat(d.c, ix, y as f32, 16.0, st.repeat == Repeat::One, th.muted);
            ix += 26.0;
        }
        if d.lib.loved(&song) {
            icons::heart(d.c, ix + 8.0, y as f32 + 8.0, 15.0, true, th.muted, th.bg);
        }

        // The bar: volume while it changes, else the scrubber, the rating or
        // the progress.
        let bar = Rect::new(BAR_X, top + BAR_Y, WIDTH - 2 * BAR_X, 10);
        let dur = st.duration_ms.max(1);
        let stage = self.waiting.filter(|waiting| waiting.0 == st.generation).map_or(0, |waiting| waiting.2);
        if st.state == PlayState::Buffering && song.radio_url.is_none() {
            if stage == 2 {
                d.fonts.draw_centered(d.c, crate::ux::Code::Stuck.message(), WIDTH / 2, top + BAR_Y - 16, 13, Weight::Bold, th.muted);
            } else if st.position_ms > 0 || self.seek_ack.is_some() || stage == 1 {
                d.fonts.draw_centered(d.c, "Buffering…", WIDTH / 2, top + BAR_Y - 16, 13, Weight::Bold, th.muted);
            }
        }
        if let (Some(v), Mode::Volume) = (d.volume, self.mode) {
            volume_bar(d.c, &th, Rect::new(BAR_X - 6, top + BAR_Y - 6, WIDTH - 2 * BAR_X + 12, 22), v, th.text);
            return;
        }
        match self.mode {
            Mode::Speed => self.speed.borrow().draw_inline(d, bar),
            Mode::Heart(on) => {
                icons::heart(d.c, (WIDTH / 2) as f32, (top + BAR_Y) as f32, 22.0, on, th.text, th.bg);
                d.fonts.draw_centered(d.c, if on { "Loved" } else { "Not Loved" }, WIDTH / 2, top + TIME_Y + 4, 13, Weight::Bold, th.muted);
            }
            Mode::Rate(r) => {
                let cx = WIDTH / 2;
                for i in 0..5u8 {
                    let x = (cx - 60 + i as i32 * 30) as f32;
                    icons::star(d.c, x, (top + BAR_Y + 4) as f32, 11.0, i < r, th.text);
                }
                d.fonts.draw_centered(d.c, "Rating", cx, top + TIME_Y + 4, 13, Weight::Bold, th.muted);
            }
            Mode::Scrub { pos, .. } => {
                progress_bar(d.c, &th, bar, pos, dur);
                // The classic's diamond at the position.
                let x = (bar.x + 1) as f32 + (bar.w - 2) as f32 * pos.min(dur) as f32 / dur as f32;
                let cy = bar.y as f32 + bar.h as f32 / 2.0;
                d.c.fill_polygon(&[(x, cy - 9.0), (x + 7.0, cy), (x, cy + 9.0), (x - 7.0, cy)], th.text);
                d.c.fill_polygon(&[(x, cy - 6.5), (x + 4.5, cy), (x, cy + 6.5), (x - 4.5, cy)], th.bg);
                times(d, pos, dur, top);
            }
            Mode::Volume => {
                if song.radio_url.is_some() {
                    d.fonts.draw_centered(d.c, if st.state == PlayState::Playing { "LIVE" } else if st.state == PlayState::Buffering { "Connecting…" } else { "Paused · Play to reconnect" }, WIDTH / 2, top + BAR_Y, 15, Weight::Bold, th.muted);
                } else { progress_bar(d.c, &th, bar, st.position_ms, dur); }
                let pos = st.position_ms;
                if song.radio_url.is_none() { times(d, pos, dur, top); }
            }
        }
    }
}

/// Ceil to the next whole second, so a positive remainder never reads zero.
fn sleep_display(deadline: Option<Instant>, fading: bool, now: Instant) -> Option<(u64, bool)> {
    deadline.map(|deadline| {
        let remaining = deadline.saturating_duration_since(now);
        (remaining.as_secs().saturating_add(u64::from(remaining.subsec_nanos() != 0)), fading)
    })
}

fn sleep_text(seconds: u64, fading: bool) -> String {
    let prefix = if fading { "Fading out" } else { "Sleep in" };
    if seconds >= 3600 {
        format!("{prefix} {}:{:02}:{:02}", seconds / 3600, seconds / 60 % 60, seconds % 60)
    } else {
        format!("{prefix} {}:{:02}", seconds / 60, seconds % 60)
    }
}

fn track_text<'a>(song: &'a subsonic::Song, radio_title: Option<&'a str>) -> (&'a str, &'a str, &'a str) {
    if let Some(info) = radio_title.filter(|s| !s.trim().is_empty()).filter(|_| song.radio_url.is_some()) {
        if let Some((artist, title)) = info.split_once(" - ").filter(|(a, t)| !a.trim().is_empty() && !t.trim().is_empty()) {
            (title.trim(), artist.trim(), &song.title)
        } else {
            (info, "", &song.title)
        }
    } else {
        (&song.title, &song.artist, &song.album)
    }
}

fn times(d: &mut Draw, pos: u64, dur: u64, top: i32) {
    let th = d.theme;
    d.fonts.draw(d.c, &duration(pos / 1000), BAR_X, top + TIME_Y, 13, Weight::Bold, th.muted);
    let rem = format!("-{}", duration(dur.saturating_sub(pos) / 1000));
    d.fonts.draw_right(d.c, &rem, WIDTH - BAR_X, top + TIME_Y, 13, Weight::Bold, th.muted);
}

/// `text` in at most two lines of `width`, the second cut with "…".
fn wrap2(d: &mut Draw, text: &str, size: u16, width: i32) -> Vec<String> {
    if d.fonts.measure(text, size, Weight::Bold) <= width {
        return vec![text.to_string()];
    }
    let words: Vec<&str> = text.split(' ').collect();
    let mut first = String::new();
    let mut i = 0;
    while i < words.len() {
        let next = if first.is_empty() { words[i].to_string() } else { format!("{first} {}", words[i]) };
        if d.fonts.measure(&next, size, Weight::Bold) > width {
            break;
        }
        first = next;
        i += 1;
    }
    if first.is_empty() {
        return vec![d.fonts.ellipsize(text, size, Weight::Bold, width).into_owned()];
    }
    let rest = words[i..].join(" ");
    vec![first, d.fonts.ellipsize(&rest, size, Weight::Bold, width).into_owned()]
}

fn wrap_lines(d: &mut Draw, text: &str, size: u16, width: i32) -> Vec<String> {
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let next = if line.is_empty() { word.into() } else { format!("{line} {word}") };
        if d.fonts.measure(&next, size, Weight::Bold) <= width {
            line = next;
            continue;
        }
        if !line.is_empty() { lines.push(std::mem::take(&mut line)); }
        // Split long unbroken decoder/device names as well as ordinary prose.
        for ch in word.chars() {
            let next = format!("{line}{ch}");
            if !line.is_empty() && d.fonts.measure(&next, size, Weight::Bold) > width {
                lines.push(std::mem::take(&mut line));
            }
            line.push(ch);
        }
    }
    if !line.is_empty() { lines.push(line); }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn scrub_is_precise_after_slowing_reversing_lifting_or_pausing() {
        let start = Instant::now();
        let mut wheel = ScrubWheel::default();
        let duration = 3_600_000;
        for i in 0..8 {
            assert_eq!(wheel.delta(1, start + Duration::from_millis(i * 300), duration), 1000);
        }
        wheel.reset();
        for i in 0..20 { wheel.delta(1, start + Duration::from_millis(i * 50), duration); }
        assert!(wheel.delta(1, start + Duration::from_millis(1000), duration) > 1000);
        assert_eq!(wheel.delta(1, start + Duration::from_millis(1200), duration), 1000, "slowdown is immediate");
        assert_eq!(wheel.delta(-1, start + Duration::from_millis(1250), duration), -1000);
        assert_eq!(wheel.delta(-1, start + Duration::from_millis(1500), duration), -1000);
        wheel.reset();
        assert_eq!(wheel.delta(1, start + Duration::from_millis(1550), duration), 1000);
        assert_eq!(wheel.delta(1, start + Duration::from_millis(1550), duration), 1000, "same timestamp cannot accelerate");
    }

    #[test]
    fn sustained_spin_crosses_an_hour_without_large_single_steps() {
        let mut wheel = ScrubWheel::default();
        let now = Instant::now();
        let mut distance = 0;
        // Two revolutions per second, 10 hardware detents per revolution.
        for i in 0..250 {
            let delta = wheel.delta(1, now + Duration::from_millis(i * 50), 3_600_000);
            assert!((1000..=30_000).contains(&delta));
            distance += delta;
        }
        assert!(distance >= 3_600_000, "an hour within 12.5 seconds of sustained fast movement");
        let mut short = ScrubWheel::default();
        for i in 0..50 {
            assert!(short.delta(1, now + Duration::from_millis(i * 20), 240_000) <= 2000);
        }
    }
    #[test]
    fn live_song_text_keeps_station_and_falls_back_without_metadata() {
        let mut song = subsonic::Song { title: "Station".into(), artist: "Internet Radio".into(), radio_url: Some("https://example.test/live".into()), ..Default::default() };
        assert_eq!(track_text(&song, Some("Artist - Song - Mix")), ("Song - Mix", "Artist", "Station"));
        assert_eq!(track_text(&song, Some("Unstructured title")), ("Unstructured title", "", "Station"));
        for empty in [None, Some("")] {
            assert_eq!(track_text(&song, empty), ("Station", "Internet Radio", ""));
        }
        song.radio_url = None;
        assert_eq!(track_text(&song, Some("Stale radio title")), ("Station", "Internet Radio", ""));
    }
    #[test]
    fn sleep_countdown_is_rounded_and_fade_is_authoritative() {
        let now = Instant::now();
        assert_eq!(sleep_display(None, false, now), None);
        assert_eq!(sleep_display(Some(now + Duration::from_millis(1)), false, now), Some((1, false)));
        assert_eq!(sleep_display(Some(now + Duration::from_secs(10)), true, now), Some((10, true)));
        assert_eq!(sleep_display(Some(now), true, now), Some((0, true)));
        assert_eq!(sleep_text(3599, false), "Sleep in 59:59");
        assert_eq!(sleep_text(3600, false), "Sleep in 1:00:00");
        assert_eq!(sleep_text(9, true), "Fading out 0:09");
    }

    #[test]
    fn new_playback_instance_discards_pending_seek_and_rating() {
        let mut screen = NowPlaying::new();
        screen.sync_track(1);
        screen.mode = Mode::Scrub { pos: 90000, pending: true };
        screen.seek_ack = Some(3);
        assert!(screen.sync_track(2));
        assert!(screen.mode == Mode::Volume);
        assert!(screen.seek_ack.is_none());
        screen.mode = Mode::Rate(5);
        assert!(!screen.sync_track(2));
        assert!(screen.sync_track(3));
        assert!(screen.mode == Mode::Volume);
    }
}
