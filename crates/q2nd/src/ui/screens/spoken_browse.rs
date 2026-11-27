//! Bounded Audiobookshelf browsing: each page is a server request, never a
//! slice of an eagerly loaded library. Parent screens retain their selection.
use super::spoken::{Page, SpokenPage};
use crate::gfx::Rect;
use crate::input::Event;
use crate::library::shelf_browse::{BrowsePage, EntryKind, LibraryChoices, Media, Paging, Section};
use crate::library::{LibError, LibEvent, Policy};
use crate::library::spoken::PlayedTarget;
use crate::ui::list::{ListView, Row, RowAction};
use crate::ui::{Action, Ctx, Dest, Draw, Screen};
use crate::ui::paging::{Applied, Window};
use std::sync::Arc;
use std::time::{Duration, Instant};

const PAGE_SIZE: u32 = 50;
const RETRY: u32 = 12;
const OPEN: u32 = 13;
const CHOOSE_LIBRARY: u32 = 14;
const INFO: &str = "__abs_page_info";
const RETRY_KEY: &str = "__abs_retry";

/// What an empty Audiobookshelf page says.
fn empty_text(page: &Page) -> &'static str {
    let (Page::Libraries { media, section, .. } | Page::Browse { media, section, .. }) = page else { return "Nothing here" };
    let podcast = *media == Media::Podcast;
    match section {
        Section::Search(_) => "No matches",
        Section::ContinueListening | Section::ContinueSeries | Section::ListenAgain => "Nothing in progress",
        Section::Series | Section::RecentSeries => "No series",
        Section::Collections => "No collections",
        Section::Playlists => "No playlists",
        Section::Authors => "No authors",
        Section::Narrators => "No narrators",
        Section::Genres => "No genres",
        Section::LatestEpisodes | Section::NewestEpisodes | Section::Episodes(_) => "No episodes",
        _ if podcast => "No podcasts",
        _ => "No books",
    }
}

pub fn open(page: Page) -> Box<dyn Screen> {
    if let Page::Libraries {media, section: Section::Search(_), ..} = &page {
        let media = *media;
        return Box::new(super::text::TextEntry::new("Search Books", "", super::text::Kind::Full, false,
            Box::new(move |term, _ctx| {
                if term.trim().is_empty() { return Action::Pop; }
                Action::Replace(Box::new(Browser::new(Page::Libraries {
                    media, section: Section::Search(term.clone()), title: format!("Search: {term}"),
                })))
            })));
    }
    if matches!(page, Page::Libraries { .. } | Page::Browse { .. }) {
        Box::new(Browser::new(page))
    } else {
        super::menu::Menu::boxed(SpokenPage(page))
    }
}

struct Fetched {
    rows: Vec<Row>,
    page: Page,
    choices: Option<LibraryChoices>,
    notice: bool,
    more: bool,
}
struct Loaded {
    result: Result<Fetched, LibError>,
    configuration: Arc<()>,
    final_reply: bool,
}
struct Browser {
    page: Page,
    list: ListView,
    window: Window,
    entered: bool,
    loading: bool,
    invalidated: bool,
    error: Option<String>,
    last_scroll: Instant,
    warmed: Option<usize>,
    retrying: bool,
    choices: Option<LibraryChoices>,
    force_choice: bool,
    switch_root: bool,
    replace_after_choice: Option<u64>,
    choice_generation: Option<Arc<()>>,
    selection_save: Option<(u64, String, Media)>,
    selection_token: u64,
    completed: Option<Action>,
    refresh_network: bool,
    /// The Audiobookshelf account the shown rows belong to.
    namespace: Option<String>,
    /// The deadline of a load the listener waits on with nothing shown (UX-006).
    deadline: crate::ui::state::Deadline,
    stuck: bool,
    /// UXI-008: download state of the shown items, by (item, episode).
    downloads: Badges,
    listening: std::collections::HashMap<String, u8>,
    progress_pending: bool,
    progress_stale: bool,
}

/// Download badges for Audiobookshelf rows: queued and active items from the
/// queue (cheap, refreshed each second while one downloads) and downloaded
/// items (read again only after a download event).
#[derive(Default)]
struct Badges {
    queue: std::collections::HashMap<(String, Option<String>), crate::ui::list::Badge>,
    detail: std::collections::HashMap<(String, Option<String>), String>,
    downloaded: std::collections::HashSet<(String, Option<String>)>,
    pending: bool,
    stale: bool,
    full: bool,
    asked: Option<Instant>,
}
struct BadgeSnapshot {
    namespace: String,
    queue: Vec<crate::library::SpokenQueueRow>,
    downloaded: Option<Vec<(String, Option<String>)>>,
}
const BADGE_TOKEN: u64 = 1 << 60;
const PROGRESS_TOKEN: u64 = BADGE_TOKEN + 1;
struct ProgressSnapshot {
    generation: Arc<()>,
    percentages: std::collections::HashMap<String, u8>,
}
impl Browser {
    fn new(page: Page) -> Self {
        let list = ListView::new(matches!(page, Page::Browse { .. }));
        Self {
            page,
            list,
            window: Window::default(),
            entered: false,
            loading: true,
            invalidated: false,
            error: None,
            last_scroll: Instant::now(),
            warmed: None,
            retrying: false,
            choices: None,
            force_choice: false,
            switch_root: false,
            replace_after_choice: None,
            choice_generation: None,
            selection_save: None, selection_token: 0, completed: None,
            refresh_network: false,
            namespace: None,
            deadline: crate::ui::state::Deadline::new(Instant::now()),
            stuck: false,
            downloads: Badges { stale: true, full: true, ..Badges::default() },
            listening: Default::default(),
            progress_pending: false,
            progress_stale: true,
        }
    }
    fn switch(media: Media, root: bool, replace: Option<u64>) -> Self {
        let mut browser = Self::new(Page::Libraries {
            media,
            section: Section::Books,
            title: if media == Media::Book {
                "Books"
            } else {
                "Podcasts"
            }
            .into(),
        });
        browser.force_choice = true;
        browser.switch_root = root;
        browser.replace_after_choice = replace;
        browser
    }
    fn rebuild(&mut self) {
        let mut rows=self.window.rows();
        if rows.is_empty() && self.choices.is_some() && !self.loading && self.error.is_none() {
            rows.push(Row::new("No matching libraries",RowAction::None).key(INFO));
        }
        if let Some(error)=self.error.as_ref().filter(|_|self.window.retry_visible() || self.retrying) {
            rows.push(Row::new("Retry",if self.retrying {RowAction::None} else {RowAction::Cmd(RETRY)})
                .key(RETRY_KEY).detail(if self.retrying {"Loading…"} else {error}));
        }
        self.list.set_rows_anchored(rows);
        self.apply_badges();
    }
    /// Badges rows from the last snapshot; rows rebuilt from the window lose them.
    fn apply_badges(&mut self) {
        use crate::ui::list::Badge;
        for row in &mut self.list.rows {
            let Some(key) = decode_play_key(&row.key) else { continue };
            row.badge = if let Some(badge) = self.downloads.queue.get(&key) { *badge }
                else if self.downloads.downloaded.contains(&key) { Badge::Offline } else { Badge::None };
            if let Some(detail) = self.downloads.detail.get(&key) { row.detail = Some(detail.clone()); }
            if let Some(percent) = self.listening.get(&row.key) { row.spoken_progress = *percent; }
        }
    }
    fn request_progress(&mut self, ctx: &mut Ctx) {
        if self.progress_pending || !self.progress_stale { return; }
        let Some(namespace) = self.namespace.clone() else { return; };
        self.progress_stale = false;
        let entries: Vec<_> = self.list.rows.iter().filter_map(|row| {
            let (item, episode) = decode_play_key(&row.key)?;
            Some((row.key.clone(), PlayedTarget::shelf(namespace.clone(), item, episode).logical))
        }).collect();
        if entries.is_empty() { return; }
        self.progress_pending = true;
        let generation = ctx.lib.shelf_generation();
        let lib = ctx.lib.clone();
        ctx.task(PROGRESS_TOKEN, move |reply| async move {
            let values = lib.spoken_percentages(entries.iter().map(|(_, logical)| (logical.clone(), 0)).collect()).await;
            let percentages = entries.into_iter().filter_map(|(key, logical)| values.get(&logical).map(|percent| (key, *percent))).collect();
            reply.send(ProgressSnapshot { generation, percentages });
        });
    }
    fn request_badges(&mut self, ctx: &mut Ctx) {
        if self.downloads.pending || !matches!(self.page, Page::Browse { .. }) { return; }
        self.downloads.pending = true;
        self.downloads.asked = Some(ctx.now);
        let full = std::mem::take(&mut self.downloads.full);
        self.downloads.stale = false;
        let lib = ctx.lib.clone();
        ctx.task(BADGE_TOKEN, move |reply| async move {
            let namespace = lib.shelf_namespace();
            let queue = lib.spoken_download_queue().await.unwrap_or_default();
            let downloaded = if full {
                Some(lib.downloaded_spoken_async().await.unwrap_or_default().into_iter().filter_map(|song| match song.spoken?.source {
                    subsonic::SpokenSource::Audiobookshelf { namespace: account, item, episode, .. } if account == namespace => Some((item, episode)),
                    _ => None,
                }).collect())
            } else { None };
            reply.send(BadgeSnapshot { namespace, queue, downloaded });
        });
    }
    fn begin_load(&mut self, page: u32) -> Option<u64> {
        let request=self.window.begin(page)?;
        self.loading=true;
        self.stuck=false;
        self.retrying=self.error.is_some() && self.window.failed()==Some(page);
        self.warmed=None;
        self.rebuild();
        Some(request.token)
    }
    fn load(&mut self, page: u32, ctx: &mut Ctx) {
        let Some(token)=self.begin_load(page) else {return;};
        if self.waiting() { self.deadline.restart(ctx.now); }
        let query=self.page.clone();
        let lib=ctx.lib.clone();
        let configuration=lib.shelf_generation();
        let saved=page_media(&query).and_then(|media|ctx.config.audiobookshelf
            .library_preference(media==Media::Podcast)).cloned();
        let force_choice=self.force_choice;
        let refresh_network=self.refresh_network;
        let stable=stable_page(&query);
        // UXI-019: the first page the listener waits on is not queued behind background loads.
        let waiting=self.list.rows.is_empty();
        ctx.page_task(token,waiting,move|reply| async move {
            let fetch=|policy|fetch_rows(&lib,&query,page,policy,saved.as_ref(),force_choice);
            let cached=fetch(Policy::CacheOnly).await;
            if lib.offline_mode() {
                reply.send(Loaded {result:cached,configuration,final_reply:true});
                return;
            }
            if let Ok(result)=cached {
                if stable && !refresh_network {
                    reply.send(Loaded {result:Ok(result),configuration,final_reply:true});
                    return;
                }
                reply.send(Loaded {result:Ok(result),configuration:configuration.clone(),final_reply:false});
            }
            reply.send(Loaded {result:fetch(Policy::NetworkOnly).await,configuration,final_reply:true});
        });
    }
    fn apply(&mut self, token:u64, rows:Result<Vec<Row>,LibError>, more:bool, final_reply:bool) {
        if self.invalidated {return;}
        match rows {
            Ok(rows)=> {
                let anchor=self.list.selected_row().map(|row|row.key.clone());
                match self.window.apply(token,rows,more,final_reply,anchor.as_deref()) {
                    Applied::Stale=>return,
                    Applied::NoProgress=>self.error=Some("The server repeated this list; try again".into()),
                    Applied::Content=> {
                        if self.window.failed().is_none() {self.error=None;}
                    },
                }
            },
            Err(error)=> {
                if !self.window.fail(token) {return;}
                // Plain words on screen, the fixed code in the report (UX-006).
                self.error=Some(crate::ux::present_in(&error.to_string(),crate::ux::Journey::OpenPage));
            },
        }
        self.loading=self.window.pending().is_some();
        self.stuck=false;
        if final_reply {self.retrying=false;}
        self.warmed=None;
        self.progress_stale = true;
        self.rebuild();
    }
    /// The listener waits on this load with nothing to use: no rows, or only Retry.
    fn waiting(&self) -> bool {
        self.loading && !self.invalidated && self.selection_save.is_none()
            && self.list.rows.iter().all(|row| row.key == RETRY_KEY)
    }
    /// The shared page state in place of the list, if the page has no content.
    fn view(&self) -> Option<crate::ui::state::View<'_>> {
        use crate::ui::state::View;
        if self.stuck { return Some(View::Failed { text: crate::ux::Code::Stuck.message(), retry: true }); }
        if self.waiting() { return Some(View::Loading); }
        match self.list.rows.as_slice() {
            [] if !self.loading => Some(View::Empty(empty_text(&self.page))),
            [row] if row.key == RETRY_KEY => Some(View::Failed { text: row.detail.as_deref().unwrap_or(crate::ux::Code::Unknown.message()), retry: true }),
            [row] if row.key == INFO => Some(View::Empty("No matching libraries")),
            _ => None,
        }
    }
    /// Retry after the deadline: the waiting request is abandoned and the page starts again.
    fn restart(&mut self, ctx: &mut Ctx) {
        self.window.reset();
        self.error=None;self.retrying=false;self.stuck=false;
        self.list.set_rows(Vec::new());
        crate::ux::begin(crate::ux::Journey::OpenPage);
        self.load(0, ctx);
    }
    fn cancel(&mut self) {
        self.window.reset();self.invalidated=true;self.loading=false;
        self.error=None;self.retrying=false;self.list.set_rows(Vec::new());
    }
    fn choose(&mut self, ctx: &mut Ctx) -> Action {
        if self.invalidated {
            return Action::Pop;
        }
        let Some(row) = self.list.selected_row().cloned() else {
            return Action::None;
        };
        if matches!(self.page,Page::Browse {..}) && !self.choice_generation.as_ref().is_some_and(|generation|ctx.lib.shelf_generation_current(generation)) {
            return Action::Toast("Audiobookshelf changed; select the item again".into());
        }
        match row.action {
            RowAction::Open(dest) => Action::Push(super::open(dest, ctx)),
            RowAction::Cmd(RETRY) => {
                if let Some(page)=self.window.failed() {self.load(page,ctx);}
                Action::Stay
            }
            RowAction::Cmd(OPEN) => {
                // The action's stable identity is carried separately from its title.
                let Some((item, episode)) = decode_play_key(&row.key) else {
                    return Action::None;
                };
                if episode.is_none() {
                    let library=match &self.page {Page::Browse {library,..}=>Some(library.clone()),_=>None};
                    return match ctx.lib.shelf_info_target(library,item.clone(),None) {
                        Ok(target)=>Action::Push(Box::new(super::book_info::BookInfoScreen::preview(target,super::spoken::Selection::Shelf {item,episode:None}))),
                        Err(error)=>Action::Toast(error.to_string()),
                    };
                }
                super::spoken::start_selection(super::spoken::Selection::Shelf {item,episode},row.title,ctx)
            }
            RowAction::Cmd(CHOOSE_LIBRARY) => {
                let Some(choices) = self.choices.as_ref() else {
                    return Action::None;
                };
                if !self
                    .choice_generation
                    .as_ref()
                    .is_some_and(|generation| ctx.lib.shelf_generation_current(generation))
                    || !ctx.lib.shelf_namespace_current(&choices.namespace)
                {
                    return Action::Toast("Audiobookshelf changed; choose a library again".into());
                }
                let Some(preference) = choices.preference(&row.key) else {
                    return Action::None;
                };
                if self.selection_save.is_some() { return Action::Stay; }
                let media = choices.media;
                self.selection_token = self.selection_token.checked_add(1).expect("selection token exhausted");
                let token = self.selection_token;
                *ctx.preference = Some(crate::ui::settings_save::Preference {
                    screen: ctx.screen, token, generation: self.choice_generation.clone().unwrap(),
                    namespace: choices.namespace.clone(), podcast: media == Media::Podcast, value: preference,
                });
                self.selection_save = Some((token, row.key, media));
                Action::Stay
            }
            _ => Action::None,
        }
    }
    fn finish_choice(&mut self, key: String, media: Media, ctx: &mut Ctx) -> Action {
                if self.switch_root {
                    return Action::Pop;
                }
                let Page::Libraries { section, title, .. } = &self.page else {
                    return Action::None;
                };
                let target = Page::Browse {
                    library: key,
                    media,
                    section: section.clone(),
                    title: title.clone(),
                };
                if let Some(previous) = self.replace_after_choice.take() {
                    return Action::Many(vec![
                        Action::Dismiss(previous),
                        Action::Replace(Box::new(Self::new(target))),
                    ]);
                }
                self.page = target;
                self.choices = None;
                self.list = ListView::new(true);
                self.window.reset();
                self.error=None;
                self.load(0, ctx);
                Action::Stay

    }

}

fn stable_page(page: &Page) -> bool {
    matches!(page, Page::Libraries { section, .. } | Page::Browse { section, .. }
        if !matches!(section, Section::ContinueListening | Section::ContinueSeries | Section::ListenAgain | Section::NewestEpisodes | Section::LatestEpisodes | Section::Episodes(_)))
}

fn page_media(page: &Page) -> Option<Media> {
    match page {
        Page::Libraries { media, .. } | Page::Browse { media, .. } => Some(*media),
        _ => None,
    }
}

async fn fetch_rows(
    lib: &Arc<crate::library::Library>,
    query: &Page,
    page: u32,
    policy: Policy,
    saved: Option<&crate::config::ShelfLibraryPreference>,
    force_choice: bool,
) -> Result<Fetched, LibError> {
    match query {
        Page::Libraries {
            media,
            section,
            title,
        } => {
            let choices = lib.shelf_library_choices_async(*media, policy).await?;
            let selected = saved.map(|preference| choices.preferred(preference));
            if !force_choice {
                if let Some(Ok(preferred)) = &selected {
                    let query = Page::Browse {
                        library: preferred.id.clone(),
                        media: *media,
                        section: section.clone(),
                        title: title.clone(),
                    };
                    return Box::pin(fetch_rows(lib, &query, page, policy, None, false)).await;
                }
            }
            let notice = selected.as_ref().is_some_and(Result::is_err);
            let mut rows = Vec::new();
            if let Some(Err(reason)) = &selected {
                rows.push(
                    Row::new("Saved library unavailable", RowAction::None)
                        .key("__abs_default_notice")
                        .detail(*reason),
                );
            }
            rows.extend(choices.libraries.iter().map(|library| {
                let row = Row::new(&library.name, RowAction::Cmd(CHOOSE_LIBRARY)).key(&library.id);
                if selected.as_ref().is_some_and(|selected| {
                    selected
                        .as_ref()
                        .is_ok_and(|preferred| preferred.id == library.id)
                }) {
                    row.detail("Default")
                } else {
                    row
                }
            }));
            Ok(Fetched {
                rows,
                page: query.clone(),
                choices: Some(choices),
                notice,
                more:false,
            })
        }
        Page::Browse {
            library,
            media,
            section,
            ..
        } => {
            let result = lib.shelf_browse_async(library, *media, section, page, PAGE_SIZE, policy).await?;
            let more=result.has_more && result.paging!=Paging::Preview;
            let namespace = lib.shelf_namespace();
            let percentages = lib.spoken_percentages(result.entries.iter().filter_map(|entry| {
                let (item, episode) = match &entry.kind {
                    EntryKind::Book { item } => (item, None),
                    EntryKind::Episode { item, episode } => (item, Some(episode.clone())),
                    _ => return None,
                };
                Some((PlayedTarget::shelf(namespace.clone(), item.clone(), episode).logical, entry.duration_ms))
            }).collect()).await;
            let mut rows = page_rows(library, *media, page, result);
            for row in &mut rows {
                if let Some((item, episode)) = decode_play_key(&row.key) {
                    let target = PlayedTarget::shelf(namespace.clone(), item, episode);
                    row.spoken_progress = percentages.get(&target.logical).copied().unwrap_or_default();
                }
            }
            Ok(Fetched {
                rows,
                page: query.clone(),
                choices: None,
                notice: false,
                more,
            })
        }
        _ => unreachable!("browser query"),
    }
}

#[cfg(test)]
fn fetch_rows_sync(
    lib: &Arc<crate::library::Library>, query: &Page, page: u32, policy: Policy,
    saved: Option<&crate::config::ShelfLibraryPreference>, force_choice: bool,
) -> Result<Fetched, LibError> {
    let lib=lib.clone();let query=query.clone();let saved=saved.cloned();
    let (tx,rx)=tokio::sync::oneshot::channel();
    crate::rt::io().spawn(async move {let _=tx.send(fetch_rows(&lib,&query,page,policy,saved.as_ref(),force_choice).await);});
    rx.blocking_recv().unwrap()
}

pub fn library_popup(media: Media) -> Box<dyn Screen> {
    Box::new(
        super::popup::Popup::new(if media == Media::Book {
            "Books"
        } else {
            "Podcasts"
        })
        .add("Switch Library", move |ctx| {
            if ctx.config.audiobookshelf.url.trim().is_empty()
                || ctx.config.audiobookshelf.token.is_empty()
            {
                return Action::Toast("Set up Audiobookshelf in Settings → Connections".into());
            }
            switch_action(media, true, None)
        }),
    )
}
fn switch_action(media: Media, root: bool, previous: Option<u64>) -> Action {
    Action::Push(Box::new(Browser::switch(media, root, previous)))
}
fn play_key(item: &str, episode: Option<&str>) -> String {
    serde_json::to_string(&(item, episode)).expect("string tuple")
}
fn decode_play_key(key: &str) -> Option<(String, Option<String>)> {
    serde_json::from_str(key).ok()
}

fn collection_identity(page: &Page) -> Option<(String, String)> {
    match page {
        Page::Browse { library, media: Media::Book, section: Section::Collection(id), .. } => Some((library.clone(), id.clone())),
        _ => None,
    }
}

fn collection_action(ctx: &mut Ctx, generation: Arc<()>, library: String, id: String, toggle: bool) -> Action {
    let lib = ctx.lib.clone();
    ctx.notice_task(async move {
        match lib.collection_download(generation, library, id, toggle).await {
            Ok(message) | Err(message) => message,
        }
    });
    Action::Toast(if toggle { "Updating collection…" } else { "Queueing collection…" }.into())
}

fn page_rows(library: &str, media: Media, _page: u32, result: BrowsePage) -> Vec<Row> {
    result
        .entries
        .into_iter()
        .map(|entry| {
            let row = match entry.kind {
                EntryKind::Book { item } => {
                    Row::new(entry.title, RowAction::Cmd(OPEN)).key(play_key(&item, None))
                }
                EntryKind::Episode { item, episode } => {
                    Row::new(entry.title, RowAction::Cmd(OPEN)).key(play_key(&item, Some(&episode)))
                }
                EntryKind::Podcast { item } => Row::link(
                    entry.title.clone(),
                    Dest::Spoken(Page::Browse {
                        library: library.into(),
                        media,
                        section: Section::Episodes(item),
                        title: entry.title,
                    }),
                )
                .key(entry.id),
                EntryKind::Group { section } => Row::link(
                    entry.title.clone(),
                    Dest::Spoken(Page::Browse {
                        library: library.into(),
                        media,
                        section,
                        title: entry.title,
                    }),
                )
                .key(entry.id),
            };
            row.detail(entry.detail).art(entry.art)
        })
        .collect()
}
impl Screen for Browser {

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

    fn script_idle(&self) -> bool { (!self.loading || self.stuck) && self.selection_save.is_none() }
    fn script_ready(&self) -> bool { self.selection_save.is_none() && (!self.loading || self.stuck || self.list.rows.iter().any(|row|!matches!(row.action,RowAction::None))) }
    #[cfg(test)]
    fn audit(&self) -> crate::ui::hierarchy::Surface {
        crate::ui::hierarchy::Surface::list(&self.list)
    }
    #[cfg(test)]
    fn audit_settle(&mut self, ctx: &mut Ctx) {
        if self.invalidated {return;}
        let Some(request)=self.window.pending() else {return;};
        let token=request.token;
        let saved = page_media(&self.page).and_then(|media| {
            ctx.config
                .audiobookshelf
                .library_preference(media == Media::Podcast)
        });
        let result = fetch_rows_sync(
            ctx.lib,
            &self.page,
            request.page,
            Policy::CacheOnly,
            saved,
            self.force_choice,
        );
        self.loaded(
            token,
            Box::new(Loaded {
                result,
                configuration: ctx.lib.shelf_generation(),
                final_reply:true,
            }),
            ctx,
        );
    }
    fn title(&self) -> String {
        if self.invalidated {
            return "Audiobookshelf changed".into();
        }
        match &self.page {
            Page::Libraries { .. } => "Choose Library".into(),
            Page::Browse { title, .. } => title.clone(),
            _ => unreachable!(),
        }
    }
    fn enter(&mut self, ctx: &mut Ctx) {
        if !self.entered {
            self.entered = true;
            self.load(0, ctx);
        }
    }
    fn event(&mut self, event: Event, ctx: &mut Ctx) -> Action {
        match event {
            Event::Scroll(ticks) => {
                self.list.wheel(ticks, ctx.now);
                self.last_scroll = ctx.now;
                if let Some(page)=self.window.next(self.list.selected,ticks) {self.load(page,ctx);}
                Action::None
            }
            Event::WheelEnd => {
                self.list.end_spin();
                Action::None
            }
            Event::Center if self.stuck => { self.restart(ctx); Action::Stay }
            Event::Center => self.choose(ctx),
            Event::Back => {
                self.cancel();
                Action::None
            }
            Event::CenterLong => {
                if matches!(self.page, Page::Libraries { .. }) {
                    return Action::Stay;
                }
                let Some(media) = page_media(&self.page) else {
                    return Action::None;
                };
                if !self.invalidated && !self.choice_generation.as_ref().is_some_and(|generation|ctx.lib.shelf_generation_current(generation)) {
                    return Action::Toast("Audiobookshelf changed; select the item again".into());
                }
                let previous = ctx.screen;
                let mut popup = super::popup::Popup::new(self.title());
                if !self.invalidated {
                    if let Some(row) = self
                        .list
                        .selected_row()
                        .filter(|row| matches!(row.action, RowAction::Cmd(OPEN)))
                    {
                        if let Some((item, episode)) = decode_play_key(&row.key) {
                            let configuration = ctx.lib.shelf_generation();
                            let library = match &self.page {
                                Page::Browse { library, .. } => Some(library.clone()),
                                _ => None,
                            };
                            let target =
                                ctx.lib
                                    .shelf_info_target(library, item.clone(), episode.clone());
                            let title = row.title.clone();
                            let download_title = title.clone();
                            if let Ok(target) = target {
                                popup = popup.add(
                                    if episode.is_some() {
                                        "Episode Info"
                                    } else {
                                        "Book Info"
                                    },
                                    move |ctx| {
                                        Action::Push(super::open(
                                            Dest::BookInfo {
                                                target: target.clone(),
                                                title: title.clone(),
                                            },
                                            ctx,
                                        ))
                                    },
                                );
                            }
                            if let Some(namespace) = self.namespace.clone() {
                                popup = super::spoken::played_menu(popup, PlayedTarget::shelf(namespace, item.clone(), episode.clone()), row.spoken_progress);
                            }
                            popup = popup.add("Download for Offline", move |ctx| {
                                if !ctx.lib.shelf_generation_current(&configuration) {
                                    return Action::Toast(
                                        "Audiobookshelf changed; select the item again".into(),
                                    );
                                }
                                let request=match ctx.lib.begin_shelf_download(&item,episode.as_deref()) {Ok(request)=>request,Err(error)=>return Action::Toast(error)};
                                let lib = ctx.lib.clone();
                                let download_title = download_title.clone();
                                ctx.notice_task(async move {
                                    match lib.enqueue_shelf_download_async(request, download_title).await {
                                        Ok(()) => "Audiobook queued".into(),
                                        Err(error) => format!("Download failed: {error}"),
                                    }
                                });
                                Action::Toast("Queuing audiobook…".into())
                            });
                        }
                    }
                }
                if !self.invalidated {
                    let collection = self.list.selected_row().and_then(|row| match &row.action {
                        RowAction::Open(Dest::Spoken(page)) => collection_identity(page),
                        _ => None,
                    }).or_else(|| collection_identity(&self.page));
                    if let Some((library, id)) = collection {
                        let generation = ctx.lib.shelf_generation();
                        let (follow_library, follow_id, follow_generation) = (library.clone(), id.clone(), generation.clone());
                        popup = popup
                            .add("Download Collection", move |ctx| collection_action(ctx, generation.clone(), library.clone(), id.clone(), false))
                            .add("Toggle Follow New Books", move |ctx| collection_action(ctx, follow_generation.clone(), follow_library.clone(), follow_id.clone(), true));
                    }
                    let page = self.page.clone();
                    popup = popup.add("Refresh", move |_| {
                        let mut browser = Browser::new(page.clone());
                        browser.refresh_network = true;
                        Action::Many(vec![Action::Dismiss(previous), Action::Replace(Box::new(browser))])
                    });
                }
                Action::Push(Box::new(popup.add("Switch Library", move |_| {
                    switch_action(media, false, Some(previous))
                })))
            }
            _ => Action::None,
        }
    }
    fn completion(&mut self) -> Option<Action> { self.completed.take() }
    fn loaded(&mut self, token: u64, data: Box<dyn std::any::Any + Send>, ctx: &mut Ctx) {
        let data = match data.downcast::<crate::ui::settings_save::PreferenceSaved>() {
            Ok(saved) => {
                if self.selection_save.as_ref().is_some_and(|(expected, _, _)| *expected == token) {
                    let (_, key, media) = self.selection_save.take().unwrap();
                    if saved.0.is_ok() { self.completed = Some(self.finish_choice(key, media, ctx)); }
                }
                return;
            }
            Err(data) => data,
        };
        let data = match data.downcast::<ProgressSnapshot>() {
            Ok(snapshot) => {
                self.progress_pending = false;
                if ctx.lib.shelf_generation_current(&snapshot.generation) {
                    self.listening = snapshot.percentages;
                    self.apply_badges();
                } else {
                    self.progress_stale = true;
                }
                return;
            }
            Err(data) => data,
        };
        let data = match data.downcast::<BadgeSnapshot>() {
            Ok(snapshot) => {
                self.downloads.pending = false;
                if snapshot.namespace != ctx.lib.shelf_namespace() { self.downloads.stale = true; self.downloads.full = true; return; }
                self.downloads.queue.clear();
                self.downloads.detail.clear();
                for row in snapshot.queue.iter().filter(|row| row.namespace == snapshot.namespace) {
                    let key = (row.item.clone(), row.episode.clone());
                    let (badge, detail) = super::spoken::spoken_queue_status(row);
                    self.downloads.queue.insert(key.clone(), badge);
                    self.downloads.detail.insert(key, detail);
                }
                if let Some(downloaded) = snapshot.downloaded { self.downloads.downloaded = downloaded.into_iter().collect(); }
                self.rebuild();
                return;
            }
            Err(data) => data,
        };
        if let Ok(loaded) = data.downcast::<Loaded>() {
            if ctx.lib.shelf_generation_current(&loaded.configuration)
                && !self.invalidated
                && self.window.pending().is_some_and(|request|request.token==token)
            {
                self.choice_generation = Some(loaded.configuration);
                self.namespace = Some(ctx.lib.shelf_namespace()).filter(|ns| !ns.is_empty());
                match loaded.result {
                    Ok(fetched) => {
                        if self.page != fetched.page {
                            self.list = ListView::new(matches!(fetched.page, Page::Browse { .. }));
                        }
                        self.page = fetched.page;
                        self.choices = fetched.choices;
                        for row in &fetched.rows {
                            self.listening.insert(row.key.clone(), row.spoken_progress);
                        }
                        self.apply(token, Ok(fetched.rows),fetched.more,loaded.final_reply);
                        if fetched.notice && self.list.selected == 0 && self.list.rows.len() > 1 {
                            self.list.select(1);
                        }
                    }
                    Err(error) => self.apply(token, Err(error),false,loaded.final_reply),
                }
            }
        }
    }
    fn refresh(&mut self, event: &LibEvent, ctx: &mut Ctx) {
        if matches!(event, LibEvent::SpokenProgress) { self.progress_stale = true; }
        if matches!(event, LibEvent::Downloads) { self.downloads.stale = true; self.downloads.full = true; }
        // UXI-018: republishing the same account's configuration (startup,
        // reconnect) keeps the shown rows and selection; only a different
        // account, or a load the change made obsolete, resets the page.
        if matches!(event, LibEvent::SpokenChanged) && !self.invalidated && self.window.pending().is_none() && !self.list.rows.is_empty() {
            let current = ctx.lib.shelf_namespace();
            if self.namespace.as_deref().is_some_and(|shown| current == shown) {
                self.choice_generation = Some(ctx.lib.shelf_generation());
                return;
            }
            if current.is_empty() && self.namespace.is_some() { return; }
        }
        if matches!(event, LibEvent::SpokenChanged) {
            self.window.reset();
            self.list.set_rows(Vec::new());
            self.error=None;self.retrying=false;
            if matches!(self.page, Page::Libraries { .. }) {
                self.load(0, ctx);
            } else {
                self.invalidated = true;
                self.loading = false;
            }
        }
    }
    fn tick(&mut self, ctx: &mut Ctx) -> bool {
        if !self.stuck && self.waiting() && self.deadline.expired(ctx.now, crate::ux::Journey::OpenPage) {
            // A late answer still replaces this (apply clears it).
            self.stuck = true;
            return true;
        }
        if ctx.screen_on { self.request_progress(ctx); }
        // After a download event, and each second while an item downloads.
        let downloading = self.downloads.queue.values().any(|badge| matches!(badge, crate::ui::list::Badge::Downloading(_) | crate::ui::list::Badge::Queued));
        if ctx.screen_on && !self.list.rows.is_empty() && (self.downloads.stale
            || (downloading && self.downloads.asked.is_none_or(|at| ctx.now.saturating_duration_since(at) >= Duration::from_secs(1)))) {
            self.request_badges(ctx);
        }
        if !ctx.screen_on
            || self.warmed == Some(self.list.selected)
            || ctx.now.saturating_duration_since(self.last_scroll) < Duration::from_millis(150)
        {
            return false;
        }
        self.warmed = Some(self.list.selected);
        // UX-007: the item under the selection is prefetched (info and position).
        if let Some((item, episode)) = self.list.selected_row().and_then(|row| decode_play_key(&row.key)) {
            let library = match &self.page { Page::Browse { library, .. } => Some(library.clone()), _ => None };
            if let Ok(target) = ctx.lib.shelf_info_target(library, item, episode) {
                crate::rt::io().spawn(ctx.lib.clone().prefetch_spoken_async(target));
            }
        }
        for delta in [0isize, 1, -1, 2, -2] {
            let index = self.list.selected as isize + delta;
            if index < 0 {
                continue;
            }
            if let Some(art) = self
                .list
                .rows
                .get(index as usize)
                .and_then(|row| row.art.as_ref())
            {
                ctx.lib
                    .art
                    .prefetch_cover(art, crate::library::art::THUMBNAIL_SIZE);
            }
        }
        false
    }
    fn draw(&mut self, draw: &mut Draw, area: Rect) {
        if self.invalidated {
            self.list.clear_hit_targets();
            draw.message(area, "Audiobookshelf changed\nReturn to choose a library");
        } else if self.selection_save.is_some() {
            self.list.clear_hit_targets();
            draw.message(area, "Saving library selection…");
        } else if let Some(view) = self.view() {
            let target = crate::ui::state::draw_target(draw, area, view);
            self.list.set_state_target(target);
        } else {
            self.list.draw(draw, area);
        }
    }
    fn animate(&mut self, now: Instant) -> bool {
        self.list.animate(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture_app() -> crate::ui::settings_tests::Fixture {
        let mut app = crate::ui::settings_tests::fixture();
        app.background=false;
        let lib = app.lib.clone();
        crate::library::shelf_browse::seed_hierarchy(&lib, &mut app.config).unwrap();
        app
    }
    #[test]
    fn page_states_use_the_shared_presentation() {
        use crate::ui::state::View;
        let browse = |section| Page::Browse { library: "l".into(), media: Media::Book, section, title: "Generated".into() };
        let mut browser = Browser::new(browse(Section::ContinueListening));
        assert_eq!(browser.view(), Some(View::Loading), "a first load is the shared loading state");
        browser.loading = false;
        assert_eq!(browser.view(), Some(View::Empty("Nothing in progress")), "an empty page says what is missing");
        browser.list.set_rows(vec![Row::new("Retry", RowAction::Cmd(RETRY)).key(RETRY_KEY).detail("Can't reach Audiobookshelf")]);
        assert_eq!(browser.view(), Some(View::Failed { text: "Can't reach Audiobookshelf", retry: true }));
        browser.loading = true;
        assert_eq!(browser.view(), Some(View::Loading), "a retry in flight");
        browser.stuck = true;
        assert_eq!(browser.view(), Some(View::Failed { text: crate::ux::Code::Stuck.message(), retry: true }));
        browser.stuck = false;
        browser.list.set_rows(vec![Row::new("Generated Book", RowAction::Cmd(OPEN)).key("book")]);
        assert_eq!(browser.view(), None, "content shows as the list");
        assert_eq!(empty_text(&browse(Section::Books)), "No books");
        assert_eq!(empty_text(&Page::Browse { library: "p".into(), media: Media::Podcast, section: Section::Books, title: "Library".into() }), "No podcasts");
        assert_eq!(empty_text(&browse(Section::Search("g".into()))), "No matches");
    }
    /// UX-006: a first load with nothing shown says it is taking too long at
    /// the shared deadline, offers Retry, and Retry starts the load again.
    #[test]
    fn waiting_page_reaches_the_deadline_and_retries() {
        let mut app = fixture_app();
        app.push(Box::new(Browser::new(Page::Browse { library: "a".into(), media: Media::Book,
            section: Section::Books, title: "Generated".into() })));
        let id = app.top_id();
        let later = Instant::now() + crate::ui::state::DEADLINE + Duration::from_millis(10);
        app.with_screen(id, |screen, ctx| {
            ctx.now = later;
            assert!(screen.tick(ctx), "the deadline redraws");
            assert!(screen.script_ready(), "a stuck page is settled");
        }).unwrap();
        assert_eq!(app.stack.last().unwrap().1.title(), "Generated");
        app.with_screen(id, |screen, ctx| {
            ctx.now = later;
            assert!(matches!(screen.event(Event::Center, ctx), Action::Stay));
            assert!(!screen.tick(ctx), "Retry restarts the deadline");
        }).unwrap();
        app.player.shutdown();
    }
    #[test]
    fn podcast_show_episode_cache_is_refreshed_online() {
        let page=Page::Browse {library:"p".into(),media:Media::Podcast,
            section:Section::Episodes("show".into()),title:"Generated".into()};
        assert!(!stable_page(&page));
    }
    fn settle(app: &mut crate::ui::App) {
        let id = app.top_id();
        app.with_screen(id, |screen, ctx| screen.audit_settle(ctx));
    }
    fn step(app: &mut crate::ui::App, event: Event) {
        app.event(event, &mut crate::platform::Headless { last: None });
        app.settle_settings_fixture();
        settle(app);
    }
    fn open_libraries(app: &mut crate::ui::App, media: Media) {
        app.push(open(Page::Libraries {
            media,
            section: Section::Books,
            title: if media == Media::Book {
                "Books"
            } else {
                "Podcasts"
            }
            .into(),
        }));
        settle(app);
    }
    /// UXI-018: a configuration republish for the same account (startup,
    /// reconnect) leaves an open Books page and its selection unchanged.
    #[test]
    fn same_account_republish_keeps_the_open_page() {
        let mut app = fixture_app();
        open_libraries(&mut app, Media::Book);
        step(&mut app, Event::Center);
        assert_eq!(app.stack.last().unwrap().1.title(), "Books");
        let keys = |app: &crate::ui::App| app.stack.last().unwrap().1.audit().rows().iter().map(|row| row.key.clone()).collect::<Vec<_>>();
        let before = keys(&app);
        assert!(!before.is_empty());
        assert!(!app.lib.shelf_namespace().is_empty());
        app.handle(crate::ui::UiMsg::Lib(LibEvent::SpokenChanged), &mut crate::platform::Headless { last: None });
        assert_eq!(keys(&app), before, "rows kept");
    }

    #[test]
    fn defaults_are_saved_separately_and_subsequent_entry_skips_the_picker() {
        let mut app = fixture_app();
        open_libraries(&mut app, Media::Book);
        assert_eq!(app.stack.last().unwrap().1.title(), "Choose Library");
        step(&mut app, Event::Center);
        assert_eq!(app.stack.last().unwrap().1.title(), "Books");
        assert_eq!(
            app.config.audiobookshelf.default_books.as_ref().unwrap().id,
            "a"
        );
        step(&mut app, Event::Back);
        open_libraries(&mut app, Media::Podcast);
        step(&mut app, Event::Center);
        assert_eq!(
            app.config
                .audiobookshelf
                .default_podcasts
                .as_ref()
                .unwrap()
                .id,
            "p"
        );
        assert_eq!(
            app.config.audiobookshelf.default_books.as_ref().unwrap().id,
            "a"
        );
        let restored = app.paths.load_config(&crate::rt::Blocking::dedicated_thread());
        assert_eq!(
            restored.audiobookshelf.default_books,
            app.config.audiobookshelf.default_books
        );
        assert_eq!(
            restored.audiobookshelf.default_podcasts,
            app.config.audiobookshelf.default_podcasts
        );
        step(&mut app, Event::Back);
        open_libraries(&mut app, Media::Book);
        assert_eq!(app.stack.last().unwrap().1.title(), "Books");
        assert!(
            app.stack
                .last()
                .unwrap()
                .1
                .audit()
                .rows()
                .iter()
                .any(|row| row.title == "Generated Book")
        );
        app.player.shutdown();
    }
    #[test]
    fn switching_can_cancel_in_place_or_replace_old_library_without_extra_back_step() {
        let mut app = fixture_app();
        open_libraries(&mut app, Media::Book);
        step(&mut app, Event::Center);
        let original = app.config.audiobookshelf.default_books.clone();
        let old_screen = app.top_id();
        step(&mut app, Event::CenterLong);
        step(&mut app, Event::Scroll(i32::MAX));
        step(&mut app, Event::Center);
        assert_eq!(app.stack.last().unwrap().1.title(), "Choose Library");
        step(&mut app, Event::Back);
        assert_eq!(app.top_id(), old_screen);
        assert_eq!(app.config.audiobookshelf.default_books, original);
        step(&mut app, Event::CenterLong);
        step(&mut app, Event::Scroll(i32::MAX));
        step(&mut app, Event::Center);
        step(&mut app, Event::Scroll(1));
        step(&mut app, Event::Center);
        assert_eq!(
            app.config.audiobookshelf.default_books.as_ref().unwrap().id,
            "b"
        );
        assert!(!app.stack.iter().any(|(id, _)| *id == old_screen));
        step(&mut app, Event::Back);
        assert_eq!(app.stack.len(), 1);
        app.player.shutdown();
    }
    #[test]
    fn pending_library_selection_keeps_back_and_newer_edits_without_early_navigation() {
        let mut app = fixture_app();
        open_libraries(&mut app, Media::Book);
        let release = app.paths.hold_config_saves();
        app.event(Event::Center, &mut crate::platform::Headless { last: None });
        assert!(app.settings_pending());
        assert_eq!(app.stack.last().unwrap().1.title(), "Choose Library");
        assert!(app.config.audiobookshelf.default_books.is_none());
        assert!(!app.stack.last().unwrap().1.script_idle());
        app.change_volume(3);
        let volume = app.config.playback.volume;
        app.event(Event::Back, &mut crate::platform::Headless { last: None });
        let returned = app.top_id();
        release.send(()).unwrap();
        assert!(app.save_settings());
        assert_eq!(app.top_id(), returned, "durable completion cannot navigate a dismissed picker");
        assert_eq!(app.config.audiobookshelf.default_books.as_ref().unwrap().id, "a");
        let stored = crate::config::Config::load(&crate::rt::Blocking::dedicated_thread(), &app.paths.config());
        assert_eq!(stored.playback.volume, volume, "preference completion merges instead of overwriting later edits");
        assert_eq!(stored.audiobookshelf.default_books, app.config.audiobookshelf.default_books);
        assert!(app.config_save.is_none());
        app.player.shutdown();
    }
    #[test]
    fn stale_account_or_failed_config_write_cannot_commit_a_choice() {
        let mut app = fixture_app();
        open_libraries(&mut app, Media::Book);
        let mut changed = app.config.clone();
        changed.audiobookshelf.token = "different-generated-account".into();
        app.lib.configure(&changed);
        app.config = changed;
        step(&mut app, Event::Center);
        assert!(app.config.audiobookshelf.default_books.is_none());
        app.player.shutdown();
        drop(app);
        let mut app = fixture_app();
        open_libraries(&mut app, Media::Book);
        std::fs::create_dir(app.paths.root.join("config.toml")).unwrap();
        step(&mut app, Event::Center);
        assert!(app.config.audiobookshelf.default_books.is_none());
        assert_eq!(app.stack.last().unwrap().1.title(), "Choose Library");
        app.player.shutdown();
    }
    #[test]
    fn wrong_media_default_shows_choices_without_silent_substitution() {
        let mut app = fixture_app();
        let preferences = app
            .lib
            .shelf_library_choices(Media::Podcast, Policy::CacheOnly)
            .unwrap();
        app.config.audiobookshelf.default_books = preferences.preference("p");
        open_libraries(&mut app, Media::Book);
        assert_eq!(app.stack.last().unwrap().1.title(), "Choose Library");
        let surface = app.stack.last().unwrap().1.audit();
        assert!(
            surface
                .rows()
                .iter()
                .any(|row| row.title == "Saved library unavailable")
        );
        assert_eq!(
            app.config.audiobookshelf.default_books.as_ref().unwrap().id,
            "p"
        );
        step(&mut app, Event::Back);
        assert_eq!(
            app.config.audiobookshelf.default_books.as_ref().unwrap().id,
            "p"
        );
        app.player.shutdown();
    }
    #[test]
    fn remote_book_preview_requires_explicit_play_and_back_ignores_late_metadata() {
        let mut app=fixture_app();
        open_libraries(&mut app,Media::Book);step(&mut app,Event::Center);
        let browser=app.top_id();
        step(&mut app,Event::Center);
        assert_eq!(app.stack.last().unwrap().1.title(),"Book Info");
        assert!(app.status.current().is_none());
        let preview=app.top_id();
        step(&mut app,Event::Back);
        let configuration=Some(app.lib.shelf_generation());
        app.handle(crate::ui::UiMsg::Loaded {screen:preview,token:1,data:Box::new(super::super::book_info::Loaded {result:Ok(Default::default()),configuration,synced:None})},&mut crate::platform::Headless {last:None});
        assert_eq!(app.top_id(),browser);
        assert!(!app.stack.iter().any(|(_,screen)|screen.is_now_playing()));
        step(&mut app,Event::Center);
        assert_eq!(app.stack.last().unwrap().1.title(),"Book Info");
        step(&mut app,Event::Center);
        assert_eq!(app.stack.last().unwrap().1.title(),"Generated Book");
        step(&mut app,Event::Back);
        assert_eq!(app.top_id(),browser);
        app.player.shutdown();
    }
    #[test]
    fn root_switch_saves_without_opening_a_second_content_root() {
        let mut app=fixture_app();
        app.push(super::super::menu::Menu::boxed(SpokenPage(Page::Books)));
        let root=app.top_id();
        step(&mut app,Event::CenterLong);step(&mut app,Event::Center);
        assert_eq!(app.stack.last().unwrap().1.title(),"Choose Library");
        step(&mut app,Event::Scroll(1));step(&mut app,Event::Center);
        assert_eq!(app.top_id(),root);
        assert_eq!(app.config.audiobookshelf.default_books.as_ref().unwrap().id,"b");
        assert!(app.config.audiobookshelf.default_podcasts.is_none());
        app.player.shutdown();
    }
    fn browser() -> Browser {
        Browser::new(Page::Browse {
            library: "fixture-library".into(),
            media: Media::Book,
            section: Section::Books,
            title: "Books".into(),
        })
    }
    fn rows(page: u32, count: usize) -> Vec<Row> {
        (0..count)
            .map(|index| {
                Row::new(
                    format!("Book {}", u64::from(page) * 50 + index as u64),
                    RowAction::Cmd(OPEN),
                )
                .key(format!("book:{page}:{index}"))
            })
            .collect()
    }
    #[test]
    fn cached_content_remains_selectable_while_refresh_and_continuation_are_pending() {
        let mut browser=browser();
        let initial=browser.begin_load(0).unwrap();
        browser.apply(initial,Ok(rows(0,50)),true,false);
        assert!(browser.loading && browser.script_ready());
        assert!(!browser.script_idle(),"usable cached controls are distinct from final request completion");
        assert!(browser.begin_load(1).is_none());
        browser.list.select(31);
        let anchor=browser.list.selected_row().unwrap().key.clone();
        browser.apply(initial,Ok(rows(0,50)),true,true);
        assert!(browser.script_idle());
        let next=browser.begin_load(1).unwrap();
        assert_eq!(browser.list.selected_row().unwrap().key,anchor);
        browser.apply(next,Err(LibError::Offline),false,true);
        assert_eq!(browser.list.selected_row().unwrap().key,anchor);
        assert!(browser.list.rows.iter().any(|row|matches!(row.action,RowAction::Cmd(RETRY))));
        let retry=browser.begin_load(1).unwrap();
        assert!(browser.retrying);
        assert!(browser.list.rows.iter().any(|row|row.key=="__abs_retry" && matches!(row.action,RowAction::None)));
        browser.apply(retry,Ok(rows(1,50)),false,false);
        assert!(browser.retrying && browser.loading);
        browser.apply(retry,Ok(rows(1,50)),false,true);
        assert!(!browser.loading && !browser.retrying && browser.error.is_none());
        assert_eq!(browser.list.selected_row().unwrap().key,anchor);
        assert_eq!(browser.list.rows.len(),100);
        assert_eq!(browser.window.next(99,1),None);
    }
    #[test]
    fn late_replies_and_cancelled_account_requests_do_not_replace_current_content() {
        let mut browser=browser();let mut rng=fastrand::Rng::with_seed(0xAB52026);
        for page in 0..1000 {
            browser.window.reset();
            let token=browser.begin_load(page).unwrap();
            browser.apply(token,Ok(rows(page,50)),true,true);
            browser.list.select(rng.usize(0..50));
            let before=browser.list.selected_row().unwrap().key.clone();
            let current=browser.begin_load(page+1).unwrap();
            for late in [token,current.wrapping_sub(2),current.wrapping_add(1)] {
                browser.apply(late,Ok(rows(page+9,50)),true,true);
                browser.apply(late,Err(LibError::Offline),false,true);
                assert_eq!(browser.list.selected_row().unwrap().key,before);
                assert!(browser.error.is_none());
            }
            browser.cancel();
            browser.apply(current,Ok(rows(page+1,50)),true,true);
            assert!(browser.list.rows.is_empty());
            browser.invalidated=false;
        }
    }
    #[test]
    fn page_reordering_preserves_identity_and_empty_groups_have_no_furniture() {
        let mut browser=browser();let token=browser.begin_load(0).unwrap();
        browser.apply(token,Ok(rows(0,50)),true,false);browser.list.select(17);
        let mut fresh=rows(0,50);fresh.reverse();
        browser.apply(token,Ok(fresh),true,true);
        assert_eq!(browser.list.selected_row().unwrap().key,"book:0:17");
        for paging in [Paging::Server,Paging::Snapshot,Paging::Preview] {
            let empty=page_rows("generated",Media::Book,0,BrowsePage {paging,has_more:true,..Default::default()});
            assert!(empty.is_empty());
        }
    }
    fn fetched(app:&crate::ui::App,page:&Page)->Fetched {
        fetch_rows_sync(&app.lib,page,0,Policy::CacheOnly,None,false).unwrap()
    }
    #[test]
    fn actual_app_opens_book_info_during_a_slow_next_page_and_returns_to_its_anchor() {
        let mut app=fixture_app();
        let page=Page::Browse {library:"a".into(),media:Media::Book,section:Section::Books,title:"Books".into()};
        app.push(open(page.clone()));let parent=app.top_id();
        let configuration=app.lib.shelf_generation();
        let mut cached=fetched(&app,&page);cached.more=true;
        let first_key=cached.rows[0].key.clone();
        app.handle(crate::ui::UiMsg::Loaded {screen:parent,token:1,data:Box::new(Loaded {result:Ok(cached),configuration:configuration.clone(),final_reply:false})},&mut crate::platform::Headless {last:None});
        assert!(app.stack.last().unwrap().1.script_ready());
        let mut fresh=fetched(&app,&page);fresh.more=true;
        app.handle(crate::ui::UiMsg::Loaded {screen:parent,token:1,data:Box::new(Loaded {result:Ok(fresh),configuration:configuration.clone(),final_reply:true})},&mut crate::platform::Headless {last:None});
        app.event(Event::Scroll(1),&mut crate::platform::Headless {last:None});
        app.event(Event::Center,&mut crate::platform::Headless {last:None});
        assert_eq!(app.stack.len(),3,"a pending continuation must not block existing book preview");
        let mut next=fetched(&app,&page);next.rows=vec![Row::new("Generated next book",RowAction::Cmd(OPEN)).key(play_key("a-next",None))];next.more=false;
        app.handle(crate::ui::UiMsg::Loaded {screen:parent,token:2,data:Box::new(Loaded {result:Ok(next),configuration,final_reply:true})},&mut crate::platform::Headless {last:None});
        app.event(Event::Back,&mut crate::platform::Headless {last:None});
        assert_eq!(app.top_id(),parent);
        let surface=app.stack.last().unwrap().1.audit();
        assert_eq!(surface.rows()[surface.selected()].key,first_key);
        assert_eq!(surface.rows().len(),2);
        assert!(surface.rows().iter().all(|row|!row.key.starts_with("__abs_")));
        app.player.shutdown();
    }
    #[test]
    fn play_identity_round_trips_reserved_unicode_and_same_titles() {
        for item in ["book", "one/two", "?&= %", "日本語:Book"] {
            for episode in [None, Some(""), Some("episode:/?&é")] {
                assert_eq!(
                    decode_play_key(&play_key(item, episode)),
                    Some((item.into(), episode.map(String::from)))
                );
            }
        }
    }
    #[test]
    fn podcast_episode_rows_keep_two_line_metadata_and_play_identity() {
        let page = BrowsePage { entries: vec![crate::library::shelf_browse::Entry {
            id:"show/ep".into(), title:"The Saga of Sagas".into(),
            detail:"August 14, 2024 · S4 E21 · 1:57:57".into(), duration_ms: 7_077_000, art:Some("show".into()),
            kind:EntryKind::Episode {item:"show".into(), episode:"ep".into()},
        }], ..Default::default() };
        let rows=page_rows("library", Media::Podcast, 0, page);
        assert_eq!(rows[0].title,"The Saga of Sagas");
        assert_eq!(rows[0].detail.as_deref(),Some("August 14, 2024 · S4 E21 · 1:57:57"));
        assert_eq!(decode_play_key(&rows[0].key),Some(("show".into(),Some("ep".into()))));
        // Use the actual browser/list with a generated row and optional retained frame.
        let mut browser=Browser::new(Page::Browse {library:"library".into(),media:Media::Podcast,
            section:Section::Episodes("show".into()),title:"Episodes".into()});
        browser.entered=true; browser.loading=false; browser.list.set_rows(rows);
        let mut app=crate::ui::settings_tests::fixture(); app.background=false;
        app.push(Box::new(browser));
        let frame=app.render();
        if let Some(directory)=std::env::var_os("Q2_PODCAST_RENDER_DIR") {
            let directory=std::path::Path::new(&directory); std::fs::create_dir_all(directory).unwrap();
            let output=directory.join("podcast-episode-row.png");
            assert!(!output.exists(),"preserve earlier generated screenshots");
            let rgb:Vec<u8>=frame.pixels.iter().flat_map(|p| [(*p>>16) as u8,(*p>>8) as u8,*p as u8]).collect();
            image::save_buffer(output,&rgb,crate::gfx::WIDTH as u32,crate::gfx::HEIGHT as u32,image::ColorType::Rgb8).unwrap();
        }
        app.player.shutdown();
    }

}
