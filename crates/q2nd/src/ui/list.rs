//! The iPod list: rows under a selection bar, moved by the wheel.

use super::theme::*;
use super::{Draw, icons};
use crate::gfx::{Color, Image, Rect, WIDTH, Weight, corner_inset};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use crate::clock::Instant;
use subsonic::Song;

/// What choosing a row does.
#[derive(Clone)]
pub enum RowAction {
    None,
    /// Open another page.
    Open(super::Dest),
    /// Play `songs` from `index`.
    Play { songs: Arc<Vec<Song>>, index: usize },
    /// Play `songs` shuffled.
    Shuffle(Arc<Vec<Song>>),
    /// A screen-specific command.
    Cmd(u32),
}

#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum Badge {
    #[default]
    None,
    /// Downloaded for offline listening.
    Offline,
    /// Waiting to download (DOWNLOAD-002).
    Queued,
    /// Downloading now: percent done, or 255 when the size is unknown.
    Downloading(u8),
    /// The song playing now.
    Playing,
    /// The current choice.
    Check,
}

#[derive(Clone)]
pub struct Row {
    /// Identifies the row across reloads, to keep the selection.
    pub key: String,
    pub title: String,
    pub detail: Option<String>,
    pub art: Option<String>,
    pub artist_image: Option<Box<subsonic::Artist>>,
    pub value: Option<String>,
    pub chevron: bool,
    /// Can't be played now (offline and not on the card).
    pub dim: bool,
    /// A visual boundary between groups of settings.
    pub separator: bool,
    pub badge: Badge,
    pub spoken_progress: u8,
    pub action: RowAction,
    /// The song or item behind the row, for its context menu: owned, or
    /// (`shared_song`) the one its `Play` action points at, so a list of
    /// songs holds each song once (`song_item`).
    pub song: Option<Box<Song>>,
    pub shared_song: bool,
}

impl Row {
    pub fn new(title: impl Into<String>, action: RowAction) -> Row {
        let title = title.into();
        Row {
            key: title.clone(),
            title,
            detail: None,
            art: None,
            artist_image: None,
            value: None,
            chevron: false,
            dim: false,
            separator: false,
            badge: Badge::None,
            spoken_progress: 0,
            action,
            song: None,
            shared_song: false,
        }
    }

    /// A row that opens a page.
    pub fn link(title: impl Into<String>, dest: super::Dest) -> Row {
        Row { chevron: true, ..Row::new(title, RowAction::Open(dest)) }
    }

    /// A command that opens another screen (rather than changing a value).
    pub fn screen(label: impl Into<String>, cmd: u32) -> Row {
        Row { chevron: true, ..Row::new(label, RowAction::Cmd(cmd)) }
    }

    pub fn separated(mut self) -> Row {
        self.separator = true;
        self
    }

    pub fn key(mut self, key: impl Into<String>) -> Row {
        self.key = key.into();
        self
    }
    pub fn detail(mut self, d: impl Into<String>) -> Row {
        let d = d.into();
        self.detail = if d.is_empty() { None } else { Some(d) };
        self
    }
    pub fn art(mut self, a: Option<String>) -> Row {
        self.art = a;
        self
    }
    pub fn artist_art(mut self, artist: &subsonic::Artist) -> Row {
        self.artist_image = Some(Box::new(artist.clone()));
        self
    }
    pub fn value(mut self, v: impl Into<String>) -> Row {
        self.value = Some(v.into());
        self
    }
    pub fn badge(mut self, b: Badge) -> Row {
        self.badge = b;
        self
    }
    pub fn dim(mut self, d: bool) -> Row {
        self.dim = d;
        self
    }
    pub fn song(mut self, s: Song) -> Row {
        self.song = Some(Box::new(s));
        self.shared_song = false;
        self
    }
    /// The row's song is its `Play` action's `songs[index]`.
    pub fn shared_song(mut self) -> Row {
        self.song = None;
        self.shared_song = matches!(self.action, RowAction::Play { .. });
        self
    }
    /// The song or item behind the row.
    pub fn song_item(&self) -> Option<&Song> {
        match (&self.action, self.shared_song) {
            (RowAction::Play { songs, index }, true) => songs.get(*index),
            _ => self.song.as_deref(),
        }
    }
}

fn artwork_download_progress(badge:Badge)->Option<Option<f32>> {
    match badge {
        Badge::Downloading(percent)=>Some((percent!=255).then_some(percent as f32/100.0)),
        Badge::Queued=>Some(None),
        _=>None,
    }
}
fn visible_badge(badge:Badge,playing:bool)->Badge {if playing {Badge::Playing} else {badge}}

/// A list longer than this accelerates on a long spin.
const SHORT_LIST: usize = 16;
/// Velocity resets promptly after a pause or reversal (see docs/interaction.md).
const RUN_GAP: Duration = Duration::from_millis(250);
pub(crate) const LETTER_MIN_ROWS: u32 = 100;
// A deliberate sweep spans at least 1.5 turns at the default wheel setting.
const LETTER_ARM_SPEED: f32 = 720.0;
const LETTER_INTERVAL_FLOOR: f32 = 540.0;
const LETTER_ARM_TIME: Duration = Duration::from_millis(500);
const LETTER_ARM_TICKS: u64 = 15;
const LETTER_STEP_TICKS: i32 = 3;

#[derive(Default)]
struct LetterRun {
    intervals: VecDeque<(Duration, u64)>,
    duration: Duration,
    ticks: u64,
}

impl LetterRun {
    fn record(&mut self, interval: Duration, ticks: u32) -> bool {
        let ticks = u64::from(ticks);
        self.intervals.push_back((interval, ticks));
        self.duration += interval;
        self.ticks += ticks;
        // Keep the most recent intervals covering both minimums so a slow
        // lead-in cannot delay a later deliberate sweep.
        while let Some(&(old_interval, old_ticks)) = self.intervals.front() {
            if self.duration - old_interval < LETTER_ARM_TIME || self.ticks - old_ticks < LETTER_ARM_TICKS {
                break;
            }
            self.intervals.pop_front();
            self.duration -= old_interval;
            self.ticks -= old_ticks;
        }
        self.duration >= LETTER_ARM_TIME && self.ticks >= LETTER_ARM_TICKS
            && 36.0 * self.ticks as f32 / self.duration.as_secs_f32() >= LETTER_ARM_SPEED
    }
}
/// Reading pages move by a short text line per detent, with no row focus.
/// This is Q2 tuning, not a measured iPod scroll distance.
const CONTENT_SCROLL_STEP: i32 = ROW_H / 2;

struct RenderedRow {
    index: usize,
    bounds: Rect,
    separator: Option<Rect>,
}

fn contains(rect: &Rect, x: i32, y: i32) -> bool {
    x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom()
}

pub struct ListView {
    pub rows: Vec<Row>,
    pub selected: usize,
    /// Rows with art (72 px) rather than text rows (44 px).
    pub media: bool,
    passive_info: bool,
    content_only: bool,
    row_h: i32,
    scroll: f32,
    run: Option<(i32, Instant)>,
    last_tick: Option<Instant>,
    speed: f32,
    pub alphabetical: bool,
    pub(crate) global_alphabet: bool,
    global_letters: bool,
    alphabet_jump: i32,
    groups: Vec<(String, usize)>,
    letter_mode: bool,
    letter_run: Option<LetterRun>,
    bump: f32,
    letter: Option<String>,
    letter_remainder: i32,
    view_h: i32,
    rendered_rows: Vec<RenderedRow>,
    #[cfg(not(target_arch = "wasm32"))]
    retry_target: Option<Rect>,
}

impl ListView {
    pub fn new(media: bool) -> ListView {
        ListView {
            rows: Vec::new(),
            selected: 0,
            media,
            passive_info: false,
            content_only: false,
            row_h: if media { MEDIA_ROW_H } else { ROW_H },
            scroll: 0.0,
            run: None,
            last_tick: None,
            speed: 0.0,
            alphabetical: false,
            global_alphabet: false,
            global_letters: false,
            alphabet_jump: 0,
            groups: Vec::new(),
            letter_mode: false,
            letter_run: None,
            bump: 0.0,
            letter: None,
            letter_remainder: 0,
            view_h: crate::gfx::HEIGHT - STATUS_H,
            rendered_rows: Vec::new(),
            #[cfg(not(target_arch = "wasm32"))]
            retry_target: None,
        }
    }

    /// Text rows tall enough for a second line.
    pub fn tall() -> ListView {
        ListView { row_h: TALL_ROW_H, ..ListView::new(false) }
    }

    /// Settings/status lists distinguish information from selectable controls.
    pub fn settings(tall: bool) -> ListView {
        let mut list = if tall { Self::tall() } else { Self::new(false) };
        list.passive_info = true;
        list
    }

    pub fn row_h(&self) -> i32 {
        self.row_h
    }

    /// Rows with no action are information: drawn plain, skipped by the
    /// wheel, never selected (as on settings pages).
    pub(crate) fn set_passive_info(&mut self, on: bool) {
        self.passive_info = on;
    }

    /// Replaces the rows, keeping actionable selection or a reading page's
    /// content offset, bounded by the new content length.
    pub fn set_rows(&mut self, rows: Vec<Row>) {
        #[cfg(target_arch = "wasm32")]
        self.rendered_rows.clear();
        #[cfg(not(target_arch = "wasm32"))]
        self.clear_hit_targets();
        let key = self.rows.get(self.selected).map(|r| r.key.clone());
        self.rows = rows;
        self.content_only = self.passive_info && self.rows.iter().all(|r| matches!(r.action, RowAction::None));
        self.groups.clear();
        if self.alphabetical && !self.global_alphabet && self.rows.len() >= LETTER_MIN_ROWS as usize {
            for (i, row) in self.rows.iter().enumerate() {
                let letter = index_letter(&row.title);
                if self.groups.last().is_none_or(|(l, _)| *l != letter) {
                    self.groups.push((letter, i));
                }
            }
        }
        self.end_spin();
        if let Some(k) = key {
            if let Some(i) = self.rows.iter().position(|r| r.key == k) {
                self.selected = i;
            }
        }
        self.selected = self.selected.min(self.rows.len().saturating_sub(1));
        if self.content_only { self.selected = 0; }
        self.clamp_scroll(true);
    }

    /// Append/prepend or evict distant pages without moving the selected
    /// content within the viewport. Ordinary replacements keep set_rows().
    pub(crate) fn set_rows_anchored(&mut self, rows: Vec<Row>) {
        let anchor=self.selected_row().map(|row|row.key.clone());
        let old_index=self.selected;
        let scroll=self.scroll;
        let wheel=(self.run,self.last_tick,self.speed,self.letter_mode,self.letter_run.take(),self.letter.take(),self.letter_remainder);
        self.set_rows(rows);
        if anchor.as_deref()==self.selected_row().map(|row|row.key.as_str()) && anchor.is_some() {
            self.scroll=(scroll+(self.selected as f32-old_index as f32)*self.row_h() as f32)
                .clamp(0.0,self.max_scroll());
        }
        if !self.alphabetical || self.global_alphabet { (self.run,self.last_tick,self.speed,self.letter_mode,self.letter_run,self.letter,self.letter_remainder)=wheel; }
    }

    pub fn selected_row(&self) -> Option<&Row> {
        if self.content_only { None } else { self.rows.get(self.selected) }
    }

    /// Actionable rows used by positional wheel navigation. Information-only
    /// reading pages return no controls.
    pub(crate) fn control_indices(&self) -> Vec<usize> {
        if self.content_only { return Vec::new(); }
        self.rows.iter().enumerate()
            .filter(|(_, row)| !self.passive_info || !matches!(row.action, RowAction::None))
            .map(|(i, _)| i).collect()
    }

    pub fn select(&mut self, i: usize) {
        if self.content_only { return; }
        self.selected = i.min(self.rows.len().saturating_sub(1));
        self.clamp_scroll(true);
    }

    pub fn hit(&self, x: i32, y: i32) -> Option<usize> {
        let row = self.rendered_rows.iter().rev().find(|row| contains(&row.bounds, x, y))?;
        let inset = corner_inset(y);
        if x < inset || x >= WIDTH - inset
            || row.separator.as_ref().is_some_and(|line| contains(line, x, y))
            || matches!(self.rows.get(row.index)?.action, RowAction::None)
        {
            return None;
        }
        Some(row.index)
    }

    pub fn select_at(&mut self, x: i32, y: i32) -> bool {
        let Some(index) = self.hit(x, y) else { return false };
        self.end_spin();
        self.bump = 0.0;
        self.select(index);
        true
    }

    /// Moves selection, or the content offset on a page without controls.
    /// Returns true if anything changed.
    pub fn wheel(&mut self, ticks: i32, now: Instant) -> bool {
        if self.rows.is_empty() || ticks == 0 {
            return false;
        }
        if self.content_only {
            let old = self.scroll;
            self.scroll = (self.scroll + ticks as f32 * CONTENT_SCROLL_STEP as f32)
                .clamp(0.0, self.max_scroll());
            self.end_spin();
            return self.scroll != old;
        }
        if self.letter_mode {
            let travel = i64::from(self.letter_remainder) + i64::from(ticks);
            let steps = (travel / i64::from(LETTER_STEP_TICKS)) as i32;
            self.letter_remainder = (travel % i64::from(LETTER_STEP_TICKS)) as i32;
            if self.global_alphabet {
                self.alphabet_jump = self.alphabet_jump.saturating_add(steps);
                return steps != 0;
            }
            if steps == 0 { return false; }
            let group = self.groups.partition_point(|(_, i)| *i <= self.selected).saturating_sub(1);
            let next = (group as i64 + i64::from(steps))
                .clamp(0, self.groups.len().saturating_sub(1) as i64) as usize;
            if next == group { return false; }
            self.selected = self.groups[next].1;
            self.letter = Some(self.groups[next].0.clone());
            self.clamp_scroll(true);
            return true;
        }
        let dir = ticks.signum();
        let continuing = self.run.is_some_and(|(d, _)| d == dir)
            && self.last_tick.is_some_and(|t| now.saturating_duration_since(t) <= RUN_GAP);
        let mut measured = 0.0;
        let mut interval = Duration::ZERO;
        if continuing {
            interval = now.saturating_duration_since(self.last_tick.unwrap());
            let dt = interval.as_secs_f32();
            // One Q2 detent is 20/200 revolution = 36 degrees. Calculate
            // once per batch: identical timestamps must not invent velocity.
            if dt >= 0.001 {
                measured = (36.0 * ticks.unsigned_abs() as f32 / dt).min(2160.0);
                self.speed = if self.speed == 0.0 { measured } else { 0.5 * self.speed + 0.5 * measured };
            }
        } else {
            self.run = Some((dir, now));
            self.speed = 0.0;
            self.letter_run = None;
        }
        let held = now.saturating_duration_since(self.run.unwrap().1);
        let gain = if self.global_alphabet || self.alphabetical || self.rows.len() <= SHORT_LIST || held < Duration::from_millis(200) {
            // Alphabetical lists have a dedicated fast mode. Ordinary movement
            // must visit every row, including after choosing a letter.
            1
        } else {
            ((self.speed / 270.0).powi(2).round() as usize).clamp(1, 32)
        };
        self.last_tick = Some(now);
        if (self.global_letters || self.groups.len() > 1) && measured >= LETTER_INTERVAL_FLOOR {
            if self.letter_run.get_or_insert_with(LetterRun::default).record(interval, ticks.unsigned_abs()) {
                self.letter_run = None;
                self.letter_mode = true;
                self.letter = self.selected_row().map(|row| index_letter(&row.title));
                return true;
            }
        } else {
            self.letter_run = None;
        }

        let old = self.selected;
        let delta = ticks as i64 * gain as i64;
        let controls = if self.passive_info { self.control_indices() } else { Vec::new() };
        if controls.is_empty() {
            self.selected = (self.selected as i64 + delta).clamp(0, self.rows.len() as i64 - 1) as usize;
        } else {
            let index = controls.partition_point(|i| *i < self.selected) as i64;
            let next = (index + delta).clamp(0, controls.len() as i64 - 1) as usize;
            self.selected = controls[next];
        }
        // A wheel is positional, not a touch-screen fling: no momentum or
        // wrap at the endpoints. Keep the selected row under the finger.
        self.clamp_scroll(true);
        self.selected != old
    }

    pub(crate) fn set_global_letters(&mut self, eligible: bool) {
        let enabled = self.global_alphabet && eligible;
        if self.global_letters != enabled { self.end_spin(); }
        self.global_letters = enabled;
    }

    pub(crate) fn take_alphabet_jump(&mut self) -> i32 { std::mem::take(&mut self.alphabet_jump) }
    pub(crate) fn in_letter_mode(&self) -> bool { self.letter_mode }

    #[cfg(test)]
    pub(crate) fn letter_label(&self) -> Option<&str> { self.letter.as_deref() }

    pub(crate) fn show_alphabet_jump(&mut self, label: String) {
        if self.letter_mode { self.letter = Some(label); }
    }

    pub fn end_spin(&mut self) {
        self.run = None;
        self.last_tick = None;
        self.speed = 0.0;
        self.letter_mode = false;
        self.letter_run = None;
        self.letter = None;
        self.letter_remainder = 0;
        self.alphabet_jump = 0;
    }

    /// In alphabet search, Centre first returns to individual items.
    pub fn finish_letters(&mut self) -> bool {
        let active = self.letter_mode;
        self.end_spin();
        active
    }

    fn max_scroll(&self) -> f32 {
        (self.row_h() * self.rows.len() as i32 - self.view_h).max(0) as f32
    }

    fn target(&self) -> f32 {
        if self.content_only { return self.scroll.clamp(0.0, self.max_scroll()); }
        let h = self.row_h();
        let top = (self.selected as i32 * h) as f32;
        let bottom = top + h as f32;
        let mut t = self.scroll;
        if top < t {
            t = top;
        }
        if bottom > t + self.view_h as f32 {
            t = bottom - self.view_h as f32;
        }
        t.clamp(0.0, self.max_scroll())
    }

    fn clamp_scroll(&mut self, jump: bool) {
        if jump {
            self.scroll = self.target();
        }
    }

    /// Steps animations; true while something still moves.
    pub fn animate(&mut self, _now: Instant) -> bool {
        let t = self.target();
        let mut moving = false;
        if (t - self.scroll).abs() > 0.5 {
            self.scroll += (t - self.scroll) * 0.45;
            moving = true;
        } else {
            self.scroll = t;
        }
        if self.bump.abs() > 0.3 {
            self.bump *= 0.6;
            moving = true;
        } else {
            self.bump = 0.0;
        }
        moving
    }

    /// The `count` rows after the last one the view shows (at the scroll
    /// target, so a moving list prefetches where it stops).
    pub(crate) fn rows_below(&self, count: usize) -> std::ops::Range<usize> {
        let h = self.row_h().max(1) as u32;
        let end = (self.target().max(0.0) as u32 + self.view_h.max(0) as u32).div_ceil(h) as usize;
        let end = end.min(self.rows.len());
        end..(end + count).min(self.rows.len())
    }

    /// Whether the list is longer than its view, so the scroll bar shows.
    fn overflows(&self) -> bool {
        self.row_h() * self.rows.len() as i32 > self.view_h
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn draw_state(&mut self, d: &mut Draw, area: Rect, view: super::state::View) {
        self.set_state_target(super::state::draw_target(d, area, view));
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn set_state_target(&mut self, retry_target: Option<Rect>) {
        self.rendered_rows.clear();
        self.retry_target = retry_target;
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn clear_hit_targets(&mut self) {
        self.set_state_target(None);
    }

    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn retry_hit(&self, x: i32, y: i32) -> bool {
        self.retry_target.is_some_and(|bounds| contains(&bounds, x, y))
    }

    pub fn draw(&mut self, d: &mut Draw, area: Rect) {
        self.view_h = area.h;
        if self.content_only { self.clamp_scroll(true); }
        let h = self.row_h();
        #[cfg(target_arch = "wasm32")]
        let old = d.c.set_clip(area.intersect(&d.c.clip()));
        #[cfg(not(target_arch = "wasm32"))]
        let old = d.c.set_clip(area);
        #[cfg(target_arch = "wasm32")]
        self.rendered_rows.clear();
        #[cfg(not(target_arch = "wasm32"))]
        self.clear_hit_targets();
        let bar = self.overflows();
        let first = (self.scroll as i32 / h).max(0) as usize;
        let last = ((self.scroll as i32 + area.h) / h + 1).min(self.rows.len() as i32) as usize;
        for i in first..last {
            let y = area.y + i as i32 * h - self.scroll.round() as i32;
            let w = if bar { area.w - SCROLLBAR_W } else { area.w };
            let row = &self.rows[i];
            let passive = self.passive_info && matches!(row.action, RowAction::None);
            let selected = i == self.selected && !passive;
            let bump = if selected { self.bump.round() as i32 } else { 0 };
            let rect = Rect::new(area.x, y + bump, w, h);
            let separator = (row.separator && !selected)
                .then(|| Rect::new(rect.x + 24, rect.y, rect.w - 48, 1));
            self.rendered_rows.push(RenderedRow {
                index: i,
                bounds: rect.intersect(&d.c.clip()),
                separator,
            });
            self.draw_row(d, i, rect, selected, separator);
        }
        if bar {
            self.draw_scrollbar(d, area);
        }
        d.c.set_clip(old);
        if let Some(letter) = &self.letter {
            let th = d.theme;
            let bx = Rect::new(WIDTH / 2 - 40, area.y + area.h / 2 - 40, 80, 80);
            d.c.fill_round_rect_alpha(bx, 14, th.hud, th.hud_alpha);
            let lh = d.fonts.line_height(48);
            d.fonts.draw_centered(d.c, letter, WIDTH / 2, bx.y + (80 - lh) / 2, 48, Weight::Bold, Color::WHITE);
        }
    }

    /// The classic's scroll bar: always there on a long list, a grey thumb in
    /// a pale track along the right edge.
    fn draw_scrollbar(&self, d: &mut Draw, area: Rect) {
        let th = d.theme;
        let total = self.row_h() * self.rows.len() as i32;
        // Clear of the glass's corners at the bottom.
        let bottom = area.bottom() - corner_inset(area.bottom() - 1).min(20);
        let track = Rect::new(area.right() - SCROLLBAR_W, area.y, SCROLLBAR_W, bottom - area.y);
        d.c.fill_rect(track, th.scroll_track);
        d.c.fill_rect(Rect::new(track.x, track.y, 1, track.h), th.scroll_line);
        let thumb_h = (track.h * area.h / total).max(18);
        let max = (total - area.h).max(1) as f32;
        let ty = track.y + ((track.h - thumb_h) as f32 * (self.scroll / max).clamp(0.0, 1.0)) as i32;
        let thumb = Rect::new(track.x + 2, ty + 1, SCROLLBAR_W - 3, thumb_h - 2);
        let r = (thumb.w / 2).max(1);
        // A rounded thumb, shaded left to right as the classic's.
        let old = d.c.set_clip(thumb.intersect(&area));
        d.c.fill_round_rect(thumb, r, th.scroll_thumb_bottom);
        d.c.fill_round_rect(Rect::new(thumb.x, thumb.y, thumb.w - 2, thumb.h), r, th.scroll_thumb_top);
        d.c.set_clip(old);
    }

    fn draw_row(&self, d: &mut Draw, i: usize, r: Rect, selected: bool, separator: Option<Rect>) {
        let row = &self.rows[i];
        let th = d.theme;
        let passive = self.passive_info && matches!(row.action, RowAction::None);
        let y = r.y;
        if selected {
            th.fill_selection(d.c, r);
        }
        let (fg, sub) = if selected {
            (th.sel_text, th.sel_muted)
        } else if passive {
            (th.muted, th.muted)
        } else if row.dim {
            (th.faint, th.faint)
        } else {
            (th.text, th.muted)
        };
        if let Some(line) = separator {
            d.c.fill_rect(line, th.track_line);
        }
        let weight = if passive { Weight::Regular } else { Weight::Bold };
        let lt = d.fonts.line_height(TITLE_SIZE);
        let ld = d.fonts.line_height(DETAIL_SIZE);
        // Where the title (and the detail under it) are drawn.
        let title_y = if row.detail.is_some() { y + (r.h - lt - ld + 2) / 2 } else { y + (r.h - lt) / 2 };
        let detail_y = title_y + lt - 2;
        // The row's own edges, and clear of the glass's corners. Low on the
        // screen the corners close in fastest, so the text is measured at the
        // baseline of its lowest line: a detail line under the bottom corner
        // is never cut.
        let probe = if y + r.h / 2 > crate::gfx::HEIGHT / 2 {
            if row.detail.is_some() { detail_y + d.fonts.ascent(DETAIL_SIZE) } else { title_y + d.fonts.ascent(TITLE_SIZE) }
        } else { y + r.h / 2 };
        let (edge_l, edge_r) = row_edges(probe.max(y + r.h / 2));
        // Media stays in straight columns as rows pass the rounded glass.
        // Text-only menus follow the corners, with a little more left padding.
        let left = if self.media { r.x + SIDE } else { (r.x + SIDE).max(edge_l) + 4 };
        let right_edge = (r.right() - SIDE).min(edge_r);
        let mut x = left;
        if self.media {
            let ay = y + (r.h - ART) / 2;
            let loaded = if let Some(artist) = &row.artist_image {
                d.lib.art.get_artist(artist, ART as u32)
            } else {
                row.art.as_deref().and_then(|id| d.lib.art.get(id, ART as u32))
            };
            let img: Arc<Image> = loaded.unwrap_or_else(|| d.placeholder(ART as u32));
            let ax = left - SIDE + ART_INSET;
            d.c.draw_image_rounded(&img, ax, ay, 2);
            let cover = Rect::new(ax, ay, ART, ART);
            if let Some(progress) = artwork_download_progress(row.badge) {
                icons::download_pie(d.c, cover, progress);
            }
            icons::listening_progress(d.c, cover, row.spoken_progress);
            x = ax + ART + 10;
        }
        // Right-hand furniture first, so the title knows its room.
        let mut right = right_edge;
        if row.chevron {
            let cx = right - 9;
            icons::chevron(d.c, cx as f32, (y + r.h / 2) as f32, 12.0, if selected { fg } else { th.chevron });
            right = cx - 8;
        }
        let badge = visible_badge(row.badge, row.song_item().is_some_and(|s| d.status.current().is_some_and(|p| p.id == s.id)));
        match badge {
            Badge::Offline => {
                icons::download(d.c, (right - 12) as f32, (y + r.h / 2 - 6) as f32, 12.0, sub);
                right -= 20;
            }
            Badge::Playing => {
                icons::speaker(d.c, (right - 14) as f32, (y + r.h / 2 - 6) as f32, 13.0, if selected { fg } else { th.sel_mid });
                right -= 22;
            }
            Badge::Check => {
                icons::check(d.c, (right - 14) as f32, (y + r.h / 2 - 6) as f32, 13.0, fg);
                right -= 22;
            }
            Badge::Queued => {
                // The download glyph, faint: asked for, not yet on the card.
                icons::download(d.c, (right - 12) as f32, (y + r.h / 2 - 6) as f32, 12.0, sub.over(th.bg, 110));
                right -= 20;
            }
            Badge::Downloading(_) if self.media => {}
            Badge::Downloading(percent) => {
                let (cx, cy) = ((right - 7) as f32, (y + r.h / 2) as f32);
                icons::progress_ring(d.c, cx, cy, 6.5, (percent != 255).then(|| percent as f32 / 100.0), sub, sub.over(th.bg, 70));
                right -= 20;
            }
            Badge::None => {}
        }
        if let Some(v) = &row.value {
            // Leave room for the label even when a status or device name is long.
            let shortened;
            let v = if self.passive_info {
                shortened = d.fonts.ellipsize(v, DETAIL_SIZE + 1, Weight::Regular, ((right - x) / 2).max(20));
                shortened.as_ref()
            } else { v.as_str() };
            let w = d.fonts.measure(v, DETAIL_SIZE + 1, Weight::Regular);
            let vy = y + (r.h - d.fonts.line_height(DETAIL_SIZE + 1)) / 2;
            d.fonts.draw(d.c, v, right - w, vy, DETAIL_SIZE + 1, Weight::Regular, sub);
            right -= w + 10;
        }
        let w = (right - x).max(20);
        d.fonts.draw_fit(d.c, &row.title, x, title_y, w, TITLE_SIZE, weight, fg);
        if let Some(det) = &row.detail {
            d.fonts.draw_fit(d.c, det, x, detail_y, w, DETAIL_SIZE, Weight::Regular, sub);
        }
    }
}

/// The letter shown while spinning: the first of the title, skipping "The",
/// "A" and "An" and leading spaces, Latin letters in capitals.
pub fn index_letter(title: &str) -> String {
    let t = title.trim_start();
    let lower = t.to_lowercase();
    let t = ["the ", "a ", "an "].iter().find(|p| lower.starts_with(*p)).map(|p| &t[p.len()..]).unwrap_or(t);
    t.trim_start().chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A list holds one row per song; rows and songs must not grow (host,
    /// 64-bit; the Q2's are about half). Before V0.124 a row was 1,024 bytes
    /// and a song 624, and a song list held each song four times.
    #[test]
    fn rows_and_songs_stay_small() {
        assert!(std::mem::size_of::<Row>() <= 264, "Row is {} bytes", std::mem::size_of::<Row>());
        assert!(std::mem::size_of::<Song>() <= 392, "Song is {} bytes", std::mem::size_of::<Song>());
        let songs = Arc::new(vec![Song { id: "a".into(), ..Default::default() }, Song { id: "b".into(), ..Default::default() }]);
        let row = Row::new("b", RowAction::Play { songs: songs.clone(), index: 1 }).shared_song();
        assert_eq!(row.song_item().map(|s| s.id.as_str()), Some("b"));
        assert!(row.song.is_none(), "a song row refers to its list's song");
    }

    #[test]
    fn taps_skip_separators_passive_rows_and_rounded_corners() {
        let mut list = ListView::new(false);
        list.set_rows(vec![Row::new("Action", RowAction::Cmd(0)), Row::new("Status", RowAction::None)]);
        let first = Rect::new(0, STATUS_H, WIDTH, ROW_H);
        list.rendered_rows.extend([
            RenderedRow { index: 0, bounds: first, separator: Some(Rect::new(24, STATUS_H, WIDTH - 48, 1)) },
            RenderedRow { index: 1, bounds: Rect::new(0, first.bottom(), WIDTH, ROW_H), separator: None },
        ]);
        assert_eq!(list.hit(0, STATUS_H + 1), None);
        assert_eq!(list.hit(WIDTH / 2, STATUS_H), None);
        assert_eq!(list.hit(WIDTH / 2, first.bottom()), None);
        assert_eq!(list.hit(WIDTH / 2, STATUS_H + 1), Some(0));
        assert!(list.select_at(WIDTH / 2, STATUS_H + 1));
        list.set_rows(vec![Row::new("Replacement", RowAction::Cmd(1))]);
        assert_eq!(list.hit(WIDTH / 2, STATUS_H + 1), None);
    }

    #[test]
    fn rows_below_follow_the_view_and_stop_at_the_end() {
        let mut l = ListView::new(true);
        l.set_rows((0..20).map(|n| Row::new(n.to_string(), RowAction::Cmd(n))).collect());
        l.view_h = 3 * MEDIA_ROW_H;
        // The view shows rows 0..3 exactly: 3.. is below.
        assert_eq!(l.rows_below(6), 3..9);
        l.select(19);
        assert_eq!(l.rows_below(6), 20..20);
        l.select(14);
        let below = l.rows_below(6);
        assert!(below.start > 14 && below.end <= 20 && below.len() <= 6, "{below:?}");
    }

    fn list(n: usize) -> ListView {
        let mut l = ListView::new(false);
        l.set_rows((0..n).map(|i| Row::new(format!("Row {i}"), RowAction::None)).collect());
        l
    }

    fn reading_page(n: usize) -> ListView {
        let mut l = ListView::settings(true);
        l.view_h = TALL_ROW_H * 3;
        l.set_rows((0..n).map(|i| Row::new(format!("Information {i}"), RowAction::None)
            .detail("Generated detail")).collect());
        l
    }

    #[test]
    fn reading_page_moves_content_on_the_first_detent_without_focus_or_coasting() {
        let mut l = reading_page(12);
        let now = Instant::now();
        assert!(l.selected_row().is_none());
        assert!(l.wheel(1, now));
        assert_eq!(l.scroll, CONTENT_SCROLL_STEP as f32, "no invisible row traversal before scrolling");
        assert_eq!(l.selected, 0);
        l.select(8);
        assert_eq!(l.selected, 0, "reading has no selectable row");
        assert!(!l.animate(now + Duration::from_millis(16)));
        assert_eq!(l.scroll, CONTENT_SCROLL_STEP as f32);
        l.set_rows(l.rows.clone());
        assert_eq!(l.scroll, CONTENT_SCROLL_STEP as f32, "refresh preserves the reading position");
        assert!(l.wheel(i32::MAX, now));
        assert_eq!(l.scroll, l.max_scroll());
        assert!(!l.wheel(1, now), "no wrap past the last content");
        l.wheel(-1, now);
        assert_eq!(l.scroll, l.max_scroll() - CONTENT_SCROLL_STEP as f32, "reversal moves immediately");
        l.wheel(i32::MIN, now);
        assert_eq!(l.scroll, 0.0);
        assert!(!l.wheel(-1, now));
        assert!(l.selected_row().is_none());
    }

    #[test]
    fn short_or_replaced_reading_pages_have_no_latent_scroll_or_selection() {
        let now = Instant::now();
        let mut l = reading_page(1);
        assert!(!l.wheel(20, now));
        assert_eq!((l.scroll, l.selected), (0.0, 0));
        l.set_rows(reading_page(12).rows);
        assert_eq!(l.scroll, 0.0, "wheel movement during loading is not retained");
        l.wheel(8, now);
        assert!(l.scroll > 0.0);
        l.set_rows(reading_page(2).rows);
        assert_eq!(l.scroll, 0.0, "shrinking content clamps the reading position");
        assert!(l.selected_row().is_none());
    }

    #[test]
    fn mixed_information_and_controls_still_use_actionable_selection() {
        let mut l = ListView::settings(true);
        l.set_rows(vec![Row::new("Status", RowAction::None),
            Row::new("First control", RowAction::Cmd(1)),
            Row::new("More information", RowAction::None),
            Row::new("Second control", RowAction::Cmd(2))]);
        l.select(1);
        l.wheel(1, Instant::now());
        assert_eq!(l.selected_row().unwrap().title, "Second control");
        l.wheel(-1, Instant::now());
        assert_eq!(l.selected_row().unwrap().title, "First control");
    }

    #[test]
    fn one_row_a_tick_then_accelerates() {
        let mut l = list(100);
        let t0 = Instant::now();
        l.wheel(1, t0);
        assert_eq!(l.selected, 1);
        // A steady spin: ticks 100 ms apart.
        for i in 1..=10 {
            l.wheel(1, t0 + Duration::from_millis(100 * i));
        }
        assert!(l.selected > 11, "spin accelerates: {}", l.selected);
        let before = l.selected;
        l.wheel(1, t0 + Duration::from_millis(3000));
        assert_eq!(l.selected, before + 1, "a pause resets the speed");
    }

    #[test]
    fn short_lists_never_accelerate() {
        let mut l = list(10);
        let t0 = Instant::now();
        for i in 0..20 {
            l.wheel(1, t0 + Duration::from_millis(50 * i));
        }
        assert_eq!(l.selected, 9);
    }

    #[test]
    fn endpoints_never_wrap() {
        let mut l = list(5);
        let t0 = Instant::now();
        l.select(4);
        l.wheel(1, t0);
        l.wheel(10, t0 + Duration::from_millis(500));
        assert_eq!(l.selected, 4);
        l.select(0);
        l.wheel(-10, t0 + Duration::from_secs(1));
        assert_eq!(l.selected, 0);
    }

    #[test]
    fn velocity_reversal_and_lift() {
        let t = Instant::now();
        let mut slow = list(1000);
        let mut fast = list(1000);
        for i in 0..10 {
            slow.wheel(1, t + Duration::from_millis(i * 200));
            fast.wheel(1, t + Duration::from_millis(i * 50));
        }
        assert_eq!(slow.selected, 10, "slow turns remain precise indefinitely");
        assert!(fast.selected > 30);
        let before = fast.selected;
        fast.wheel(-1, t + Duration::from_millis(500));
        assert_eq!(fast.selected, before - 1);
        fast.end_spin();
        fast.wheel(-1, t + Duration::from_millis(510));
        assert_eq!(fast.selected, before - 2, "lift resets gain");
    }

    fn alphabet_list() -> ListView {
        let mut l = ListView::new(true);
        l.alphabetical = true;
        l.set_rows((0..1000).map(|i| Row::new(format!("{} Album {i}", (b'A' + (i / 40) as u8) as char), RowAction::None)).collect());
        l
    }

    fn deliberate_spin(l: &mut ListView, t: Instant) {
        for i in 0..=15 { l.wheel(1, t + Duration::from_millis(i * 45)); }
    }

    #[test]
    fn global_alphabet_spin_requests_external_groups_without_selecting_unloaded_rows() {
        let t = Instant::now(); let mut list = alphabet_list();
        list.global_alphabet = true;
        list.set_global_letters(true);
        list.set_rows((0..100).map(|i| Row::new(format!("A album {i}"), RowAction::None).key(format!("{i}"))).collect());
        deliberate_spin(&mut list, t);
        assert_eq!(list.letter_label(), Some("A"));
        assert_eq!(list.take_alphabet_jump(), 0, "entry keeps the current global group");
        let selected = list.selected;
        list.wheel(2, t + Duration::from_secs(2));
        assert_eq!(list.take_alphabet_jump(), 0);
        list.wheel(1, t + Duration::from_secs(3));
        assert_eq!(list.selected, selected);
        assert_eq!(list.take_alphabet_jump(), 1);
        list.show_alphabet_jump("Z".into());
        assert_eq!(list.letter_label(), Some("Z"));
        assert!(list.finish_letters());
        list.show_alphabet_jump("Z".into());
        assert_eq!(list.letter_label(), None, "a late lookup cannot restore the overlay after lift");
        list.wheel(1, t + Duration::from_secs(4));
        assert_eq!(list.selected, selected + 1);
        assert_eq!(list.take_alphabet_jump(), 0);
    }

    #[test]
    fn alphabet_search_moves_groups_and_centre_exits() {
        let t = Instant::now();
        let mut l = alphabet_list();
        deliberate_spin(&mut l, t);
        assert_eq!(l.letter_label(), Some("A"));
        assert_eq!(l.selected, 15, "entry preserves the current row");
        l.wheel(3, t + Duration::from_millis(800));
        assert_eq!(l.selected, 40);
        assert_eq!(l.letter_label(), Some("B"));
        assert!(l.finish_letters());
        assert!(!l.finish_letters());
        for i in 0..20 {
            let old = l.selected;
            l.wheel(1, t + Duration::from_millis(900 + i * 100));
            assert_eq!(l.selected, old + 1, "Centre restores detailed selection indefinitely");
            assert_eq!(l.letter_label(), None);
        }
    }

    #[test]
    fn ordinary_alphabet_browsing_never_skips_rows_or_arms_letters() {
        let t=Instant::now();
        let mut l=alphabet_list();
        for i in 0..100 {
            l.wheel(1,t+Duration::from_millis(i*100));
            assert_eq!(l.selected,i as usize+1);
            assert!(!l.letter_mode);
        }
        // A short high-speed burst and a large coalesced event are not proof
        // of a sustained fast spin.
        l.end_spin();
        for i in 0..20 { l.wheel(1,t+Duration::from_millis(11000+i*12)); }
        assert!(!l.letter_mode);
        l.end_spin();l.wheel(40,t+Duration::from_secs(13));
        l.wheel(40,t+Duration::from_secs(13));
        assert!(!l.letter_mode);
        l.set_rows((0..200).map(|i|Row::new(format!("AKG model {i}"),RowAction::None)).collect());
        l.select(0);deliberate_spin(&mut l,t+Duration::from_secs(20));
        assert_eq!(l.selected,16,"a single-letter search must not jump back to its first row");
        assert!(!l.letter_mode);
    }

    #[test]
    fn letter_entry_tolerates_jitter_but_requires_sustained_fast_travel() {
        let start = Instant::now();
        let mut list = alphabet_list();
        for i in 0..=60 {
            list.wheel(1, start + Duration::from_millis(i * 60));
            assert_eq!(list.selected, i as usize + 1, "the former trigger speed stays in rows");
        }
        for i in 1..=6 {
            list.wheel(1, start + Duration::from_millis(3600 + i * 45));
            assert_eq!(list.letter_label(), None, "a slow lead-in cannot arm a short fast burst");
        }
        list.end_spin();

        let mut time = start + Duration::from_secs(5);
        list.wheel(1, time);
        for i in 0..15 {
            time += Duration::from_millis(if i % 2 == 0 { 38 } else { 62 });
            list.wheel(1, time);
            assert_eq!(list.letter_mode, i == 14, "alternating 38/62 ms trace");
        }
    }

    #[test]
    fn letter_entry_restarts_after_hesitation_reversal_or_lift() {
        for (gap, ticks, lift) in [(110, 1, false), (45, -1, false), (45, 1, true)] {
            let start = Instant::now();
            let mut list = alphabet_list();
            for i in 0..15 {
                list.wheel(1, start + Duration::from_millis(i * 45));
                assert_eq!(list.letter_label(), None);
            }
            if lift { list.end_spin(); }
            let next = start + Duration::from_millis(14 * 45 + gap);
            list.wheel(ticks, next);
            for i in 1..=8 {
                list.wheel(ticks, next + Duration::from_millis(i * 45));
                assert_eq!(list.letter_label(), None);
            }
        }
    }

    #[test]
    fn alphabet_mode_stays_latched_through_rest_slow_movement_and_reversal() {
        let t = Instant::now();
        let mut list = alphabet_list();
        deliberate_spin(&mut list, t);
        assert!(!list.animate(t + Duration::from_secs(8)), "a held overlay does not animate");
        assert_eq!(list.letter_label(), Some("A"));

        for (i, ticks, label, selected) in [
            (9, 1, "A", 15), (10, 1, "A", 15), (11, -1, "A", 15),
            (12, 1, "A", 15), (13, 1, "B", 40), (14, -2, "B", 40), (15, -1, "A", 0),
        ] {
            list.wheel(ticks, t + Duration::from_secs(i));
            assert_eq!(list.letter_label(), Some(label));
            assert_eq!(list.selected, selected);
        }
        list.end_spin();
        assert_eq!(list.letter_label(), None);
        list.wheel(1, t + Duration::from_secs(16));
        assert_eq!(list.selected, 1, "the next contact starts in rows");
    }

    #[test]
    fn akg_k702_is_reachable_row_by_row_in_the_full_headphone_list() {
        let mut l=ListView::new(false);l.alphabetical=true;
        l.set_rows(crate::audio::autoeq::search("").iter().map(|e|Row::new(e.name(),RowAction::None)).collect());
        let first=l.rows.iter().position(|r|r.title.starts_with("AKG ")).unwrap();
        let target=l.rows.iter().position(|r|r.title=="AKG K702").unwrap();
        l.select(first);let t=Instant::now();
        for i in first..target { l.wheel(1,t+Duration::from_millis((i-first) as u64*100)); }
        assert_eq!(l.selected_row().unwrap().title,"AKG K702");
        assert!(!l.letter_mode);
    }

    #[test]
    fn a_fast_spin_past_the_end_stops_there() {
        let mut l = list(5);
        let t0 = Instant::now();
        l.select(3);
        l.wheel(1, t0 - Duration::from_secs(1));
        l.wheel(10, t0);
        assert_eq!(l.selected, 4, "no wrap within one burst of ticks");
    }

    #[test]
    fn continuous_page_changes_keep_the_selected_content_at_the_same_pixel() {
        let rows=|start:usize,end:usize| (start..end).map(|i|Row::new(format!("Generated {i}"),RowAction::Cmd(1)).key(i.to_string())).collect();
        for media in [false,true] {
            let mut list=ListView::new(media);list.set_rows(rows(50,250));list.select(170);
            let key=list.selected_row().unwrap().key.clone();
            let y=list.selected as f32*list.row_h() as f32-list.scroll;
            for (start,end) in [(50,300),(100,350),(50,300),(0,250)] {
                list.set_rows_anchored(rows(start,end));
                assert_eq!(list.selected_row().unwrap().key,key);
                assert_eq!(list.selected as f32*list.row_h() as f32-list.scroll,y);
            }
        }
    }

    #[test]
    fn keeps_selection_across_reloads() {
        let mut l = list(5);
        l.select(3);
        let mut rows: Vec<Row> = (0..5).map(|i| Row::new(format!("Row {i}"), RowAction::None)).collect();
        rows.insert(0, Row::new("New", RowAction::None));
        l.set_rows(rows);
        assert_eq!(l.selected_row().unwrap().title, "Row 3");
    }

    #[test]
    fn letters() {
        assert_eq!(index_letter("The Beatles"), "B");
        assert_eq!(index_letter("  abba"), "A");
        assert_eq!(index_letter("Ánimo"), "Á");
        assert_eq!(index_letter("An Album"), "A");
        assert_eq!(index_letter("Another"), "A");
        assert_eq!(index_letter(""), "");
    }
}

#[cfg(test)]
mod download_badge_tests {
    use super::*;
    #[test]
    fn playback_speaker_does_not_hide_artwork_progress() {
        assert_eq!(artwork_download_progress(Badge::Downloading(40)),Some(Some(0.4)));
        assert!(matches!(visible_badge(Badge::Downloading(40),true),Badge::Playing));
        assert_eq!(artwork_download_progress(Badge::Queued),Some(None));
        assert_eq!(artwork_download_progress(Badge::Downloading(255)),Some(None));
    }
}
