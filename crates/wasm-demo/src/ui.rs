use crate::audio::player::{PlayState, Player, Status};
use crate::clock::Instant;
use crate::config::{Accent, Config, Look};
use crate::gfx::{Canvas, Fonts, Image, Rect};
use crate::input::Event;
use crate::library::{LibEvent, Library};
use crate::{AlbumListType, Song};
use std::{any::Any, cell::RefCell, collections::HashMap, rc::Rc, sync::Arc, time::Duration};

#[path = "../../q2nd/src/ui/connectivity/glyph.rs"]
pub mod connectivity;
#[path = "../../q2nd/src/ui/icons.rs"]
pub mod icons;
#[path = "../../q2nd/src/ui/list.rs"]
pub mod list;
#[path = "../../q2nd/src/ui/music_rows.rs"]
pub mod music_rows;
#[path = "../../q2nd/src/ui/player_chrome.rs"]
pub mod player_chrome;
pub mod screens;
#[path = "../../q2nd/src/ui/state.rs"]
pub mod state;
#[path = "../../q2nd/src/ui/theme.rs"]
pub mod theme;
pub use player_chrome::{duration, progress_bar, volume_bar};

#[derive(Clone, Debug)]
pub enum Dest {
    Music,
    CoverFlow,
    Artists,
    Artist { id: String, name: String },
    Albums { kind: AlbumListType, title: String },
    Album { id: String, name: String },
    Playlists,
    Playlist { id: String, name: String },
    Genres,
    Genre { name: String },
    Favorites,
    StarredSongs,
    StarredAlbums,
    StarredArtists,
    Similar { id: String, title: String },
    Search,
    SearchResults { query: String },
    Queue,
    Settings,
    SettingsPage(screens::settings::Page),
    Equalizer,
    Spoken(screens::spoken::Page),
    RadioStations { featured: bool },
    Downloads,
    Wifi,
    Tailscale,
    Bluetooth,
    StorageUsage,
    FixturePage(&'static str),
}

pub enum Action {
    None,
    Stay,
    Push(Box<dyn Screen>),
    Replace(Box<dyn Screen>),
    Pop,
    Many(Vec<Action>),
    NowPlaying,
    TogglePlayback,
    Restart,
    Play {
        songs: Arc<Vec<Song>>,
        index: usize,
        shuffle: bool,
    },
    Toast(String),
}

pub enum Request {
    Sleep,
    PowerOff,
}

pub(crate) type Replies = Rc<RefCell<Vec<(u64, Box<dyn Any + Send>)>>>;

pub struct Reply {
    token: u64,
    pending: Replies,
}

impl Reply {
    pub fn send(&self, data: impl Any + Send) {
        self.pending.borrow_mut().push((self.token, Box::new(data)));
    }
}

pub struct Ctx<'a> {
    pub status: &'a Status,
    pub lib: &'a Arc<Library>,
    pub config: &'a Config,
    pub player: &'a Player,
    pub now: Instant,
    pub screen_on: bool,
    pub look: &'a mut Look,
    pub accent: &'a mut Accent,
    pub volume: &'a mut u8,
    pub sleep_timer: &'a mut Option<Instant>,
    pub sleep_fading: &'a mut bool,
    pub spoken_speed: Rc<RefCell<screens::spoken::PlaybackSpeed>>,
    pub requests: &'a mut Vec<Request>,
    pub demo: &'a mut crate::destinations::State,
    pub(crate) replies: Replies,
}

impl Ctx<'_> {
    // The shared screens request fixture data through the same reply boundary
    // as the player. Every demo task completes without waiting for I/O.
    pub fn task<F>(&mut self, token: u64, make: impl FnOnce(Reply) -> F)
    where
        F: Future<Output = ()>,
    {
        let reply = Reply {
            token,
            pending: Rc::clone(&self.replies),
        };
        let mut future = std::pin::pin!(make(reply));
        assert!(
            future
                .as_mut()
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                .is_ready()
        );
    }

    pub fn music_action(&mut self, _: (), _notice: Option<String>) {}
}

pub struct Draw<'a> {
    pub c: &'a mut Canvas,
    pub fonts: &'a mut Fonts,
    pub theme: theme::Theme,
    pub lib: &'a Library,
    pub status: &'a Status,
    pub volume: Option<u8>,
    pub sleep_timer: Option<(Instant, bool)>,
    pub now: Instant,
    pub placeholders: &'a mut HashMap<u32, Arc<Image>>,
}

impl Draw<'_> {
    pub fn placeholder(&mut self, size: u32) -> Arc<Image> {
        self.placeholders
            .entry(size)
            .or_insert_with(|| Arc::new(Image::placeholder(size)))
            .clone()
    }

    pub fn message(&mut self, area: Rect, text: &str) {
        self.state(area, state::View::Empty(text));
    }

    pub fn state(&mut self, area: Rect, view: state::View) {
        state::draw(self, area, view);
    }
}

pub trait Screen {
    fn clickable(&self, _x: i32, _y: i32) -> bool {
        false
    }
    fn click(&mut self, _x: i32, _y: i32, _ctx: &mut Ctx) -> Action {
        Action::None
    }
    fn title(&self) -> String;
    fn accessible_details(&self) -> Option<&str> {
        None
    }

    fn selected_label(&self) -> Option<&str> {
        None
    }
    fn script_ready(&self) -> bool {
        true
    }
    fn is_now_playing(&self) -> bool {
        false
    }
    fn is_overlay(&self) -> bool {
        false
    }
    fn wheel_is_volume(&self) -> bool {
        false
    }
    fn enter(&mut self, _ctx: &mut Ctx) {}
    fn event(&mut self, ev: Event, ctx: &mut Ctx) -> Action;
    fn loaded(&mut self, _token: u64, _data: Box<dyn Any + Send>, _ctx: &mut Ctx) {}
    fn refresh(&mut self, _ev: &LibEvent, _ctx: &mut Ctx) {}
    fn draw(&mut self, d: &mut Draw, area: Rect);
    fn animate(&mut self, _now: Instant) -> bool {
        false
    }
    fn tick(&mut self, _ctx: &mut Ctx) -> bool {
        false
    }
    fn next_frame(&self, _now: Instant) -> Option<Duration> {
        None
    }
    fn player_redraw(&self, old: &Status, next: &Status, _volume_visible: bool) -> bool {
        old.state != next.state || matches!(next.state, PlayState::Playing)
    }
}

use list::{Row, RowAction};

pub fn track_rows(songs: Arc<Vec<Song>>, art: bool, track_numbers: bool) -> Vec<list::Row> {
    (0..songs.len())
        .map(|i| music_rows::track_row(&songs, i, art, track_numbers))
        .collect()
}
