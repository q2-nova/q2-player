//! The library's pages.

use super::menu::{PageTask, Rows, RowsTask, WindowPageTask, Source, on_pool, ready};
use crate::audio::player::{Cmd, PlayerHandle};
use crate::library::db::OfflineSet;
use crate::library::{LibError, LibEvent, Library, Policy};
use crate::ui::list::{Badge, Row, RowAction};
use crate::ui::music_rows::{album_row, artist_row, track_row};
use crate::ui::{Action, Ctx, Dest, long_duration};
use std::sync::{Arc, Mutex};
use subsonic::{Album, AlbumListType, Song, calls};
use subsonic::AlbumListType::{AlphabeticalByArtist, AlphabeticalByName};

/// Publish a whole immutable page payload. Only pointer swaps/clones hold the
/// shared lock; replacing an old page cannot destroy its song list under it.
struct SharedSnapshot<T>(Mutex<Arc<T>>);
impl<T> SharedSnapshot<T> {
    fn new(value: T) -> Self { Self(Mutex::new(Arc::new(value))) }
    fn get(&self) -> Arc<T> { self.0.lock().unwrap().clone() }
    fn set(&self, value: T) {
        let value = Arc::new(value);
        let retired = std::mem::replace(&mut *self.0.lock().unwrap(), value);
        drop(retired);
    }
}

/// Admit the user's request before deferred preparation. A later remove or
/// account change still supersedes this request while metadata is being cloned.
fn queue_snapshot_download(ctx: &mut Ctx, kind: &str, id: &str,
    prepare: impl FnOnce(&crate::rt::Blocking) -> (OfflineSet, Vec<Song>) + Send + 'static) -> Action {
    let action = match ctx.lib.begin_music_download(kind, id) {
        Ok(action) => action, Err(error) => return Action::Toast(error),
    };
    let lib = ctx.lib.clone();
    ctx.notice_task(async move {
        match prepare_snapshot_download(lib, action, prepare).await {
            Ok(count) => format!("Queued {count} songs for download"), Err(error) => error,
        }
    });
    Action::Toast("Queueing download…".into())
}
async fn prepare_snapshot_download(lib: Arc<Library>, action: crate::library::MusicDownloadAction,
    prepare: impl FnOnce(&crate::rt::Blocking) -> (OfflineSet, Vec<Song>) + Send + 'static) -> Result<usize, String> {
    let (set, songs) = crate::rt::jobs().run_isolated(prepare).await
        .map_err(|_| "Unable to prepare download".to_owned())?;
    let count = songs.len();
    lib.download_requested_async(action, set, songs).await?;
    Ok(count)
}

/// Register user ordering now; queue and card mutations run off the UI thread.
pub(super) fn queue_download(ctx:&mut Ctx,set:OfflineSet,songs:Vec<Song>)->Action {
    let action=match ctx.lib.begin_music_download(&set.kind,&set.id) {Ok(action)=>action,Err(error)=>return Action::Toast(error)};
    let lib=ctx.lib.clone();let count=songs.len();
    ctx.notice_task(async move {match lib.download_requested_async(action,set,songs).await {
        Ok(())=>format!("Queued {count} songs for download"),Err(error)=>error,
    }});Action::Toast("Queueing download…".into())
}
pub(super) fn remove_download(ctx:&mut Ctx,kind:&str,id:&str)->Action {
    let action=match ctx.lib.begin_music_download(kind,id) {Ok(action)=>action,Err(error)=>return Action::Toast(error)};
    let lib=ctx.lib.clone();let kind=kind.to_owned();let id=id.to_owned();
    ctx.notice_task(async move {match lib.remove_download_requested_async(action,kind,id).await {Ok(())=>"Download removed".into(),Err(error)=>error}});
    Action::Toast("Removing download…".into())
}

/// A download state as a row badge.
pub fn download_badge(state: crate::library::DownloadState) -> Badge {
    use crate::library::DownloadState::*;
    match state {
        None => Badge::None,
        Done => Badge::Offline,
        Queued => Badge::Queued,
        Active(f) => Badge::Downloading(f.map_or(255, |f| (f * 100.0).round().min(100.0) as u8)),
    }
}

pub(super) async fn album_rows(lib: Arc<Library>, albums: Vec<Album>, show_artist: bool) -> Result<Vec<Row>, LibError> {
    let view = lib.download_view_async(lib.client_generation()).await.ok_or(LibError::Offline)?;
    Ok(on_pool(lib, move |_| albums
        .iter()
        .map(|a| {
            let detail = match (show_artist, a.year) {
                (true, _) => a.artist.clone(),
                (false, Some(y)) if y > 0 => y.to_string(),
                _ => format!("{} songs", a.song_count),
            };
            album_row(a, detail).badge(download_badge(view.album(&a.id)))
        })
        .collect()).await)
}

#[derive(Clone, Copy)]
pub struct SongStyle {
    pub art: bool,
    pub track_numbers: bool,
}

/// Rows playing `songs` from the chosen one, dimmed if they can't play now.
async fn song_rows(lib: Arc<Library>, songs: Vec<Song>, style: SongStyle) -> Result<Vec<Row>, LibError> {
    let snapshot = lib.music_rows_snapshot(songs).await?;
    Ok(on_pool(lib, move |_| build_song_rows(snapshot, style)).await)
}
fn build_song_rows(snapshot: crate::library::MusicRowSnapshot, style: SongStyle) -> Vec<Row> {
    let crate::library::MusicRowSnapshot { songs, view, local, online } = snapshot;
    let shared = Arc::new(songs);
    shared
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let downloaded = view.offline.contains(&s.id);
            track_row(&shared, i, style.art, style.track_numbers)
                .dim(!online && !local.contains(&s.id))
                .badge(download_badge(view.song(&s.id, downloaded)))
        })
        .collect()
}

fn shuffle_row(songs: &[Song]) -> Row {
    Row::new("Shuffle", RowAction::Shuffle(Arc::new(songs.to_vec()))).key("#shuffle")
}

/// The songs that `song_rows` rows share.
fn shared_songs(rows: &[Row]) -> Arc<Vec<Song>> {
    rows.iter().find_map(|r| match &r.action { RowAction::Play { songs, .. } => Some(songs.clone()), _ => None }).unwrap_or_default()
}

/// `song_rows` with a Shuffle row first that shares their songs.
fn with_shuffle(mut rows: Vec<Row>) -> Vec<Row> {
    rows.insert(0, Row::new("Shuffle", RowAction::Shuffle(shared_songs(&rows))).key("#shuffle"));
    rows
}

/// Music, as the classic's menu, then Navidrome's own lists.
pub struct MusicMenu;

impl Source for MusicMenu {
    fn title(&self) -> String {
        "Music".into()
    }
    fn local(&self) -> bool {
        true
    }
    fn load(self: Arc<Self>, _lib: Arc<Library>, _p: Policy) -> RowsTask {
        ready(Ok(super::music_menu::rows()))
    }
}

/// Every album, for Cover Flow: by artist, as the classic orders it.
pub async fn all_albums(lib: Arc<Library>, p: Policy) -> Result<Vec<Album>, LibError> {
    let page = 500;
    let mut all = Vec::new();
    for i in 0..40 {
        let got = lib.clone().fetch_async(calls::album_list(AlbumListType::AlphabeticalByArtist, page, i * page), p).await?.value.album;
        let n = got.len();
        all.extend(got);
        if n < page as usize {
            break;
        }
    }
    Ok(all)
}

pub struct Artists;

impl Source for Artists {
    fn title(&self) -> String {
        "Artists".into()
    }
    fn empty(&self) -> &'static str { "No artists" }
    fn media(&self) -> bool { true }
    fn load(self: Arc<Self>, lib: Arc<Library>, p: Policy) -> RowsTask {
        Box::pin(async move {
            let idx = lib.fetch_async(calls::artists(), p).await?.value;
            Ok(idx.all().map(artist_row).collect())
        })
    }
}

pub struct Artist {
    pub id: String,
    pub name: String,
}

impl Source for Artist {
    fn title(&self) -> String {
        self.name.clone()
    }
    fn media(&self) -> bool {
        true
    }
    fn load(self: Arc<Self>, lib: Arc<Library>, p: Policy) -> RowsTask {
        Box::pin(async move {
            let mut a = lib.clone().fetch_async(calls::artist(&self.id), p).await?.value;
            a.album.sort_by(|x, y| y.year.cmp(&x.year).then_with(|| x.name.cmp(&y.name)));
            let mut rows = vec![Row::link("Top Songs", Dest::TopSongs { artist: self.name.clone() }).key("#top").artist_art(&a.artist)];
            rows.extend(album_rows(lib, a.album, false).await?);
            Ok(rows)
        })
    }
}

pub struct Albums {
    pub kind: AlbumListType,
    pub title: String,
    random_native: std::sync::atomic::AtomicBool,
    alphabetical: alphabetical::AlphabeticalAlbums,
    random_seed: u32,
}

impl Albums {
    pub fn new(kind: AlbumListType, title: String) -> Albums {
        Albums { kind, title, random_native: std::sync::atomic::AtomicBool::new(false), alphabetical: alphabetical::AlphabeticalAlbums::new(), random_seed: fastrand::u32(..) }
    }

    async fn random_page(&self, lib: Arc<Library>, offset: u32, policy: Policy) -> Result<(Vec<Row>, bool), LibError> {
        // A legacy standard random cache is only a first-visit preview, never
        // replay evidence for an established native seed (including page zero).
        if policy == Policy::CacheOnly && self.random_native.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(LibError::Offline);
        }
        let generation = lib.client_generation();
        let page = offset / 100;
        let (albums, native) = lib.random_album_page(self.random_seed, page, policy).await?;
        if lib.client_generation() != generation { return Err(LibError::Offline); }
        self.random_native.store(native, std::sync::atomic::Ordering::Relaxed);
        let more = if native { albums.len() == 100 } else { !albums.is_empty() };
        let mut rows = album_rows(lib.clone(), albums, true).await?;
        if lib.client_generation() != generation { return Err(LibError::Offline); }
        // A repeated draw is legitimate on generic servers. Give occurrences
        // distinct identities while the window retains exact five-page history.
        if !native { for row in &mut rows { row.key = format!("random:{page}:{}", row.key); } }
        Ok((rows, more))
    }
}

#[path = "sources_alphabetical.rs"]
mod alphabetical;

const CMD_SHUFFLE_ALBUMS: u32 = 11;
/// Shuffle Albums queues whole albums until about this many songs.
const SHUFFLE_ALBUM_SONGS: usize = 150;

/// An album's songs in order, from the server or its download.
async fn album_songs(lib: Arc<Library>, id: String, p: Policy) -> Result<subsonic::AlbumWithSongs, LibError> {
    let mut album = match lib.clone().fetch_async(calls::album(&id), p).await {
        Ok(f) => f.value,
        // Downloaded albums play without the server.
        Err(e) => match lib.offline_collection_async("album".into(), id).await?.map(|(set, songs)| subsonic::AlbumWithSongs {
            album: Album { id: set.id.clone(), name: set.name.clone(), artist: set.artist.clone(), cover_art: set.cover_art.clone(), ..Default::default() },
            song: songs,
        }) {
            Some(album) => album,
            None => return Err(e),
        },
    };
    for song in &mut album.song {
        if song.cover_art.as_deref().is_none_or(|id| id.is_empty()) {
            song.cover_art = album.album.cover_art.clone();
        }
    }
    Ok(album)
}

/// Sample the whole server catalog independently of the visible album window.
/// Retain complete album track order and stop at an album boundary after 150 songs.
async fn shuffled_albums_online(lib: Arc<Library>) -> Result<Vec<Song>, LibError> {
    let generation = lib.music_account_generation();
    let albums = lib.clone().fetch_async(calls::album_list(AlbumListType::Random, 500, 0), Policy::NetworkOnly).await?.value.album;
    let mut seen = std::collections::HashSet::new();
    let mut songs = Vec::new();
    let mut error = None;
    for album in albums {
        if lib.music_account_generation() != generation { return Err(LibError::Offline); }
        if songs.len() >= SHUFFLE_ALBUM_SONGS { break; }
        if !seen.insert(album.id.clone()) { continue; }
        match album_songs(lib.clone(), album.id, Policy::PreferCache).await {
            Ok(album) => songs.extend(album.song),
            Err(e) => if error.is_none() { error = Some(e); },
        }
    }
    if songs.is_empty() { if let Some(error) = error { return Err(error); } }
    Ok(songs)
}

impl Source for Albums {
    fn alphabetical(&self) -> bool { matches!(self.kind, AlbumListType::AlphabeticalByName) }
    fn empty(&self) -> &'static str { "No albums" }
    fn title(&self) -> String {
        self.title.clone()
    }
    fn media(&self) -> bool {
        true
    }
    fn load(self: Arc<Self>, lib: Arc<Library>, p: Policy) -> RowsTask {
        Box::pin(async move {
            if matches!(self.kind, AlbumListType::Random) { return self.random_page(lib, 0, p).await.map(|(rows, _)| rows); }
            if matches!(self.kind, AlphabeticalByName) {
                return self.alphabetical.page(lib, 0, p, CMD_SHUFFLE_ALBUMS).await.map(|page| page.rows);
            }
            let all = lib.clone().fetch_async(calls::album_list(self.kind, 100, 0), p).await?.value.album;
            album_rows(lib, all, true).await
        })
    }
    fn paged(&self) -> bool { true }
    fn refetch_history(&self) -> bool {
        !matches!(self.kind, AlbumListType::Random) || self.random_native.load(std::sync::atomic::Ordering::Relaxed)
    }
    fn refresh_retained(&self) -> bool { !matches!(self.kind, AlbumListType::Random) }
    fn global_alphabet(&self) -> bool { matches!(self.kind, AlbumListType::AlphabeticalByName) }
    fn alphabet_jump(self: Arc<Self>, lib: Arc<Library>, position: u32, delta: i32) -> super::menu::JumpTask {
        Box::pin(async move { self.alphabetical.jump(lib, position, delta).await })
    }
    fn windowed(&self) -> bool { self.paged() }
    fn window_page(self: Arc<Self>, lib: Arc<Library>, page: u32, policy: Policy) -> WindowPageTask {
        Box::pin(async move {
            if matches!(self.kind, AlphabeticalByName) { return self.alphabetical.page(lib, page, policy, CMD_SHUFFLE_ALBUMS).await; }
            let offset = page.checked_mul(100).ok_or_else(|| LibError::Server("Album offset exceeds server range".into()))?;
            if matches!(self.kind, AlbumListType::Random) { return self.random_page(lib, offset, policy).await.map(Into::into); }
            let albums = lib.clone().fetch_async(calls::album_list(self.kind, 100, offset), policy).await?.value.album;
            let more = albums.len() == 100;
            Ok((album_rows(lib, albums, true).await?, more).into())
        })
    }
    /// Compatibility path; the menu uses bounded windows for these lists.
    fn page(self: Arc<Self>, lib: Arc<Library>, page: u32) -> Option<PageTask> {
        let policy = self.refresh_policy();
        Some(Box::pin(async move {
            self.window_page(lib, page, policy).await.map(|page| (page.rows, page.more))
        }))
    }

    fn cmd(&self, cmd: u32, _row: &Row, ctx: &mut Ctx) -> Action {
        if cmd != CMD_SHUFFLE_ALBUMS { return Action::None; }
        crate::ux::begin(crate::ux::Journey::ShuffleAlbums);
        Action::Push(Box::new(ShufflePreparation::albums(ctx.lib.is_online())))
    }
}

/// An album's songs; also remembered for the download command.
pub struct AlbumPage {
    pub id: String,
    pub name: String,
    loaded: SharedSnapshot<Option<Arc<subsonic::AlbumWithSongs>>>,
}

impl AlbumPage {
    pub fn new(id: String, name: String) -> AlbumPage {
        AlbumPage { id, name, loaded: SharedSnapshot::new(None) }
    }

    /// The page's rows from awaited owned storage state.
    async fn rows(&self, lib: Arc<Library>, album: subsonic::AlbumWithSongs) -> Result<Vec<Row>, LibError> {
        let songs = album.song.clone();
        let mut rows = with_shuffle(song_rows(lib.clone(), songs, SongStyle { art: false, track_numbers: true }).await?);
        let downloaded = lib.download_choice(lib.music_account_generation(), Some("album".into()), self.id.clone()).await?;
        rows.push(if downloaded {
            Row::new("Remove Download", RowAction::Cmd(CMD_REMOVE)).key("#remove")
        } else {
            Row::new("Download Album", RowAction::Cmd(CMD_DOWNLOAD)).key("#download")
        });
        if let Some(artist_id) = &album.album.artist_id {
            rows.push(
                Row::link(format!("More by {}", album.album.artist), Dest::Artist {
                    id: artist_id.clone(),
                    name: album.album.artist.clone(),
                })
                .key("#artist"),
            );
        }
        self.loaded.set(Some(Arc::new(album)));
        Ok(rows)
    }
}

const CMD_DOWNLOAD: u32 = 1;
const CMD_REMOVE: u32 = 2;
const CMD_FOLLOW_PLAYLIST: u32 = 30;
const CMD_UNFOLLOW_PLAYLIST: u32 = 31;

impl Source for AlbumPage {
    fn refresh_policy(&self) -> Policy { Policy::PreferCache }
    fn title(&self) -> String {
        self.name.clone()
    }
    fn load(self: Arc<Self>, lib: Arc<Library>, p: Policy) -> RowsTask {
        Box::pin(async move {
            let album = album_songs(lib.clone(), self.id.clone(), p).await?;
            self.rows(lib, album).await
        })
    }

    fn cmd(&self, cmd: u32, _row: &Row, ctx: &mut Ctx) -> Action {
        let Some(album) = self.loaded.get().as_ref().clone() else { return Action::None };
        match cmd {
            CMD_DOWNLOAD => {
                queue_snapshot_download(ctx, "album", &album.album.id.clone(), move |_| {
                    let set = offline_set("album", &album.album.id, &album.album.name, &album.album.artist, album.album.cover_art.clone(), &album.song);
                    (set, album.song.clone())
                })
            }
            CMD_REMOVE => {
                remove_download(ctx,"album",&album.album.id)
            }
            _ => Action::None,
        }
    }
    fn reload_on(&self, ev: &LibEvent) -> bool {
        matches!(ev, LibEvent::Online(_))
    }
}

pub fn offline_set(kind: &str, id: &str, name: &str, artist: &str, cover: Option<String>, songs: &[Song]) -> OfflineSet {
    OfflineSet {
        kind: kind.into(),
        id: id.into(),
        name: name.into(),
        artist: artist.into(),
        cover_art: cover,
        song_ids: songs.iter().map(|s| s.id.clone()).collect(),
    }
}

pub struct Playlists;

impl Source for Playlists {
    fn title(&self) -> String {
        "Playlists".into()
    }
    fn media(&self) -> bool {
        true
    }
    fn load(self: Arc<Self>, lib: Arc<Library>, p: Policy) -> RowsTask {
        Box::pin(async move {
        let pls = lib.clone().fetch_async(calls::playlists(), p).await?.value.playlist;
        let offline: std::collections::HashSet<String> = lib.offline_sets_async().await?
            .into_iter().filter(|s| s.kind == "playlist").map(|s| s.id).collect();
        Ok(pls
            .iter()
            .map(|pl| {
                Row::link(pl.name.clone(), Dest::Playlist { id: pl.id.clone(), name: pl.name.clone() })
                    .key(pl.id.clone())
                    .detail(format!("{} songs · {}", pl.song_count, long_duration(pl.duration as u64)))
                    .art(pl.cover_art.clone())
                    .badge(if offline.contains(&pl.id) { Badge::Offline } else { Badge::None })
            })
            .collect())
        })
    }
    fn empty(&self) -> &'static str {
        "No playlists"
    }
}

pub struct PlaylistPage {
    pub id: String,
    pub name: String,
    loaded: SharedSnapshot<Option<Arc<(subsonic::Playlist, Arc<Vec<Song>>)>>>,
}

impl PlaylistPage {
    pub fn new(id: String, name: String) -> PlaylistPage {
        PlaylistPage { id, name, loaded: SharedSnapshot::new(None) }
    }
}

impl PlaylistPage {
    /// The page's rows, from the server's answer or the download.
    async fn rows(&self, lib: Arc<Library>, fetched: Result<subsonic::PlaylistWithSongs, LibError>) -> Result<Vec<Row>, LibError> {
        let pl = match fetched {
            Ok(pl) => pl,
            Err(e) => match lib.offline_collection_async("playlist".into(), self.id.clone()).await? {
                Some((set, songs)) => subsonic::PlaylistWithSongs {
                    playlist: subsonic::Playlist { id: set.id.clone(), name: set.name.clone(), ..Default::default() },
                    entry: songs,
                },
                None => return Err(e),
            },
        };
        // The rows, the Shuffle row and the download command share one copy
        // of the songs.
        let subsonic::PlaylistWithSongs { playlist, entry } = pl;
        let mut rows = with_shuffle(song_rows(lib.clone(), entry, SongStyle { art: true, track_numbers: false }).await?);
        let songs = shared_songs(&rows);
        let downloaded = lib.download_choice(lib.music_account_generation(), Some("playlist".into()), self.id.clone()).await?;
        rows.push(if downloaded {
            Row::new("Remove Download", RowAction::Cmd(CMD_REMOVE)).key("#remove")
        } else {
            Row::new("Download Playlist", RowAction::Cmd(CMD_DOWNLOAD)).key("#download")
        });
        rows.push(if lib.playlist_followed(playlist.id.clone()).await {
            Row::new("Stop Following New Songs", RowAction::Cmd(CMD_UNFOLLOW_PLAYLIST)).key("#unfollow")
        } else {
            Row::new("Download and Follow", RowAction::Cmd(CMD_FOLLOW_PLAYLIST)).key("#follow")
        });
        self.loaded.set(Some(Arc::new((playlist, songs))));
        Ok(rows)
    }
}

impl Source for PlaylistPage {
    fn reload_on(&self, event: &LibEvent) -> bool {
        matches!(event, LibEvent::Online(_)) || matches!(event, LibEvent::PlaylistFollowChanged(id) if *id == self.id)
    }
    fn refresh_policy(&self) -> Policy { Policy::PreferCache }
    fn title(&self) -> String {
        self.name.clone()
    }
    fn media(&self) -> bool {
        true
    }
    fn load(self: Arc<Self>, lib: Arc<Library>, p: Policy) -> RowsTask {
        Box::pin(async move {
            let fetched = lib.clone().fetch_async(calls::playlist(&self.id), p).await;
            self.rows(lib, fetched.map(|f| f.value)).await
        })
    }
    fn cmd(&self, cmd: u32, _row: &Row, ctx: &mut Ctx) -> Action {
        let Some(pl) = self.loaded.get().as_ref().clone() else { return Action::None };
        match cmd {
            CMD_DOWNLOAD => {
                queue_snapshot_download(ctx, "playlist", &pl.0.id.clone(), move |_| {
                    let (playlist, songs) = &*pl;
                    let owner = playlist.owner.as_deref().unwrap_or_default();
                    let set = offline_set("playlist", &playlist.id, &playlist.name, owner, playlist.cover_art.clone(), songs);
                    (set, songs.to_vec())
                })
            }
            CMD_REMOVE => {
                remove_download(ctx,"playlist",&pl.0.id)
            }
            CMD_FOLLOW_PLAYLIST | CMD_UNFOLLOW_PLAYLIST => {
                let action = match ctx.lib.begin_music_download("playlist", &pl.0.id) {
                    Ok(action) => action,
                    Err(error) => return Action::Toast(error),
                };
                let lib = ctx.lib.clone();
                ctx.notice_task(async move {
                    let prepared = crate::rt::spawn_blocking(move |_| subsonic::PlaylistWithSongs {
                        playlist: pl.0.clone(), entry: pl.1.to_vec(),
                    }).await;
                    let Ok(data) = prepared else { return "Unable to prepare playlist".into(); };
                    match lib.set_playlist_follow(action, data, cmd == CMD_FOLLOW_PLAYLIST).await {
                        Ok(()) if cmd == CMD_FOLLOW_PLAYLIST => "Following new songs".into(),
                        Ok(()) => "Stopped following".into(),
                        Err(error) => error,
                    }
                });
                Action::Toast("Updating playlist…".into())
            }
            _ => Action::None,
        }
    }
}

pub struct Genres;

impl Source for Genres {
    fn title(&self) -> String {
        "Genres".into()
    }
    fn empty(&self) -> &'static str { "No genres" }
    fn load(self: Arc<Self>, lib: Arc<Library>, p: Policy) -> RowsTask {
        Box::pin(async move {
        let mut g = lib.fetch_async(calls::genres(), p).await?.value.genre;
        g.sort_by_key(|x| x.value.to_lowercase());
        Ok(g
            .iter()
            .filter(|x| x.album_count > 0)
            .map(|x| Row::link(x.value.clone(), Dest::Genre { name: x.value.clone() }).value(x.album_count.to_string()))
            .collect())
        })
    }
}

pub struct GenrePage {
    pub name: String,
}

const CMD_SHUFFLE_GENRE: u32 = 10;

impl Source for GenrePage {
    fn title(&self) -> String {
        self.name.clone()
    }
    fn media(&self) -> bool {
        true
    }
    fn load(self: Arc<Self>, lib: Arc<Library>, p: Policy) -> RowsTask {
        Box::pin(async move {
            let albums = lib.clone()
                .fetch_async(calls::album_list_filtered(AlbumListType::ByGenre, 500, 0, Some(&self.name), None), p).await?
                .value
                .album;
            let mut rows = vec![Row::new(format!("Shuffle {}", self.name), RowAction::Cmd(CMD_SHUFFLE_GENRE)).key("#shuffle")];
            rows.extend(album_rows(lib, albums, true).await?);
            Ok(rows)
        })
    }
    fn paged(&self) -> bool { true }
    fn windowed(&self) -> bool { true }
    fn window_page(self: Arc<Self>, lib: Arc<Library>, page: u32, policy: Policy) -> WindowPageTask {
        Box::pin(async move {
            if page == 0 { return self.load(lib, policy).await.map(|rows| (rows, true).into()); }
            let offset = page.checked_mul(500).ok_or_else(|| LibError::Server("Genre offset exceeds server range".into()))?;
            let albums = lib.clone().fetch_async(calls::album_list_filtered(AlbumListType::ByGenre, 500, offset, Some(&self.name), None), policy).await?.value.album;
            let more = albums.len() == 500;
            Ok((album_rows(lib, albums, true).await?, more).into())
        })
    }
    /// A genre with more than 500 albums continues at the end (BROWSE-001).
    fn page(self: Arc<Self>, lib: Arc<Library>, page: u32) -> Option<PageTask> {
        let size = 500;
        Some(Box::pin(async move {
            let albums = lib.clone()
                .fetch_async(calls::album_list_filtered(AlbumListType::ByGenre, size, page * size, Some(&self.name), None), Policy::NetworkFirst).await?
                .value.album;
            let more = albums.len() == size as usize;
            Ok((album_rows(lib, albums, true).await?, more))
        }))
    }
    fn cmd(&self, cmd: u32, _row: &Row, ctx: &mut Ctx) -> Action {
        if cmd != CMD_SHUFFLE_GENRE {
            return Action::None;
        }
        shuffle_from(ctx, Some(self.name.clone()))
    }
}

/// Plays random songs (of a genre), fetched in the background.
pub fn shuffle_from(ctx: &mut Ctx, genre: Option<String>) -> Action {
    crate::ux::begin(crate::ux::Journey::ShuffleSongs);
    Action::Push(Box::new(ShufflePreparation {
        online: ctx.lib.is_online(),
        ..ShufflePreparation::new(genre)
    }))
}

pub(in crate::ui) enum ShuffleSelection {
    /// Start this local song while the remaining queue is fetched.
    Seed(Box<Song>),
    Queue(Vec<Song>),
}

/// Own asynchronous shuffle preparation so Back cancels further fetches and playback admission. Empty
/// results leave the original menu/queue intact rather than opening a blank player.
struct ShufflePreparation {
    genre: Option<String>,
    albums: bool,
    online: bool,
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
    action: Option<Action>,
    origin: Option<(String, String, u64, u32)>,
    /// The shared page deadline (UX-006): past it the wait is cancelled and says so.
    deadline: Option<crate::ui::state::Deadline>,
}
impl ShufflePreparation {
    fn new(genre: Option<String>) -> Self { Self { genre, albums: false, online: false, cancel: None, action: None, origin: None, deadline: None } }
    fn albums(online: bool) -> Self { Self { albums: true, online, ..Self::new(None) } }
    fn message(&self) -> &'static str { if self.online { if self.albums { "Preparing albums…" } else { "Preparing songs…" } } else { "Reading downloaded music…" } }
    fn label(&self) -> &'static str { if self.albums { "Shuffle Albums" } else { "Shuffle Songs" } }
}
impl crate::ui::Screen for ShufflePreparation {
    fn title(&self) -> String { self.label().into() }
    fn script_ready(&self) -> bool { false }
    fn event(&mut self, _event: crate::input::Event, _ctx: &mut Ctx) -> Action { Action::None }
    #[cfg(test)]
    fn audit(&self) -> crate::ui::hierarchy::Surface {
        crate::ui::hierarchy::Surface::widget(if self.online { "AlbumShuffle" } else if self.albums { "OfflineAlbumShuffle" } else { "OfflineShuffle" }, self.message().trim_end_matches('…').into())
    }
    fn enter(&mut self, ctx: &mut Ctx) {
        if self.origin.is_some() { return; }
        self.origin = Some((ctx.config.server.url.clone(), ctx.config.server.user.clone(), ctx.player.status().generation, ctx.lib.music_account_generation()));
        self.deadline = Some(crate::ui::state::Deadline::new(ctx.now));
        let lib = ctx.lib.clone(); let albums = self.albums; let online = self.online; let genre = self.genre.clone();
        let (cancel, cancelled) = tokio::sync::oneshot::channel();
        self.cancel = Some(cancel);
        // A user-started play never waits behind page loads.
        ctx.foreground_task(1, move |reply| async move {
            let work = async move {
                if online && albums { return shuffled_albums_online(lib).await.map(ShuffleSelection::Queue); }
                if albums { return lib.shuffled_downloaded_albums(fastrand::Rng::new()).await.map(ShuffleSelection::Queue); }
                // UX-004/UX-009: start at once from a random song on the card,
                // then queue the rest behind it: the server's random songs
                // online, every downloaded and cached song offline.
                let first = match lib.random_local_song_async(genre.clone()).await {
                    Some(song) => Some(song),
                    None => {
                        let mut local = lib.local_music_async(genre.clone()).await.unwrap_or_default();
                        (!local.is_empty()).then(|| local.swap_remove(fastrand::usize(..local.len())))
                    }
                };
                let Some(first) = first else {
                    if !online { return Ok(ShuffleSelection::Queue(Vec::new())); }
                    let songs = lib.clone().fetch_async(calls::random_songs(200, genre.as_deref()), Policy::NetworkOnly).await.map(|f| f.value.song);
                    crate::rt::io().spawn(lib.clone().fill_shuffle_reserve());
                    return songs.map(ShuffleSelection::Queue);
                };
                Ok(ShuffleSelection::Seed(Box::new(first)))
            };
            // Dropping the screen on Back drops the sender. Prefer cancellation
            // even if work has finished while waiting for a page-task permit.
            tokio::select! {
                biased;
                _ = cancelled => {},
                songs = work => reply.send(songs.map_err(|e| e.to_string())),
            }
        });
    }
    fn loaded(&mut self, token: u64, data: Box<dyn std::any::Any + Send>, ctx: &mut Ctx) {
        if token != 1 || self.action.is_some() { return; }
        let Ok(result) = data.downcast::<Result<ShuffleSelection, String>>() else { return };
        let mut actions = vec![Action::Dismiss(ctx.screen)];
        if self.origin.as_ref() != Some(&(ctx.config.server.url.clone(), ctx.config.server.user.clone(), ctx.player.status().generation, ctx.lib.music_account_generation())) {
            actions.push(Action::Toast(format!("Playback changed; choose {} again", self.label())));
        } else {
            match *result {
                Ok(ShuffleSelection::Seed(first)) => {
                    let seed = Arc::new(vec![*first]);
                    ctx.player.send(crate::audio::player::Cmd::Play { songs: seed.clone(), index: 0, shuffle: false });
                    actions.push(Action::NowPlaying);
                    let lib = ctx.lib.clone();
                    let player = ctx.player.clone();
                    let online = self.online;
                    let genre = self.genre.clone();
                    let account = lib.music_account_generation();
                    crate::rt::io().spawn(async move {
                        let remote = if online { lib.clone().fetch_async(calls::random_songs(200, genre.as_deref()), Policy::NetworkOnly).await.ok() } else { None };
                        let mut songs = match remote {
                            Some(found) => found.value.song,
                            None => {
                                let mut songs = lib.local_music_async(genre).await.unwrap_or_default();
                                fastrand::shuffle(&mut songs);
                                songs
                            }
                        };
                        songs.retain(|song| song.id != seed[0].id);
                        if !songs.is_empty() && lib.music_account_generation() == account {
                            player.send(crate::audio::player::Cmd::AppendShuffle { seed, songs });
                        }
                        if online { lib.fill_shuffle_reserve().await; }
                    });
                }
                Ok(ShuffleSelection::Queue(songs)) if !songs.is_empty() => { ctx.play(songs, 0, !self.albums && !self.online); actions.push(Action::NowPlaying); }
                Ok(ShuffleSelection::Queue(_)) => actions.push(Action::Toast(if self.online { if self.albums { "No albums available" } else { "No songs available" } } else if self.albums { "No downloaded albums" } else { "No downloaded songs" }.into())),
                Err(error) => actions.push(Action::Toast(error)),
            }
        }
        self.action = Some(Action::Many(actions));
    }
    fn completion(&mut self) -> Option<Action> { self.action.take() }
    fn tick(&mut self, ctx: &mut Ctx) -> bool {
        let journey = if self.albums { crate::ux::Journey::ShuffleAlbums } else { crate::ux::Journey::ShuffleSongs };
        if self.action.is_none() && self.deadline.as_mut().is_some_and(|deadline| deadline.expired(ctx.now, journey)) {
            self.cancel = None;
            self.action = Some(Action::Many(vec![Action::Dismiss(ctx.screen), Action::Toast(crate::ux::Code::Stuck.message().into())]));
            return true;
        }
        false
    }
    fn draw(&mut self, d: &mut crate::ui::Draw, area: crate::gfx::Rect) { d.message(area, self.message()); }
}

pub struct Favorites;

impl Source for Favorites {
    fn title(&self) -> String {
        "Favorites".into()
    }
    fn local(&self) -> bool {
        true
    }
    fn load(self: Arc<Self>, _lib: Arc<Library>, _p: Policy) -> RowsTask {
        ready(Ok(vec![
            Row::link("Songs", Dest::StarredSongs),
            Row::link("Albums", Dest::StarredAlbums),
            Row::link("Artists", Dest::StarredArtists),
        ]))
    }
}

pub enum StarredKind {
    Songs,
    Albums,
    Artists,
}

pub struct Starred(pub StarredKind);

impl Source for Starred {
    fn title(&self) -> String {
        match self.0 {
            StarredKind::Songs => "Favorite Songs",
            StarredKind::Albums => "Favorite Albums",
            StarredKind::Artists => "Favorite Artists",
        }
        .into()
    }
    fn media(&self) -> bool {
        true
    }
    fn load(self: Arc<Self>, lib: Arc<Library>, p: Policy) -> RowsTask {
        Box::pin(async move {
            let s = lib.clone().fetch_async(calls::starred(), p).await?.value;
            self.rows(lib, s).await
        })
    }
    fn empty(&self) -> &'static str {
        "Nothing loved yet\nHold Center on a song to love it"
    }
}

impl Starred {
    async fn rows(&self, lib: Arc<Library>, s: subsonic::Starred) -> Result<Vec<Row>, LibError> {
        Ok(match self.0 {
            StarredKind::Songs => {
                let rows = with_shuffle(song_rows(lib, s.song, SongStyle { art: true, track_numbers: false }).await?);
                if rows.len() == 1 { Vec::new() } else { rows }
            }
            StarredKind::Albums => album_rows(lib, s.album, true).await?,
            StarredKind::Artists => s
                .artist
                .iter()
                .map(artist_row)
                .collect(),
        })
    }
}

/// A plain list of songs: an artist's top songs, or similar songs.
pub struct SongsPage {
    pub title: String,
    pub call: subsonic::Call<subsonic::SongList>,
    pub empty: &'static str,
}

impl Source for SongsPage {
    fn title(&self) -> String {
        self.title.clone()
    }
    fn media(&self) -> bool {
        true
    }
    fn load(self: Arc<Self>, lib: Arc<Library>, p: Policy) -> RowsTask {
        Box::pin(async move {
            let songs = lib.clone().fetch_async(self.call.clone(), p).await?.value.song;
            if songs.is_empty() {
                return Ok(Vec::new());
            }
            Ok(with_shuffle(song_rows(lib, songs, SongStyle { art: true, track_numbers: false }).await?))
        })
    }
    fn empty(&self) -> &'static str {
        self.empty
    }
}

pub struct SearchResults {
    pub query: String,
}

impl SearchResults {
    fn request(&self, page: u32) -> subsonic::Call<subsonic::SearchResult> {
        let mut call = calls::search(&self.query, 10, 20, 50, page * 50);
        if page > 0 {
            // Each result class has its own offset. Keep the first-page cache
            // identity, whose absent artist/album offsets already mean zero.
            call.request = call.request.param("artistOffset", page * 10).param("albumOffset", page * 20);
        }
        call
    }

    async fn rows(lib: Arc<Library>, result: subsonic::SearchResult) -> Rows {
        // IDs are only unique within their media class. Search also needs a
        // song identity independent of its position in a particular page.
        let mut rows: Vec<Row> = result.artist.iter()
            .map(|a| artist_row(a).key(format!("artist:{}", a.id)).detail("Artist")).collect();
        rows.extend(album_rows(lib.clone(), result.album, true).await?.into_iter()
            .map(|mut row| { row.key = format!("album:{}", row.key); row }));
        rows.extend(song_rows(lib, result.song, SongStyle { art: true, track_numbers: false }).await?.into_iter()
            .map(|mut row| { row.key = format!("song:{}", row.song_item().unwrap().id); row }));
        Ok(rows)
    }
}

impl Source for SearchResults {
    fn title(&self) -> String {
        format!("“{}”", self.query)
    }
    fn media(&self) -> bool {
        true
    }
    fn load(self: Arc<Self>, lib: Arc<Library>, p: Policy) -> RowsTask {
        Box::pin(async move {
            let result = lib.clone().fetch_async(self.request(0), p).await?.value;
            Self::rows(lib, result).await
        })
    }
    fn paged(&self) -> bool { true }
    fn windowed(&self) -> bool { true }
    fn window_page(self: Arc<Self>, lib: Arc<Library>, page: u32, policy: Policy) -> WindowPageTask {
        Box::pin(async move {
            page.checked_mul(50).ok_or_else(|| LibError::Server("Search offset exceeds server range".into()))?;
            let result = lib.clone().fetch_async(self.request(page), policy).await?.value;
            let more = result.artist.len() == 10 || result.album.len() == 20 || result.song.len() == 50;
            Ok((Self::rows(lib, result).await?, more).into())
        })
    }
    /// Continue every result class, including artist/album-only matches.
    fn page(self: Arc<Self>, lib: Arc<Library>, page: u32) -> Option<PageTask> {
        Some(Box::pin(async move {
            let result = lib.clone().fetch_async(self.request(page), Policy::NetworkFirst).await?.value;
            let more = result.artist.len() == 10 || result.album.len() == 20 || result.song.len() == 50;
            Ok((Self::rows(lib, result).await?, more))
        }))
    }
    fn empty(&self) -> &'static str {
        "No matches"
    }
}

/// Downloads: what is on the card, and what is still coming.
pub struct Downloads;

const CMD_PAUSE: u32 = 20;
const CMD_RETRY: u32 = 21;
const CMD_RETRY_SPOKEN: u32 = 22;

impl Source for Downloads {
    fn title(&self) -> String {
        "Downloads".into()
    }
    fn media(&self) -> bool {
        true
    }
    fn local(&self) -> bool {
        true
    }
    fn immediate(&self) -> bool { false }
    fn load(self: Arc<Self>, lib: Arc<Library>, _p: Policy) -> RowsTask {
        let generation = lib.client_generation();
        let inventory = lib.download_inventory();
        Box::pin(async move {
            let inventory = inventory.await?;
            let check = lib.clone();
            let spoken = lib.spoken_download_status().await;
            let mut rows = on_pool(lib, move |lib| Downloads::rows(inventory, lib.is_online())).await;
            // UXI-010: each queue says how many wait and that it takes one at a time;
            // songs and audiobooks have a queue each.
            if let Some((title, fraction)) = spoken.current {
                rows.insert(0, Row::new(queue_title("Audiobooks", spoken.queued), RowAction::None).key("#spoken-status").detail(queue_now(&title, fraction)));
            } else if spoken.queued > spoken.failed {
                rows.insert(0, Row::new(queue_title("Audiobooks", spoken.queued - spoken.failed), RowAction::None).key("#spoken-status").detail("Waiting to start"));
            }
            if spoken.queued > 0 && !rows.iter().any(|row| row.key == "#pause") {
                let label = if check.downloads.is_paused() { "Resume Downloads" } else { "Pause Downloads" };
                rows.insert(0, Row::new(label, RowAction::Cmd(CMD_PAUSE)).key("#pause"));
            }
            if spoken.failed > 0 { rows.insert(0, Row::new(format!("Retry {} Failed Audiobooks", spoken.failed), RowAction::Cmd(CMD_RETRY_SPOKEN)).key("#spoken-retry")); }
            // What is happening first, then the controls, then the downloaded items.
            rows.sort_by_key(|row| match row.key.as_str() { "#status" => 0, "#spoken-status" => 1, "#pause" => 2, "#retry" => 3, "#spoken-retry" => 4, _ => 5 });
            if check.client_generation() != generation { return Err(LibError::Offline); }
            Ok(rows)
        })
    }
    fn empty(&self) -> &'static str {
        "Nothing downloaded\nHold Center on an album to download it"
    }
    fn cmd(&self, cmd: u32, _row: &Row, ctx: &mut Ctx) -> Action {
        match cmd {
            CMD_PAUSE => {
                ctx.lib.downloads.set_paused(!ctx.lib.downloads.is_paused());
                ctx.lib.notify(LibEvent::Downloads);
            }
            CMD_RETRY_SPOKEN => ctx.lib.retry_spoken_downloads(),
            CMD_RETRY => {
                let retry = ctx.lib.retry_downloads();
                ctx.task(0, move |_| retry);
            }
            _ => {}
        }
        Action::None
    }
    fn reload_on(&self, ev: &LibEvent) -> bool {
        matches!(ev, LibEvent::Downloads | LibEvent::Online(_))
    }
}

impl Downloads {
    /// Pure row construction from the awaited account-bound DB snapshot.
    fn rows(inventory: crate::library::DownloadInventory, online: bool) -> Vec<Row> {
        let st = inventory.status;
        let mut rows = Vec::new();
        if let Some((title, frac)) = &st.current {
            rows.push(Row::new(queue_title("Songs", st.queued), RowAction::None).key("#status").detail(queue_now(title, *frac)));
        } else if st.queued > 0 {
            let why = if st.paused { "Paused" } else if !online { "Waiting for the server" } else { "Starting" };
            rows.push(Row::new(queue_title("Songs", st.queued), RowAction::None).key("#status").detail(why));
        }
        if st.queued > 0 {
            let label = if st.paused { "Resume Downloads" } else { "Pause Downloads" };
            rows.push(Row::new(label, RowAction::Cmd(CMD_PAUSE)).key("#pause"));
        }
        if st.failed > 0 {
            rows.push(Row::new(format!("Retry {} Failed", st.failed), RowAction::Cmd(CMD_RETRY)).key("#retry"));
        }
        for (set, have) in inventory.sets {
            let total = set.song_ids.len();
            let kind = match set.kind.as_str() { "album" => "Album", "playlist" => "Playlist", "auto" => "Automatic", _ => "Songs" };
            let failures=set.song_ids.iter().filter(|id|inventory.failed.contains(*id)).count();
            let waiting=set.song_ids.iter().any(|id|inventory.queued.contains(id));
            let active=inventory.active.as_ref().filter(|(id,_)|set.song_ids.contains(id));
            let (badge,state)=if st.paused && (waiting || active.is_some()) {(Badge::Queued,"Paused")}
            else if let Some((id,fraction))=active {
                let completed=set.song_ids.iter().filter(|song| inventory.offline.contains(*song) && *song!=id && !inventory.queued.contains(*song)).count();
                let percent=if total==0 {None} else {fraction.map(|f|((completed as f32+f)/(total as f32)).clamp(0.0,1.0))};
                (Badge::Downloading(percent.map_or(255,|f|(f*100.0).round() as u8)), if failures>0 {"Downloading · some failed"} else {"Downloading"})
            } else if failures>0 {(Badge::Queued,"Failed · retry available")}
            else if waiting && st.paused {(Badge::Queued,"Paused")}
            else if waiting && !online {(Badge::Queued,"Waiting for server")}
            else if waiting {(Badge::Queued,"Queued")}
            else if have==total && total>0 {(Badge::Offline,"Downloaded")}
            else {(Badge::None,"Incomplete")};
            let detail=format!("{kind} · {state} · {have} of {total}");
            rows.push(Row::link(set.name.clone(), Dest::OfflineSet { kind: set.kind.clone(), id: set.id.clone(), name: set.name.clone() })
                .key(format!("{}:{}", set.kind, set.id)).detail(detail).art(set.cover_art.clone()).badge(badge));
        }
        rows
    }
}

/// A download queue's heading: what it holds and how many wait (UXI-010).
fn queue_title(kind: &str, count: usize) -> String { format!("{kind} · {count} to download") }
/// The item downloading now: one at a time per queue, its percent before
/// its title so a long title cannot hide it.
fn queue_now(title: &str, fraction: Option<f32>) -> String {
    match fraction {
        Some(f) => format!("One at a time · {}% · {title}", (f.clamp(0.0, 1.0) * 100.0) as u32),
        None => format!("One at a time · {title}"),
    }
}

pub struct OfflinePage {
    pub kind: String,
    pub id: String,
    pub name: String,
}

impl Source for OfflinePage {
    fn title(&self) -> String {
        self.name.clone()
    }
    fn media(&self) -> bool {
        true
    }
    fn local(&self) -> bool {
        true
    }
    fn immediate(&self) -> bool { false }
    fn load(self: Arc<Self>, lib: Arc<Library>, _p: Policy) -> RowsTask {
        Box::pin(async move {
            let Some((_, songs)) = lib.offline_collection_async(self.kind.clone(), self.id.clone()).await? else { return Ok(Vec::new()) };
            let mut rows = if songs.is_empty() { vec![Row::new("Waiting for downloaded songs", RowAction::None).key("#waiting")] } else { vec![shuffle_row(&songs)] };
            rows.extend(song_rows(lib, songs, SongStyle { art: true, track_numbers: false }).await?);
            rows.push(Row::new("Remove Download", RowAction::Cmd(CMD_REMOVE)).key("#remove"));
            Ok(rows)
        })
    }
    fn cmd(&self, cmd: u32, _row: &Row, ctx: &mut Ctx) -> Action {
        if cmd == CMD_REMOVE {
            let action=remove_download(ctx,&self.kind,&self.id);
            return Action::Many(vec![Action::Pop,action]);
        }
        Action::None
    }
    fn reload_on(&self, ev: &LibEvent) -> bool {
        matches!(ev, LibEvent::Downloads)
    }
}

/// Up Next: the play queue.
pub struct Queue {
    pub player: PlayerHandle,
}

const CMD_JUMP: u32 = 1000;

impl Source for Queue {
    fn title(&self) -> String {
        "Up Next".into()
    }
    fn media(&self) -> bool {
        true
    }
    fn local(&self) -> bool {
        true
    }
    fn load(self: Arc<Self>, _lib: Arc<Library>, _p: Policy) -> RowsTask {
        let st = self.player.status();
        ready(Ok(st
            .queue
            .iter()
            .enumerate()
            .map(|(i, s)| {
                Row::new(s.title.clone(), RowAction::Cmd(CMD_JUMP + i as u32))
                    .key(format!("{}#{i}", s.id))
                    .detail(s.artist.clone())
                    .art(s.cover_art.clone())
                    .song(s.clone())
            })
            .collect()))
    }
    fn empty(&self) -> &'static str {
        "Nothing queued"
    }
    fn download_badges(&self) -> bool { false }
    fn menu(&self, row: &Row, _ctx: &mut Ctx) -> Option<Box<dyn crate::ui::Screen>> {
        let RowAction::Cmd(c) = row.action else { return None };
        let i = c.checked_sub(CMD_JUMP)? as usize;
        let (p1, p2) = (self.player.clone(), self.player.clone());
        Some(Box::new(
            super::popup::Popup::new(row.title.clone())
                .add("Play Now", move |_| {
                    p1.send(Cmd::Jump(i));
                    Action::Pop
                })
                .add("Remove from Up Next", move |_| {
                    p2.send(Cmd::Remove(i));
                    Action::None
                }),
        ))
    }
    fn cmd(&self, cmd: u32, _row: &Row, _ctx: &mut Ctx) -> Action {
        if cmd >= CMD_JUMP {
            self.player.send(Cmd::Jump((cmd - CMD_JUMP) as usize));
            return Action::Pop;
        }
        Action::None
    }
}

/// Navidrome stations are cached like albums; public presets remain available
/// without a server. Playback always requires a connection.
pub struct Radios { pub featured: bool }
impl Source for Radios {
    fn title(&self) -> String { if self.featured { "Featured Stations" } else { "Radios" }.into() }
    fn local(&self) -> bool { self.featured }
    // "Server stations unavailable" and "No stations on Navidrome" are notes
    // under Featured Stations, not choices.
    fn info_rows(&self) -> bool { true }
    fn load(self: Arc<Self>, lib: Arc<Library>, p: Policy) -> RowsTask {
        Box::pin(async move {
        let mut rows=Vec::new();
        if !self.featured { rows.push(Row::link("Featured Stations", Dest::RadioStations { featured: true })); }
        let stations = if self.featured {
            vec![
                ("FIP", "https://icecast.radiofrance.fr/fip-midfi.mp3"),
                ("KEXP", "https://kexp.streamguys1.com/kexp160.aac"),
                ("SomaFM · Groove Salad", "https://somafm.com/groovesalad.pls"),
                ("SomaFM · Drone Zone", "https://somafm.com/dronezone.pls"),
                ("SomaFM · Secret Agent", "https://somafm.com/secretagent.pls"),
            ].into_iter().map(|(name,url)| subsonic::RadioStation { name:name.into(), stream_url:url.into(), ..Default::default() }).collect()
        } else {
            match lib.fetch_async(calls::internet_radio_stations(),p).await {
                Ok(r) => r.value.internet_radio_station,
                Err(_) => { rows.push(Row::new("Server stations unavailable",RowAction::None)); return Ok(rows); }
            }
        };
        for station in stations {
            let song=Song { id:format!("radio:{}",station.id), title:station.name.clone(), artist:"Internet Radio".into(), radio_url:Some(station.stream_url), ..Default::default() };
            rows.push(Row::new(station.name,RowAction::Cmd(1)).key(song.radio_url.clone().unwrap_or_default()).song(song));
        }
        if rows.len()==1 && !self.featured { rows.push(Row::new("No stations on Navidrome",RowAction::None)); }
        Ok(rows)
        })
    }
    fn cmd(&self, _:u32, row:&Row, ctx:&mut Ctx) -> Action {
        if row.song_item().is_some_and(|song| ctx.status.is_current_radio(song)) {
            return Action::NowPlaying;
        }
        if ctx.lib.offline_mode() { return Action::Toast("Radio needs a connection".into()); }
        if let Some(song)=row.song_item() { ctx.play(vec![song.clone()],0,false); return Action::NowPlaying; }
        Action::None
    }
    fn reload_on(&self,ev:&LibEvent)->bool { matches!(ev,LibEvent::Online(_)|LibEvent::RadiosUpdated) && !self.featured }
    fn menu(&self,row:&Row,_ctx:&mut Ctx)->Option<Box<dyn crate::ui::Screen>> {
        let song=row.song_item().cloned()?;
        let saved=song.clone();
        let mut popup=super::popup::Popup::new(&song.title.clone()).add("Play Station",move |ctx| {
            if ctx.status.is_current_radio(&song) { return Action::NowPlaying; }
            if ctx.lib.offline_mode() { return Action::Toast("Radio needs a connection".into()); }
            ctx.play(vec![song.clone()],0,false); Action::NowPlaying
        });
        if self.featured {
            popup=popup.add("Add to Navidrome",move |ctx| {
                let lib=ctx.lib.clone();let name=saved.title.clone();let url=saved.radio_url.clone().unwrap_or_default();
                ctx.notice_task(async move { match lib.add_radio(name,url).await {
                    Ok(())=>"Station saved to Navidrome".into(),
                    Err(LibError::Auth)=>"Server denied access; radio editing may need an admin".into(),
                    Err(e)=>format!("Could not save station: {e}"),
                }});
                Action::Toast("Saving station…".into())
            });
        }
        Some(Box::new(popup))
    }
}

#[cfg(test)]
mod download_visibility_tests {
    use super::*;
    #[test]
    fn album_rows_report_real_aggregate_and_failures_without_counting_failed_as_done() {
        let set=OfflineSet {kind:"album".into(),id:"a".into(),name:"Album".into(),artist:String::new(),cover_art:None,song_ids:vec!["one".into(),"two".into(),"three".into()]};
        let mut inventory=crate::library::DownloadInventory {status:Default::default(),sets:vec![(set,1)],queued:["two".into()].into_iter().collect(),offline:["one".into()].into_iter().collect(),failed:["three".into()].into_iter().collect(),active:Some(("two".into(),Some(0.5)))};
        let rows=Downloads::rows(inventory,true);let row=rows.iter().find(|r|r.title=="Album").unwrap();
        assert!(matches!(row.badge,Badge::Downloading(50)));assert!(row.detail.as_deref().unwrap().contains("some failed"));
        inventory=crate::library::DownloadInventory {status:Default::default(),sets:vec![],queued:Default::default(),offline:Default::default(),failed:Default::default(),active:None};
        assert!(Downloads::rows(inventory,true).is_empty());
    }
}

#[cfg(test)]
mod shuffle_albums_tests {
    use super::*;
    use crate::config::{Config, Paths};

    #[test]
    fn shuffle_albums_queues_whole_albums_in_track_order_and_skips_unavailable_ones() {
        let root = crate::library::generated_test_directory("shuffle-albums");
        let mut config = Config::default();
        config.server.url = "http://127.0.0.1:9".into(); config.server.user = "generated".into(); config.server.password = "synthetic-only".into();
        let lib = Library::open(Paths::new(root.clone()), &config, Arc::new(|_| {}));
        lib.set_offline_mode(true);
        for a in 0..4 {
            let songs: Vec<Song> = (0..3).map(|t| Song { id: format!("g{a}-{t}"), title: format!("Album {a} track {t}"), album_id: Some(format!("album-{a}")), ..Default::default() }).collect();
            for s in &songs { lib.db().put_offline_song(&crate::rt::Blocking::dedicated_thread(), s, &root.join(format!("{}.flac", s.id)), 1); }
            lib.db().put_offline_set(&crate::rt::Blocking::dedicated_thread(), &OfflineSet { kind: "album".into(), id: format!("album-{a}"), name: format!("Album {a}"), artist: "Generated".into(),
                cover_art: None, song_ids: songs.iter().map(|s| s.id.clone()).collect() });
        }
        // Cached metadata but nothing on the card: never queued offline.
        lib.db().put_offline_set(&crate::rt::Blocking::dedicated_thread(), &OfflineSet { kind: "playlist".into(), id: "not-an-album".into(), name: "x".into(), artist: String::new(), cover_art: None, song_ids: vec!["g0-0".into()] });
        let runtime = crate::rt::Ui::new().unwrap();
        let a = runtime.block_on(lib.shuffled_downloaded_albums(fastrand::Rng::with_seed(1))).unwrap();
        let b = runtime.block_on(lib.shuffled_downloaded_albums(fastrand::Rng::with_seed(2))).unwrap();
        for songs in [&a, &b] {
            assert_eq!(songs.len(), 12, "four whole albums; the missing one is skipped");
            for chunk in songs.chunks(3) {
                let album = chunk[0].album_id.clone();
                assert!(chunk.iter().all(|s| s.album_id == album), "albums stay whole");
                assert_eq!(chunk.iter().map(|s| s.id.rsplit('-').next().unwrap().to_string()).collect::<Vec<_>>(), ["0", "1", "2"], "track order kept");
            }
        }
        let order = |s: &[Song]| s.chunks(3).map(|c| c[0].album_id.clone()).collect::<Vec<_>>();
        assert_ne!(order(&a), order(&b), "album order is random");
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
mod local_storage_tests {
    use super::*;
    use crate::config::{Config, Paths};
    use std::{task::{Context, Waker}, time::Duration};

    #[test]
    fn downloaded_pages_yield_to_input_while_storage_is_busy() {
        let base = std::env::var_os("Q2_GENERATED_TEST_ROOT").map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build/perf/tests"));
        let root = base.join(format!("download-pages-{}-{}", std::process::id(), fastrand::u64(..)));
        let lib = Library::open(Paths::new(root), &Config::default(), Arc::new(|_| {}));
        let sources: Vec<Arc<dyn Source>> = vec![Arc::new(Downloads), Arc::new(OfflinePage { kind: "album".into(), id: "generated".into(), name: "Generated".into() })];
        for source in sources {
            assert!(source.local(), "refresh on every return without a network request");
            assert!(!source.immediate(), "Menu must await these storage rows");
            let held = lib.db();
            let reader = lib.clone();
            let (tx, rx) = std::sync::mpsc::channel();
            let caller = std::thread::spawn(move || {
                let mut rows = source.load(reader, Policy::CacheOnly);
                let pending = rows.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending();
                let _ = tx.send((pending, rows));
            });
            // Release the DB even on a regression so the negative path has a
            // finite lifetime instead of wedging the whole test process.
            let result = rx.recv_timeout(Duration::from_secs(5));
            drop(held);
            caller.join().unwrap();
            let (pending, rows) = result.expect("creating/polling rows must not wait for SQLite");
            assert!(pending);
            assert!(crate::rt::Ui::new().unwrap().block_on(rows).is_ok());
        }
        // Owned generated storage is retained: background library work may
        // finish after the last caller, so do not race it with recursive unlink.
    }
}


#[cfg(test)]
mod shared_snapshot_tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};

    struct HeldDrop(Option<(mpsc::Sender<()>, Mutex<mpsc::Receiver<()>>)>, Arc<std::sync::atomic::AtomicBool>);
    impl Drop for HeldDrop {
        fn drop(&mut self) {
            if let Some((entered, release)) = self.0.take() {
                let _ = entered.send(());
                let _ = release.into_inner().unwrap().recv_timeout(Duration::from_secs(3));
                self.1.store(true, std::sync::atomic::Ordering::Release);
            }
        }
    }
    struct Release(mpsc::Sender<()>);
    impl Drop for Release { fn drop(&mut self) { let _ = self.0.send(()); } }

    #[test]
    fn page_snapshot_publication_does_not_hold_reader_lock_during_payload_retirement() {
        let (entered, begun) = mpsc::channel(); let (release, held) = mpsc::channel();
        let release = Release(release);
        // No Clone implementation: snapshots share the payload itself.
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let snapshot = Arc::new(SharedSnapshot::new(HeldDrop(Some((entered, Mutex::new(held))), completed.clone())));
        let first = snapshot.get(); let second = snapshot.get();
        assert!(Arc::ptr_eq(&first, &second)); drop(first); drop(second);
        let publisher = snapshot.clone();
        let marker = completed.clone();
        let task = std::thread::spawn(move || publisher.set(HeldDrop(None, marker)));
        begun.recv_timeout(Duration::from_secs(2)).unwrap();
        // Inspect without parking the fixture on a regressed held lock.
        let available = snapshot.0.try_lock().ok().map(|current| (current.0.is_none(), completed.load(std::sync::atomic::Ordering::Acquire)));
        drop(release); task.join().unwrap();
        assert_eq!(available, Some((true, false)), "old page destruction must not hold the UI reader lock");
    }

    #[test]
    fn snapshot_download_preparation_yields_and_preserves_newer_remove_intent() {
        let mut config = crate::config::Config::default();
        config.server.url = "http://127.0.0.1:9".into();
        config.server.user = "generated-page-snapshot".into(); config.server.password = "synthetic-only".into();
        let lib = Library::open(crate::config::Paths::new(crate::library::generated_test_directory("page-snapshot-download")),
            &config, Arc::new(|_| {}));
        lib.set_offline_mode(true);
        let song = Song { id: "generated-song".into(), ..Default::default() };
        let set = offline_set("album", "generated-album", "Generated", "", None, std::slice::from_ref(&song));
        let action = lib.begin_music_download(&set.kind, &set.id).unwrap();
        let (entered, begun) = tokio::sync::oneshot::channel(); let (release, held) = mpsc::channel();
        let release = Release(release); let prepare_lib = lib.clone();
        let pending = crate::rt::io().spawn(prepare_snapshot_download(prepare_lib, action, move |_| {
            let _ = entered.send(()); held.recv_timeout(Duration::from_secs(3)).unwrap(); (set, vec![song])
        }));
        crate::rt::Ui::new().unwrap().block_on(async {
            tokio::time::timeout(Duration::from_secs(2), begun).await.unwrap().unwrap();
            // Both runtimes still progress while metadata preparation is held.
            assert_eq!(tokio::time::timeout(Duration::from_secs(2), crate::rt::io().spawn(async { 19 })).await.unwrap().unwrap(), 19);
            let newer = lib.begin_music_download("album", "generated-album").unwrap();
            drop(release);
            let result = tokio::time::timeout(Duration::from_secs(3), pending).await.unwrap().unwrap();
            assert_eq!(result.unwrap_err(), "Download action superseded");
            lib.remove_download_requested_async(newer, "album".into(), "generated-album".into()).await.unwrap();
        });
        assert!(lib.db().next_download_request(&crate::rt::Blocking::dedicated_thread()).unwrap().is_none());
    }
}


#[cfg(test)]
mod search_paging_tests {
    use super::*;
    use crate::config::{Config, Paths};

    #[test]
    fn search_continues_each_result_class_with_distinct_stable_row_identities() {
        let root = crate::library::generated_test_directory("search-continuation");
        let mut config = Config::default();
        config.server.url = "http://127.0.0.1:9".into();
        config.server.user = "generated-search".into();
        config.server.password = "synthetic-only".into();
        let lib = Library::open(Paths::new(root.clone()), &config, Arc::new(|_| {}));
        lib.set_offline_mode(true);
        let runtime = crate::rt::Ui::new().unwrap();
        for (artists, albums, songs, more) in [(10,0,0,true), (0,20,0,true), (0,0,50,true), (1,1,1,false), (0,0,0,false)] {
            let result = serde_json::json!({"searchResult3": {
                "artist": (0..artists).map(|i| serde_json::json!({"id":format!("g{i}"),"name":format!("Artist {i}")})).collect::<Vec<_>>(),
                "album": (0..albums).map(|i| serde_json::json!({"id":format!("g{i}"),"name":format!("Album {i}")})).collect::<Vec<_>>(),
                "song": (0..songs).map(|i| serde_json::json!({"id":format!("g{i}"),"title":format!("Song {i}")})).collect::<Vec<_>>()
            }});
            // Seed only the actual independently specified second-page request;
            // the former songs-only continuation cannot retrieve this response.
            lib.db().put_response(&crate::rt::Blocking::dedicated_thread(),
                "search3?query=generated&artistCount=10&albumCount=20&songCount=50&songOffset=50&artistOffset=10&albumOffset=20", &result.to_string());
            let source = Arc::new(SearchResults { query: "generated".into() });
            let (rows, got_more) = runtime.block_on(source.page(lib.clone(), 1).unwrap()).unwrap();
            assert_eq!(got_more, more);
            assert_eq!(rows.len(), artists + albums + songs);
            let keys: std::collections::HashSet<_> = rows.iter().map(|row| &row.key).collect();
            assert_eq!(keys.len(), rows.len(), "IDs shared across media classes stay distinct");
            for row in &rows {
                match &row.action {
                    RowAction::Open(Dest::Artist { id, .. }) => assert_eq!(row.key, format!("artist:{id}")),
                    RowAction::Open(Dest::Album { id, .. }) => assert_eq!(row.key, format!("album:{id}")),
                    RowAction::Play { songs, index } => assert_eq!(row.key, format!("song:{}", songs[*index].id)),
                    _ => panic!("search content must retain its normal destination"),
                }
            }
        }
        // The same song retains its key when a different page places it first.
        let result = |ids: &[&str]| subsonic::SearchResult { song: ids.iter()
            .map(|id| Song { id: (*id).into(), title: "Generated".into(), ..Default::default() }).collect(), ..Default::default() };
        let first = runtime.block_on(SearchResults::rows(lib.clone(), result(&["other", "same"]))).unwrap();
        let next = runtime.block_on(SearchResults::rows(lib.clone(), result(&["same"]))).unwrap();
        assert_eq!(first[1].key, next[0].key);
        drop(lib);
        let _ = std::fs::remove_dir_all(root);
    }
}

#[cfg(test)]
#[path = "shuffle_tests.rs"]
mod shuffle_preparation_tests;

#[cfg(test)]
#[path = "sources_random_tests.rs"]
mod random_catalog_tests;

#[cfg(test)]
mod download_queue_tests {
    use super::*;
    /// UXI-010: each queue says how many wait and that it takes one at a
    /// time; the percent comes before a title that may be cut short.
    #[test]
    fn queue_rows_say_one_at_a_time_with_percent_before_title() {
        assert_eq!(queue_title("Songs", 5), "Songs · 5 to download");
        assert_eq!(queue_now("A Very Long Generated Title", Some(0.256)), "One at a time · 25% · A Very Long Generated Title");
        assert_eq!(queue_now("Generated", None), "One at a time · Generated");
    }
}
