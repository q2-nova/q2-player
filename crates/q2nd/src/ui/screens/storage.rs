//! Filesystem usage, with actual allocated media bytes and wheel-selected wedges.
mod downloads;
mod folders;
use folders::FileEntry;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use crate::gfx::{Color, Rect, Weight};
use crate::input::Event;
use crate::ui::{Action, Ctx, Draw, Screen};
use std::any::Any;
use std::path::Path;


#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Slice {
    pub(crate) label: &'static str,
    pub(crate) bytes: u64,
    color: Color,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Usage {
    total: u64,
    slices: Vec<Slice>,
}
impl Usage {
    /// `free` includes reserved blocks; `available` is usable by this app.
    /// Allocation accounting must fit the filesystem, rather than silently
    /// drawing overlapping categories or inventing a negative Other wedge.
    fn from_counts(total: u64, free: u64, available: u64, cache: u64, downloads: u64) -> Result<Self, String> {
        let used = total.checked_sub(free).ok_or("Storage totals unavailable")?;
        let reserved = free.checked_sub(available).ok_or("Storage totals unavailable")?;
        let other = used.checked_sub(downloads).and_then(|n| n.checked_sub(cache))
            .ok_or("Storage changed while reading")?;
        let slices = [
            Slice { label: "Downloads", bytes: downloads, color: Color(0x32A178) },
            Slice { label: "Song Cache", bytes: cache, color: Color(0x387BD4) },
            Slice { label: "Other Files", bytes: other, color: Color(0xDB973A) },
            Slice { label: "Reserved Space", bytes: reserved, color: Color(0x9B89BE) },
            Slice { label: "Free", bytes: available, color: Color(0xA6ADB7) },
        ].into_iter().filter(|slice| slice.bytes > 0).collect();
        Ok(Self { total, slices })
    }
    #[cfg(test)]
    fn read(lib: &crate::library::Library, root: &Path) -> Result<Self, String> {
        Self::from_allocations(&crate::rt::Blocking::dedicated_thread(),root, lib.storage_allocations()?)
    }
    async fn read_async(lib: std::sync::Arc<crate::library::Library>, root: std::path::PathBuf) -> Result<Self, String> {
        let allocations = lib.storage_allocations_async().await?;
        crate::rt::jobs().run_isolated(move |blocking| Self::from_allocations(blocking,&root, allocations))
            .await.map_err(|_| "Storage worker stopped".to_string())?
    }
    fn from_allocations(blocking: &crate::rt::Blocking,root: &Path, (cache, downloads): (u64, u64)) -> Result<Self, String> {
        let stat=crate::platform::system::filesystem(blocking,root).map_err(|_|"Storage unavailable")?;
        let bytes = |blocks: u64| blocks.checked_mul(stat.f_frsize as u64).ok_or("Storage totals unavailable");
        Self::from_counts(bytes(stat.f_blocks as u64)?, bytes(stat.f_bfree as u64)?, bytes(stat.f_bavail as u64)?, cache, downloads)
    }
}

pub struct Storage {
    usage: Option<Result<Usage, String>>,
    selected: usize,
    token: u64,
    generation: u32,
    temporary: bool,
    /// A download event asked for a read not yet made, and when the last
    /// read started: download progress re-reads the totals only while this
    /// screen is on top and lit, at most once a second.
    stale: bool,
    read_at: Option<std::time::Instant>,
}
/// Download-driven reads are at least this far apart.
const DOWNLOAD_READ: std::time::Duration = std::time::Duration::from_secs(1);
impl Storage {
    pub fn new() -> Self { Self { usage: None, selected: 0, token: 0, generation: 0, temporary: false, stale: false, read_at: None } }
    fn read(&mut self, ctx: &Ctx) {
        self.stale = false;
        self.read_at = Some(ctx.now);
        self.token = self.token.wrapping_add(1);
        self.generation = ctx.lib.client_generation();
        let lib = ctx.lib.clone();
        let root = ctx.paths.media_root().to_path_buf();
        ctx.task(self.token, move |reply| async move { reply.send(Usage::read_async(lib, root).await); });
    }
    fn wheel(&mut self, ticks: i32) {
        let Some(Ok(usage)) = &self.usage else { return; };
        if usage.slices.is_empty() { return; }
        self.selected = (self.selected as i64 + i64::from(ticks)).rem_euclid(usage.slices.len() as i64) as usize;
    }
}
impl Screen for Storage {
    fn script_ready(&self) -> bool { self.usage.is_some() }
    #[cfg(test)]
    fn audit(&self) -> crate::ui::hierarchy::Surface {
        let state = match &self.usage {
            None => "loading".into(),
            Some(Err(_)) => "unavailable".into(),
            Some(Ok(usage)) => format!("wedges={};selected={}", usage.slices.iter().map(|s| s.label).collect::<Vec<_>>().join(","), self.selected),
        };
        crate::ui::hierarchy::Surface::widget("Storage", state)
    }
    fn title(&self) -> String { "Storage Usage".into() }
    fn enter(&mut self, ctx: &mut Ctx) { self.temporary = ctx.paths.ephemeral_media; self.read(ctx); }
    fn event(&mut self, event: Event, ctx: &mut Ctx) -> Action {
        match event {
            Event::Scroll(ticks) => { self.wheel(ticks); Action::Stay },
            Event::Center if matches!(self.usage, Some(Err(_))) => { self.usage = None; self.enter(ctx); Action::Stay },
            Event::Center => {
                let Some(Ok(usage)) = &self.usage else { return Action::None; };
                let Some(slice) = usage.slices.get(self.selected) else { return Action::None; };
                match slice.label {
                    "Downloads" => Action::Push(super::menu::Menu::boxed(downloads::DownloadedItems)),
                    "Song Cache" => Action::Push(super::menu::Menu::boxed(CachedSongs)),
                    "Other Files" => Action::Push(Box::new(Files::new("Media Folders", ctx.paths.media_root().to_path_buf()))),
                    "Reserved Space" => Action::Toast("Kept back by the card's filesystem".into()),
                    "Free" => Action::Toast("Available for downloads and streamed-song cache".into()),
                    _ => Action::None,
                }
            },
            _ => Action::None,
        }
    }
    fn loaded(&mut self, token: u64, data: Box<dyn Any + Send>, ctx: &mut Ctx) {
        if token != self.token { return; }
        let Ok(usage) = data.downcast::<Result<Usage, String>>() else { return; };
        let usage = if self.generation == ctx.lib.client_generation() { *usage }
            else { Err("Storage changed while reading".into()) };
        // Plain words on screen, the fixed code in the report (UX-006).
        let usage = usage.map_err(|error| crate::ux::present_in(&error, crate::ux::Journey::OpenPage));
        let selected_label = self.usage.as_ref().and_then(|result| result.as_ref().ok())
            .and_then(|usage| usage.slices.get(self.selected)).map(|slice| slice.label);
        self.selected = usage.as_ref().ok().and_then(|usage| selected_label.and_then(|label| usage.slices.iter().position(|s| s.label == label))).unwrap_or(0);
        self.usage = Some(usage);
    }
    fn refresh(&mut self, event: &crate::library::LibEvent, _ctx: &mut Ctx) {
        if matches!(event, crate::library::LibEvent::Downloads | crate::library::LibEvent::SpokenChanged) { self.stale = true; }
    }
    fn tick(&mut self, ctx: &mut Ctx) -> bool {
        if self.stale && ctx.screen_on && self.read_at.is_none_or(|at| ctx.now.saturating_duration_since(at) >= DOWNLOAD_READ) {
            self.read(ctx);
        }
        false
    }
    fn draw(&mut self, d: &mut Draw, area: Rect) {
        let usage = match &self.usage {
            None => { d.state(area, crate::ui::state::View::Loading); return; },
            Some(Err(error)) => { d.state(area, crate::ui::state::View::Failed { text: error, retry: true }); return; },
            Some(Ok(usage)) => usage,
        };
        if usage.slices.is_empty() { d.state(area, crate::ui::state::View::Empty("No storage found")); return; }
        let cx = area.x + area.w / 2;
        let cy = area.y + 119;
        draw_pie(d.c, usage, self.selected, cx, cy, d.theme.text, fill_slice);
        let selected = &usage.slices[self.selected];
        d.fonts.draw_centered(d.c, if self.temporary { "Temporary Storage" } else { "Card Storage" }, cx, area.y + 2, 15, Weight::Bold, d.theme.muted);
        d.fonts.draw_centered(d.c, selected.label, cx, area.y + 220, 21, Weight::Bold, d.theme.text);
        let percent = selected.bytes as f64 / usage.total as f64 * 100.0;
        let percent = if percent < 0.1 { "<0.1%".into() } else { format!("{percent:.1}%") };
        d.fonts.draw_centered(d.c, &format!("{} · {}", bytes(selected.bytes), percent), cx, area.y + 246, 16, Weight::Regular, d.theme.text);
        let hint = if matches!(selected.label, "Reserved Space" | "Free") { "Wheel: select · Center: about" } else { "Wheel: select · Center: open" };
        d.fonts.draw_centered(d.c, hint, cx, area.bottom() - 22, 14, Weight::Regular, d.theme.muted);
    }
}

/// A read-only browser of the media volume. Never follows links or exposes
/// configuration paths outside the media root. Directory reads run on a worker;
/// each page is bounded and explicitly reports truncation.
struct Files {
    title: String,
    path: std::path::PathBuf,
    cancelled: Arc<AtomicBool>,
    deadline: crate::ui::state::Deadline,
    token: u64,
    entries: Vec<FileEntry>,
    list: crate::ui::list::ListView,
    ready: bool,
    error: Option<String>,
}
impl Files {
    fn new(title: impl Into<String>, path: std::path::PathBuf) -> Self {
        Self { title: title.into(), path, cancelled: Arc::new(AtomicBool::new(false)), token: 0,
            entries: Vec::new(), list: crate::ui::list::ListView::settings(false),
            ready: false, error: None, deadline: crate::ui::state::Deadline::new(std::time::Instant::now()) }
    }
    fn start_scan(&mut self, ctx: &Ctx) {
        self.cancelled.store(true, Ordering::Relaxed);
        self.cancelled = Arc::new(AtomicBool::new(false));
        let cancelled = self.cancelled.clone();
        let paths = ctx.paths.clone();
        let path = self.path.clone();
        self.token = self.token.wrapping_add(1);
        self.ready = false;
        self.error = None;
        self.deadline.restart(ctx.now);
        ctx.task(self.token, move |reply| async move {
            let result = crate::rt::jobs().run_isolated(move |blocking| folders::read(blocking, &paths, &path, &cancelled)).await
                .unwrap_or_else(|_| Err("Storage worker stopped".into()));
            reply.send(result);
        });
    }
}
impl Drop for Files {
    fn drop(&mut self) { self.cancelled.store(true, Ordering::Relaxed); }
}
impl Screen for Files {

    fn clickable(&self, x: i32, y: i32) -> bool {
        self.list.retry_hit(x, y) || self.list.hit(x, y).is_some()
    }

    fn click(&mut self, x: i32, y: i32, ctx: &mut Ctx) -> Action {
        if self.list.retry_hit(x, y) || self.list.select_at(x, y) {
            self.event(Event::Center, ctx)
        } else {
            Action::None
        }
    }

    fn title(&self) -> String { self.title.clone() }
    fn script_ready(&self) -> bool { self.ready }
    #[cfg(test)]
    fn audit(&self) -> crate::ui::hierarchy::Surface { crate::ui::hierarchy::Surface::list(&self.list) }
    fn enter(&mut self, ctx: &mut Ctx) {
        if !self.ready { self.start_scan(ctx); }
    }
    fn loaded(&mut self, token: u64, data: Box<dyn Any + Send>, _ctx: &mut Ctx) {
        if token != self.token { return; }
        let Ok(result) = data.downcast::<Result<(Vec<FileEntry>, bool), String>>() else { return; };
        self.ready = true;
        match *result {
            Err(error) => self.error = Some(crate::ux::present_in(&error, crate::ux::Journey::OpenPage)),
            Ok((entries, truncated)) => {
                self.error = None;
                self.entries = entries;
                let mut rows = self.entries.iter().enumerate().map(|(i, entry)| {
                    crate::ui::list::Row::new(&entry.name, crate::ui::list::RowAction::Cmd(i as u32))
                        .detail(if entry.directory { format!("{} · Folder", bytes(entry.allocated)) } else { format!("{} allocated", bytes(entry.allocated)) })
                }).collect::<Vec<_>>();
                if self.title == "Media Folders" {
                    rows.push(crate::ui::list::Row::new("App media folders", crate::ui::list::RowAction::None)
                        .detail("Other Files also counts the rest of the card"));
                }
                if truncated { rows.push(crate::ui::list::Row::new("Largest 256 items shown", crate::ui::list::RowAction::None)); }
                self.list.set_rows(rows);
            }
        }
    }
    fn event(&mut self, event: Event, ctx: &mut Ctx) -> Action {
        match event {
            Event::Scroll(ticks) => { self.list.wheel(ticks, ctx.now); },
            Event::WheelEnd => self.list.end_spin(),
            Event::Center if self.error.is_some() => { self.start_scan(ctx); return Action::Stay; },
            Event::Center => {
                if let Some(crate::ui::list::RowAction::Cmd(i)) = self.list.selected_row().map(|r| &r.action) {
                    if let Some(entry) = self.entries.get(*i as usize) {
                        return if entry.directory { Action::Push(Box::new(Self::new(&entry.name, entry.path.clone()))) }
                            else { Action::Toast(format!("{} allocated on card", bytes(entry.allocated))) };
                    }
                }
            },
            _ => {},
        }
        Action::None
    }
    fn tick(&mut self, ctx: &mut Ctx) -> bool {
        if !self.ready && self.deadline.expired(ctx.now, crate::ux::Journey::OpenPage) {
            self.error = Some(crate::ux::Code::Stuck.message().into());
            self.ready = true;
            return true;
        }
        false
    }
    fn draw(&mut self, d: &mut Draw, area: Rect) {
        if !self.ready { self.list.draw_state(d, area, crate::ui::state::View::Loading); }
        else if let Some(error) = &self.error { self.list.draw_state(d, area, crate::ui::state::View::Failed { text: error, retry: true }); }
        else if self.entries.is_empty() { self.list.draw_state(d, area, crate::ui::state::View::Empty("No files in this folder")); }
        else { self.list.draw(d, area); }
    }
    fn animate(&mut self, now: std::time::Instant) -> bool { self.list.animate(now) }
}

/// Intersect edges once per 4× subscanline. Keep the original large-polygon
/// geometry and exact 4×4 coverage, without using the per-pixel icon filler.
fn fill_slice(canvas: &mut crate::gfx::Canvas, points: &[(f32, f32)], color: Color) {
    if points.len() < 3 { return; }
    let clip = canvas.clip();
    let min_x = (points.iter().map(|p| p.0).fold(f32::MAX, f32::min).floor() as i32).max(clip.x);
    let max_x = (points.iter().map(|p| p.0).fold(f32::MIN, f32::max).ceil() as i32).min(clip.right());
    let min_y = (points.iter().map(|p| p.1).fold(f32::MAX, f32::min).floor() as i32).max(clip.y);
    let max_y = (points.iter().map(|p| p.1).fold(f32::MIN, f32::max).ceil() as i32).min(clip.bottom());
    if min_x >= max_x || min_y >= max_y { return; }
    let mut hits = vec![0u8; (max_x - min_x) as usize];
    let mut intersections = Vec::with_capacity(points.len());
    for y in min_y..max_y {
        hits.fill(0);
        for sub_y in 0..4 {
            let sample_y = y as f32 + (sub_y as f32 + 0.5) / 4.0;
            intersections.clear();
            let mut previous = points.len() - 1;
            for i in 0..points.len() {
                let (xi, yi) = points[i];
                let (xj, yj) = points[previous];
                if (yi > sample_y) != (yj > sample_y) {
                    // Same projection and even-odd rule as point_in_polygon.
                    intersections.push((xj - xi) * (sample_y - yi) / (yj - yi) + xi);
                }
                previous = i;
            }
            intersections.sort_unstable_by(f32::total_cmp);
            for span in intersections.chunks_exact(2) {
                // The old samples are (integer + 0.5)/4. Include the left
                // boundary, exclude the right, exactly as x < intersection.
                let first = ((span[0] * 4.0 - 0.5).ceil() as i32).max(min_x * 4);
                let end = ((span[1] * 4.0 - 0.5).ceil() as i32).min(max_x * 4);
                for sample in first..end { hits[(sample.div_euclid(4) - min_x) as usize] += 1; }
            }
        }
        for (x, hits) in hits.iter().enumerate() {
            if *hits > 0 { canvas.blend(min_x + x as i32, y, color, u32::from(*hits) * 255 / 16); }
        }
    }
}

fn draw_pie(canvas: &mut crate::gfx::Canvas, usage: &Usage, selected_index: usize,
            cx: i32, cy: i32, marker: Color,
            rasterize: fn(&mut crate::gfx::Canvas, &[(f32, f32)], Color)) {
    let radius = 80.0f32;
    let mut start = -std::f64::consts::FRAC_PI_2;
    for (i, slice) in usage.slices.iter().enumerate() {
        let angle = slice.bytes as f64 / usage.total as f64 * std::f64::consts::TAU;
        let middle = start + angle / 2.0;
        let selected = i == selected_index;
        let offset = if selected && usage.slices.len() > 1 { 8.0 } else { 0.0 };
        let x = cx as f32 + middle.cos() as f32 * offset;
        let y = cy as f32 + middle.sin() as f32 * offset;
        let steps = (angle * radius as f64).ceil().max(1.0) as usize;
        let mut points = Vec::with_capacity(steps + 2);
        points.push((x, y));
        for step in 0..=steps {
            let a = start + angle * step as f64 / steps as f64;
            points.push((x + a.cos() as f32 * radius, y + a.sin() as f32 * radius));
        }
        rasterize(canvas, &points, slice.color);
        if selected {
            // The marker keeps even a subpixel wedge discoverable without
            // inflating its area and falsifying the chart.
            let mx = cx as f32 + middle.cos() as f32 * (radius + 12.0);
            let my = cy as f32 + middle.sin() as f32 * (radius + 12.0);
            canvas.fill_circle(mx, my, 4.0, 0.0, marker);
        }
        start += angle;
    }
}

/// The Song Cache slice opened: the streamed songs it holds, by title, largest
/// first, each playable from here (UXI-013). Downloads open their own page.
struct CachedSongs;
impl super::menu::Source for CachedSongs {
    fn title(&self) -> String { "Song Cache".into() }
    fn media(&self) -> bool { true }
    fn local(&self) -> bool { true }
    fn immediate(&self) -> bool { false }
    fn download_badges(&self) -> bool { false }
    fn empty(&self) -> &'static str { "No streamed songs on the card\nSongs you stream are kept here" }
    fn load(self: std::sync::Arc<Self>, lib: std::sync::Arc<crate::library::Library>, _: crate::library::Policy) -> super::menu::RowsTask {
        use crate::ui::list::{Row, RowAction};
        Box::pin(async move {
            let songs = lib.cached_music_async().await?;
            let queue: std::sync::Arc<Vec<subsonic::Song>> = std::sync::Arc::new(songs.iter().map(|(song, _)| song.clone()).collect());
            Ok(songs.into_iter().enumerate().map(|(index, (song, size))| {
                Row::new(song.title.clone(), RowAction::Play { songs: queue.clone(), index })
                    .key(song.id.clone()).detail(format!("{} · {}", bytes(size), song.artist))
                    .art(song.cover_art.clone()).song(song)
            }).collect())
        })
    }
    fn menu(&self, row: &crate::ui::list::Row, ctx: &mut Ctx) -> Option<Box<dyn Screen>> {
        let song = row.song_item()?;
        let id = song.id.clone();
        let account = ctx.lib.music_account_generation();
        let popup = super::popup::Popup::new(song.title.clone()).add("Delete from Cache", move |_| {
            let id = id.clone();
            Action::Push(Box::new(super::controls::Confirm::new(
                "Delete Cached Song?", "Remove this song from the cache?\nDownloads are kept.",
                "Delete from Cache", move |ctx| {
                    let lib = ctx.lib.clone();
                    let id = id.clone();
                    ctx.notice_task(async move {
                        match lib.delete_cached_song_async(account, id).await {
                            Ok(()) => "Removed from cache".into(),
                            Err(error) => error,
                        }
                    });
                    Action::Many(vec![Action::Pop, Action::Toast("Removing from cache…".into())])
                }
            )))
        });
        Some(Box::new(super::popup::song_menu_from(song.clone(), ctx, popup)))
    }
    fn reload_on(&self, event: &crate::library::LibEvent) -> bool { matches!(event, crate::library::LibEvent::Downloads) }
}

fn bytes(value: u64) -> String {
    if value >= 1 << 40 { format!("{:.1} TB", value as f64 / (1u64 << 40) as f64) }
    else if value >= 1 << 30 { format!("{:.1} GB", value as f64 / (1u64 << 30) as f64) }
    else if value >= 1 << 20 { format!("{} MB", value >> 20) }
    else if value >= 1 << 10 { format!("{} KB", value >> 10) }
    else { format!("{value} B") }
}

/// The same old/new pie paths can be measured on a generated guest or physical
/// Q2 without opening the installed configuration, library, audio or framebuffer.
pub(crate) fn benchmark(iterations: usize, mode: &str) -> Result<serde_json::Value, String> {
    use crate::gfx::Canvas;
    use md5::{Digest, Md5};
    if !(1..=10).contains(&iterations) { return Err("storage render iterations must be 1–10".into()); }
    let rasterize = match mode { "legacy" => Canvas::fill_polygon, "scanline" => fill_slice, _ => return Err("storage render scene must be legacy or scanline".into()) };
    let usage = Usage::from_counts(200, 50, 40, 30, 60)?;
    let mut draws = Vec::new(); let mut hashes = Vec::new();
    for selected in 0..iterations {
        let mut canvas = Canvas::new(375, 320); canvas.clear(Color::WHITE);
        let started = std::time::Instant::now();
        draw_pie(&mut canvas, &usage, selected % usage.slices.len(), 187, 141, Color::BLACK, rasterize);
        draws.push(started.elapsed().as_micros() as u64);
        let mut hash = Md5::new();
        for pixel in &canvas.pixels { hash.update(pixel.to_le_bytes()); }
        hashes.push(format!("{:x}", hash.finalize()));
    }
    Ok(serde_json::json!({"rasterizer":mode,"iterations":iterations,"draw_us":draws,"pixel_md5":hashes,
        "method":"actual shared generated pie geometry; no user config/library/audio/display access; CPU/wall counters are reported by the benchmark runner"}))
}

#[cfg(test)]
pub(crate) fn generated_usage_cases() -> Vec<(&'static str, Result<Usage, String>)> {
    vec![
        ("mixed cache/music-spoken downloads/other/reserved/free", Usage::from_counts(200, 50, 40, 30, 60)),
        ("empty filesystem", Usage::from_counts(200, 200, 200, 0, 0)),
        ("full filesystem", Usage::from_counts(200, 0, 0, 0, 0)),
        ("zero capacity", Usage::from_counts(0, 0, 0, 0, 0)),
        ("missing storage", Err("Storage unavailable".into())),
        ("inconsistent concurrent counts", Usage::from_counts(200, 50, 40, 100, 100)),
    ]
}

#[cfg(test)]
mod tests {
    /// Budget: one minute of download progress (two events a second)
    /// re-reads the totals at most once a second on screen, and not at all
    /// with the screen off (V0.128 read on every event).
    #[test]
    fn download_progress_storage_read_budget_per_minute() {
        use std::time::Duration;
        let minute = |screen_on: bool| {
            let mut app = crate::ui::settings_tests::fixture();
            let screen = app.top_id();
            let mut storage = super::Storage::new();
            let start = std::time::Instant::now();
            let reads = app.with_screen(screen, |_, ctx| {
                ctx.now = start;
                crate::ui::Screen::enter(&mut storage, ctx);
                let first = storage.token;
                ctx.screen_on = screen_on;
                for quarter in 1..=240u64 {
                    ctx.now = start + Duration::from_millis(quarter * 250);
                    if quarter % 2 == 0 { crate::ui::Screen::refresh(&mut storage, &crate::library::LibEvent::Downloads, ctx); }
                    crate::ui::Screen::tick(&mut storage, ctx);
                }
                storage.token - first
            }).unwrap();
            app.player.shutdown();
            reads
        };
        let (lit, dark) = (minute(true), minute(false));
        eprintln!("download-progress storage reads per minute: on screen {lit}, screen off {dark}");
        assert!((55..=60).contains(&lit), "on screen: {lit}");
        assert_eq!(dark, 0, "screen off");
    }

    use super::*;

    #[test]
    fn media_browser_is_bounded_read_only_and_skips_links() {
        use std::os::unix::fs::{symlink, MetadataExt};
        let paths = crate::config::Paths::new(crate::library::generated_test_directory("storage-inspect-folders"));
        let root = paths.media_root();
        let nested = root.join("nested");
        let art = paths.art_cache();
        let cover = art.join("0a").join("generated.png");
        std::fs::create_dir_all(cover.parent().unwrap()).unwrap();
        std::fs::write(&cover, [17; 4097]).unwrap();
        for excluded in [paths.offline(), paths.stream_cache(), paths.spoken_downloads(), paths.store_root(crate::shard::Store::OfflineArt)] {
            std::fs::create_dir_all(&excluded).unwrap();
            std::fs::write(excluded.join("large"), vec![1; 65536]).unwrap();
        }
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("audio"), [1u8; 4097]).unwrap();
        std::fs::File::create(root.join("sparse")).unwrap().set_len(1 << 30).unwrap();
        symlink("/", root.join("outside")).unwrap();
        let allocated = |path: &Path| std::fs::metadata(path).unwrap().blocks() * 512;
        let read = |path: &Path| folders::read(&crate::rt::Blocking::dedicated_thread(), &paths, path, &AtomicBool::new(false)).unwrap();
        let (entries, truncated) = read(root);
        assert!(!truncated);
        assert!(entries.windows(2).all(|pair| pair[0].allocated >= pair[1].allocated));
        assert_eq!(entries.len(), 3);
        assert_eq!(entries.iter().find(|e| e.name == "nested").unwrap().allocated, allocated(&nested) + allocated(&nested.join("audio")));
        assert_eq!(entries.iter().find(|e| e.name == "cache").unwrap().allocated,
            [&root.join("cache"), &art, &art.join("0a"), &cover].into_iter().map(|p| allocated(p)).sum::<u64>());
        assert_eq!(read(&root.join("cache")).0[0].name, "art");
        assert_eq!(read(&art).0[0].name, "generated.png");
        assert_eq!(entries.iter().find(|e| e.name == "sparse").unwrap().allocated, 0);
        let largest = entries[0].name.clone();
        let mut app = crate::ui::settings_tests::fixture();
        app.background = false;
        app.push(Box::new(Files::new("Media Folders", root.to_owned())));
        let screen = app.top_id();
        app.handle(crate::ui::UiMsg::Loaded { screen, token: 1,
            data: Box::new(Ok::<_, String>((entries, false))) }, &mut crate::platform::Headless { last: None });
        app.event(Event::Center, &mut crate::platform::Headless { last: None });
        assert_eq!(app.stack.last().unwrap().1.title(), largest, "Center opens the largest folder immediately");
        app.player.shutdown();
        for i in 0..300 { std::fs::write(nested.join(format!("file-{i}")), []).unwrap(); }
        std::fs::write(nested.join("last-large"), [2u8; 16384]).unwrap();
        let (entries, truncated) = read(&nested);
        assert!(truncated);
        assert_eq!(entries.len(), 256);
        assert_eq!(entries[0].name, "last-large");
        assert_eq!(entries[1].name, "audio");
        assert!(folders::read(&crate::rt::Blocking::dedicated_thread(), &paths, &root.join("outside"), &AtomicBool::new(false)).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn scanline_pie_preserves_legacy_pixels_for_every_selected_wedge_and_boundaries() {
        use crate::gfx::Canvas;
        let usages = [Usage::from_counts(200, 50, 40, 30, 60).unwrap(),
            Usage::from_counts(200, 200, 200, 0, 0).unwrap(),
            Usage::from_counts(1_000_000, 999_999, 999_998, 0, 1).unwrap()];
        for usage in usages {
            for selected in 0..usage.slices.len() {
                let mut reference = Canvas::new(375, 320);
                reference.clear(Color::WHITE);
                let mut candidate = Canvas::new(375, 320);
                candidate.clear(Color::WHITE);
                draw_pie(&mut reference, &usage, selected, 187, 141, Color::BLACK, Canvas::fill_polygon);
                draw_pie(&mut candidate, &usage, selected, 187, 141, Color::BLACK, fill_slice);
                assert_eq!(candidate.pixels, reference.pixels, "same polygon geometry, 4×4 sample positions and blend order; selected={selected}");
            }
        }
        // Negative coordinates, clipping, horizontal edges, concavity and
        // vertices exactly on sample boundaries exercise the coverage rule.
        for points in [vec![(-4.125, 4.125), (30.125, 4.125), (12.125, 28.125)],
            vec![(4.0, 4.0), (31.0, 4.0), (31.0, 20.0), (14.0, 8.0), (4.0, 31.0)]] {
            let mut old = Canvas::new(40, 40); let mut new = Canvas::new(40, 40);
            old.set_clip(Rect::new(3, 2, 30, 27)); new.set_clip(Rect::new(3, 2, 30, 27));
            old.fill_polygon(&points, Color(0x987654)); fill_slice(&mut new, &points, Color(0x987654));
            assert_eq!(old.pixels, new.pixels);
        }
    }

    #[test]
    fn actual_pie_rasterizers_have_paired_thread_cpu_ground_truth() {
        use crate::gfx::Canvas;
        fn cpu() -> u64 {
            crate::platform::system::clock_nanos(libc::CLOCK_THREAD_CPUTIME_ID).unwrap()
        }
        let usage = Usage::from_counts(200, 50, 40, 30, 60).unwrap();
        let mut old_times = Vec::new(); let mut new_times = Vec::new();
        for round in 0..3 {
            let mut old = Canvas::new(375, 320); let mut new = Canvas::new(375, 320);
            let measure = |canvas: &mut Canvas, rasterize| {
                let start = cpu();
                draw_pie(canvas, &usage, round % usage.slices.len(), 187, 141, Color::WHITE, rasterize);
                cpu() - start
            };
            if round % 2 == 0 {
                old_times.push(measure(&mut old, Canvas::fill_polygon)); new_times.push(measure(&mut new, fill_slice));
            } else {
                new_times.push(measure(&mut new, fill_slice)); old_times.push(measure(&mut old, Canvas::fill_polygon));
            }
            assert_eq!(old.pixels, new.pixels);
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build/ui");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("storage-raster-cpu.json"), serde_json::to_vec_pretty(&serde_json::json!({
            "hardware":false,"method":"paired actual pie geometry; unchanged icon filler vs pie-only scanline; CLOCK_THREAD_CPUTIME_ID nanoseconds; three alternating generated selections; host test build, not device CPU/energy",
            "reference_thread_cpu_ns":old_times,"scanline_thread_cpu_ns":new_times
        })).unwrap()).unwrap();
        old_times.sort_unstable(); new_times.sort_unstable();
        assert!(new_times[1] * 20 < old_times[1], "large chart must avoid icon-scale per-pixel edge traversal: old={old_times:?}, new={new_times:?}");
    }
    #[test]
    fn generated_accounting_partitions_capacity_without_overlap_or_phantom_zero_wedges() {
        let mut random = fastrand::Rng::with_seed(0x5132_57);
        for _ in 0..4096 {
            let total = random.u64(0..=u64::MAX / 2);
            let free = random.u64(0..=total);
            let available = random.u64(0..=free);
            let downloads = random.u64(0..=total - free);
            let cache = random.u64(0..=total - free - downloads);
            let usage = Usage::from_counts(total, free, available, cache, downloads).unwrap();
            assert!(usage.slices.iter().all(|s| s.bytes > 0));
            assert_eq!(usage.slices.iter().map(|s| s.bytes).sum::<u64>(), total);
            let mut screen = Storage { usage: Some(Ok(usage.clone())), ..Storage::new() };
            for i in 0..usage.slices.len() { assert_eq!(screen.selected, i); screen.wheel(1); }
            assert_eq!(screen.selected, 0);
            screen.wheel(i32::MIN); screen.wheel(i32::MAX); screen.wheel(1);
            assert_eq!(screen.selected, 0, "opposite turns and one detent cancel exactly");
        }
        let empty = Usage::from_counts(0, 0, 0, 0, 0).unwrap();
        assert!(empty.slices.is_empty());
        let free = Usage::from_counts(4096, 4096, 4096, 0, 0).unwrap();
        assert_eq!(free.slices.iter().map(|s| s.label).collect::<Vec<_>>(), ["Free"]);
        assert!(Usage::from_counts(10, 11, 0, 0, 0).is_err());
        assert!(Usage::from_counts(10, 5, 6, 0, 0).is_err());
        assert!(Usage::from_counts(10, 5, 5, 3, 3).is_err());
    }
    #[test]
    fn unavailable_or_empty_storage_does_not_move_or_invent_a_selection() {
        let mut screen = Storage::new();
        for usage in [None, Some(Err("Storage unavailable".into())), Some(Ok(Usage::from_counts(0, 0, 0, 0, 0).unwrap()))] {
            screen.usage = usage;
            screen.wheel(i32::MAX); screen.wheel(i32::MIN);
            assert_eq!(screen.selected, 0);
        }
    }

    #[test]
    fn cached_song_tap_plays_and_long_center_offers_cancel_first_deletion() {
        use crate::ui::list::RowAction;
        let mut app = crate::ui::settings_tests::fixture();
        app.background = false;
        let blocking = crate::rt::Blocking::dedicated_thread();
        let song = subsonic::Song { id: "generated-cache-choice".into(), title: "Generated Cached Song".into(), ..Default::default() };
        let path = app.paths.stream_cache().join("generated.raw");
        std::fs::write(&path, b"generated cache").unwrap();
        {
            let db = app.lib.db();
            db.put_cached_song(&blocking, &song.id, "raw", &path, 15).unwrap();
            db.put_cached_song_meta(&blocking, &song).unwrap();
        }
        app.push(super::super::menu::Menu::boxed(CachedSongs));
        let list = app.top_id();
        app.with_screen(list, |screen, ctx| screen.audit_settle(ctx));
        let surface = app.stack.last().unwrap().1.audit();
        assert!(matches!(&surface.rows()[0].action, RowAction::Play { songs, index: 0 } if songs[0].id == song.id));

        let step = |app: &mut crate::ui::App, event| app.event(event, &mut crate::platform::Headless { last: None });
        step(&mut app, Event::CenterLong);
        let popup = app.stack.last().unwrap().1.audit();
        let labels: Vec<_> = popup.rows().iter().map(|row| row.title.as_str()).collect();
        assert_eq!(labels[0], "Delete from Cache");
        for label in ["Play Next", "Add to Queue", "Rate"] { assert!(labels.contains(&label)); }
        step(&mut app, Event::Back);
        assert_eq!(app.top_id(), list);
        step(&mut app, Event::CenterLong);
        step(&mut app, Event::Center);
        let screen = &app.stack.last().unwrap().1;
        assert_eq!(screen.title(), "Delete Cached Song?");
        let options = screen.audit();
        assert_eq!(options.rows().iter().map(|row| row.title.as_str()).collect::<Vec<_>>(), ["Cancel", "Delete from Cache"]);
        assert_eq!(options.selected(), 0);
        step(&mut app, Event::Center);
        assert_eq!(app.top_id(), list);
        step(&mut app, Event::CenterLong);
        step(&mut app, Event::Center);
        step(&mut app, Event::Scroll(1));
        step(&mut app, Event::Back);
        assert_eq!(app.top_id(), list);
        assert!(path.exists());
        assert_eq!(app.lib.db().cached_song(&blocking, &song.id, &["raw"]).unwrap().1, path);
        app.player.shutdown();
    }

    #[test]
    fn filesystem_accounting_counts_actual_allocations_once_including_spoken_and_sparse_media() {
        use std::os::unix::fs::MetadataExt;
        let app = crate::ui::settings_tests::fixture();
        let root = app.paths.media_root();
        std::fs::create_dir_all(root).unwrap();
        let cache = root.join("generated-cache");
        let downloaded = root.join("generated-download");
        let spoken = root.join("generated-spoken");
        let alias = root.join("generated-download-alias");
        let artwork=root.join("generated-offline-art.png");
        let sparse = root.join("generated-sparse");
        std::fs::write(&cache, [1u8; 4097]).unwrap();
        std::fs::write(&downloaded, [2u8; 8193]).unwrap();
        std::fs::write(&spoken, [3u8; 4096]).unwrap();
        std::fs::write(&artwork,[4u8;4097]).unwrap();
        std::fs::hard_link(&downloaded, &alias).unwrap();
        std::fs::File::create(&sparse).unwrap().set_len(1 << 30).unwrap();
        let song = |id: &str| subsonic::Song { id: id.into(), ..Default::default() };
        {
            let db = app.lib.db();
            db.put_cached_song(&crate::rt::Blocking::dedicated_thread(), "cache-a", "original", &cache, u64::MAX / 2).unwrap();
            db.put_cached_song(&crate::rt::Blocking::dedicated_thread(), "cache-b", "mp3", &cache, 1).unwrap();
            db.put_cached_song(&crate::rt::Blocking::dedicated_thread(), "also-cache", "original", &alias, 999_999).unwrap();
            db.put_cached_song(&crate::rt::Blocking::dedicated_thread(), "missing", "original", &root.join("generated-missing"), 999_999).unwrap();
            db.put_cached_song(&crate::rt::Blocking::dedicated_thread(), "sparse", "original", &sparse, 1 << 30).unwrap();
            db.put_offline_song(&crate::rt::Blocking::dedicated_thread(), &song("download"), &downloaded, 1);
            let art=crate::library::db::OfflineArt {key:"generated-shared-cover".into(),path:artwork.clone(),size:1};
            db.put_spoken_downloads_with_art(&crate::rt::Blocking::dedicated_thread(), "generated-logical", &[("spoken".into(), spoken.clone(), 1), ("also-spoken".into(), alias, 1)],Some(&art)).unwrap();
            db.put_spoken_downloads_with_art(&crate::rt::Blocking::dedicated_thread(), "other-generated-episode",&[("other-spoken".into(),spoken.clone(),1)],Some(&art)).unwrap();
            db.put_cached_song(&crate::rt::Blocking::dedicated_thread(), "same-cover","original",&artwork,u64::MAX/2).unwrap();
        }
        let allocation = |path: &Path| std::fs::metadata(path).unwrap().blocks() * 512;
        assert_eq!(app.lib.storage_allocations().unwrap(), (allocation(&cache) + allocation(&sparse), allocation(&downloaded) + allocation(&spoken) + allocation(&artwork)));
        let runtime = crate::rt::Ui::new().unwrap();
        assert_eq!(runtime.block_on(app.lib.storage_allocations_async()).unwrap(), app.lib.storage_allocations().unwrap(), "awaited index preserves exact allocation/dedup accounting");
        std::fs::remove_file(&cache).unwrap();
        assert_eq!(app.lib.storage_allocations().unwrap().0, allocation(&sparse), "stale sizes do not become fictional allocated bytes");
        assert!(Usage::read(&app.lib, &root.join("generated-missing-storage")).is_err());
        app.player.shutdown();
    }

    #[test]
    fn real_app_wheel_changes_wedges_preserves_refresh_identity_and_returns_to_storage_row() {
        use crate::ui::{screens::settings::{Page, Settings}, UiMsg};
        let mut app = crate::ui::settings_tests::fixture();
        app.background=false;
        app.push(Box::new(Settings::new(Page::Storage)));
        let parent = app.top_id();
        let step = |app: &mut crate::ui::App, event| app.event(event, &mut crate::platform::Headless { last: None });
        step(&mut app, Event::Center);
        assert_eq!(app.stack.last().unwrap().1.title(), "Storage Usage");
        let screen = app.top_id();
        let probe = |app: &mut crate::ui::App| {
            let (reply, mut state)=tokio::sync::oneshot::channel();
            app.handle(UiMsg::ScriptProbe(reply), &mut crate::platform::Headless { last: None });
            state.try_recv().unwrap()
        };
        assert!(!probe(&mut app).ready, "script barrier cannot mistake initial loading for interactive wedges");
        let usage = Usage::from_counts(200, 50, 40, 30, 60).unwrap();
        app.handle(UiMsg::Loaded { screen, token: 1, data: Box::new(Ok::<_, String>(usage.clone())) }, &mut crate::platform::Headless { last: None });
        assert!(probe(&mut app).ready, "actual completion releases the script barrier");
        let root = crate::library::generated_test_directory("storage-ui");
        std::fs::create_dir_all(&root).unwrap();
        let mut frames = std::collections::HashSet::new();
        for look in [crate::config::Look::Classic, crate::config::Look::Dark] {
            app.config.display.look = look;
            app.config_changed();
            for slice in &usage.slices {
                let canvas = app.render();
                let rgb: Vec<u8> = canvas.pixels.iter().flat_map(|p| [(p >> 16) as u8, (p >> 8) as u8, *p as u8]).collect();
                image::save_buffer(root.join(format!("{}-{}.png", if look == crate::config::Look::Classic { "light" } else { "dark" }, slice.label.replace(' ', "-"))), &rgb, canvas.width as u32, canvas.height as u32, image::ColorType::Rgb8).unwrap();
                assert!(frames.insert(canvas.pixels.clone()), "every selected wedge/theme has a distinct actual rendered state");
                step(&mut app, Event::Scroll(1));
            }
        }
        step(&mut app, Event::Scroll(1)); // Song Cache.
        step(&mut app, Event::Center);
        assert_eq!(app.stack.last().unwrap().1.title(), "Song Cache");
        step(&mut app, Event::Back);
        assert_eq!(app.top_id(), screen, "folder Back restores selected pie wedge");
        assert!(app.stack.last().unwrap().1.audit().state().ends_with("selected=1"));
        app.with_screen(screen, |screen, ctx| {
            screen.refresh(&crate::library::LibEvent::Downloads, ctx);
            // The read waits for a lit tick a second after the last one.
            ctx.screen_on = true;
            assert!(!screen.tick(ctx));
            ctx.now += DOWNLOAD_READ;
            screen.tick(ctx);
        });
        let new_usage = Usage::from_counts(200, 50, 40, 60, 0).unwrap();
        // Returning from the folder re-enters Storage and starts read 2;
        // the explicit Downloads refresh supersedes it with read 3.
        app.handle(UiMsg::Loaded { screen, token: 3, data: Box::new(Ok::<_, String>(new_usage)) }, &mut crate::platform::Headless { last: None });
        assert_eq!(app.stack.last().unwrap().1.audit().state(), "wedges=Song Cache,Other Files,Reserved Space,Free;selected=0");
        app.handle(UiMsg::Loaded { screen, token: 1, data: Box::new(Ok::<_, String>(usage)) }, &mut crate::platform::Headless { last: None });
        assert!(app.stack.last().unwrap().1.audit().state().starts_with("wedges=Song Cache,"), "old refresh cannot restore removed wedges");
        step(&mut app, Event::Back);
        assert_eq!(app.top_id(), parent);
        step(&mut app, Event::Center);
        assert_eq!(app.stack.last().unwrap().1.title(), "Storage Usage", "Back preserved the Storage Usage parent row");
        app.player.shutdown();
    }
}
