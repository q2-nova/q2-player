use super::*;
use crate::audio::player::Cmd;
use crate::gfx::{Color, WIDTH, Weight};
use crate::ui::list::ListView;

#[path = "../../../q2nd/src/ui/screens/controls.rs"]
pub mod controls;
#[path = "../../../q2nd/src/ui/screens/coverflow.rs"]
pub mod coverflow;
#[path = "../../../q2nd/src/ui/screens/home.rs"]
pub mod home;
#[path = "../../../q2nd/src/ui/screens/music_menu.rs"]
mod music_menu;
#[path = "../../../q2nd/src/ui/screens/now_playing.rs"]
pub mod now_playing;
#[path = "../../../q2nd/src/ui/screens/text.rs"]
pub mod text;

pub mod settings {
    use super::*;
    pub use crate::destinations::SettingsPage as Page;

    pub fn sleep_timer_choice(ctx: &Ctx) -> Box<dyn Screen> {
        crate::destinations::sleep_timer_choice(ctx)
    }
}

#[path = "../../../q2nd/src/ui/screens/spoken_speed_view.rs"]
pub(crate) mod spoken_speed_view;

pub mod spoken {
    use super::*;
    pub use crate::destinations::SpokenPage as Page;

    use super::spoken_speed_view as speed_view;

    pub struct PlaybackSpeed {
        generation: u64,
        percent: u16,
        custom: bool,
    }

    impl PlaybackSpeed {
        pub fn current() -> Self {
            Self {
                generation: 0,
                percent: 100,
                custom: false,
            }
        }
        pub fn enter(&mut self, ctx: &mut Ctx) {
            self.generation = ctx.status.generation;
            self.tick(ctx);
        }
        pub fn tick(&mut self, ctx: &mut Ctx) -> bool {
            if self.stale(ctx.status) {
                return false;
            }
            let custom = ctx.status.speed_override.is_some();
            let percent = ctx.status.speed_override.unwrap_or(ctx.demo.speed_percent);
            let changed = self.percent != percent || self.custom != custom;
            self.percent = percent;
            self.custom = custom;
            changed
        }
        fn stale(&self, status: &crate::playback::Status) -> bool {
            self.generation != status.generation
                || status.current().is_none_or(|song| song.spoken.is_none())
        }
        pub fn event(&mut self, ev: Event, ctx: &mut Ctx) -> Action {
            if matches!(ev, Event::Center | Event::Back) {
                return Action::Pop;
            }
            if self.stale(ctx.status) {
                return Action::Many(vec![
                    Action::Pop,
                    Action::Toast("Playback changed; open Playback Speed again".into()),
                ]);
            }
            let percent = match ev {
                Event::Scroll(tick) => {
                    let next = speed_view::adjust(self.percent, tick);
                    if next == self.percent {
                        return Action::Stay;
                    }
                    Some(next)
                }
                Event::CenterLong => None,
                _ => return Action::None,
            };
            self.apply(ctx, percent)
        }
        fn apply(&mut self, ctx: &mut Ctx, percent: Option<u16>) -> Action {
            ctx.player.send(Cmd::SetSpeed {
                generation: self.generation,
                percent,
            });
            self.percent = percent.unwrap_or(ctx.demo.speed_percent);
            self.custom = percent.is_some();
            Action::Stay
        }
        pub fn draw_inline(&self, d: &mut Draw, bar: Rect) {
            speed_view::draw_inline(d, bar, self.percent, self.custom, None);
        }
    }

    struct PlaybackSpeedScreen {
        speed: Rc<RefCell<PlaybackSpeed>>,
        bar: Rect,
        confirm: Rect,
    }

    impl Screen for PlaybackSpeedScreen {
        fn title(&self) -> String {
            "Playback Speed".into()
        }
        fn enter(&mut self, ctx: &mut Ctx) {
            self.speed.borrow_mut().enter(ctx);
        }
        fn tick(&mut self, ctx: &mut Ctx) -> bool {
            self.speed.borrow_mut().tick(ctx)
        }
        fn event(&mut self, event: Event, ctx: &mut Ctx) -> Action {
            self.speed.borrow_mut().event(event, ctx)
        }
        fn clickable(&self, x: i32, y: i32) -> bool {
            crate::destinations::hits(self.bar, x, y)
                || crate::destinations::hits(self.confirm, x, y)
        }
        fn click(&mut self, x: i32, y: i32, ctx: &mut Ctx) -> Action {
            if crate::destinations::hits(self.confirm, x, y) {
                return Action::Pop;
            }
            let mut speed = self.speed.borrow_mut();
            if speed.stale(ctx.status) || !crate::destinations::hits(self.bar, x, y) {
                return Action::None;
            }
            let width = self.bar.w - 2;
            let offset = (x - self.bar.x - 1).clamp(0, width);
            let steps = i32::from((speed_view::MAX - speed_view::MIN) / speed_view::STEP);
            let percent = speed_view::adjust(speed_view::MIN, (offset * steps + width / 2) / width);
            if percent == speed.percent {
                Action::Stay
            } else {
                speed.apply(ctx, Some(percent))
            }
        }
        fn draw(&mut self, d: &mut Draw, area: Rect) {
            let speed = self.speed.borrow();
            if speed.stale(d.status) {
                self.bar = Rect::default();
                self.confirm = Rect::default();
                d.message(
                    area,
                    "Playback changed\nReturn and open Playback Speed again",
                );
                return;
            }
            let podcast = d
                .status
                .current()
                .and_then(crate::destinations::podcast_series)
                .is_some();
            let caption = speed_view::caption(false, speed.custom, podcast);
            speed_view::draw(d, area, speed.percent, caption, false, None);
            self.bar = speed_view::bar(area);
            self.confirm = crate::destinations::center_target(
                d, "Wheel: adjust · Center: done", area.x + area.w / 2, area.y + 186,
            );
        }
    }

    pub fn playback_menu(ctx: &Ctx) -> popup::Popup {
        let podcast = ctx
            .status
            .current()
            .and_then(crate::destinations::podcast_series)
            .is_some();
        let mut menu = popup::Popup::new(if podcast {
            "Podcast Episode"
        } else {
            "Audiobook"
        })
        .add("Playback Speed", |ctx| {
            Action::Push(Box::new(PlaybackSpeedScreen {
                speed: ctx.spoken_speed.clone(),
                bar: Rect::default(),
                confirm: Rect::default(),
            }))
        })
        .add("Sleep Timer", |ctx| {
            Action::Push(settings::sleep_timer_choice(ctx))
        })
        .add("Brightness", |ctx| {
            Action::Push(open(Dest::SettingsPage(settings::Page::Brightness), ctx))
        })
        .add("Equalizer", |ctx| Action::Push(open(Dest::Equalizer, ctx)));
        if let Some(song) = ctx.status.current() {
            if let Some(item) = crate::destinations::State::item_index(song) {
                menu = menu.add(
                    if podcast { "Episode Info" } else { "Book Info" },
                    move |ctx| {
                        Action::Push(open(
                            Dest::Spoken(Page::Info {
                                item,
                                preview: false,
                            }),
                            ctx,
                        ))
                    },
                );
            }
            if let Some(book) = song
                .spoken
                .as_ref()
                .filter(|book| !book.chapters.is_empty())
            {
                let chapters = book.chapters.clone();
                let generation = ctx.status.generation;
                menu = menu.add("Chapters", move |_| {
                    let mut list = popup::Popup::new("Chapters");
                    for chapter in &chapters {
                        let position = chapter.start_ms;
                        list = list.add(&chapter.title, move |ctx| {
                            ctx.player.send(Cmd::Scrub {
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
        menu
    }
}

pub mod sources {
    use super::*;
    use crate::{
        Album,
        library::{LibError, Policy},
    };

    pub async fn all_albums(lib: Arc<Library>, _: Policy) -> Result<Vec<Album>, LibError> {
        let mut albums = lib.albums.clone();
        albums.sort_by(|a, b| a.artist.cmp(&b.artist).then(a.name.cmp(&b.name)));
        Ok(albums)
    }

    pub fn shuffle_from(ctx: &mut Ctx, _scope: Option<()>) -> Action {
        Action::Play {
            songs: ctx.lib.tracks.clone(),
            index: 0,
            shuffle: true,
        }
    }
}

pub fn open(dest: Dest, ctx: &Ctx) -> Box<dyn Screen> {
    match dest {
        Dest::CoverFlow => Box::new(coverflow::CoverFlow::new()),
        Dest::Search => text::search(),
        dest => crate::destinations::special(&dest, ctx)
            .unwrap_or_else(|| Box::new(Menu::new(dest, ctx))),
    }
}

const SHUFFLE_ALBUMS: u32 = 11;
const ALBUM_DOWNLOAD: u32 = 1;

fn toggle_album_download(id: &str, ctx: &mut Ctx) -> Action {
    let saved = ctx.lib.album_downloaded(id);
    if !saved && !ctx.demo.online() {
        return Action::Toast("Connect to download this album".into());
    }
    ctx.lib.set_downloaded(id, !saved);
    Action::Toast(
        if saved {
            "Download removed"
        } else {
            "Album downloaded"
        }
        .into(),
    )
}

struct Menu {
    dest: Dest,
    list: ListView,
}

impl Menu {
    fn new(dest: Dest, ctx: &Ctx) -> Self {
        let rows = rows(&dest, ctx);
        let media = rows
            .iter()
            .any(|row| row.art.is_some() || row.artist_image.is_some());
        let mut list = if let Some(tall) = crate::destinations::menu_tall(&dest) {
            ListView::settings(tall)
        } else if media {
            ListView::new(true)
        } else if rows.iter().any(|row| row.detail.is_some()) {
            ListView::tall()
        } else {
            ListView::new(false)
        };
        list.set_rows(rows);
        use crate::destinations::SettingsPage;
        if matches!(dest, Dest::SettingsPage(SettingsPage::TimeZone)) {
            if let Some(index) = list.rows.iter().position(|row| matches!(row.badge, crate::ui::list::Badge::Check)) {
                list.select(index);
            }
        }
        Self { dest, list }
    }
}

impl Screen for Menu {
    fn title(&self) -> String {
        if let Some(title) = crate::destinations::title(&self.dest) {
            return title.to_owned();
        }
        match &self.dest {
            Dest::Music => "Music".into(),
            Dest::Artists => "Artists".into(),
            Dest::Albums { title, .. } => title.clone(),
            Dest::Artist { name, .. }
            | Dest::Album { name, .. }
            | Dest::Playlist { name, .. }
            | Dest::Genre { name } => name.clone(),
            Dest::Playlists => "Playlists".into(),
            Dest::Genres => "Genres".into(),
            Dest::Favorites => "Favorites".into(),
            Dest::StarredSongs => "Loved Songs".into(),
            Dest::StarredAlbums => "Loved Albums".into(),
            Dest::StarredArtists => "Loved Artists".into(),
            Dest::Similar { title, .. } => format!("Like {title}"),
            Dest::SearchResults { query } => format!("Search: {query}"),
            Dest::Queue => "Up Next".into(),
            Dest::FixturePage(name) => (*name).into(),
            _ => String::new(),
        }
    }

    fn accessible_details(&self) -> Option<&str> {
        matches!(self.dest, Dest::SettingsPage(crate::destinations::SettingsPage::About)).then_some(
            concat!("Q2 Navidrome ", env!("Q2_NATIVE_APP_VERSION"), " · OS Version Web Demo",
                " · System Information unavailable · Report a Problem unavailable"),
        )
    }

    fn selected_label(&self) -> Option<&str> {
        self.list.selected_row().map(|row| row.title.as_str())
    }

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

    fn enter(&mut self, ctx: &mut Ctx) {
        self.list.set_rows(rows(&self.dest, ctx));
    }

    fn event(&mut self, ev: Event, ctx: &mut Ctx) -> Action {
        match ev {
            Event::Scroll(ticks) => {
                self.list.wheel(ticks, ctx.now);
                Action::None
            }
            Event::WheelEnd => {
                self.list.end_spin();
                Action::None
            }
            Event::Center => match self.list.selected_row().map(|r| r.action.clone()) {
                Some(RowAction::Open(dest)) => Action::Push(open(dest, ctx)),
                Some(RowAction::Play { songs, index }) => Action::Play {
                    songs,
                    index,
                    shuffle: false,
                },
                Some(RowAction::Shuffle(songs)) => Action::Play {
                    songs,
                    index: 0,
                    shuffle: true,
                },
                Some(RowAction::Cmd(SHUFFLE_ALBUMS))
                    if matches!(self.dest, Dest::Albums { .. }) =>
                {
                    let songs = ctx
                        .lib
                        .ordered_albums(AlbumListType::Random)
                        .into_iter()
                        .flat_map(|album| {
                            ctx.lib
                                .tracks
                                .iter()
                                .filter(move |s| s.album_id.as_ref() == Some(&album.id))
                        })
                        .cloned()
                        .collect();
                    Action::Play {
                        songs: Arc::new(songs),
                        index: 0,
                        shuffle: false,
                    }
                }
                Some(RowAction::Cmd(cmd)) => {
                    let action = match (&self.dest, cmd) {
                        (Dest::Album { id, .. }, ALBUM_DOWNLOAD) => toggle_album_download(id, ctx),
                        _ => crate::destinations::command(&self.dest, cmd, ctx),
                    };
                    self.list.set_rows(rows(&self.dest, ctx));
                    action
                }
                _ => Action::None,
            },
            Event::CenterLong => {
                let Some(row) = self.list.selected_row() else {
                    return Action::None;
                };
                if let Some(dest) = crate::destinations::context(&self.dest, row) {
                    Action::Push(open(dest, ctx))
                } else {
                    popup::for_row(row, ctx).map_or(Action::None, Action::Push)
                }
            }
            _ => Action::None,
        }
    }

    fn refresh(&mut self, _event: &LibEvent, ctx: &mut Ctx) {
        self.list.set_rows(rows(&self.dest, ctx));
    }

    fn animate(&mut self, now: Instant) -> bool {
        self.list.animate(now)
    }

    fn draw(&mut self, d: &mut Draw, area: Rect) {
        if self.list.rows.is_empty() {
            let text = match self.dest {
                Dest::SearchResults { .. } => "No matches",
                Dest::Queue => "Nothing queued",
                Dest::StarredSongs => "No loved songs",
                Dest::StarredArtists => "No loved artists",
                Dest::StarredAlbums => "No loved albums",
                _ => "Nothing here yet",
            };
            d.message(area, text);
        } else {
            self.list.draw(d, area);
        }
    }
}

fn rows(dest: &Dest, ctx: &Ctx) -> Vec<Row> {
    if let Some(rows) = crate::destinations::rows(dest, ctx) {
        return rows;
    }
    match dest {
        Dest::Music => music_menu::rows(),
        Dest::Artists | Dest::StarredArtists => {
            let mut seen = std::collections::BTreeMap::new();
            for album in &ctx.lib.albums {
                if matches!(dest, Dest::StarredArtists) && album.artist != "Mara Vale" {
                    continue;
                }
                if let Some(id) = &album.artist_id {
                    let artist = seen.entry(&album.artist).or_insert_with(|| crate::Artist {
                        id: id.clone(),
                        name: album.artist.clone(),
                        cover_art: album.cover_art.clone(),
                        ..Default::default()
                    });
                    artist.album_count += 1;
                }
            }
            seen.values().map(super::music_rows::artist_row).collect()
        }
        Dest::Artist { id, .. } => albums(
            ctx.lib
                .albums
                .iter()
                .filter(|a| a.artist_id.as_ref() == Some(id)),
        ),
        Dest::Albums { kind, .. } => {
            let mut rows = albums(ctx.lib.ordered_albums(*kind));
            if matches!(kind, AlbumListType::AlphabeticalByName) {
                rows.insert(
                    0,
                    Row::new("Shuffle Albums", RowAction::Cmd(SHUFFLE_ALBUMS)),
                );
            }
            rows
        }
        Dest::StarredAlbums => albums(
            ctx.lib
                .albums
                .iter()
                .filter(|a| matches!(a.id.as_str(), "tidal" | "copper")),
        ),
        Dest::Album { id, .. } => {
            let songs = selected_songs(ctx, |s| s.album_id.as_ref() == Some(id));
            let mut rows = with_shuffle(songs, false);
            if let Some(album) = ctx.lib.albums.iter().find(|a| &a.id == id) {
                let saved = ctx.lib.album_downloaded(id);
                let label = if saved {
                    "Remove Download"
                } else {
                    "Download Album"
                };
                rows.push(Row::new(label, RowAction::Cmd(ALBUM_DOWNLOAD)).key("#download"));
                rows.push(
                    Row::link(
                        format!("More by {}", album.artist),
                        Dest::Artist {
                            id: album.artist_id.clone().unwrap_or_default(),
                            name: album.artist.clone(),
                        },
                    )
                    .key("#artist"),
                );
            }
            rows
        }
        Dest::Playlists => ["Evening Drive", "Slow Mornings", "Analog Favorites"]
            .into_iter()
            .map(|name| {
                Row::link(
                    name,
                    Dest::Playlist {
                        id: name.into(),
                        name: name.into(),
                    },
                )
                .detail("Demo playlist")
                .art(Some(
                    match name {
                        "Evening Drive" => "afterhours",
                        "Slow Mornings" => "sundial",
                        _ => "copper",
                    }
                    .into(),
                ))
            })
            .collect(),
        Dest::Playlist { id, .. } => with_shuffle(
            selected_songs(ctx, |s| match id.as_str() {
                "Evening Drive" => s.genre.as_deref() == Some("Electronic"),
                "Slow Mornings" => s.genre.as_deref() == Some("Ambient"),
                _ => ctx.lib.loved(s),
            }),
            true,
        ),
        Dest::Genres => {
            let genres: std::collections::BTreeSet<_> = ctx
                .lib
                .albums
                .iter()
                .filter_map(|a| a.genre.as_deref())
                .collect();
            genres
                .into_iter()
                .map(|name| Row::link(name, Dest::Genre { name: name.into() }))
                .collect()
        }
        Dest::Genre { name } => albums(
            ctx.lib
                .albums
                .iter()
                .filter(|a| a.genre.as_ref() == Some(name)),
        ),
        Dest::Favorites => vec![
            Row::link("Songs", Dest::StarredSongs),
            Row::link("Albums", Dest::StarredAlbums),
            Row::link("Artists", Dest::StarredArtists),
        ],
        Dest::StarredSongs => with_shuffle(selected_songs(ctx, |s| ctx.lib.loved(s)), true),
        Dest::Similar { id, .. } => {
            let genre = ctx
                .lib
                .tracks
                .iter()
                .find(|song| &song.id == id)
                .and_then(|s| s.genre.as_deref());
            track_rows(
                selected_songs(ctx, |s| &s.id != id && s.genre.as_deref() == genre),
                true,
                false,
            )
        }
        Dest::SearchResults { query } => {
            let query = query.to_lowercase();
            let mut rows = albums(ctx.lib.albums.iter().filter(|a| {
                a.name.to_lowercase().contains(&query) || a.artist.to_lowercase().contains(&query)
            }));
            rows.extend(track_rows(
                selected_songs(ctx, |s| s.title.to_lowercase().contains(&query)),
                true,
                false,
            ));
            rows
        }
        Dest::Queue => track_rows(ctx.status.queue.clone(), true, false),
        _ => Vec::new(),
    }
}

fn selected_songs(ctx: &Ctx, predicate: impl Fn(&Song) -> bool) -> Arc<Vec<Song>> {
    Arc::new(
        ctx.lib
            .tracks
            .iter()
            .filter(|song| predicate(song))
            .cloned()
            .collect(),
    )
}

fn with_shuffle(songs: Arc<Vec<Song>>, art: bool) -> Vec<Row> {
    let mut rows = track_rows(songs.clone(), art, !art);
    if !rows.is_empty() {
        rows.insert(
            0,
            Row::new("Shuffle", RowAction::Shuffle(songs)).key("#shuffle"),
        );
    }
    rows
}

fn albums<'a>(albums: impl IntoIterator<Item = &'a crate::Album>) -> Vec<Row> {
    albums
        .into_iter()
        .map(|a| super::music_rows::album_row(a, a.artist.clone()))
        .collect()
}

pub mod popup;
