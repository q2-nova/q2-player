//! Cover Flow: every album as a cover, the chosen one facing out in the
//! middle, the others fanned out behind it, with soft reflections on the
//! theme’s background. The wheel flips through them; the centre opens the album.

use super::popup;
use crate::gfx::{Canvas, Color, Image, Rect, WIDTH, Weight};
use crate::input::Event;
use crate::library::{LibError, Policy};
use crate::ui::list::Row;
use crate::ui::{Action, Ctx, Dest, Draw, Screen};
use std::any::Any;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
#[cfg(target_arch = "wasm32")]
use crate::clock::Instant;
use subsonic::Album;

/// The middle cover's size.
const SIZE: i32 = 150;
const TOP: i32 = 28;
/// The fanned covers recede toward the middle, where the main cover overlaps them.
const SIDE_W: f32 = 64.0;
const OUTER_HEIGHT: f32 = 0.94;
const INNER_HEIGHT: f32 = 0.72;
/// The first side cover's centre from the middle, and the step after it.
const FIRST: f32 = 101.0;
const STEP: f32 = 27.0;
const REFLECTION: i32 = 36;
/// Covers drawn either side.
const REACH: i32 = 6;
/// Covers warmed beyond the drawn ones, in the direction of travel.
const PREFETCH_AHEAD: i64 = 8;

pub struct CoverFlow {
    albums: Vec<Album>,
    selected: usize,
    /// The position shown, easing toward `selected`.
    shown: f32,
    error: Option<String>,
    loading: bool,
    /// The selection whose neighbours' covers were last warmed.
    warmed: Option<usize>,
    /// Frames drawn with covers, and those whose middle cover was still a
    /// placeholder (logged on close, to measure prefetching).
    frames: u32,
    missing: u32,
    visible: Option<(i32, i32)>,
    entries: u32,
    ready_entries: u32,
    /// The first load's deadline (UX-006).
    deadline: crate::ui::state::Deadline,
    started: bool,
    /// Whose artwork cache to shrink when Cover Flow closes.
    lib: Option<std::sync::Arc<crate::library::Library>>,
    hit_covers: Vec<(usize, f32, f32, f32, f32, f32)>,
    #[cfg(not(target_arch = "wasm32"))]
    retry_target: Option<Rect>,
}

impl Drop for CoverFlow {
    fn drop(&mut self) {
        if self.frames > 0 { log::info!("cover flow: {} frames, middle cover missing in {}, art ready on entry {}/{}", self.frames, self.missing, self.ready_entries, self.entries); }
        // Its covers filled the decoded-art cache: keep a little, and return
        // the freed heap to the kernel (docs/server-status.md, memory).
        if let Some(lib) = self.lib.take() {
            lib.art.close_window();
            lib.art.shrink_memory(crate::library::art::MEMORY_AFTER_BROWSING);
            crate::platform::resident::trim_after_browsing();
        }
    }
}

impl CoverFlow {
    pub fn new() -> CoverFlow {
        CoverFlow { albums: Vec::new(), selected: 0, shown: 0.0, error: None, loading: true, warmed: None, frames: 0, missing: 0, visible: None, entries: 0, ready_entries: 0,
            deadline: crate::ui::state::Deadline::new(Instant::now()), started: false, lib: None,
            hit_covers: Vec::new(),
            #[cfg(not(target_arch = "wasm32"))]
            retry_target: None,
        }
    }
}

/// Where cover `d` places from the middle (fractional while moving) is
/// drawn: its centre, width, and the heights of its left and right edges.
fn placement(d: f32, centre: f32) -> (f32, f32, f32, f32) {
    let a = d.abs().min(1.0);
    let side_x = FIRST + (d.abs() - 1.0).max(0.0) * STEP;
    let x = centre + d.signum() * a * side_x;
    let w = SIZE as f32 + (SIDE_W - SIZE as f32) * a;
    let outer = SIZE as f32 * (1.0 + (OUTER_HEIGHT - 1.0) * a);
    let inner = SIZE as f32 * (1.0 + (INNER_HEIGHT - 1.0) * a);
    if d < 0.0 { (x, w, outer, inner) } else { (x, w, inner, outer) }
}

/// Pixels of a column handled per pass (stack buffers, no allocation).
const SEGMENT: usize = 512;
/// Source rows by fixed point: 20 fraction bits, and the distance from a
/// whole row within which the float expression decides instead.
const FRACTION: u32 = 20;
const MARGIN: u32 = 2048;

/// The reflection's alpha before shading, by `[refl][row]`.
/// The quadratic falloff keeps the reflection recognizable at the edge
/// of the sleeve and softens it into the floor before the caption.
const FADE: [[u8; REFLECTION as usize]; REFLECTION as usize + 1] = {
    let mut table = [[0u8; REFLECTION as usize]; REFLECTION as usize + 1];
    let mut refl = 1;
    while refl <= REFLECTION as usize {
        let mut row = 0;
        while row < refl {
            table[refl][row] = fade(row as i32, refl as i32) as u8;
            row += 1;
        }
        refl += 1;
    }
    table
};

const fn fade(row: i32, refl: i32) -> u32 {
    let remaining = 1.0 - row as f32 / refl as f32;
    (95.0 * remaining * remaining) as u32
}

/// A cover to draw: its image, centre, width, edge heights and shade.
struct Turned<'a> {
    img: &'a Image,
    cx: f32,
    w: f32,
    hl: f32,
    hr: f32,
    shade: u32,
}

/// The most covers drawn in a frame, and the widest canvas with culling.
const COVERS: usize = (2 * REACH + 1) as usize;
const HIDDEN_W: usize = 384;
/// Per canvas column, the screen rows `[from, to)` that covers drawn later
/// overwrite with opaque pixels.
type Hidden = [(i16, i16); HIDDEN_W];

/// An image no cover draws (`column_range` skips it); fills unused slots.
static EMPTY: Image = Image { width: 0, height: 0, pixels: Vec::new() };

/// Column `col` of a turned cover: its place across the cover (0..1), its
/// height, first screen row and row count.
#[inline(always)]
fn column(col: i32, cy: f32, w: f32, hl: f32, hr: f32) -> (f32, f32, i32, i32) {
    let u = (col as f32 + 0.5) / w;
    let h = hl + (hr - hl) * u;
    let top = cy - h / 2.0;
    (u, h, top as i32, h.round() as i32)
}

/// The columns of `c` that cover `t` draws: its left edge and `first..end`.
fn column_range(c: &Canvas, t: &Turned) -> Option<(i32, i32, i32)> {
    let cols = t.w.round() as i32;
    if cols <= 0 || t.img.width == 0 || t.img.height == 0 {
        return None;
    }
    let left = (t.cx - t.w / 2.0).round() as i32;
    let clip = c.clip();
    Some((left, (clip.x - left).max(0), cols.min(clip.right() - left)))
}

/// Adds the opaque rows of `t` to `hull`. A column keeps one interval:
/// spans that meet are joined, and of two apart the longer stays, so the
/// interval never claims a row that is not overwritten.
fn add_opaque(hull: &mut Hidden, c: &Canvas, t: &Turned, cy: f32) {
    let Some((left, first, end)) = column_range(c, t) else { return };
    for col in first..end {
        let (_, _, y0, rows) = column(col, cy, t.w, t.hl, t.hr);
        if rows <= 0 {
            continue;
        }
        let (from, to) = (y0.clamp(-30000, 30000) as i16, y0.saturating_add(rows).clamp(-30000, 30000) as i16);
        let slot = &mut hull[(left + col) as usize];
        if slot.0 >= slot.1 {
            *slot = (from, to);
        } else if from <= slot.1 && to >= slot.0 {
            *slot = (slot.0.min(from), slot.1.max(to));
        } else if to - from > slot.1 - slot.0 {
            *slot = (from, to);
        }
    }
}

/// Draws `covers` in order (the farthest first), all centred on `cy`.
/// Pixels a later cover overwrites are skipped: a cover's own pixels are
/// opaque, so what was under them never shows, and the reflections blended
/// between are overwritten with them.
fn draw_covers(c: &mut Canvas, covers: &[Turned], cy: f32) {
    if covers.len() > COVERS || c.width as usize > HIDDEN_W || c.height > 30000 {
        for t in covers { draw_turned(c, t.img, t.cx, cy, t.w, t.hl, t.hr, t.shade, None); }
        return;
    }
    let mut hidden = [[(0i16, 0i16); HIDDEN_W]; COVERS];
    let mut hull = [(0i16, 0i16); HIDDEN_W];
    for (k, t) in covers.iter().enumerate().rev() {
        hidden[k] = hull;
        add_opaque(&mut hull, c, t, cy);
    }
    for (k, t) in covers.iter().enumerate() {
        draw_turned(c, t.img, t.cx, cy, t.w, t.hl, t.hr, t.shade, Some(&hidden[k]));
    }
}

/// The source row of screen row `row` of a column `h` tall:
/// `((row + 0.5) / h * sh) as u32`. `step` is `sh / h` in half rows with
/// `FRACTION` bits: `(2 row + 1) * step` differs from the exact quotient by
/// at most `row + 0.5` units and the f32 expression by at most 128 (two
/// roundings of a value below 1024), so outside `MARGIN` of a whole row all
/// three have the same integer part. Inside it the f32 expression decides.
#[inline(always)]
fn source_row(row: i32, step: u32, h: f32, sh: f32) -> u32 {
    if step != 0 {
        let v = (2 * row as u32 + 1).wrapping_mul(step);
        if (v.wrapping_add(MARGIN) & ((1 << FRACTION) - 1)) >= 2 * MARGIN {
            return v >> FRACTION;
        }
    }
    ((row as f32 + 0.5) / h * sh) as u32
}

/// The reflection's source row, `(sh - 1.0 - (row + 0.5) / h * sh).max(0.0) as u32`:
/// one more rounding (at most 64 units) than `source_row`, the same margin.
#[inline(always)]
fn mirrored_row(row: i32, step: u32, h: f32, sh: f32, height: u32) -> u32 {
    if step != 0 {
        let v = (2 * row as u32 + 1).wrapping_mul(step);
        if (v.wrapping_add(MARGIN) & ((1 << FRACTION) - 1)) >= 2 * MARGIN {
            return (height as i32 - 2 - (v >> FRACTION) as i32).max(0) as u32;
        }
    }
    (sh - 1.0 - (row as f32 + 0.5) / h * sh).max(0.0) as u32
}

/// Draws `img` as a quadrilateral `w` wide centred on `cx`, its left edge
/// `hl` tall and its right `hr`, centred on `cy`, then its reflection.
///
/// Column by column, with the column's state in locals: the source pixels
/// of a run of rows are gathered into a buffer, one span call shades the
/// buffer (the cover) or blends it at per-pixel alpha over the canvas
/// column (the reflection), and the buffer is written down the column.
/// `hidden` names rows not to draw (`draw_covers`).
fn draw_turned(c: &mut Canvas, img: &Image, cx: f32, cy: f32, w: f32, hl: f32, hr: f32, shade: u32, hidden: Option<&Hidden>) {
    let Some((left, first, end)) = column_range(c, &Turned { img, cx, w, hl, hr, shade }) else { return };
    let (sw, sh) = (img.width as f32, img.height as f32);
    let clip = c.clip();
    let fixed = img.height <= 1024;
    for col in first..end {
        let (u, h, y0, rows) = column(col, cy, w, hl, hr);
        let x = left + col;
        let strip = TurnedColumn {
            x,
            sx: ((u * sw) as u32).min(img.width - 1),
            h,
            sh,
            step: if fixed && rows <= 1024 && h.is_finite() && h > 0.0 {
                (sh as f64 / h as f64 * (1u32 << (FRACTION - 1)) as f64).round() as u32
            } else { 0 },
            shade,
        };
        // Clipped columns are on the canvas, which is at most HIDDEN_W wide with `hidden`.
        let (hide_from, hide_to) = hidden.map_or((0, 0), |hidden| { let (a, b) = hidden[x as usize]; (a as i32, (b as i32).max(a as i32)) });
        // The rows of [from, to) inside the clip, before and after the hidden rows.
        let visible = |from: i32, to: i32| {
            let (from, to) = (from.max(clip.y), to.min(clip.bottom()));
            [(from, to.min(hide_from)), (from.max(hide_to), to)]
        };
        if rows > 0 {
            for (from, to) in visible(y0, y0.saturating_add(rows)) {
                strip.cover(c, img, y0, from, to);
            }
        }
        // The reflection: mirrored, fading into the floor.
        let refl = (REFLECTION as f32 * h / SIZE as f32) as i32;
        if refl > 0 {
            let base = y0 + rows + 1;
            for (from, to) in visible(base, base.saturating_add(refl)) {
                strip.reflection(c, img, base, refl, from, to);
            }
        }
    }
}

/// One screen column of a turned cover: its canvas column, source column,
/// height, the image's height, the fixed-point source step (0: use the
/// float expression) and the cover's shade.
struct TurnedColumn {
    x: i32,
    sx: u32,
    h: f32,
    sh: f32,
    step: u32,
    shade: u32,
}

impl TurnedColumn {
    /// Screen rows `from..to` of the cover, whose row 0 is screen row `y0`.
    fn cover(&self, c: &mut Canvas, img: &Image, y0: i32, mut from: i32, to: i32) {
        let (iw, last, width) = (img.width, img.height - 1, c.width as usize);
        let mut run = [0u32; SEGMENT];
        while from < to {
            let n = ((to - from) as usize).min(SEGMENT);
            let mut row = from - y0;
            for slot in &mut run[..n] {
                let sy = source_row(row, self.step, self.h, self.sh).min(last);
                *slot = img.pixels[(sy * iw + self.sx) as usize];
                row += 1;
            }
            crate::gfx::span::blend_color(&mut run[..n], Color::BLACK.0, self.shade);
            let at = from as usize * width + self.x as usize;
            for (pixel, value) in c.pixels[at..].iter_mut().step_by(width).zip(&run[..n]) {
                *pixel = *value;
            }
            from += n as i32;
        }
    }

    /// Screen rows `from..to` of the reflection, `refl` rows from `base`.
    fn reflection(&self, c: &mut Canvas, img: &Image, base: i32, refl: i32, mut from: i32, to: i32) {
        let (iw, last, width) = (img.width, img.height - 1, c.width as usize);
        let shaded = self.shade / 3;
        let mut run = [0u32; SEGMENT];
        let mut under = [0u32; SEGMENT];
        let mut alpha = [0u8; SEGMENT];
        while from < to {
            let n = ((to - from) as usize).min(SEGMENT);
            let mut row = from - base;
            for (slot, a) in run[..n].iter_mut().zip(&mut alpha[..n]) {
                let sy = mirrored_row(row, self.step, self.h, self.sh, img.height).min(last);
                *slot = img.pixels[(sy * iw + self.sx) as usize];
                let faded = if refl <= REFLECTION { FADE[refl as usize][row as usize] as u32 } else { fade(row, refl) };
                *a = faded.saturating_sub(shaded) as u8;
                row += 1;
            }
            let at = from as usize * width + self.x as usize;
            for (value, pixel) in under[..n].iter_mut().zip(c.pixels[at..].iter().step_by(width)) {
                *value = *pixel;
            }
            crate::gfx::span::blend_pixels_mask(&mut under[..n], &run[..n], &alpha[..n]);
            for (pixel, value) in c.pixels[at..].iter_mut().step_by(width).zip(&under[..n]) {
                *pixel = *value;
            }
            from += n as i32;
        }
    }
}

/// The per-pixel code `draw_turned` replaced, as the reference for tests.
#[cfg(test)]
fn reference_draw_turned(c: &mut Canvas, img: &Image, cx: f32, cy: f32, w: f32, hl: f32, hr: f32, shade: u32) {
    let cols = w.round() as i32;
    if cols <= 0 {
        return;
    }
    let left = (cx - w / 2.0).round() as i32;
    let (sw, sh) = (img.width as f32, img.height as f32);
    let clip = c.clip();
    for col in 0..cols {
        let x = left + col;
        if x < clip.x || x >= clip.right() {
            continue;
        }
        let t = (col as f32 + 0.5) / w;
        let h = hl + (hr - hl) * t;
        let sx = ((t * sw) as u32).min(img.width - 1);
        let top = cy - h / 2.0;
        let rows = h.round() as i32;
        let y0 = top as i32;
        for row in (clip.y-y0).max(0)..rows.min(clip.bottom()-y0) {
            let sy = ((row as f32 + 0.5) / h * sh) as u32;
            let p = Color(img.pixels[(sy.min(img.height - 1) * img.width + sx) as usize]);
            let p = Color::BLACK.over(p, shade);
            // Both coordinates are clipped once outside this inner loop.
            c.pixels[((y0+row)*c.width+x) as usize] = p.0;
        }
        // The reflection: mirrored, fading into the floor.
        let base = top as i32 + rows + 1;
        let refl = (REFLECTION as f32 * h / SIZE as f32) as i32;
        for row in (clip.y-base).max(0)..refl.min(clip.bottom()-base) {
            let sy = sh - 1.0 - (row as f32 + 0.5) / h * sh;
            let p = Color(img.pixels[((sy.max(0.0) as u32).min(img.height - 1) * img.width + sx) as usize]);
            let remaining = 1.0 - row as f32 / refl as f32;
            let alpha = (95.0 * remaining * remaining) as u32;
            let i=((base+row)*c.width+x) as usize;
            c.pixels[i]=p.over(Color(c.pixels[i]),alpha.saturating_sub(shade/3)).0;
        }
    }
}

impl CoverFlow {
    /// Cached albums first, then the server's; a retry starts again.
    fn load(&mut self, ctx: &mut Ctx) {
        self.started = true;
        self.lib = Some(ctx.lib.clone());
        self.loading = true;
        self.error = None;
        self.deadline.restart(ctx.now);
        let lib = ctx.lib.clone();
        ctx.task(0, move |reply| async move {
            let cached = super::sources::all_albums(lib.clone(), Policy::CacheOnly).await;
            let have = cached.is_ok();
            if have {
                reply.send(cached);
            }
            // With a cached list, refresh at most daily (PreferCache) instead of
            // re-reading every album page on each open (COVERFLOW-001).
            if !have {
                reply.send(super::sources::all_albums(lib, Policy::NetworkFirst).await);
            } else if lib.is_online() {
                // The cached covers are on screen: refreshing the list is
                // low priority and its failures leave the server status alone.
                let refresh = super::sources::all_albums(lib, Policy::PreferCache);
                reply.send(crate::library::with_priority(crate::library::Priority::Low, refresh).await);
            }
        });
    }

    fn event_with_albums(&mut self, ev: Event, ctx: &mut Ctx) -> Action {
        let n = self.albums.len();
        match ev {
            Event::Scroll(t) => {
                self.selected = (self.selected as i32 + t).clamp(0, n as i32 - 1) as usize;
                Action::None
            }
            Event::Center => {
                let a = &self.albums[self.selected];
                Action::Push(super::open(Dest::Album { id: a.id.clone(), name: a.name.clone() }, ctx))
            }
            Event::CenterLong => {
                let a = &self.albums[self.selected];
                let row = Row::link(a.name.clone(), Dest::Album { id: a.id.clone(), name: a.name.clone() }).art(a.cover_art.clone());
                match popup::for_row(&row, ctx) {
                    Some(p) => Action::Push(p),
                    None => Action::None,
                }
            }
            _ => Action::None,
        }
    }

    fn warm(&mut self, lib: &crate::library::Library) {
        if self.warmed == Some(self.selected) { return; }
        let forward = self.warmed.is_none_or(|w| self.selected >= w);
        self.warmed = Some(self.selected);
        if std::env::var_os("Q2ND_COVERFLOW_PREFETCH").is_some_and(|v| v == "0") { return; }
        let step = if forward { 1 } else { -1 };
        let selected = self.selected as i64;
        let ids = (0..=REACH as i64 + PREFETCH_AHEAD).flat_map(|d| {
            [Some(selected + step * d), (d > 0 && d <= REACH as i64).then_some(selected - step * d)]
        }).flatten().filter_map(|i| self.albums.get(i as usize)?.cover_art.as_deref());
        lib.art.warm_window(ids, SIZE as u32);
    }

}

impl CoverFlow {
    fn cover_at(&self, x: i32, y: i32) -> Option<usize> {
        // Reverse painter order so overlap resolves to the cover on top.
        self.hit_covers.iter().rev().find_map(|&(index, cx, cy, w, hl, hr)| {
            let col = x - (cx - w / 2.0).round() as i32;
            if !(0..w.round() as i32).contains(&col) {
                return None;
            }
            let (_, _, top, rows) = column(col, cy, w, hl, hr);
            (top..top + rows).contains(&y).then_some(index)
        }).filter(|&index| index < self.albums.len())
    }
}

impl Screen for CoverFlow {
    fn clickable(&self, x: i32, y: i32) -> bool {
        #[cfg(not(target_arch = "wasm32"))]
        if self.albums.is_empty() && self.retry_target.is_some_and(|target| super::hits(target, x, y)) {
            return true;
        }
        self.cover_at(x, y).is_some()
    }

    fn click(&mut self, x: i32, y: i32, ctx: &mut Ctx) -> Action {
        #[cfg(not(target_arch = "wasm32"))]
        if self.albums.is_empty() && self.clickable(x, y) {
            return self.event(Event::Center, ctx);
        }
        let Some(index) = self.cover_at(x, y) else { return Action::None };
        if index == self.selected {
            self.event_with_albums(Event::Center, ctx)
        } else {
            self.selected = index;
            Action::Stay
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn selected_label(&self) -> Option<&str> {
        self.albums.get(self.selected).map(|album| album.name.as_str())
    }

    fn script_ready(&self) -> bool { !self.loading || self.error.is_some() }
    #[cfg(test)]
    fn audit(&self) -> crate::ui::hierarchy::Surface { crate::ui::hierarchy::Surface::widget("CoverFlow", format!("loading={};albums={};selected={}", self.loading, self.albums.len(), self.selected)) }
    fn title(&self) -> String {
        "Cover Flow".into()
    }

    fn enter(&mut self, ctx: &mut Ctx) {
        if self.started {
            return;
        }
        self.load(ctx);
    }

    fn loaded(&mut self, _token: u64, data: Box<dyn Any + Send>, _ctx: &mut Ctx) {
        if let Ok(r) = data.downcast::<Result<Vec<Album>, LibError>>() {
            self.loading = false;
            match *r {
                Ok(albums) => {
                    #[cfg(not(target_arch = "wasm32"))]
                    self.hit_covers.clear();
                    self.error = None;
                    let id = self.albums.get(self.selected).map(|a| a.id.clone());
                    self.albums = albums;
                    self.warmed = None;
                    self.visible = None;
                    self.selected = id
                        .and_then(|id| self.albums.iter().position(|a| a.id == id))
                        .unwrap_or(0)
                        .min(self.albums.len().saturating_sub(1));
                    self.shown = self.selected as f32;
                }
                // Plain words on screen, the fixed code in the report (UX-006).
                Err(e) if self.albums.is_empty() => self.error = Some(crate::ux::present_in(&e.to_string(), crate::ux::Journey::OpenPage)),
                Err(_) => {}
            }
        }
    }

    fn event(&mut self, ev: Event, ctx: &mut Ctx) -> Action {
        let n = self.albums.len();
        if n == 0 {
            if matches!(ev, Event::Center) && self.error.is_some() {
                crate::ux::begin(crate::ux::Journey::OpenPage);
                self.load(ctx);
                return Action::Stay;
            }
            return Action::None;
        }
        self.event_with_albums(ev, ctx)
    }

    /// Warm covers ahead of the wheel, nearest first and further in the
    /// direction it last moved, so scrolling meets loaded images.
    fn tick(&mut self, ctx: &mut Ctx) -> bool {
        if self.loading && self.albums.is_empty() && self.error.is_none() && self.deadline.expired(ctx.now, crate::ux::Journey::OpenPage) {
            // A late answer still replaces this.
            self.error = Some(crate::ux::Code::Stuck.message().into());
            return true;
        }
        false
    }

    fn animate(&mut self, _now: Instant) -> bool {
        let target = self.selected as f32;
        let d = target - self.shown;
        if d.abs() < 0.01 {
            self.shown = target;
            return false;
        }
        // Long jumps catch up quickly; the last cover settles gently.
        self.shown += d * 0.3;
        true
    }

    fn draw(&mut self, d: &mut Draw, area: Rect) {
        self.hit_covers.clear();
        d.c.fill_rect(area, d.theme.bg);
        if self.albums.is_empty() {
            use crate::ui::state::View;
            let view = match (&self.error, self.loading) {
                (Some(text), _) => View::Failed { text, retry: true },
                (None, true) => View::Loading,
                (None, false) => View::Empty("No albums"),
            };
            #[cfg(target_arch = "wasm32")]
            d.state(area, view);
            #[cfg(not(target_arch = "wasm32"))]
            { self.retry_target = crate::ui::state::draw_target(d, area, view); }
            return;
        }
        let centre = (WIDTH / 2) as f32;
        let cy = (area.y + TOP + SIZE / 2) as f32;
        let base = self.shown.round() as i32;
        // The farthest first, the middle one last, on top.
        let mut order = [0i32; COVERS];
        let mut count = 0;
        for i in (base - REACH..=base + REACH).filter(|i| *i >= 0 && (*i as usize) < self.albums.len()) {
            let (cx, w, ..) = placement(i as f32 - self.shown, centre);
            let left = (cx - w / 2.0).round() as i32;
            if left + w.round() as i32 <= area.x || left >= area.right() { continue; }
            order[count] = i;
            count += 1;
        }
        order[..count].sort_by(|a, b| {
            let da = (*a as f32 - self.shown).abs();
            let db = (*b as f32 - self.shown).abs();
            db.partial_cmp(&da).unwrap()
        });
        self.frames += 1;
        let ids: [Option<&str>; COVERS] = std::array::from_fn(|k| {
            (k < count).then(|| self.albums[order[k] as usize].cover_art.as_deref()).flatten()
        });
        let mut images = d.lib.art.frame_covers(ids, SIZE as u32);
        let mut bounds = (i32::MAX, i32::MIN);
        for (slot, &i) in images.iter_mut().zip(&order[..count]) {
            bounds = (bounds.0.min(i), bounds.1.max(i));
            if self.albums[i as usize].cover_art.as_deref().is_some_and(|id| !id.trim().is_empty()) {
                if self.visible.is_some_and(|(left,right)| i < left || i > right) {
                    self.entries += 1;
                    self.ready_entries += u32::from(slot.is_some());
                }
                if i as usize == self.selected && slot.is_none() { self.missing += 1; }
            }
            if slot.is_none() { *slot = Some(d.placeholder(SIZE as u32)); }
        }
        self.visible = Some(bounds);
        self.warm(d.lib);
        let shown = self.shown;
        let covers: [Turned; COVERS] = std::array::from_fn(|k| {
            let dist = order[k] as f32 - shown;
            let (cx, w, hl, hr) = placement(dist, centre);
            // The turned covers a shade darker, as the light falls off.
            let shade = (dist.abs().min(1.0) * 50.0 + (dist.abs() - 1.0).max(0.0) * 12.0).min(120.0) as u32;
            Turned { img: images[k].as_deref().unwrap_or(&EMPTY), cx, w, hl, hr, shade }
        });
        self.hit_covers.extend(order[..count].iter().zip(&covers).map(|(&index, cover)| {
            (index as usize, cover.cx, cy, cover.w, cover.hl, cover.hr)
        }));
        draw_covers(d.c, &covers[..count], cy);
        // The middle album's name and artist.
        if let Some(a) = self.albums.get(self.selected) {
            let y = area.bottom() - 65;
            let name = d.fonts.ellipsize(&a.name, 19, Weight::Bold, WIDTH - 48);
            d.fonts.draw_centered(d.c, &name, WIDTH / 2, y, 19, Weight::Bold, d.theme.text);
            let artist = d.fonts.ellipsize(&a.artist, 14, Weight::Regular, WIDTH - 64);
            d.fonts.draw_centered(d.c, &artist, WIDTH / 2, y + 24, 14, Weight::Regular, if d.theme.dark { Color(0xB8B8B8) } else { Color(0x525252) });
        }
    }
}

pub(crate) fn benchmark_frame(c: &mut Canvas, img: &Image, phase: f32) {
    benchmark_part(c, std::slice::from_ref(img), phase, -REACH, REACH);
}

/// The covers `first..=last` places from the middle at `phase` over black,
/// cover `i` with image `i` of `images` (in rotation).
pub(crate) fn benchmark_part(c: &mut Canvas, images: &[Image], phase: f32, first: i32, last: i32) {
    c.clear(Color::BLACK);
    let (covers, count) = benchmark_covers(images, phase, first, last);
    draw_covers(c, &covers[..count], (30 + TOP + SIZE / 2) as f32);
}

/// The benchmark's covers at `phase`, the farthest first, and their count.
fn benchmark_covers(images: &[Image], phase: f32, first: i32, last: i32) -> ([Turned<'_>; COVERS], usize) {
    let mut order = [0i32; COVERS];
    let mut count = 0;
    for i in first.max(-REACH)..=last.min(REACH) {
        order[count] = i;
        count += 1;
    }
    order[..count].sort_by(|a,b| ((*b as f32-phase).abs()).partial_cmp(&(*a as f32-phase).abs()).unwrap());
    (order.map(|i| {
        let dist = i as f32-phase;
        let (cx,w,hl,hr) = placement(dist,(WIDTH/2) as f32);
        let shade = (dist.abs().min(1.0)*50.0+(dist.abs()-1.0).max(0.0)*12.0).min(120.0) as u32;
        Turned { img: &images[(i + REACH) as usize % images.len()], cx, w, hl, hr, shade }
    }), count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(count: usize, seed: u32) -> Vec<u32> {
        let mut value = seed;
        (0..count).map(|_| { value = value.wrapping_mul(1664525).wrapping_add(1013904223); value }).collect()
    }

    #[test]
    fn fade_table_equals_the_float_expression() {
        for refl in 1..=REFLECTION {
            for row in 0..refl {
                let expected = fade(std::hint::black_box(row), std::hint::black_box(refl));
                assert_eq!(FADE[refl as usize][row as usize] as u32, expected, "{refl} {row}");
            }
        }
    }

    #[test]
    fn fixed_point_source_rows_equal_the_float_expressions() {
        // Every column height a cover takes in steps of 1/64, and heights
        // that put rows on whole source rows.
        let mut fallbacks = 0u32;
        for sh in [1u32, 37, 52, 150, 151, 1024] {
            for k in 32..(150 * 64 + 64) {
                let h = k as f32 / 64.0 + if k % 3 == 0 { 0.0031 } else { 0.0 };
                let rows = h.round() as i32;
                let step = (sh as f64 / h as f64 * (1u32 << (FRACTION - 1)) as f64).round() as u32;
                for row in 0..rows {
                    let exact = ((row as f32 + 0.5) / h * sh as f32) as u32;
                    assert_eq!(source_row(row, step, h, sh as f32), exact, "{sh} {h} {row}");
                    let mirrored = (sh as f32 - 1.0 - (row as f32 + 0.5) / h * sh as f32).max(0.0) as u32;
                    assert_eq!(mirrored_row(row, step, h, sh as f32, sh), mirrored, "{sh} {h} {row}");
                    let v = (2 * row as u32 + 1).wrapping_mul(step);
                    if (v.wrapping_add(MARGIN) & ((1 << FRACTION) - 1)) < 2 * MARGIN { fallbacks += 1; }
                }
            }
        }
        assert!(fallbacks > 1000, "the float path is exercised");
    }

    #[test]
    fn turned_covers_equal_the_per_pixel_reference() {
        let images = [
            Image { width: 150, height: 150, pixels: noise(150 * 150, 1) },
            Image::placeholder(150),
            Image { width: 52, height: 52, pixels: noise(52 * 52, 2) },
            Image { width: 37, height: 91, pixels: noise(37 * 91, 3) },
            Image { width: 1, height: 1, pixels: vec![0x80123456] },
        ];
        let mut a = Canvas::new(WIDTH, crate::gfx::HEIGHT);
        let mut b = Canvas::new(WIDTH, crate::gfx::HEIGHT);
        for img in &images {
            for step in 0..=80 {
                let phase = step as f32 / 40.0 - 1.0;
                benchmark_frame(&mut a, img, phase);
                b.clear(Color::BLACK);
                for t in benchmark_covers(std::slice::from_ref(img), phase, -REACH, REACH).0 { reference_draw_turned(&mut b, t.img, t.cx, (30 + TOP + SIZE / 2) as f32, t.w, t.hl, t.hr, t.shade); }
                assert!(a.pixels == b.pixels, "{}x{} phase {phase}", img.width, img.height);
            }
        }
        // Clipped, off-screen, upside-down and degenerate quadrilaterals over noise.
        let under = noise((WIDTH * crate::gfx::HEIGHT) as usize, 9);
        let mut seed = 77u32;
        let mut next = |range: f32| { seed = seed.wrapping_mul(1664525).wrapping_add(1013904223); (seed >> 8) as f32 / 16777216.0 * range };
        for case in 0..400 {
            let img = &images[case % images.len()];
            let clip = match case % 4 {
                0 => Rect::new(0, 0, WIDTH, crate::gfx::HEIGHT),
                1 => Rect::new(40, 50, 200, 150),
                2 => Rect::new(next(300.0) as i32, next(250.0) as i32, next(120.0) as i32, next(120.0) as i32),
                _ => Rect::new(0, 30, WIDTH, 260),
            };
            let (cx, cy, w) = (next(500.0) - 60.0, next(400.0) - 40.0, next(420.0));
            let (hl, hr) = (next(200.0), if case % 5 == 0 { next(1.5) } else { next(200.0) });
            let shade = match case % 6 { 0 => 0, 1 => 255, 2 => 400, _ => next(130.0) as u32 };
            for c in [&mut a, &mut b] { c.pixels.copy_from_slice(&under); c.set_clip(clip); }
            draw_turned(&mut a, img, cx, cy, w, hl, hr, shade, None);
            reference_draw_turned(&mut b, img, cx, cy, w, hl, hr, shade);
            assert!(a.pixels == b.pixels, "case {case}: {cx} {cy} {w} {hl} {hr} {shade} {clip:?}");
        }
        // Up to thirteen arbitrary covers on one centre line: culling what
        // later covers overwrite leaves the same pixels.
        for case in 0..300 {
            let clip = if case % 3 == 0 { Rect::new(30, 20, 300, 250) } else { Rect::new(0, 0, WIDTH, crate::gfx::HEIGHT) };
            for c in [&mut a, &mut b] { c.pixels.copy_from_slice(&under); c.set_clip(clip); }
            let cy = 60.0 + next(200.0);
            let covers: Vec<Turned> = (0..1 + case % 13).map(|k| Turned {
                img: &images[(case + k) % images.len()], cx: next(420.0) - 20.0, w: next(180.0),
                hl: if (case + k) % 7 == 0 { next(3.0) } else { next(190.0) }, hr: next(190.0),
                shade: if k % 4 == 0 { 0 } else { next(125.0) as u32 },
            }).collect();
            draw_covers(&mut a, &covers, cy);
            for t in &covers { reference_draw_turned(&mut b, t.img, t.cx, cy, t.w, t.hl, t.hr, t.shade); }
            assert!(a.pixels == b.pixels, "covers case {case}");
        }
    }

    #[test]
    fn placements() {
        let (x, w, hl, hr) = placement(0.0, 187.0);
        assert_eq!((x, w, hl, hr), (187.0, SIZE as f32, SIZE as f32, SIZE as f32));
        let (x, w, hl, hr) = placement(-1.0, 187.0);
        assert_eq!(x, 187.0 - FIRST);
        assert_eq!(w, SIDE_W);
        assert!(hl > hr, "the left cover recedes toward the middle");
        let (x2, ..) = placement(-2.0, 187.0);
        assert_eq!(x2, 187.0 - FIRST - STEP);
        let (_, _, l, r) = placement(1.0, 187.0);
        assert_eq!((l, r), (hr, hl), "the side covers mirror each other");
    }
}
