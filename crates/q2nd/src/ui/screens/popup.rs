//! Pop-up menus: what holding the centre offers for a song, album or
//! playlist, and other short choices.

use super::sources::offline_set;
use crate::audio::player::Cmd;
use crate::gfx::{Color, Rect, WIDTH, Weight};
use crate::input::Event;
use crate::library::{Library, Policy};
use crate::ui::list::{ListView, Row, RowAction};
use crate::ui::theme::ROW_H;
use crate::ui::{Action, Ctx, Dest, Draw, Screen};
use std::sync::Arc;
use std::time::Instant;
use subsonic::{ItemKind, Song, calls};

type Handler = Box<dyn Fn(&mut Ctx) -> Action>;

pub struct Popup {
    title: String,
    list: ListView,
    handlers: Vec<Option<Handler>>,
    download: Option<DownloadChoice>,
}
struct DownloadChoice {
    account: u32,
    kind: Option<String>,
    id: String,
    index: usize,
    choose: Box<dyn FnOnce(bool) -> Option<(String, Handler)>>,
}

impl Popup {
    pub fn new(title: impl Into<String>) -> Popup {
        Popup { title: title.into(), list: ListView::new(false), handlers: Vec::new(), download: None }
    }

    /// Adds the owner's “Report a Problem” (TELEMETRY-005): marks the moment
    /// and sends the recent numeric history with the current state.
    pub fn report_problem(self) -> Popup {
        self.add("Report a Problem", |_| { crate::ux::problem(); Action::Toast("Problem reported. Thank you".into()) })
    }

    pub fn add(mut self, label: &str, f: impl Fn(&mut Ctx) -> Action + 'static) -> Popup {
        let i = self.handlers.len() as u32;
        let mut rows = std::mem::take(&mut self.list.rows);
        rows.push(Row::new(label, RowAction::Cmd(i)).key(format!("{i}")));
        self.list.set_rows(rows);
        self.handlers.push(Some(Box::new(f)));
        self
    }
    fn download_choice(mut self, ctx: &Ctx, kind: Option<String>, id: String,
        choose: impl FnOnce(bool) -> Option<(String, Handler)> + 'static) -> Self {
        let index = self.handlers.len();
        let mut rows = std::mem::take(&mut self.list.rows);
        rows.push(Row::new("Checking download…", RowAction::Cmd(index as u32)).key("#download-choice").dim(true));
        self.list.set_rows(rows);
        self.handlers.push(None);
        self.download = Some(DownloadChoice { account: ctx.lib.music_account_generation(), kind, id, index, choose: Box::new(choose) });
        self
    }

}

impl Screen for Popup {

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

    fn script_ready(&self) -> bool { self.download.is_none() }
    fn enter(&mut self, ctx: &mut Ctx) {
        let Some(pending) = &self.download else { return; };
        let result = ctx.lib.download_choice(pending.account, pending.kind.clone(), pending.id.clone());
        ctx.task(1, move |reply| async move { reply.send(result.await); });
    }
    fn loaded(&mut self, token: u64, data: Box<dyn std::any::Any + Send>, ctx: &mut Ctx) {
        if token != 1 { return; }
        let Ok(result) = data.downcast::<Result<bool, crate::library::LibError>>() else { return; };
        let Some(pending) = self.download.take() else { return; };
        let replacement = if ctx.lib.music_account_generation() != pending.account {
            Some(("Navidrome changed; reopen menu".into(), None))
        } else {
            match *result {
                Ok(downloaded) => (pending.choose)(downloaded).map(|(label, handler)| {
                    let account = pending.account;
                    let guarded: Handler = Box::new(move |ctx| {
                        if ctx.lib.music_account_generation() != account {
                            return Action::Toast("Navidrome changed; select the item again".into());
                        }
                        handler(ctx)
                    });
                    (label, Some(guarded))
                }),
                Err(_) => Some(("Downloads unavailable".into(), None)),
            }
        };
        let mut rows = std::mem::take(&mut self.list.rows);
        if let Some(position) = rows.iter().position(|row| row.key == "#download-choice") {
            if let Some((label, handler)) = replacement {
                rows[position].title = label;
                rows[position].dim = handler.is_none();
                self.handlers[pending.index] = handler;
            } else { rows.remove(position); }
        }
        self.list.set_rows(rows);
    }
    #[cfg(test)]
    fn audit_settle(&mut self, ctx: &mut Ctx) {
        if let Some(pending) = &self.download {
            let result = crate::rt::Ui::new().unwrap().block_on(ctx.lib.download_choice(pending.account, pending.kind.clone(), pending.id.clone()));
            self.loaded(1, Box::new(result), ctx);
        }
    }
    #[cfg(test)]
    fn audit(&self) -> crate::ui::hierarchy::Surface { crate::ui::hierarchy::Surface::list(&self.list).contextual() }
    fn title(&self) -> String {
        self.title.clone()
    }

    fn is_overlay(&self) -> bool {
        true
    }

    fn event(&mut self, ev: Event, ctx: &mut Ctx) -> Action {
        match ev {
            Event::WheelEnd => { self.list.end_spin(); Action::None }
            Event::Scroll(t) => {
                self.list.wheel(t, ctx.now);
                Action::None
            }
            Event::Center => {
                let Some(RowAction::Cmd(index)) = self.list.selected_row().map(|row| &row.action) else { return Action::None; };
                let Some(handler) = self.handlers.get(*index as usize).and_then(Option::as_ref) else { return Action::None; };
                let a = handler(ctx);
                // The pop-up closes, then the choice takes effect.
                match a {
                    Action::Push(s) => Action::Replace(s),
                    Action::Pop | Action::None | Action::Stay => Action::Pop,
                    other => Action::Many(vec![Action::Pop, other]),
                }
            }
            Event::CenterLong => Action::Pop,
            _ => Action::None,
        }
    }

    /// The classic's pop-up menu: translucent black, white type, the blue
    /// selection.
    fn draw(&mut self, d: &mut Draw, area: Rect) {
        let rows = self.list.rows.len().min(6) as i32;
        let head = 34;
        let h = head + rows * ROW_H + 8;
        let w = WIDTH - 64;
        let r = Rect::new((WIDTH - w) / 2, area.y + (area.h - h) / 2, w, h);
        d.c.set_clip(area);
        let th = d.theme;
        d.c.fill_round_rect_alpha(r, 12, th.hud, th.hud_alpha);
        let title = d.fonts.ellipsize(&self.title, 15, Weight::Bold, w - 32).into_owned();
        let lh = d.fonts.line_height(15);
        d.fonts.draw_centered(d.c, &title, WIDTH / 2, r.y + (head - lh) / 2 + 2, 15, Weight::Bold, Color(0xB8B8B8));
        let list_area = Rect::new(r.x + 6, r.y + head, r.w - 12, rows * ROW_H);
        // The list in the pop-up's own colours.
        let saved = d.theme;
        d.theme.text = Color::WHITE;
        d.theme.muted = Color(0xB8B8B8);
        d.theme.chevron = Color(0xB8B8B8);
        d.theme.scroll_track = Color(0x2A2A2A);
        d.theme.scroll_line = Color(0x444444);
        let old = d.c.set_clip(list_area.intersect(&area));
        self.list.draw(d, list_area);
        d.c.set_clip(old);
        d.theme = saved;
    }

    fn animate(&mut self, now: Instant) -> bool {
        self.list.animate(now)
    }
}

/// An album's or playlist's songs, fetched when chosen from its menu.
async fn songs_of(lib: Arc<Library>, dest: Dest) -> Result<Vec<Song>, String> {
    let (got, what) = match &dest {
        Dest::Album { id, .. } => (lib.fetch_async(calls::album(id), Policy::NetworkFirst).await.map(|f| f.value.song), "album"),
        Dest::Playlist { id, .. } => (lib.fetch_async(calls::playlist(id), Policy::NetworkFirst).await.map(|f| f.value.entry), "playlist"),
        _ => return Err("This collection cannot be played".into()),
    };
    match got {
        Ok(songs) if !songs.is_empty() => Ok(songs),
        Ok(_) => Err(format!("The {what} has no songs")),
        Err(error) => {
            log::warn!("could not load {dest:?}: {error}");
            Err(format!("Could not load the {what}"))
        }
    }
}

/// The latest Play or Shuffle chosen from a collection's menu.
static PLAY_CHOICE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// What was playing when Play or Shuffle was chosen. The songs arrive later;
/// a queue started since, a later choice, or another account wins over them.
struct Chosen {
    choice: u32,
    queue: Arc<Vec<Song>>,
    account: u32,
}
impl Chosen {
    fn now(player: &crate::audio::player::PlayerHandle, lib: &Library) -> Chosen {
        let choice = PLAY_CHOICE.fetch_add(1, std::sync::atomic::Ordering::SeqCst).wrapping_add(1);
        Chosen { choice, queue: player.status().queue, account: lib.music_account_generation() }
    }
    /// `Status::queue` is a new `Arc` whenever the queue's contents or order change.
    fn current(&self, player: &crate::audio::player::PlayerHandle, lib: &Library) -> bool {
        PLAY_CHOICE.load(std::sync::atomic::Ordering::SeqCst) == self.choice
            && Arc::ptr_eq(&self.queue, &player.status().queue)
            && lib.music_account_generation() == self.account
    }
    /// Plays the fetched songs if nothing newer was chosen; otherwise the notice.
    fn play(&self, player: &crate::audio::player::PlayerHandle, lib: &Library, songs: Result<Vec<Song>, String>, shuffle: bool) -> Result<(), String> {
        let songs = songs?;
        if !self.current(player, lib) { return Err("Playback changed; choose Play again".into()); }
        player.send(Cmd::Play { songs: Arc::new(songs), index: 0, shuffle });
        Ok(())
    }
}

/// The menu for a row, if it has one.
pub fn for_row(row: &Row, ctx: &mut Ctx) -> Option<Box<dyn Screen>> {
    if let Some(song) = row.song_item() {
        return Some(Box::new(song_menu(song.clone(), ctx)));
    }
    let RowAction::Open(dest) = &row.action else { return None };
    match dest {
        Dest::Album { id, name } => Some(Box::new(collection_menu(dest.clone(), "album", id.clone(), name.clone(), row.art.clone(), ctx))),
        Dest::Playlist { id, name } => {
            Some(Box::new(collection_menu(dest.clone(), "playlist", id.clone(), name.clone(), row.art.clone(), ctx)))
        }
        Dest::Artist { id, name } => {
            let id = id.clone();
            Some(Box::new(Popup::new(name.clone()).add("Love Artist", move |ctx| {
                ctx.music_action(ctx.lib.star(ItemKind::Artist, &id, true), Some("Loved".into()));
                Action::None
            })))
        }
        _ => None,
    }
}

pub fn song_menu(song: Song, ctx: &Ctx) -> Popup {
    let popup = Popup::new(song.title.clone());
    song_menu_from(song, ctx, popup)
}

pub(super) fn song_menu_from(song: Song, ctx: &Ctx, popup: Popup) -> Popup {
    if song.spoken.is_some() {
        let target = crate::library::book_info::BookInfoTarget::from_song(&song);
        let episode=super::spoken::is_episode(&song);
        let title = song.title.clone();
        let mut popup = Popup::new(&title).add(if episode {"Resume Episode"} else {"Resume Audiobook"}, move |ctx| {
            super::spoken::start_selection(super::spoken::Selection::Local(song.clone()),song.title.clone(),ctx)
        });
        if let Some(target) = target {
            popup = popup.add(if episode {"Episode Info"} else {"Book Info"}, move |ctx| Action::Push(super::open(Dest::BookInfo { target: target.clone(), title: title.clone() }, ctx)));
        }
        return popup;
    }

    let s1 = song.clone();
    let s2 = song.clone();
    let s3 = song.clone();
    let s4 = song.clone();
    let s5 = song.clone();
    let loved = ctx.lib.loved(&song);
    let mut p = popup.add("Play Next", move |ctx| {
            ctx.player.send(Cmd::Insert { songs: vec![s1.clone()], next: true });
            Action::Toast("Playing next".into())
        })
        .add("Add to Queue", move |ctx| {
            ctx.player.send(Cmd::Insert { songs: vec![s2.clone()], next: false });
            Action::Toast("Added to the queue".into())
        })
        .add(if loved { "Unlove" } else { "Love" }, move |ctx| {
            ctx.music_action(ctx.lib.star(ItemKind::Song, &s3.id, !loved), Some(if loved { "Unloved" } else { "Loved" }.into()));
            Action::None
        });
    let s6 = song.clone();
    p = p.add("Rate", move |ctx| Action::Push(Box::new(rating_menu(&s6, ctx.lib.rating(&s6)))));
    if let Some(album_id) = song.album_id.clone() {
        let name = song.album.clone();
        p = p.add("Go to Album", move |ctx| Action::Push(super::open(Dest::Album { id: album_id.clone(), name: name.clone() }, ctx)));
    }
    if let Some(artist_id) = song.artist_id.clone() {
        let name = song.artist.clone();
        p = p.add("Go to Artist", move |ctx| Action::Push(super::open(Dest::Artist { id: artist_id.clone(), name: name.clone() }, ctx)));
    }
    if ctx.lib.is_online() {
        p = p.add("Similar Songs", move |ctx| {
            Action::Push(super::open(Dest::Similar { id: s4.id.clone(), title: s4.title.clone() }, ctx))
        });
    }
    p = p.download_choice(ctx, None, song.id.clone(), move |downloaded| {
        if downloaded { return None; }
        let handler: Handler = Box::new(move |ctx| {
            let set = offline_set("song", &s5.id, &s5.title, &s5.artist, s5.cover_art.clone(), std::slice::from_ref(&s5));
            super::sources::queue_download(ctx, set, vec![s5.clone()])
        });
        Some(("Download".into(), handler))
    });
    p
}

fn collection_menu(dest: Dest, kind: &'static str, id: String, name: String, art: Option<String>, ctx: &Ctx) -> Popup {
    // A failed or superseded fetch says so on the screen that is open.
    let play = |shuffle: bool, d: Dest| {
        move |ctx: &mut Ctx| {
            let chosen = Chosen::now(ctx.player, ctx.lib);
            let (player, lib, d) = (ctx.player.clone(), ctx.lib.clone(), d.clone());
            ctx.music_action(async move {
                let songs = songs_of(lib.clone(), d).await;
                chosen.play(&player, &lib, songs, shuffle)
            }, None);
            Action::NowPlaying
        }
    };
    let queue = |next: bool, d: Dest| {
        move |ctx: &mut Ctx| {
            let (player, lib, d) = (ctx.player.clone(), ctx.lib.clone(), d.clone());
            ctx.music_action(async move {
                let songs = songs_of(lib, d).await?;
                player.send(Cmd::Insert { songs, next });
                Ok(())
            }, None);
            Action::Toast(if next { "Playing next" } else { "Added to the queue" }.into())
        }
    };
    let mut p = Popup::new(name.clone())
        .add("Play", play(false, dest.clone()))
        .add("Shuffle", play(true, dest.clone()))
        .add("Play Next", queue(true, dest.clone()))
        .add("Add to Queue", queue(false, dest.clone()));
    let id2 = id.clone();
    p = p.download_choice(ctx, Some(kind.into()), id.clone(), move |downloaded| {
        if downloaded {
            let handler: Handler = Box::new(move |ctx| super::sources::remove_download(ctx, kind, &id2));
            return Some(("Remove Download".into(), handler));
        }

        let d = dest.clone();
        let handler: Handler = Box::new(move |ctx| {
            let (id, name, art) = (id2.clone(), name.clone(), art.clone());
            let action=match ctx.lib.begin_music_download(kind,&id) {Ok(action)=>action,Err(error)=>return Action::Toast(error)};
            let lib=ctx.lib.clone();let d=d.clone();
            ctx.notice_task(async move {
                let got=match &d {
                    Dest::Album {id,..}=>lib.clone().fetch_async(calls::album(id),Policy::NetworkFirst).await.map(|f|(f.value.song,f.value.album.artist)),
                    Dest::Playlist {id,..}=>lib.clone().fetch_async(calls::playlist(id),Policy::NetworkFirst).await.map(|f|(f.value.entry,f.value.playlist.owner.unwrap_or_default())),
                    _=>return "This collection cannot be downloaded".into(),
                };
                match got {
                    Ok((songs,by)) if !songs.is_empty()=>{
                        let count=songs.len();
                        let set=offline_set(kind,&id,&name,&by,art,&songs);
                        match lib.download_requested_async(action,set,songs).await {
                            Ok(())=>format!("Queued {count} songs for download"),Err(error)=>error,
                        }
                    },
                    Ok(_)=>"No songs to download".into(),Err(_)=>"Could not load songs for download".into(),
                }
            });Action::Toast("Loading download…".into())
        });
        Some(("Download".into(), handler))
    });
    if kind == "album" {
        p = p.add("Love Album", move |ctx| {
            ctx.music_action(ctx.lib.star(ItemKind::Album, &id, true), Some("Loved".into()));
            Action::None
        });
    }
    p
}

/// Rating a song, 1 to 5 (Navidrome's stars; 0 clears it).
fn rating_menu(song: &Song, current: u8) -> Popup {
    let mut p = Popup::new(format!("Rate {}", song.title));
    for r in (0..=5u8).rev() {
        let id = song.id.clone();
        let label = match r {
            0 => "No Rating".to_string(),
            1 => "1 Star".to_string(),
            n => format!("{n} Stars"),
        };
        let label = if r == current { format!("{label} (now)") } else { label };
        p = p.add(&label, move |ctx| {
            ctx.music_action(ctx.lib.rate(&id, r), Some(if r == 0 { "Rating cleared".into() } else { format!("Rated {r}") }));
            Action::None
        });
    }
    p
}

#[cfg(test)]
mod storage_tests {
    use super::*;
    use crate::ui::UiMsg;
    use std::time::Duration;

    fn collection(app: &mut crate::ui::App) -> Popup {
        app.with_screen(app.top_id(), |_, ctx| collection_menu(
            Dest::Album { id: "generated-album".into(), name: "Generated".into() },
            "album", "generated-album".into(), "Generated".into(), None, ctx,
        )).unwrap()
    }
    fn result(app: &mut crate::ui::App, screen: u64, result: Result<bool, crate::library::LibError>) {
        app.handle(UiMsg::Loaded { screen, token: 1, data: Box::new(result) }, &mut crate::platform::Headless { last: None });
    }
    fn labels(app: &crate::ui::App) -> Vec<String> {
        app.stack.last().unwrap().1.audit().rows().iter().map(|row| row.title.clone()).collect()
    }
    fn step(app: &mut crate::ui::App, event: Event) { app.event(event, &mut crate::platform::Headless { last: None }); }

    #[test]
    fn popup_download_lookup_yields_preserves_selection_and_back_discards_reply() {
        let mut app = crate::ui::settings_tests::fixture();
        app.background = false;
        let parent = app.top_id();
        let (entered, seen) = std::sync::mpsc::channel();
        let (release, resume) = std::sync::mpsc::channel();
        let (done, completed) = std::sync::mpsc::channel();
        app.lib.db_post(move |_, _| {
            entered.send(()).unwrap();
            done.send(resume.recv_timeout(Duration::from_secs(5)).is_ok()).unwrap();
        });
        seen.recv_timeout(Duration::from_secs(3)).unwrap();
        let lookup = app.lib.download_choice(app.lib.music_account_generation(), Some("album".into()), "generated-album".into());
        let popup = collection(&mut app);
        app.push(Box::new(popup));
        let screen = app.top_id();
        for _ in 0..4 { step(&mut app, Event::Scroll(1)); step(&mut app, Event::WheelEnd); }
        assert!(labels(&app).contains(&"Checking download…".into()));
        step(&mut app, Event::Center);
        assert_eq!(app.top_id(), screen, "unknown membership cannot activate or close the menu");
        let _ = release.send(());
        assert!(completed.recv_timeout(Duration::from_secs(3)).unwrap(), "opening/input waited for SQLite");
        let loaded = crate::rt::Ui::new().unwrap().block_on(lookup);
        result(&mut app, screen, loaded);
        assert!(labels(&app).contains(&"Download".into()));
        assert_eq!(app.stack.last().unwrap().1.audit().selected(), 4);
        step(&mut app, Event::Back);
        assert_eq!(app.top_id(), parent);
        let popup = collection(&mut app);
        app.push(Box::new(popup)); let dismissed = app.top_id();
        step(&mut app, Event::Back);
        result(&mut app, dismissed, Ok(true));
        assert_eq!(app.top_id(), parent, "late lookup cannot resurrect a dismissed menu");
        app.player.shutdown();
    }

    #[test]
    fn popup_membership_keeps_routes_but_rejects_account_replacement_and_storage_failure() {
        let mut app = crate::ui::settings_tests::fixture();
        app.background = false;
        let mut config = app.config.clone();
        config.server.url = "http://127.0.0.1:1/generated".into();
        config.server.user = "generated-a".into();
        app.lib.configure(&config);
        app.lib.db().put_offline_set(&crate::rt::Blocking::dedicated_thread(), &crate::library::db::OfflineSet {
            kind: "album".into(), id: "generated-album".into(), name: "Generated".into(),
            artist: String::new(), cover_art: None, song_ids: vec![],
        });
        let account = app.lib.music_account_generation();
        let popup = collection(&mut app); app.push(Box::new(popup)); let screen = app.top_id();
        config.tailscale.enabled = true; app.lib.configure(&config);
        assert_eq!(app.lib.music_account_generation(), account);
        let loaded = crate::rt::Ui::new().unwrap().block_on(app.lib.download_choice(account, Some("album".into()), "generated-album".into()));
        result(&mut app, screen, loaded);
        assert!(labels(&app).contains(&"Remove Download".into()));
        step(&mut app, Event::Back);
        let popup = collection(&mut app); app.push(Box::new(popup)); let screen = app.top_id();
        config.server.user = "generated-b".into(); app.lib.configure(&config);
        config.server.user = "generated-a".into(); app.lib.configure(&config);
        result(&mut app, screen, Ok(true));
        assert!(labels(&app).contains(&"Navidrome changed; reopen menu".into()));
        assert!(!labels(&app).contains(&"Remove Download".into()));
        step(&mut app, Event::Back);
        let popup = collection(&mut app); app.push(Box::new(popup)); let screen = app.top_id();
        app.lib.db().reject_offline_set_reads(&crate::rt::Blocking::dedicated_thread(), true);
        let loaded = crate::rt::Ui::new().unwrap().block_on(app.lib.download_choice(app.lib.music_account_generation(), Some("album".into()), "generated-album".into()));
        app.lib.db().reject_offline_set_reads(&crate::rt::Blocking::dedicated_thread(), false);
        assert!(loaded.is_err());
        result(&mut app, screen, loaded);
        assert!(labels(&app).contains(&"Downloads unavailable".into()));
        assert!(!labels(&app).contains(&"Download".into()));
        app.player.shutdown();
    }
}

#[cfg(test)]
mod play_tests {
    use super::*;
    use crate::audio::player::{PlayerHandle, Repeat};

    fn songs(name: &str) -> Vec<Song> {
        (0..3).map(|i| Song { id: format!("{name}-{i}"), title: format!("{name} {i}"), ..Song::default() }).collect()
    }
    fn first(player: &PlayerHandle) -> Option<String> { player.status().queue.first().map(|song| song.id.clone()) }
    /// Commands are handled in order: once the repeat mode shows, every
    /// command sent before it has been handled.
    fn settle(player: &PlayerHandle, repeat: Repeat) {
        player.send(Cmd::SetRepeat(repeat));
        let start = std::time::Instant::now();
        while player.status().repeat != repeat {
            assert!(start.elapsed() < crate::library::TEST_LIVENESS, "the player handles its commands");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    /// Finding 24 (code review round 2): Play or Shuffle from an album's or
    /// playlist's menu fetched the songs detached and sent them whenever
    /// they came, over a queue the listener started since.
    #[test]
    fn a_late_collection_reply_never_replaces_a_queue_or_choice_made_since() {
        let app = crate::ui::settings_tests::fixture();
        let (player, lib) = (app.player.clone(), app.lib.clone());
        let changed = Err("Playback changed; choose Play again".to_string());

        let album = Chosen::now(&player, &lib);
        player.send(Cmd::Play { songs: Arc::new(songs("generated-started")), index: 0, shuffle: false });
        settle(&player, Repeat::All);
        assert_eq!(album.play(&player, &lib, Ok(songs("generated-album")), false), changed, "a queue started since wins");
        settle(&player, Repeat::Off);
        assert_eq!(first(&player).as_deref(), Some("generated-started-0"));

        let earlier = Chosen::now(&player, &lib);
        let later = Chosen::now(&player, &lib);
        assert_eq!(earlier.play(&player, &lib, Ok(songs("generated-earlier")), false), changed, "a later choice wins");
        assert_eq!(later.play(&player, &lib, Ok(songs("generated-later")), true), Ok(()));
        settle(&player, Repeat::All);
        assert!(player.status().queue.iter().all(|song| song.id.starts_with("generated-later-")));

        let failed = Chosen::now(&player, &lib);
        assert_eq!(failed.play(&player, &lib, Err("Could not load the album".into()), false), Err("Could not load the album".into()));
        let mut config = app.config.clone();
        config.server.url = "http://127.0.0.1:1/generated".into();
        config.server.user = "generated-a".into();
        lib.configure(&config);
        let account = Chosen::now(&player, &lib);
        config.server.user = "generated-b".into();
        lib.configure(&config);
        assert_eq!(account.play(&player, &lib, Ok(songs("generated-account")), false), changed, "another account's songs never play");
        settle(&player, Repeat::Off);
        assert!(player.status().queue.iter().all(|song| song.id.starts_with("generated-later-")));
        player.shutdown();
    }
}
