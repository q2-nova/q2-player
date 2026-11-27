use crate::Song;
use crate::audio::player::{Cmd, Origin, PlayState, Player, Status, seek_position};
use crate::clock::{self, Instant};
use crate::config::{Accent, Config, Look};
use crate::gfx::{Canvas, Color, Fonts, HEIGHT, Image, Rect, WIDTH, Weight, mask_corners};
use crate::input::Event;
use crate::library::{LibEvent, Library};
use crate::ui::{Action, Ctx, Draw, Screen, player_chrome, screens, theme};
use q2_machines::queue::{self, Effect, Event as Transport, Queue, Track};
use std::{cell::RefCell, collections::HashMap, rc::Rc, sync::Arc, time::Duration};

pub struct Session {
    stack: Vec<Box<dyn Screen>>,
    status: Status,
    player: Player,
    original_queue: Option<Arc<Vec<Song>>>,
    shuffle_rng: fastrand::Rng,
    lib: Arc<Library>,
    config: Config,
    look: Look,
    accent: Accent,
    demo: crate::destinations::State,
    spoken_speed: Rc<RefCell<screens::spoken::PlaybackSpeed>>,
    speed_overrides: HashMap<String, u16>,
    fractional_ms: f64,
    external_clock: bool,
    local_minutes: Option<u32>,
    pending_end: bool,
    sleep_timer: Option<Instant>,
    sleep_fading: bool,
    scrubbing: bool,
    volume: u8,
    volume_until: Option<Instant>,
    notice: Option<(String, Instant)>,
    frame: Canvas,
    fonts: Fonts,
    placeholders: HashMap<u32, Arc<Image>>,
    replies: crate::ui::Replies,
    rgba: Vec<u8>,
    label: String,
    elapsed: u32,
    dirty: bool,
}

const NOTICE_RECT: Rect = Rect::new(32, HEIGHT - 74, WIDTH - 64, 45);

impl Session {
    pub fn new() -> Self {
        let mut session = Self {
            stack: vec![Box::new(screens::home::Home::new())],
            status: Status::default(),
            player: Player::default(),
            original_queue: None,
            shuffle_rng: fastrand::Rng::with_seed(0x83A7_CC79),
            lib: Arc::new(Library::new()),
            config: Config::default(),
            look: Look::default(),
            accent: Accent::default(),
            demo: crate::destinations::State::default(),
            spoken_speed: Rc::new(RefCell::new(screens::spoken::PlaybackSpeed::current())),
            speed_overrides: HashMap::new(),
            fractional_ms: 0.0,
            external_clock: false,
            local_minutes: None,
            pending_end: false,
            sleep_timer: None,
            sleep_fading: false,
            scrubbing: false,
            volume: 44,
            volume_until: None,
            notice: None,
            frame: Canvas::new(WIDTH, HEIGHT),
            fonts: Fonts::bundled(),
            placeholders: HashMap::new(),
            replies: Rc::default(),
            rgba: vec![0; (WIDTH * HEIGHT * 4) as usize],
            label: String::new(),
            elapsed: 0,
            dirty: true,
        };
        session.with_top(|s, ctx| s.enter(ctx));
        session.update_label();
        session
    }

    fn with_top<R>(&mut self, run: impl FnOnce(&mut dyn Screen, &mut Ctx) -> R) -> R {
        let mut requests = Vec::new();
        let replies = self.replies.clone();
        self.config.display.rating_controls = self.demo.rating_controls();
        let mut ctx = Ctx {
            status: &self.status,
            lib: &self.lib,
            config: &self.config,
            player: &self.player,
            now: Instant::now(),
            screen_on: true,
            look: &mut self.look,
            accent: &mut self.accent,
            volume: &mut self.volume,
            sleep_timer: &mut self.sleep_timer,
            sleep_fading: &mut self.sleep_fading,
            spoken_speed: self.spoken_speed.clone(),
            requests: &mut requests,
            demo: &mut self.demo,
            replies: replies.clone(),
        };
        let screen = self
            .stack
            .last_mut()
            .expect("home screen stays on the stack");
        let result = run(screen.as_mut(), &mut ctx);
        loop {
            let waiting = replies.take();
            if waiting.is_empty() {
                break;
            }
            for (token, data) in waiting {
                screen.loaded(token, data, &mut ctx);
            }
        }
        if !requests.is_empty() {
            self.transport(Transport::Pause);
            while self.stack.len() > 1 {
                self.stack.pop();
            }
            self.show_notice("Power controls are simulated".into());
        }
        result
    }

    pub fn input(&mut self, code: i32) {
        self.dirty = true;
        match code {
            4 => self.transport(Transport::PlayPause),
            5 => self.skip(false),
            6 => self.skip(true),
            9 if self.stack.last().is_some_and(|s| s.is_now_playing()) => self.apply(Action::Pop),
            9 if self.status.current().is_some() => self.apply(Action::NowPlaying),
            _ => {
                let event = match code {
                    0 => Event::Scroll(-1),
                    1 => Event::Scroll(1),
                    2 => Event::Center,
                    3 => Event::Back,
                    7 => Event::CenterLong,
                    8 => Event::WheelEnd,
                    _ => return,
                };
                if let Event::Scroll(ticks) = event {
                    if self.wheel_is_volume() {
                        // The browser's Up button sends the negative list direction.
                        self.volume = (i32::from(self.volume) - 2 * ticks)
                            .clamp(0, i32::from(self.demo.volume_limit()))
                            as u8;
                        self.volume_until = Some(Instant::now() + Duration::from_millis(1250));
                        self.update_label();
                        return;
                    }
                }
                let action = self.with_top(|s, ctx| s.event(event, ctx));
                self.apply(match (action, event) {
                    (Action::None, Event::Back) => Action::Pop,
                    (action, _) => action,
                });
            }
        }
        self.flush_commands();
        self.update_label();
    }

    pub fn clickable(&self, x: i32, y: i32) -> bool {
        if !(theme::STATUS_H..HEIGHT).contains(&y) {
            return false;
        }
        let inset = crate::gfx::corner_inset(y);
        if !(inset..WIDTH - inset).contains(&x) {
            return false;
        }
        if self.notice.is_some()
            && (NOTICE_RECT.x..NOTICE_RECT.right()).contains(&x)
            && (NOTICE_RECT.y..NOTICE_RECT.bottom()).contains(&y)
        {
            return false;
        }
        self.stack.last().is_some_and(|screen| screen.clickable(x, y))
    }

    pub fn click(&mut self, x: i32, y: i32) -> bool {
        if !self.clickable(x, y) {
            return false;
        }
        let action = self.with_top(|screen, ctx| screen.click(x, y, ctx));
        self.apply(action);
        self.flush_commands();
        self.dirty = true;
        self.update_label();
        true
    }

    fn apply(&mut self, action: Action) {
        match action {
            Action::None | Action::Stay => {}
            Action::TogglePlayback => self.transport(Transport::PlayPause),
            Action::Restart => {
                self.transport(Transport::Pause);
                // The native app saves the queue and loads it paused after
                // restart. Keep the same item/position, but release old media.
                self.status.generation = self.status.generation.wrapping_add(1);
                self.scrubbing = false;
                self.fractional_ms = 0.0;
                self.pending_end = false;
                self.sleep_timer = None;
                self.sleep_fading = false;
                self.volume_until = None;
                self.notice = None;
                self.stack = vec![Box::new(screens::home::Home::new())];
                self.with_top(|s, ctx| s.enter(ctx));
            }
            Action::Pop => {
                if self.stack.len() > 1 {
                    self.stack.pop();
                    self.with_top(|s, ctx| s.enter(ctx));
                }
            }
            Action::Push(screen) => {
                self.stack.push(screen);
                self.with_top(|s, ctx| s.enter(ctx));
            }
            Action::Replace(screen) => {
                if self.stack.len() > 1 {
                    self.stack.pop();
                }
                self.stack.push(screen);
                self.with_top(|s, ctx| s.enter(ctx));
            }
            Action::Many(actions) => actions.into_iter().for_each(|a| self.apply(a)),
            Action::Toast(text) => self.show_notice(text),
            Action::NowPlaying => {
                if !self.stack.last().is_some_and(|s| s.is_now_playing()) {
                    self.apply(Action::Push(Box::new(
                        screens::now_playing::NowPlaying::new(),
                    )));
                }
            }
            Action::Play {
                songs,
                index,
                shuffle,
            } => {
                if songs.is_empty() {
                    return;
                }
                let index = if shuffle && !self.demo.online() {
                    let Some(index) = songs.iter().position(|song| self.is_local(song)) else {
                        self.show_notice("Unavailable offline".into());
                        return;
                    };
                    index
                } else {
                    index.min(songs.len() - 1)
                };
                if !self.demo.online() && !self.is_local(&songs[index]) {
                    self.show_notice("Unavailable offline".into());
                    return;
                }
                self.save_position();
                self.original_queue = None;
                self.status.queue = songs;
                self.status.index = index;
                self.status.shuffle = false;
                self.shuffle(shuffle, false);
                self.transport(Transport::Load {
                    len: self.status.queue.len(),
                    index: self.status.index,
                    autoplay: true,
                });
                self.apply(Action::NowPlaying);
            }
        }
    }

    fn show_notice(&mut self, text: String) {
        self.notice = Some((text, Instant::now() + Duration::from_secs(2)));
    }

    fn is_local(&self, song: &Song) -> bool {
        song.album_id
            .as_deref()
            .is_some_and(|id| self.lib.album_downloaded(id))
            || matches!(
                song.spoken.as_ref().map(|spoken| &spoken.source),
                Some(crate::SpokenSource::Local { .. })
            )
            || self.demo.spoken_downloaded(song)
    }

    fn shuffle(&mut self, enabled: bool, keep_current: bool) {
        if self.status.shuffle == enabled || self.status.queue.len() < 2 {
            return;
        }
        if enabled {
            self.original_queue = Some(self.status.queue.clone());
            let mut queue = self.status.queue.as_ref().clone();
            let current = queue.remove(self.status.index);
            self.shuffle_rng.shuffle(&mut queue);
            queue.insert(0, current);
            if !keep_current {
                self.shuffle_rng.shuffle(&mut queue);
                if !self.demo.online() {
                    if let Some(first_playable) = queue.iter().position(|song| self.is_local(song))
                    {
                        queue.swap(0, first_playable);
                    }
                }
            }
            self.status.queue = Arc::new(queue);
            self.status.index = 0;
        } else if let Some(original) = self.original_queue.take() {
            let index = self
                .status
                .current()
                .and_then(|song| original.iter().position(|entry| entry.id == song.id));
            self.status.queue = original;
            self.status.index = index.unwrap_or_default();
        }
        self.status.shuffle = enabled;
    }

    fn select(&mut self, index: usize) {
        self.status.select(index);
        self.status.speed_override = self
            .status
            .current()
            .and_then(Self::speed_key)
            .and_then(|key| self.speed_overrides.get(key).copied());
        self.scrubbing = false;
        self.fractional_ms = 0.0;
        self.pending_end = false;
        self.status.error = None;
        if let Some(song) = self.status.current() {
            let local = self.is_local(song);
            if local {
                self.status.origin = Some(Origin::Offline);
            }
        }
    }

    fn speed_key(song: &Song) -> Option<&str> {
        let spoken = song.spoken.as_ref()?;
        Some(crate::destinations::podcast_series(song).unwrap_or(&spoken.id))
    }

    fn save_position(&mut self) {
        if let Some(song) = self.status.current().filter(|song| song.spoken.is_some()) {
            self.demo.save_position(song, self.status.position_ms);
        }
    }

    fn queue(&self) -> Queue {
        Queue {
            len: self.status.queue.len(),
            index: self.status.index,
            play: self.status.state,
            failures: 0,
        }
    }

    fn transport(&mut self, event: Transport) {
        let step = queue::transition(self.queue(), event);
        if matches!(
            step.effect,
            Effect::Pause | Effect::Start | Effect::Advance | Effect::FinishBook
        ) && !matches!(event, Transport::Load { .. })
        {
            self.save_position();
        }
        self.status.state = step.queue.play;
        if step.effect == Effect::Restart {
            let radio = self
                .status
                .current()
                .is_some_and(|song| song.radio_url.is_some());
            if radio {
                return;
            }
            self.status.state = queue::transition(self.queue(), Transport::Seek { radio })
                .queue
                .play;
        }
        let new_selection = matches!(
            step.effect,
            Effect::Start | Effect::Restart | Effect::Advance
        );
        if new_selection {
            self.select(step.queue.index);
            if step.effect == Effect::Start {
                if let Some(song) = self.status.current().filter(|song| song.spoken.is_some()) {
                    self.status.position_ms =
                        seek_position(self.demo.resume_position(song), self.status.duration_ms);
                }
            }
        }
        let unavailable = self
            .status
            .current()
            .is_some_and(|song| !self.demo.online() && !self.is_local(song));
        if unavailable && (new_selection || self.status.state == PlayState::Buffering) {
            self.status.state = PlayState::Paused;
            self.status.error = Some("Unavailable offline".into());
        } else {
            self.status.error = None;
            if self.status.state == PlayState::Buffering {
                self.status.state = queue::transition(self.queue(), Transport::Accepted)
                    .queue
                    .play;
            }
            if new_selection && self.status.state == PlayState::Playing {
                if let Some(song) = self.status.current() {
                    self.lib.record_play(song);
                }
            }
        }
        self.dirty = true;
    }

    fn skip(&mut self, forward: bool) {
        if let Some(spoken) = self.status.current().and_then(|song| song.spoken.as_ref()) {
            let current = self.status.position_ms;
            let podcast = matches!(
                &spoken.source,
                crate::SpokenSource::Audiobookshelf {
                    episode: Some(_),
                    ..
                }
            );
            let target = if self.demo.skip_by_chapter() && !podcast {
                if spoken.chapters.is_empty() {
                    self.show_notice("This book has no chapters".into());
                    return;
                }
                if forward {
                    spoken
                        .chapters
                        .iter()
                        .find(|chapter| chapter.start_ms > current)
                        .map_or(spoken.duration_ms, |chapter| chapter.start_ms)
                } else {
                    spoken
                        .chapters
                        .partition_point(|chapter| chapter.start_ms <= current)
                        .checked_sub(2)
                        .and_then(|i| spoken.chapters.get(i))
                        .map_or(0, |chapter| chapter.start_ms)
                }
            } else {
                let delta =
                    self.demo.skip_seconds(forward) as i64 * if forward { 1000 } else { -1000 };
                current.saturating_add_signed(delta)
            };
            self.seek(target.min(self.status.duration_ms.saturating_sub(1)));
        } else if forward {
            self.transport(Transport::Next {
                repeat: self.status.repeat,
            });
        } else {
            self.transport(Transport::Prev {
                restart: self.status.position_ms >= 3000,
            });
        }
    }

    fn seek(&mut self, position: u64) {
        self.status.position_ms = position;
        self.status.seek_revision = self.status.seek_revision.wrapping_add(1);
        self.fractional_ms = 0.0;
        self.pending_end = false;
        self.save_position();
        if self.status.state == PlayState::Stopped {
            self.transport(Transport::Seek { radio: false });
        }
    }

    fn flush_commands(&mut self) {
        for cmd in self.player.take() {
            match cmd {
                Cmd::Scrub {
                    generation,
                    position,
                    active,
                } if generation == self.status.generation => {
                    self.scrubbing = active;
                    if let Some(position) = position {
                        self.seek(seek_position(position, self.status.duration_ms));
                    } else if !active && self.pending_end {
                        self.finish_media();
                    }
                }
                Cmd::Scrub { .. } => {}
                Cmd::SetSpeed {
                    generation,
                    percent,
                } => {
                    if generation == self.status.generation {
                        if let Some(key) = self.status.current().and_then(Self::speed_key) {
                            if let Some(percent) = percent {
                                self.speed_overrides.insert(key.to_owned(), percent);
                            } else {
                                self.speed_overrides.remove(key);
                            }
                            self.status.speed_override = percent;
                        }
                    }
                }
                Cmd::SetRepeat(repeat) => self.status.repeat = repeat,
                Cmd::SetShuffle(shuffle) => self.shuffle(shuffle, true),
                Cmd::Enqueue { songs, next } => {
                    let step = queue::transition(
                        self.queue(),
                        Transport::Insert {
                            count: songs.len(),
                            next,
                        },
                    );
                    if let Effect::Insert { at, arm } = step.effect {
                        Arc::make_mut(&mut self.status.queue).splice(at..at, songs.iter().cloned());
                        self.status.state = step.queue.play;
                        if let Some(original) = self.original_queue.as_mut() {
                            Arc::make_mut(original).extend(songs.iter().cloned());
                        }
                        if arm {
                            self.select(0);
                        }
                    }
                }
            }
            self.dirty = true;
        }
    }

    fn finish_media(&mut self) {
        let Some(song) = self.status.current() else {
            return;
        };
        let track = if song.radio_url.is_some() {
            Track::Radio
        } else if let Some(spoken) = &song.spoken {
            let continues = self
                .status
                .queue
                .get(self.status.index + 1)
                .and_then(|song| song.spoken.as_ref())
                .is_some_and(|next| spoken.id == next.id);
            Track::Book { continues }
        } else {
            Track::Music
        };
        self.pending_end = self.scrubbing && track != Track::Radio;
        if track != Track::Radio {
            self.status.position_ms = self.status.duration_ms;
        }
        self.transport(Transport::Ended {
            repeat: self.status.repeat,
            track,
            scrubbing: self.scrubbing,
        });
    }

    pub fn set_external_clock(&mut self, enabled: bool) {
        self.external_clock = enabled;
        if !enabled {
            self.pending_end = false;
        }
    }

    pub fn media_progress(
        &mut self,
        generation: u32,
        seek_revision: u32,
        position_ms: u32,
        duration_ms: u32,
    ) {
        if !self.external_clock
            || generation != self.status.generation as u32
            || seek_revision != self.status.seek_revision as u32
            || self.status.state == PlayState::Stopped
        {
            return;
        }
        let duration = if duration_ms != 0
            && self
                .status
                .current()
                .is_some_and(|song| song.radio_url.is_none())
        {
            u64::from(duration_ms)
        } else {
            self.status.duration_ms
        };
        let position = if duration == 0 {
            u64::from(position_ms)
        } else {
            u64::from(position_ms).min(duration)
        };
        self.dirty |= duration != self.status.duration_ms
            || position / 1000 != self.status.position_ms / 1000;
        self.status.duration_ms = duration;
        self.status.position_ms = position;
    }

    pub fn media_ended(&mut self, generation: u32) {
        if self.external_clock
            && generation == self.status.generation as u32
            && self.status.state == PlayState::Playing
        {
            self.finish_media();
            self.update_label();
        }
    }

    pub fn timezone(&self) -> &'static str {
        self.demo.timezone
    }

    pub fn set_local_minutes(&mut self, minutes: u32) {
        let minutes = (minutes < 24 * 60).then_some(minutes);
        if self.local_minutes != minutes {
            self.local_minutes = minutes;
            self.dirty |= self.demo.clock_in_title;
        }
    }

    fn clock_title(&self) -> String {
        let Some(minutes) = self.local_minutes else { return "—:—".into() };
        let (hour, minute) = (minutes / 60, minutes % 60);
        if crate::destinations::clock_24h(&self.demo) {
            format!("{hour:02}:{minute:02}")
        } else {
            let clock_hour = (hour + 11) % 12 + 1;
            let meridiem = if hour < 12 { "AM" } else { "PM" };
            format!("{clock_hour}:{minute:02} {meridiem}")
        }
    }

    pub fn frame(&mut self, elapsed: u32) -> *const u8 {
        let elapsed = elapsed.max(self.elapsed);
        let delta = elapsed - self.elapsed;
        self.elapsed = elapsed;
        clock::set_elapsed(elapsed);
        let now = Instant::now();
        if !self.external_clock && self.status.state == PlayState::Playing && delta != 0 {
            let old = self.status.position_ms;
            let rate = f64::from(self.speed_percent()) / 100.0;
            let advanced = f64::from(delta) * rate + self.fractional_ms;
            self.status.position_ms = old.saturating_add(advanced as u64);
            self.fractional_ms = advanced.fract();
            if self.status.duration_ms != 0 && self.status.position_ms >= self.status.duration_ms {
                self.finish_media();
            }
            self.dirty |= old / 1000 != self.status.position_ms / 1000;
        }
        if self.sleep_timer.is_some_and(|deadline| now >= deadline) {
            self.sleep_timer = None;
            self.sleep_fading = false;
            self.transport(Transport::Pause);
        }
        if self.volume_until.is_some_and(|until| now >= until) {
            self.volume_until = None;
            self.dirty = true;
        }
        if self.notice.as_ref().is_some_and(|(_, until)| now >= *until) {
            self.notice = None;
            self.dirty = true;
        }
        let changed = self.with_top(|s, ctx| {
            let refreshed = crate::destinations::refresh(ctx);
            if refreshed {
                s.refresh(&LibEvent::Downloads, ctx);
            }
            let ticked = s.tick(ctx);
            let animated = s.animate(ctx.now);
            refreshed
                || ticked
                || animated
                || s.next_frame(ctx.now)
                    .is_some_and(|interval| interval.is_zero())
        });
        self.dirty |= changed;
        self.flush_commands();
        if self.dirty {
            self.paint();
            self.update_label();
            self.dirty = false;
        }
        self.rgba.as_ptr()
    }

    fn paint(&mut self) {
        use theme::{STATUS_H, Theme};
        let theme = Theme::new(self.look, self.accent);
        let area = Rect::new(0, STATUS_H, WIDTH, HEIGHT - STATUS_H);
        let bounds = Rect::new(0, 0, WIDTH, HEIGHT);
        self.frame.set_clip(bounds);
        self.frame.clear(theme.bg);
        let base = self
            .stack
            .iter()
            .rposition(|s| !s.is_overlay())
            .unwrap_or_default();
        let title = if self.demo.clock_in_title {
            self.clock_title()
        } else {
            self.stack[base].title()
        };
        let volume = self.volume_until.map(|_| self.volume);
        let sleep_timer = self
            .sleep_timer
            .map(|deadline| (deadline, self.sleep_fading));
        let mut draw = Draw {
            c: &mut self.frame,
            fonts: &mut self.fonts,
            theme,
            lib: &self.lib,
            status: &self.status,
            volume,
            sleep_timer,
            now: Instant::now(),
            placeholders: &mut self.placeholders,
        };
        for screen in &mut self.stack[base..] {
            draw.c.set_clip(area);
            screen.draw(&mut draw, area);
        }
        self.frame.set_clip(bounds);
        player_chrome::Header {
            title: &title,
            play: self.status.state,
            level: Some(82),
            charging: false,
            badge: self.demo.connection_badge(),
            bars: 3,
        }
        .draw(&mut self.frame, &mut self.fonts, theme);
        if let Some((notice, _)) = &self.notice {
            let r = NOTICE_RECT;
            self.frame.fill_round_rect_alpha(r, 8, Color::BLACK, 220);
            self.fonts.draw_centered(
                &mut self.frame,
                notice,
                WIDTH / 2,
                r.y + 11,
                16,
                Weight::Bold,
                Color::WHITE,
            );
        }
        mask_corners(&mut self.frame.pixels);
        // The browser cannot raise physical backlight intensity. The native
        // palette is the reference at the normal setting and above.
        let brightness = u32::from(self.demo.brightness.clamp(5, 50));
        let dim = |channel: u32| ((u32::from(channel as u8) * brightness + 25) / 50) as u8;
        for (rgba, &pixel) in self.rgba.chunks_exact_mut(4).zip(&self.frame.pixels) {
            rgba.copy_from_slice(&[dim(pixel >> 16), dim(pixel >> 8), dim(pixel), 255]);
        }
    }

    fn update_label(&mut self) {
        let screen = self.stack.last().expect("home screen stays on the stack");
        self.label = screen.title();
        if let Some(details) = screen.accessible_details().or_else(|| screen.selected_label()) {
            self.label.push_str(" · ");
            self.label.push_str(details);
        }
        if self.demo.clock_in_title {
            self.label.push_str(" · ");
            self.label.push_str(&self.clock_title());
        }
        if let Some(song) = self.status.current() {
            let state = match self.status.state {
                PlayState::Playing => "Playing",
                PlayState::Paused => "Paused",
                PlayState::Stopped => "Stopped",
                PlayState::Buffering => "Buffering",
            };
            self.label
                .push_str(&format!(" · {state} {} by {}", song.title, song.artist));
        } else if screen.is_now_playing() {
            self.label.push_str(" · Nothing playing");
        }
        if self.volume_until.is_some() && screen.is_now_playing() {
            self.label.push_str(&format!(" · Volume {}%", self.volume));
        }
        if let Some(error) = &self.status.error {
            self.label.push_str(&format!(" · {error}"));
        }
        if let Some((notice, _)) = &self.notice {
            self.label.push_str(&format!(" · {notice}"));
        }
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    pub fn status(&self) -> &Status {
        &self.status
    }

    pub fn volume(&self) -> u8 {
        self.volume
    }

    pub fn wheel_is_volume(&self) -> bool {
        self.stack.last().is_some_and(|s| s.wheel_is_volume())
    }

    pub fn speed_percent(&self) -> u16 {
        if self
            .status
            .current()
            .is_some_and(|song| song.spoken.is_some())
        {
            self.status
                .speed_override
                .unwrap_or(self.demo.speed_percent)
        } else {
            100
        }
    }

    pub fn online(&self) -> bool {
        self.demo.online()
    }
}
