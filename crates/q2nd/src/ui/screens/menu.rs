//! A page of rows from a [`Source`]: what was cached shows at once, and the
//! server's fresh answer replaces it when it comes.

use super::popup;
use crate::input::Event;
use crate::library::{LibError, LibEvent, Library, Policy};
use crate::ui::list::{Badge, ListView, Row, RowAction};
use crate::ui::{Action, Ctx, Draw, Screen};
use crate::gfx::Rect;
use std::any::Any;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Rows below the view whose artwork a resting list prefetches.
const ROWS_BELOW: usize = 6;

mod continuous;
use continuous::{Pages, WindowLoaded};

pub type Rows = Result<Vec<Row>, LibError>;
/// A source's rows, as a task.
pub type RowsTask = std::pin::Pin<Box<dyn std::future::Future<Output = Rows> + Send>>;
/// A later page's rows and whether another may follow, as a task.
pub type JumpTask = std::pin::Pin<Box<dyn std::future::Future<Output = Result<Option<(u32, String)>, LibError>> + Send>>;
pub type PageTask = std::pin::Pin<Box<dyn std::future::Future<Output = Result<(Vec<Row>, bool), LibError>> + Send>>;

/// An accepted page carries letter eligibility for the same catalog version.
pub struct WindowPage {
    pub rows: Vec<Row>,
    pub more: bool,
    pub letters: bool,
}
impl From<(Vec<Row>, bool)> for WindowPage {
    fn from((rows, more): (Vec<Row>, bool)) -> Self {
        Self { rows, more, letters: false }
    }
}
pub type WindowPageTask = std::pin::Pin<Box<dyn std::future::Future<Output = Result<WindowPage, LibError>> + Send>>;

/// Rows already built (a local source).
pub fn ready(rows: Rows) -> RowsTask {
    Box::pin(std::future::ready(rows))
}

/// Projects already-awaited snapshots into rows on the finite local job pool.
/// No database, card or network waits belong inside this projection.
pub async fn on_pool<T: Send + 'static>(lib: Arc<Library>, build: impl FnOnce(&Library) -> T + Send + 'static) -> T {
    crate::rt::jobs().run_isolated(move |_| build(&lib)).await.expect("row building panicked")
}

/// Ordinary fixture compatibility; production transfers a pending load to an
/// async page task even when a local source expected to finish immediately.
#[cfg(test)]
pub fn load_now(source: &Arc<dyn Source>, lib: &Arc<Library>, policy: Policy) -> Rows {
    use std::task::{Context, Poll, Waker};
    let mut task = source.clone().load(lib.clone(), policy);
    if let Poll::Ready(rows) = task.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        return rows;
    }
    assert!(crate::rt::may_wait_for_tasks(), "await menu rows on runtime/pool callers");
    let (tx, rx) = tokio::sync::oneshot::channel();
    crate::rt::io().spawn(async move {
        let _ = tx.send(task.await);
    });
    rx.blocking_recv().unwrap_or(Err(LibError::Offline))
}

/// Where a page's rows come from.
pub trait Source: Send + Sync + 'static {
    fn title(&self) -> String;
    /// Rows with art (72 px) rather than text rows.
    fn alphabetical(&self) -> bool { false }
    fn global_alphabet(&self) -> bool { false }
    fn alphabet_jump(self: Arc<Self>, _lib: Arc<Library>, _position: u32, _delta: i32) -> JumpTask { Box::pin(async { Ok(None) }) }
    fn refetch_history(&self) -> bool { true }
    fn refresh_retained(&self) -> bool { true }
    fn refresh_policy(&self) -> Policy { Policy::NetworkFirst }
    fn media(&self) -> bool {
        false
    }
    /// Builds the rows, from the cache only or from the network (else the
    /// cache). An immediate local source returns a ready future; storage
    /// sources await owned storage snapshots before [`on_pool`], and a network
    /// source awaits [`Library::fetch_async`] before projecting rows. Polled on the
    /// I/O runtime, or polled once inline for an immediate local menu.
    fn load(self: Arc<Self>, lib: Arc<Library>, policy: Policy) -> RowsTask;
    /// Rows built from what is on hand (menus, settings): no network, and
    /// built again on every visit.
    fn local(&self) -> bool {
        false
    }
    /// In-memory local menus may build on the UI thread. Local sources that
    /// read storage override this and return an awaited worker result.
    fn immediate(&self) -> bool { self.local() }
    fn empty(&self) -> &'static str {
        "Nothing here"
    }
    /// Rows with no action are notes, drawn as information rather than as
    /// choices (see `ListView::set_passive_info`).
    fn info_rows(&self) -> bool { false }
    /// A `RowAction::Cmd` row was chosen.
    /// Rows of a later page (1, 2, …) for endpoints that page, and whether
    /// another may follow; `None` when the source has a single page. The menu
    /// asks when the selection nears the end of what it has (BROWSE-001).
    fn page(self: Arc<Self>, _lib: Arc<Library>, _page: u32) -> Option<PageTask> { None }
    /// Whether `page` can return rows; others are never asked.
    fn paged(&self) -> bool { false }
    /// Deterministic offset pages can be evicted and fetched again. Random
    /// draws need separate history semantics and do not opt in here.
    fn windowed(&self) -> bool { false }
    /// Fetch a window page under the requested cache/network policy. Page zero
    /// uses the ordinary source, including any leading commands.
    fn window_page(self: Arc<Self>, lib: Arc<Library>, page: u32, policy: Policy) -> WindowPageTask {
        Box::pin(async move {
            if page == 0 { return self.load(lib, policy).await.map(|rows| (rows, true).into()); }
            Err(LibError::Server("This list does not support page history".into()))
        })
    }
    /// Whether song and album rows show download badges (not a play queue).
    fn download_badges(&self) -> bool { true }
    fn cmd(&self, _cmd: u32, _row: &Row, _ctx: &mut Ctx) -> Action {
        Action::None
    }
    /// The menu for holding the centre on a row, if not the usual one.
    fn menu(&self, _row: &Row, _ctx: &mut Ctx) -> Option<Box<dyn Screen>> {
        None
    }
    /// Account changes must clear the previous account's visible rows.
    fn reset_on(&self,_ev:&LibEvent)->bool{false}
    /// Which library changes call for a reload.
    fn reload_on(&self, ev: &LibEvent) -> bool {
        matches!(ev, LibEvent::Online(_))
    }
}

enum State {
    Loading,
    Ready,
    Failed(String),
}

pub struct Menu {
    source: Arc<dyn Source>,
    pages: Option<Pages>,
    list: ListView,
    state: State,
    generation: u64,
    loaded_once: bool,
    last_scroll: Instant,
    warmed: Option<usize>,
    /// Pages after the first: the next to ask for, whether one may follow,
    /// and the token of the request in flight.
    next_page: u32,
    more: bool,
    page_pending: Option<u64>,
    /// When download badges were last brought up to date, whether a
    /// download event made them stale, and the request in flight.
    badged: Instant,
    badges_stale: bool,
    badge_pending: Option<u64>,
    badge_seq: u64,
    /// Rows from the first page, ahead of appended ones.
    base_len: usize,
    /// The current load's deadline (UX-006).
    deadline: crate::ui::state::Deadline,
    /// A download event asked for a reload not yet made, and when the last
    /// reload was made: download progress reloads a list (Downloads, an
    /// offline page) only while it is on screen and lit, at most once a
    /// second; a list under another screen reloads when entered again.
    reload_stale: bool,
    reloaded: Option<Instant>,
}

/// Download-driven reloads of the list on screen are at least this far apart.
const DOWNLOAD_RELOAD: Duration = Duration::from_secs(1);

/// A page with nothing to show after this says so and offers a retry (UX-006).
#[cfg(test)]
pub(crate) const LOAD_DEADLINE: Duration = crate::ui::state::DEADLINE;

/// Download state read off the UI thread for re-badging.
struct BadgeData {
    account_generation: u32,
    view: Option<crate::library::DownloadView>,
}
const BADGE_TOKEN: u64 = 1 << 61;

struct Loaded {
    rows: Result<Vec<Row>, LibError>,
}

struct PageLoaded {
    page: u32,
    rows: Result<(Vec<Row>, bool), LibError>,
}

/// Ask for the next page this many rows before the end.
const PAGE_EDGE: usize = 8;
/// At most this many pages after the first (a random endpoint never ends).
const MAX_PAGES: u32 = 20;
const PAGE_TOKEN: u64 = 1 << 62;
const JUMP_TOKEN: u64 = PAGE_TOKEN + 1;

impl Menu {
    pub fn new(source: impl Source) -> Menu {
        let pages = source.windowed().then(Pages::default);
        let media = source.media();
        let mut list = ListView::new(media);
        list.alphabetical = source.alphabetical();
        list.global_alphabet = source.global_alphabet();
        list.set_passive_info(source.info_rows());
        Menu {
            source: Arc::new(source),
            pages,
            list,
            state: State::Loading,
            generation: 0,
            loaded_once: false,
            last_scroll: Instant::now(),
            warmed: None,
            next_page: 1,
            more: true,
            page_pending: None,
            badged: Instant::now(),
            badges_stale: false,
            badge_pending: None,
            badge_seq: 0,
            base_len: 0,
            deadline: crate::ui::state::Deadline::new(Instant::now()),
            reload_stale: false,
            reloaded: None,
        }
    }

    fn badgeable(&self) -> bool {
        self.source.download_badges() && self.list.rows.iter().any(|r| r.song_item().is_some() || matches!(r.action, RowAction::Open(crate::ui::Dest::Album { .. })))
    }

    /// Await the DB owner: a card fsync must not retain a UI job worker.
    /// `apply_badges` checks the account again before touching visible rows.
    fn request_badges(&mut self, ctx: &mut Ctx) {
        if self.badge_pending.is_some() || !self.badgeable() { return; }
        self.badge_seq = self.badge_seq.wrapping_add(1) & 0xffff_ffff;
        let token = BADGE_TOKEN | self.badge_seq;
        self.badge_pending = Some(token);
        self.badges_stale = false;
        self.badged = ctx.now;
        let lib = ctx.lib.clone();
        let account_generation = lib.client_generation();
        ctx.task(token, move |reply| async move {
            let view = lib.download_view_async(account_generation).await;
            reply.send(BadgeData { account_generation, view });
        });
    }

    /// True when a badge changed. Other badges (Check, Playing) are left alone.
    fn apply_badges(&mut self, data: BadgeData, lib: &Library) -> bool {
        use super::sources::download_badge;
        self.badge_pending = None;
        let Some(view) = data.view.filter(|_| data.account_generation == lib.client_generation()) else {
            self.badges_stale = true;
            return false;
        };
        let downloadish = |b: Badge| matches!(b, Badge::None | Badge::Offline | Badge::Queued | Badge::Downloading(_));
        let mut changed = false;
        for row in &mut self.list.rows {
            if !downloadish(row.badge) { continue; }
            let next = if let Some(song) = row.song_item() {
                download_badge(view.song(&song.id, view.downloaded(&song.id)))
            } else if let RowAction::Open(crate::ui::Dest::Album { id, .. }) = &row.action {
                download_badge(view.album(id))
            } else { continue };
            if next != row.badge { row.badge = next; changed = true; }
        }
        changed
    }

    /// Near the end of a paged list: fetch the next page in the background.
    fn maybe_page(&mut self, ctx: &mut Ctx) {
        if !self.source.paged() || !self.more || self.page_pending.is_some() || !matches!(self.state, State::Ready) || self.list.rows.is_empty()
            || self.list.selected + PAGE_EDGE < self.list.rows.len() || self.next_page > MAX_PAGES {
            return;
        }
        // Page tokens are their own space, so a reload's later (fresh) reply
        // still matches its generation; a reload clears page_pending.
        let token = PAGE_TOKEN | (self.generation << 8) | u64::from(self.next_page);
        self.page_pending = Some(token);
        let (source, lib, page) = (self.source.clone(), ctx.lib.clone(), self.next_page);
        ctx.task(token, move |reply| async move {
            // A single-page source answers "no more" once, then is never asked.
            let rows = match source.page(lib, page) {
                Some(task) => task.await,
                None => Ok((Vec::new(), false)),
            };
            reply.send(PageLoaded { page, rows });
        });
    }

    fn apply_page(&mut self, l: PageLoaded) {
        self.page_pending = None;
        match l.rows {
            Ok((rows, more)) => {
                let seen: std::collections::HashSet<String> = self.list.rows.iter().map(|r| r.key.clone()).collect();
                let fresh: Vec<Row> = rows.into_iter().filter(|r| !seen.contains(&r.key)).collect();
                // A page that adds nothing new (a repeating or random
                // endpoint) ends the list rather than looping.
                self.more = more && !fresh.is_empty();
                self.next_page = l.page + 1;
                if !fresh.is_empty() {
                    log::info!("{}: page {} added {} rows{}", self.source.title(), l.page, fresh.len(), if self.more { "" } else { "; end" });
                }
                if !fresh.is_empty() {
                    let mut all = self.list.rows.clone();
                    all.extend(fresh);
                    self.list.set_rows_anchored(all);
                }
            }
            // Retried when the selection moves near the end again.
            Err(_) => {}
        }
    }

    pub fn boxed(source: impl Source) -> Box<dyn Screen> {
        Box::new(Menu::new(source))
    }

    fn reload(&mut self, ctx: &mut Ctx) {
        self.deadline.restart(ctx.now);
        self.reload_stale = false;
        self.reloaded = Some(ctx.now);
        // UXI-019: an empty page the listener just opened does not queue
        // behind background loads.
        let waiting = self.list.rows.is_empty();
        if self.pages.is_some() { self.reload_window(ctx); return; }
        self.generation += 1;
        // Appended pages and badge snapshots belong to the previous rows.
        self.page_pending = None;
        self.badge_pending = None;
        let source = self.source.clone();
        let lib = ctx.lib.clone();
        if source.local() {
            if !source.immediate() {
                ctx.page_task(self.generation, waiting, move |reply| async move {
                    reply.send(Loaded { rows: source.load(lib, Policy::CacheOnly).await });
                });
                return;
            }
            let mut rows = source.load(lib, Policy::CacheOnly);
            use std::task::{Context, Poll, Waker};
            if let Poll::Ready(rows) = rows.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
                self.apply(Loaded { rows });
                return;
            }
            // Keep the already-started future: restarting a load can duplicate
            // work or lose its reply. Generation and screen tokens still own it.
            ctx.page_task(self.generation, waiting, move |reply| async move {
                reply.send(Loaded { rows: rows.await });
            });
            return;
        }
        ctx.page_task(self.generation, waiting, move |reply| async move {
            let cached = source.clone().load(lib.clone(), Policy::CacheOnly).await;
            let have_cache = cached.is_ok();
            if have_cache {
                reply.send(Loaded { rows: cached });
            }
            if lib.is_online() || !have_cache {
                let policy = source.refresh_policy();
                // Behind cached rows the refresh is low priority: its failure
                // changes nothing on screen and is no evidence about the server.
                let priority = if have_cache { crate::library::Priority::Low } else { crate::library::Priority::Foreground };
                let fresh = crate::library::with_priority(priority, source.load(lib, policy)).await;
                // A failure after cached rows changes nothing on screen.
                if fresh.is_ok() || !have_cache {
                    reply.send(Loaded { rows: fresh });
                }
            }
        });
    }

    fn apply(&mut self, l: Loaded) {
        match l.rows {
            Ok(mut rows) => {
                // A first page arriving after later pages were appended (the
                // fresh reply of a reload, a reconnect) keeps the appended
                // rows it does not repeat, so the list has no hole.
                let first = rows.len();
                if self.next_page > 1 && self.base_len <= self.list.rows.len() {
                    let keys: std::collections::HashSet<String> = rows.iter().map(|r| r.key.clone()).collect();
                    rows.extend(self.list.rows[self.base_len..].iter().filter(|r| !keys.contains(&r.key)).cloned());
                    self.list.set_rows_anchored(rows);
                } else {
                    self.list.set_rows(rows);
                }
                self.base_len = first;
                self.warmed = None;
                self.state = State::Ready;
                crate::ux::end(crate::ux::Journey::OpenPage, crate::ux::Outcome::Shown);
            }
            Err(e) => {
                if self.list.rows.is_empty() {
                    self.state = State::Failed(crate::ux::present_in(&e.to_string(), crate::ux::Journey::OpenPage));
                    crate::ux::end(crate::ux::Journey::OpenPage, crate::ux::Outcome::Failed);
                }
            }
        }
    }

    fn choose(&mut self, ctx: &mut Ctx) -> Action {
        let Some(row) = self.list.selected_row().cloned() else { return Action::None };
        if self.retry_window(&row.key, ctx) { return Action::Stay; }
        match &row.action {
            RowAction::None => Action::None,
            RowAction::Open(dest) => Action::Push(super::open(dest.clone(), ctx)),
            RowAction::Play { songs, index } => {
                if row.dim {
                    return Action::Toast("Not downloaded".into());
                }
                // Offline, only what is on the card plays.
                if !ctx.lib.is_online() {
                    return Action::Push(Box::new(Playable::new(songs.clone(), *index, false)));
                }
                ctx.play(songs.clone(), *index, false);
                Action::NowPlaying
            }
            RowAction::Shuffle(songs) => {
                if !ctx.lib.is_online() {
                    return Action::Push(Box::new(Playable::new(songs.clone(), 0, true)));
                }
                if songs.is_empty() { return Action::Toast("Nothing downloaded to play".into()); }
                ctx.play(songs.clone(), 0, true);
                Action::NowPlaying
            }
            RowAction::Cmd(c) => {
                let source = self.source.clone();
                let a = source.cmd(*c, &row, ctx);
                if source.local() {
                    self.reload(ctx);
                }
                a
            }
        }
    }
}

/// Offline activation rechecks one owned DB snapshot, while Back can dismiss
/// this screen. A stale or dismissed reply never starts playback.
struct Playable {
    songs: Arc<Vec<subsonic::Song>>,
    index: usize,
    shuffle: bool,
    origin: Option<(u32, u64)>,
    action: Option<Action>,
    deadline: crate::ui::state::Deadline,
}
impl Playable {
    fn new(songs: Arc<Vec<subsonic::Song>>, index: usize, shuffle: bool) -> Self {
        Self { songs, index, shuffle, origin: None, action: None, deadline: crate::ui::state::Deadline::new(Instant::now()) }
    }
}
fn playable(snapshot: crate::library::MusicRowSnapshot, index: usize) -> (Vec<subsonic::Song>, usize) {
    if snapshot.online { return (snapshot.songs, index); }
    let mut start = 0;
    let mut out = Vec::new();
    for (i, song) in snapshot.songs.into_iter().enumerate() {
        if snapshot.local.contains(&song.id) {
            if i <= index { start = out.len(); }
            out.push(song);
        }
    }
    (out, start)
}
impl Screen for Playable {
    fn title(&self) -> String { "Opening Music".into() }
    fn script_ready(&self) -> bool { false }
    fn event(&mut self, _: Event, _: &mut Ctx) -> Action { Action::None }
    #[cfg(test)]
    fn audit(&self) -> crate::ui::hierarchy::Surface {
        crate::ui::hierarchy::Surface::widget("OfflineMusicOpening", "Reading downloaded music".into())
    }
    fn enter(&mut self, ctx: &mut Ctx) {
        if self.origin.is_some() { return; }
        self.origin = Some((ctx.lib.music_account_generation(), ctx.player.status().generation));
        self.deadline.restart(ctx.now);
        let lib = ctx.lib.clone(); let snapshot = lib.music_rows_snapshot(self.songs.as_ref().clone()); let index = self.index;
        ctx.foreground_task(1, move |reply| async move {
            let result = match snapshot.await {
                Ok(snapshot) => Ok(on_pool(lib, move |_| playable(snapshot, index)).await),
                Err(error) => Err(error),
            };
            reply.send(result);
        });
    }
    fn loaded(&mut self, token: u64, data: Box<dyn Any + Send>, ctx: &mut Ctx) {
        if token != 1 { return; }
        let Ok(result) = data.downcast::<Result<(Vec<subsonic::Song>, usize), LibError>>() else { return; };
        let mut actions = vec![Action::Dismiss(ctx.screen)];
        if self.origin != Some((ctx.lib.music_account_generation(), ctx.player.status().generation)) {
            actions.push(Action::Toast("Playback changed; select the item again".into()));
        } else {
            match *result {
                Ok((songs, index)) if !songs.is_empty() => {
                    ctx.play(songs, index, self.shuffle);
                    actions.push(Action::NowPlaying);
                }
                Ok(_) => actions.push(Action::Toast(if self.shuffle { "Nothing downloaded to play" } else { "Not downloaded" }.into())),
                Err(error) => actions.push(Action::Toast(error.to_string())),
            }
        }
        self.action = Some(Action::Many(actions));
    }
    fn completion(&mut self) -> Option<Action> { self.action.take() }
    fn tick(&mut self, ctx: &mut Ctx) -> bool {
        // The shared page deadline (UX-006): a late reply is then ignored.
        if self.action.is_none() && self.origin.is_some() && self.deadline.expired(ctx.now, crate::ux::Journey::PlayMusic) {
            self.origin = Some((u32::MAX, u64::MAX));
            self.action = Some(Action::Many(vec![Action::Dismiss(ctx.screen), Action::Toast(crate::ux::Code::Stuck.message().into())]));
            return true;
        }
        false
    }
    fn draw(&mut self, draw: &mut Draw, area: Rect) { draw.message(area, "Reading downloaded music…"); }
}

impl Menu {
    fn end_wheel_gesture(&mut self) -> bool {
        if self.pages.as_ref().is_some_and(|pages| pages.jump_pending()) {
            self.cancel_jump();
        }
        self.list.finish_letters()
    }
}

impl Screen for Menu {

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

    fn holds_art(&self) -> bool { self.list.media }
    fn script_ready(&self) -> bool { !matches!(self.state, State::Loading) || !self.list.rows.is_empty() }
    #[cfg(test)]
    fn audit_settle(&mut self, ctx: &mut Ctx) {
        self.apply(Loaded { rows: load_now(&self.source, ctx.lib, Policy::CacheOnly) });
    }
    #[cfg(test)]
    fn audit(&self) -> crate::ui::hierarchy::Surface {
        let state = match &self.state { State::Loading => "loading".into(), State::Ready if self.list.rows.is_empty() => "empty".into(), State::Ready => "ready".into(), State::Failed(error) => format!("failed: {error}") };
        crate::ui::hierarchy::Surface::list(&self.list).with_state(state)
    }
    fn title(&self) -> String {
        self.source.title()
    }

    fn enter(&mut self, ctx: &mut Ctx) {
        // The gesture belongs to the visible list. Another screen can receive
        // the release while this one is covered.
        self.end_wheel_gesture();
        if !self.loaded_once || self.source.local() || self.reload_stale {
            if !self.loaded_once { crate::ux::begin(crate::ux::Journey::OpenPage); }
            self.loaded_once = true;
            self.reload(ctx);
        }
    }

    fn event(&mut self, ev: Event, ctx: &mut Ctx) -> Action {
        match ev {
            Event::Scroll(t) => {
                self.list.wheel(t, ctx.now);
                self.last_scroll = ctx.now;
                let jump = self.list.take_alphabet_jump();
                if self.list.in_letter_mode() {
                    if jump != 0 { self.jump_window(jump, ctx); }
                } else if self.pages.is_some() {
                    self.scroll_window(t, ctx);
                } else {
                    self.maybe_page(ctx);
                }
                Action::None
            }
            Event::WheelEnd => {
                if self.end_wheel_gesture() { Action::Stay } else { Action::None }
            }
            Event::Center if self.list.in_letter_mode() => {
                self.end_wheel_gesture();
                Action::Stay
            },
            Event::Center if matches!(self.state, State::Failed(_)) && (self.list.rows.is_empty() || self.lone_retry().is_some()) => {
                self.state = State::Loading;
                crate::ux::begin(crate::ux::Journey::OpenPage);
                if self.lone_retry().is_some() { self.restart_window(ctx); } else { self.reload(ctx); }
                Action::Stay
            }
            Event::Center => self.choose(ctx),
            Event::CenterLong => match self.list.selected_row() {
                Some(row) => match self.source.menu(row, ctx).or_else(|| popup::for_row(row, ctx)) {
                    Some(p) => Action::Push(p),
                    None => Action::None,
                },
                None => Action::None,
            },
            _ => Action::None,
        }
    }

    fn loaded(&mut self, token: u64, data: Box<dyn Any + Send>, ctx: &mut Ctx) {
        if token == JUMP_TOKEN && self.pages.is_some() {
            if let Ok(loaded) = data.downcast::<continuous::JumpLoaded>() { self.apply_jump(*loaded, ctx); }
        } else if token == PAGE_TOKEN && self.pages.is_some() {
            if let Ok(loaded) = data.downcast::<WindowLoaded>() { self.apply_window(*loaded, ctx); }
        } else if token == self.generation {
            if let Ok(l) = data.downcast::<Loaded>() { self.apply(*l); }
        } else if Some(token) == self.page_pending {
            if let Ok(l) = data.downcast::<PageLoaded>() { self.apply_page(*l); }
        } else if Some(token) == self.badge_pending {
            if let Ok(d) = data.downcast::<BadgeData>() { self.apply_badges(*d, ctx.lib); }
        }
    }

    fn refresh(&mut self, ev: &LibEvent, ctx: &mut Ctx) {
        if self.source.reset_on(ev) { self.reset_window(); }
        if self.source.reset_on(ev){self.badge_pending=None;self.list.set_rows(Vec::new());self.state=State::Loading;self.next_page=1;self.more=true;self.base_len=0;self.deadline.restart(ctx.now);}
        // Download progress arrives once a second while anything downloads:
        // the reload waits for tick (on screen, lit, DOWNLOAD_RELOAD apart)
        // or the next enter.
        if self.source.reload_on(ev) && matches!(ev, LibEvent::Downloads) { self.reload_stale = true; }
        else if self.source.reload_on(ev) {self.reload(ctx);}
        // Mark stale; the top screen re-badges in tick, off the UI thread.
        else if matches!(ev, LibEvent::Downloads) { self.badges_stale = true; }
    }

    fn tick(&mut self, ctx: &mut Ctx) -> bool {
        let waiting = (matches!(self.state, State::Loading) && self.list.rows.is_empty())
            || matches!(self.lone_retry(), Some(crate::ui::state::View::Loading));
        if waiting && self.deadline.expired(ctx.now, crate::ux::Journey::OpenPage) {
            // A late answer still replaces this message (apply accepts it).
            self.state = State::Failed(crate::ux::Code::Stuck.message().into());
            return true;
        }
        if self.reload_stale && ctx.screen_on
            && self.reloaded.is_none_or(|at| ctx.now.saturating_duration_since(at) >= DOWNLOAD_RELOAD) {
            self.reload(ctx);
        }
        // After a download event, and once a second while a ring moves.
        if ctx.screen_on && (self.badges_stale || (ctx.now.saturating_duration_since(self.badged) >= Duration::from_secs(1)
            && self.list.rows.iter().any(|r| matches!(r.badge, Badge::Downloading(_))))) {
            self.request_badges(ctx);
        }
        if !ctx.screen_on || self.warmed == Some(self.list.selected) || ctx.now.saturating_duration_since(self.last_scroll) < Duration::from_millis(150) { return false; }
        self.warmed = Some(self.list.selected);
        let center = self.list.selected;
        let mut ids = Vec::new();
        // Selected album, then alternating neighbours. Nothing scales with
        // library length and a moving wheel does not flood the network.
        for delta in [0isize, 1, -1, 2, -2, 3, -3, 4] {
            let i = center as isize + delta;
            if i < 0 { continue; }
            let Some(row) = self.list.rows.get(i as usize) else { continue };
            if let RowAction::Open(crate::ui::Dest::Album { id, .. }) = &row.action { ids.push(id.clone()); }
            if let Some(art) = &row.art { ctx.lib.art.prefetch_cover(art, crate::library::art::THUMBNAIL_SIZE); }
        }
        // The rows just below the view, from the card's cache or the server,
        // so the next scroll draws them at once. Bounded by the decoded-art
        // cache (50 px thumbnails, 10 KiB each).
        for i in self.list.rows_below(ROWS_BELOW) {
            if let Some(art) = &self.list.rows[i].art { ctx.lib.art.prefetch_cover(art, crate::library::art::THUMBNAIL_SIZE); }
        }
        if !ids.is_empty() { ctx.lib.prefetch_albums(ids); }
        false
    }

    fn draw(&mut self, d: &mut Draw, area: Rect) {
        use crate::ui::state::View;
        let lone = self.lone_retry();
        match &self.state {
            State::Failed(text) if self.list.rows.is_empty() || lone.is_some() => self.list.draw_state(d, area, View::Failed { text, retry: true }),
            State::Loading if self.list.rows.is_empty() => self.list.draw_state(d, area, View::Loading),
            _ if lone.is_some() => {
                let target = crate::ui::state::draw_target(d, area, lone.unwrap());
                self.list.set_state_target(target);
            }
            _ if self.list.rows.is_empty() => self.list.draw_state(d, area, View::Empty(self.source.empty())),
            _ => self.list.draw(d, area),
        }
    }


    fn animate(&mut self, now: Instant) -> bool {
        self.list.animate(now)
    }
}

#[cfg(test)]
mod local_load_tests {
    use super::*;
    use std::sync::{Mutex, atomic::{AtomicUsize, Ordering}};

    struct Deferred {
        release: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
        calls: Arc<AtomicUsize>,
    }
    impl Source for Deferred {
        fn title(&self) -> String { "Generated deferred local menu".into() }
        fn local(&self) -> bool { true }
        fn load(self: Arc<Self>, _: Arc<Library>, _: Policy) -> RowsTask {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let release = self.release.lock().unwrap().take().expect("one load");
            Box::pin(async move {
                release.await.map_err(|_| LibError::Offline)?;
                Ok(vec![Row::new("Generated ready row", RowAction::None)])
            })
        }
    }

    #[test]
    fn deferred_immediate_menu_keeps_ui_live_and_ignores_reply_after_back() {
        let mut app = crate::ui::settings_tests::fixture();
        app.player.shutdown();
        let runtime = crate::rt::Ui::new().unwrap();
        let mut platform = crate::platform::Headless { last: None };
        for back in [false, true] {
            let (tx, mut inbox) = crate::ui::channel();
            app.tx = tx;
            let parent = app.top_id();
            let (release, waiting) = tokio::sync::oneshot::channel();
            let calls = Arc::new(AtomicUsize::new(0));
            runtime.block_on(async {
                app.push(Menu::boxed(Deferred { release: Mutex::new(Some(waiting)), calls: calls.clone() }));
                // The real enter/reload path returns with the source still
                // pending; no blocking bridge or second load may be used.
                tokio::task::yield_now().await;
            });
            let screen = app.top_id();
            assert!(!app.stack.last().unwrap().1.script_ready());
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            if back { runtime.block_on(async { app.event(Event::Back, &mut platform); }); }
            release.send(()).unwrap();
            runtime.block_on(async {
                loop {
                    let message = tokio::time::timeout(Duration::from_secs(3), inbox.recv()).await.unwrap().unwrap();
                    let ours = matches!(&message, crate::ui::UiMsg::Loaded { screen: owner, .. } if *owner == screen);
                    app.handle_async(message, &mut platform).await;
                    if ours { break; }
                }
            });
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            if back {
                assert_eq!(app.top_id(), parent, "late local rows must not reopen a cancelled menu");
            } else {
                assert_eq!(app.top_id(), screen);
                assert_eq!(app.stack.last().unwrap().1.audit().rows()[0].title, "Generated ready row");
                runtime.block_on(async { app.event(Event::Back, &mut platform); });
                assert_eq!(app.top_id(), parent);
            }
        }
    }
}

#[cfg(test)]
mod paging_tests {
    use super::*;

    struct Paged;
    impl Source for Paged {
        fn title(&self) -> String { "Paged".into() }
        fn paged(&self) -> bool { true }
        fn load(self: Arc<Self>, _: Arc<Library>, _: Policy) -> RowsTask { ready(Ok(rows(0..20))) }
    }
    fn rows(r: std::ops::Range<u32>) -> Vec<Row> {
        r.map(|i| Row::new(&format!("Generated {i}"), RowAction::None).key(&format!("k{i}"))).collect()
    }

    #[test]
    fn later_pages_append_new_rows_keep_selection_and_stop_when_nothing_is_new() {
        let mut m = Menu::new(Paged);
        m.apply(Loaded { rows: Ok(rows(0..20)) });
        m.list.selected = 15;
        m.page_pending = Some(1);
        m.apply_page(PageLoaded { page: 1, rows: Ok((rows(18..40), true)) });
        assert_eq!(m.list.rows.len(), 40, "overlapping rows are not repeated");
        assert_eq!(m.list.selected_row().unwrap().key, "k15", "selection stays on its row");
        assert!(m.more && m.next_page == 2 && m.page_pending.is_none());
        // A reload's fresh first page (or a reconnect) after the append keeps
        // the appended rows: no hole, selection kept.
        m.apply(Loaded { rows: Ok(rows(0..20)) });
        assert_eq!(m.list.rows.len(), 40);
        assert_eq!(m.list.selected_row().unwrap().key, "k15");
        m.apply_page(PageLoaded { page: 2, rows: Ok((rows(0..40), true)) });
        assert_eq!(m.list.rows.len(), 40);
        assert!(!m.more, "a page with nothing new ends the list (random endpoints)");
        let mut m = Menu::new(Paged);
        m.apply(Loaded { rows: Ok(rows(0..20)) });
        m.apply_page(PageLoaded { page: 1, rows: Err(LibError::Server("generated".into())) });
        assert!(m.more && m.next_page == 1, "a failed page is asked for again");
        m.apply_page(PageLoaded { page: 1, rows: Ok((rows(20..30), false)) });
        assert_eq!(m.list.rows.len(), 30);
        assert!(!m.more, "the endpoint said this was the last page");
    }
}


#[cfg(test)]
mod badge_tests {
    use super::*;

    struct Local;
    impl Source for Local {
        fn title(&self) -> String { "Generated badges".into() }
        fn local(&self) -> bool { true }
        fn reset_on(&self, event: &LibEvent) -> bool { matches!(event, LibEvent::SpokenChanged) }
        fn reload_on(&self, event: &LibEvent) -> bool { self.reset_on(event) }
        fn load(self: Arc<Self>, _: Arc<Library>, _: Policy) -> RowsTask {
            let song = subsonic::Song { id: "generated-badge".into(), ..Default::default() };
            ready(Ok(vec![
                Row::new("Generated", RowAction::None).key("song").song(song.clone()),
                Row::new("Selected setting", RowAction::None).key("check").song(song).badge(Badge::Check),
            ]))
        }
    }
    fn reply(generation: u32) -> Box<dyn Any + Send> {
        let mut view = crate::library::DownloadView::default();
        view.offline.insert("generated-badge".into());
        Box::new(BadgeData { account_generation: generation, view: Some(view) })
    }

    #[test]
    fn delayed_badges_cannot_cross_reload_shelf_reset_or_account_and_preserve_selection() {
        let mut app = crate::ui::settings_tests::fixture();
        let screen = app.top_id();
        let mut menu = Menu::new(Local);
        app.with_screen(screen, |_, ctx| {
            menu.reload(ctx);
            assert!(matches!(menu.state, State::Ready), "in-memory menus remain immediately ready");
            menu.list.selected = 1;
            let account = ctx.lib.client_generation();
            let old = BADGE_TOKEN | 1;
            menu.badge_pending = Some(old);
            menu.reload(ctx);
            menu.loaded(old, reply(account), ctx);
            assert!(matches!(menu.list.rows[0].badge, Badge::None));
            menu.badge_pending = Some(old);
            menu.refresh(&LibEvent::SpokenChanged, ctx);
            menu.loaded(old, reply(account), ctx);
            assert!(matches!(menu.list.rows[0].badge, Badge::None));
            let fresh = BADGE_TOKEN | 2;
            menu.badge_pending = Some(fresh);
            menu.loaded(fresh, reply(account.wrapping_add(1)), ctx);
            assert!(menu.badge_pending.is_none() && menu.badges_stale);
            assert!(matches!(menu.list.rows[0].badge, Badge::None));
            menu.list.selected = 1;
            menu.badge_pending = Some(fresh);
            menu.loaded(fresh, reply(account), ctx);
            assert!(matches!(menu.list.rows[0].badge, Badge::Offline));
            assert!(matches!(menu.list.rows[1].badge, Badge::Check));
            assert_eq!(menu.list.selected_row().unwrap().key, "check");
        }).unwrap();
        app.player.shutdown();
    }
}

#[cfg(test)]
mod offline_opening_tests {
    use super::*;
    use crate::audio::player::{Cmd, PlayerHandle, Status};
    use crate::ui::UiMsg;

    #[test]
    fn offline_opening_preserves_filtered_index_shuffle_and_cancels_on_back_or_account_change() {
        let mut app = crate::ui::settings_tests::fixture();
        app.background = false;
        app.player.shutdown();
        let (player, commands) = PlayerHandle::fixture(Status::default()); app.player = player;
        let songs = Arc::new((0..3).map(|n| subsonic::Song { id: format!("generated-{n}"), ..Default::default() }).collect::<Vec<_>>());
        for i in [0, 2] { app.lib.db().put_offline_song(&crate::rt::Blocking::dedicated_thread(), &songs[i], &app.paths.root.join(format!("generated-{i}.flac")), 1); }
        let runtime = crate::rt::Ui::new().unwrap();
        let (filtered, index) = playable(runtime.block_on(app.lib.music_rows_snapshot(songs.as_ref().clone())).unwrap(), 2);
        assert_eq!(filtered.iter().map(|song| song.id.as_str()).collect::<Vec<_>>(), vec!["generated-0", "generated-2"]);
        assert_eq!(index, 1);
        let (_, index_before_missing) = playable(runtime.block_on(app.lib.music_rows_snapshot(songs.as_ref().clone())).unwrap(), 1);
        assert_eq!(index_before_missing, 0, "same preceding available-song rule");
        let parent = app.top_id();
        app.push(Box::new(Playable::new(songs.clone(), 2, false)));
        let cancelled = app.top_id();
        assert!(!app.stack.last().unwrap().1.script_ready());
        app.event(Event::Back, &mut crate::platform::Headless { last: None });
        app.handle(UiMsg::Loaded { screen: cancelled, token: 1, data: Box::new(Ok::<_, LibError>((filtered.clone(), index))) }, &mut crate::platform::Headless { last: None });
        assert_eq!(app.top_id(), parent);
        assert!(commands.try_recv().is_err());
        for shuffle in [false, true] {
            app.push(Box::new(Playable::new(songs.clone(), 2, shuffle)));
            let screen = app.top_id();
            app.handle(UiMsg::Loaded { screen, token: 1, data: Box::new(Ok::<_, LibError>((filtered.clone(), index))) }, &mut crate::platform::Headless { last: None });
            assert!(matches!(commands.try_recv().unwrap(), Cmd::Play { songs: actual, index: 1, shuffle: actual_shuffle } if *actual == filtered && actual_shuffle == shuffle));
            app.event(Event::Back, &mut crate::platform::Headless { last: None });
        }
        app.push(Box::new(Playable::new(songs, 2, false)));
        let stale = app.top_id();
        let mut config = app.config.clone();
        config.server.url = "http://127.0.0.1:1/generated".into(); config.server.user = "generated-other".into();
        app.lib.configure(&config);
        app.handle(UiMsg::Loaded { screen: stale, token: 1, data: Box::new(Ok::<_, LibError>((filtered, index))) }, &mut crate::platform::Headless { last: None });
        assert!(commands.try_recv().is_err(), "old account result cannot start playback");
        assert_ne!(app.top_id(), stale);
    }
}

#[cfg(test)]
mod download_reload_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Counted(Arc<AtomicUsize>);
    impl Source for Counted {
        fn title(&self) -> String { "Generated downloads".into() }
        fn local(&self) -> bool { true }
        fn reload_on(&self, event: &LibEvent) -> bool { matches!(event, LibEvent::Downloads | LibEvent::Online(_)) }
        fn load(self: Arc<Self>, _: Arc<Library>, _: Policy) -> RowsTask {
            self.0.fetch_add(1, Ordering::Relaxed);
            ready(Ok(vec![Row::new("Generated", RowAction::None).key("generated")]))
        }
    }

    /// Loads of a Downloads-like list during one simulated minute of
    /// download progress: two events a second (a spoken and a song
    /// download), the UI ticking every 250 ms when `on_top`.
    fn minute(screen_on: bool, on_top: bool) -> usize {
        let mut app = crate::ui::settings_tests::fixture();
        let screen = app.top_id();
        let loads = Arc::new(AtomicUsize::new(0));
        let mut menu = Menu::new(Counted(loads.clone()));
        let start = Instant::now();
        app.with_screen(screen, |_, ctx| {
            ctx.now = start;
            menu.enter(ctx);
            ctx.screen_on = screen_on;
            for quarter in 1..=240u64 {
                ctx.now = start + Duration::from_millis(quarter * 250);
                if quarter % 2 == 0 { menu.refresh(&LibEvent::Downloads, ctx); }
                if on_top { menu.tick(ctx); }
            }
        }).unwrap();
        app.player.shutdown();
        loads.load(Ordering::Relaxed) - 1
    }

    /// Budget: one minute of download progress reloads the list on screen
    /// at most once a second, and not at all with the screen off or under
    /// another screen (V0.128 reloaded on every event).
    #[test]
    fn download_progress_reload_budget_per_minute() {
        let (lit, dark, covered) = (minute(true, true), minute(false, true), minute(true, false));
        eprintln!("download-progress reloads per minute: on screen {lit}, screen off {dark}, under another screen {covered}");
        assert!((55..=60).contains(&lit), "on screen: {lit}");
        assert_eq!(dark, 0, "screen off");
        assert_eq!(covered, 0, "under another screen");
    }

    /// The deferred reload is made when the screen lights or the list is
    /// entered again; other events still reload at once.
    #[test]
    fn deferred_download_reload_follows_screen_on_and_enter() {
        let mut app = crate::ui::settings_tests::fixture();
        let screen = app.top_id();
        let loads = Arc::new(AtomicUsize::new(0));
        let mut menu = Menu::new(Counted(loads.clone()));
        app.with_screen(screen, |_, ctx| {
            menu.enter(ctx);
            ctx.screen_on = false;
            ctx.now += Duration::from_secs(5);
            menu.refresh(&LibEvent::Downloads, ctx);
            menu.tick(ctx);
            assert_eq!(loads.load(Ordering::Relaxed), 1, "dark: no reload");
            ctx.screen_on = true;
            menu.tick(ctx);
            assert_eq!(loads.load(Ordering::Relaxed), 2, "lit: the deferred reload");
            menu.tick(ctx);
            assert_eq!(loads.load(Ordering::Relaxed), 2, "one reload per event batch");
            menu.refresh(&LibEvent::Downloads, ctx);
            menu.enter(ctx);
            assert_eq!(loads.load(Ordering::Relaxed), 3, "entered again: reloaded");
            menu.refresh(&LibEvent::Online(true), ctx);
            assert_eq!(loads.load(Ordering::Relaxed), 4, "a connection change reloads at once");
        }).unwrap();
        app.player.shutdown();
    }
}
