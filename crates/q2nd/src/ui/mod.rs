#![forbid(unsafe_code)]
//! The interface: a stack of screens under a status bar.

mod settings_save;
pub mod icons;
mod connectivity;
pub mod portal;
mod info_text;
mod player_chrome;
pub use player_chrome::{duration, long_duration, progress_bar, volume_bar};
pub(crate) mod doze;
mod doze_cache;
mod doze_log;
mod doze_probe;
pub mod list;
pub mod music_rows;
mod paging;
pub mod screens;
pub mod state;
pub mod theme;
pub mod startup;
mod night;
mod bench;
mod auto_sleep;
pub mod outputs;
/// UI fixtures share process-wide state (the player, the trace); tests hold
/// this to run one at a time.
#[cfg(test)]
pub(crate) static TEST_FIXTURE: std::sync::Mutex<()> = std::sync::Mutex::new(());
#[cfg(test)]
pub(crate) mod settings_tests;
#[cfg(test)]
mod optimization_tests;
#[cfg(test)]
mod doze_tests;
#[cfg(test)]
mod machine_tests;
#[cfg(test)]
mod redraw_tests;
#[cfg(test)]
pub(crate) mod hierarchy;

use crate::audio::player::{Cmd, PlayState, PlayerHandle, Status};
use crate::config::{Config, Paths};
use crate::gfx::{Canvas, Color, Fonts, HEIGHT, Image, Rect, WIDTH, Weight, corner_inset};
use crate::input::{Event, Keys, Raw};
use crate::library::{LibEvent, Library};
use crate::net::Net;
use crate::platform::{Battery, Platform};
use q2_machines::power::{self, Power};
use std::any::Any;
use std::collections::HashMap;
#[cfg(test)]
use std::sync::mpsc;

/// The UI's inbox: any thread or task sends without waiting; the UI loop
/// awaits it on the UI runtime.
pub type UiTx = tokio::sync::mpsc::UnboundedSender<UiMsg>;
pub type UiRx = tokio::sync::mpsc::UnboundedReceiver<UiMsg>;
use std::sync::Arc;
use std::time::{Duration, Instant};
use subsonic::AlbumListType;
use theme::*;

/// Where a row can lead.
#[derive(Debug, Clone, PartialEq)]
pub enum Dest {
    BookInfo { target: crate::library::book_info::BookInfoTarget, title: String },
    Spoken(screens::spoken::Page),
    Music,
    RadioStations { featured: bool },
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
    TopSongs { artist: String },
    Similar { id: String, title: String },
    Search,
    SearchResults { query: String },
    Downloads,
    StorageUsage,
    OfflineSet { kind: String, id: String, name: String },
    Queue,
    Settings,
    Equalizer,
    SettingsPage(screens::settings::Page),
    Wifi,
    Tailscale,
    Bluetooth,
}

pub enum Action {
    /// Nothing; Return then goes back.
    None,
    /// Handled: Return does not go back.
    Stay,
    Push(Box<dyn Screen>),
    Pop,
    /// Remove an owned transient screen even if another screen was opened above it.
    Dismiss(u64),
    /// Replace the top screen.
    Replace(Box<dyn Screen>),
    NowPlaying,
    TogglePlayback,
    Toast(String),
    /// Several, in order.
    Many(Vec<Action>),
}

/// Messages to the UI thread.
pub enum UiMsg {
    Notice(String),
    SettingsSaved { id: u64, result: Result<(), std::io::ErrorKind> },
    RestartPrepared { revision: u64, result: Result<(), String> },
    Input(Raw),
    TimedInput { raw: Raw, received: Instant },
    Player,
    Lib(LibEvent),
    Loaded { screen: u64, token: u64, data: Box<dyn Any + Send> },
    Net,
    Portal(u64, portal::Event),
    Tailscale,
    /// The Bluetooth thread published a new state.
    Bluetooth,
    /// A Bluetooth device's headphone profile was found or read (`outputs::Outputs::resolved`).
    OutputEq { token: u64, addr: q2_bt::host::Addr, matched: Option<crate::bluetooth::DeviceEq>, profile: Result<Option<crate::audio::eq::Profile>, String> },
    Headphones,
    /// The owned local-audio snapshot is ready; handling never waits for storage.
    DozeCacheReady(u64),
    /// Saves the screen as a PNG (headless runs).
    Screenshot(std::path::PathBuf),
    /// In-process headless-script barrier; no normal UI polling is added.
    #[cfg(test)]
    ScriptProbe(tokio::sync::oneshot::Sender<ScriptState>),
    /// One active script observer; latest state replaces intermediate copies.
    ScriptSubscribe(tokio::sync::oneshot::Sender<tokio::sync::watch::Receiver<ScriptState>>),
    /// Numeric generated-playback observation; no periodic UI work is added.
    NumericScriptProbe(tokio::sync::oneshot::Sender<crate::numeric_probe::Playback>),
    NumericScriptSubscribe(tokio::sync::oneshot::Sender<tokio::sync::watch::Receiver<crate::numeric_probe::Playback>>),
    /// Forwards a bounded diagnostic request; the UI never waits for the PCM.
    MusicCursorProbe {
        deadline: Instant,
        reply: tokio::sync::oneshot::Sender<Result<crate::numeric_probe::MusicCursor, &'static str>>,
        at_doze: Option<Arc<std::sync::OnceLock<crate::numeric_probe::DozeBoundary>>>,
    },
    /// Test-script acknowledgement of the next successful normal presentation.
    PresentBarrier(tokio::sync::oneshot::Sender<()>),
    ScriptFailed(String),
    /// Headless scripts: record every string drawn from now on (UX-006 checks).
    ScriptRecordText,
    /// Headless scripts: the current frame's strings, every string recorded
    /// so far and the playback state (`None` before `record-text`).
    ScriptText(tokio::sync::oneshot::Sender<serde_json::Value>),
    PowerPrepared { off: bool, result: Result<(), String> },
    Quit,
}

/// Strings drawn on screen, recorded for a headless script.
pub struct TextRecord {
    started: Instant,
    /// The last rendered frame's strings, in drawing order.
    current: Vec<String>,
    /// Each appearance of a string (drawn in a frame, not in the frame
    /// before) with the milliseconds since recording began. Bounded.
    seen: Vec<(u64, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptState {
    pub title: String,
    pub ready: bool,
    pub idle: bool,
    pub overlay: bool,
    pub shuffle: bool,
    pub repeat: crate::audio::player::Repeat,
}

/// Delivers a background job's results to the screen that asked.
#[derive(Clone)]
pub struct Reply {
    tx: UiTx,
    screen: u64,
    token: u64,
}

impl Reply {
    pub fn send<T: Send + 'static>(&self, data: T) {
        let _ = self.tx.send(UiMsg::Loaded { screen: self.screen, token: self.token, data: Box::new(data) });
    }
}

/// Page loads running at once, as many as the page threads were.
const PAGE_TASKS: usize = 3;

/// Runs a page load on the I/O runtime, at most [`PAGE_TASKS`] at once.
fn spawn_page(work: impl std::future::Future<Output = ()> + Send + 'static) -> tokio::task::JoinHandle<()> {
    static PAGES: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(PAGE_TASKS);
    crate::rt::io().spawn(async move {
        let _permit = PAGES.acquire().await;
        crate::ux::page_started();
        struct Finished;
        impl Drop for Finished { fn drop(&mut self) { crate::ux::page_finished(); } }
        let _finished = Finished;
        work.await;
    })
}

#[cfg(test)]
mod page_task_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A fourth page load waits for one of the first three to end.
    #[test]
    fn at_most_three_page_loads_run_at_once() {
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(tokio::sync::Notify::new());
        let (done, finished) = mpsc::channel();
        for _ in 0..6 {
            let (running, peak, gate, done) = (running.clone(), peak.clone(), gate.clone(), done.clone());
            spawn_page(async move {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                gate.notified().await;
                running.fetch_sub(1, Ordering::SeqCst);
                done.send(()).unwrap();
            });
        }
        let start = Instant::now();
        while running.load(Ordering::SeqCst) < PAGE_TASKS { assert!(start.elapsed() < Duration::from_secs(5)); std::thread::sleep(Duration::from_millis(5)); }
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(running.load(Ordering::SeqCst), PAGE_TASKS, "the fourth waits");
        for _ in 0..6 {
            gate.notify_one();
            finished.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        assert_eq!(peak.load(Ordering::SeqCst), PAGE_TASKS);
    }
}

/// A playback instance stopped by a transient failure (UX-006).
#[derive(Clone, Copy, Debug)]
struct AutoResume { generation: u64, code: u16, since: Instant, attempts: u8, next: Instant }
const AUTO_RESUME_FOR: Duration = Duration::from_secs(600);
/// A spoken item paused longer than this refreshes its remote position on resume.
const SPOKEN_REFRESH_AFTER: Duration = Duration::from_secs(2 * 60 * 60);
const AUTO_RESUME_ATTEMPTS: u8 = 6;

/// How long a Wi-Fi link with an address may be outside COMPLETED before the
/// status badge shows it as unavailable.
const WIFI_GRACE: Duration = Duration::from_secs(10);

/// A numeric flight-recorder code for an input: kind in the high byte.
fn input_code(raw: &Raw) -> u32 {
    match raw {
        Raw::Key { key, down } => 0x100 | (*key as u32) << 1 | u32::from(*down),
        Raw::KeysReset => 0x200,
        Raw::HeadphonesChanged => 0x300,
        Raw::WheelTicks(ticks) | Raw::WheelMotion { ticks, .. } => 0x400 | (*ticks as u32 & 0xff),
        Raw::WheelEnd => 0x500,
        Raw::Touch { .. } => 0x700,
        Raw::TouchUnavailable { .. } => 0x701,
        Raw::Media(media) => 0x600 | *media as u32,
    }
}

/// What screens can reach while handling input.
pub struct Ctx<'a> {
    pub sleep_timer: &'a mut Option<Instant>,
    pub sleep_timer_period: &'a mut Option<Duration>,
    pub sleep_fading: &'a mut bool,
    pub lib: &'a Arc<Library>,
    pub player: &'a PlayerHandle,
    pub status: &'a Status,
    pub config: &'a mut Config,
    /// Set when `config` changed, to save it and apply it.
    pub config_changed: &'a mut bool,
    spoken_speed: &'a std::rc::Rc<std::cell::RefCell<screens::spoken::PlaybackSpeed>>,
    pub net: &'a Net,
    pub portal: &'a mut portal::Portal,
    pub tailscale: &'a crate::tailscale::Tailscale,
    pub bluetooth: &'a crate::bluetooth::Bluetooth,
    /// Volume and headphone EQ of the output in use.
    pub outputs: &'a mut outputs::Outputs,
    pub paths: &'a Paths,
    pub now: Instant,
    pub screen_on: bool,
    pub hardware: crate::platform::Hardware,
    pub usb_drive: &'a crate::usb::UsbDrive,
    /// False only in the hierarchy audit: background work is not started.
    background: bool,
    tx: &'a UiTx,
    screen: u64,
    pub requests: &'a mut Vec<Request>,
    preference: &'a mut Option<settings_save::Preference>,
}

/// Things only the app can do, asked for by screens.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    Sleep,
    EnterPower { off: bool },
    PowerOff,
    SetBrightness(u8),
    Toast(String),
    /// Turn Wi-Fi on or off with the configured networks.
    Wifi(bool),
    /// Temporary Offline intent, captured by the requesting screen.
    OfflineMode { offline: bool, requested: bool, networks: Vec<crate::config::WifiNetwork> },
    /// Start again (a new time zone); the launcher restarts q2nd.
    Restart,
    RestartReady,
    UsbDrive,
}

impl Ctx<'_> {
    /// Runs `f` on a job thread (`rt::jobs`); what it sends through its
    /// [`Reply`] reaches this screen's [`Screen::loaded`] with `token`. For
    /// finite local work only; network/page loads use [`Ctx::task`].
    pub fn spawn(&self, token: u64, f: impl FnOnce(Reply) + Send + 'static) {
        self.spawn_blocking(token, move |_, reply| f(reply));
    }

    /// The same finite page job with its existing blocking witness exposed.
    /// Helpers doing filesystem work require this proof instead of relying on
    /// an unwitnessed caller convention. Reply and background guards are shared.
    pub fn spawn_blocking(&self, token: u64, f: impl FnOnce(&crate::rt::Blocking, Reply) + Send + 'static) {
        if !self.background { return; }
        let reply = Reply { tx: self.tx.clone(), screen: self.screen, token };
        crate::rt::jobs().run_isolated(move |blocking| f(blocking, reply));
    }

    /// Runs the future `f` makes as a task on the I/O runtime; what it sends
    /// through its [`Reply`] reaches this screen's [`Screen::loaded`] with
    /// `token`. At most three run at once, as the page threads did.
    pub fn task<F>(&self, token: u64, f: impl FnOnce(Reply) -> F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        if !self.background { return; }
        spawn_page(f(Reply { tx: self.tx.clone(), screen: self.screen, token }));
    }

    /// Like [`Ctx::task`] for work the listener just asked for and waits on
    /// (starting playback): never queued behind background page loads.
    pub fn foreground_task<F>(&self, token: u64, f: impl FnOnce(Reply) -> F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        if !self.background { return; }
        crate::rt::io().spawn(f(Reply { tx: self.tx.clone(), screen: self.screen, token }));
    }

    /// [`Ctx::foreground_task`] when the listener is waiting on an empty page,
    /// otherwise an ordinary bounded [`Ctx::task`].
    pub fn page_task<F>(&self, token: u64, waiting: bool, f: impl FnOnce(Reply) -> F)
    where
        F: std::future::Future<Output = ()> + Send + 'static,
    {
        if waiting { self.foreground_task(token, f) } else { self.task(token, f) }
    }

    /// The future's text is shown
    /// when it finishes, even if the originating pop-up has closed.
    pub fn notice_task(&self, work: impl std::future::Future<Output = String> + Send + 'static) {
        if !self.background { return; }
        let tx = self.tx.clone();
        crate::rt::io().spawn(async move {
            let _ = tx.send(UiMsg::Notice(work.await));
        });
    }

    /// Music feedback is immediate; acknowledgements follow durable local storage.
    pub fn music_action(&self, work: impl std::future::Future<Output = Result<(), String>> + Send + 'static, success: Option<String>) {
        if !self.background { return; }
        let tx = self.tx.clone();
        crate::rt::io().spawn(async move {
            let notice = match work.await { Ok(()) => success, Err(error) => Some(error) };
            if let Some(notice) = notice { let _ = tx.send(UiMsg::Notice(notice)); }
        });
    }

    /// `songs` may be a list's own shared songs: the queue then holds no copy.
    pub fn play(&mut self, songs: impl Into<std::sync::Arc<Vec<subsonic::Song>>>, index: usize, shuffle: bool) {
        let songs = songs.into();
        if songs.get(index).is_some_and(|song| song.spoken.is_some()) {
            if crate::ux::pending_audio() != Some(crate::ux::Journey::PlaySpoken) { crate::ux::begin(crate::ux::Journey::PlaySpoken); }
        }
        else if !matches!(crate::ux::pending_audio(), Some(crate::ux::Journey::ShuffleSongs | crate::ux::Journey::ShuffleAlbums)) { crate::ux::begin(crate::ux::Journey::PlayMusic); }
        self.player.send(Cmd::Play { songs, index, shuffle });
    }
}

/// What screens draw with.
pub struct Draw<'a> {
    pub c: &'a mut Canvas,
    pub fonts: &'a mut Fonts,
    pub theme: Theme,
    pub lib: &'a Library,
    pub status: &'a Status,
    /// The volume, while the wheel is changing it.
    pub volume: Option<u8>,
    /// Current session timer and fade state at this render.
    pub sleep_timer: Option<(Instant, bool)>,
    pub now: Instant,
    placeholders: &'a mut HashMap<u32, Arc<Image>>,
}

impl Draw<'_> {
    /// Art for songs and albums without any.
    pub fn placeholder(&mut self, size: u32) -> Arc<Image> {
        self.placeholders.entry(size).or_insert_with(|| Arc::new(Image::placeholder(size))).clone()
    }

    /// A centred message in `area`: the first line, then any hint lines,
    /// in the shared page-state presentation ([`state`]).
    pub fn message(&mut self, area: Rect, text: &str) {
        state::draw(self, area, state::View::Empty(text));
    }

    /// A page's loading, empty or failed state ([`state`]).
    pub fn state(&mut self, area: Rect, view: state::View) {
        state::draw(self, area, view);
    }
}

pub trait Screen {
    fn opening_spoken(&self) -> bool { false }
    /// A spoken resume that returns to the screen below rather than Now Playing.
    fn opening_spoken_in_place(&self) -> bool { false }
    /// Whether the initial control is available for a scripted interaction.
    /// Stable empty/error states count as settled; this is not network health.
    fn script_ready(&self) -> bool { true }
    /// Screens with usable cached controls can expose a pending refresh.
    fn script_idle(&self) -> bool { self.script_ready() }
    /// Test instrumentation must describe the actual control, never a copied menu.
    /// Required on every screen so new implementations cannot silently disappear.
    #[cfg(test)]
    fn audit(&self) -> hierarchy::Surface;
    #[cfg(test)]
    fn audit_settle(&mut self, _ctx: &mut Ctx) {}
    fn title(&self) -> String;
    /// Called when the screen first shows, and again when returned to.
    fn enter(&mut self, _ctx: &mut Ctx) {}
    fn event(&mut self, ev: Event, ctx: &mut Ctx) -> Action;
    /// Whether the coordinates land on an action in the last drawn screen.
    fn clickable(&self, _x: i32, _y: i32) -> bool { false }
    /// Activate the rendered target using this screen's existing actions.
    fn click(&mut self, _x: i32, _y: i32, _ctx: &mut Ctx) -> Action { Action::None }
    fn loaded(&mut self, _token: u64, _data: Box<dyn Any + Send>, _ctx: &mut Ctx) {}
    fn completion(&mut self)->Option<Action>{None}
    /// The library changed (online, downloads).
    fn refresh(&mut self, _ev: &LibEvent, _ctx: &mut Ctx) {}
    /// A network-owner publication, including scan results and saved profiles.
    fn network_changed(&mut self, _ctx: &mut Ctx) {}
    fn portal_changed(&mut self, _ctx: &mut Ctx) {}
    fn leave(&mut self, _ctx: &mut Ctx) {}
    fn interrupted(&mut self, _ctx: &mut Ctx) {}
    fn tailscale_changed(&mut self, _ctx: &mut Ctx) {}
    fn bluetooth_changed(&mut self, _ctx: &mut Ctx) {}
    fn draw(&mut self, d: &mut Draw, area: Rect);
    /// Timers, called every pass of the main loop for the top screen; true
    /// to redraw.
    fn tick(&mut self, _ctx: &mut Ctx) -> bool {
        false
    }
    /// Steps animations; true while one runs (the UI draws at 60 Hz then).
    fn animate(&mut self, _now: Instant) -> bool {
        false
    }
    /// A slower animation's next frame, if one is due (the Home slideshow).
    fn next_frame(&self, _now: Instant) -> Option<Duration> {
        None
    }
    /// A pop-up drawn over the screen below it.
    fn is_overlay(&self) -> bool {
        false
    }
    /// The wheel sets the volume here.
    fn wheel_is_volume(&self) -> bool {
        false
    }
    /// A list of artwork rows (thumbnails in the decoded-art cache): closing
    /// it leaves the cache at `art::MEMORY_AFTER_BROWSING`.
    fn holds_art(&self) -> bool { false }
    fn is_now_playing(&self) -> bool {
        false
    }
    fn player_redraw(&self, _old: &Status, _next: &Status, _volume_visible: bool) -> bool { false }
    /// Optional pitch for the current selection, sampled after navigation.
    fn click_frequency(&self) -> Option<u16> { None }
}

const TOAST_TIME: Duration = Duration::from_millis(2200);
const VOLUME_TIME: Duration = Duration::from_millis(1500);
/// Settings are written this long after the last change.
const CONFIG_SAVE_DELAY: Duration = Duration::from_secs(3);
/// The main loop's longest sleep with the screen off.
const IDLE_TICK: Duration = Duration::from_secs(30);

/// Accepted screen restoration survives cancellation of its UI observer.
struct ScreenRestore {
    battery_ready: bool,
    fresh_battery: Option<crate::platform::ControlFuture<'static, Battery>>,
    power: bool,
    error: Option<String>,
}

pub struct App {
    sleep_timer: Option<Instant>,
    sleep_timer_period: Option<Duration>,
    sleep_fading: bool,
    auto_sleep: auto_sleep::AutoSleep,
    stack: Vec<(u64, Box<dyn Screen>)>,
    next_id: u64,
    spoken_speed: std::rc::Rc<std::cell::RefCell<screens::spoken::PlaybackSpeed>>,
    pub lib: Arc<Library>,
    pub player: PlayerHandle,
    pub config: Config,
    pub paths: Paths,
    pub net: Net,
    portal: portal::Portal,
    pub tailscale: crate::tailscale::Tailscale,
    /// Replaced by main with the configured controller; off and simulated until then.
    pub bluetooth: crate::bluetooth::Bluetooth,
    /// `net.power_ups()` already passed to `bluetooth`.
    bluetooth_power_ups: u32,
    /// Set by main from the platform after startup.
    pub hardware: crate::platform::Hardware,
    /// Whether Wi-Fi had an address at the last network message.
    net_had_ip: bool,
    /// The link last seen (SSID, address): a change drops pooled connections.
    net_link: (Option<String>, Option<String>),
    /// When the link was last fully up, for the status badge's grace period.
    wifi_completed_at: Option<Instant>,
    /// Server transport (library/health.rs): when the Wi-Fi address was
    /// last held, and until when a doze's reconnect excuses its absence.
    transport_seen_at: Option<Instant>,
    transport_excused_until: Option<Instant>,
    /// When to make sure a few songs are on the card for Shuffle Songs.
    shuffle_reserve_at: Option<Instant>,
    /// Playback stopped by a network failure, resumed when the link returns.
    auto_resume: Option<AutoResume>,
    /// When this playback instance was paused, for the resume policy.
    paused_since: Option<Instant>,
    fonts: Fonts,
    canvas: Canvas,
    theme: Theme,
    night: night::Night,
    /// False only in the hierarchy audit: screens start no background work.
    pub(crate) background: bool,
    usb_drive: crate::usb::UsbDrive,
    tx: UiTx,
    keys: Keys,
    /// The sleep/power-off transaction (`q2_machines::power`,
    /// docs/state-machines.md).
    power: Power,
    wake_measurement: Option<Instant>,
    present_ack: Option<tokio::sync::oneshot::Sender<()>>,
    script_observer: Option<tokio::sync::watch::Sender<ScriptState>>,
    /// Headless scripts only: strings drawn since `record-text`.
    text_record: Option<TextRecord>,
    numeric_observer: Option<tokio::sync::watch::Sender<crate::numeric_probe::Playback>>,
    resume_measurement: Option<Instant>,
    pub screen_on: bool,
    /// When the backlight timeout last turned the screen off; None while lit.
    /// The paused radio doze counts its delay from here (ui/doze.rs).
    dark_since: Option<Instant>,
    screen_restore: Option<ScreenRestore>,
    screen_blanking: bool,
    /// Start of the current panel session. The evdev owner also rejects contacts held at open.
    touchscreen_since: Option<Instant>,
    /// A gesture must have started on the currently presented screen.
    touch_screen: Option<(u64, Instant)>,
    touch_occlusion: [Option<Rect>; 2],
    last_input: Instant,
    input_time: Option<Instant>,
    resumed_at: Option<Instant>,
    toast: Option<(String, Instant)>,
    volume_shown: Option<Instant>,
    /// Volume and headphone EQ per output: the jack, or the Bluetooth device in use.
    outputs: outputs::Outputs,
    dirty: bool,
    connectivity_trace: connectivity::SelectionTrace,
    status: Status,
    battery: Battery,
    headphones: Option<bool>,
    placeholders: HashMap<u32, Arc<Image>>,
    config_save: Option<Instant>,
    config_save_failed: bool,
    settings_save: settings_save::State,
    polled: Instant,
    battery_polled: Option<Instant>,
    battery_refresh: Option<crate::platform::ControlFuture<'static, Battery>>,
    pending: Vec<Request>,
    /// When and for which (queue, index) the local audio ahead was computed.
    doze_checked: Option<(Instant, usize, usize)>,
    doze_ahead_ms: u64,
    doze_cache: doze_cache::Cache,
    #[cfg(test)]
    settle_doze_fixture: bool,
    doze_fetching: bool,
    /// Per-cycle doze telemetry in internal state (Product feedback, 2026-11-12).
    doze_log: doze_log::DozeLog,
    doze_probe: doze_probe::DozeProbe,
    /// After a doze, ask for the upcoming songs once the link is back.
    doze_refill: bool,
    /// The (queue, index, position) last seen and when. With the screen off
    /// the player sends no position updates, so the position is estimated
    /// from this while playing.
    doze_seen: Option<((usize, usize, u64), Instant)>,
    /// The status bar's clock as last drawn.
    shown_clock: String,
    /// The centre or back press that woke the screen, not to act on.
    wake_swallow: bool,
}

fn prepare_power_state(player: &PlayerHandle, lib: &Arc<Library>) -> impl std::future::Future<Output = Result<(), String>> + Send + 'static {
    struct OwnedHandoff(Option<PlayerHandle>);
    impl Drop for OwnedHandoff {
        fn drop(&mut self) { if let Some(player) = self.0.take() { player.cancel_handoff(); } }
    }
    let player = player.clone();
    let lib = lib.clone();
    let (reply, received) = tokio::sync::oneshot::channel();
    // The admitted preparation owns acquisition, durability and failure cleanup.
    // Dropping a UI observer cannot leave a successfully acquired player frozen.
    crate::rt::io().spawn(async move {
        let acquired = player.prepare_sleep_async().await.map_err(String::from);
        match acquired {
            Ok(()) => {
                let owned = OwnedHandoff(Some(player));
                match lib.prepare_sleep_async().await {
                    Ok(()) => { let _ = reply.send(Ok(owned)); }
                    Err(error) => { drop(owned); let _ = reply.send(Err(error)); }
                }
            }
            Err(error) => { let _ = reply.send(Err(error)); }
        }
    });
    async move {
        let mut owned = received.await.unwrap_or_else(|_| Err("Power preparation stopped".into()))?;
        // Transfer the prepared player only when the observer receives success.
        // Dropping an unpolled/completed reply still drops its armed guard.
        owned.0.take();
        Ok(())
    }
}

impl App {
    pub(crate) async fn settle_telemetry(&self) -> bool {
        let library = self.lib.settle_telemetry().await;
        let doze = self.doze_log.settle().await;
        library && doze
    }

    pub fn new(
        lib: Arc<Library>,
        player: PlayerHandle,
        config: Config,
        paths: Paths,
        net: Net,
        tailscale: crate::tailscale::Tailscale,
        fonts: Fonts,
        tx: UiTx,
    ) -> App {
        let theme = Theme::new(config.display.look, config.display.accent);
        let doze_log_path = lib.paths.database().with_file_name("doze-cycles.log");
        let bluetooth = crate::bluetooth::Bluetooth::new(crate::bluetooth::Options::absent(&paths.root), Arc::new(|| {}));
        let mut app = App {
            bluetooth,
            bluetooth_power_ups: 0,
            stack: Vec::new(),
            next_id: 1,
            sleep_timer: None,
            sleep_timer_period: None,
            sleep_fading: false,
            auto_sleep: auto_sleep::AutoSleep::default(),
            spoken_speed: std::rc::Rc::new(std::cell::RefCell::new(screens::spoken::PlaybackSpeed::current())),
            lib,
            player,
            config,
            paths,
            portal: portal::Portal::new(tx.clone(),net.simulated()),
            net,
            hardware: crate::platform::Hardware::default(),
            net_had_ip: false,
            net_link: (None, None),
            wifi_completed_at: None,
            transport_seen_at: None,
            transport_excused_until: None,
            shuffle_reserve_at: Some(Instant::now() + Duration::from_secs(60)),
            auto_resume: None,
            paused_since: None,
            tailscale,
            fonts,
            canvas: Canvas::new(WIDTH, HEIGHT),
            theme,
            night: night::Night::new(),
            background: true,
            usb_drive: crate::usb::UsbDrive::default(),
            tx,
            keys: Keys::default(),
            power: Power::Idle,
            wake_measurement: None,
            present_ack: None,
            script_observer: None,
            text_record: None,
            numeric_observer: None,
            resume_measurement: None,
            screen_on: true,
            dark_since: None,
            screen_restore: None,
            screen_blanking: false,
            touchscreen_since: None,
            touch_screen: None,
            touch_occlusion: [None; 2],
            last_input: Instant::now(),
            input_time: None,
            resumed_at: None,
            toast: None,
            volume_shown: None,
            outputs: outputs::Outputs::default(),
            dirty: true,
            connectivity_trace: connectivity::SelectionTrace::default(),
            status: Status::default(),
            battery: Battery::default(),
            headphones: None,
            placeholders: HashMap::new(),
            config_save: None,
            config_save_failed: false,
            settings_save: Default::default(),
            polled: Instant::now() - Duration::from_secs(10),
            battery_polled: None,
            battery_refresh: None,
            pending: Vec::new(),
            doze_checked: None,
            doze_ahead_ms: 0,
            doze_cache: doze_cache::Cache::default(),
            #[cfg(test)]
            settle_doze_fixture: false,
            doze_fetching: false,
            doze_log: doze_log::DozeLog::new(Some(doze_log_path)),
            doze_probe: doze_probe::DozeProbe::default(),
            doze_refill: false,
            doze_seen: None,
            shown_clock: String::new(),
            wake_swallow: false,
        };
        app.status = app.player.status();
        app.lib.set_offline_group_queue(&app.status.queue);
        let home = screens::home::Home::new();
        app.push(Box::new(home));
        if !app.lib.is_configured() {
            app.push(screens::settings::server_page());
        }
        app
    }

    /// Runs `f` on the screen `id` with a context.
    fn with_screen<R>(&mut self, id: u64, f: impl FnOnce(&mut dyn Screen, &mut Ctx) -> R) -> Option<R> {
        let idx = self.stack.iter().position(|(sid, _)| *sid == id)?;
        let previous_speed = self.config.spoken.speed_percent;
        let mut changed = false;
        let mut requests = Vec::new();
        let mut preference = None;
        let r = {
            let (_, screen) = &mut self.stack[idx];
            let mut ctx = Ctx {
                sleep_timer: &mut self.sleep_timer,
                sleep_timer_period: &mut self.sleep_timer_period,
                sleep_fading: &mut self.sleep_fading,
                lib: &self.lib,
                player: &self.player,
                status: &self.status,
                config: &mut self.config,
                config_changed: &mut changed,
                spoken_speed: &self.spoken_speed,
                net: &self.net,
                portal: &mut self.portal,
                tailscale: &self.tailscale,
                bluetooth: &self.bluetooth,
                outputs: &mut self.outputs,
                paths: &self.paths,
                now: self.input_time.unwrap_or_else(Instant::now),
                screen_on: self.screen_on,
                hardware: self.hardware,
                usb_drive: &self.usb_drive,
                background: self.background,
                tx: &self.tx,
                screen: id,
                requests: &mut requests,
                preference: &mut preference,
            };
            f(screen.as_mut(), &mut ctx)
        };
        if changed {
            self.config_changed();
            if self.config.spoken.speed_percent != previous_speed {
                // TrackSource resolves the new default from the library after
                // configuration publication, preserving explicit overrides.
                self.player.send(Cmd::RefreshSpeed);
            }
        }
        if let Some(preference) = preference { self.save_library_preference(preference); }
        self.pending.extend(requests);
        Some(r)
    }

    fn top_id(&self) -> u64 {
        self.stack.last().map(|s| s.0).unwrap_or(0)
    }

    pub fn push(&mut self, screen: Box<dyn Screen>) {
        let id = self.next_id;
        self.next_id += 1;
        self.stack.push((id, screen));
        self.with_screen(id, |s, ctx| s.enter(ctx));
        self.dirty = true;
    }

    fn pop(&mut self) {
        if self.stack.len() > 1 {
            self.with_screen(self.top_id(), |screen,ctx| screen.leave(ctx));
            let closed = self.stack.pop();
            // Backed out of a list: its thumbnails are unlikely to be drawn
            // again soon. The rows were dropped with the screen; one heap
            // trim (at most one queued) returns both.
            if closed.is_some_and(|(_, screen)| screen.holds_art()) {
                self.lib.art.shrink_memory(crate::library::art::MEMORY_AFTER_BROWSING);
                crate::platform::resident::trim_after_sync();
            }
            let id = self.top_id();
            self.with_screen(id, |s, ctx| s.enter(ctx));
        }
        self.dirty = true;
    }

    fn apply(&mut self, action: Action) {
        match action {
            Action::None | Action::Stay => {}
            Action::Push(s) => self.push(s),
            Action::Pop => self.pop(),
            Action::Dismiss(id) => {
                if let Some(index) = self.stack.iter().position(|(screen, _)| *screen == id).filter(|index| *index > 0) {
                    let was_top = index + 1 == self.stack.len();
                    self.with_screen(id, |screen,ctx| screen.leave(ctx));
                    self.stack.remove(index);
                    if was_top {
                        let parent = self.top_id();
                        self.with_screen(parent, |screen, ctx| screen.enter(ctx));
                    }
                }
            }
            Action::Replace(s) => {
                if self.stack.len() > 1 {
                    self.with_screen(self.top_id(), |screen,ctx| screen.leave(ctx));
                    self.stack.pop();
                }
                self.push(s);
            }
            Action::TogglePlayback => self.toggle_playback(),
            Action::NowPlaying => {
                if !self.stack.last().is_some_and(|s| s.1.is_now_playing()) {
                    self.with_screen(self.top_id(), |screen,ctx| screen.interrupted(ctx));
                    self.push(Box::new(screens::now_playing::NowPlaying::new()));
                }
            }
            Action::Toast(t) => self.toast = Some((crate::ux::present(&t), Instant::now())),
            Action::Many(list) => {
                for a in list {
                    self.apply(a);
                }
            }
        }
        self.dirty = true;
    }

    fn config_changed(&mut self) {
        self.theme = Theme::new(self.config.display.look, self.config.display.accent);
        let configured = self.lib.configure_async(&self.config);
        let tx = self.tx.clone();
        crate::rt::io().spawn(async move {
            if let Err(error) = configured.await { let _ = tx.send(UiMsg::Notice(error)); }
        });
        self.settings_changed();
        self.dirty = true;
    }

    pub fn open_usb_drive(&mut self) {
        self.request_settings_handoff(settings_save::Handoff::Usb);
    }
    fn open_usb_drive_saved(&mut self) {
        let tx = self.tx.clone();
        let prepare = self.player.clone();
        let cancel = self.player.clone();
        let lib = self.lib.clone();
        let prepared = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let completed = prepared.clone();
        match self.usb_drive.start_async(move || async move {
            prepare.prepare_handoff_async().await?;
            prepared.store(true, std::sync::atomic::Ordering::Release);
            lib.save_now_async().await.map_err(|_| "Queue storage failed; USB drive cancelled")
        }, move |m| {
            if completed.load(std::sync::atomic::Ordering::Acquire) { cancel.cancel_handoff(); }
            let _ = tx.send(UiMsg::Notice(m.into()));
        }) {
            Ok(()) => self.toast("Opening USB drive…"),
            Err(m) => self.toast(m),
        }
    }

    /// Keep UI health/input acquisition alive while the audio owner and local
    /// database acknowledge a durable retained-stream transition.
    pub fn prepare_power(&mut self, off: bool) {
        if self.power.pending() { return; }
        self.request_settings_handoff(settings_save::Handoff::Power(off));
    }
    fn prepare_power_saved(&mut self, off: bool) {
        if self.power.pending() { return; }
        let (next, effect) = power::transition(self.power, power::Event::Request);
        if effect != power::Effect::Prepare { return; }
        // Input will close during suspend. A thumb lifted while it is closed
        // cannot deliver WheelEnd to the list on resume.
        self.with_screen(self.top_id(), |screen, ctx| screen.event(Event::WheelEnd, ctx));
        self.power = next;
        self.wake_measurement = None;
        self.toast(if off { "Preparing power off…" } else { "Preparing sleep…" });
        let player = self.player.clone();
        let lib = self.lib.clone();
        let tx = self.tx.clone();
        let preparation = prepare_power_state(&player, &lib);
        crate::rt::io().spawn(async move {
            let result = preparation.await;
            let success = result.is_ok();
            if tx.send(UiMsg::PowerPrepared { off, result }).is_err() && success { player.cancel_handoff(); }
        });
    }

    pub fn await_power_off(&mut self) { self.power = power::transition(self.power, power::Event::AwaitOff).0; }

    pub async fn finish_power_async(&mut self, platform: &mut dyn Platform, result: Result<(), String>, returned: Option<Instant>) {
        if !self.screen_restore.as_ref().is_some_and(|restore| restore.power) {
            // Install the original outcome once, before any cancellable await.
            // Re-entry must neither release a newer handoff nor lose its error
            // or replace the actual helper-return measurement boundary.
            self.resume_measurement = if result.is_ok() { returned } else { None };
            let (next, effect) = power::transition(self.power, power::Event::Finished);
            if effect == (power::Effect::Restore { cancel_barrier: true }) { self.player.cancel_handoff(); }
            self.power = next;
            self.keys.cancel();
            self.wake_swallow = false;
            self.last_input = Instant::now();
            self.resumed_at = Some(self.last_input);
            self.headphones = None; // Re-establish jack state after input re-registration.
            self.polled = Instant::now() - Duration::from_secs(10);
            self.screen_restore = Some(ScreenRestore { battery_ready: false, fresh_battery: None, power: true, error: result.err() });
        }
        self.restore_screen_async(platform, Instant::now()).await;
    }

    fn observe_auto_resume(&mut self, old: &Status) {
        let now = Instant::now();
        let active = |s: PlayState| matches!(s, PlayState::Playing | PlayState::Buffering);
        if let Some(pending) = self.auto_resume {
            if self.status.generation != pending.generation { self.auto_resume = None; }
            else if self.status.state == PlayState::Playing {
                if pending.attempts > 0 { crate::ux::recovered(crate::ux::Code::StreamInterrupted, crate::ux::Journey::Resume); }
                self.auto_resume = None;
            }
        }
        if active(old.state) && self.status.state == PlayState::Paused
            && self.status.error_code.is_some_and(crate::ux::Code::transient) {
            self.auto_resume = Some(AutoResume { generation: self.status.generation, code: self.status.error_code.unwrap_or(0),
                since: now, attempts: 0, next: now + Duration::from_secs(2) });
        }
    }

    /// The Wi-Fi chip powered up again (a doze wake, Wi-Fi or Offline toggled): the
    /// Bluetooth side shares it and starts over (docs/bugs/wifi-lps-after-power-cycle.md).
    fn tick_bluetooth_power(&mut self) {
        let power_ups = self.net.power_ups();
        if power_ups == self.bluetooth_power_ups { return }
        self.bluetooth_power_ups = power_ups;
        self.bluetooth.wifi_powered_up();
    }

    /// Resumes a network-stopped playback once the link is usable again.
    fn tick_auto_resume(&mut self, now: Instant) {
        let Some(mut pending) = self.auto_resume else { return };
        if now.duration_since(pending.since) > AUTO_RESUME_FOR || pending.attempts >= AUTO_RESUME_ATTEMPTS
            || self.status.generation != pending.generation || self.status.state != PlayState::Paused {
            self.auto_resume = None;
            return;
        }
        let wifi = self.net.state();
        // Wait for an address while Wi-Fi is meant to be up or dozing; with
        // Wi-Fi not requested (a host run) there is no address to wait for.
        if now < pending.next || self.lib.offline_mode() || (wifi.ip.is_none() && (wifi.enabled || self.net.dozing())) { return; }
        pending.attempts += 1;
        pending.next = now + Duration::from_secs(5u64 << pending.attempts.min(4));
        self.auto_resume = Some(pending);
        log::info!("auto-resume attempt {} after code {}", pending.attempts, pending.code);
        self.player.send(Cmd::Resume { generation: pending.generation });
    }

    /// Requests screens made that the main loop carries out.
    pub fn take_requests(&mut self) -> Vec<Request> {
        std::mem::take(&mut self.pending)
    }

    fn update_player(&mut self, next: Status) {
        let old = std::mem::replace(&mut self.status, next);
        self.lib.set_offline_group_queue(&self.status.queue);
        crate::ux::set_play(match self.status.state { PlayState::Stopped => 0, PlayState::Playing => 1, PlayState::Paused => 2, PlayState::Buffering => 3 });
        self.observe_auto_resume(&old);
        self.bluetooth.set_playing(matches!(self.status.state, PlayState::Playing | PlayState::Buffering));
        match self.status.state {
            PlayState::Playing | PlayState::Buffering => self.paused_since = None,
            PlayState::Paused if self.paused_since.is_none() && old.is_active() && old.generation == self.status.generation => self.paused_since = Some(Instant::now()),
            PlayState::Paused if old.generation == self.status.generation => {}
            _ => self.paused_since = None,
        }
        match (old.state == PlayState::Buffering, self.status.state == PlayState::Buffering) {
            (false, true) => crate::ux::begin(crate::ux::Journey::Buffering),
            (true, false) => crate::ux::end(crate::ux::Journey::Buffering, if self.status.state == PlayState::Playing { crate::ux::Outcome::Shown } else { crate::ux::Outcome::Cancelled }),
            _ => {}
        }
        let progress = self.stack.last().is_some_and(|s| {
            s.1.player_redraw(&old, &self.status, self.volume_shown.is_some())
        });
        if !old.same_non_position(&self.status) || progress { self.dirty = true; }
    }

    #[cfg(test)]
    pub fn handle(&mut self, msg: UiMsg, platform: &mut dyn Platform) {
        crate::rt::Ui::new().unwrap().block_on(self.handle_async(msg, platform));
    }

    pub async fn handle_async(&mut self, msg: UiMsg, platform: &mut dyn Platform) {
        match msg {
            UiMsg::Input(raw) => self.input_async(raw, Instant::now(), platform).await,
            UiMsg::TimedInput { raw, received } => self.input_async(raw, received, platform).await,
            UiMsg::Player => {
                self.update_player(self.player.status());
            }
            UiMsg::Lib(ev) => {
                if matches!(ev, LibEvent::Downloads) { self.doze_cache.invalidate(); self.doze_checked = None; }
                if let LibEvent::Notice(text) = &ev { self.toast = Some((text.clone(), Instant::now())); }
                let ids: Vec<u64> = self.stack.iter().map(|s| s.0).collect();
                for id in ids {
                    self.with_screen(id, |s, ctx| s.refresh(&ev, ctx));
                }
                self.dirty = true;
            }
            UiMsg::Loaded { screen, token, data } => {
                let action=self.with_screen(screen, |s, ctx| {s.loaded(token, data, ctx);s.completion()}).flatten();
                if let Some(action)=action {self.apply(action);}
                self.dirty = true;
            }
            UiMsg::SettingsSaved { id, result } => self.settings_saved(id, result),
            UiMsg::RestartPrepared { revision, result } => self.restart_prepared(revision, result),
            UiMsg::Notice(message) => { self.toast = Some((crate::ux::present(&message),Instant::now())); self.dirty = true; }
            UiMsg::Headphones => self.refresh_headphones_async(platform).await,
            UiMsg::Tailscale => {
                let ids: Vec<u64> = self.stack.iter().map(|s| s.0).collect();
                for id in ids { self.with_screen(id, |screen, ctx| screen.tailscale_changed(ctx)); }
                self.dirty = true;
            }
            UiMsg::Bluetooth => {
                self.sync_outputs();
                let ids: Vec<u64> = self.stack.iter().map(|s| s.0).collect();
                let top = self.top_id();
                for id in ids {
                    // The top screen may close itself on a new state (pairing done, a device forgotten).
                    let action = self.with_screen(id, |screen, ctx| { screen.bluetooth_changed(ctx); if id == top { screen.completion() } else { None } }).flatten();
                    if let Some(action) = action { self.apply(action); }
                }
                self.dirty = true;
            }
            UiMsg::Portal(generation, event) => {
                if matches!(event, portal::Event::State(portal::State::Credentials)) {
                    // The network owner may already have published a new lease.
                    self.portal.observe(&self.net.state());
                }
                if self.portal.update(generation,event) {
                    match self.portal.state {
                        portal::State::Online => self.lib.network_up(),
                        portal::State::Delta | portal::State::Guest => {
                            self.toast=Some(("Wi-Fi sign-in needed".into(),Instant::now()));
                        }
                        _ => {}
                    }
                    let ids: Vec<_> = self.stack.iter().map(|s|s.0).collect();
                    for id in ids { self.with_screen(id, |s,ctx| s.portal_changed(ctx)); }
                    self.dirty=true;
                }
            }
            UiMsg::Net => {
                if let Some((offline, message)) = self.net.take_fault_async().await {
                    self.lib.set_offline_mode(offline);
                    self.toast = Some((message.into(), Instant::now()));
                }
                let state = self.net.state();
                self.portal.observe(&state);
                let has_ip = state.ip.is_some();
                crate::diagnostics::online(has_ip);
                let link = (state.ssid.clone(), state.ip.clone());
                if link != self.net_link {
                    subsonic::http::network_changed();
                    self.net_link = link;
                }
                if has_ip && !self.net_had_ip {
                    self.lib.network_up();
                    // A Bluetooth profile that needed a download can have it now.
                    if self.outputs.bluetooth.as_ref().is_some_and(|b| matches!(b.eq_state, outputs::EqState::Unavailable(_))) { self.resolve_output_eq(); }
                }
                self.net_had_ip = has_ip;
                let ids: Vec<u64> = self.stack.iter().map(|s| s.0).collect();
                for id in ids { self.with_screen(id, |screen, ctx| screen.network_changed(ctx)); }
                self.dirty = true;
            }
            UiMsg::OutputEq { token, addr, matched, profile } => {
                let name = matched.as_ref().map(|eq| eq.name.clone());
                if let Some(matched) = self.outputs.resolved(token, addr, matched, profile) {
                    if let Some(eq) = matched { self.bluetooth.set_device_eq(addr, Some(eq)); }
                    if let Some(name) = name { self.toast = Some((format!("Headphone EQ: {name}"), Instant::now())); }
                    self.player.send(Cmd::SetEq(self.outputs.eq(&self.config.playback.eq)));
                    let ids: Vec<u64> = self.stack.iter().map(|s| s.0).collect();
                    for id in ids { self.with_screen(id, |screen, ctx| screen.bluetooth_changed(ctx)); }
                    self.dirty = true;
                }
            }
            UiMsg::DozeCacheReady(token) => self.doze_cache_ready(token, Instant::now()).await,
            UiMsg::PresentBarrier(reply) => {self.present_ack=Some(reply);self.dirty=true;}
            UiMsg::PowerPrepared { off, result } => {
                self.power = power::transition(self.power, power::Event::Prepared { off, ok: result.is_ok() }).0;
                if let Err(error) = result { self.finish_power_async(platform, Err(error), None).await; }
            }
            #[cfg(test)]
            UiMsg::ScriptProbe(reply) => {
                let _ = reply.send(self.script_state());
            }
            UiMsg::ScriptSubscribe(reply) => {
                let (sender, receiver) = tokio::sync::watch::channel(self.script_state());
                if reply.send(receiver).is_ok() { self.script_observer = Some(sender); }
            }
            UiMsg::NumericScriptSubscribe(reply) => {
                let (sender, receiver) = tokio::sync::watch::channel(crate::numeric_probe::Playback::from_status(&self.player.status()));
                if reply.send(receiver).is_ok() { self.numeric_observer = Some(sender); }
            }
            UiMsg::MusicCursorProbe { deadline, reply, at_doze } => {
                if let Some(context) = at_doze {
                    self.doze_probe.arm(Instant::now(), deadline, context, reply);
                } else {
                    self.player.send(Cmd::ObserveMusicCursor { deadline, reply });
                }
            }
            UiMsg::NumericScriptProbe(reply) => {
                let _=reply.send(crate::numeric_probe::Playback::from_status(&self.player.status()));
            }
            UiMsg::ScriptRecordText => {
                self.text_record = Some(TextRecord { started: Instant::now(), current: Vec::new(), seen: Vec::new() });
                self.dirty = true;
            }
            UiMsg::ScriptText(reply) => {
                // A fresh frame, as for a screenshot.
                let _ = self.render();
                if let Some(value) = self.script_text() { let _ = reply.send(value); }
            }
            UiMsg::Screenshot(_) | UiMsg::ScriptFailed(_) | UiMsg::Quit => {}
        }
        self.apply_offline_requests().await;
    }

    fn script_state(&self) -> ScriptState {
        let (_, screen) = self.stack.last().unwrap();
        ScriptState { title: screen.title(), ready: screen.script_ready(), idle: screen.script_idle(),
            overlay: screen.is_overlay(), shuffle: self.status.shuffle, repeat: self.status.repeat }
    }

    /// Publish after the existing UI event/timer/presentation cycle. This adds
    /// no timer and computes no snapshots when no script is observing.
    pub(crate) fn publish_script_observers(&mut self) {
        if self.script_observer.as_ref().is_some_and(|sender| sender.is_closed()) { self.script_observer = None; }
        if let Some(sender) = &self.script_observer {
            let next = self.script_state();
            sender.send_if_modified(|old| { if *old == next { false } else { *old = next; true } });
        }
        if self.numeric_observer.as_ref().is_some_and(|sender| sender.is_closed()) { self.numeric_observer = None; }
        if let Some(sender) = &self.numeric_observer {
            let next = crate::numeric_probe::Playback::from_status(&self.player.status());
            sender.send_if_modified(|old| { if *old == next { false } else { *old = next; true } });
        }
    }

    // Screens remain synchronous; publish their captured radio intent before
    // another UI message or automatic doze can observe it. Other requests keep
    // their existing dispatch phase and order.
    async fn apply_offline_requests(&mut self) {
        for request in std::mem::take(&mut self.pending) {
            match request {
                Request::OfflineMode { offline, requested, networks } => {
                    self.net.set_offline_mode_async(offline, requested, &networks).await;
                }
                other => self.pending.push(other),
            }
        }
    }

    async fn refresh_headphones_async(&mut self, platform: &mut dyn Platform) {
        if self.power.pending() { self.headphones = None; return; }
        if self.status.is_active() && self.config.playback.pause_on_unplug {
            let h = platform.headphones_async().await;
            if self.headphones == Some(true) && h == Some(false) { crate::ux::expect_stop(crate::ux::StopCause::Unplug); self.player.send(Cmd::Pause); }
            self.headphones = h;
        } else { self.headphones = None; }
    }

    async fn input_async(&mut self, raw: Raw, now: Instant, platform: &mut dyn Platform) {
        // Also protect alternate/test input producers from treating jack state
        // as a gesture, including screen restoration and idle-time changes.
        if matches!(raw, Raw::HeadphonesChanged) { self.refresh_headphones_async(platform).await; return; }
        if let Raw::TouchUnavailable { at } = raw {
            if self.touchscreen_since.is_some_and(|since| at >= since) {
                self.touchscreen_failure();
                self.sync_touchscreen(platform).await;
            }
            return;
        }
        if let Raw::Touch { x, y, start_x, start_y, at } = raw {
            if !self.config.display.touchscreen || !self.screen_on || self.screen_transitioning()
                || self.power.pending() || self.usb_drive.busy()
                || self.touchscreen_since.is_none_or(|since| at < since)
                || self.touch_screen.is_none_or(|(id, since)| self.top_id() != id || at < since)
            { return; }
            self.last_input = Instant::now();
            crate::ux::input(input_code(&raw));
            let hittable = |x: u16, y: u16| {
                let (x, y) = (i32::from(x), i32::from(y));
                let inset = corner_inset(y);
                (STATUS_H..HEIGHT).contains(&y) && (inset..WIDTH-inset).contains(&x)
                    && !self.touch_occlusion.iter().flatten().any(|r|
                        (r.x..r.right()).contains(&x) && (r.y..r.bottom()).contains(&y))
                    && self.stack.last().is_some_and(|(_, s)| s.clickable(x, y))
            };
            if hittable(start_x, start_y) && hittable(x, y) {
                if let Some(action) = self.with_screen(self.top_id(), |s, ctx| s.click(x.into(), y.into(), ctx)) {
                    self.apply(action);
                }
            }
            return;
        }
        self.poll_screen_restore(platform, now).await;
        if self.resumed_at.is_some_and(|cutoff| now <= cutoff) { return; }
        self.last_input = Instant::now();
        crate::ux::input(input_code(&raw));
        if self.power.pending() {
            // Observe release without executing stale gestures. Long Play and
            // the Center wake pin must be released before entering suspend.
            match raw {
                Raw::Key { key, down } => self.keys.key(key, down, now, &mut Vec::new()),
                Raw::KeysReset => self.keys.cancel(),
                _ => {}
            }
            return;
        }
        let mut events = Vec::new();
        match raw {
            Raw::Key { key, down } => {
                if !self.screen_on && down && matches!(key, crate::input::Key::Center | crate::input::Key::Back) {
                    // Centre or Return only wakes the screen.
                    if self.screen_restore.is_none() {
                        crate::metrics::record(crate::metrics::Event::ScreenWakeRequested, Duration::ZERO);
                        self.wake_measurement = Some(now);
                    }
                    self.wake_async(platform).await;
                    self.wake_swallow = true;
                    return;
                }
                if self.wake_swallow && !down {
                    self.wake_swallow = false;
                    return;
                }
                self.keys.key(key, down, now, &mut events);
            }
            Raw::KeysReset => { self.keys.cancel(); self.wake_swallow = false; },
            Raw::Touch { .. } | Raw::TouchUnavailable { .. } => unreachable!("panel events handled above"),
            Raw::HeadphonesChanged => unreachable!("jack events are handled before gestures"),
            Raw::WheelTicks(t) => events.push(Event::Scroll(t)),
            Raw::WheelMotion { ticks, .. } => events.push(Event::Scroll(ticks)),
            Raw::WheelEnd => {
                let id = self.top_id();
                // A release must erase a visible letter without waking the screen.
                if matches!(self.with_screen(id, |s, ctx| s.event(Event::WheelEnd, ctx)), Some(Action::Stay)) {
                    self.dirty = true;
                }
                return;
            }
            Raw::Media(m) => events.push(match m {
                crate::input::Media::PlayPause => Event::PlayPause,
                crate::input::Media::Next => Event::Next,
                crate::input::Media::Prev => Event::Prev,
                // The headset's volume keys step the volume; the screen stays as it is.
                crate::input::Media::VolumeUp | crate::input::Media::VolumeDown => {
                    self.change_volume(if m == crate::input::Media::VolumeUp { outputs::KEY_STEP } else { -outputs::KEY_STEP });
                    return;
                }
            }),
        }
        self.input_time = Some(match raw { Raw::WheelMotion { at, .. } => at, _ => now });
        for ev in events {
            self.event_async(ev, platform).await;
        }
        self.input_time = None;
    }

    /// Reconcile the driver lifetime with the saved setting and display state.
    pub async fn sync_touchscreen(&mut self, platform: &mut dyn Platform) {
        let enable = self.hardware.touchscreen && self.config.display.touchscreen && self.screen_on
            && !self.screen_transitioning() && !self.power.pending();
        if enable == self.touchscreen_since.is_some() { return; }
        let started = Instant::now();
        match platform.set_touchscreen(enable).await {
            Ok(()) => self.touchscreen_since = enable.then_some(started),
            Err(error) => {
                log::warn!("Touchscreen control failed: {error}");
                self.touchscreen_since = None;
                self.touchscreen_failure();
            }
        }
    }

    fn touchscreen_failure(&mut self) {
        self.config.display.touchscreen = false;
        self.settings_changed();
        self.with_screen(self.top_id(), |s, ctx| s.enter(ctx));
        self.toast("Touchscreen unavailable");
    }

    /// Called only after normal presentation returns; screenshots never satisfy it.
    pub fn presentation_completed(&mut self) {
        if self.screen_on {
            let id = self.top_id();
            if self.touch_screen.is_none_or(|(shown, _)| shown != id) {
                self.touch_screen = Some((id, Instant::now()));
            }
            if let Some(start) = self.resume_measurement.take() {
                crate::metrics::record(crate::metrics::Event::ResumePresent, start.elapsed());
            }
            if let Some(start) = self.wake_measurement.take() {
                crate::metrics::record(crate::metrics::Event::ScreenWake, start.elapsed());
                crate::ux::wake_presented(start.elapsed().as_micros().min(u128::from(u64::MAX)) as u64);
            }
        }
        if let Some(reply)=self.present_ack.take() {let _=reply.send(());}
    }

    #[cfg(test)]
    fn wake(&mut self, platform: &mut dyn Platform) { immediate_control(self.wake_async(platform)); }
    #[cfg(test)]
    fn wake_at(&mut self, platform: &mut dyn Platform, now: Instant) { immediate_control(self.wake_at_async(platform, now)); }
    #[cfg(test)]
    fn input(&mut self, raw: Raw, now: Instant, platform: &mut dyn Platform) { immediate_control(self.input_async(raw, now, platform)); }
    #[cfg(test)]
    fn event(&mut self, event: Event, platform: &mut dyn Platform) { immediate_control(self.event_async(event, platform)); }
    #[cfg(test)]
    pub fn finish_power(&mut self, platform: &mut dyn Platform, result: Result<(), String>, returned: Option<Instant>) {
        immediate_control(self.finish_power_async(platform, result, returned));
    }

    async fn wake_async(&mut self, platform: &mut dyn Platform) {
        self.begin_screen_restore();
        self.poll_screen_restore(platform, Instant::now()).await;
    }

    #[cfg(test)]
    async fn wake_at_async(&mut self, platform: &mut dyn Platform, now: Instant) {
        self.begin_screen_restore();
        self.restore_screen_async(platform, now).await;
    }

    fn begin_screen_restore(&mut self) {
        if !self.screen_on && self.screen_restore.is_none() {
            self.screen_restore = Some(ScreenRestore { battery_ready: false, fresh_battery: None, power: false, error: None });
        }
    }

    pub(crate) fn screen_transitioning(&self) -> bool { self.screen_blanking || self.screen_restore.is_some() }

    /// Give ready work one turn without retaining the inbox behind a helper.
    /// The main loop awaits the retained phase alongside input and health.
    async fn poll_screen_restore(&mut self, platform: &mut dyn Platform, now: Instant) {
        let restoring = self.restore_screen_async(platform, now);
        tokio::pin!(restoring);
        let _ = std::future::poll_fn(|cx| std::task::Poll::Ready(
            std::future::Future::poll(restoring.as_mut(), cx)
        )).await;
    }

    pub(crate) async fn restore_screen_async(&mut self, platform: &mut dyn Platform, now: Instant) {
        // Join accepted blanking before a later wake. Dropping this observer
        // leaves both intent and the platform-owned operation intact.
        if self.screen_restore.is_some() { crate::ux::record(crate::ux::Mark::Screen, crate::ux::WAKE_BEGIN); }
        if self.screen_blanking {
            platform.set_screen_async(false).await;
            self.screen_blanking = false;
            crate::ux::record(crate::ux::Mark::Screen, crate::ux::WAKE_BLANK_JOINED);
        }
        let Some(restore) = &self.screen_restore else { return; };
        if !restore.battery_ready {
            if self.screen_restore.as_ref().unwrap().fresh_battery.is_none() {
                // Restoration exclusively drains old periodic work, then owns
                // one fresh read across every cancelled select observer.
                if let Some(pending) = self.battery_refresh.as_mut() {
                    pending.await;
                    self.battery_refresh = None;
                }
                self.screen_restore.as_mut().unwrap().fresh_battery = platform.start_battery_refresh();
            }
            let battery = match self.screen_restore.as_mut().unwrap().fresh_battery.as_mut() {
                Some(pending) => pending.await,
                // Only immediate memory-backed platforms use this fallback.
                None => platform.battery_async().await,
            };
            self.apply_battery(battery, now);
            let restore = self.screen_restore.as_mut().unwrap();
            restore.fresh_battery = None;
            restore.battery_ready = true;
            crate::ux::record(crate::ux::Mark::Screen, crate::ux::WAKE_BATTERY_READY);
        }
        // The platform retains its accepted modeset/backlight phase. Do not
        // publish completion until re-entry has joined that exact operation.
        platform.set_screen_async(true).await;
        crate::ux::record(crate::ux::Mark::Screen, crate::ux::WAKE_ENABLED);
        let restore = self.screen_restore.take().unwrap();
        self.screen_on = true;
        self.dark_since = None;
        crate::ux::set_screen(true);
        if !restore.power { self.update_player(self.player.status()); }
        // Clock formatting is suppressed while hidden. Refresh only after
        // enable has completed, but before the first restored frame is drawn.
        self.refresh_clock(clock);
        self.player.set_visible(true);
        self.mark_dirty();
        if let Some(error) = restore.error { self.toast(error); }
    }

    fn toggle_playback(&mut self) {
        if self.stack.last().is_some_and(|(_, screen)| screen.opening_spoken()) {
            self.pop(); // A second press cancels the pending resume.
            return;
        }
        let now = Instant::now();
        let status = self.player.status();
        if status.is_active() && self.sleep_timer.is_some_and(|deadline| now < deadline && deadline.duration_since(now) <= Duration::from_secs(10)) {
            if let Some(period) = self.sleep_timer_period {
                let deadline = now + period;
                self.sleep_timer = Some(deadline);
                self.sleep_fading = false;
                self.player.send(Cmd::SetSleepTimer { deadline: Some(deadline) });
                self.toast("Sleep timer restarted");
                return;
            }
        }
        // PLAYBACK-007 / UXI-024: a recent pause resumes the retained
        // audio at once; after a long pause (or a restored queue) the
        // remote position is refreshed first.
        let recent_pause = self.paused_since.is_some_and(|at| now.duration_since(at) < SPOKEN_REFRESH_AFTER);
        if status.state == crate::audio::player::PlayState::Paused && !self.lib.offline_mode() && !recent_pause {
            if let Some(song) = status.current().filter(|song| matches!(song.spoken.as_ref().map(|book| &book.source), Some(subsonic::SpokenSource::Audiobookshelf { .. }))).cloned() {
                let action = screens::spoken::resume_in_place(song, status.generation, status.seek_revision);
                self.apply(action);
                self.dirty = true;
                return;
            }
        }
        self.player.send(Cmd::PlayPause);
    }

    async fn event_async(&mut self, ev: Event, platform: &mut dyn Platform) {
        if self.usb_drive.busy() { return; }
        let top_volume = self.stack.last().is_some_and(|s| s.1.wheel_is_volume());
        match ev {
            // The dark screen's wheel is the volume, and stays dark.
            Event::Scroll(t) if !self.screen_on || top_volume => {
                self.change_volume(t);
                return;
            }
            Event::PlayPause => { self.toggle_playback(); return; }
            Event::PlayPauseLong => {
                log::info!("sleep requested: Play/Pause held");
                self.pending.push(Request::Sleep);
                return;
            }
            Event::Next => {
                self.player.send(Cmd::Next);
                return;
            }
            Event::Prev => {
                self.player.send(Cmd::Prev);
                return;
            }
            Event::SeekForward => {
                self.player.send(Cmd::SeekBy(5000));
                return;
            }
            Event::SeekBack => {
                self.player.send(Cmd::SeekBy(-5000));
                return;
            }
            Event::BackLong => {
                // Hold Return: to Now Playing, and back again.
                if self.stack.last().is_some_and(|s| s.1.is_now_playing()) {
                    self.pop();
                } else if self.status.current().is_some() {
                    self.apply(Action::NowPlaying);
                }
                return;
            }
            _ => {}
        }
        self.wake_async(platform).await;
        let id = self.top_id();
        let action = self.with_screen(id, |s, ctx| s.event(ev, ctx)).unwrap_or(Action::None);
        if self.config.display.clicker && matches!(ev, Event::Scroll(_) | Event::Center) {
            match self.stack.last().and_then(|(_, s)| s.click_frequency()) {
                Some(hz) => platform.click_tone(hz),
                None => platform.click(),
            }
        }
        let action = match (action, ev) {
            (Action::None, Event::Back) => Action::Pop,
            (a, _) => a,
        };
        self.apply(action);
        self.dirty = true;
    }

    /// The volume of the output in use: the jack's, or the Bluetooth device's.
    fn output_volume(&self) -> u8 {
        self.outputs.volume(self.config.playback.volume)
    }

    fn change_volume(&mut self, ticks: i32) {
        let max = self.config.playback.max_volume.min(100) as i32;
        let current = self.output_volume();
        let v = (current as i32).saturating_add(ticks).clamp(0, max) as u8;
        if v != current {
            self.player.send(Cmd::SetVolume(v));
            match &mut self.outputs.bluetooth {
                // Saved per device by the Bluetooth thread, and sent to headphones with absolute volume.
                Some(bluetooth) => { bluetooth.volume = v; self.bluetooth.set_volume(bluetooth.addr, v); }
                None => { self.config.playback.volume = v; self.settings_changed(); }
            }
        }
        self.volume_shown = Some(Instant::now());
        self.dirty = true;
    }

    /// Follows the Bluetooth output: its volume, absolute volume and headphone EQ, or the jack's again.
    fn sync_outputs(&mut self) {
        let state = self.bluetooth.state();
        let mut changes = self.outputs.sync(state.output.as_ref(), self.config.playback.volume);
        // A snapshot from before a limit change can arrive on any route update, including reconnect.
        let max = self.config.playback.max_volume.min(100);
        if let Some(bluetooth) = self.outputs.bluetooth.as_mut().filter(|b| b.volume > max) {
            bluetooth.volume = max;
            changes.volume = Some(max);
        }
        self.apply_output_changes(changes);
    }

    fn apply_output_changes(&mut self, changes: outputs::Changes) {
        if let Some(remote) = changes.remote { self.player.send(Cmd::SetRemoteVolume(remote)); }
        if let Some(volume) = changes.volume { self.player.send(Cmd::SetVolume(volume)); }
        if changes.show_volume { self.volume_shown = Some(Instant::now()); self.dirty = true; }
        if changes.eq { self.player.send(Cmd::SetEq(self.outputs.eq(&self.config.playback.eq))); }
        if changes.resolve { self.resolve_output_eq(); }
    }

    /// Finds the Bluetooth device's headphone profile: matched in the catalog, or its saved choice, read from the card or downloaded.
    fn resolve_output_eq(&mut self) {
        let Some((token, addr, plan)) = self.outputs.plan() else {
            self.player.send(Cmd::SetEq(self.outputs.eq(&self.config.playback.eq)));
            return;
        };
        if !self.background { return }
        let (tx, root, offline) = (self.tx.clone(), self.paths.root.clone(), self.lib.offline_mode());
        spawn_page(async move {
            use crate::audio::autoeq;
            let (entry, matched) = match plan {
                outputs::Plan::Match(hint) => match autoeq::suggest_async(hint).await {
                    Ok(Some(entry)) => {
                        let eq = crate::bluetooth::DeviceEq { choice: crate::bluetooth::EqChoice::Automatic, enabled: true, path: entry.path().into(), name: entry.name().into() };
                        (Ok(Some(entry)), Some(eq))
                    }
                    other => (other, None),
                },
                outputs::Plan::Load { path } => (autoeq::find_async(path).await, None),
            };
            let profile = match entry {
                // Public AutoEQ data connects directly, not through Tailscale.
                Ok(Some(entry)) => entry.load_async(root, offline, None).await.map(Some),
                Ok(None) => Ok(None),
                Err(error) => Err(error),
            };
            let _ = tx.send(UiMsg::OutputEq { token, addr, matched, profile });
        });
    }

    /// Timers: long presses, the backlight, sleep, saving settings, and
    /// polling the battery and the jacks.
    pub async fn tick_async(&mut self, platform: &mut dyn Platform) { self.tick_at_async(platform, Instant::now()).await; }

    #[cfg(test)]
    pub fn tick(&mut self, platform: &mut dyn Platform) { self.tick_at(platform, Instant::now()); }

    #[cfg(test)]
    fn tick_at(&mut self, platform: &mut dyn Platform, now: Instant) {
        self.settle_doze_fixture = true;
        crate::rt::Ui::new().unwrap().block_on(self.tick_at_async(platform, now));
        self.settle_doze_fixture = false;
    }

    /// Clock is sampled at the platform boundary; policy uses the supplied
    /// local minute so generated tests exercise the actual UI owner.
    fn update_auto_sleep(&mut self, now: Instant, minute: u16) {
        if self.config.playback.auto_sleep_minutes > 0 {
            let spoken_playing = self.status.is_active() && self.status.current().is_some_and(|song| song.spoken.is_some());
            let was_armed = self.sleep_timer.is_some();
            self.auto_sleep.tick(now, minute, self.config.playback.auto_sleep_start, self.config.playback.auto_sleep_end,
                self.config.playback.auto_sleep_minutes, spoken_playing, self.status.generation, &mut self.sleep_timer);
            if !was_armed && self.sleep_timer.is_some() {
                self.sleep_timer_period = Some(Duration::from_secs(u64::from(self.config.playback.auto_sleep_minutes) * 60));
                self.player.send(Cmd::SetSleepTimer { deadline: self.sleep_timer });
            }
        } else {
            self.auto_sleep = auto_sleep::AutoSleep::default();
        }
    }

    async fn tick_at_async(&mut self, platform: &mut dyn Platform, now: Instant) {
        self.poll_screen_restore(platform, now).await;
        if self.power.pending() {
            if self.screen_transitioning() { return; }
            if self.power == Power::AwaitingOff {
                if let Some(error) = platform.power_off_error() {
                    self.finish_power_async(platform, Err(error), None).await;
                    return;
                }
            }
            let (next, effect) = power::transition(self.power, power::Event::Tick { keys_held: self.keys.any_held() });
            self.power = next;
            if let power::Effect::Enter { off } = effect { self.pending.push(Request::EnterPower { off }); }
            return;
        }

        self.player.set_visible(self.screen_on);
        self.lib.set_browsing(self.screen_on);
        // Bulk library sync uses the radio only with the screen on or on charge.
        self.lib.set_background_sync_allowed(self.screen_on || self.battery.charging);
        self.lib.set_background_sync_charging(self.battery.charging);
        self.net.set_visible(self.screen_on);
        let mut events = Vec::new();
        self.keys.tick(now, &mut events);
        for ev in events {
            self.event_async(ev, platform).await;
        }
        if self.screen_on && self.night.tick(self.config.display.night_warmth, now) { self.dirty = true; }
        let top = self.top_id();
        if self.with_screen(top, |s, ctx| s.tick(ctx)) == Some(true) {
            self.dirty = true;
        }
        if self.toast.as_ref().is_some_and(|(_, t)| now.duration_since(*t) > TOAST_TIME) {
            self.toast = None;
            self.dirty = true;
        }
        if self.volume_shown.is_some_and(|t| now.duration_since(t) > VOLUME_TIME) {
            self.volume_shown = None;
            self.dirty = true;
        }
        let minute = if self.config.playback.auto_sleep_minutes > 0 {
            let local = crate::platform::system::local_time();
            (local.tm_hour * 60 + local.tm_min) as u16
        } else { 0 };
        self.update_auto_sleep(now, minute);
        if let Some(deadline) = self.sleep_timer {
            if !self.sleep_fading && now < deadline && deadline.duration_since(now) <= Duration::from_secs(10) {
                self.sleep_fading = true;
                self.player.send(Cmd::StartSleepFade { deadline });
            }
        }
        if let Some(deadline) = self.sleep_timer.filter(|deadline| now >= *deadline) {
            self.sleep_timer = None;
            self.sleep_timer_period = None;
            self.sleep_fading = false;
            crate::ux::expect_stop(crate::ux::StopCause::SleepTimer);
            self.player.send(Cmd::FinishSleepTimer { deadline, generation: self.status.generation, rewind_ms: 15_000 });
            self.toast("Sleep timer ended");
        }
        {
            let wifi = self.net.state();
            crate::ux::set_wifi(if wifi.ssid.is_some() && wifi.ip.is_some() { 2 } else if self.net.wanted() { 1 } else { 0 });
            self.observe_transport(&wifi, now);
            crate::ux::set_offline(self.lib.offline_mode());
            crate::ux::set_doze(self.lib.radio_dozing());
            crate::ux::tick();
            self.tick_auto_resume(now);
            if !cfg!(test) && self.shuffle_reserve_at.is_some_and(|at| now >= at) && self.lib.is_online() {
                self.shuffle_reserve_at = None;
                crate::rt::io().spawn(self.lib.clone().fill_shuffle_reserve());
            }
        }
        let idle = now.duration_since(self.last_input);
        let backlight = self.config.display.backlight_seconds;
        if self.screen_on && backlight > 0 && idle > Duration::from_secs(backlight as u64) && !self.keys.any_held() {
            self.screen_on = false;
            self.sync_touchscreen(platform).await;
            self.dark_since = Some(now);
            crate::ux::set_screen(false);
            // Playback with the screen off draws no artwork.
            self.lib.art.shrink_memory(crate::library::art::MEMORY_SCREEN_OFF);
            if !cfg!(test) { crate::platform::resident::trim_after_screen_off(); }
            self.screen_blanking = true;
            self.poll_screen_restore(platform, now).await;
        }
        let sleep = self.config.power.sleep_minutes;
        if !self.screen_on
            && !self.screen_transitioning()
            && sleep > 0
            && !self.status.is_active()
            && !self.powered_download()
            && idle > Duration::from_secs(sleep as u64 * 60)
        {
            log::info!("sleep: idle {} min with the screen off", sleep);
            self.pending.push(Request::Sleep);
            self.last_input = now;
        }
        if self.config_save.is_some_and(|t| now.duration_since(t) > CONFIG_SAVE_DELAY) {
            self.start_settings_save();
        }
        self.apply_offline_requests().await;
        self.doze_tick_async(now).await;
        self.tick_bluetooth_power();
        let battery_interval = if self.screen_on { Duration::from_millis(500) } else { IDLE_TICK };
        if !self.screen_transitioning() && (self.battery_refresh.is_some()
            || self.battery_polled.is_none_or(|last| now.saturating_duration_since(last) > battery_interval)) {
            self.poll_battery_refresh(platform, now).await;
        }
        if now.duration_since(self.polled) > Duration::from_millis(500) {
            self.polled = now;
            self.refresh_clock(clock);
            // Establish a baseline on playback/enable/resume and retry unknown
            // hardware. A healthy event owner needs no repeated jack ioctl.
            if !self.status.is_active() || !self.config.playback.pause_on_unplug { self.headphones = None; }
            else if self.headphones.is_none() || !platform.headphone_events() {
                self.refresh_headphones_async(platform).await;
            }
        }
    }

    /// Wi-Fi doze while cached songs play; the policy is in doze.rs.
    #[cfg(test)]
    fn doze_tick(&mut self, now: Instant) {
        self.settle_doze_fixture = true;
        crate::rt::Ui::new().unwrap().block_on(self.doze_tick_async(now));
        self.settle_doze_fixture = false;
    }

    async fn doze_cache_ready(&mut self, token: u64, now: Instant) {
        if self.doze_cache.has_token(token) {
            let position = self.doze_position(now);
            self.doze_checked = self.doze_cache.receive(token, &self.status, self.lib.client_generation(), position);
            self.doze_tick_async(now).await;
        }
    }

    fn doze_position(&self, now: Instant) -> u64 {
        let seen = (Arc::as_ptr(&self.status.queue) as usize, self.status.index, self.status.position_ms);
        let since = self.doze_seen.filter(|(previous, _)| *previous == seen)
            .map_or(Duration::ZERO, |(_, at)| now.saturating_duration_since(at));
        self.status.position_ms.saturating_add(if matches!(self.status.state, PlayState::Playing | PlayState::Buffering) {
            since.as_millis().min(u128::from(u64::MAX)) as u64
        } else { 0 })
    }

    fn powered_download(&self) -> bool {
        (self.battery.plugged || self.battery.charging) && self.lib.downloads.transferring()
    }

    async fn doze_tick_async(&mut self, now: Instant) {
        self.doze_probe.expire(now);
        let enabled = self.config.power.radio_doze && !self.portal.interactive();
        let dozing = self.net.dozing();
        // The radio's intent is the state and the library's flag follows it.
        // An explicit Wi-Fi or Offline choice, or the Wi-Fi worker rolling a
        // failed radio-down back, ends a doze without this policy's wake.
        if !dozing && self.lib.radio_dozing() {
            self.lib.set_radio_doze(false);
            self.doze_refill = true;
            self.doze_log.finish(now);
        }
        if !enabled && !dozing && !self.doze_refill { return; }
        let playing = matches!(self.status.state, PlayState::Playing | PlayState::Buffering);
        let key = (Arc::as_ptr(&self.status.queue) as usize, self.status.index);
        let seen = (key.0, key.1, self.status.position_ms);
        if self.doze_seen.is_none_or(|(s, _)| s != seen) { self.doze_seen = Some((seen, now)); }
        let fetching = self.lib.fetching();
        // A fetch that just ended changes what is cached: recheck at once.
        if self.doze_fetching && !fetching { self.doze_cache.invalidate(); self.doze_checked = None; }
        self.doze_fetching = fetching;
        let position = self.doze_position(now);
        let music = self.lib.client_generation();
        let paused_after = doze::paused_after(self.config.power.doze_paused_minutes);
        let paused = self.status.state == PlayState::Paused;
        if dozing || ((playing || (paused && paused_after.is_some())) && !self.screen_on) {
            if self.doze_checked.is_none_or(|(t, q, i)| (q, i) != key || now.saturating_duration_since(t) >= doze::RECHECK)
                || self.doze_cache.ahead(&self.status, music, position).is_none() {
                self.doze_cache.request(self.lib.clone(), self.tx.clone(), &self.status, position, now);
            }
        }
        // Ordinary fixture ticks explicitly settle real owner work before
        // evaluating policy. Production tick/handle never take this path.
        #[cfg(test)]
        if self.settle_doze_fixture && self.doze_cache.pending() {
            self.doze_checked = self.doze_cache.settle(&self.status, music, position).await;
        }
        self.doze_ahead_ms = self.doze_cache.ahead(&self.status, music, position)
            .or_else(|| dozing.then(|| self.doze_cache.carried_ahead(&self.status, music, position)).flatten()).unwrap_or(0);
        // Pending lookup never authorizes a new doze. While already dozing,
        // use only a consumption-adjusted bound from the same immutable queue;
        // replaced queue/account/content yields zero and wakes safely.
        if !dozing && self.doze_cache.pending() { self.doze_ahead_ms = 0; }
        // A player stream waits for the radio: wake it as for low audio.
        if dozing && self.lib.playback_wants_radio() { self.doze_ahead_ms = 0; }
        let wifi = self.net.state();
        let inputs = doze::Inputs {
            enabled,
            wifi_wanted: self.net.wanted(),
            connected: wifi.ssid.is_some() && wifi.ip.is_some(),
            dozing,
            playing,
            stopped: self.status.state == PlayState::Stopped,
            screen_on: self.screen_on,
            idle: now.saturating_duration_since(self.last_input),
            dark: self.dark_since.filter(|_| !self.screen_on).map_or(Duration::ZERO, |at| now.saturating_duration_since(at)),
            ahead_ms: self.doze_ahead_ms,
            fetching: fetching || self.powered_download(),
            paused_after,
        };
        let thresholds = if self.config.storage.fetch_minutes > 0 {
            doze::Thresholds::after_fetch(self.config.storage.fetch_low_minutes)
        } else {
            doze::Thresholds::from_minutes(self.config.power.doze_ahead_minutes, self.config.power.doze_wake_minutes)
        };
        match doze::step(inputs, thresholds) {
            doze::Effect::Doze => {
                // A completed fetch can permit doze on this very tick.
                // Observe its end before Dozed closes the awake cycle.
                self.doze_log.awake(now, inputs.connected, fetching, self.doze_ahead_ms, playing);
                let observation_begin = if self.doze_probe.armed() { crate::playtrace::boottime_us() } else { 0 };
                self.lib.set_radio_doze(true);
                self.net.set_doze_async(true).await;
                if !dozing && self.net.dozing() {
                    let confirmed = if self.doze_probe.armed() { crate::playtrace::boottime_us() } else { 0 };
                    if let Some(command) = self.doze_probe.entered(Instant::now(), observation_begin, confirmed, self.doze_ahead_ms) {
                        self.player.send(command);
                    }
                }
                let epoch = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64);
                let boot = crate::metrics::uptime().parse::<f64>().unwrap_or(0.0);
                self.doze_log.dozed(now, epoch, boot, self.doze_ahead_ms);
                let queued = u32::try_from(self.doze_ahead_ms).unwrap_or(u32::MAX);
                self.lib.trace().event(crate::playtrace::Kind::Doze, queued, 0, 0);
                crate::ux::record(crate::ux::Mark::DozeAhead, crate::ux::tagged(0, self.doze_ahead_ms / 1000));
            }
            doze::Effect::Wake(reason) => {
                self.net.set_doze_async(false).await;
                self.lib.set_radio_doze(false);
                self.doze_refill = true;
                self.doze_log.woke(now, reason, self.doze_ahead_ms);
                let queued = u32::try_from(self.doze_ahead_ms).unwrap_or(u32::MAX);
                self.lib.trace().event(crate::playtrace::Kind::Wake, reason as u32 + 1, queued, 0);
                crate::ux::record(crate::ux::Mark::DozeAhead, crate::ux::tagged(reason as u32 + 1, self.doze_ahead_ms / 1000));
            }
            doze::Effect::None => {}
        }
        if !self.net.dozing() {
            self.doze_log.awake(now, inputs.connected, fetching, self.doze_ahead_ms, playing);
            if inputs.stopped || !enabled { self.doze_log.finish(now); }
        }
        if self.doze_refill && !self.net.dozing() && wifi.ip.is_some() && self.lib.is_online() {
            self.doze_refill = false;
            let next = self.status.index.saturating_add(1).min(self.status.queue.len());
            self.lib.prefetch_upcoming(&self.status.queue[next..]);
        }
    }

    fn refresh_clock(&mut self, read: impl FnOnce(bool) -> String) {
        // Neither a dark header nor a page-title header displays this clock.
        // Wake refreshes it before the first visible frame.
        if !self.screen_on || !self.config.display.clock_in_title { return; }
        let text = read(self.config.display.clock_24h);
        if text != self.shown_clock { self.shown_clock = text; self.dirty = true; }
    }

    async fn refresh_battery_async(&mut self, platform: &mut dyn Platform, now: Instant) {
        // Wake still requires a fresh observation before its first frame.
        // Join an older periodic request without publishing it, then read
        // afresh. Keep the accepted future in self across cancellation and
        // never accumulate overlapping periodic jobs or publish an old reply
        // after the fresh wake observation.
        if let Some(pending) = self.battery_refresh.as_mut() {
            pending.await;
            self.battery_refresh = None;
        }
        let battery = platform.battery_async().await;
        self.apply_battery(battery, now);
    }

    async fn poll_battery_refresh(&mut self, platform: &mut dyn Platform, now: Instant) {
        if self.battery_refresh.is_none() {
            self.battery_refresh = platform.start_battery_refresh();
            if self.battery_refresh.is_none() {
                self.refresh_battery_async(platform, now).await;
                return;
            }
        }
        // A single poll starts/observes the existing finite job, registering
        // this task's waker while always returning control to the inbox.
        let outcome = std::future::poll_fn(|cx| std::task::Poll::Ready(
            self.battery_refresh.as_mut().unwrap().as_mut().poll(cx)
        )).await;
        if let std::task::Poll::Ready(battery) = outcome {
            self.battery_refresh = None;
            self.apply_battery(battery, now);
        }
    }

    fn apply_battery(&mut self, battery: Battery, now: Instant) {
        self.battery_polled = Some(now);
        if battery != self.battery {
            self.battery = battery;
            self.dirty = true;
        }
    }

    pub fn animating(&mut self) -> bool {
        if !self.screen_on { return false; }
        let now = Instant::now();
        let mut any = false;
        if let Some((_, s)) = self.stack.last_mut() {
            any |= s.animate(now);
            // A slower animation whose frame is due.
            if self.screen_on && s.next_frame(now) == Some(Duration::ZERO) {
                self.dirty = true;
            }
        }
        any
    }

    /// When the main loop should next wake up with nothing to do.
    pub fn next_wake(&self) -> Duration {
        if self.power.pending() { return Duration::from_secs(1); }
        // Keep the existing active-playback cadence for unknown jack state
        // and platforms without event delivery. Healthy mainline jack state
        // no longer performs periodic observations at these ticks.
        let now = Instant::now();
        let mut d = if self.screen_on { Duration::from_millis(250) }
            else if self.status.is_active() && self.config.playback.pause_on_unplug { Duration::from_secs(1) }
            else { IDLE_TICK };
        if !self.screen_on {
            // Only deadlines still ahead: one that passed without effect (a
            // failed save, sleep blocked by playback or the charger) is
            // rechecked by the regular tick, never in a tight loop.
            let mut until = |at: Instant| if at > now { d = d.min(at - now + Duration::from_millis(50)); };
            if let Some(t) = self.config_save { until(t + CONFIG_SAVE_DELAY); }
            let sleep = self.config.power.sleep_minutes as u64 * 60;
            if sleep > 0 { until(self.last_input + Duration::from_secs(sleep)); }
        }
        if let Some(deadline) = self.sleep_timer {
            d = d.min(deadline.saturating_duration_since(now));
            if !self.sleep_fading {
                d = d.min(deadline.saturating_duration_since(now + Duration::from_secs(10)));
            }
        }
        if let Some(t) = self.keys.next_deadline() {
            d = d.min(t.saturating_duration_since(Instant::now()));
        }
        if self.screen_on {
            if let Some(f) = self.stack.last().and_then(|s| s.1.next_frame(Instant::now())) {
                d = d.min(f);
            }
        }
        d
    }

    pub fn needs_draw(&self) -> bool {
        self.dirty && self.screen_on
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    /// The recorded strings as JSON (`record-text` must have run).
    pub fn script_text(&self) -> Option<serde_json::Value> {
        let record = self.text_record.as_ref()?;
        let seen: Vec<_> = record.seen.iter().map(|(ms, text)| serde_json::json!({"ms": ms, "text": text})).collect();
        let title = self.stack.last().map(|(_, screen)| screen.title()).unwrap_or_default();
        let status = &self.status;
        let play = serde_json::json!({"state": format!("{:?}", status.state), "index": status.index,
            "generation": status.generation, "position_ms": status.position_ms,
            "song": status.current().map(|song| song.title.clone()), "error": status.error.is_some()});
        Some(serde_json::json!({"title": title, "current": record.current, "seen": seen, "play": play,
            "ms": record.started.elapsed().as_millis() as u64}))
    }

    pub fn render(&mut self) -> &Canvas {
        if self.text_record.is_some() { self.fonts.capture(true); }
        self.render_frame();
        if let Some(record) = &mut self.text_record {
            let at = record.started.elapsed().as_millis() as u64;
            let previous = std::mem::replace(&mut record.current, self.fonts.capture(false));
            for text in &record.current {
                if record.seen.len() < 100_000 && !previous.contains(text) { record.seen.push((at, text.clone())); }
            }
        }
        &self.canvas
    }

    fn render_frame(&mut self) {
        self.dirty = false;
        let theme = self.theme;
        self.canvas.set_clip(Rect::new(0, 0, WIDTH, HEIGHT));
        self.canvas.clear(theme.bg);
        let area = Rect::new(0, STATUS_H, WIDTH, HEIGHT - STATUS_H);
        // An overlay draws over the screen below it.
        let first = match self.stack.iter().rposition(|s| !s.1.is_overlay()) {
            Some(i) => i,
            None => 0,
        };
        let n = self.stack.len();
        let output_volume = self.output_volume();
        for i in first..n {
            let mut d = Draw {
                c: &mut self.canvas,
                fonts: &mut self.fonts,
                theme,
                lib: &self.lib,
                status: &self.status,
                volume: self.volume_shown.map(|_| output_volume),
                sleep_timer: self.sleep_timer.map(|deadline| (deadline, self.sleep_fading)),
                now: Instant::now(),
                placeholders: &mut self.placeholders,
            };
            if i > first {
                d.c.set_clip(Rect::new(0, 0, WIDTH, HEIGHT));
                d.c.dim(70);
            }
            d.c.set_clip(area);
            self.stack[i].1.draw(&mut d, area);
        }
        self.canvas.set_clip(Rect::new(0, 0, WIDTH, HEIGHT));
        self.draw_status_bar();
        self.touch_occlusion = [None; 2];
        // Now Playing shows the volume in its own bar.
        if self.volume_shown.is_some() && !self.stack.last().is_some_and(|s| s.1.is_now_playing()) {
            self.draw_volume();
        }
        if let Some((t, _)) = self.toast.clone() {
            self.draw_toast(&t);
        }
        self.night.apply(&mut self.canvas.pixels);
    }

    /// The classic's title bar: silver, the page's name in bold, the play
    /// state at the left and the battery at the right.
    fn draw_status_bar(&mut self) {
        let wifi = self.net.state();
        // UXI-002: wpa_supplicant leaves COMPLETED during rescans and roams
        // while the address and the servers stay usable. Show the link as down
        // only once that has lasted WIFI_GRACE, or at once without an address.
        let completed = wifi.enabled && wifi.wpa_state == "COMPLETED" && wifi.ip.is_some();
        let now = Instant::now();
        if completed { self.wifi_completed_at = Some(now); }
        let wifi_online = completed || (wifi.enabled && wifi.ip.is_some()
            && self.wifi_completed_at.is_some_and(|at| now.duration_since(at) < WIFI_GRACE));
        let tail = self.tailscale.state();
        let offline = self.lib.offline_mode();
        let server_online = self.lib.is_online();
        // A LocalAPI status that timed out says nothing: with the server
        // answering, Tailscale is carrying it (UX-005).
        let tail_online = tail.status == "Connected" || (tail.observation.transport().is_none() && server_online);
        let badge = connectivity::select(offline, wifi_online,
            self.config.tailscale.enabled, tail_online, server_online);
        if let Some(record) = self.connectivity_trace.observe([
            wifi.enabled, wifi.wpa_state == "COMPLETED", wifi.ip.is_some(), self.net.dozing(),
            offline, self.config.tailscale.enabled, server_online, tail_online,
        ], tail.observation, badge) {
            use connectivity::SelectionRecord;
            let (event, value) = match record {
                SelectionRecord::Selected(value) => (crate::metrics::Event::BadgeSelected, value),
                SelectionRecord::Limit => (crate::metrics::Event::BadgeSelectedLimit, connectivity::SELECTION_LIMIT),
            };
            crate::metrics::record_code(event, value);
        }
        let title = match self.stack.last() {
            _ if self.config.display.clock_in_title => clock(self.config.display.clock_24h),
            Some((_, s)) if !s.is_overlay() => s.title(),
            _ => self.stack.iter().rev().find(|s| !s.1.is_overlay()).map(|s| s.1.title()).unwrap_or_default(),
        };
        player_chrome::Header {
            title: &title,
            play: self.status.state,
            level: self.battery.level,
            charging: self.battery.charging,
            badge,
            bars: self.net.signal_bars(),
        }.draw(&mut self.canvas, &mut self.fonts, self.theme);
    }

    /// Tells the library whether the link to Navidrome can work (docs/
    /// server-status.md): Wi-Fi without an address for [`WIFI_GRACE`] or
    /// turned off, or Tailscale (when used) known stopped, is down. A doze
    /// and its reconnect allowance excuse a missing address; simulated
    /// Wi-Fi and an unknown Tailscale state say nothing.
    fn observe_transport(&mut self, wifi: &crate::net::WifiState, now: Instant) {
        if self.lib.radio_dozing() || self.net.dozing() {
            self.transport_excused_until = Some(now + Duration::from_millis(q2_machines::doze::RECONNECT_MS));
        }
        let tailscale = if self.config.tailscale.enabled { self.tailscale.state().observation.transport() } else { Some(true) };
        if (self.net.simulated() || wifi.ip.is_some()) && !self.lib.radio_dozing() && !self.net.dozing() { self.lib.link_available(); }
        let wifi_up = if self.net.simulated() { Some(true) } else if !self.net.wanted() { Some(false) } else if wifi.ip.is_some() {
            self.transport_seen_at = Some(now);
            Some(true)
        } else {
            let excused = self.transport_excused_until.is_some_and(|until| now < until)
                || self.transport_seen_at.is_some_and(|at| now.duration_since(at) < WIFI_GRACE);
            if excused { None } else { Some(false) }
        };
        match (wifi_up, tailscale) {
            (Some(false), _) | (_, Some(false)) => self.lib.set_transport(false),
            (Some(true), Some(true)) => self.lib.set_transport(true),
            _ => {}
        }
    }

    /// The volume, where a page doesn't show it itself: a
    /// translucent panel with the classic's bar between two speakers.
    fn draw_volume(&mut self) {
        let volume = self.output_volume();
        let th = self.theme;
        let r = Rect::new(36, HEIGHT - 72, WIDTH - 72, 44);
        self.touch_occlusion[0] = Some(r);
        self.canvas.fill_round_rect_alpha(r, 10, th.hud, th.hud_alpha);
        volume_bar(&mut self.canvas, &th, Rect::new(r.x + 14, r.y + 12, r.w - 28, 20), volume, Color::WHITE);
    }

    fn draw_toast(&mut self, text: &str) {
        let th = self.theme;
        let w = (self.fonts.measure(text, 16, Weight::Bold) + 36).min(WIDTH - 60);
        let r = Rect::new((WIDTH - w) / 2, HEIGHT - 80, w, 42);
        let inset = corner_inset(r.bottom());
        let r = Rect::new(r.x.max(inset), r.y, w.min(WIDTH - 2 * inset), r.h);
        self.touch_occlusion[1] = Some(r);
        self.canvas.fill_round_rect_alpha(r, 10, th.hud, th.hud_alpha);
        let lh = self.fonts.line_height(16);
        let t = self.fonts.ellipsize(text, 16, Weight::Bold, r.w - 24).into_owned();
        self.fonts.draw_centered(&mut self.canvas, &t, WIDTH / 2, r.y + (r.h - lh) / 2, 16, Weight::Bold, Color::WHITE);
    }

    pub fn toast(&mut self, text: impl Into<String>) {
        self.toast = Some((crate::ux::present(&text.into()), Instant::now()));
        self.dirty = true;
    }

    /// Restores the last session's queue, paused.
    pub async fn restore_queue(&mut self) {
        if let Some((songs, index, position_ms)) = self.lib.saved_queue_async().await {
            if !songs.is_empty() {
                self.player.send(Cmd::Restore { songs: std::sync::Arc::new(songs), index, position_ms });
            }
        }
    }
}

/// The time now, as the status bar shows it.
pub fn clock(h24: bool) -> String {
    let tm=crate::platform::system::local_time();
    if h24 {
        format!("{:02}:{:02}", tm.tm_hour, tm.tm_min)
    } else {
        let h = match tm.tm_hour % 12 {
            0 => 12,
            h => h,
        };
        format!("{h}:{:02} {}", tm.tm_min, if tm.tm_hour < 12 { "AM" } else { "PM" })
    }
}

pub fn channel() -> (UiTx, UiRx) {
    tokio::sync::mpsc::unbounded_channel()
}

/// Tests: the next message within `wait`, polling the inbox.
#[cfg(test)]
pub(crate) fn recv_within(rx: &mut UiRx, wait: Duration) -> Option<UiMsg> {
    let end = Instant::now() + wait;
    loop {
        if let Ok(m) = rx.try_recv() { return Some(m); }
        if Instant::now() >= end { return None; }
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(duration(187), "3:07");
        assert_eq!(duration(3723), "1:02:03");
        assert_eq!(long_duration(3900), "1 hr 5 min");
        assert_eq!(long_duration(2500), "42 min");
    }
}

/// Real list/font/Cover Flow drawing with generated data; no user library.
pub fn benchmark(blocking: &crate::rt::Blocking, root: &std::path::Path, iterations: usize) -> Result<serde_json::Value,String> {
    let config = Config::default();
    let lib = Library::open(Paths::new(root.to_owned()), &config, Arc::new(|_|{}));
    let start = Instant::now();
    let mut fonts = Fonts::load(blocking, &[])?;
    let font_us = start.elapsed().as_micros() as u64;
    let theme = Theme::new(config.display.look,config.display.accent);
    let mut canvas = Canvas::new(WIDTH,HEIGHT);
    let status = Status::default();
    let mut placeholders = HashMap::new();
    let mut results = serde_json::Map::new();
    let mut checksums = Vec::new();
    for media in [false,true] {
        let mut list = list::ListView::new(media);
        list.set_rows((0..1000).map(|i| {
            let row = list::Row::new(format!("Track {i:04} — Tomorrow's Music"),list::RowAction::None);
            if media { row.detail("A synthetic artist · 2026") } else { row }
        }).collect());
        let mut samples = Vec::new();
        let trace_start = Instant::now();
        let mut checksum = 0u64;
        for i in 0..iterations+10 {
            // Fixed wheel timing: machine speed must not change acceleration
            // or which rows are drawn. Include input and animation work.
            let now = trace_start + Duration::from_millis(i as u64 * 32);
            let started = Instant::now();
            list.wheel(if i % 80 < 40 { 1 } else { -1 }, now);
            list.animate(now);
            canvas.clear(theme.bg);
            let mut d = Draw { c:&mut canvas,fonts:&mut fonts,theme,lib:&lib,status:&status,volume:None,sleep_timer:None,now:Instant::now(),placeholders:&mut placeholders };
            list.draw(&mut d,Rect::new(0,STATUS_H,WIDTH,HEIGHT-STATUS_H));
            let elapsed = started.elapsed().as_micros() as u64;
            std::hint::black_box(&canvas.pixels);
            // Outside the timed region; check every rendered frame.
            for p in &canvas.pixels { checksum = checksum.wrapping_mul(31).wrapping_add(*p as u64); }
            if i>=10 { samples.push(elapsed); }
        }
        checksums.push(checksum);
        results.insert(if media {"media_list_median_us"} else {"text_list_median_us"}.into(),bench::median(&samples).into());
        results.insert(if media {"media_list_us"} else {"text_list_us"}.into(),serde_json::json!(samples));
    }
    bench::scenes(&mut canvas, &mut fonts, theme, iterations, &mut results);
    results.insert("font_load_us".into(),font_us.into());
    results.insert("list_checksums".into(),serde_json::json!(checksums));
    Ok(results.into())
}


/// The `ui-parts` benchmark kind (`bench::parts`).
pub fn parts_benchmark(blocking: &crate::rt::Blocking, iterations: usize) -> Result<serde_json::Value, String> { bench::parts(blocking, iterations) }

/// The `ui-check` benchmark kind (`bench::check`).
pub fn span_check() -> Result<serde_json::Value, String> { bench::check() }

#[cfg(test)]
fn immediate_control<T>(future: impl std::future::Future<Output = T>) -> T {
    let mut future = std::pin::pin!(future);
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    match future.as_mut().poll(&mut context) {
        std::task::Poll::Ready(value) => value,
        std::task::Poll::Pending => panic!("memory-only control fixture became asynchronous; await its production API"),
    }
}
