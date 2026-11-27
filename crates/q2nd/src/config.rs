#![forbid(unsafe_code)]
//! Settings, kept as TOML on the card (`Q2Navidrome/config.toml`) so the
//! server can be set up from a computer, and rewritten when changed on the Q2.

mod keep;
mod save;
pub use save::Save;
pub mod telemetry;
pub use telemetry::Telemetry;

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: Server,
    pub audiobookshelf: Audiobookshelf,
    pub spoken: SpokenSettings,
    pub playback: Playback,
    #[serde(skip_serializing_if = "ListenBrainz::is_default")]
    pub listenbrainz: ListenBrainz,
    #[serde(skip_serializing_if = "LastFm::is_default")]
    pub lastfm: LastFm,
    pub storage: Storage,
    pub display: Display,
    pub power: Power,
    pub wifi: Wifi,
    pub tailscale: Tailscale,
    pub bluetooth: Bluetooth,
    pub telemetry: Telemetry,
    /// Written only when something in it is set.
    #[serde(skip_serializing_if = "Diagnostics::is_default")]
    pub diagnostics: Diagnostics,
    /// Wi-Fi sign-in details typed into the card file by hand. Not shown in
    /// Settings. Never written from this value: a save keeps the card file's
    /// own `[portal]` text (config/keep.rs), and the internal copy omits it.
    #[serde(skip_serializing)]
    pub portal: PortalSettings,
}

/// `[portal]`: provider accounts for Wi-Fi sign-in. Unknown keys are kept so
/// a save from Settings rewrites the table as it was read.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PortalSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<DeltaAccount>,
    #[serde(flatten)]
    pub other: toml::Table,
}

/// `[portal.delta]`: `username` or `skymiles_number` (Delta accepts either),
/// and `password`. Values stay as written, string or number, so a save keeps
/// them byte for byte in meaning.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DeltaAccount {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<toml::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skymiles_number: Option<toml::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<toml::Value>,
    #[serde(flatten)]
    pub other: toml::Table,
}

impl DeltaAccount {
    /// The account name and password for sign-in, when both are present.
    pub fn sign_in(&self) -> Option<(zeroize::Zeroizing<String>, zeroize::Zeroizing<String>)> {
        fn text(value: &Option<toml::Value>) -> Option<String> {
            match value {
                Some(toml::Value::String(s)) if !s.is_empty() => Some(s.clone()),
                Some(toml::Value::Integer(n)) => Some(n.to_string()),
                _ => None,
            }
        }
        let user = text(&self.username).map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
            .or_else(|| text(&self.skymiles_number).map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()))?;
        let password = text(&self.password)?;
        Some((zeroize::Zeroizing::new(user), zeroize::Zeroizing::new(password)))
    }
}

// Account values never reach logs or panic messages.
impl std::fmt::Debug for PortalSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PortalSettings").field("delta", &self.delta)
            .field("other_keys", &self.other.len()).finish()
    }
}

impl std::fmt::Debug for DeltaAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DeltaAccount").field("configured", &self.sign_in().is_some()).finish_non_exhaustive()
    }
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ListenBrainz {
    pub enabled: bool,
    pub token: String,
    pub contact_url: String,
}

impl ListenBrainz {
    fn is_default(&self) -> bool { *self == Self::default() }
}

impl std::fmt::Debug for ListenBrainz {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ListenBrainz").field("enabled", &self.enabled).finish_non_exhaustive()
    }
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LastFm {
    pub enabled: bool,
    pub api_key: String,
    pub shared_secret: String,
    pub session: String,
    pub contact_url: String,
}

impl LastFm {
    fn is_default(&self) -> bool { *self == Self::default() }
}

impl std::fmt::Debug for LastFm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LastFm").field("enabled", &self.enabled).finish_non_exhaustive()
    }
}

/// Measurement switches for rigs. Read at start; not shown in Settings.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Diagnostics {
    /// Record the playback trace (docs/playtrace.md). Off until its cost on
    /// the Q2 is measured.
    pub playtrace: bool,
}

impl Diagnostics {
    fn is_default(&self) -> bool { *self == Diagnostics::default() }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tailscale {
    /// Route all Navidrome requests through the local Tailscale proxy. The
    /// daemon is the one the image ships (docs/tailscale-backend.md); the
    /// `backend` key of app 0.127–0.130 is read and ignored.
    pub enabled: bool,
}

/// `[tailscale] backend` chose the daemon in app 0.127–0.130. The player runs
/// the daemon the image ships. A file that still has the key parses as before
/// without it; the key is logged once per process, not as an error, and the
/// next save drops it.
static LEGACY_BACKEND_LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn has_legacy_tailscale_backend(user: &toml::Table) -> bool {
    user.get("tailscale").and_then(toml::Value::as_table).is_some_and(|t| t.contains_key("backend"))
}

/// Bluetooth headphones (docs/bluetooth.md). Paired devices and their keys
/// are kept outside this file, in `/state/bluetooth/devices`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Bluetooth {
    pub enabled: bool,
    /// The controller's UART, or `fake` for the built-in test controller.
    pub device: String,
    /// UART rate after the controller's patch starts (`uart_mode = "manual"`). The Q2's 24 MHz UART
    /// clock gives 1500000 exactly; 3000000 needs kernel patches 0191 and 0192.
    pub baud: u32,
    /// RTS/CTS flow control; needs the UART0 flow-control pins muxed by the kernel.
    pub flow_control: bool,
    /// A directory with `rtl8733bs_fw` and `rtl8733bs_config`, searched before the installed ones.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub firmware_dir: Option<String>,
    /// `auto` (the best codec the headphones offer), `ldac`, `aptx-hd`, `aptx`, `aac` or `sbc`.
    pub codec: String,
    /// LDAC bit rate: `high` (990 kbit/s), `standard` (660) or `mobile` (330).
    /// With `ldac_adaptive` this is the highest rate used.
    pub ldac_quality: String,
    /// LDAC lowers its bit rate (down to 330 kbit/s) while the link falls behind
    /// and returns to `ldac_quality` when it clears, as Android's adaptive mode does.
    pub ldac_adaptive: bool,
    /// `auto`: 3 Mbaud with RTS/CTS where the kernel takes the rate (device-verified
    /// 2026-11-22); a bring-up that fails is tried at 1.5 Mbaud with RTS/CTS, then
    /// without (`baud` and `flow_control` are not used). `manual`: `baud` and
    /// `flow_control`; a bring-up that fails is tried once more at 1.5 Mbaud without
    /// flow control.
    pub uart_mode: String,
    /// After the Wi-Fi chip powers up again (a radio doze wake), run the controller's
    /// start-up commands again when no headphones are connected
    /// (docs/bugs/wifi-lps-after-power-cycle.md). Written only when turned off.
    #[serde(skip_serializing_if = "is_true")]
    pub resync_after_wifi: bool,
}

fn is_true(value: &bool) -> bool { *value }

impl Default for Bluetooth {
    fn default() -> Self { Self { enabled: false, device: "/dev/ttyS0".into(), baud: 1_500_000, flow_control: false, firmware_dir: None, codec: "auto".into(), ldac_quality: "standard".into(), ldac_adaptive: true, uart_mode: "auto".into(), resync_after_wifi: true } }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Server {
    /// e.g. `https://music.example.com` or `http://192.168.1.10:4533`.
    pub url: String,
    pub user: String,
    /// Stored as written; sent to the server only as a salted token.
    pub password: String,
    /// An OpenSubsonic API key, used instead of user and password when set.
    pub api_key: String,
}

impl Server {
    pub fn is_configured(&self) -> bool {
        !self.url.trim().is_empty() && (!self.api_key.is_empty() || !self.user.is_empty())
    }

    pub fn credentials(&self) -> subsonic::Credentials {
        let auth = if self.api_key.is_empty() {
            subsonic::Auth::Password(self.password.clone())
        } else {
            subsonic::Auth::ApiKey(self.api_key.clone())
        };
        // Match Client's canonical base URL before comparing connection settings.
        // Otherwise every unrelated setting change replaces a trailing-slash
        // server's client and briefly resets its online indicator.
        subsonic::Credentials { server: self.url.trim().trim_end_matches('/').to_string(), user: self.user.clone(), auth }
    }
}

pub(crate) mod options;
pub use options::{HeadphonePower, Quality, ReplayGainMode};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Playback {
    pub eq: crate::audio::eq::Settings,
    pub stream_quality: Quality,
    pub download_quality: Quality,
    pub gapless: bool,
    pub replay_gain: ReplayGainMode,
    /// Report library song plays to Navidrome.
    pub scrobble: bool,
    /// Sync the play queue with the server, to resume on other clients.
    pub sync_queue: bool,
    /// DAC volume, 0-100.
    pub volume: u8,
    /// Upper bound for the volume, protecting ears and IEMs.
    pub max_volume: u8,
    /// The Headphone Power setting; read it with
    /// [`Playback::headphone_power`]. Absent in files from before V0.133,
    /// where `high_gain` carries the choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headphone_power: Option<HeadphonePower>,
    /// The earlier Headphone Gain (false Low, now Standard; true High).
    /// Kept in step with `headphone_power` so an older version reading
    /// this file finds the nearest level.
    pub high_gain: bool,
    /// 0 fast, 1 slow, 2 short-delay fast, 3 short-delay slow.
    pub dac_filter: u8,
    pub pause_on_unplug: bool,
    /// Automatic spoken sleep timer; zero disables it.
    pub auto_sleep_minutes: u16,
    /// Local clock minutes after midnight, start inclusive and end exclusive.
    pub auto_sleep_start: u16,
    pub auto_sleep_end: u16,
}

impl Default for Playback {
    fn default() -> Self {
        Playback {
            eq: Default::default(),
            // MP3 320 saves about 6 mW against streaming a 918 kbit/s FLAC
            // (docs/validation/stream-format-power.md); originals at or
            // under 320 kbit/s still stream untouched (library::variant).
            stream_quality: options::DEFAULT_STREAM_QUALITY,
            download_quality: options::DEFAULT_DOWNLOAD_QUALITY,
            gapless: true,
            replay_gain: options::DEFAULT_REPLAY_GAIN,
            scrobble: true,
            sync_queue: true,
            volume: 50,
            max_volume: 100,
            headphone_power: None,
            high_gain: false,
            dac_filter: options::DEFAULT_DAC_FILTER,
            pause_on_unplug: true,
            auto_sleep_minutes: options::DEFAULT_AUTO_SLEEP_DURATION,
            auto_sleep_start: options::DEFAULT_AUTO_SLEEP_START,
            auto_sleep_end: options::DEFAULT_AUTO_SLEEP_END,
        }
    }
}

impl Playback {
    /// The chosen Headphone Power. A file from before the setting keeps an
    /// explicit High gain; the old Low default becomes the new default,
    /// Efficient. The default can change because the listener can set it
    /// back in Settings.
    pub fn headphone_power(&self) -> HeadphonePower {
        self.headphone_power.unwrap_or(if self.high_gain { HeadphonePower::High } else { HeadphonePower::default() })
    }

    pub fn set_headphone_power(&mut self, level: HeadphonePower) {
        self.headphone_power = Some(level);
        self.high_gain = matches!(level, HeadphonePower::High | HeadphonePower::Maximum);
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Storage {
    pub fill_my_drive: bool,
    pub fill_target_percent: u8,
    /// Cap on songs kept from streaming (downloads don't count), in MB.
    pub stream_cache_mb: u64,
    /// Keep this much of the card free, in MB.
    pub reserve_mb: u64,
    /// Fetch the next songs in the queue ahead of time.
    pub prefetch_songs: u32,
    /// Fetch ahead in bursts of this many songs, once the next song is not
    /// on the card, instead of one at a time; 0 keeps one at a time. Four:
    /// quick-batch-01c measured 2.4 J per MP3 320 song in a burst of four
    /// against 5.9 J alone, about 17 mW over a 210 s song
    /// (docs/validation/quick-experiments.md).
    pub fetch_batch: u32,
    /// Fetch ahead by duration instead (0: by `fetch_batch`): once less than
    /// `fetch_low_minutes` of cached songs follow the playing one, fetch the
    /// next songs until this many minutes are cached. With `power.radio_doze`
    /// the radio dozes as soon as that fetch ends and wakes to fetch more.
    pub fetch_minutes: u32,
    pub fetch_low_minutes: u32,
}

impl Default for Storage {
    fn default() -> Self {
        Storage { fill_my_drive: false, fill_target_percent: 50, stream_cache_mb: options::DEFAULT_CACHE_LIMIT, reserve_mb: 512, prefetch_songs: options::DEFAULT_FETCH_AHEAD, fetch_batch: 4, fetch_minutes: 10, fetch_low_minutes: 3 }
    }
}

mod appearance;
pub use appearance::{Accent, Look, RatingControls};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Display {
    /// Gradual warm tint, 21:00–07:00 in the configured local time zone.
    pub night_warmth: bool,
    pub rating_controls: RatingControls,
    /// 1-100, as stock's brightness.
    pub brightness: u8,
    /// Seconds without input before the screen turns off; 0 never.
    pub backlight_seconds: u32,
    pub look: Look,
    pub accent: Accent,
    /// The time in the title bar instead of the page's name (the classic's
    /// Date & Time > Time in Title).
    pub clock_in_title: bool,
    /// 24-hour clock.
    pub clock_24h: bool,
    /// The classic's Clicker: a click from the Q2's buzzer as the wheel
    /// moves and the centre is pressed.
    pub clicker: bool,
    /// Allow taps on the lit hardware display.
    pub touchscreen: bool,
    /// POSIX TZ string or IANA name, e.g. `Europe/London`.
    pub timezone: String,
}

impl Default for Display {
    fn default() -> Self {
        Display {
            night_warmth: false,
            rating_controls: RatingControls::Stars,
            brightness: 50,
            backlight_seconds: options::DEFAULT_BACKLIGHT_TIMEOUT,
            look: Look::Classic,
            accent: Accent::Blue,
            clock_in_title: false,
            clicker: true,
            touchscreen: false,
            clock_24h: false,
            timezone: String::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Power {
    /// Minutes idle and not playing before sleeping; 0 never.
    pub sleep_minutes: u32,
    /// Turn Wi-Fi off while enough cached songs are queued and the screen is
    /// off, and back on before they run out (ui/doze.rs). On by default: with
    /// fetch by duration (`storage.fetch_minutes`) it dozes as soon as a fetch
    /// ends.
    pub radio_doze: bool,
    /// Minutes of complete cached audio ahead before the radio may doze, and
    /// the margin left when it wakes (ui/doze.rs; the wake margin must be
    /// positive and below the threshold, else both defaults apply).
    pub doze_ahead_minutes: u32,
    pub doze_wake_minutes: u32,
    /// Also doze while paused, once the screen has been off and untouched
    /// for this many minutes, when at least the wake margin of the queue is
    /// on the card (ui/doze.rs). Lighting the screen wakes the radio. 0 keeps
    /// the radio up while paused. Default 1 (the delay's reasons are at
    /// `ui::doze::PAUSED_DELAY_MINUTES`).
    pub doze_paused_minutes: u32,
}

impl Default for Power {
    fn default() -> Self {
        Power { sleep_minutes: options::DEFAULT_IDLE_TIMEOUT, radio_doze: true, doze_ahead_minutes: 30, doze_wake_minutes: 5,
            doze_paused_minutes: crate::ui::doze::PAUSED_DELAY_MINUTES }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Wifi {
    pub enabled: bool,
    /// Networks to join, most preferred first.
    pub networks: Vec<WifiNetwork>,
}

impl Default for Wifi {
    fn default() -> Self {
        Wifi { enabled: true, networks: Vec::new() }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WifiNetwork {
    pub ssid: String,
    /// Empty for an open network.
    pub psk: String,
    /// Existing files retain WPA2. Select WPA3 explicitly or from an SAE scan.
    pub security: WifiSecurity,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WifiSecurity {
    #[default]
    Wpa2,
    Wpa2Wpa3,
    Wpa3,
}

const HEADER: &str = "\
# Q2 Navidrome settings. Edit on a computer or on the Q2 (Settings).
# [server] url is the address you open Navidrome at, e.g.
#   url = \"https://music.example.com\"  or  \"http://192.168.1.10:4533\"
";

impl Config {
    /// All content clients share the global optional Tailscale HTTP transport.
    /// None uses system resolution/routing; an explicit proxy never falls back.
    pub fn http_proxy(&self) -> Option<&'static str> {
        self.tailscale.enabled.then_some(crate::tailscale::PROXY)
    }
    pub fn load(_blocking: &crate::rt::Blocking, path: &Path) -> Config {
        match std::fs::read_to_string(path) {
            Ok(text) => Config::parse(&text, &path.display().to_string()),
            Err(_) => Config::default(),
        }
    }

    /// Parses settings, keeping every one that reads: a value this version
    /// doesn't know (from an older or newer one, or a typo) falls back to
    /// its default alone, not the whole file.
    pub fn parse(text: &str, name: &str) -> Config {
        Self::parse_unchecked(text, name).validated()
    }

    /// Brings numbers a hand-edited or older file can carry into the ranges
    /// the app assumes: the Volume Limit within 1..=100, the volume within
    /// the limit (a saved volume must never exceed the owner's limit at
    /// boot), and the brightness within 1..=100. Doze thresholds are
    /// validated where they are used (`q2_machines::doze::Thresholds`).
    pub fn validated(mut self) -> Config {
        if self.telemetry.validate().is_err() {
            log::warn!("invalid telemetry settings; optional reporting disabled");
            self.telemetry = Telemetry::default();
        }
        self.playback.max_volume = self.playback.max_volume.clamp(1, 100);
        self.playback.volume = self.playback.volume.min(self.playback.max_volume);
        self.display.brightness = self.display.brightness.clamp(1, 100);
        self
    }

    /// Strict parse. Every production Config parse goes through a TOML table
    /// and its `Value` deserializer, so the Config tree's `Deserialize` code
    /// is compiled once rather than once per deserializer (about 0.2 MB).
    pub(crate) fn from_toml(text: &str) -> Result<Config, toml::de::Error> {
        Self::from_table(text.parse()?)
    }

    fn from_table(mut table: toml::Table) -> Result<Config, toml::de::Error> {
        migrate_scrobble_settings(&mut table);
        toml::Value::Table(table).try_into()
    }

    fn parse_unchecked(text: &str, name: &str) -> Config {
        let mut user: toml::Table = match text.parse() {
            Ok(t) => t,
            Err(_) => {
                // TOML diagnostics include source lines and values (passwords).
                log::error!("{name}: invalid settings syntax; using defaults");
                return Config::default();
            }
        };
        if has_legacy_tailscale_backend(&user) && !LEGACY_BACKEND_LOGGED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            log::info!("{name}: ignoring tailscale.backend; the image's Tailscale daemon runs");
        }
        if let Ok(c) = Self::from_table(user.clone()) {
            return c;
        }
        migrate_scrobble_settings(&mut user);
        let mut merged = toml::Table::try_from(Config::default()).unwrap_or_default();
        // Default-off service sections are omitted on save but still support
        // field-by-field recovery when an unrelated setting is invalid.
        for section in ["listenbrainz", "lastfm"] {
            merged.insert(section.to_owned(), toml::Value::Table(toml::Table::new()));
        }
        for (section, value) in user {
            // Routing/classification is one atomic setting. Salvaging enabled
            // while dropping an invalid stream could misclassify test reports.
            if section == "telemetry" {
                if let Ok(settings) = value.try_into::<Telemetry>() {
                    if settings.validate().is_ok() {
                        merged.insert(section, toml::Value::try_from(settings).unwrap());
                    }
                }
                continue;
            }
            let (Some(toml::Value::Table(base)), toml::Value::Table(fields)) = (merged.get(&section).cloned(), value) else {
                log::warn!("{name}: ignoring an unsupported settings section");
                continue;
            };
            let mut base = base;
            for (key, v) in fields {
                let mut trial = merged.clone();
                let mut t = base.clone();
                t.insert(key.clone(), v.clone());
                trial.insert(section.clone(), toml::Value::Table(t.clone()));
                if toml::Value::Table(trial).try_into::<Config>().is_ok() {
                    base = t;
                } else {
                    log::warn!("{name}: ignoring an invalid setting");
                }
            }
            merged.insert(section, toml::Value::Table(base));
        }
        toml::Value::Table(merged).try_into().unwrap_or_default()
    }

    /// Writes the file through a temporary file and a rename, so a power
    /// loss leaves the old or the new settings, never half of them.
    /// Tables the typed settings do not model stay as the replaced file had them.
    pub fn save(&self, blocking: &crate::rt::Blocking, path: &Path) -> std::io::Result<()> {
        write_atomic(blocking, path, &self.serialized_over(read_file(blocking, path).ok().as_deref())?)
    }

    /// The typed settings, then the unmodeled keys of `previous`.
    fn serialized_over(&self, previous: Option<&[u8]>) -> std::io::Result<Vec<u8>> {
        Ok(keep::with_unmodeled(self.serialized()?, previous))
    }

    fn serialized(&self) -> std::io::Result<Vec<u8>> {
        let text = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        Ok(format!("{HEADER}\n{text}").into_bytes())
    }

    /// The startup fallback is already durable when its private regular file
    /// contains the exact normalized bytes. Repair every other case atomically.
    fn refresh_snapshot(&self, blocking: &crate::rt::Blocking, path: &Path) -> std::io::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let bytes = self.serialized()?;
        let private_regular = std::fs::symlink_metadata(path).is_ok_and(|m|
            m.file_type().is_file() && m.permissions().mode() & 0o7777 == 0o600);
        if private_regular && std::fs::read(path).is_ok_and(|old| old == bytes) {
            return Ok(());
        }
        write_atomic(blocking, path, &bytes)
    }
}

// Legacy direct service settings were gated by the playback scrobble switch.
// Carry their effective opt-in forward when an explicit modern setting is absent.
fn migrate_scrobble_settings(table: &mut toml::Table) {
    let legacy_master = table.get("playback")
        .and_then(toml::Value::as_table)
        .and_then(|fields| fields.get("scrobble"))
        .and_then(toml::Value::as_bool)
        .unwrap_or_else(|| Playback::default().scrobble);
    if let Some(fields) = table.get_mut("listenbrainz").and_then(toml::Value::as_table_mut) {
        if let Some(mode) = fields.remove("mode") {
            fields.entry("enabled").or_insert_with(|| match mode.as_str() {
                Some("off") => toml::Value::Boolean(false),
                Some("radio" | "music-and-radio") => toml::Value::Boolean(legacy_master),
                _ => toml::Value::String("invalid legacy mode".into()),
            });
        }
    }
    if let Some(fields) = table.get_mut("lastfm").and_then(toml::Value::as_table_mut) {
        if let Some(radio) = fields.remove("radio") {
            fields.entry("enabled").or_insert_with(|| match radio {
                toml::Value::Boolean(enabled) => toml::Value::Boolean(legacy_master && enabled),
                invalid => invalid,
            });
        }
    }
}

/// Startup-path microbenchmark using only a dedicated generated fixture tree.
/// It does not measure kernel boot, first frame, storage energy or owner data.
pub fn startup_benchmark(blocking: &crate::rt::Blocking, root: &Path, iterations: usize, scene: &str) -> Result<serde_json::Value, String> {
    use std::os::unix::fs::MetadataExt;
    if !root.components().any(|c| c.as_os_str() == "build") || iterations == 0 ||
        !matches!(scene, "warm" | "cold" | "changed") {
        return Err("config-startup needs build/ scratch, positive iterations and warm/cold/changed scene".into());
    }
    let marker = root.join(".q2-generated-config-benchmark");
    if root.exists() && !marker.is_file() {
        return Err("existing scratch is not a generated config benchmark".into());
    }
    std::fs::create_dir_all(root).map_err(|e| e.to_string())?;
    std::fs::write(marker, b"generated fixture only\n").map_err(|e| e.to_string())?;
    let paths = Paths::installed(root.join("internal"), root.join("card/Q2Mainline"),
        root.join("run/media"), true, None);
    paths.create_all(blocking).map_err(|e| e.to_string())?;
    let mut generated = Config::default();
    generated.server.password = "generated-benchmark-only".into();
    generated.save(blocking, &paths.config()).map_err(|e| e.to_string())?;
    let snapshot = paths.internal.as_ref().unwrap();
    generated.save(blocking, snapshot).map_err(|e| e.to_string())?;
    let mut samples = Vec::new();
    let mut replacements = 0;
    for i in 0..iterations {
        if scene == "cold" { std::fs::remove_file(snapshot).map_err(|e| e.to_string())?; }
        if scene == "changed" {
            generated.playback.volume = (20 + i % 30) as u8;
            generated.save(blocking, &paths.config()).map_err(|e| e.to_string())?;
        }
        let before = std::fs::metadata(snapshot).ok().map(|m|m.ino());
        let start = std::time::Instant::now();
        let loaded = paths.load_config(blocking);
        samples.push(start.elapsed().as_nanos() as u64);
        if loaded != generated { return Err("generated settings changed".into()); }
        let after = std::fs::metadata(snapshot).map_err(|e| e.to_string())?.ino();
        replacements += usize::from(before != Some(after));
    }
    Ok(serde_json::json!({"scope":"generated installed settings startup path only", "scene":scene,
        "load_ns":samples, "snapshot_replacements":replacements, "iterations":iterations,
        "full_boot_measurement":false, "energy_measurement":false}))
}

pub fn write_atomic(_blocking: &crate::rt::Blocking, path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true)
            .mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&tmp)?;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    std::fs::File::open(path.parent().unwrap_or(Path::new(".")))?.sync_all()
}

/// Where everything lives.
#[derive(Debug, Clone)]
pub struct Paths {
    /// The data folder.
    pub root: PathBuf,
    media: PathBuf,
    database_path: PathBuf,
    legacy_media: Option<PathBuf>,
    config_path: PathBuf,
    log_path: PathBuf,
    /// Installed only: the NAND copy of the settings.
    internal: Option<PathBuf>,
    writable_config: bool,
    pub ephemeral_media: bool,
    config_revision: std::sync::Arc<std::sync::Mutex<Option<[u8; 16]>>>,
    save_order: save::Order,
    /// Which media stores finished moving to the sharded layout.
    layout: std::sync::Arc<crate::shard::Layout>,
}

impl Paths {
    pub fn new(root: PathBuf) -> Paths {
        Paths { media: root.clone(), database_path: root.join("library.db"),
            legacy_media: None, config_path: root.join("config.toml"),
            log_path: root.join("q2nd.log"), root, internal: None,
            writable_config: true, ephemeral_media: false,
            config_revision: Default::default(), save_order: Default::default(),
            layout: Default::default() }
    }

    pub(crate) fn layout(&self) -> &crate::shard::Layout { &self.layout }

    /// Used only by the explicit installed mainline service; simulator
    /// paths never acquire access to the installed configuration implicitly.
    pub fn mainline(root: PathBuf) -> Paths {
        if std::env::var("Q2ND_INSTALLED_STORAGE").as_deref() != Ok("1") {
            return Self::new(root);
        }
        let ready = std::env::var("Q2ND_SD_READY").as_deref() == Ok("1");
        let legacy = (ready && std::env::var("Q2ND_LEGACY_MEDIA").as_deref() == Ok("1"))
            .then(|| PathBuf::from("/mnt/mmc/Q2Navidrome"));
        Self::installed(root, PathBuf::from("/mnt/mmc/Q2Mainline"), PathBuf::from("/run/q2-media"), ready, legacy)
    }

    pub(crate) fn installed(root: PathBuf, card: PathBuf, temporary: PathBuf,
                           ready: bool, legacy: Option<PathBuf>) -> Paths {
        let mut paths = Self::new(root);
        paths.config_path = card.join("config.toml");
        paths.internal = Some(paths.root.join("config.last-valid.toml"));
        paths.writable_config = ready;
        paths.ephemeral_media = !ready;
        paths.log_path = temporary.parent().unwrap_or(Path::new("/run")).join("log/q2nd.log");
        paths.media = if ready { card.join("media") } else { temporary };
        paths.database_path = if ready { card.join("library.db") } else { paths.media.join("library.db") };
        // Only the service's explicit nonsecret marker enables old media reads.
        // The original database/configuration are never opened or imported here.
        paths.legacy_media = if ready { legacy } else { None };
        paths
    }

    pub fn media_root(&self) -> &Path { &self.media }
    pub fn legacy_art_cache(&self) -> Option<PathBuf> {
        self.legacy_media.as_ref().map(|p| p.join("cache/art"))
    }
    /// Imported database paths remain on the original card tree. Reading them
    /// is opt-in; all new writes and removals belong to the new media tree.
    pub fn media_path(&self, path: &Path) -> Option<PathBuf> {
        if self.internal.is_none() { return Some(path.to_path_buf()); }
        if path.components().any(|c| matches!(c, std::path::Component::ParentDir)) { return None; }
        (path.starts_with(&self.media) || self.legacy_media.as_ref()
            .is_some_and(|old| path.starts_with(old))).then(|| path.to_path_buf())
    }
    pub fn can_modify_media(&self, path: &Path) -> bool {
        self.internal.is_none() || self.media_path(path).is_some() && path.starts_with(&self.media)
    }
    /// Installed only: the record of the card file as last read or written,
    /// which tells a host edit (or another card) from saves made without it.
    fn card_record(&self) -> Option<PathBuf> {
        self.internal.as_ref().map(|internal| internal.with_file_name("config.card-sync"))
    }

    /// Installed settings live on NAND and on the card. The NAND copy is
    /// always current.
    pub fn load_config(&self, blocking: &crate::rt::Blocking) -> Config {
        let Some(internal) = &self.internal else {
            let config = Config::load(blocking, &self.config_path);
            if !self.config_path.exists() { let _ = config.save(blocking, &self.config_path); }
            log_saved_accounts(&config);
            return config;
        };
        let stored = read_file(blocking, internal).ok()
            .and_then(|bytes| String::from_utf8(bytes).ok())
            .and_then(|text| Config::from_toml(&text).ok());
        let (config, revision) = if self.writable_config {
            self.load_with_card(blocking, stored)
        } else {
            log::info!("No card; using the internal settings");
            (stored.unwrap_or_default(), None)
        };
        crate::record_profile_stage("profile-config-snapshot-begin");
        if config.refresh_snapshot(blocking, internal).is_err() { log::warn!("Could not save the internal settings"); }
        crate::record_profile_stage("profile-config-snapshot-returned");
        *self.config_revision.lock().unwrap() = revision;
        log_saved_accounts(&config);
        config
    }

    /// The settings to use and, when the card holds them, the card file's
    /// fingerprint. An invalid or unreadable card file is never replaced and
    /// never replaces the NAND copy.
    fn load_with_card(&self, blocking: &crate::rt::Blocking, stored: Option<Config>) -> (Config, Option<[u8; 16]>) {
        let record = self.card_record().expect("installed paths");
        crate::record_profile_stage("profile-config-read-begin");
        let card = read_file(blocking, &self.config_path);
        crate::record_profile_stage("profile-config-read-returned");
        match card {
            Ok(bytes) => {
                let fingerprint = config_fingerprint(&bytes);
                let parsed = std::str::from_utf8(&bytes).ok().and_then(|text| Config::from_toml(text).ok());
                crate::record_profile_stage("profile-config-parse-returned");
                let Some(card) = parsed else {
                    log::warn!("Card settings are invalid; keeping the card file and using the internal settings");
                    return (stored.unwrap_or_default(), None);
                };
                let unchanged = read_file(blocking, &record).is_ok_and(|seen| seen == fingerprint_text(&fingerprint).as_bytes());
                // The internal copy never holds `[portal]`; the card's is current.
                let stored = stored.map(|mut current| { current.portal = card.portal.clone(); current });
                match stored {
                    // The card is as this player left it: settings saved
                    // without the card catch it up.
                    Some(current) if unchanged && current != card => {
                        let revision = self.write_card(blocking, &record, &current);
                        if revision.is_none() { log::warn!("Could not update the card settings"); }
                        (current, revision)
                    }
                    // Edited on the host, another card, or the first start
                    // with a record: the card wins.
                    _ => {
                        if !unchanged {
                            log::info!("Card settings changed; using them");
                            if write_atomic(blocking, &record, fingerprint_text(&fingerprint).as_bytes()).is_err() {
                                log::warn!("Could not record the card settings");
                            }
                        }
                        (card, Some(fingerprint))
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // A card without settings, new or cleared on the host, is
                // seeded from NAND. This reverses the 2026-11-02 rule.
                let config = stored.unwrap_or_default();
                let revision = self.write_card(blocking, &record, &config);
                if revision.is_some() { log::info!("Card settings written from the internal settings"); }
                else { log::warn!("Could not write the card settings"); }
                (config, revision)
            }
            Err(_) => {
                log::warn!("Card settings unavailable; using the internal settings");
                (stored.unwrap_or_default(), None)
            }
        }
    }

    /// Writes the settings to the card, then the record of what was written.
    fn write_card(&self, blocking: &crate::rt::Blocking, record: &Path, config: &Config) -> Option<[u8; 16]> {
        let previous = read_file(blocking, &self.config_path).ok();
        let bytes = config.serialized_over(previous.as_deref()).ok()?;
        write_atomic(blocking, &self.config_path, &bytes).ok()?;
        let fingerprint = config_fingerprint(&bytes);
        if write_atomic(blocking, record, fingerprint_text(&fingerprint).as_bytes()).is_err() {
            log::warn!("Could not record the card settings");
        }
        Some(fingerprint)
    }

    /// NAND first, and it must succeed. Then the card, when this run holds
    /// the card's settings and the host has not changed them since.
    fn save_config_serial(&self, blocking: &crate::rt::Blocking, config: &Config) -> std::io::Result<()> {
        let Some(internal) = &self.internal else { return config.save(blocking, &self.config_path); };
        let mut revision = self.config_revision.lock().unwrap();
        let card = match *revision {
            Some(expected) => match read_file(blocking, &self.config_path) {
                Ok(current) if config_fingerprint(&current) != expected => {
                    return Err(std::io::Error::new(std::io::ErrorKind::WouldBlock,
                        "Settings changed externally; restart to reload them"));
                }
                Ok(_) => true,
                Err(_) => { log::warn!("Card settings unavailable; saving the internal settings only"); false }
            },
            None => false,
        };
        config.save(blocking, internal)?;
        if card {
            match self.write_card(blocking, &self.card_record().expect("installed paths"), config) {
                Some(fingerprint) => *revision = Some(fingerprint),
                None => log::warn!("Could not update the card settings; the internal settings are saved"),
            }
        }
        Ok(())
    }

    pub fn config(&self) -> PathBuf {
        self.config_path.clone()
    }
    pub fn database(&self) -> PathBuf {
        self.database_path.clone()
    }
    /// Songs kept from streaming; evicted oldest-first past the cap.
    pub fn stream_cache(&self) -> PathBuf {
        self.media.join("cache").join("songs")
    }
    /// Songs being fetched; a finished fetch is renamed into the cache.
    pub fn stream_partials(&self) -> PathBuf { self.stream_cache().join("partial") }
    pub fn art_cache(&self) -> PathBuf {
        self.media.join("cache").join("art")
    }
    pub fn offline_art(&self) -> PathBuf { self.media.join("offline-art") }
    /// Downloaded audiobook and podcast files.
    pub fn spoken_downloads(&self) -> PathBuf { self.media.join("spoken-downloads") }
    /// Downloaded songs, never evicted.
    pub fn offline(&self) -> PathBuf {
        self.media.join("offline")
    }
    pub fn log(&self) -> PathBuf {
        self.log_path.clone()
    }

    pub fn create_all(&self, _blocking: &crate::rt::Blocking) -> std::io::Result<()> {
        for d in [self.root.clone(), self.stream_partials(), self.art_cache(), self.offline()] {
            std::fs::create_dir_all(d)?;
        }
        if !self.ephemeral_media {std::fs::create_dir_all(self.media_root().join("audiobooks"))?;}
        if self.writable_config {
            if let Some(parent) = self.config_path.parent() { std::fs::create_dir_all(parent)?; }
        }
        if let Some(parent) = self.log_path.parent() { std::fs::create_dir_all(parent)?; }
        if let Some(parent) = self.database_path.parent() { std::fs::create_dir_all(parent)?; }
        Ok(())
    }
}

fn config_fingerprint(bytes: &[u8]) -> [u8; 16] {
    use md5::{Digest, Md5};
    Md5::digest(bytes).into()
}

fn fingerprint_text(fingerprint: &[u8; 16]) -> String {
    fingerprint.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Presence only: whether Wi-Fi sign-in can use an account from the card.
fn log_saved_accounts(config: &Config) {
    let delta = match &config.portal.delta {
        Some(account) if account.sign_in().is_some() => "saved",
        Some(_) => "incomplete",
        None => "absent",
    };
    log::info!("Wi-Fi sign-in: Delta account {delta}");
}

fn read_file(_blocking: &crate::rt::Blocking, path: &Path) -> std::io::Result<Vec<u8>> {
    std::fs::read(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_scrobbling_defaults_off_independently_of_navidrome() {
        let settings = Config::from_toml("").unwrap();
        assert!(settings.playback.scrobble);
        assert!(!settings.listenbrainz.enabled);
        assert!(!settings.lastfm.enabled);
        let settings = Config::from_toml(
            "[listenbrainz]\ntoken = \"generated-token\"\n[lastfm]\nsession = \"generated-session\"\n",
        ).unwrap();
        assert!(!settings.listenbrainz.enabled);
        assert!(!settings.lastfm.enabled);
    }

    #[test]
    fn scrobble_migration_preserves_effective_opt_ins_and_saves_only_enabled_keys() {
        for master in [false, true] {
            for mode in ["off", "radio", "music-and-radio"] {
                for radio in [false, true] {
                    let input = format!(r#"
[playback]
scrobble = {master}
[listenbrainz]
mode = "{mode}"
token = "generated-token"
[lastfm]
radio = {radio}
session = "generated-session"
"#);
                    let settings = Config::from_toml(&input).unwrap();
                    let expected_lb = master && mode != "off";
                    let expected_lfm = master && radio;
                    assert_eq!(settings.playback.scrobble, master);
                    assert_eq!(settings.listenbrainz.enabled, expected_lb);
                    assert_eq!(settings.lastfm.enabled, expected_lfm);
                    assert_eq!(settings.listenbrainz.token, "generated-token");
                    assert_eq!(settings.lastfm.session, "generated-session");

                    let saved = toml::to_string(&settings).unwrap();
                    let table = saved.parse::<toml::Table>().unwrap();
                    assert_eq!(table["listenbrainz"]["enabled"].as_bool(), Some(expected_lb));
                    assert_eq!(table["lastfm"]["enabled"].as_bool(), Some(expected_lfm));
                    assert!(table["listenbrainz"].get("mode").is_none());
                    assert!(table["lastfm"].get("radio").is_none());
                    assert_eq!(Config::from_toml(&saved).unwrap(), settings);
                }
            }
        }
    }

    #[test]
    fn explicit_scrobble_preferences_win_and_migration_survives_field_recovery() {
        let legacy = r#"
[listenbrainz]
mode = "radio"
token = "generated-token"
[lastfm]
radio = true
session = "generated-session"
"#;
        for master in [false, true] {
            let playback = format!("[playback]\nscrobble = {master}\n");
            let recovery = format!("{playback}volume = \"invalid\"\n");
            let modern = legacy
                .replace("[listenbrainz]", &format!("[listenbrainz]\nenabled = {}", !master))
                .replace("[lastfm]", &format!("[lastfm]\nenabled = {}", !master));
            for (services, expected) in [(legacy, master), (&modern, !master)] {
                let strict = Config::from_toml(&format!("{playback}{services}")).unwrap();
                assert_eq!(strict.listenbrainz.enabled, expected);
                assert_eq!(strict.lastfm.enabled, expected);
                assert_eq!(strict.listenbrainz.token, "generated-token");
                assert_eq!(strict.lastfm.session, "generated-session");
                assert_eq!(Config::parse(&format!("{recovery}{services}"), "generated"), strict);
            }
        }
        let defaults = Config::from_toml(legacy).unwrap();
        assert!(defaults.listenbrainz.enabled && defaults.lastfm.enabled);

        let malformed = legacy.replace("radio = true", "radio = 1").replace(r#""radio""#, "1");
        let recovered = Config::parse(&malformed, "generated");
        assert!(!recovered.listenbrainz.enabled && !recovered.lastfm.enabled);
        assert_eq!(recovered.listenbrainz.token, "generated-token");
        assert_eq!(recovered.lastfm.session, "generated-session");
    }

    #[test]
    fn out_of_range_numbers_are_clamped_when_settings_load() {
        let c = Config::parse("[playback]\nvolume = 250\nmax_volume = 60\n[display]\nbrightness = 0\n", "generated");
        assert_eq!((c.playback.volume, c.playback.max_volume, c.display.brightness), (60, 60, 1));
        let c = Config::parse("[playback]\nvolume = 100\nmax_volume = 255\n[display]\nbrightness = 255\n", "generated");
        assert_eq!((c.playback.volume, c.playback.max_volume, c.display.brightness), (100, 100, 100));
        let c = Config::parse("[playback]\nvolume = 40\nmax_volume = 0\n", "generated");
        assert_eq!((c.playback.volume, c.playback.max_volume), (1, 1));
        // A number outside its type is one invalid setting, not a lost file.
        let c = Config::parse("[playback]\nvolume = 300\nmax_volume = 70\n", "generated");
        assert_eq!(c.playback.max_volume, 70);
        assert!(c.playback.volume <= 70);
        assert_eq!(Config::default().validated(), Config::default(), "the defaults are already valid");
        // Thresholds built from any configured minutes keep their hysteresis.
        for (doze, wake) in [(0, 0), (1, 1), (5, 9), (u32::MAX, 0), (u32::MAX, u32::MAX), (7, 3)] {
            assert!(q2_machines::doze::Thresholds::from_minutes(doze, wake).valid(), "{doze}/{wake}");
            assert!(q2_machines::doze::Thresholds::after_fetch(wake).valid(), "{wake}");
        }
    }

    #[test]
    fn headphone_power_defaults_to_efficient_and_keeps_an_earlier_high_gain() {
        use HeadphonePower::*;
        assert_eq!(Config::default().playback.headphone_power(), Efficient);
        assert_eq!(Config::parse("", "generated").playback.headphone_power(), Efficient);
        // Files from before the setting: High stays High; the old Low default
        // takes the new default.
        assert_eq!(Config::parse("[playback]\nhigh_gain = true\n", "generated").playback.headphone_power(), High);
        assert_eq!(Config::parse("[playback]\nhigh_gain = false\n", "generated").playback.headphone_power(), Efficient);
        for (text, level) in [("efficient", Efficient), ("standard", Standard), ("high", High), ("maximum", Maximum)] {
            let c = Config::parse(&format!("[playback]\nheadphone_power = \"{text}\"\nhigh_gain = true\nvolume = 30\n"), "generated");
            assert_eq!(c.playback.headphone_power(), level, "{text}: the new key wins over high_gain");
            assert_eq!(c.playback.volume, 30);
        }
        // An unknown level is one invalid setting: the rest of the section reads.
        let c = Config::parse("[playback]\nheadphone_power = \"loud\"\nhigh_gain = true\nvolume = 30\n", "generated");
        assert_eq!((c.playback.headphone_power(), c.playback.volume), (High, 30));
        // Written as a lowercase name, with high_gain in step for older versions.
        for level in HeadphonePower::ALL {
            let mut c = Config::default();
            c.playback.set_headphone_power(level);
            let text = toml::to_string_pretty(&c).unwrap();
            assert!(text.contains(&format!("headphone_power = \"{}\"", level.label().to_lowercase())), "{text}");
            assert_eq!(c.playback.high_gain, matches!(level, High | Maximum));
            assert_eq!(Config::parse(&text, "generated").playback.headphone_power(), level);
        }
        // Never chosen: the key stays out of the file.
        assert!(!toml::to_string_pretty(&Config::default()).unwrap().contains("headphone_power"));
    }

    #[test]
    fn parser_diagnostics_never_include_toml_secrets_or_unknown_keys() {
        struct Capture(std::sync::Mutex<Vec<String>>);
        impl log::Log for Capture {
            fn enabled(&self, _: &log::Metadata) -> bool { true }
            fn log(&self, record: &log::Record) {
                if record.target() == module_path!().trim_end_matches("::tests") {
                    self.0.lock().unwrap().push(record.args().to_string());
                }
            }
            fn flush(&self) {}
        }
        static LOGGER: Capture = Capture(std::sync::Mutex::new(Vec::new()));
        log::set_logger(&LOGGER).unwrap();
        log::set_max_level(log::LevelFilter::Warn);
        Config::parse("[server]\npassword = \"synthetic-secret-value\n", "fixture");
        Config::parse("[playback]\nvolume = \"synthetic-secret-value\"\n[synthetic-secret-section]\nx=1", "fixture");
        let output = LOGGER.0.lock().unwrap().join("\n");
        assert!(output.contains("invalid settings syntax"));
        assert!(output.contains("invalid setting"));
        assert!(!output.contains("synthetic-secret"));
    }

    fn installed_fixture(name: &str) -> Paths {
        let base = crate::library::generated_test_directory(&format!("config-installed-{name}"));
        let _ = std::fs::remove_dir_all(&base);
        let mut paths = Paths::new(base.join("internal"));
        paths.config_path = base.join("sd/Q2Mainline/config.toml");
        paths.internal = Some(paths.root.join("config.last-valid.toml"));
        paths.media = base.join("sd/Q2Mainline/media");
        paths.log_path = base.join("run/player.log");
        paths.create_all(&crate::rt::Blocking::dedicated_thread()).unwrap();
        paths
    }

    #[test]
    fn card_database_and_explicit_legacy_reads_never_authorize_original_writes() {
        let base = crate::library::generated_test_directory("storage-layout");
        std::fs::create_dir_all(&base).unwrap();
        let base = base.canonicalize().unwrap();
        let card = base.join("card/Q2Mainline");
        let old = base.join("card/Q2Navidrome");
        let paths = Paths::installed(base.join("internal"), card.clone(), base.join("run/media"), true, Some(old.clone()));
        assert_eq!(paths.database(), card.join("library.db"));
        assert!(!paths.database().starts_with(&paths.root));
        let legacy_song = old.join("offline/generated.flac");
        assert_eq!(paths.media_path(&legacy_song), Some(legacy_song.clone()));
        assert!(!paths.can_modify_media(&legacy_song));
        assert!(paths.can_modify_media(&paths.stream_cache().join("generated.mp3")));
        assert!(paths.media_path(&paths.media_root().join("../config.toml")).is_none());
        let disabled = Paths::installed(base.join("internal"), card.clone(), base.join("run/media"), true, None);
        assert!(disabled.media_path(&legacy_song).is_none());
        assert!(!disabled.can_modify_media(&legacy_song));
        let absent = Paths::installed(base.join("internal"), card, base.join("run/media"), false, Some(old));
        assert!(absent.ephemeral_media);
        assert!(absent.database().starts_with(base.join("run")));
        assert!(absent.legacy_art_cache().is_none());
    }

    #[test]
    fn unchanged_startup_snapshot_skips_replacement_but_repairs_privacy_and_changes() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
        let paths = installed_fixture("snapshot-no-rewrite");
        let original = paths.load_config(&crate::rt::Blocking::dedicated_thread());
        let snapshot = paths.internal.as_ref().unwrap();
        let inode = || std::fs::symlink_metadata(snapshot).unwrap().ino();
        let first = inode();
        assert_eq!(paths.load_config(&crate::rt::Blocking::dedicated_thread()), original);
        assert_eq!(inode(), first, "unchanged private snapshot is not replaced");
        std::fs::set_permissions(snapshot, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(paths.load_config(&crate::rt::Blocking::dedicated_thread()), original);
        assert_ne!(inode(), first, "matching but nonprivate snapshot is repaired");
        assert_eq!(std::fs::metadata(snapshot).unwrap().permissions().mode() & 0o7777, 0o600);
        let target = paths.root.join("generated-symlink-target");
        std::fs::rename(snapshot, &target).unwrap();
        symlink(&target, snapshot).unwrap();
        let untouched = std::fs::read(&target).unwrap();
        assert_eq!(paths.load_config(&crate::rt::Blocking::dedicated_thread()), original);
        assert!(std::fs::symlink_metadata(snapshot).unwrap().file_type().is_file());
        assert_eq!(std::fs::read(&target).unwrap(), untouched);
        std::fs::write(snapshot, b"invalid generated snapshot").unwrap();
        assert_eq!(paths.load_config(&crate::rt::Blocking::dedicated_thread()), original);
        assert_eq!(Config::load(&crate::rt::Blocking::dedicated_thread(), snapshot), original);
        let prior = inode();
        let mut changed = original;
        changed.playback.volume = 19;
        changed.save(&crate::rt::Blocking::dedicated_thread(), &paths.config()).unwrap();
        assert_eq!(paths.load_config(&crate::rt::Blocking::dedicated_thread()), changed);
        assert_ne!(inode(), prior);
        // A fresh Paths object, as on restart/card removal, reads the persisted
        // changed snapshot independently of the previous in-memory revision.
        let mut restart = paths.clone();
        restart.writable_config = false;
        restart.config_revision = Default::default();
        assert_eq!(restart.load_config(&crate::rt::Blocking::dedicated_thread()), changed);
    }

    fn blocking() -> crate::rt::Blocking { crate::rt::Blocking::dedicated_thread() }
    fn internal(paths: &Paths) -> Config { Config::load(&blocking(), paths.internal.as_ref().unwrap()) }
    fn card(paths: &Paths) -> Config { Config::load(&blocking(), &paths.config()) }
    fn volume(config: &Config, volume: u8) -> Config { let mut c = config.clone(); c.playback.volume = volume; c }
    /// The same player after a restart: a fresh in-memory revision.
    fn restart(paths: &Paths, card_ready: bool) -> Paths {
        let mut restart = paths.clone();
        restart.writable_config = card_ready;
        restart.config_revision = Default::default();
        restart.save_order = Default::default();
        restart
    }

    #[test]
    fn host_edit_wins_and_refreshes_nand_and_stale_ui_never_overwrites_it() {
        let paths = installed_fixture("host-wins");
        let config = paths.load_config(&blocking());
        assert!(config.server.password.is_empty() && config.wifi.networks.is_empty());
        let config = volume(&config, 25);
        paths.save_config(&config).unwrap();
        assert_eq!((card(&paths), internal(&paths)), (config.clone(), config.clone()), "saves go to NAND and the card");
        let mut host = volume(&config, 40);
        host.server.url = "https://generated.invalid".into();
        host.save(&blocking(), &paths.config()).unwrap();
        assert_eq!(paths.save_config(&config).unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
        assert_eq!(internal(&paths), config, "a refused save writes nothing");
        assert_eq!(restart(&paths, true).load_config(&blocking()), host);
        assert_eq!(internal(&paths), host, "the host edit refreshes NAND");
        assert!(!paths.stream_cache().starts_with(&paths.root));
        assert!(!paths.log().starts_with(&paths.root));
    }

    #[test]
    fn no_card_runs_from_nand_and_saves_there() {
        use std::os::unix::fs::PermissionsExt;
        let mut paths = installed_fixture("no-card");
        let good = volume(&paths.load_config(&blocking()), 30);
        paths.save_config(&good).unwrap();
        paths = restart(&paths, false);
        paths.config_path = paths.root.join("absent-sd/config.toml");
        paths.media = paths.root.parent().unwrap().join("run/media");
        paths.ephemeral_media = true;
        paths.create_all(&blocking()).unwrap();
        assert_eq!(paths.load_config(&blocking()), good);
        let saved = volume(&good, 31);
        paths.save_config(&saved).unwrap();
        assert_eq!(internal(&paths), saved);
        assert_eq!(restart(&paths, false).load_config(&blocking()), saved);
        assert!(!paths.config_path.parent().unwrap().exists(), "no card paths are created");
        assert_eq!(std::fs::metadata(paths.internal.as_ref().unwrap()).unwrap().permissions().mode() & 0o777, 0o600);
        // First start with neither copy: defaults, and saving works.
        let fresh = restart(&installed_fixture("no-card-first"), false);
        assert_eq!(fresh.load_config(&blocking()), Config::default());
        fresh.save_config(&volume(&Config::default(), 12)).unwrap();
        assert_eq!(internal(&fresh).playback.volume, 12);
    }

    #[test]
    fn a_card_without_settings_is_seeded_from_nand() {
        let paths = installed_fixture("seed");
        let first = volume(&paths.load_config(&blocking()), 33);
        paths.save_config(&first).unwrap();
        // Removed: saves continue on NAND.
        let removed = restart(&paths, false);
        assert_eq!(removed.load_config(&blocking()), first);
        let latest = volume(&first, 34);
        removed.save_config(&latest).unwrap();
        // A new card without config.toml.
        let mut swapped = restart(&paths, true);
        swapped.config_path = paths.config_path.parent().unwrap().with_file_name("new-card").join("config.toml");
        std::fs::create_dir_all(swapped.config_path.parent().unwrap()).unwrap();
        assert_eq!(swapped.load_config(&blocking()), latest);
        assert_eq!(card(&swapped), latest, "seeded from NAND");
        let next = volume(&latest, 35);
        swapped.save_config(&next).unwrap();
        assert_eq!((card(&swapped), internal(&swapped)), (next.clone(), next.clone()));
        // A config.toml deleted on the host is also written again.
        std::fs::remove_file(swapped.config()).unwrap();
        assert_eq!(restart(&swapped, true).load_config(&blocking()), next);
        assert_eq!(card(&swapped), next);
    }

    #[test]
    fn installed_saves_keep_the_card_portal_table_and_never_copy_it_to_nand() {
        let paths = installed_fixture("portal-table");
        // Generated account; a hand-written table the player does not model.
        let portal = "# typed on the host\n[portal.delta]\nskymiles_number = 9000000004\npassword = \"generated-only\"\n";
        let hand = format!("[playback]\nvolume = 20\n\n{portal}\n[later]\nkey = 'x'\n");
        std::fs::write(paths.config(), &hand).unwrap();
        let loaded = paths.load_config(&blocking());
        assert!(loaded.portal.delta.as_ref().unwrap().sign_in().is_some());
        assert_eq!(std::fs::read_to_string(paths.config()).unwrap(), hand, "starting does not rewrite the card");
        let internal_text = || std::fs::read_to_string(paths.internal.as_ref().unwrap()).unwrap();
        assert!(!internal_text().contains("portal") && !internal_text().contains("generated-only"));
        paths.save_config(&volume(&loaded, 21)).unwrap();
        let saved = std::fs::read_to_string(paths.config()).unwrap();
        assert!(saved.contains(portal) && saved.contains("[later]\nkey = 'x'\n"), "{saved}");
        assert!(!internal_text().contains("generated-only"));
        // The next start uses NAND for the modeled settings and the card's account.
        let next = restart(&paths, true).load_config(&blocking());
        assert_eq!(next.playback.volume, 21);
        assert_eq!(next.portal, loaded.portal);
        assert_eq!(std::fs::read_to_string(paths.config()).unwrap(), saved);
        // Saved without the card, then the card returns unchanged: the catch-up
        // write keeps the account too.
        let removed = restart(&paths, false);
        let away = removed.load_config(&blocking());
        assert!(away.portal.delta.is_none(), "no account without the card");
        removed.save_config(&volume(&away, 22)).unwrap();
        let returned = restart(&paths, true).load_config(&blocking());
        assert_eq!((returned.playback.volume, &returned.portal), (22, &loaded.portal));
        assert!(std::fs::read_to_string(paths.config()).unwrap().contains(portal));
    }

    #[test]
    fn an_unchanged_card_catches_up_with_settings_saved_without_it() {
        let paths = installed_fixture("catch-up");
        let first = volume(&paths.load_config(&blocking()), 41);
        paths.save_config(&first).unwrap();
        let removed = restart(&paths, false);
        removed.load_config(&blocking());
        let latest = volume(&first, 42);
        removed.save_config(&latest).unwrap();
        assert_eq!(card(&paths), first, "the card was away");
        let returned = restart(&paths, true);
        assert_eq!(returned.load_config(&blocking()), latest, "NAND is current; the card is not newer");
        assert_eq!(card(&paths), latest);
        // Power loss after the card write but before the record: the card
        // looks edited and wins, with the same settings.
        let record = paths.card_record().unwrap();
        std::fs::write(&record, b"0123456789abcdef0123456789abcdef").unwrap();
        assert_eq!(restart(&paths, true).load_config(&blocking()), latest);
        // A missing record (the first start of this version) lets the card win.
        std::fs::remove_file(&record).unwrap();
        volume(&latest, 43).save(&blocking(), paths.internal.as_ref().unwrap()).unwrap();
        assert_eq!(restart(&paths, true).load_config(&blocking()), latest);
        assert_eq!(internal(&paths), latest);
    }

    #[test]
    fn invalid_card_settings_never_replace_nand_and_are_kept() {
        let paths = installed_fixture("invalid-host");
        let good = volume(&paths.load_config(&blocking()), 28);
        paths.save_config(&good).unwrap();
        let invalid = "[server]\npassword = \"generated-secret\"\nthis = = invalid\n";
        std::fs::write(paths.config(), invalid).unwrap();
        let running = restart(&paths, true);
        assert_eq!(running.load_config(&blocking()), good);
        assert_eq!(internal(&paths), good);
        let saved = volume(&good, 29);
        running.save_config(&saved).unwrap();
        assert_eq!(internal(&paths), saved, "saving works on NAND");
        assert_eq!(std::fs::read_to_string(paths.config()).unwrap(), invalid, "the host file is kept");
        assert_eq!(restart(&paths, true).load_config(&blocking()), saved);
        // Fixed on the host: the card wins again.
        let fixed = volume(&good, 50);
        fixed.save(&blocking(), &paths.config()).unwrap();
        assert_eq!(restart(&paths, true).load_config(&blocking()), fixed);
        assert_eq!(internal(&paths), fixed);
    }

    #[test]
    fn round_trips_and_defaults() {
        let dir = std::env::temp_dir().join(format!("q2nd-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        std::fs::write(&p, "[server]\nurl = \"http://nd:4533\"\nuser = \"me\"\npassword = \"pw\"\n[playback]\nstream_quality = \"mp3-192\"\n").unwrap();
        let c = Config::load(&crate::rt::Blocking::dedicated_thread(), &p);
        assert!(c.server.is_configured());
        assert_eq!(c.playback.stream_quality, Quality::Mp3_192);
        assert!(c.playback.gapless, "unset fields keep their defaults");
        assert_eq!(c.display.brightness, 50);
        c.save(&crate::rt::Blocking::dedicated_thread(), &p).unwrap();
        assert_eq!(Config::load(&crate::rt::Blocking::dedicated_thread(), &p), c);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_saved_tailscale_backend_is_accepted_ignored_and_dropped() {
        for value in ["\"go\"", "\"rs\"", "\"python\"", "3"] {
            let text = format!("[tailscale]\nenabled = true\nbackend = {value}\n[playback]\nvolume = 41\n");
            let user: toml::Table = text.parse().unwrap();
            assert!(has_legacy_tailscale_backend(&user), "{value}");
            let c = Config::parse(&text, "test");
            assert!(c.tailscale.enabled, "{value}: the rest of the section is kept");
            assert_eq!(c.playback.volume, 41, "{value}: and the rest of the file");
            assert!(!String::from_utf8(c.serialized().unwrap()).unwrap().contains("backend"), "{value}: a save drops it");
        }
        assert!(!has_legacy_tailscale_backend(&"[tailscale]\nenabled = true\n".parse().unwrap()));
        assert!(!has_legacy_tailscale_backend(&"[server]\nbackend = 1\n".parse().unwrap()));
    }

    #[test]
    fn a_bad_value_costs_only_itself() {
        let c = Config::parse(
            "[server]\nurl = \"http://nd\"\nuser = \"me\"\n[display]\naccent = \"ivory\"\nbrightness = 80\n[nonsense]\nx = 1\n",
            "test",
        );
        assert_eq!(c.server.url, "http://nd");
        assert_eq!(c.display.brightness, 80);
        assert_eq!(c.display.accent, Accent::Blue);
    }

    #[test]
    fn legacy_abs_route_is_ignored_and_global_route_is_shared_without_enabling_it() {
        for enabled in [false,true] {
            for legacy in ["direct","tailscale","automatic"] {
                let body=format!("[tailscale]\nenabled={enabled}\n[audiobookshelf]\nurl='http://fixture.invalid'\ntoken='generated'\nconnection='{legacy}'\n");
                let config=Config::parse(&body,"generated-route-migration");
                assert_eq!(config.tailscale.enabled,enabled);
                assert_eq!(config.http_proxy(),enabled.then_some(crate::tailscale::PROXY));
                assert_eq!(config.audiobookshelf.token,"generated");
                assert_eq!(config.audiobookshelf.url,"http://fixture.invalid");
                let saved=toml::to_string(&config).unwrap();
                assert!(!saved.contains("connection ="));
                assert_eq!(toml::from_str::<Config>(&saved).unwrap(),config);
            }
        }
    }
    #[test]
    fn the_example_parses() {
        let c: Config = toml::from_str(include_str!("../config.toml.example")).unwrap();
        assert!(c.server.is_configured());
        assert_eq!(c.wifi.networks.len(), 1);
        assert_eq!(c.storage.stream_cache_mb, 4096);
    }

    #[test]
    fn bad_file_gives_defaults() {
        let dir = std::env::temp_dir().join(format!("q2nd-cfg-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("config.toml");
        std::fs::write(&p, "this is = = not toml").unwrap();
        assert_eq!(Config::load(&crate::rt::Blocking::dedicated_thread(), &p), Config::default());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

#[derive(Clone, Default, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShelfLibraryPreference {
    pub namespace: String,
    pub id: String,
    pub name: String,
}

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Audiobookshelf {
    pub url: String,
    pub token: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_books: Option<ShelfLibraryPreference>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_podcasts: Option<ShelfLibraryPreference>,
    /// Hours between background full resyncs of the library, run only while
    /// charging (`full_sync_hours`, default 24; 0 turns them off).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub full_sync_hours: Option<u32>,
}
impl Audiobookshelf {
    /// The background full resync interval; `None` when turned off.
    pub fn full_sync_interval(&self) -> Option<std::time::Duration> {
        match self.full_sync_hours.unwrap_or(24) { 0 => None, hours => Some(std::time::Duration::from_secs(u64::from(hours.min(24 * 365)) * 3600)) }
    }
    pub fn library_preference(&self, podcast: bool) -> Option<&ShelfLibraryPreference> {
        if podcast {
            self.default_podcasts.as_ref()
        } else {
            self.default_books.as_ref()
        }
    }
    pub fn set_library_preference(&mut self, podcast: bool, value: ShelfLibraryPreference) {
        if podcast {
            self.default_podcasts = Some(value);
        } else {
            self.default_books = Some(value);
        }
    }
}
impl std::fmt::Debug for Audiobookshelf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Audiobookshelf")
            .field("default_books_set", &self.default_books.is_some())
            .field("default_podcasts_set", &self.default_podcasts.is_some())
            .field(
                "configured",
                &(!self.url.is_empty() && !self.token.is_empty()),
            )
            .finish()
    }
}

#[cfg(test)]
mod shelf_library_tests {
    use super::*;
    #[test]
    fn book_and_podcast_defaults_roundtrip_independently_without_debug_content() {
        let legacy: Config =
            toml::from_str("[audiobookshelf]\nurl='http://fixture.invalid'\ntoken='generated'\n")
                .unwrap();
        assert!(
            legacy.audiobookshelf.default_books.is_none()
                && legacy.audiobookshelf.default_podcasts.is_none()
        );
        let mut config = legacy;
        for i in 0..256 {
            let podcast = i % 2 == 0;
            let old = config.audiobookshelf.library_preference(!podcast).cloned();
            let preference = ShelfLibraryPreference {
                namespace: "generated-account".into(),
                id: format!("library-{i}"),
                name: "synthetic-private-library-name".into(),
            };
            config
                .audiobookshelf
                .set_library_preference(podcast, preference.clone());
            assert_eq!(
                config.audiobookshelf.library_preference(!podcast),
                old.as_ref()
            );
            let restored: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
            assert_eq!(
                restored.audiobookshelf.library_preference(podcast),
                Some(&preference)
            );
            assert_eq!(
                restored.audiobookshelf.library_preference(!podcast),
                old.as_ref()
            );
            assert!(
                !format!("{:?}", restored.audiobookshelf)
                    .contains("synthetic-private-library-name")
            );
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct SpokenSettings {
    pub skip_forward_seconds: u32,
    pub skip_backward_seconds: u32,
    pub skip_chapter: bool,
    pub speed_percent: u16,
}
impl Default for SpokenSettings {
    fn default() -> Self {
        Self { skip_forward_seconds: options::DEFAULT_SKIP_FORWARD, skip_backward_seconds: options::DEFAULT_SKIP_BACKWARD, skip_chapter: false, speed_percent: options::DEFAULT_SPOKEN_SPEED }
    }
}
impl<'de> Deserialize<'de> for SpokenSettings {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Default, Deserialize)]
        #[serde(default)]
        struct Saved {
            skip_seconds: Option<u32>,
            skip_forward_seconds: Option<u32>,
            skip_backward_seconds: Option<u32>,
            skip_chapter: bool,
            speed_percent: Option<u16>,
        }
        let saved = Saved::deserialize(deserializer)?;
        let defaults = Self::default();
        let speed = saved.speed_percent.unwrap_or(defaults.speed_percent);
        if !(50..=300).contains(&speed) || speed % 5 != 0 {
            return Err(serde::de::Error::custom("spoken speed_percent must be 50..300 in steps of 5"));
        }
        Ok(Self {
            skip_forward_seconds: saved.skip_forward_seconds.or(saved.skip_seconds).unwrap_or(defaults.skip_forward_seconds),
            skip_backward_seconds: saved.skip_backward_seconds.or(saved.skip_seconds).unwrap_or(defaults.skip_backward_seconds),
            skip_chapter: saved.skip_chapter,
            speed_percent: speed,
        })
    }
}

#[cfg(test)]
mod spoken_settings_tests {
    use super::*;
    #[test]
    fn directional_skips_migrate_explicit_legacy_values_and_round_trip_independently() {
        let defaults: SpokenSettings = toml::from_str("").unwrap();
        assert_eq!((defaults.skip_forward_seconds, defaults.skip_backward_seconds), (30, 15));
        for value in 1..=3600 {
            let legacy: SpokenSettings = toml::from_str(&format!("skip_seconds={value}\n")).unwrap();
            assert_eq!((legacy.skip_forward_seconds, legacy.skip_backward_seconds), (value, value));
            let separate: SpokenSettings = toml::from_str(&format!("skip_seconds={value}\nskip_backward_seconds=15\n")).unwrap();
            assert_eq!((separate.skip_forward_seconds, separate.skip_backward_seconds), (value, 15));
            assert_eq!(toml::from_str::<SpokenSettings>(&toml::to_string(&separate).unwrap()).unwrap(), separate);
        }
    }
}
