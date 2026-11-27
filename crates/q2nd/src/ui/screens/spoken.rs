//! Spoken libraries use logical books/episodes; selecting never shuffles files.
#[path = "spoken_speed_view.rs"]
mod speed_view;

use super::menu::Source;
use crate::gfx::Rect;
use crate::library::book_info::BookInfoTarget;
use crate::input::Event;
use crate::library::shelf_browse::{Media, Section};
use crate::library::spoken::PlayedTarget;
use crate::library::{LibError, Library, Policy};
use crate::ui::list::{Badge, Row, RowAction};
use crate::ui::{Action, Ctx, Dest, Draw, Screen};
use std::sync::Arc;
use subsonic::Song;

#[derive(Debug, Clone, PartialEq)]
pub enum Page {
    Local,
    DownloadedBooks,
    DownloadedPodcasts,
    Books,
    Podcasts,
    Libraries {
        media: Media,
        section: Section,
        title: String,
    },
    Browse {
        library: String,
        media: Media,
        section: Section,
        title: String,
    },
}
pub struct SpokenPage(pub Page);
impl Source for SpokenPage {
    fn title(&self) -> String {
        match &self.0 {
            Page::Books => "Books".into(),
            Page::Local => "On This Card".into(),
            Page::DownloadedBooks => "Downloaded Books".into(),
            Page::DownloadedPodcasts => "Downloaded Episodes".into(),
            Page::Podcasts => "Podcasts".into(),
            Page::Libraries { title, .. } | Page::Browse { title, .. } => title.clone(),
        }
    }
    fn local(&self) -> bool {
        matches!(self.0, Page::Books | Page::Podcasts)
    }
    fn media(&self) -> bool {
        matches!(
            self.0,
            Page::Local | Page::DownloadedBooks | Page::DownloadedPodcasts
        )
    }
    fn empty(&self) -> &'static str {
        match self.0 {
            Page::Local => "Copy M4B files into the\naudiobooks folder on the card",
            _ => "No books or downloaded episodes",
        }
    }
    fn load(self: std::sync::Arc<Self>, lib: std::sync::Arc<Library>, p: Policy) -> super::menu::RowsTask {
        if self.local() { return super::menu::ready(Ok(home_rows(if self.0 == Page::Books { Media::Book } else { Media::Podcast }))); }
        Box::pin(async move {
            match self.0 {
                Page::DownloadedBooks | Page::DownloadedPodcasts => {
                    let (namespace,generation)=lib.spoken_download_identity().await.map_err(LibError::Server)?;
                    let books = lib.downloaded_spoken_async().await.map_err(LibError::Server)?;
                    let queued = lib.spoken_download_queue().await.map_err(LibError::Server)?;
                    let verify=lib.clone();
                    let rows=super::menu::on_pool(lib.clone(), move |_| {
                        let books=books.into_iter().filter(|song| song_in_namespace(song,&namespace)).collect();
                        Ok::<_, LibError>(download_rows(books, queued, &self.0))
                    }).await?;
                    let rows = with_listening_progress(&lib, rows).await;
                    let current=crate::rt::spawn_blocking(move |_|verify.shelf_generation_current(&generation)).await.map_err(|_|LibError::Offline)?;
                    if !current {return Err(LibError::Offline);}
                    Ok(rows)
                }
                Page::Local => {
                    // Cache-only intentionally misses: Menu schedules the scan.
                    if p == Policy::CacheOnly { return Err(LibError::Offline); }
                    let (books, issues) = lib.local_audiobooks_async().await.map_err(LibError::Server)?;
                    let mut rows: Vec<_> = books.into_iter().map(|song| Row::new(song.title.clone(), RowAction::Cmd(1))
                        .key(song.id.clone()).art(song.cover_art.clone()).song(song)).collect();
                    rows.extend(issues.into_iter().map(|issue| Row::new(issue, RowAction::None)));
                    Ok(with_listening_progress(&lib, rows).await)
                }
                _ => unreachable!("home or paged screen"),
            }
        })
    }

    fn download_badges(&self) -> bool { false }
    fn reset_on(&self, event: &crate::library::LibEvent) -> bool {
        matches!(event, crate::library::LibEvent::SpokenChanged)
    }
    fn reload_on(&self, event: &crate::library::LibEvent) -> bool {
        self.reset_on(event)
            || (!self.local() && matches!(event, crate::library::LibEvent::SpokenProgress))
            || matches!(event, crate::library::LibEvent::Online(_))
            || (matches!(self.0, Page::DownloadedBooks | Page::DownloadedPodcasts)
                && matches!(event, crate::library::LibEvent::Downloads))
    }
    fn menu(&self, row: &Row, _ctx: &mut Ctx) -> Option<Box<dyn Screen>> {
        if matches!(self.0, Page::Books | Page::Podcasts) {
            return Some(super::spoken_browse::library_popup(
                if matches!(self.0, Page::Books) {
                    Media::Book
                } else {
                    Media::Podcast
                },
            ));
        }
        if !matches!(self.0, Page::Local | Page::DownloadedBooks | Page::DownloadedPodcasts) {
            return None;
        }
        let song = row.song_item().cloned()?;
        let episode=is_episode(&song);
        let logical = song.spoken.as_ref()?.id.clone();
        if matches!(row.action, RowAction::Cmd(2)) {
            let logical=logical.clone();
            return Some(Box::new(super::popup::Popup::new(row.title.clone()).add("Cancel Download", move |ctx| {
                let request=match ctx.lib.begin_remove_spoken_download(&logical) {Ok(request)=>request,Err(error)=>return Action::Toast(error)};
                let lib=ctx.lib.clone();
                ctx.notice_task(async move {match lib.remove_spoken_download_requested_async(request).await {Ok(())=>"Download removed".into(),Err(error)=>error}});
                Action::Pop
            })));
        }
        let resume=Selection::Local(song.clone());
        let resume_title=song.title.clone();
        let mut popup = super::popup::Popup::new(row.title.clone())
            .add("Resume", move |ctx| start_selection(resume.clone(),resume_title.clone(),ctx));
        if let Some(target) = BookInfoTarget::from_song(&song) {
            let title = row.title.clone();
            popup = popup.add(if episode {"Episode Info"} else {"Book Info"}, move |ctx| {
                Action::Push(super::open(Dest::BookInfo { target: target.clone(), title: title.clone() }, ctx))
            });
        }
        if let Some(target) = PlayedTarget::song(&song) {
            popup = played_menu(popup, target, row.spoken_progress);
        }
        if matches!(self.0, Page::DownloadedBooks | Page::DownloadedPodcasts) {
            popup = popup.add("Remove Download", move |ctx| {
                let lib = ctx.lib.clone();
                let request=match lib.begin_remove_spoken_download(&logical) {Ok(request)=>request,Err(error)=>return Action::Toast(error)};
                ctx.notice_task(async move {match lib.remove_spoken_download_requested_async(request).await {
                    Ok(()) => "Download removed".into(),
                    Err(error) => error,
                }});
                Action::Pop
            });
        }
        Some(Box::new(popup))
    }
    fn cmd(&self, cmd: u32, row: &Row, ctx: &mut Ctx) -> Action {
        match &self.0 {
            Page::Local | Page::DownloadedBooks | Page::DownloadedPodcasts if cmd == 2 => Action::Toast(row.detail.clone().unwrap_or_else(||"Waiting to download".into())),
            Page::Local | Page::DownloadedBooks | Page::DownloadedPodcasts => row
                .song_item()
                .cloned()
                .map(|song| {
                    let target=BookInfoTarget::from_song(&song);
                    if !is_episode(&song) {
                        if let Some(target)=target {
                            return Action::Push(Box::new(super::book_info::BookInfoScreen::preview(target,Selection::Local(song))));
                        }
                    }
                    let title=song.title.clone();
                    start_selection(Selection::Local(song),title,ctx)
                })
                .unwrap_or(Action::None),
            _ => Action::None,
        }
    }
}
async fn with_listening_progress(lib: &Library, mut rows: Vec<Row>) -> Vec<Row> {
    let entries = rows.iter().filter_map(|row| {
        let spoken = row.song_item()?.spoken.as_ref()?;
        Some((spoken.id.clone(), spoken.duration_ms))
    }).collect();
    let progress = lib.spoken_percentages(entries).await;
    for row in &mut rows {
        if let Some(key) = row.song_item().and_then(|song| song.spoken.as_ref()).map(|spoken| &spoken.id) {
            row.spoken_progress = progress.get(key).copied().unwrap_or_default();
        }
    }
    rows
}

pub(crate) fn played_menu(popup: super::popup::Popup, target: PlayedTarget, percent: u8) -> super::popup::Popup {
    let played = percent < 100;
    popup.add(if played { "Mark Played" } else { "Mark Unplayed" }, move |ctx| {
        let target = target.clone();
        let lib = ctx.lib.clone();
        ctx.notice_task(async move {
            match lib.mark_spoken_played(target, played).await {
                Ok(()) if played => "Marked played".into(),
                Ok(()) => "Marked unplayed".into(),
                Err(error) => error,
            }
        });
        Action::Pop
    })
}

fn home_rows(media: Media) -> Vec<Row> {
    let entries = match media {
        Media::Book => vec![
            ("Continue Listening", Section::ContinueListening),
            ("Continue Series", Section::ContinueSeries),
            ("Recently Added", Section::RecentlyAdded),
            ("Recent Series", Section::RecentSeries),
            ("Search Books", Section::Search(String::new())),
            ("Discover", Section::Discover),
            ("All Books", Section::Books),
            ("Genres", Section::Genres),
            ("Series", Section::Series),
            ("Collections", Section::Collections),
            ("Playlists", Section::Playlists),
            ("Authors", Section::Authors),
            ("Narrators", Section::Narrators),
        ],
        Media::Podcast => vec![
            ("Continue Listening", Section::ContinueListening),
            ("Latest Episodes", Section::LatestEpisodes),
            ("Listen Again", Section::ListenAgain),
            ("Library", Section::Books),
            ("Playlists", Section::Playlists),
        ],
    };
    let mut rows: Vec<_> = entries
        .into_iter()
        .map(|(title, section)| {
            Row::link(
                title,
                Dest::Spoken(Page::Libraries {
                    media,
                    section,
                    title: title.into(),
                }),
            )
        })
        .collect();
    if media == Media::Book {
        if let Some(books) = rows.iter_mut().find(|row| row.title == "All Books") {
            books.separator = true;
        }
    }
    if media == Media::Book {
        rows.push(Row::link("On This Card", Dest::Spoken(Page::Local)));
    }
    let (title, page) = if media == Media::Book {
        ("Downloads", Page::DownloadedBooks)
    } else {
        ("Downloads", Page::DownloadedPodcasts)
    };
    rows.push(Row::link(title, Dest::Spoken(page)));
    rows
}

pub(crate) fn is_episode(song:&Song)->bool {
    song.spoken.as_ref().is_some_and(|book|matches!(book.source,subsonic::SpokenSource::Audiobookshelf {episode:Some(_),..}))
}

fn download_matches(song: &Song, page: &Page) -> bool {
    let episode=is_episode(song);
    match page {
        Page::DownloadedBooks => !episode,
        Page::DownloadedPodcasts => episode,
        _ => true,
    }
}

#[derive(Clone)]
pub(crate) enum Selection {
    Local(Song),
    Shelf {
        item: String,
        episode: Option<String>,
    },
}
impl Selection {
    pub(crate) fn matches(&self,current:&Song,namespace_current:impl Fn(&str)->bool)->bool {
        let Some(book)=current.spoken.as_ref() else {return false;};
        match self {
            Self::Local(song)=>song.spoken.as_ref().is_some_and(|selected| {
                selected.id==book.id && BookInfoTarget::from_song(song).is_some()
                    && BookInfoTarget::from_song(song)==BookInfoTarget::from_song(current)
                    && match &book.source {
                        subsonic::SpokenSource::Audiobookshelf {namespace,..}=>namespace_current(namespace),
                        _=>true,
                    }
            }),
            Self::Shelf {item,episode}=>matches!(&book.source,subsonic::SpokenSource::Audiobookshelf {
                namespace,item:parent,episode:actual,..} if namespace_current(namespace) && parent==item && actual==episode),
        }
    }
}
/// Resume a matching retained instance without replacing its queue or opening a
/// second server session. The player validates generation/state again atomically.
pub(crate) fn start_selection(selection:Selection,title:String,ctx:&Ctx)->Action { select(selection,title,ctx,true) }

/// Play from an info page (UXI-020): the item already playing keeps playing and
/// the page stays; a paused one resumes in place. Another item opens as usual.
pub(crate) fn play_here(selection:Selection,title:String,ctx:&Ctx)->Action { select(selection,title,ctx,false) }

fn select(selection:Selection,title:String,ctx:&Ctx,navigate:bool)->Action {
    if let Selection::Local(song)=&selection {
        if let Some(subsonic::Spoken {source:subsonic::SpokenSource::Audiobookshelf {namespace,..},..})=song.spoken.as_deref() {
            if !ctx.lib.shelf_namespace_current(namespace) {return Action::Toast("Audiobookshelf changed; select the item again".into());}
        }
    }
    let status=ctx.player.status();
    if status.state!=crate::audio::player::PlayState::Stopped && status.current().is_some_and(|song|selection.matches(song,|namespace|ctx.lib.shelf_namespace_current(namespace))) {
        if !navigate {
            if status.state!=crate::audio::player::PlayState::Paused { return Action::Stay; }
            let song=status.current().unwrap().clone();
            if matches!(song.spoken.as_ref().map(|book| &book.source), Some(subsonic::SpokenSource::Audiobookshelf { .. })) && !ctx.lib.offline_mode() {
                return resume_in_place(song,status.generation,status.seek_revision);
            }
            ctx.player.send(crate::audio::player::Cmd::Resume {generation:status.generation});
            return Action::Stay;
        }
        if status.state==crate::audio::player::PlayState::Paused {
            let song = status.current().unwrap();
            if matches!(song.spoken.as_ref().map(|book| &book.source), Some(subsonic::SpokenSource::Audiobookshelf { .. })) && !ctx.lib.offline_mode() {
                let mut opening = Opening::local(song.clone());
                opening.retained = Some((status.generation, status.seek_revision));
                return Action::Push(Box::new(opening));
            }
            ctx.player.send(crate::audio::player::Cmd::Resume {generation:status.generation});
        }
        return Action::NowPlaying;
    }
    let opening=match selection {
        Selection::Local(song)=>Opening::local(song),
        Selection::Shelf {item,episode}=>Opening::shelf(item,episode,title),
    };
    Action::Push(Box::new(opening))
}
/// The transport button resumes in place; selecting a book explicitly opens
/// Now Playing. A remote retained resume still reconciles server progress first.
pub(crate) fn resume_in_place(song: Song, generation: u64, seek_revision: u64) -> Action {
    let mut opening = Opening::local(song);
    opening.retained = Some((generation, seek_revision));
    opening.show_now_playing = false;
    Action::Push(Box::new(opening))
}
pub(crate) struct Opened {
    pub(crate) songs: Result<Vec<Song>, String>,
    pub(crate) configuration: Option<Arc<()>>,
}
pub struct Opening {
    title: String,
    selection: Option<Selection>,
    message: String,
    action: Option<Action>,
    remote: bool,
    cancelled: bool,
    generation: u64,
    failed: bool,
    retry_target: Option<Rect>,
    retained: Option<(u64, u64)>,
    show_now_playing: bool,
    /// The account this open began under, when the attempt started, and how
    /// many times a same-account republish restarted it.
    namespace: String,
    started: Option<std::time::Instant>,
    restarts: u8,
}
/// An open with no answer after this offers a retry (UX-006). Longer than the
/// 15 s page deadline: an open is a session plus a position fetch, each with
/// its own 15 s request timeout, and a late answer here is discarded.
const OPENING_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);
impl Opening {
    pub(crate) fn local(song: Song) -> Self {
        let remote=BookInfoTarget::from_song(&song).is_some_and(|target|target.remote());
        Self {
            title: song.title.clone(),
            selection: Some(Selection::Local(song)),
            message: "Opening audiobook…".into(),
            action: None,
            remote,
            cancelled: false,
            generation: 0,
            failed: false,
            retry_target: None,
            retained: None,
            show_now_playing: true,
            namespace: String::new(),
            started: None,
            restarts: 0,
        }
    }
    pub(crate) fn shelf(item: String, episode: Option<String>, title: String) -> Self {
        Self {
            title,
            selection: Some(Selection::Shelf { item, episode }),
            message: "Opening Audiobookshelf…".into(),
            action: None,
            remote: true,
            cancelled: false,
            generation: 0,
            failed: false,
            retry_target: None,
            retained: None,
            show_now_playing: true,
            namespace: String::new(),
            started: None,
            restarts: 0,
        }
    }
}
impl Screen for Opening {
    fn clickable(&self, x: i32, y: i32) -> bool {
        self.failed && !self.cancelled && self.retry_target.is_some_and(|target| super::hits(target, x, y))
    }
    fn click(&mut self, x: i32, y: i32, ctx: &mut Ctx) -> Action {
        if self.clickable(x, y) { self.event(Event::Center, ctx) } else { Action::None }
    }

    fn opening_spoken(&self) -> bool { true }
    fn opening_spoken_in_place(&self) -> bool { !self.show_now_playing }
    fn script_ready(&self) -> bool { self.failed || self.cancelled }
    #[cfg(test)]
    fn audit(&self) -> crate::ui::hierarchy::Surface { crate::ui::hierarchy::Surface::widget("Opening", format!("failed={};cancelled={}", self.failed, self.cancelled)) }
    fn title(&self) -> String {
        self.title.clone()
    }
    fn enter(&mut self, ctx: &mut Ctx) {
        if self.generation > 0 {
            return;
        }
        self.start(ctx);
    }
    fn event(&mut self, ev: Event, ctx: &mut Ctx) -> Action {
        match ev {
            Event::Back => {
                self.cancelled = true;
                Action::None
            }
            Event::Center if self.failed && !self.cancelled => {
                self.start(ctx);
                Action::Stay
            }
            _ => Action::None,
        }
    }
    fn tick(&mut self, ctx: &mut Ctx) -> bool {
        if self.failed || self.cancelled || self.action.is_some() { return false; }
        if self.started.is_some_and(|at| ctx.now.saturating_duration_since(at) >= OPENING_DEADLINE) {
            self.failed = true;
            self.generation = self.generation.wrapping_add(1); // a late answer is ignored
            self.message = crate::ux::Code::Stuck.message().into();
            crate::ux::fail(crate::ux::Code::Stuck, crate::ux::Journey::PlaySpoken, 0);
            return true;
        }
        false
    }
    fn refresh(&mut self, event: &crate::library::LibEvent, ctx: &mut Ctx) {
        // A republish for the same account (startup, reconnect) is not a change.
        if matches!(event, crate::library::LibEvent::SpokenChanged) {
            let current = ctx.lib.shelf_namespace();
            if current.is_empty() || current == self.namespace { return; }
        }
        if self.remote && matches!(event, crate::library::LibEvent::SpokenChanged) {
            self.cancelled = true;
            self.message = "Audiobookshelf changed\nReturn and select the item again".into();
        }
    }
    fn loaded(&mut self, token: u64, data: Box<dyn std::any::Any + Send>, ctx: &mut Ctx) {
        if self.cancelled || token != self.generation {
            return;
        }
        if let Ok(result) = data.downcast::<Opened>() {
            if result
                .configuration
                .as_ref()
                .is_some_and(|generation| !ctx.lib.shelf_generation_current(generation))
            {
                // Same account, newer configuration: open again, at most twice.
                let current = ctx.lib.shelf_namespace();
                if self.restarts < 2 && !current.is_empty() && current == self.namespace {
                    self.restarts += 1;
                    self.start(ctx);
                    return;
                }
                self.cancelled = true;
                self.message = "Audiobookshelf changed\nReturn and select the item again".into();
                return;
            }
            match result.songs {
                Ok(songs) => {
                    if songs.is_empty() {
                        self.failed = true;
                        self.message = "No playable audio".into();
                        return;
                    }
                    let account_current=songs.iter().all(|song|match song.spoken.as_deref() {
                        Some(subsonic::Spoken {source:subsonic::SpokenSource::Audiobookshelf {namespace,..},..})=>ctx.lib.shelf_namespace_current(namespace),
                        _=>true,
                    });
                    let session_current=!matches!(self.selection,Some(Selection::Shelf {..}))
                        || ctx.lib.shelf_queue_current(&songs);
                    if self.remote && (!account_current || !session_current) {
                        self.message =
                            "Audiobookshelf changed\nReturn and select the item again".into();
                        self.cancelled = true;
                        return;
                    }
                    if let Some((generation, seek_revision)) = self.retained {
                        ctx.player.send(crate::audio::player::Cmd::ResumeSpoken { generation, seek_revision });
                    } else { ctx.play(songs, 0, false); }
                    self.action = Some(Action::Many(vec![
                        Action::Dismiss(ctx.screen),
                        if self.show_now_playing { Action::NowPlaying } else { Action::Stay },
                    ]));
                }
                Err(error) => {
                    self.failed = true;
                    self.message = crate::ux::present_in(&error, crate::ux::Journey::PlaySpoken);
                    crate::ux::end(crate::ux::Journey::PlaySpoken, crate::ux::Outcome::Failed);
                }
            }
        }
    }
    fn completion(&mut self) -> Option<Action> {
        self.action.take()
    }
    fn draw(&mut self, d: &mut Draw, area: Rect) {
        // Failures offer Retry on Center; a change of account sends the listener back.
        if self.failed && !self.cancelled {
            self.retry_target = crate::ui::state::draw_target(d, area,
                crate::ui::state::View::Failed { text: &self.message, retry: true });
        } else {
            self.retry_target = None;
            d.message(area, &self.message);
        }
    }
}
impl Opening {
    fn start(&mut self, ctx: &mut Ctx) {
        let Some(selection) = self.selection.clone() else {
            return;
        };
        let lib = ctx.lib.clone();
        let configuration = self.remote.then(|| lib.shelf_generation());
        self.generation = self.generation.wrapping_add(1);
        self.failed = false;
        self.message = "Opening…".into();
        self.started = Some(ctx.now);
        if self.namespace.is_empty() { self.namespace = ctx.lib.shelf_namespace(); }
        crate::ux::begin(crate::ux::Journey::PlaySpoken);
        // A user-started play never waits behind page loads.
        ctx.foreground_task(self.generation, move |reply| async move {
            let songs = match selection {
                Selection::Local(song) => match song.spoken {
                    Some(book) => lib.spoken_queue_for_playback_async(book.id).await,
                    None => Err("Audiobook identity missing".into()),
                },
                Selection::Shelf { item, episode } => lib.shelf_open_async(item, episode).await,
            };
            reply.send(Opened {
                songs,
                configuration,
            });
        });
    }
}

pub fn playback_menu(ctx: &Ctx) -> super::popup::Popup {
    let episode=ctx.status.current().is_some_and(is_episode);
    let mut popup = super::popup::Popup::new(if episode {"Podcast Episode"} else {"Audiobook"})
        .add("Playback Speed", |ctx| Action::Push(Box::new(SharedPlaybackSpeed(ctx.spoken_speed.clone()))))
        .add("Sleep Timer", |ctx| Action::Push(super::settings::sleep_timer_choice(ctx)))
        .add("Brightness", |ctx| {
            Action::Push(super::open(
                Dest::SettingsPage(super::settings::Page::Brightness),
                ctx,
            ))
        })
        .add("Equalizer", |ctx| {
            Action::Push(super::open(Dest::Equalizer, ctx))
        });
    if let Some(song) = ctx.status.current() {
        if let Some(target) = BookInfoTarget::from_song(song) {
            let title = song.title.clone();
            popup = popup.add(if episode {"Episode Info"} else {"Book Info"}, move |ctx| {
                Action::Push(super::open(Dest::BookInfo { target: target.clone(), title: title.clone() }, ctx))
            });
        }
    }
    if let Some(book) = ctx.status.current().and_then(|s| s.spoken.as_ref()) {
        let chapters = book.chapters.clone();
        let generation = ctx.status.generation;
        if !chapters.is_empty() {
            popup = popup.add("Chapters", move |_| {
                let mut list = super::popup::Popup::new("Chapters");
                for chapter in &chapters {
                    let position = chapter.start_ms;
                    list = list.add(&chapter.title, move |ctx| {
                        ctx.player.send(crate::audio::player::Cmd::Scrub {
                            generation,
                            position: Some(position),
                            active: false,
                        });
                        Action::Pop
                    });
                }
                Action::Push(Box::new(list))
            });
        }
    }
    popup.report_problem()
}

/// Speed controls apply live. The listening controller owns one playback
/// generation; Settings changes only the default used by uncustomized titles.
pub struct PlaybackSpeed {
    setting_default: bool,
    bar: Rect,
    confirm: Rect,
    generation: u64,
    percent: u16,
    custom: bool,
    podcast: bool,
    request: u64,
    pending: Option<u64>,
    error: Option<String>,
}
impl PlaybackSpeed {
    pub fn default_setting() -> Self { Self { setting_default: true, ..Self::current() } }
    pub(crate) fn current() -> Self { Self { setting_default: false, bar: Rect::default(), confirm: Rect::default(), generation: 0, percent: 100, custom: false, podcast: false, request: 0, pending: None, error: None } }
    fn stale(&self, status: &crate::audio::player::Status) -> bool {
        !self.setting_default && (status.generation != self.generation || status.current().is_none_or(|song| song.spoken.is_none()))
    }
}
/// Every spoken speed view shares one app-owned controller and request sequence.
/// Pending wheel edits therefore survive inline/menu transitions and re-entry.
pub struct SharedPlaybackSpeed(pub std::rc::Rc<std::cell::RefCell<PlaybackSpeed>>);
impl Screen for SharedPlaybackSpeed {
    fn clickable(&self, x: i32, y: i32) -> bool { self.0.borrow().clickable(x, y) }
    fn click(&mut self, x: i32, y: i32, ctx: &mut Ctx) -> Action { self.0.borrow_mut().click(x, y, ctx) }

    #[cfg(test)]
    fn audit(&self) -> crate::ui::hierarchy::Surface { self.0.borrow().audit() }
    fn title(&self) -> String { "Playback Speed".into() }
    fn enter(&mut self, ctx: &mut Ctx) { self.0.borrow_mut().enter(ctx); }
    fn event(&mut self, event: Event, ctx: &mut Ctx) -> Action { self.0.borrow_mut().event(event, ctx) }
    fn tick(&mut self, ctx: &mut Ctx) -> bool { self.0.borrow_mut().tick(ctx) }
    fn draw(&mut self, d: &mut Draw, area: Rect) { self.0.borrow_mut().draw(d, area); }
}
impl PlaybackSpeed {
    pub(super) fn draw_inline(&self, d: &mut Draw, bar: Rect) {
        speed_view::draw_inline(d, bar, self.percent, self.custom, self.error.as_deref());
    }
    #[cfg(test)]
    pub(crate) fn request_id(&self) -> u64 { self.request }
    #[cfg(test)]
    pub(crate) fn selected_percent(&self) -> u16 { self.percent }
}

impl Screen for PlaybackSpeed {
    fn clickable(&self, x: i32, y: i32) -> bool {
        super::hits(self.bar, x, y) || super::hits(self.confirm, x, y)
    }

    fn click(&mut self, x: i32, y: i32, ctx: &mut Ctx) -> Action {
        if super::hits(self.confirm, x, y) {
            return self.event(Event::Center, ctx);
        }
        if self.stale(ctx.status) || !super::hits(self.bar, x, y) {
            return Action::None;
        }
        let steps = i32::from((speed_view::MAX - speed_view::MIN) / speed_view::STEP);
        let percent = speed_view::adjust(speed_view::MIN, super::slider_step(self.bar, x, steps));
        let ticks = (i32::from(percent) - i32::from(self.percent)) / i32::from(speed_view::STEP);
        self.event(Event::Scroll(ticks), ctx)
    }

    #[cfg(test)]
    fn audit(&self) -> crate::ui::hierarchy::Surface { crate::ui::hierarchy::Surface::widget("PlaybackSpeed", format!("percent={};default={};custom={};podcast={};request={};pending={:?};error={:?}", self.percent, self.setting_default, self.custom, self.podcast, self.request, self.pending, self.error)).contextual() }
    fn title(&self) -> String { if self.setting_default { "Default Speed" } else { "Playback Speed" }.into() }
    fn enter(&mut self, ctx: &mut Ctx) {
        let changed = self.generation != ctx.status.generation;
        self.generation = ctx.status.generation;
        self.request = self.request.max(ctx.status.speed_request);
        if changed { self.pending = None; self.error = None; }
        if self.setting_default {
            self.percent = ctx.config.spoken.speed_percent;
        } else if self.pending.is_none() {
            self.percent = ctx.status.speed_percent;
            self.custom = ctx.status.speed_override.is_some();
        } else {
            self.tick(ctx);
        }
        self.podcast = ctx.status.current().and_then(|song| song.spoken.as_ref()).is_some_and(|spoken|
            matches!(spoken.source, subsonic::SpokenSource::Audiobookshelf { episode: Some(_), .. }));
    }
    fn event(&mut self, event: Event, ctx: &mut Ctx) -> Action {
        if matches!(event, Event::Center | Event::Back) { return Action::Pop; }
        if self.stale(ctx.status) {
            return Action::Many(vec![Action::Pop, Action::Toast("Playback changed; open Playback Speed again".into())]);
        }
        match event {
            Event::Scroll(ticks) => {
                let next = speed_view::adjust(self.percent, ticks);
                if next != self.percent {
                    self.percent = next;
                    if self.setting_default {
                        ctx.config.spoken.speed_percent = next;
                        *ctx.config_changed = true;
                    } else {
                        self.custom = true;
                        self.request = self.request.wrapping_add(1);
                        self.pending = Some(self.request);
                        self.error = None;
                        ctx.player.send(crate::audio::player::Cmd::SetSpeed { generation: self.generation, percent: Some(next), request: self.request });
                    }
                }
                Action::Stay
            }
            Event::CenterLong if !self.setting_default => {
                self.percent = ctx.config.spoken.speed_percent;
                self.custom = false;
                self.request = self.request.wrapping_add(1);
                self.pending = Some(self.request);
                self.error = None;
                ctx.player.send(crate::audio::player::Cmd::SetSpeed { generation: self.generation, percent: None, request: self.request });
                Action::Stay
            }
            _ => Action::None,
        }
    }
    fn tick(&mut self, ctx: &mut Ctx) -> bool {
        if self.setting_default || self.stale(ctx.status) { return false; }
        if self.pending.is_some_and(|request| ctx.status.speed_request < request) { return false; }
        let acknowledged = self.pending.take().is_some();
        let percent = ctx.status.speed_percent;
        let custom = ctx.status.speed_override.is_some();
        let error = if acknowledged { ctx.status.error.clone() } else { self.error.clone() };
        let changed = self.percent != percent || self.custom != custom || self.error != error;
        self.percent = percent;
        self.custom = custom;
        self.error = error;
        changed
    }
    fn draw(&mut self, d: &mut Draw, area: Rect) {
        if self.stale(d.status) {
            self.bar = Rect::default();
            self.confirm = Rect::default();
            d.message(area, "Playback changed
Return and open Playback Speed again");
            return;
        }
        let caption = speed_view::caption(self.setting_default, self.custom, self.podcast);
        speed_view::draw(d, area, self.percent, caption, self.setting_default, self.error.as_deref());
        self.bar = speed_view::bar(area);
        self.confirm = super::center_target(d, "Wheel: adjust · Center: done", area.x + area.w / 2, area.y + 186);
    }
}

fn song_in_namespace(song:&Song, namespace:&str)->bool {
    namespace.is_empty() || song.spoken.as_ref().is_some_and(|book| matches!(&book.source,
        subsonic::SpokenSource::Audiobookshelf {namespace:account,..} if account==namespace))
}
fn download_rows(books: Vec<Song>, queue: Vec<crate::library::SpokenQueueRow>, page: &Page) -> Vec<Row> {
    let mut pending:std::collections::HashMap<_,_>=queue.into_iter().map(|q|(q.logical.clone(),q)).collect();
    let mut rows:Vec<_>=books.into_iter().filter(|song| download_matches(song, page)).map(|song| {
        let logical=song.spoken.as_ref().map(|book|book.id.clone()).unwrap_or_else(||song.id.clone());
        let queued=pending.remove(&logical);
        let mut row=Row::new(song.title.clone(), RowAction::Cmd(1)).key(logical).art(song.cover_art.clone()).song(song);
        if let Some(q)=queued { let (badge,state)=spoken_queue_status(&q);row=row.badge(badge).detail(state); }
        else { row=row.badge(Badge::Offline).detail("Downloaded"); }
        row
    }).collect();
    for (_,q) in pending {
        if (q.episode.is_some()) != matches!(page,Page::DownloadedPodcasts) {continue;}
        let (badge,state)=spoken_queue_status(&q);
        let art=format!("shelf-book-cover:{}:{}",q.namespace,q.item);
        let song=Song {id:q.logical.clone(),title:q.title.clone(),cover_art:Some(art.clone()),spoken:Some(Box::new(subsonic::Spoken {
            id:q.logical.clone(),source:subsonic::SpokenSource::Audiobookshelf {namespace:q.namespace.clone(),item:q.item.clone(),episode:q.episode.clone(),content_url:String::new()},..Default::default()
        })),..Default::default()};
        rows.push(Row::new(q.title,RowAction::Cmd(2)).key(q.logical).art(Some(art)).badge(badge).detail(state).song(song));
    }
    rows.sort_by(|a,b|a.title.to_lowercase().cmp(&b.title.to_lowercase()).then_with(||a.key.cmp(&b.key)));
    rows
}
/// A queued audiobook's badge and detail; the detail carries the percent,
/// so progress reads in text as well as on the cover (UXI-008).
pub(crate) fn spoken_queue_status(q:&crate::library::SpokenQueueRow)->(Badge,String) {
    if q.failed {(Badge::Queued,"Failed · retry in Downloads".into())}
    else if q.paused {(Badge::Queued,"Paused".into())}
    else if let Some(f)=q.progress {
        let percent=f.map(|f|(f.clamp(0.0,1.0)*100.0).round() as u8);
        (Badge::Downloading(percent.unwrap_or(255)),percent.map_or_else(||"Downloading".into(),|p|format!("Downloading · {p}%")))
    }
    else {(Badge::Queued,"Queued".into())}
}

#[cfg(test)]
mod tests {
    use super::*;
    fn queued(logical:&str, episode:Option<&str>, progress:Option<Option<f32>>) -> crate::library::SpokenQueueRow {
        crate::library::SpokenQueueRow {logical:logical.into(),item:"item".into(),episode:episode.map(str::to_owned),title:logical.into(),namespace:"account".into(),failed:false,paused:false,progress}
    }
    #[test]
    fn spoken_queue_rows_are_immediate_account_bound_deduplicated_and_filter_media() {
        let mut book=queued("book",None,Some(Some(0.4)));let episode=queued("episode",Some("ep"),None);
        let rows=download_rows(Vec::new(),vec![book,episode],&Page::DownloadedBooks);
        assert_eq!(rows.len(),1);assert_eq!(rows[0].title,"book");assert!(matches!(rows[0].action,RowAction::Cmd(2)));
        assert!(matches!(rows[0].badge,Badge::Downloading(40)));
        assert_eq!(rows[0].detail.as_deref(),Some("Downloading · 40%"),"UXI-008: progress reads as text too");
        assert!(rows[0].art.as_deref().unwrap().contains("account:item"));
        book=queued("book",None,Some(Some(0.5)));
        let saved=Song {id:"part".into(),title:"saved".into(),spoken:Some(Box::new(subsonic::Spoken {id:"book".into(),source:subsonic::SpokenSource::Audiobookshelf {namespace:"account".into(),item:"item".into(),episode:None,content_url:"part".into()},..Default::default()})),..Default::default()};
        assert!(song_in_namespace(&saved,"account"));assert!(!song_in_namespace(&saved,"other-account"));
        let rows=download_rows(vec![saved],vec![book],&Page::DownloadedBooks);
        assert_eq!(rows.len(),1);assert!(matches!(rows[0].action,RowAction::Cmd(1)));assert!(matches!(rows[0].badge,Badge::Downloading(50)));
        let mut failed=queued("ep",Some("episode"),None);failed.failed=true;
        let rows=download_rows(Vec::new(),vec![failed],&Page::DownloadedPodcasts);
        assert_eq!(rows[0].detail.as_deref(),Some("Failed · retry in Downloads"));
        let mut paused=queued("paused",None,Some(Some(0.3)));paused.paused=true;
        assert_eq!(spoken_queue_status(&paused).1,"Paused");
    }
    #[test]
    fn retained_selection_matches_logical_book_episode_and_account_only() {
        for i in 0..512 {
            let namespace=format!("generated-account-{i}");
            let item=format!("generated-show-{i}");
            let episode=(i%2==0).then(||format!("episode-{i}"));
            let song=Song {spoken:Some(Box::new(subsonic::Spoken {id:format!("logical-{i}"),source:subsonic::SpokenSource::Audiobookshelf {namespace:namespace.clone(),item:item.clone(),episode:episode.clone(),content_url:"generated-part".into()},..Default::default()})),..Default::default()};
            let selected=Selection::Shelf {item:item.clone(),episode:episode.clone()};
            assert!(selected.matches(&song,|account|account==namespace));
            assert!(!selected.matches(&song,|_|false));
            assert!(!Selection::Shelf {item:format!("other-{i}"),episode:episode.clone()}.matches(&song,|_|true));
            assert!(!Selection::Shelf {item,episode:Some(format!("other-episode-{i}"))}.matches(&song,|_|true));
            let local=Song {spoken:Some(Box::new(subsonic::Spoken {id:format!("local-{i}"),source:subsonic::SpokenSource::Local {relative_path:format!("{i}.m4b")},..Default::default()})),..Default::default()};
            assert!(Selection::Local(local.clone()).matches(&local,|_|false));
            let mut other=local.clone();other.spoken.as_mut().unwrap().id.push('x');
            assert!(!Selection::Local(other).matches(&local,|_|true));
            let mut other=local.clone();other.spoken.as_mut().unwrap().source=subsonic::SpokenSource::Local {relative_path:"another.m4b".into()};
            assert!(!Selection::Local(other).matches(&local,|_|true));
        }
    }
    #[test]
    fn transport_resume_reconciles_then_returns_to_the_browsing_screen() {
        use crate::audio::player::{Cmd, PlayerHandle, Status};
        let mut app = crate::ui::settings_tests::fixture();
        app.player.shutdown(); app.background = false;
        let song = Song { id: "generated-resume".into(), spoken: Some(Box::new(subsonic::Spoken {
            id: "generated-logical".into(), source: subsonic::SpokenSource::Local { relative_path: "generated.m4b".into() },
            ..Default::default()
        })), ..Default::default() };
        let (player, commands) = PlayerHandle::fixture(Status::default()); app.player = player;
        let parent = app.top_id();
        let action = resume_in_place(song.clone(), 17, 2); app.apply(action);
        let opening = app.top_id(); assert_ne!(opening, parent);
        app.handle(crate::ui::UiMsg::Loaded {screen: opening, token: 1, data: Box::new(Opened {songs: Ok(vec![song]), configuration: None})}, &mut crate::platform::Headless {last: None});
        assert_eq!(app.top_id(), parent, "transport resume preserves the current browsing context");
        assert!(!app.stack.iter().any(|(_, screen)|screen.is_now_playing()));
        assert!(matches!(commands.try_recv().unwrap(), Cmd::ResumeSpoken {generation: 17, seek_revision: 2}));
        assert!(commands.try_recv().is_err());
    }
    #[test]
    fn separate_media_homes_preserve_requested_abs_hierarchy_and_local_books() {
        let books = home_rows(Media::Book);
        assert_eq!(
            books
                .iter()
                .map(|row| row.title.as_str())
                .collect::<Vec<_>>(),
            [
                "Continue Listening",
                "Continue Series",
                "Recently Added",
                "Recent Series",
                "Search Books",
                "Discover",
                "All Books",
                "Genres",
                "Series",
                "Collections",
                "Playlists",
                "Authors",
                "Narrators",
                "On This Card",
                "Downloads",
            ]
        );
        let podcasts = home_rows(Media::Podcast);
        assert_eq!(
            podcasts
                .iter()
                .map(|row| row.title.as_str())
                .collect::<Vec<_>>(),
            [
                "Continue Listening",
                "Latest Episodes",
                "Listen Again",
                "Library",
                "Playlists",
                "Downloads",
            ]
        );
        for row in books.iter().chain(&podcasts) {
            assert!(!row.title.contains("Settings"));
            if let RowAction::Open(Dest::Spoken(Page::Libraries { media, .. })) = &row.action {
                assert_eq!(
                    *media,
                    if books.iter().any(|book| std::ptr::eq(book, row)) {
                        Media::Book
                    } else {
                        Media::Podcast
                    }
                );
            }
        }
    }
    #[test]
    fn downloaded_episodes_and_books_are_disjoint_despite_duplicate_titles() {
        let book = Song {
            title: "Same title".into(),
            spoken: Some(Box::new(subsonic::Spoken {
                source: subsonic::SpokenSource::Audiobookshelf {
                    namespace: "fixture".into(),
                    item: "book".into(),
                    episode: None,
                    content_url: String::new(),
                },
                ..Default::default()
            })),
            ..Default::default()
        };
        let mut podcast = book.clone();
        if let subsonic::SpokenSource::Audiobookshelf { episode, .. } =
            &mut podcast.spoken.as_mut().unwrap().source
        {
            *episode = Some("episode".into());
        }
        assert!(download_matches(&book, &Page::DownloadedBooks));
        assert!(!download_matches(&book, &Page::DownloadedPodcasts));
        assert!(!download_matches(&podcast, &Page::DownloadedBooks));
        assert!(download_matches(&podcast, &Page::DownloadedPodcasts));
    }
    #[test]
    fn remote_opening_does_not_assume_a_connection_route() {
        let opening = Opening::shelf("fixture".into(), None, "Fixture".into());
        assert!(!opening.message.contains("Tailscale"));
    }
}
