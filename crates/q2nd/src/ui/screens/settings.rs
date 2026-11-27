//! Settings grouped by task, with direct toggles, checked choices and sliders.

use super::controls::{Choice, Confirm};
use super::text::{Kind, TextEntry};
use crate::audio::player::{Cmd, PlayState};
use crate::config::{Accent, HeadphonePower, Look, Quality, RatingControls, ReplayGainMode, options};
use crate::gfx::Rect;
use crate::input::Event;
use crate::library::Policy;
use crate::sysinfo::SystemInfo;
use crate::timezones;
use crate::ui::list::{ListView, Row, RowAction};
use crate::ui::{Action, Ctx, Dest, Draw, Request, Screen};
use std::time::{Duration, Instant};
use std::sync::Arc;

mod pages;
pub use pages::Page;
use pages::{FILTER, GAIN, SOUND_CHECK};

/// Navidrome's state in the words the Audiobookshelf row uses: Offline is the
/// listener's Offline Mode (or the radio resting), Unavailable a server that
/// does not answer.
fn navidrome_status(lib: &crate::library::Library) -> &'static str {
    if !lib.is_configured() { "Not Set" }
    else if lib.offline_mode() || lib.radio_dozing() { "Offline" }
    else {
        match lib.server_status() {
            crate::library::ServerStatus::Online => "Connected",
            // A request failed; the health probe decides (docs/server-status.md).
            crate::library::ServerStatus::Checking => "Checking…",
            crate::library::ServerStatus::Offline => "Unavailable",
        }
    }
}

// Row commands.
const ABS_URL:u32=70;
const ABS_TOKEN:u32=71;
const ABS_TEST:u32=74;
const ABS_SYNC:u32=81;
const ABS_FULL:u32=82;
const FILL_DRIVE: u32 = 83;
const FILL_SHARE: u32 = 84;
const SKIP_MODE:u32=72;
const SKIP_TIME:u32=73;
const SKIP_BACK:u32=76;
const DEFAULT_SPEED:u32=77;
const AUTO_SLEEP_DURATION:u32=78;
const AUTO_SLEEP_START:u32=79;
const AUTO_SLEEP_END:u32=80;
const LISTENBRAINZ_ENABLED:u32=91;
const LISTENBRAINZ_TOKEN:u32=92;
const LISTENBRAINZ_CONTACT:u32=93;
const LASTFM_ENABLED:u32=94;
const USB_DRIVE:u32=75;
const URL: u32 = 1;
const USER: u32 = 2;
const PASSWORD: u32 = 3;
const TEST: u32 = 4;
const RESCAN: u32 = 5;
const FULL_MUSIC_SYNC: u32 = 307;
const STREAM_Q: u32 = 10;
const DOWNLOAD_Q: u32 = 11;
const GAPLESS: u32 = 12;
const SCROBBLE: u32 = 14;
const SYNC_QUEUE: u32 = 15;
const UNPLUG: u32 = 19;
const CACHE: u32 = 20;
const PREFETCH: u32 = 21;
const CLEAR_CACHE: u32 = 22;
const NIGHT: u32 = 36;
const RATINGS: u32 = 37;
const BACKLIGHT: u32 = 31;
const ACCENT: u32 = 32;
const LOOK: u32 = 33;
const CLOCK24: u32 = 34;
const CLOCK_IN_TITLE: u32 = 35;
const SLEEP: u32 = 40;
const CLICKER: u32 = 41;
const TOUCHSCREEN: u32 = 42;
const OFFLINE_MODE: u32 = 50;
const REPORT_PROBLEM: u32 = 60;
/// The token for reading system information.
const SYSINFO: u64 = 60;

struct UsbProbe { available: bool, activate: bool }
struct ShelfProbe {configuration:Arc<()>,result:Result<(),String>}
pub struct Settings {
    usb_available: bool,
    usb_pending: bool,
    usb_activating: bool,
    usb_token: u64,
    shelf_generation:Option<Arc<()>>,
    shelf_offline:bool,
    shelf_token:u64,
    shelf_status:&'static str,
    shelf_error:Option<String>,
    page: Page,
    list: ListView,
    level: u8,
    level_bar: Rect,
    level_done: Rect,
    entered: bool,
    refreshed: Instant,
    info: Option<SystemInfo>,
    /// Navidrome's status as last shown on Connections; the machine changes
    /// Online ↔ Checking without a library event.
    navidrome_shown: &'static str,
    listenbrainz_shown: &'static str,
}

pub fn server_page() -> Box<dyn Screen> {
    Box::new(Settings::new(Page::Server))
}

fn on_off(b: bool) -> &'static str {
    if b { "On" } else { "Off" }
}

fn gb(bytes: u64) -> String {
    if bytes >= 1 << 30 { format!("{:.1} GB", bytes as f64 / (1u64 << 30) as f64) } else { format!("{} MB", bytes >> 20) }
}

impl Settings {
    pub fn new(page: Page) -> Settings {
        let list = ListView::settings(page.tall_rows());
        Settings { usb_available:false,usb_pending:false,usb_activating:false,usb_token:0,shelf_generation:None,shelf_offline:false,shelf_token:0,shelf_status:"Not Set",shelf_error:None,page, list, level: 50, level_bar: Rect::default(), level_done: Rect::default(), entered: false, refreshed: Instant::now(), info: None, navidrome_shown: "", listenbrainz_shown: "" }
    }

    fn probe_usb(&mut self, ctx: &Ctx, activate: bool) {
        if self.page != Page::Connections || !ctx.background || self.usb_activating { return; }
        if self.usb_pending {
            // Finish the one owned read, then perform a fresh activation check.
            // Repeated Center cannot accumulate jobs behind a slow sysfs read.
            self.usb_activating = activate;
            return;
        }
        self.usb_pending = true;
        self.usb_activating = activate;
        self.usb_token = self.usb_token.wrapping_add(1);
        let drive = ctx.usb_drive.clone();
        ctx.spawn_blocking(self.usb_token, move |blocking, reply| {
            reply.send(UsbProbe { available: drive.available(blocking), activate });
        });
    }
    fn shelf_status(&self,ctx:&Ctx)->&'static str {
        if ctx.config.audiobookshelf.url.trim().is_empty() || ctx.config.audiobookshelf.token.is_empty() {return "Not Set";}
        if ctx.lib.offline_mode() {return "Offline";}
        if !self.shelf_generation.as_ref().is_some_and(|generation|ctx.lib.shelf_generation_current(generation)) || self.shelf_offline {return "Checking…";}
        self.shelf_status
    }
    fn probe_shelf(&mut self,ctx:&mut Ctx,force:bool) {
        if !matches!(self.page,Page::Connections|Page::Audiobookshelf) {return;}
        if force { drop(ctx.lib.configure_async(ctx.config)); }
        let configuration=ctx.lib.shelf_generation();let offline=ctx.lib.offline_mode();
        let same=self.shelf_generation.as_ref().is_some_and(|old|Arc::ptr_eq(old,&configuration)) && self.shelf_offline==offline;
        if same && (!force || self.shelf_status=="Checking…") {return;}
        self.shelf_generation=Some(configuration.clone());self.shelf_offline=offline;
        self.shelf_token=self.shelf_token.wrapping_add(1);self.shelf_error=None;
        if ctx.config.audiobookshelf.url.trim().is_empty() || ctx.config.audiobookshelf.token.is_empty() {self.shelf_status="Not Set";return;}
        if offline {self.shelf_status="Offline";return;}
        self.shelf_status="Checking…";
        let lib=ctx.lib.clone();ctx.task(self.shelf_token,move |reply| async move {
            let applied = lib.shelf_configuration_ready(&configuration).await.map_err(|error| error.to_string());
            let result = match applied { Ok(()) => lib.check_shelf_connection_async().await.map_err(|error|error.to_string()), Err(error) => Err(error) };
            reply.send(ShelfProbe {configuration,result});
        });
    }
    fn rows(&self, ctx: &Ctx) -> Vec<Row> {
        let c = &ctx.config;
        let p = &c.playback;
        let set = |label: &str, cmd: u32, value: String| Row::new(label, RowAction::Cmd(cmd)).key(label).value(value);
        let choice = |label: &str, cmd: u32, value: String| Row::screen(label, cmd).key(label).value(value);
        let link = |label: &str, page: Page| Row::link(label, Dest::SettingsPage(page));
        // A setting for hardware that did not open: shown, not selectable.
        let unavailable = |label: &str| Row::new(label, RowAction::None).key(label).value("Unavailable".to_string());
        let wifi_status = || {
            let net = ctx.net.state();
            if !net.enabled { "Off" } else if net.ip.is_some() { "Connected" } else { "Not Connected" }
        };
        match self.page {
            Page::Audiobookshelf=>vec![Row::screen("Address",ABS_URL).detail(if c.audiobookshelf.url.is_empty(){"Not set"}else{&c.audiobookshelf.url}),
                choice("API Token",ABS_TOKEN,if c.audiobookshelf.token.is_empty(){"Not set".into()}else{"••••••".into()}),
                Row::new("Test Connection",RowAction::Cmd(ABS_TEST)).value(self.shelf_status(ctx)).detail(self.shelf_error.clone().unwrap_or_default()),
                Row::new("Sync Now",RowAction::Cmd(ABS_SYNC)).key("abs-sync").value(shelf_sync_value(ctx, false)).detail("New and removed books and episodes"),
                Row::new("Full Resync",RowAction::Cmd(ABS_FULL)).key("abs-full").value(shelf_sync_value(ctx, true)).detail("Everything again; also daily while charging"),
                Row::new("Create an API key in Audiobookshelf",RowAction::None).detail("Settings → Users → API keys")],
            Page::Root => pages::root_rows(
                p.eq.graphic_enabled || p.eq.headphone_enabled,
                c.display.brightness,
                wifi_status(),
                if c.bluetooth.enabled { ctx.bluetooth.state().summary() } else { "Off".into() },
                Dest::Bluetooth,
            ),
            Page::Sound => pages::sound_rows(p.eq.summary(), p.replay_gain, p.max_volume, p.headphone_power(), p.dac_filter),
            Page::About => about_rows(crate::sysinfo::installed_os_version()),
            Page::SystemInfo => match &self.info {
                Some(info) => system_rows(info),
                None => vec![Row::new("Reading…", RowAction::None)],
            },
            Page::Server => {
                let url = if c.server.url.is_empty() { "Not set".to_string() } else { c.server.url.clone() };
                let user = if c.server.user.is_empty() { "Not set".to_string() } else { c.server.user.clone() };
                let pw = if c.server.password.is_empty() && c.server.api_key.is_empty() { "Not set" } else { "••••••" };
                vec![
                    Row::screen("Address", URL).detail(url),
                    Row::screen("User", USER).detail(user),
                    choice("Password", PASSWORD, pw.into()),
                    Row::new("Test Connection", RowAction::Cmd(TEST)),
                    Row::new("Scan Library", RowAction::Cmd(RESCAN)),
                    Row::screen("Full Sync", FULL_MUSIC_SYNC).detail("Refresh downloads and review deletions"),
                    Row::new("Or edit config.toml", RowAction::None).detail("in Q2Navidrome on the card"),
                ]
            }
            Page::Playback => vec![
                link("Auto Sleep Timer", Page::AutoSleep).value(options::minutes_label(u32::from(p.auto_sleep_minutes))),
                set("Gapless", GAPLESS, on_off(p.gapless).into()),
                if ctx.hardware.headphone_switch { set("Pause on Unplug", UNPLUG, on_off(p.pause_on_unplug).into()) } else { unavailable("Pause on Unplug") },
                choice("Song Ratings", RATINGS, c.display.rating_controls.label().into()),
                Row::screen("Default Spoken Speed", DEFAULT_SPEED).separated()
                    .value(format!("{}.{:02}×", c.spoken.speed_percent / 100, c.spoken.speed_percent % 100)),
                set("Book Skip Buttons", SKIP_MODE, if c.spoken.skip_chapter { "One Chapter".into() } else { "Time".into() }),
                choice("Skip Forward", SKIP_TIME, format!("{} Seconds", c.spoken.skip_forward_seconds)),
                choice("Skip Backward", SKIP_BACK, format!("{} Seconds", c.spoken.skip_backward_seconds)),
                choice("Streaming Quality", STREAM_Q, p.stream_quality.label().into()).separated(),
                choice("Download Quality", DOWNLOAD_Q, p.download_quality.label().into()),
                set("Navidrome Scrobbles", SCROBBLE, on_off(p.scrobble).into()).detail("Library music only"),
                link("ListenBrainz", Page::ListenBrainz).value(on_off(c.listenbrainz.enabled)),
                link("Last.fm", Page::LastFm).value(on_off(c.lastfm.enabled)),
                set("Sync Queue", SYNC_QUEUE, on_off(p.sync_queue).into()),
            ],
            Page::AutoSleep => vec![
                choice("Duration", AUTO_SLEEP_DURATION, options::minutes_label(u32::from(p.auto_sleep_minutes))).detail("Audiobooks and podcasts only"),
                choice("Auto Timer Starts", AUTO_SLEEP_START, options::clock_label(p.auto_sleep_start)),
                choice("Auto Timer Ends", AUTO_SLEEP_END, options::clock_label(p.auto_sleep_end)),
            ],
            Page::ListenBrainz => vec![
                set("Scrobbling", LISTENBRAINZ_ENABLED, on_off(c.listenbrainz.enabled).into()).detail("Music and radio"),
                Row::screen("API Token", LISTENBRAINZ_TOKEN).detail(if c.listenbrainz.token.is_empty() { "From listenbrainz.org" } else { "Saved" }),
                Row::screen("Client Contact URL", LISTENBRAINZ_CONTACT).detail("HTTPS maintainer contact"),
                Row::new("Status", RowAction::None).detail(ctx.lib.listenbrainz_status()),
            ],
            Page::LastFm => vec![
                set("Scrobbling", LASTFM_ENABLED, on_off(c.lastfm.enabled).into()).detail("Music and radio"),
                Row::new("Status", RowAction::None).detail(ctx.lib.lastfm_status()),
                Row::new("Edit config.toml", RowAction::None).detail("Add authorized Last.fm settings on the card"),
            ],
            Page::Storage => {
                let mut rows = vec![
                    Row::link("Storage Usage", Dest::StorageUsage),
                    set("Offline Mix", FILL_DRIVE, on_off(c.storage.fill_my_drive).into())
                        .detail("Rotates music, audiobooks and podcasts"),
                ];
                if c.storage.fill_my_drive {
                    rows.push(choice("Mix Size", FILL_SHARE, format!("{}% of card", c.storage.fill_target_percent.clamp(10, 90))));
                }
                rows.extend([
                    choice("Song Cache Limit", CACHE, options::cache_limit_label(c.storage.stream_cache_mb)).detail("Streamed songs only, not downloads"),
                    choice("Fetch Ahead", PREFETCH, format!("{} Songs", c.storage.prefetch_songs)),
                    Row::screen("Clear Song Cache…", CLEAR_CACHE),
                    // UXI-014: the current artwork policy, stated; the owner has not chosen a limit.
                    Row::new("Cover Art", RowAction::None).key("cover-art-policy").detail("Kept on the card, with no time or size limit"),
                ]);
                rows
            }
            Page::Brightness | Page::VolumeLimit => vec![],
            Page::Display => vec![
                link("Brightness", Page::Brightness).value(format!("{}%", c.display.brightness)),
                choice("Backlight Timeout", BACKLIGHT, options::backlight_label(c.display.backlight_seconds)),
                set("Night Warmth", NIGHT, if c.display.night_warmth { "21–07" } else { "Off" }.into()),
                set("Theme", LOOK, if c.display.look == Look::Classic { "Classic" } else { "Dark" }.into()),
                choice("Selection Color", ACCENT, c.display.accent.label().into()),
                set("Time in Title", CLOCK_IN_TITLE, on_off(c.display.clock_in_title).into()),
            ],
            Page::Connections => {
                let mut rows = vec![
                Row::link("Wi-Fi", Dest::Wifi).value(wifi_status()),
                link("Navidrome Server", Page::Server).value(navidrome_status(ctx.lib)),
                link("Audiobookshelf",Page::Audiobookshelf).value(self.shelf_status(ctx)),
                Row::link("Tailscale", Dest::Tailscale).value(if c.tailscale.enabled { ctx.tailscale.state().status } else { "Off".into() }),
                set("Offline Mode", OFFLINE_MODE, on_off(ctx.lib.offline_mode()).into()),
                ];
                // USB Drive, when shown, stays last: scripts reach the rows above it by position.
                if self.usb_available {
                    rows.push(Row::new("USB Drive", RowAction::Cmd(USB_DRIVE)).key("usb-drive").detail("Edit the card from your computer"));
                }
                rows
            },
            Page::System => {
                let clicker = if ctx.hardware.clicker {
                    set("Clicker", CLICKER, on_off(c.display.clicker).into())
                } else {
                    unavailable("Clicker")
                };
                let mut rows = vec![clicker];
                if ctx.hardware.touchscreen {
                    rows.push(set("Touchscreen", TOUCHSCREEN, on_off(c.display.touchscreen).into()));
                }
                rows.extend([
                    choice("Sleep When Idle", SLEEP, options::minutes_label(c.power.sleep_minutes)),
                    link("Date & Time", Page::DateTime),
                    link("About", Page::About).separated(),
                ]);
                rows
            },
            Page::DateTime => {
                vec![
                    link("Time Zone", Page::TimeZone).detail(timezones::label(&c.display.timezone)),
                    set("24-Hour Clock", CLOCK24, on_off(c.display.clock_24h).into()),
                ]
            }
            Page::TimeZone => timezones::rows(&c.display.timezone),
        }
    }

    fn text(title: &str, initial: &str, kind: Kind, secret: bool, set: fn(&mut crate::config::Config, String)) -> Action {
        Action::Push(Box::new(TextEntry::new(
            title,
            initial,
            kind,
            secret,
            Box::new(move |text, ctx| {
                set(ctx.config, text);
                *ctx.config_changed = true;
                Action::Pop
            }),
        )))
    }

    fn cmd(&mut self, cmd: u32, ctx: &mut Ctx) -> Action {
        if cmd == DEFAULT_SPEED { return Action::Push(Box::new(super::spoken::PlaybackSpeed::default_setting())); }
        if let Some(choice) = choice_screen(cmd, ctx.config) { return Action::Push(Box::new(choice)); }
        let changed = |ctx: &mut Ctx| *ctx.config_changed = true;
        let c = &mut *ctx.config;
        match cmd {
            REPORT_PROBLEM=>{crate::ux::problem();return Action::Toast("Problem reported. Thank you".into());}
            ABS_TEST=>{self.probe_shelf(ctx,true);return Action::Stay;}
            ABS_SYNC|ABS_FULL=>{ctx.lib.request_shelf_sync(cmd==ABS_FULL);self.list.set_rows(self.rows(ctx));return Action::Stay;}
            LISTENBRAINZ_TOKEN => return Self::text("ListenBrainz API Token", "", Kind::Full, true, |c, t| c.listenbrainz.token = t),
            LISTENBRAINZ_CONTACT => return Self::text("Client Contact URL", &c.listenbrainz.contact_url, Kind::Url, false, |c, t| c.listenbrainz.contact_url = t),
            ABS_URL=>return Self::text("Audiobookshelf Address",&c.audiobookshelf.url.clone(),Kind::Url,false,|c,t|c.audiobookshelf.url=t),
            ABS_TOKEN=>return Self::text("Audiobookshelf API Token","",Kind::Full,true,|c,t|c.audiobookshelf.token=t),
            USB_DRIVE => {
                self.probe_usb(ctx, true);
                return Action::Stay;
            }
            SKIP_MODE=>c.spoken.skip_chapter=!c.spoken.skip_chapter,
            URL => {
                let init = if c.server.url.is_empty() { "http://".to_string() } else { c.server.url.clone() };
                return Self::text("Server Address", &init, Kind::Url, false, |c, t| c.server.url = t);
            }
            USER => return Self::text("User", &c.server.user.clone(), Kind::Full, false, |c, t| c.server.user = t),
            PASSWORD => {
                return Self::text("Password", "", Kind::Full, true, |c, t| {
                    c.server.password = t;
                    c.server.api_key.clear();
                });
            }
            FULL_MUSIC_SYNC => return Action::Push(Box::new(super::music_sync::FullSync::new())),
            TEST | RESCAN => {
                let lib = ctx.lib.clone();
                ctx.task(cmd as u64, move |reply| async move {
                    let r = if cmd == TEST {
                        lib.fetch_async(subsonic::calls::ping(), Policy::NetworkOnly).await.map(|_| ())
                    } else {
                        lib.fetch_async(subsonic::calls::start_scan(), Policy::NetworkOnly).await.map(|_| ())
                    };
                    reply.send(r.map_err(|e| e.to_string()));
                });
                return Action::Toast(if cmd == TEST { "Connecting…" } else { "Asking the server to scan" }.into());
            }
            FILL_DRIVE => c.storage.fill_my_drive = !c.storage.fill_my_drive,
            GAPLESS => {
                c.playback.gapless = !c.playback.gapless;
                ctx.player.send(Cmd::SetGapless(c.playback.gapless));
            }
            LISTENBRAINZ_ENABLED => c.listenbrainz.enabled = !c.listenbrainz.enabled,
            LASTFM_ENABLED => c.lastfm.enabled = !c.lastfm.enabled,
            SCROBBLE => c.playback.scrobble = !c.playback.scrobble,
            SYNC_QUEUE => c.playback.sync_queue = !c.playback.sync_queue,
            UNPLUG => c.playback.pause_on_unplug = !c.playback.pause_on_unplug,
            CLEAR_CACHE => return Action::Push(Box::new(Confirm::clear_cache(|ctx| {
                    let lib = ctx.lib.clone();
                    ctx.notice_task(async move { match lib.clear_song_cache_async().await { Ok(()) => "Song cache cleared".into(), Err(error) => error } });
                    Action::Many(vec![Action::Pop, Action::Toast("Clearing song cache…".into())])
                },
            ))),
            NIGHT => c.display.night_warmth = !c.display.night_warmth,
            LOOK => c.display.look = if c.display.look == Look::Classic { Look::Dark } else { Look::Classic },
            CLOCK24 => c.display.clock_24h = !c.display.clock_24h,
            CLOCK_IN_TITLE => c.display.clock_in_title = !c.display.clock_in_title,
            CLICKER => c.display.clicker = !c.display.clicker,
            TOUCHSCREEN if ctx.hardware.touchscreen => {
                c.display.touchscreen = !c.display.touchscreen;
                changed(ctx);
                return if ctx.config.display.touchscreen {
                    Action::Toast("Touchscreen is a work in progress".into())
                } else {
                    Action::Stay
                };
            }
            OFFLINE_MODE => {
                let offline = !ctx.lib.offline_mode();
                ctx.lib.set_offline_mode(offline);
                ctx.requests.push(Request::OfflineMode { offline, requested: c.wifi.enabled, networks: c.wifi.networks.clone() });
                return Action::None;
            }
            z if z >= timezones::ZONE_COMMAND => {
                return timezones::change(z, &c.display.timezone, |ctx, id| {
                    ctx.config.display.timezone = id.into();
                    *ctx.config_changed = true;
                    ctx.requests.push(Request::Restart);
                    Action::Pop
                });
            }
            _ => return Action::None,
        }
        changed(ctx);
        Action::None
    }
}

/// The Audiobookshelf sync rows' value: the running round with its count of
/// stored entries, or the last outcome.
fn shelf_sync_value(ctx: &Ctx, full: bool) -> String {
    use crate::library::shelf_auto_sync::Status;
    let (running_full, entries) = ctx.lib.shelf_sync_progress();
    match ctx.lib.shelf_sync_status() {
        Status::Syncing if running_full == full => if entries > 0 { format!("Syncing… {entries}") } else { "Syncing…".into() },
        Status::Syncing => String::new(),
        _ if full => String::new(),
        Status::Waiting => "Waiting".into(),
        Status::Current => "Up to date".into(),
        Status::Unavailable => "Incomplete".into(),
    }
}

fn about_rows(os_version: Option<&str>) -> Vec<Row> {
    pages::about_rows(env!("CARGO_PKG_VERSION"), os_version.unwrap_or("Unavailable"), Some(REPORT_PROBLEM))
}

fn system_rows(i: &SystemInfo) -> Vec<Row> {
    let info = |label: &str| Row::new(label, RowAction::None);
    let mut rows = vec![info("Q2 Navidrome").value(env!("CARGO_PKG_VERSION"))];
    if let Some(os) = &i.os { rows.push(info("Operating System").detail(os.as_str())); }
    if let Some(k) = &i.kernel {
        let arch = i.arch.as_deref().filter(|a| !k.contains(a)).map(|a| format!(" · {a}")).unwrap_or_default();
        rows.push(info("Kernel").detail(format!("Linux {k}{arch}")));
    }
    if let Some(b) = &i.kernel_build { rows.push(info("Kernel Build").detail(b.as_str())); }
    let hardware = rows.len();
    if let Some(m) = &i.model { rows.push(info("Model").detail(m.as_str())); }
    if let Some(p) = i.soc.as_ref().or(i.cpu.as_ref()) {
        let r = info("Processor").detail(p.as_str());
        rows.push(if i.cores > 0 { r.value(if i.cores == 1 { "1 Core".into() } else { format!("{} Cores", i.cores) }) } else { r });
    }
    if let (Some(_), Some(c)) = (&i.soc, &i.cpu) { rows.push(info("CPU").detail(c.as_str())); }
    if let Some(m) = i.memory_bytes { rows.push(info("Memory").value(gb(m))); }
    if let Some(c) = i.card_bytes { rows.push(info("Card Capacity").value(gb(c))); }
    if let Some(r) = rows.get_mut(hardware) { r.separator = true; }
    rows
}

fn choose<T: Copy + PartialEq + 'static>(
    title: &str, values: &[T], current: T, label: impl Fn(T) -> String,
    apply: impl Fn(T, &mut Ctx) + 'static,
) -> Choice {
    let selected = values.iter().position(|v| *v == current);
    let labels = values.iter().map(|v| label(*v)).collect();
    let values = values.to_vec();
    Choice::new(title, labels, selected, Box::new(move |i, ctx| {
        apply(values[i], ctx);
        *ctx.config_changed = true;
        Action::Pop
    }))
}

fn choice_screen(cmd: u32, c: &crate::config::Config) -> Option<Choice> {
    Some(match cmd {
        FILL_SHARE => {
            let mut values: Vec<u8> = (10..=90).step_by(5).collect();
            let selected = c.storage.fill_target_percent.clamp(10, 90);
            if !values.contains(&selected) { values.push(selected); values.sort_unstable(); }
            choose("Mix Size", &values, selected,
                |percent| format!("{percent}% of card"),
                |percent, ctx| ctx.config.storage.fill_target_percent = percent)
        }
        AUTO_SLEEP_DURATION => choose("Duration", &options::AUTO_SLEEP_DURATIONS, c.playback.auto_sleep_minutes,
            |n| options::minutes_label(u32::from(n)), |n,ctx| ctx.config.playback.auto_sleep_minutes=n),
        AUTO_SLEEP_START | AUTO_SLEEP_END => choose(if cmd == AUTO_SLEEP_START { "Auto Timer Starts" } else { "Auto Timer Ends" },
            &(0u16..48).map(|n| n*30).collect::<Vec<_>>(),
            if cmd == AUTO_SLEEP_START { c.playback.auto_sleep_start } else { c.playback.auto_sleep_end }, options::clock_label,
            move |n,ctx| if cmd == AUTO_SLEEP_START {ctx.config.playback.auto_sleep_start=n} else {ctx.config.playback.auto_sleep_end=n}),
        SKIP_TIME | SKIP_BACK => choose(if cmd == SKIP_TIME { "Skip Forward" } else { "Skip Backward" },
            &options::SKIP_SECONDS, if cmd == SKIP_TIME { c.spoken.skip_forward_seconds } else { c.spoken.skip_backward_seconds },
            |n| format!("{n} Seconds"), move |n,ctx| {
                if cmd == SKIP_TIME {ctx.config.spoken.skip_forward_seconds=n;} else {ctx.config.spoken.skip_backward_seconds=n;}
            }),
        STREAM_Q | DOWNLOAD_Q => choose(
            if cmd == STREAM_Q { "Streaming Quality" } else { "Download Quality" },
            &Quality::ALL, if cmd == STREAM_Q { c.playback.stream_quality } else { c.playback.download_quality },
            |q| q.label().into(), move |q, ctx| {
                if cmd == STREAM_Q { ctx.config.playback.stream_quality = q; }
                else { ctx.config.playback.download_quality = q; }
            },
        ),
        SOUND_CHECK => choose("Volume Normalization", &ReplayGainMode::ALL,
            c.playback.replay_gain, |m| m.label().into(), |m, ctx| {
                ctx.config.playback.replay_gain = m;
                ctx.player.send(Cmd::SetReplayGain(m));
            }),
        GAIN => {
            let levels = HeadphonePower::ALL;
            let current = levels.iter().position(|level| *level == c.playback.headphone_power());
            Choice::new("Headphone Power", levels.iter().map(|level| level.label().into()).collect(), current, Box::new(move |i, ctx| {
                let level = levels[i];
                ctx.config.playback.set_headphone_power(level);
                ctx.player.send(Cmd::SetHeadphonePower(level));
                *ctx.config_changed = true;
                // The codecs change level only with the headphone output
                // closed; the player closes it at the next pause.
                if matches!(ctx.status.state, PlayState::Playing | PlayState::Buffering) {
                    Action::Many(vec![Action::Pop, Action::Toast("Applies at the next pause".into())])
                } else { Action::Pop }
            })).details(levels.iter().map(|level| level.detail().into()).collect())
        }
        FILTER => choose("DAC Filter", &[0, 1, 2, 3], c.playback.dac_filter,
            |i| options::DAC_FILTERS[i as usize].into(), |i, ctx| {
                ctx.config.playback.dac_filter = i;
                ctx.player.send(Cmd::SetFilter(i));
            }),
        CACHE => choose("Song Cache Limit", &options::CACHE_LIMITS, c.storage.stream_cache_mb,
            options::cache_limit_label, |mb, ctx| ctx.config.storage.stream_cache_mb = mb),
        PREFETCH => choose("Fetch Ahead", &options::FETCH_AHEAD, c.storage.prefetch_songs,
            |n| format!("{n} Songs"), |n, ctx| ctx.config.storage.prefetch_songs = n),
        RATINGS => choose("Song Ratings", &RatingControls::ALL, c.display.rating_controls,
            |r| r.label().into(), |r, ctx| ctx.config.display.rating_controls = r),
        BACKLIGHT => choose("Backlight Timeout", &options::BACKLIGHT_TIMEOUTS, c.display.backlight_seconds,
            options::backlight_label, |s, ctx| ctx.config.display.backlight_seconds = s),
        ACCENT => choose("Selection Color", &Accent::ALL, c.display.accent,
            |a| a.label().into(), |a, ctx| ctx.config.display.accent = a),
        SLEEP => choose("Sleep When Idle", &options::IDLE_TIMEOUTS, c.power.sleep_minutes,
            options::minutes_label, |m, ctx| ctx.config.power.sleep_minutes = m),
        _ => return None,
    })
}

impl Screen for Settings {

    fn clickable(&self, x: i32, y: i32) -> bool {
        super::hits(self.level_bar, x, y) || super::hits(self.level_done, x, y) || self.list.hit(x, y).is_some()
    }

    fn click(&mut self, x: i32, y: i32, ctx: &mut Ctx) -> Action {
        if super::hits(self.level_done, x, y) {
            return self.event(Event::Center, ctx);
        }
        if super::hits(self.level_bar, x, y) {
            let level = super::slider_step(self.level_bar, x, 100).max(1);
            return self.event(Event::Scroll(level - i32::from(self.level)), ctx);
        }
        let selected = self.list.selected;
        if self.list.select_at(x, y) {
            if self.list.selected != selected { self.usb_activating = false; }
            self.event(Event::Center, ctx)
        } else {
            Action::None
        }
    }

    fn script_ready(&self) -> bool { !self.usb_pending && (self.page != Page::SystemInfo || self.info.is_some()) }
    #[cfg(test)]
    fn audit(&self) -> crate::ui::hierarchy::Surface { if matches!(self.page, Page::Brightness | Page::VolumeLimit) { crate::ui::hierarchy::Surface::widget("Settings", format!("level={}", self.level)) } else { crate::ui::hierarchy::Surface::list(&self.list) } }
    fn title(&self) -> String {
        self.page.title().into()
    }

    fn enter(&mut self, ctx: &mut Ctx) {
        // The status shown must be tested now, not remembered from before
        // the server last failed (SERVER-001; docs/server-status.md).
        if self.page == Page::Connections { ctx.lib.recheck_server(); }
        self.probe_usb(ctx, false);
        self.probe_shelf(ctx,self.entered);
        self.level = if self.page == Page::VolumeLimit { ctx.config.playback.max_volume } else { ctx.config.display.brightness };
        if self.page == Page::SystemInfo && self.info.is_none() && !self.entered {
            let card = ctx.paths.media_root().to_path_buf();
            ctx.spawn_blocking(SYSINFO, move |blocking, reply| reply.send(SystemInfo::get(blocking, &card)));
        }
        let rows = self.rows(ctx);
        self.list.set_rows(rows);
        if self.page == Page::TimeZone && !self.entered {
            if let Some(index) = self.list.rows.iter().position(|row| matches!(row.badge, crate::ui::list::Badge::Check)) {
                self.list.select(index);
            }
        }
        self.entered = true;
    }

    fn event(&mut self, ev: Event, ctx: &mut Ctx) -> Action {
        if matches!(self.page, Page::Brightness | Page::VolumeLimit) {
            return match ev {
                Event::Scroll(t) => {
                    let volume = self.page == Page::VolumeLimit;
                    let previous = if volume { ctx.config.playback.max_volume } else { ctx.config.display.brightness };
                    self.level = (previous as i32).saturating_add(t).clamp(1, 100) as u8;
                    if self.level != previous {
                        if volume {
                            ctx.config.playback.max_volume = self.level;
                            ctx.bluetooth.set_volume_limit(self.level);
                            let active = ctx.outputs.volume(ctx.config.playback.volume);
                            ctx.config.playback.volume = ctx.config.playback.volume.min(self.level);
                            if active > self.level {
                                if let Some(headphones) = &mut ctx.outputs.bluetooth {
                                    headphones.volume = self.level;
                                }
                                ctx.player.send(Cmd::SetVolume(self.level));
                            }
                        } else {
                            ctx.config.display.brightness = self.level;
                            ctx.requests.push(Request::SetBrightness(self.level));
                        }
                        *ctx.config_changed = true;
                    }
                    Action::Stay
                }
                Event::Center => Action::Pop,
                _ => Action::None,
            };
        }
        match ev {
            Event::WheelEnd => { self.list.end_spin(); Action::None }
            Event::Scroll(t) => {
                let selected = self.list.selected;
                self.list.wheel(t, ctx.now);
                if self.list.selected != selected { self.usb_activating = false; }
                Action::None
            }
            Event::Center => {
                let Some(row) = self.list.selected_row().cloned() else { return Action::None };
                if !matches!(row.action, RowAction::Cmd(USB_DRIVE)) { self.usb_activating = false; }
                let a = match row.action {
                    RowAction::Open(dest) => Action::Push(super::open(dest, ctx)),
                    RowAction::Cmd(c) => self.cmd(c, ctx),
                    _ => Action::None,
                };
                let rows = self.rows(ctx);
                self.list.set_rows(rows);
                a
            }
            _ => Action::None,
        }
    }

    fn loaded(&mut self, token: u64, data: Box<dyn std::any::Any + Send>, ctx: &mut Ctx) {
        let data = match data.downcast::<UsbProbe>() {
            Ok(probe) => {
                if token != self.usb_token { return; }
                let activate_after_probe = self.usb_activating && !probe.activate;
                let activate = self.usb_activating && probe.activate;
                self.usb_pending = false;
                self.usb_activating = false;
                self.usb_available = probe.available;
                if activate_after_probe { self.probe_usb(ctx, true); }
                else if activate {
                    ctx.requests.push(if probe.available { Request::UsbDrive }
                        else { Request::Toast("Connect USB to a computer first".into()) });
                }
                let rows = self.rows(ctx); self.list.set_rows(rows); return;
            }
            Err(data) => data,
        };
        let data=match data.downcast::<ShelfProbe>() {
            Ok(probe)=>{
                if token==self.shelf_token && ctx.lib.shelf_generation_current(&probe.configuration) && !ctx.lib.offline_mode() {
                    self.shelf_status=if probe.result.is_ok() {"Connected"} else {"Unavailable"};
                    // Plain words beside the status, the fixed code in the report (UX-006).
                    self.shelf_error=probe.result.err().map(|error|crate::ux::present_in(&error,crate::ux::Journey::OpenPage));
                }
                self.probe_shelf(ctx,false);
                let rows=self.rows(ctx);self.list.set_rows(rows);return;
            }
            Err(data)=>data,
        };
        let data = match data.downcast::<SystemInfo>() {
            Ok(info) => {
                self.info = Some(*info);
                let rows = self.rows(ctx);
                self.list.set_rows(rows);
                return;
            }
            Err(data) => data,
        };
        if let Ok(r) = data.downcast::<Result<(), String>>() {
            let msg = match *r {
                Ok(()) => "Connected".to_string(),
                Err(e) => e,
            };
            ctx.requests.push(Request::Toast(msg));
        }
        let rows = self.rows(ctx);
        self.list.set_rows(rows);
    }

    fn refresh(&mut self, ev: &crate::library::LibEvent, ctx: &mut Ctx) {
        if matches!(ev, crate::library::LibEvent::ArtReady) { return; }
        self.probe_shelf(ctx,false);
        let rows = self.rows(ctx);
        self.list.set_rows(rows);
    }

    fn network_changed(&mut self, ctx: &mut Ctx) {
        if matches!(self.page, Page::Root | Page::Connections) { self.list.set_rows(self.rows(ctx)); }
    }
    fn tailscale_changed(&mut self, ctx: &mut Ctx) {
        if self.page == Page::Connections { self.list.set_rows(self.rows(ctx)); }
    }
    fn bluetooth_changed(&mut self, ctx: &mut Ctx) {
        if self.page == Page::Root { self.list.set_rows(self.rows(ctx)); }
    }

    fn tick(&mut self, ctx: &mut Ctx) -> bool {
        // USB availability still needs its bounded hardware observation. Its
        // completion refreshes rows; network/library publications handle status.
        if self.page == Page::Connections && ctx.now.saturating_duration_since(self.refreshed) >= Duration::from_secs(2) {
            self.refreshed = ctx.now;
            self.probe_usb(ctx, false);
        }
        if matches!(self.page, Page::ListenBrainz | Page::LastFm) {
            let status = if self.page == Page::ListenBrainz { ctx.lib.listenbrainz_status() } else { ctx.lib.lastfm_status() };
            if status != self.listenbrainz_shown {
                self.listenbrainz_shown = status;
                self.list.set_rows(self.rows(ctx));
                return true;
            }
        }
        // Checking → Connected publishes no library event.
        if self.page == Page::Connections {
            let status = navidrome_status(ctx.lib);
            if status != self.navidrome_shown {
                self.navidrome_shown = status;
                self.list.set_rows(self.rows(ctx));
                return true;
            }
        }
        false
    }

    fn draw(&mut self, d: &mut Draw, area: Rect) {
        if matches!(self.page, Page::Brightness | Page::VolumeLimit) {
            self.level_bar = pages::draw_level(d, area, self.level, self.page == Page::VolumeLimit);
            self.level_done = super::center_target(d, "Wheel: adjust · Center: done", area.x + area.w / 2, area.y + 180);
        } else { self.list.draw(d, area); }
    }

    fn animate(&mut self, now: Instant) -> bool {
        self.list.animate(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abs_status_requires_current_live_probe_and_groups_with_peer_servers() {
        let mut app=crate::ui::settings_tests::fixture();app.background=false;
        app.config.audiobookshelf.url="http://generated.fixture.invalid".into();
        app.config.audiobookshelf.token="generated-token".into();app.lib.configure(&app.config);
        app.push(Box::new(Settings::new(Page::Connections)));
        let rows=app.stack.last().unwrap().1.audit();
        assert_eq!(&rows.rows()[..4].iter().map(|row|row.title.as_str()).collect::<Vec<_>>(),&["Wi-Fi","Navidrome Server","Audiobookshelf","Tailscale"]);
        assert!(!rows.rows()[2].separator);assert_eq!(rows.rows()[2].value.as_deref(),Some("Checking…"));
        app.push(Box::new(Settings::new(Page::Audiobookshelf)));let id=app.top_id();
        let configuration=app.lib.shelf_generation();
        let value=|app:&crate::ui::App|app.stack.last().unwrap().1.audit().rows().iter().find(|row|row.title=="Test Connection").unwrap().value.clone().unwrap();
        app.handle(crate::ui::UiMsg::Loaded {screen:id,token:1,data:Box::new(ShelfProbe {configuration:configuration.clone(),result:Ok(())})},&mut crate::platform::Headless {last:None});
        assert_eq!(value(&app),"Connected");
        app.event(Event::Scroll(2),&mut crate::platform::Headless {last:None});app.event(Event::Center,&mut crate::platform::Headless {last:None});
        assert_eq!(value(&app),"Checking…");
        // Repeated retry while pending coalesces; a late old error cannot win.
        app.event(Event::Center,&mut crate::platform::Headless {last:None});
        app.handle(crate::ui::UiMsg::Loaded {screen:id,token:1,data:Box::new(ShelfProbe {configuration:configuration.clone(),result:Err("old error".into())})},&mut crate::platform::Headless {last:None});assert_eq!(value(&app),"Checking…");
        app.handle(crate::ui::UiMsg::Loaded {screen:id,token:2,data:Box::new(ShelfProbe {configuration:configuration.clone(),result:Err("generated unavailable".into())})},&mut crate::platform::Headless {last:None});assert_eq!(value(&app),"Unavailable");
        app.event(Event::Center,&mut crate::platform::Headless {last:None});
        app.config.audiobookshelf.token="generated-other-account".into();app.lib.configure(&app.config);
        app.handle(crate::ui::UiMsg::Loaded {screen:id,token:3,data:Box::new(ShelfProbe {configuration,result:Ok(())})},&mut crate::platform::Headless {last:None});assert_eq!(value(&app),"Checking…");
        app.lib.set_offline_mode(true);
        let current=app.lib.shelf_generation();
        app.handle(crate::ui::UiMsg::Loaded {screen:id,token:4,data:Box::new(ShelfProbe {configuration:current,result:Ok(())})},&mut crate::platform::Headless {last:None});assert_eq!(value(&app),"Offline");
        app.player.shutdown();
    }
    #[test]
    fn about_orders_independent_app_and_os_versions_without_connection_duplicates() {
        for os in [Some("V0.16"), None] {
            let rows = about_rows(os);
            let titles: Vec<_> = rows.iter().map(|r| r.title.as_str()).collect();
            assert_eq!(titles, ["Q2 Navidrome", "OS Version", "System Information", "Report a Problem"]);
            assert_eq!(rows[0].value.as_deref(), Some(env!("CARGO_PKG_VERSION")));
            assert_eq!(rows[1].value.as_deref(), Some(os.unwrap_or("Unavailable")));
            assert!(matches!(rows[0].action, RowAction::None));
            assert!(matches!(rows[1].action, RowAction::None));
            assert!(matches!(rows[2].action, RowAction::Open(Dest::SettingsPage(Page::SystemInfo))));
        }
    }

    #[test]
    fn system_rows_skip_missing_fields_and_group_hardware() {
        let titles = |rows: &[Row]| rows.iter().map(|r| r.title.as_str()).collect::<Vec<_>>().join(",");
        let q2 = SystemInfo {
            os: Some("Buildroot 2020.02.1".into()), kernel: Some("5.10.21+".into()),
            kernel_build: Some("#1 SMP PREEMPT".into()), arch: Some("mips".into()), model: None,
            soc: Some("Ingenic X2000".into()), cpu: Some("Ingenic XBurst II V0.0 FPU V0.0".into()), cores: 2,
            memory_bytes: Some(120 << 20), card_bytes: Some(64 << 30),
        };
        let rows = system_rows(&q2);
        assert_eq!(titles(&rows), "Q2 Navidrome,Operating System,Kernel,Kernel Build,Processor,CPU,Memory,Card Capacity");
        assert_eq!(rows[2].detail.as_deref(), Some("Linux 5.10.21+ · mips"));
        assert_eq!(rows[4].value.as_deref(), Some("2 Cores"));
        assert!(rows[4].separator && rows.iter().filter(|r| r.separator).count() == 1);
        assert!(rows.iter().all(|r| matches!(r.action, RowAction::None)));
        let host = SystemInfo { kernel: Some("7.2.7.x86_64".into()), arch: Some("x86_64".into()), cpu: Some("Ryzen".into()), cores: 1, ..Default::default() };
        let rows = system_rows(&host);
        assert_eq!(titles(&rows), "Q2 Navidrome,Kernel,Processor");
        assert_eq!(rows[1].detail.as_deref(), Some("Linux 7.2.7.x86_64"));
        assert_eq!(rows[2].value.as_deref(), Some("1 Core"));
    }
}

/// Manual timer for the Now Playing menu.
pub(super) fn sleep_timer_choice(ctx: &Ctx) -> Box<dyn Screen> {
    let period = if ctx.sleep_timer.is_none() { Some(Duration::ZERO) } else { *ctx.sleep_timer_period };
    pages::sleep_timer_choice(period, |period, ctx| {
        *ctx.sleep_fading = false;
        *ctx.sleep_timer_period = period;
        *ctx.sleep_timer = period.map(|period| ctx.now + period);
        ctx.player.send(Cmd::SetSleepTimer { deadline: *ctx.sleep_timer });
    })
}
