//! Bluetooth headphones: the thread that owns the controller's UART and
//! runs the `q2-bt` host, the state Settings shows, and the paired-device
//! store, which also keeps each device's volume, headphone EQ choice and
//! Device ID. Audio reaches it through [`pcm`]; the player selects it as
//! `audio::output::Route::Bluetooth` while a stream is open.
//! Design and device facts: docs/bluetooth.md.
pub mod coder;
mod media;
pub mod pacing;
pub mod pcm;
pub mod probe;
mod telemetry;
mod uart;

use q2_bt::host::{Addr, Command, Diagnostics, Event, Host, Key, LinkStats, Settings, Stream, Version};
pub use q2_bt::host::Step;
use q2_bt::codec::Kind;
use media::Media;
pub use media::MediaStats;
use std::collections::VecDeque;
use crate::rt::Blocking;
use std::io::{self, PipeReader, PipeWriter, Write};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub use q2_bt::avrcp::Button;
pub use q2_bt::sdp::DeviceId;

/// The player's volume (0..=100) as an AVRCP absolute volume level (0..=127).
pub fn to_level(volume: u8) -> u8 { ((u32::from(volume.min(100)) * 127 + 50) / 100) as u8 }
/// An AVRCP absolute volume level (0..=127) as the player's volume (0..=100).
pub fn from_level(level: u8) -> u8 { ((u32::from(level.min(127)) * 100 + 63) / 127) as u8 }
/// How long after the last volume change the store is written.
const VOLUME_SAVE_DELAY: Duration = Duration::from_secs(2);

/// How a device's headphone EQ profile was picked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EqChoice {
    /// Matched from the device's name and Device ID when it first connected.
    Automatic,
    /// Picked by the listener.
    Chosen,
}

/// A device's headphone EQ: an AutoEQ catalog entry (its path; empty for none) and its name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceEq { pub choice: EqChoice, pub enabled: bool, pub path: String, pub name: String }

/// What the store keeps for a paired device besides its key.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Saved {
    /// The player's volume for this device, 0..=100; `None` until first set.
    pub volume: Option<u8>,
    /// `None`: not chosen yet; the player matches one automatically.
    pub eq: Option<DeviceEq>,
    pub id: Option<DeviceId>,
}

/// The device audio goes to, while its stream is open.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Output {
    pub addr: Addr,
    pub name: String,
    pub class: u32,
    pub saved: Saved,
    /// Revision of the opened stream or its last committed volume, shared across reconnects.
    pub volume_revision: u32,
    /// The headphones take absolute volume: their amplifier follows the player's volume.
    pub absolute: bool,
}

/// Where the controller is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Port {
    /// A tty, `/dev/ttyS0` on the Q2.
    Tty(PathBuf),
    /// The in-process controller and headphones (simulator, emulator, tests).
    Fake,
    /// No controller: switching on reports that at once, without a thread.
    Absent,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub port: Port,
    pub baud: u32,
    pub flow: bool,
    /// Searched in order for `rtl8733bs_fw` and `rtl8733bs_config`.
    pub firmware_dirs: Vec<PathBuf>,
    /// The paired-device file.
    pub store: PathBuf,
    /// The name headphones see.
    pub name: String,
    /// A regulator consumer that keeps the combo chip's Bluetooth enable up while Wi-Fi is off.
    pub power_hold: Option<PathBuf>,
    /// Connect to the last used headphones when the controller is ready.
    pub reconnect: bool,
    pub bench_peer: Option<Addr>,
    /// Codecs to offer and choose from, most preferred first.
    pub codecs: Vec<Kind>,
    pub ldac: coder::LdacQuality,
    /// LDAC lowers its bit rate when the link falls behind and returns to `ldac` when it clears.
    pub ldac_adaptive: bool,
    /// `baud` and `flow` are replaced by [`AUTO_UART`]: 3 Mbaud with RTS/CTS (device-verified
    /// on 2026-11-22, kernel `pm-q2-perf-v134` with patches 0191 and 0192), then 1.5 Mbaud
    /// with and without RTS/CTS (docs/bluetooth.md, UART fallback).
    pub uart_auto: bool,
    /// A bring-up that fails before Ready is tried at the next setting of [`uart_settings`].
    /// Off only for `--bt-probe` runs that name a rate or flow control.
    pub uart_fallback: bool,
    /// After the Wi-Fi chip powers up again, run the start-up commands again on a
    /// controller with no headphones connected (`[bluetooth] resync_after_wifi`).
    pub resync: bool,
    /// A one-line state summary for device diagnosis (no addresses or names).
    pub status_file: Option<PathBuf>,
    /// Switches the fake's first headphones off, which drops their link, and on again.
    #[cfg(test)]
    pub(crate) fake_off: Arc<AtomicBool>,
    /// Presses of the fake's first headphones' volume-up key asked for.
    #[cfg(test)]
    pub(crate) fake_keys: Arc<AtomicU32>,
    /// Applied to each fake controller when it starts (a slow bring-up, a long page).
    #[cfg(test)]
    pub(crate) fake_setup: Option<fn(&mut q2_bt::fake::Fake)>,
    /// Drops and raises the fake controller's enable: it returns to its ROM.
    #[cfg(test)]
    pub(crate) fake_power_cycle: Arc<AtomicBool>,
    /// UART settings (rate, flow control) at which the fake's line carries garbage.
    #[cfg(test)]
    pub(crate) fake_uart_broken: Option<fn(u32, bool) -> bool>,
    /// The rates the fake's port takes (`Wire::accepts`); all where unset.
    #[cfg(test)]
    pub(crate) fake_uart_accepts: Option<fn(u32) -> bool>,
    /// Keeps the fake controller across bring-ups of a session: the Wi-Fi side powers it, so
    /// dropping the hold does not reset it.
    #[cfg(test)]
    pub(crate) fake_kept: Option<KeptFake>,
}

/// A fake controller between the bring-ups that share it, with the time its clock started.
#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct KeptFake {
    controller: Arc<Mutex<Option<(q2_bt::fake::Fake, Instant)>>>,
    volume_sets: Arc<AtomicU32>,
}
#[cfg(test)]
impl KeptFake {
    fn take(&self) -> Option<(q2_bt::fake::Fake, Instant)> { self.controller.lock().unwrap().take() }
    fn keep(&self, fake: q2_bt::fake::Fake, start: Instant) { *self.controller.lock().unwrap() = Some((fake, start)); }
}
#[cfg(test)]
impl std::fmt::Debug for KeptFake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { f.write_str("KeptFake") }
}

impl Options {
    pub fn mainline(config: &crate::config::Bluetooth) -> Self {
        let mut firmware_dirs: Vec<PathBuf> = config.firmware_dir.iter().map(PathBuf::from).collect();
        firmware_dirs.extend(["/lib/firmware/rtlbt", "/state/bluetooth/firmware"].map(PathBuf::from));
        Self { port: if config.device == "fake" { Port::Fake } else { Port::Tty(config.device.clone().into()) }, baud: config.baud, flow: config.flow_control,
            firmware_dirs, store: "/state/bluetooth/devices".into(), name: "Q2".into(),
            power_hold: Some("/sys/devices/platform/bt-power/state".into()), reconnect: true, bench_peer: None,
            codecs: codecs(&config.codec), ldac: ldac_quality(&config.ldac_quality), ldac_adaptive: config.ldac_adaptive,
            uart_auto: config.uart_mode != "manual", uart_fallback: true, resync: config.resync_after_wifi, status_file: (config.device != "fake").then(|| STATUS_FILE.into()),
            #[cfg(test)] fake_off: Arc::default(), #[cfg(test)] fake_keys: Arc::default(), #[cfg(test)] fake_power_cycle: Arc::default(), #[cfg(test)] fake_setup: None,
            #[cfg(test)] fake_uart_broken: None, #[cfg(test)] fake_uart_accepts: None, #[cfg(test)] fake_kept: None }
    }
    /// The handle a UI holds before main supplies the configured one.
    pub fn absent(data: &Path) -> Self { Self { port: Port::Absent, ..Self::simulated(data) } }
    pub fn simulated(data: &Path) -> Self {
        Self { port: Port::Fake, baud: 1_500_000, flow: false, firmware_dirs: Vec::new(), store: data.join("bluetooth/devices"), name: "Q2".into(),
            power_hold: None, reconnect: true, bench_peer: None, codecs: Kind::AUTOMATIC.to_vec(), ldac: coder::LdacQuality::Standard, ldac_adaptive: true, uart_auto: false,
            uart_fallback: true, resync: true, status_file: None,
            #[cfg(test)] fake_off: Arc::default(), #[cfg(test)] fake_keys: Arc::default(), #[cfg(test)] fake_power_cycle: Arc::default(), #[cfg(test)] fake_setup: None,
            #[cfg(test)] fake_uart_broken: None, #[cfg(test)] fake_uart_accepts: None, #[cfg(test)] fake_kept: None }
    }
}

/// Where the device build writes its state line (`status_file`).
pub const STATUS_FILE: &str = "/run/q2/bluetooth-status";

/// The `[bluetooth] codec` setting: `auto` prefers the best codec the headphones offer;
/// a named codec is used when they have it. SBC is always the fallback.
pub fn codecs(setting: &str) -> Vec<Kind> {
    match CODEC_SETTINGS.iter().find(|(name, _, _)| *name == setting).and_then(|(_, _, kind)| *kind) {
        Some(Kind::Sbc) => vec![Kind::Sbc],
        // The other codecs stay available to headphones that configure one themselves.
        Some(kind) => std::iter::once(kind).chain(Kind::AUTOMATIC.into_iter().filter(|k| *k == Kind::Sbc)).collect(),
        None => Kind::AUTOMATIC.to_vec(),
    }
}
/// Setting value, the name Settings shows, the codec.
pub const CODEC_SETTINGS: [(&str, &str, Option<Kind>); 6] = [("auto", "Automatic", None), ("ldac", "LDAC", Some(Kind::Ldac)), ("aptx-hd", "aptX HD", Some(Kind::AptXHd)),
    ("aptx", "aptX", Some(Kind::AptX)), ("aac", "AAC", Some(Kind::Aac)), ("sbc", "SBC", Some(Kind::Sbc))];
pub fn ldac_quality(setting: &str) -> coder::LdacQuality {
    match setting { "high" => coder::LdacQuality::High, "mobile" => coder::LdacQuality::Mobile, _ => coder::LdacQuality::Standard }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device { pub addr: Addr, pub name: String, pub class: u32, /// Signal strength in dBm when found by a scan; 0 when not measured.
    pub rssi: i8,
    /// Its extended inquiry response lists the A2DP sink service (found devices only).
    pub sink: bool }
impl Device {
    /// Audio/video major device class: headphones, headsets, speakers.
    pub fn is_audio(&self) -> bool { self.class >> 8 & 0x1f == 4 }
    /// What the device is, in a word or two, from its class of device and inquiry response.
    pub fn kind(&self) -> &'static str {
        if !self.is_audio() { return if self.sink { "Takes Bluetooth audio" } else { "Other device" } }
        // Audio/video minor classes: 1 wearable headset, 2 hands-free, 6 headphones;
        // 5 loudspeaker, 7 portable audio, 8 car audio, 10 hi-fi.
        match self.class >> 2 & 0x3f { 1 | 2 | 6 => "Headphones", 5 | 7 | 8 | 10 => "Speaker", _ => "Audio device" }
    }
    /// The signal of a found device, as the Wi-Fi page words it; `None` when not measured.
    pub fn signal(&self) -> Option<&'static str> {
        match self.rssi { 0 => None, s if s >= -60 => Some("Strong"), s if s >= -72 => Some("Good"), _ => Some("Weak") }
    }
    /// Of the audio class, or says in its inquiry response that it takes A2DP audio
    /// (a computer or a speaker of another class).
    pub fn takes_audio(&self) -> bool { self.is_audio() || self.sink }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Phase { #[default] Off, Starting, Ready, Failed(String) }

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Link { #[default] Idle, Connecting(Addr), Connected(Addr), Playing(Addr) }

#[derive(Clone, Debug, Default)]
pub struct State {
    pub phase: Phase,
    pub controller: Option<Version>,
    pub bench_local_address: Option<Addr>,
    pub scanning: bool,
    pub found: Vec<Device>,
    pub paired: Vec<Device>,
    pub link: Link,
    /// The last thing that went wrong, in words for the screen.
    pub notice: String,
    pub stream: Option<Stream>,
    /// Media packets dropped, H5 retransmissions, PCM underruns.
    pub counters: (u32, u32, u32),
    /// HCI and transport counts for bring-up diagnosis.
    pub diagnostics: Diagnostics,
    /// The device audio goes to, while its stream is open.
    pub output: Option<Output>,
    /// The audio path: controller buffers, transport errors, RSSI and link quality.
    pub link_stats: LinkStats,
    /// The open stream's pacing and bit rate; None without a stream.
    pub media: Option<MediaStats>,
    /// The UART's receive errors since Bluetooth started (None where the port has no counters).
    pub uart_errors: Option<uart::Errors>,
    /// The UART rate and flow control in use after bring-up.
    pub uart: (u32, bool),
    /// What the thread did after the Wi-Fi chip powered up again.
    pub resync: Resync,
    /// The step a connection attempt has reached (`Link::Connecting`).
    pub step: Option<Step>,
    /// The device the listener last asked to connect, and whether it needs the sink check:
    /// what Try Again repeats after a failure. Cleared when it connects.
    pub attempt: Option<(Addr, bool)>,
    /// A search for devices in pairing mode is open (`Bluetooth::discover`).
    pub discovering: bool,
    /// When each paired device was last connected or disconnected, in seconds since 1970.
    pub seen: Vec<(Addr, u64)>,
    /// The LDAC quality this session encodes at.
    pub ldac: Option<coder::LdacQuality>,
    /// The last problem of this session, kept after `notice` is cleared, for diagnosis.
    pub last_error: String,
    /// Why the open stream has its codec, in words.
    pub codec_reason: String,
    /// Requests that wait until the controller can take them (a scan during a connection attempt).
    pub waiting: u32,
}

/// Counts of the controller checks that follow a Wi-Fi power-up (docs/bugs/wifi-lps-after-power-cycle.md).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Resync {
    /// Wi-Fi power-ups seen while the controller was ready.
    pub checks: u32,
    /// The controller answered: it kept its power and patch.
    pub alive: u32,
    /// The controller did not answer and was brought up again.
    pub lost: u32,
    /// Start-up commands run again on an idle controller.
    pub reinits: u32,
    /// Not run again: headphones connected or connecting, or a scan open.
    pub busy: u32,
}

impl State {
    /// Sets the notice, and the last error kept for diagnosis.
    fn problem(&mut self, text: impl Into<String>) {
        self.notice = text.into();
        self.last_error.clone_from(&self.notice);
    }
    pub fn seen(&self, addr: Addr) -> Option<u64> { self.seen.iter().find(|s| s.0 == addr).map(|s| s.1) }
    /// The codec in use and its bit rate or quality mode, for the connected device.
    pub fn codec_detail(&self) -> Option<String> { self.stream.map(|stream| describe(&stream, self.ldac.unwrap_or(coder::LdacQuality::Standard))) }
    pub fn name(&self, addr: Addr) -> String {
        self.paired.iter().chain(&self.found).find(|d| d.addr == addr).map(|d| d.name.clone()).unwrap_or_else(|| "Headphones".into())
    }
    pub fn connected(&self) -> Option<Addr> { match self.link { Link::Connected(addr) | Link::Playing(addr) => Some(addr), _ => None } }
    /// One line for Settings.
    pub fn summary(&self) -> String {
        match (&self.phase, &self.link) {
            (Phase::Off, _) => "Off".into(),
            (Phase::Starting, _) => "Starting…".into(),
            (Phase::Failed(_), _) => "Unavailable".into(),
            (Phase::Ready, Link::Idle) => "Not Connected".into(),
            (Phase::Ready, Link::Connecting(_)) => "Connecting…".into(),
            (Phase::Ready, Link::Connected(addr) | Link::Playing(addr)) => self.name(*addr),
        }
    }
}

/// Why the stream has `kind`, from how it was chosen and the codec preference in force.
fn codec_reason(origin: Option<q2_bt::host::Origin>, kind: Kind, preference: &[Kind]) -> String {
    use q2_bt::host::Origin;
    match origin {
        Some(Origin::Upgraded { from }) => format!("Changed from {}, which the headphones chose", from.name()),
        Some(Origin::Theirs { checked: true }) => "Chosen by the headphones; nothing better in common".into(),
        Some(Origin::Theirs { checked: false }) => "Chosen by the headphones".into(),
        _ if preference.len() > 2 => "The best both sides have".into(),
        _ if preference.first() == Some(&kind) => "Chosen in Settings".into(),
        _ => format!("The headphones lack {}", preference.first().map_or("it", |k| k.name())),
    }
}

/// The stream's sampling rate: "44.1 kHz".
pub fn sample_rate(stream: &Stream) -> String {
    let rate = stream.codec.rate();
    if rate % 1000 == 0 { format!("{} kHz", rate / 1000) } else { format!("{:.1} kHz", f64::from(rate) / 1000.0) }
}

/// The codec and its bit rate, with LDAC's quality mode: "LDAC · 606 kbit/s (Standard)".
pub fn describe(stream: &Stream, ldac: coder::LdacQuality) -> String {
    use q2_bt::codec::Codec;
    let rate = stream.codec.rate();
    let family = u32::from(rate % 11025 == 0);
    let kbits = match stream.codec {
        Codec::Sbc(p) => format!("{} kbit/s", (p.frame_len() as u64 * 8 * u64::from(p.rate) / p.samples() as u64 + 500) / 1000),
        Codec::Aac { bitrate, .. } => format!("{} kbit/s", bitrate / 1000),
        // 4:1 of 16-bit (aptX) or 24-bit (aptX HD) stereo.
        Codec::AptX { .. } => format!("{} kbit/s", rate * 32 / 4 / 1000),
        Codec::AptXHd { .. } => format!("{} kbit/s", rate * 48 / 4 / 1000),
        // libldac's rates: 990/660/330 at 48 kHz, 909/606/303 at 44.1 kHz.
        Codec::Ldac { .. } => match ldac {
            coder::LdacQuality::High => format!("{} kbit/s (High)", [990, 909][family as usize]),
            coder::LdacQuality::Standard => format!("{} kbit/s (Standard)", [660, 606][family as usize]),
            coder::LdacQuality::Mobile => format!("{} kbit/s (Mobile)", [330, 303][family as usize]),
        },
    };
    format!("{} · {kbits}", stream.codec.kind().name())
}

/// Paired devices, their link keys, what is kept for each and when each was last seen, one record per line.
#[derive(Default, Debug, PartialEq)]
struct Store { devices: Vec<Device>, keys: Vec<Key>, last: Option<Addr>, saved: Vec<(Addr, Saved)>, seen: Vec<(Addr, u64)> }

/// Seconds since 1970, for `State::seen`.
pub fn unix_now() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs()) }

fn hex(bytes: &[u8]) -> String { bytes.iter().map(|b| format!("{b:02x}")).collect() }
fn unhex<const N: usize>(text: &str) -> Option<[u8; N]> {
    if text.len() != 2 * N || !text.is_ascii() { return None }
    let mut out = [0u8; N];
    for (i, byte) in out.iter_mut().enumerate() { *byte = u8::from_str_radix(&text[2 * i..2 * i + 2], 16).ok()?; }
    Some(out)
}

fn hex16(text: &str) -> Option<u16> { (text.len() == 4).then(|| u16::from_str_radix(text, 16).ok()).flatten() }

impl Store {
    fn parse(text: &str) -> Self {
        let mut store = Self::default();
        for line in text.lines() {
            let mut fields = line.splitn(3, ' ');
            let (Some(kind), Some(addr), rest) = (fields.next(), fields.next().and_then(unhex::<6>), fields.next()) else { continue };
            let pair = rest.and_then(|rest| rest.split_once(' '));
            match (kind, rest, pair) {
                ("device", _, Some((class, name))) => if let Ok(class) = u32::from_str_radix(class, 16) {
                    store.devices.retain(|d| d.addr != addr);
                    store.devices.push(Device { addr, name: name.into(), class, rssi: 0, sink: false });
                },
                ("key", _, Some((kind, key))) => if let (Ok(kind), Some(key)) = (kind.parse(), unhex::<16>(key)) {
                    store.keys.retain(|k| k.addr != addr);
                    store.keys.push(Key { addr, key, kind });
                },
                ("last", None, _) => store.last = Some(addr),
                ("volume", Some(volume), None) => if let Some(volume) = volume.parse::<u8>().ok().filter(|v| *v <= 100) { store.saved_mut(addr).volume = Some(volume); },
                ("eq", Some(rest), _) => {
                    let mut f = rest.splitn(4, ' ');
                    let choice = match f.next() { Some("auto") => Some(EqChoice::Automatic), Some("chosen") => Some(EqChoice::Chosen), _ => None };
                    let enabled = match f.next() { Some("on") => Some(true), Some("off") => Some(false), _ => None };
                    if let (Some(choice), Some(enabled), Some(path), Some(name)) = (choice, enabled, f.next(), f.next()) {
                        let path = if path == "-" { String::new() } else { path.into() };
                        // A catalog path never holds spaces or control characters.
                        if !path.contains(char::is_control) { store.saved_mut(addr).eq = Some(DeviceEq { choice, enabled, path, name: name.into() }); }
                    }
                }
                ("id", Some(rest), _) => {
                    let values: Vec<Option<u16>> = rest.split(' ').map(hex16).collect();
                    if let [Some(source), Some(vendor), Some(product), Some(version)] = values[..] { store.saved_mut(addr).id = Some(DeviceId { source, vendor, product, version }); }
                }
                ("seen", Some(at), None) => if let Ok(at) = at.parse() { store.seen.retain(|s| s.0 != addr); store.seen.push((addr, at)); },
                _ => {}
            }
        }
        // A device without its key cannot reconnect; a key without its device has no row to forget it from.
        let keys = store.keys.clone();
        store.devices.retain(|d| keys.iter().any(|k| k.addr == d.addr));
        let devices = store.devices.clone();
        store.keys.retain(|k| devices.iter().any(|d| d.addr == k.addr));
        store.last = store.last.filter(|addr| devices.iter().any(|d| d.addr == *addr));
        store.saved.retain(|(addr, saved)| devices.iter().any(|d| d.addr == *addr) && *saved != Saved::default());
        store.seen.retain(|s| devices.iter().any(|d| d.addr == s.0));
        store
    }
    fn saw(&mut self, addr: Addr) { self.seen.retain(|s| s.0 != addr); self.seen.push((addr, unix_now())); }
    fn text(&self) -> String {
        let mut out = String::new();
        for d in &self.devices { out += &format!("device {} {:06x} {}\n", hex(&d.addr), d.class, d.name.replace(['\n', '\r'], " ")); }
        for k in &self.keys { out += &format!("key {} {} {}\n", hex(&k.addr), k.kind, hex(&k.key)); }
        if let Some(last) = self.last { out += &format!("last {}\n", hex(&last)); }
        for (addr, saved) in &self.saved {
            if let Some(volume) = saved.volume { out += &format!("volume {} {volume}\n", hex(addr)); }
            if let Some(eq) = &saved.eq {
                let path = if eq.path.is_empty() { "-".into() } else { eq.path.replace([' ', '\n', '\r'], "") };
                out += &format!("eq {} {} {} {path} {}\n", hex(addr), match eq.choice { EqChoice::Automatic => "auto", EqChoice::Chosen => "chosen" },
                    if eq.enabled { "on" } else { "off" }, eq.name.replace(['\n', '\r'], " "));
            }
            if let Some(id) = saved.id { out += &format!("id {} {:04x} {:04x} {:04x} {:04x}\n", hex(addr), id.source, id.vendor, id.product, id.version); }
        }
        for (addr, at) in &self.seen { out += &format!("seen {} {at}\n", hex(addr)); }
        out
    }
    fn saved(&self, addr: Addr) -> Saved { self.saved.iter().find(|(a, _)| *a == addr).map(|(_, s)| s.clone()).unwrap_or_default() }
    fn saved_mut(&mut self, addr: Addr) -> &mut Saved {
        let at = match self.saved.iter().position(|(a, _)| *a == addr) { Some(at) => at, None => { self.saved.push((addr, Saved::default())); self.saved.len() - 1 } };
        &mut self.saved[at].1
    }
    fn forget(&mut self, addr: Addr) {
        self.devices.retain(|d| d.addr != addr); self.keys.retain(|k| k.addr != addr);
        self.saved.retain(|(a, _)| *a != addr); self.seen.retain(|s| s.0 != addr);
        self.last = self.last.filter(|last| *last != addr);
    }
    fn load(_blocking: &Blocking, path: &Path) -> Self { std::fs::read_to_string(path).map(|text| Self::parse(&text)).unwrap_or_default() }
    /// Link keys are secrets: the file is readable by its owner only and replaced whole.
    fn save(&self, _blocking: &Blocking, path: &Path) -> io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
        let temporary = path.with_extension("tmp");
        let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&temporary)?;
        file.write_all(self.text().as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    }
}

/// How long a request the controller cannot take yet waits (a scan or connection during a
/// connection attempt: a page is at most `PAGE_LIMIT_MS`, pairing `SECURE_LIMIT_MS`).
const REQUEST_WAIT: Duration = Duration::from_secs(45);
/// How long `Bluetooth::discover` keeps searching.
pub const DISCOVERY: Duration = Duration::from_secs(120);
/// The pause between one inquiry round of a discovery and the next.
const DISCOVERY_PAUSE: Duration = Duration::from_secs(1);

enum Request { Scan, Connect(Addr), ConnectSink(Addr), Disconnect, Forget(Addr), Raw(u16, Vec<u8>), Codecs(Vec<Kind>),
    /// The Wi-Fi chip powered up again: check the controller (`Bluetooth::wifi_powered_up`).
    WifiPowered,
    /// Search until this time (`Bluetooth::discover`), or stop searching.
    Discover(Option<Instant>),
    LimitVolume(u8), Volume(Addr, u8), DeviceEq(Addr, Option<DeviceEq>) }

impl Request {
    /// A scan or a connection: what the listener waits to see happen.
    fn awaited(&self) -> bool { matches!(self, Request::Scan | Request::Connect(_) | Request::ConnectSink(_) | Request::Discover(Some(_))) }
}

/// What one bring-up of the controller hands to the next (a controller lost after a Wi-Fi
/// power-up is brought up again): requests not yet taken, and the search asked for.
#[derive(Default)]
struct Carry { pending: VecDeque<(Request, Option<Instant>)>, discover_until: Option<Instant>,
    /// The index in [`uart_settings`] of the next bring-up's UART setting.
    uart_step: usize,
    /// The setting of the bring-up that failed before this one: the controller may still run at it.
    uart_previous: Option<(u32, bool)> }

type Callback<T> = Arc<dyn Fn(T) + Send + Sync>;

struct Shared {
    state: Mutex<State>,
    /// Requests in order, until the thread takes them; one the controller could not take yet
    /// carries when it was first held.
    requests: Mutex<VecDeque<(Request, Option<Instant>)>>,
    /// Counts sessions; see `Session`.
    epoch: AtomicU32,
    playing: AtomicBool,
    volume_limit: AtomicU8,
    volume_revision: AtomicU32,
    /// The last value given to `output`.
    routed: AtomicBool,
    notify: Arc<dyn Fn() + Send + Sync>,
    /// Told whether a stream is open, so the player routes to it.
    output: Mutex<Option<Callback<bool>>>,
    buttons: Mutex<Option<Callback<Button>>>,
    /// A preference chosen in Settings after start; replaces the configured one for later sessions too.
    codecs: Mutex<Option<Vec<Kind>>>,
    endpoint: Arc<pcm::Endpoint>,
    /// Counts `update`s, so the status file is rewritten only after a change.
    revision: AtomicU32,
    /// The index in [`uart_settings`] of the setting that last reached Ready: later starts
    /// use it at once. And how many bring-ups failed and were tried at the next setting.
    uart_step: AtomicU32,
    uart_fallbacks: AtomicU32,
}

impl Shared {
    fn next_volume_revision(&self) -> u32 { self.volume_revision.fetch_add(1, Ordering::Relaxed) }
    fn update(&self, change: impl FnOnce(&mut State)) {
        change(&mut self.state.lock().unwrap());
        self.revision.fetch_add(1, Ordering::Release);
        (self.notify)();
    }
    fn output(&self, open: bool) {
        // The route changes before writes start failing, so the player sees a route change first.
        if !open { self.emit_output(false); }
        self.endpoint.set_available(open);
        if open { self.emit_output(true); }
    }
    fn emit_output(&self, open: bool) {
        if self.routed.swap(open, Ordering::AcqRel) == open { return }
        let output = self.output.lock().unwrap().clone();
        if let Some(output) = output { output(open); }
    }
}

struct Worker { thread: std::thread::JoinHandle<()>, wake: PipeWriter, stop: Arc<AtomicBool> }

/// One run of the thread. What a retired session publishes late is dropped.
struct Session { shared: Arc<Shared>, epoch: u32, stop: Arc<AtomicBool> }
impl Session {
    fn update(&self, change: impl FnOnce(&mut State)) {
        if self.shared.epoch.load(Ordering::Acquire) == self.epoch { self.shared.update(change); }
    }
}

/// PCM stays attenuated until the headphones report a level safe for the current handoff.
#[derive(Default)]
struct Amplifier {
    level: Option<u8>,
    /// A quiet restore or rejected ceiling still awaiting a sufficiently low hardware step.
    waiting_for: Option<u8>,
    remote: bool,
}

impl Amplifier {
    fn follow(&mut self, session: &Session, cap: u8) {
        let ceiling = self.waiting_for.unwrap_or(cap).min(cap);
        let safe = self.level.is_some_and(|raw| raw <= to_level(ceiling));
        self.waiting_for = (!safe).then_some(ceiling);
        if safe != self.remote {
            self.remote = safe;
            session.update(|state| if let Some(output) = &mut state.output { output.absolute = safe; });
        }
    }
}

/// Commit player and headphone changes through the same revision so either input can win in order.
fn commit_volume(session: &Session, store: &mut Store, addr: Addr, volume: u8, save_at: &mut Option<Instant>) {
    let saved = store.saved_mut(addr);
    if saved.volume == Some(volume) { return }
    saved.volume = Some(volume);
    *save_at = Some(Instant::now() + VOLUME_SAVE_DELAY);
    session.update(|state| if let Some(output) = state.output.as_mut().filter(|o| o.addr == addr) {
        output.saved.volume = Some(volume);
        output.volume_revision = session.shared.next_volume_revision();
    });
}

/// The handle the UI and main hold.
pub struct Bluetooth {
    options: Options,
    shared: Arc<Shared>,
    worker: Mutex<Option<Worker>>,
    /// A stopped thread still closing its link; the next start or `shutdown` joins it.
    retiring: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl Bluetooth {
    pub fn new(options: Options, notify: Arc<dyn Fn() + Send + Sync>) -> Self { Self::with_endpoint(options, notify, pcm::endpoint().clone()) }
    fn with_endpoint(options: Options, notify: Arc<dyn Fn() + Send + Sync>, endpoint: Arc<pcm::Endpoint>) -> Self {
        // The paired list is read by the thread when Bluetooth is switched on.
        let shared = Arc::new(Shared { state: Mutex::new(State::default()), requests: Mutex::default(), epoch: AtomicU32::new(0),
            playing: AtomicBool::new(false), volume_limit: AtomicU8::new(100), volume_revision: AtomicU32::new(0), routed: AtomicBool::new(false), notify, output: Mutex::new(None), buttons: Mutex::new(None), codecs: Mutex::new(None), endpoint, revision: AtomicU32::new(0),
            uart_step: AtomicU32::new(0), uart_fallbacks: AtomicU32::new(0) });
        Self { options, shared, worker: Mutex::new(None), retiring: Mutex::new(None) }
    }
    pub fn state(&self) -> State { self.shared.state.lock().unwrap().clone() }
    pub fn set_output(&self, output: Callback<bool>) { *self.shared.output.lock().unwrap() = Some(output); }
    pub fn set_buttons(&self, buttons: Callback<Button>) { *self.shared.buttons.lock().unwrap() = Some(buttons); }
    /// Whether music is playing, for the headphones' status queries.
    pub fn set_playing(&self, playing: bool) { if self.shared.playing.swap(playing, Ordering::Relaxed) != playing { self.wake(); } }

    fn wake(&self) { if let Some(worker) = &*self.worker.lock().unwrap() { uart::signal(&worker.wake); } }
    fn request(&self, request: Request) { self.shared.requests.lock().unwrap().push_back((request, None)); self.wake(); }
    pub fn scan(&self) { self.request(Request::Scan) }
    pub fn connect(&self, addr: Addr) { self.request(Request::Connect(addr)) }
    /// Pairs and connects only if the device's SDP records show an A2DP sink; for a
    /// device that is not of the audio class. A refusal sets the notice.
    pub fn connect_checked(&self, addr: Addr) { self.request(Request::ConnectSink(addr)) }
    /// Disconnects, or ends a connection attempt.
    pub fn disconnect(&self) { self.request(Request::Disconnect) }
    /// Searches for devices in pairing mode for [`DISCOVERY`], one inquiry after another, so a
    /// device put in pairing mode late or missed by one inquiry still appears. Asked while the
    /// controller starts, the search begins when it is ready; a connection attempt pauses it.
    pub fn discover(&self) { self.shared.update(|state| state.discovering = true); self.request(Request::Discover(Some(Instant::now() + DISCOVERY))); }
    pub fn stop_discovery(&self) { self.request(Request::Discover(None)); }
    /// After a failed start: starts the controller again, as switching off and on does.
    pub fn retry(&self) {
        if !matches!(self.state().phase, Phase::Failed(_)) { return }
        self.set_enabled(false);
        self.set_enabled(true);
    }
    pub fn forget(&self, addr: Addr) { self.request(Request::Forget(addr)) }
    /// The codec preference for the next connection (Settings; `codecs` maps the setting).
    pub fn set_codecs(&self, codecs: Vec<Kind>) { *self.shared.codecs.lock().unwrap() = Some(codecs.clone()); self.request(Request::Codecs(codecs)) }
    /// The player's volume for a device (0..=100): kept in the store, and sent to the
    /// headphones' amplifier when they take absolute volume.
    pub fn set_volume(&self, addr: Addr, volume: u8) { self.request(Request::Volume(addr, volume.min(self.shared.volume_limit.load(Ordering::Acquire)))) }
    /// Applies immediately to later requests and reconnects, including when the controller is off.
    pub fn set_volume_limit(&self, limit: u8) {
        let limit = limit.clamp(1, 100);
        if limit != self.shared.volume_limit.swap(limit, Ordering::AcqRel) { self.request(Request::LimitVolume(limit)); }
    }
    /// A device's headphone EQ choice; `None` forgets it, so the player matches one again.
    pub fn set_device_eq(&self, addr: Addr, eq: Option<DeviceEq>) { self.request(Request::DeviceEq(addr, eq)) }
    /// Sends one HCI command for diagnosis (`--bt-probe diag`); the reply appears in `State::diagnostics`.
    pub fn raw(&self, opcode: u16, parameters: Vec<u8>) { self.request(Request::Raw(opcode, parameters)) }
    /// The Wi-Fi chip has powered up again after being off (a radio doze, Wi-Fi Off and
    /// On, Offline Mode). The thread asks the controller for its version: one that does
    /// not answer lost its power with the Wi-Fi side and is brought up again; one that
    /// answers, with no headphones connected, runs its start-up commands again so it
    /// starts over beside the new Wi-Fi driver instance (`Options::resync`). Connected
    /// headphones are left alone. Nothing happens while Bluetooth is off: a start clears
    /// the requests queued before it.
    pub fn wifi_powered_up(&self) { self.request(Request::WifiPowered) }

    /// Starts or stops the controller. Neither waits: a stopping thread
    /// disconnects and closes the port on its own (the UI calls this), and a
    /// new thread waits for it before opening the port.
    pub fn set_enabled(&self, enabled: bool) {
        let mut worker = self.worker.lock().unwrap();
        if self.options.port == Port::Absent {
            self.shared.update(|state| state.phase = if enabled { Phase::Failed("Bluetooth is not available here".into()) } else { Phase::Off });
            return;
        }
        if enabled == worker.is_some() { return }
        if let Some(old) = worker.take() {
            old.stop.store(true, Ordering::Release);
            uart::signal(&old.wake);
            self.shared.epoch.fetch_add(1, Ordering::AcqRel);
            self.shared.endpoint.set_wake(None);
            *self.retiring.lock().unwrap() = Some(old.thread);
            self.shared.update(|state| { *state = State { paired: std::mem::take(&mut state.paired), ..State::default() }; });
            return;
        }
        let Ok((woken, wake)) = uart::wake_pipe() else { return };
        self.shared.requests.lock().unwrap().clear();
        if let Ok(waker) = wake.try_clone() { self.shared.endpoint.set_wake(Some(Arc::new(move || uart::signal(&waker)))); }
        let stop = Arc::new(AtomicBool::new(false));
        let session = Session { shared: self.shared.clone(), epoch: self.shared.epoch.fetch_add(1, Ordering::AcqRel) + 1, stop: stop.clone() };
        self.shared.update(|state| state.phase = Phase::Starting);
        let (options, predecessor) = (self.options.clone(), self.retiring.lock().unwrap().take());
        let spawned = std::thread::Builder::new().name("bluetooth".into()).spawn(move || thread_main(&session, &options, &woken, predecessor));
        match spawned {
            Ok(thread) => *worker = Some(Worker { thread, wake, stop }),
            Err(error) => self.shared.update(|state| state.phase = Phase::Failed(format!("Bluetooth could not start: {error}"))),
        }
    }

    /// Stops the controller and waits until its thread has closed the port (process exit, tests).
    pub fn shutdown(&self) {
        self.set_enabled(false);
        let retiring = self.retiring.lock().unwrap().take();
        if let Some(thread) = retiring { let _ = thread.join(); }
    }
}

impl Drop for Bluetooth {
    fn drop(&mut self) { self.shutdown(); }
}

/// The thread's nice value. It is the A2DP stream's clock: while it does not run, nothing is
/// sent (Product feedback, 2026-11-22: "Please do increase the bt thread's priority."). docs/bluetooth.md,
/// "Thread priority".
const THREAD_NICE: i32 = -10;
/// Whether the priority's outcome has been logged: once per process, not per start.
static PRIORITY_LOGGED: AtomicBool = AtomicBool::new(false);

/// Raises the calling (Bluetooth) thread to [`THREAD_NICE`]. Without the privilege it stays at
/// the nice value it was created with, and the stream runs as before.
fn raise_priority() {
    let result = crate::platform::system::set_thread_nice(THREAD_NICE);
    if PRIORITY_LOGGED.swap(true, Ordering::Relaxed) { return }
    match result {
        Ok(()) => log::info!("Bluetooth: thread at nice {THREAD_NICE}"),
        Err(error) => log::warn!("Bluetooth: thread priority not raised to nice {THREAD_NICE}: {error}"),
    }
}

/// The bluetooth thread: blocking is its own business.
fn thread_main(session: &Session, options: &Options, woken: &PipeReader, predecessor: Option<std::thread::JoinHandle<()>>) {
    let blocking = Blocking::dedicated_thread();
    raise_priority();
    // The port has one owner: the previous thread must have closed it.
    if let Some(predecessor) = predecessor { let _ = predecessor.join(); }
    let (mut written, mut carry) = (None, Carry { uart_step: session.shared.uart_step.load(Ordering::Acquire) as usize, ..Carry::default() });
    loop {
        let exit = run(&blocking, session, options, woken, &mut written, &mut carry);
        if session.stop.load(Ordering::Acquire) { break }
        match exit {
            // A controller lost under a ready session (it shared the Wi-Fi chip's power) is brought up again.
            Exit::Restart => log::info!("Bluetooth: the controller did not answer after Wi-Fi powered up; starting it again"),
            // The UART setting failed at bring-up: again at the next one.
            Exit::Fallback => {}
            Exit::Stopped | Exit::Failed => break,
        }
    }
    telemetry::off();
    // The last line: off or failed. A newer session waits for this thread before it writes.
    write_status(&blocking, session, options, &mut written);
}

/// Pair New Device drops the connections still waiting from before it was asked for: the
/// listener looking for a new device does not want an older attempt to start after it
/// (Product feedback, 2026-11-22). Connections asked for after it, the pairing among them, stay.
fn drop_connects_before_search(pending: &mut VecDeque<(Request, Option<Instant>)>) {
    let Some(search) = pending.iter().rposition(|(r, _)| matches!(r, Request::Discover(Some(_)))) else { return };
    let mut index = 0;
    pending.retain(|(r, _)| { index += 1; index > search || !matches!(r, Request::Connect(_) | Request::ConnectSink(_)) });
}

/// How one bring-up of the controller ended.
#[derive(Debug, PartialEq, Eq)]
enum Exit { Stopped, Failed, Restart, Fallback }

/// One line for `options.status_file`: no addresses or names.
fn status_line(state: &State) -> String {
    let phase = match &state.phase { Phase::Off => "off", Phase::Starting => "starting", Phase::Ready => "ready", Phase::Failed(_) => "failed" };
    let link = match state.link { Link::Idle => "idle", Link::Connecting(_) => "connecting", Link::Connected(_) => "connected", Link::Playing(_) => "playing" };
    let stream = state.stream.map_or("none", |s| s.codec.kind().name());
    let r = state.resync;
    let d = &state.diagnostics;
    format!("phase={phase} link={link} stream={stream} scanning={} patched={} wifi_checks={} alive={} lost={} reinits={} busy={} h5_retransmissions={} h5_rejected={} command_timeouts={} dropped_packets={} underruns={}\n",
        u8::from(state.scanning), state.controller.map_or(0, |v| u8::from(v.already_patched || v.rom_version.is_some())),
        r.checks, r.alive, r.lost, r.reinits, r.busy, d.h5_retransmissions, d.h5_rejected, d.command_timeouts, state.counters.0, state.counters.2)
}

/// Rewrites the status file when the state changed since `written` (a revision of `Shared`).
fn write_status(_blocking: &Blocking, session: &Session, options: &Options, written: &mut Option<u32>) {
    let Some(path) = options.status_file.as_deref() else { return };
    let revision = session.shared.revision.load(Ordering::Acquire);
    if *written == Some(revision) { return }
    *written = Some(revision);
    let line = status_line(&session.shared.state.lock().unwrap());
    let temporary = path.with_extension("tmp");
    if std::fs::write(&temporary, line).and_then(|()| std::fs::rename(&temporary, path)).is_err() { let _ = std::fs::remove_file(&temporary); }
}

/// The byte path to the controller.
trait Wire {
    fn readable(&self) -> std::os::fd::BorrowedFd<'_>;
    /// Zero when nothing has arrived.
    fn read(&mut self, blocking: &Blocking, buffer: &mut [u8]) -> io::Result<usize>;
    fn write(&mut self, blocking: &Blocking, bytes: &[u8]) -> io::Result<()>;
    fn retune(&mut self, baud: u32, flow: bool) -> io::Result<()>;
    /// The kernel's receive error counts, where the port has them.
    fn errors(&mut self, _blocking: &Blocking) -> Option<uart::Errors> { None }
    /// Whether the port can run at `baud`.
    fn accepts(&mut self, _baud: u32) -> bool { true }
}

impl Wire for uart::Uart {
    fn readable(&self) -> std::os::fd::BorrowedFd<'_> { self.file().as_fd() }
    fn read(&mut self, blocking: &Blocking, buffer: &mut [u8]) -> io::Result<usize> { uart::Uart::read(self, blocking, buffer) }
    fn write(&mut self, blocking: &Blocking, bytes: &[u8]) -> io::Result<()> { uart::write_all(blocking, self.file(), bytes) }
    fn retune(&mut self, baud: u32, flow: bool) -> io::Result<()> { self.configure(baud, flow).map(|_| ()) }
    fn errors(&mut self, blocking: &Blocking) -> Option<uart::Errors> { uart::Uart::line_state(self, blocking).ok().map(|l| l.errors) }
    fn accepts(&mut self, baud: u32) -> bool { uart::Uart::accepts(self, baud) }
}

/// Frames on the fake controller's pipes: the sender's UART rate, a length, the bytes.
fn frame(baud: u32, bytes: &[u8]) -> Vec<u8> {
    let mut out = baud.to_le_bytes().to_vec();
    out.extend_from_slice(&(bytes.len() as u16).to_le_bytes());
    out.extend_from_slice(bytes);
    out
}
/// Removes one whole frame from the front of `pending`.
fn unframe(pending: &mut Vec<u8>) -> Option<(u32, Vec<u8>)> {
    let len = u16::from_le_bytes(pending.get(4..6)?.try_into().ok()?) as usize;
    if pending.len() < 6 + len { return None }
    let baud = u32::from_le_bytes(pending[..4].try_into().unwrap());
    let bytes = pending[6..6 + len].to_vec();
    pending.drain(..6 + len);
    Some((baud, bytes))
}

/// The fake controller on its own thread, behind two pipes.
struct FakeWire { from_fake: PipeReader, to_fake: PipeWriter, pending: Vec<u8>, baud: u32, flow: bool, broken: Option<fn(u32, bool) -> bool>,
    accepts: Option<fn(u32) -> bool>, stop: Arc<AtomicBool>, thread: Option<std::thread::JoinHandle<()>> }

/// The fake controller's thread.
/// Test hooks of the fake: its first headphones' power switch, presses of their volume-up
/// key asked for, a controller power cycle, a setup, UART settings whose line carries garbage,
/// the rates the port takes, a controller kept from the bring-up before (tests only).
#[derive(Default)]
struct FakeHooks { off: Arc<AtomicBool>, keys: Arc<AtomicU32>, power: Arc<AtomicBool>, setup: Option<fn(&mut q2_bt::fake::Fake)>,
    uart_broken: Option<fn(u32, bool) -> bool>, uart_accepts: Option<fn(u32) -> bool>,
    #[cfg(test)] kept: Option<KeptFake> }

fn fake_main(from_host: PipeReader, to_host: PipeWriter, stopped: Arc<AtomicBool>, hooks: FakeHooks) {
    #[cfg(test)]
    let kept = hooks.kept.as_ref().and_then(KeptFake::take);
    #[cfg(not(test))]
    let kept = None;
    let (mut fake, start) = kept.unwrap_or_else(|| (fake_controller(&hooks), Instant::now()));
    fake_loop(&Blocking::dedicated_thread(), &from_host, &to_host, &stopped, &hooks, &mut fake, start);
    #[cfg(test)]
    if let Some(kept) = &hooks.kept { kept.keep(fake, start); }
}
fn fake_controller(hooks: &FakeHooks) -> q2_bt::fake::Fake {
    let mut fake = q2_bt::fake::Fake::new(0);
    // The fixture headphones have every codec, absolute volume and a Device ID record
    // (a vendor no brand has); the earbuds have SBC only and no absolute volume; the computer is
    // not of the audio class and has an A2DP sink (SBC and AAC) that only its SDP records show.
    fake.devices[0].codecs = [Kind::Aac, Kind::AptX, Kind::AptXHd, Kind::Ldac].map(q2_bt::codec::capabilities).to_vec();
    fake.devices[0].absolute_volume = true;
    fake.devices[0].device_id = Some(DeviceId { source: 1, vendor: 0x7ffe, product: 0x0001, version: 0x0100 });
    let mut second = q2_bt::fake::Headphones::new(0x12, "Fixture Earbuds");
    second.named_in_inquiry = false;
    fake.devices.push(second);
    let mut computer = q2_bt::fake::Headphones::computer(0x13, "Fixture Computer");
    computer.codecs = vec![q2_bt::codec::capabilities(Kind::Aac)];
    fake.devices.push(computer);
    // The simulator and emulator run at about a real controller's pace (a start of about a
    // second, a page and a pairing step of most of a second each), so "Starting…" and each
    // pairing step can be seen; tests run at full speed unless their setup slows them.
    #[cfg(not(test))]
    { fake.bring_up_delay = 40; fake.link_delay = 700; }
    if let Some(setup) = hooks.setup { setup(&mut fake); }
    fake
}
fn fake_loop(blocking: &Blocking, from_host: &PipeReader, to_host: &PipeWriter, stopped: &AtomicBool, hooks: &FakeHooks, fake: &mut q2_bt::fake::Fake, start: Instant) {
    let mut pressed = 0;
    let (mut buffer, mut pending) = (vec![0u8; 8192], Vec::new());
    while !stopped.load(Ordering::Acquire) {
        let _ = uart::wait_readable(blocking, [from_host.as_fd()], 2);
        match uart::read_ready(blocking, from_host, &mut buffer) { Ok(n) => pending.extend_from_slice(&buffer[..n]), Err(_) => return }
        let now = start.elapsed().as_millis() as u64;
        // The switch first: a command sent after the switch was flipped finds the headphones as switched.
        let on = !hooks.off.load(Ordering::Acquire);
        if on != fake.devices[0].powered { fake.devices[0].powered = on; if !on { fake.drop_link(); } }
        while let Some((baud, bytes)) = unframe(&mut pending) { fake.rx(&bytes, baud, now); }
        #[cfg(test)]
        if let Some(kept) = &hooks.kept { kept.volume_sets.store(fake.log.volumes_set.len() as u32, Ordering::Release); }
        if hooks.keys.load(Ordering::Acquire) > pressed && fake.volume_button(true) { pressed += 1; }
        if hooks.power.swap(false, Ordering::AcqRel) { fake.power_cycle(now); }
        let mut wire = Vec::new();
        fake.poll(now, &mut wire);
        for chunk in wire.chunks(4096) { if (&*to_host).write_all(&frame(fake.tx_baud(), chunk)).is_err() { return } }
    }
}

impl FakeWire {
    fn start(_blocking: &Blocking, hooks: FakeHooks) -> io::Result<Self> {
        let (from_host, to_fake) = uart::data_pipe()?;
        let (from_fake, to_host) = uart::data_pipe()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let (broken, accepts) = (hooks.uart_broken, hooks.uart_accepts);
        let thread = std::thread::Builder::new().name("bluetooth-fake".into()).spawn(move || fake_main(from_host, to_host, stopped, hooks))?;
        Ok(Self { from_fake, to_fake, pending: Vec::new(), baud: q2_bt::host::INITIAL_BAUD, flow: false, broken, accepts, stop, thread: Some(thread) })
    }
    /// The rate the line runs at: ours, or (a setting the test breaks) one the fake does not hear.
    fn line(&self) -> u32 { if self.broken.is_some_and(|broken| broken(self.baud, self.flow)) { self.baud ^ 1 } else { self.baud } }
}
impl Wire for FakeWire {
    fn readable(&self) -> std::os::fd::BorrowedFd<'_> { self.from_fake.as_fd() }
    fn read(&mut self, blocking: &Blocking, buffer: &mut [u8]) -> io::Result<usize> {
        let mut chunk = [0u8; 4096];
        let n = uart::read_ready(blocking, &self.from_fake, &mut chunk)?;
        self.pending.extend_from_slice(&chunk[..n]);
        while let Some((baud, bytes)) = unframe(&mut self.pending) {
            // Bytes sent at another rate than ours are line noise; a real UART would deliver garbage.
            if baud != self.line() { continue }
            let n = bytes.len().min(buffer.len());
            buffer[..n].copy_from_slice(&bytes[..n]);
            return Ok(n);
        }
        Ok(0)
    }
    fn write(&mut self, _blocking: &Blocking, bytes: &[u8]) -> io::Result<()> {
        let line = self.line();
        for chunk in bytes.chunks(4096) { (&self.to_fake).write_all(&frame(line, chunk))?; }
        Ok(())
    }
    fn retune(&mut self, baud: u32, flow: bool) -> io::Result<()> { self.baud = baud; self.flow = flow; Ok(()) }
    fn accepts(&mut self, baud: u32) -> bool { self.accepts.is_none_or(|accepts| accepts(baud)) }
}
impl Drop for FakeWire {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() { let _ = thread.join(); }
    }
}

/// The combo chip's SDIO host: its driver binding powers both radios' enables (device.md).
const WIFI_SLOT: &str = "/sys/bus/platform/devices/13460000.mmc";

fn firmware(_blocking: &Blocking, options: &Options) -> Option<(Vec<u8>, Vec<u8>)> {
    options.firmware_dirs.iter().find_map(|dir| Some((std::fs::read(dir.join("rtl8733bs_fw")).ok()?, std::fs::read(dir.join("rtl8733bs_config")).ok()?)))
}

/// Powers the controller where the kernel offers the hold, and opens the port.
fn open(blocking: &Blocking, options: &Options) -> Result<(Box<dyn Wire>, Option<(Vec<u8>, Vec<u8>)>), String> {
    match &options.port {
        Port::Absent => Err("Bluetooth is not available here".into()),
        Port::Fake => {
            #[cfg(test)]
            let hooks = FakeHooks { off: options.fake_off.clone(), keys: options.fake_keys.clone(), power: options.fake_power_cycle.clone(), setup: options.fake_setup,
                uart_broken: options.fake_uart_broken, uart_accepts: options.fake_uart_accepts, kept: options.fake_kept.clone() };
            #[cfg(not(test))]
            let hooks = FakeHooks::default();
            Ok((Box::new(FakeWire::start(blocking, hooks).map_err(|e| e.to_string())?), Some((q2_bt::rtl::fixture::firmware(&[0, 2], 3000), q2_bt::rtl::fixture::config()))))
        }
        Port::Tty(path) => {
            if let Some(hold) = options.power_hold.as_deref().filter(|hold| hold.exists()) {
                std::fs::write(hold, "enabled").map_err(|error| format!("Bluetooth power: {error}"))?;
                // BT_DIS_N high to a responsive ROM: the stock driver allows 200 ms.
                std::thread::sleep(Duration::from_millis(200));
            } else if Path::new(WIFI_SLOT).exists() && !Path::new(WIFI_SLOT).join("driver").exists() {
                // Without the hold, BT_DIS_N is up only while the Wi-Fi slot is powered.
                return Err("Turn Wi-Fi on to use Bluetooth".into());
            }
            let uart = uart::Uart::open(blocking, path).map_err(|error| format!("Bluetooth port {}: {error}", path.display()))?;
            Ok((Box::new(uart), firmware(blocking, options)))
        }
    }
}

/// The stock firmware's rate (Realtek code 0x04928001; the stock configuration sets the flow
/// bit). The X2000's UART reaches it with UMR 8 (kernel patches 0191 and 0192); the line runs at
/// 2.70 Mbaud there, and the controller comes up with RTS/CTS (device-verified 2026-11-22).
pub const FAST_BAUD: u32 = 3_000_000;
/// The rate the 24 MHz UART clock gives exactly with the 16x divisor (rate code 0x04928002).
/// Device-verified with and without RTS/CTS on 2026-11-22.
pub const SAFE_BAUD: u32 = 1_500_000;
/// `uart_mode = "auto"`'s UART settings (rate, RTS/CTS), in the order a failed bring-up tries them.
pub const AUTO_UART: [(u32, bool); 3] = [(FAST_BAUD, true), (SAFE_BAUD, true), (SAFE_BAUD, false)];

/// The UART settings a start tries, in order: [`AUTO_UART`] with `uart_auto`; otherwise the
/// configured one, then 1.5 Mbaud without flow control. A probe that names a rate or flow
/// control (`uart_fallback` off) tries only its own.
fn uart_settings(options: &Options) -> Vec<(u32, bool)> {
    let mut settings = if options.uart_auto { AUTO_UART.to_vec() } else { vec![(options.baud, options.flow), (SAFE_BAUD, false)] };
    settings.dedup();
    if !options.uart_fallback { settings.truncate(1); }
    settings
}

/// Drops the controller's power hold. The controller resets to its ROM (and 115200 baud) only
/// when nothing else holds BT_DIS_N up, that is with the Wi-Fi slot powered down.
fn release_power(_blocking: &Blocking, options: &Options, settle: Duration) {
    let Port::Tty(_) = options.port else { return };
    if let Some(hold) = options.power_hold.as_deref().filter(|hold| hold.exists()) {
        let _ = std::fs::write(hold, "disabled");
        if !settle.is_zero() { std::thread::sleep(settle); }
    }
}

fn run(blocking: &Blocking, session: &Session, options: &Options, woken: &PipeReader, written: &mut Option<u32>, carry: &mut Carry) -> Exit {
    let shared = &*session.shared;
    let failed = |message: String| {
        log::warn!("Bluetooth: {message}");
        session.update(|state| {
            state.last_error.clone_from(&message); state.phase = Phase::Failed(message); state.link = Link::Idle; state.step = None; state.scanning = false;
            state.stream = None; state.output = None;
        });
        Exit::Failed
    };
    let (mut wire, files) = match open(blocking, options) { Ok(opened) => opened, Err(message) => return failed(message) };
    if files.is_none() { log::warn!("Bluetooth: no firmware files; using the controller's ROM"); }
    let mut store = Store::load(blocking, &options.store);
    // `discovering` starts false in each bring-up; the thread sets it when it starts searching.
    session.update(|state| { state.paired = store.devices.clone(); state.seen = store.seen.clone(); state.ldac = Some(options.ldac); state.discovering = false; });
    let (firmware, config) = files.map_or((None, None), |(firmware, config)| (Some(firmware), Some(config)));
    let ladder = uart_settings(options);
    carry.uart_step = carry.uart_step.min(ladder.len() - 1);
    // A rate the kernel refuses is not tried. In auto mode that is a kernel without 3 Mbaud, and
    // not an error; a configured rate the kernel refuses counts as a fallback.
    while carry.uart_step + 1 < ladder.len() && ladder[carry.uart_step].0 != SAFE_BAUD && !wire.accepts(ladder[carry.uart_step].0) {
        let ((refused, _), (baud, flow)) = (ladder[carry.uart_step], ladder[carry.uart_step + 1]);
        if options.uart_auto { log::info!("Bluetooth: the UART does not take {refused} baud; using {baud} baud, flow control {flow}"); } else {
            log::warn!("Bluetooth: the UART does not take {refused} baud; using {baud} baud, flow control {flow}");
            shared.uart_fallbacks.fetch_add(1, Ordering::Relaxed);
        }
        carry.uart_step += 1;
    }
    let (step, (baud, flow)) = (carry.uart_step, ladder[carry.uart_step]);
    log::info!("Bluetooth: UART {baud} baud, flow control {flow}");
    session.update(|state| state.uart = (baud, flow));
    // A failed bring-up at this setting is tried at the next.
    let next = ladder.get(step + 1).copied();
    let original = options;
    let options = &Options { baud, flow, ..options.clone() };
    // Whether this bring-up reached Ready: later failures are not the UART setting's.
    let mut was_ready = false;
    let fall_back = |carry: &mut Carry, wire: Box<dyn Wire>, message: String, (next_baud, next_flow): (u32, bool)| {
        log::warn!("Bluetooth: bring-up at {baud} baud, flow control {flow} failed ({message}); trying {next_baud} baud, flow control {next_flow}");
        shared.output(false);
        drop(wire);
        // Where only the hold keeps it powered (Wi-Fi off), the controller returns to its ROM at
        // 115200 baud. Where the Wi-Fi side powers it too, it stays at the rate it took: the next
        // bring-up's sync also tries this setting and moves it from there (`Settings::previous`).
        release_power(blocking, original, Duration::from_millis(100));
        carry.uart_step = step + 1;
        carry.uart_previous = Some((baud, flow));
        shared.uart_fallbacks.fetch_add(1, Ordering::Relaxed);
        session.update(|state| { state.last_error = message; state.phase = Phase::Starting; state.link = Link::Idle; state.step = None; state.scanning = false; });
        Exit::Fallback
    };
    let start = Instant::now();
    // The codec preference in force, for the host and for the reason shown with a stream.
    let mut preference = shared.codecs.lock().unwrap().clone().unwrap_or_else(|| options.codecs.clone());
    let mut host = Host::new(Settings { name: options.name.clone(), baud: options.baud, flow: options.flow, previous: carry.uart_previous.take(), firmware, config, keys: store.keys.clone(),
        codecs: preference.clone(), bench_peer: options.bench_peer }, 0);
    let mut media: Option<Media> = None;
    let mut buffer = [0u8; 4096];
    let mut stopping: Option<Instant> = None;
    let mut user_connect: Option<Addr> = None;
    let mut counters = (0, 0, 0);
    let mut last_diagnostics = Diagnostics::default();
    // Retain the configured ceiling across controller restarts.
    let limit = || shared.volume_limit.load(Ordering::Acquire);
    host.set_volume_limit(to_level(limit()));
    let mut amplifier = Amplifier::default();
    let mut save_at: Option<Instant> = None;
    // The address of the encrypted link, as `State::connected` has it.
    let mut linked: Option<Addr> = None;
    let save = |store: &Store| if let Err(error) = store.save(blocking, &options.store) { log::warn!("Bluetooth: paired devices not saved: {error}"); };
    let mut telemetry = telemetry::Telemetry::new(options.baud, options.flow, options.ldac_adaptive, shared.uart_fallbacks.load(Ordering::Relaxed));
    // Start-up commands run again after a Wi-Fi power-up: the Ready that follows does not reconnect.
    let mut resyncing = false;
    // The paired device paged at Ready, and whether a request of the listener cancelled that page.
    let mut auto_connect: Option<Addr> = None;
    let mut cancelled = false;
    // Discovery: on at the last look, a round of ours is open, the next round starts at this time.
    let (mut discovering, mut discovery_scan, mut next_round) = (false, false, None::<Instant>);
    let mut held_count = 0u32;
    // The scan flag last published.
    let mut shown_scanning = false;
    loop {
        if save_at.is_some_and(|at| Instant::now() >= at) { save_at = None; save(&store); }
        write_status(blocking, session, options, written);
        let now_us = start.elapsed().as_micros() as u64;
        let now = now_us / 1000;
        // Stop: disconnect, give the controller a moment to send it, then leave.
        if session.stop.load(Ordering::Acquire) && stopping.is_none() { stopping = Some(Instant::now()); host.command(Command::Disconnect); }
        if stopping.is_some_and(|since| since.elapsed() >= Duration::from_millis(if shared.state.lock().unwrap().link == Link::Idle { 0 } else { 300 })) { break }
        // A stopping session leaves new requests for its successor. The host takes commands only
        // when ready: requests made during bring-up or a re-run of the start-up commands wait.
        // Requests are kept here from when they arrive until the host can take them.
        if stopping.is_none() { carry.pending.extend(std::mem::take(&mut *shared.requests.lock().unwrap())); }
        drop_connects_before_search(&mut carry.pending);
        let requests = if stopping.is_none() && host.ready() { std::mem::take(&mut carry.pending) } else { VecDeque::new() };
        let mut held = VecDeque::new();
        for (request, since) in requests {
            // The newest connection asked for replaces any still waiting; a disconnect or a forget
            // ends the waiting ones (of that device, for a forget).
            match request {
                Request::Connect(_) | Request::ConnectSink(_) | Request::Disconnect => held.retain(|(r, _)| !matches!(r, Request::Connect(_) | Request::ConnectSink(_))),
                Request::Forget(gone) => held.retain(|(r, _)| !matches!(r, Request::Connect(a) | Request::ConnectSink(a) if *a == gone)),
                _ => {}
            }
            // A scan or connection the host cannot start yet waits, and those after it keep their order.
            let ordered = matches!(request, Request::Scan | Request::Connect(_) | Request::ConnectSink(_));
            let blocked = match request {
                Request::Scan => !host.can_scan() && !host.scanning(),
                Request::Connect(addr) | Request::ConnectSink(addr) => !host.can_connect() && host.connected() != Some(addr),
                _ => false,
            };
            // The listener asks for the device the reconnect at start is paging: that page is theirs now.
            if let Request::Connect(addr) = request {
                if auto_connect == Some(addr) && host.paging() == Some(addr) && !cancelled {
                    user_connect = Some(addr); auto_connect = None;
                    session.update(|state| { state.notice.clear(); state.attempt = Some((addr, false)); });
                    continue;
                }
            }
            if blocked || ordered && !held.is_empty() {
                let since = since.unwrap_or_else(Instant::now);
                if since.elapsed() >= REQUEST_WAIT {
                    log::info!("Bluetooth: a request waited {} s for the controller; dropped", REQUEST_WAIT.as_secs());
                    let attempt = match request { Request::Connect(addr) => Some((addr, false)), Request::ConnectSink(addr) => Some((addr, true)), _ => None };
                    session.update(|state| {
                        state.problem("Bluetooth was busy with another connection. Try again.");
                        // Try Again repeats this request, not an older attempt.
                        if attempt.is_some() { state.attempt = attempt; }
                    });
                    continue;
                }
                // An automatic reconnect gives way to what the listener asked for, and is not resumed.
                if auto_connect.is_some() && host.paging() == auto_connect && !cancelled {
                    cancelled = true;
                    host.command(Command::CancelConnect);
                }
                held.push_back((request, Some(since)));
                continue;
            }
            match request {
                Request::Scan => if !host.scanning() {
                    discovery_scan = false;
                    session.update(|state| { state.found.clear(); state.notice.clear(); });
                    host.command(Command::Scan);
                },
                Request::Connect(addr) | Request::ConnectSink(addr) if host.connected() == Some(addr) => {}
                Request::Discover(until) => {
                    // Pair New Device: a connection attempt in progress (the reconnect at start, or
                    // one the listener asked for earlier) ends, and is not resumed when the search
                    // ends (Product feedback, 2026-11-22). A connected device stays connected.
                    if until.is_some() && (user_connect.is_some() || auto_connect.is_some()) {
                        log::info!("Bluetooth: Pair New Device; ending the connection attempt in progress");
                        if host.paging().is_some() { host.command(Command::CancelConnect); }
                        else if host.linked() && host.connected().is_none() { host.command(Command::Disconnect); }
                        user_connect = None; auto_connect = None;
                        session.update(|state| state.attempt = None);
                    }
                    carry.discover_until = until;
                }
                Request::Connect(addr) => { user_connect = Some(addr); session.update(|state| { state.notice.clear(); state.attempt = Some((addr, false)); }); host.command(Command::Connect(addr)); }
                Request::ConnectSink(addr) => { user_connect = Some(addr); session.update(|state| { state.notice.clear(); state.attempt = Some((addr, true)); }); host.command(Command::ConnectSink(addr)); }
                // Disconnecting during a page ends the page; the attempt the listener ended is no problem.
                Request::Disconnect => { user_connect = None; host.command(if host.paging().is_some() { Command::CancelConnect } else { Command::Disconnect }); }
                Request::Raw(opcode, parameters) => host.command(Command::Raw(opcode, parameters)),
                Request::Codecs(codecs) => { preference.clone_from(&codecs); host.set_codecs(codecs) }
                Request::WifiPowered => {
                    session.update(|state| state.resync.checks += 1);
                    host.command(Command::Check);
                }
                Request::Forget(addr) => {
                    host.command(Command::Forget(addr));
                    store.forget(addr);
                    save(&store);
                    session.update(|state| { state.paired = store.devices.clone(); state.seen = store.seen.clone(); state.attempt = state.attempt.filter(|a| a.0 != addr); });
                }
                Request::LimitVolume(cap) => {
                    let cap = cap.min(limit());
                    host.set_volume_limit(to_level(cap));
                    let Some(addr) = linked else { continue };
                    amplifier.follow(session, cap);
                    let too_loud = amplifier.level.is_some_and(|raw| raw > to_level(cap));
                    if let Some(volume) = store.saved(addr).volume.filter(|v| *v > cap || too_loud) {
                        let volume = volume.min(cap);
                        commit_volume(&session, &mut store, addr, volume, &mut save_at);
                        host.command(Command::SetVolume(to_level(volume)));
                    }
                }
                Request::Volume(addr, volume) => {
                    if !store.devices.iter().any(|d| d.addr == addr) { continue }
                    let volume = volume.min(limit());
                    commit_volume(&session, &mut store, addr, volume, &mut save_at);
                    if linked == Some(addr) {
                        if amplifier.waiting_for.is_some() {
                            amplifier.waiting_for = Some(volume);
                            amplifier.follow(session, limit());
                        }
                        host.command(Command::SetVolume(to_level(volume)));
                    }
                }
                Request::DeviceEq(addr, eq) => {
                    if !store.devices.iter().any(|d| d.addr == addr) { continue }
                    store.saved_mut(addr).eq = eq.clone();
                    save(&store);
                    session.update(|state| if let Some(output) = state.output.as_mut().filter(|o| o.addr == addr) { output.saved.eq = eq; });
                }
            }
        }
        for item in held.into_iter().rev() { carry.pending.push_front(item); }
        // Scans and connections not yet taken, including those waiting for a controller that is not ready.
        let count = carry.pending.iter().filter(|(r, _)| matches!(r, Request::Scan | Request::Connect(_) | Request::ConnectSink(_))).count() as u32;
        if count != held_count { held_count = count; session.update(|state| state.waiting = count); }
        // Discovery: rounds of inquiry while the listener looks for a device to pair.
        if stopping.is_none() && host.ready() {
            let wanted = carry.discover_until.is_some_and(|until| Instant::now() < until);
            if wanted != discovering {
                discovering = wanted;
                next_round = None;
                if wanted { session.update(|state| { state.discovering = true; state.found.clear(); state.notice.clear(); }); }
                else {
                    session.update(|state| state.discovering = false);
                    if discovery_scan && host.scanning() { host.command(Command::StopScan); }
                }
            }
            // The reconnect at start gives way to the search, and is not resumed after it.
            if discovering && auto_connect.is_some() && host.paging() == auto_connect && !cancelled {
                cancelled = true;
                host.command(Command::CancelConnect);
            }
            // Not while a device is connected: an inquiry takes air time from its audio.
            if discovering && held_count == 0 && !host.linked() && !host.scanning() && host.can_scan() && next_round.is_none_or(|at| Instant::now() >= at) {
                discovery_scan = true; next_round = None;
                host.command(Command::Scan);
            }
        }
        host.set_playing(shared.playing.load(Ordering::Relaxed));
        let wake_at_us = media.as_mut().and_then(|media| media.tick(&mut host, &shared.endpoint, now_us));
        let mut out = Vec::new();
        let deadline = host.poll(now, &mut out);
        if !out.is_empty() {
            let started = Instant::now();
            if let Err(error) = wire.write(blocking, &out) { return failed(format!("Bluetooth port: {error}")) }
            telemetry.wrote(started.elapsed().as_millis() as u64);
        }
        let mut flush_volume = false;
        for event in host.events().collect::<Vec<_>>() {
            match event {
                Event::Uart { baud, flow } => if let Err(error) = wire.retune(baud, flow) {
                    let message = format!("Bluetooth port at {baud} baud: {error}");
                    if let Some(next) = next.filter(|_| !was_ready) { return fall_back(carry, wire, message, next) }
                    return failed(message)
                },
                Event::Controller(version) => {
                    log::info!("Bluetooth controller: HCI {:#04x} revision {:#06x}, LMP {:#04x} subversion {:#06x}, manufacturer {:#06x}, ROM {:?}, patch kept {}",
                        version.hci_version, version.hci_revision, version.lmp_version, version.lmp_subversion, version.manufacturer, version.rom_version, version.already_patched);
                    session.update(|state| state.controller = Some(version));
                }
                Event::Ready { addr } => {
                    session.update(|state| {
                        state.phase = Phase::Ready;
                        state.bench_local_address = options.bench_peer.map(|_| addr);
                    });
                    was_ready = true;
                    // The setting that reached Ready is used at once by later starts of this player.
                    shared.uart_step.store(step as u32, Ordering::Release);
                    // After a resync the headphones were not connected: they page us when they want to be.
                    // Nor when the listener has already asked for a scan or a connection.
                    let asked = carry.pending.iter().any(|(r, _)| r.awaited()) || carry.discover_until.is_some_and(|until| Instant::now() < until);
                    if let Some(last) = store.last.filter(|_| options.reconnect && !resyncing && !asked) { auto_connect = Some(last); cancelled = false; host.command(Command::Connect(last)); }
                    resyncing = false;
                }
                Event::Alive => {
                    // No link, connection attempt, scan or check: nothing a reset would cut.
                    let idle = host.idle() && user_connect.is_none();
                    if options.resync && idle {
                        log::info!("Bluetooth: Wi-Fi powered up; running the controller's start-up commands again");
                        resyncing = true;
                        host.command(Command::Reinit);
                    }
                    session.update(|state| { state.resync.alive += 1; if idle && options.resync { state.resync.reinits += 1; } else if !idle { state.resync.busy += 1; } });
                }
                Event::Lost => {
                    // BT_DIS_N dropped with the Wi-Fi side: the controller is back in its ROM.
                    log::warn!("Bluetooth: the controller did not answer after Wi-Fi powered up");
                    shared.output(false);
                    // Volume changes not yet written are kept before this bring-up's state is dropped.
                    if save_at.take().is_some() { save(&store); }
                    session.update(|state| { state.resync.lost += 1; state.phase = Phase::Starting; state.link = Link::Idle; state.step = None; state.scanning = false; state.stream = None; state.output = None; });
                    return Exit::Restart;
                }
                Event::Failed(message) => {
                    // Before Ready: the H5 sync, a command the controller did not answer, or the rate it
                    // refused. Bluetooth is not lost to a UART setting (2026-11-22).
                    if let Some(next) = next.filter(|_| !was_ready) { return fall_back(carry, wire, message, next) }
                    shared.output(false); return failed(message)
                }
                Event::Found { addr, class, name, rssi, sink } => session.update(|state| {
                    let name = name.unwrap_or_else(|| "Unnamed Device".into());
                    match state.found.iter_mut().find(|d| d.addr == addr) {
                        Some(known) => { known.name = name; known.sink |= sink; }
                        None => state.found.push(Device { addr, name, class, rssi, sink }),
                    }
                }),
                Event::ScanFinished => {
                    if discovering { next_round = Some(Instant::now() + DISCOVERY_PAUSE); }
                    session.update(|state| state.scanning = false);
                }
                Event::Connecting(addr) => session.update(|state| { state.link = Link::Connecting(addr); state.step = Some(Step::Page); }),
                Event::Step { addr, step } => session.update(|state| if state.link == Link::Connecting(addr) || state.link == Link::Idle { state.link = Link::Connecting(addr); state.step = Some(step); }),
                Event::TimedOut { addr, step } => {
                    log::info!("Bluetooth: connection attempt timed out at {step:?}");
                    let asked = user_connect.take() == Some(addr);
                    if auto_connect == Some(addr) { auto_connect = None; }
                    session.update(|state| {
                        state.link = Link::Idle; state.step = None;
                        if asked { state.problem(match step {
                            Step::Page => "The headphones did not answer. Turn them on or put them in pairing mode, then try again.",
                            Step::SinkCheck => "The device did not answer the check for Bluetooth audio. Try again.",
                            Step::Authentication => "Pairing did not finish in time. Put the headphones in pairing mode and try again.",
                            Step::Encryption => "The headphones did not finish securing the connection. Try again.",
                        }); }
                    });
                }
                Event::Connected { addr, .. } => {
                    user_connect = None;
                    linked = Some(addr);
                    amplifier = Amplifier::default();
                    if let Some(volume) = store.saved(addr).volume {
                        let volume = volume.min(limit());
                        store.saved_mut(addr).volume = Some(volume);
                        amplifier.waiting_for = Some(volume);
                    }
                    if auto_connect == Some(addr) { auto_connect = None; }
                    store.saw(addr);
                    let known = shared.state.lock().unwrap().found.iter().chain(&store.devices).find(|d| d.addr == addr).cloned();
                    let device = Device { rssi: 0, sink: false, ..known.unwrap_or(Device { addr, name: "Headphones".into(), class: 0x24_0404, rssi: 0, sink: false }) };
                    store.devices.retain(|d| d.addr != addr); store.devices.push(device); store.last = Some(addr);
                    if let Err(error) = store.save(blocking, &options.store) { log::warn!("Bluetooth: paired devices not saved: {error}"); }
                    log::info!("Bluetooth: connected, {} paired", store.devices.len());
                    session.update(|state| { state.paired = store.devices.clone(); state.seen = store.seen.clone(); state.link = Link::Connected(addr); state.step = None; state.attempt = None; });
                }
                Event::ConnectFailed { addr, status } => {
                    log::info!("Bluetooth: connection failed with status {status:#04x}");
                    let asked = user_connect.take() == Some(addr);
                    if auto_connect == Some(addr) { auto_connect = None; }
                    session.update(|state| {
                        state.link = Link::Idle; state.step = None;
                        // Reconnecting at start-up to headphones that are off is not worth a message.
                        if asked { state.problem(match status {
                            0x04 => "Headphones not found. Turn them on, or put them in pairing mode.",
                            0x05 | 0x06 | 0x18 | 0x29 => "Pairing was refused. Put the headphones in pairing mode and try again.",
                            _ => "Could not connect to the headphones. Try again.",
                        }); }
                    });
                }
                Event::NotSink(addr) => {
                    log::info!("Bluetooth: the device has no A2DP sink; not paired");
                    // The disconnect that follows is not reported as a failed connection.
                    if user_connect == Some(addr) { user_connect = None; }
                    // No device name: the probe prints the notice.
                    session.update(|state| { state.problem("This device does not take Bluetooth audio (no A2DP sink). It was not paired."); state.attempt = None; });
                }
                Event::Disconnected { addr, reason } => {
                    log::info!("Bluetooth: disconnected, reason {reason:#04x}");
                    media = None;
                    amplifier = Amplifier::default();
                    linked = None;
                    shared.output(false);
                    // The time it was last seen, with any volume change not yet written.
                    let paired = store.devices.iter().any(|d| d.addr == addr);
                    if paired { store.saw(addr); }
                    if save_at.take().is_some() || paired { save(&store); }
                    session.update(|state| { state.link = Link::Idle; state.stream = None; state.output = None; state.step = None; state.seen = store.seen.clone(); });
                }
                Event::LinkKey(key) => {
                    store.keys.retain(|k| k.addr != key.addr); store.keys.push(key);
                    // Saved with the device record when the link comes up.
                }
                Event::KeyRejected(addr) => {
                    store.forget(addr);
                    save(&store);
                    session.update(|state| { state.paired = store.devices.clone(); state.seen = store.seen.clone(); state.problem("The headphones no longer know this player. Pair them again."); });
                }
                Event::Stream(Some(stream)) => {
                    log::info!("Bluetooth: {} at {} Hz, media MTU {}", stream.codec.kind().name(), stream.codec.rate(), stream.mtu);
                    match Media::new(stream, options.ldac, options.ldac_adaptive) {
                        Ok(new) => {
                            media = Some(new);
                            let reason = codec_reason(host.origin(), stream.codec.kind(), &preference);
                            log::info!("Bluetooth: codec {}: {reason}", stream.codec.kind().name());
                            let addr = linked;
                            let output = addr.and_then(|addr| store.devices.iter().find(|d| d.addr == addr)).map(|device| Output {
                                addr: device.addr, name: device.name.clone(), class: device.class, saved: store.saved(device.addr), volume_revision: shared.next_volume_revision(), absolute: amplifier.remote });
                            session.update(|state| { state.stream = Some(stream); state.output = output; state.codec_reason = reason; });
                            shared.output(true);
                        }
                        Err(error) => {
                            // The encoder for a codec we offered did not start: end the link rather than play silence.
                            log::warn!("Bluetooth: {} encoder: {error}", stream.codec.kind().name());
                            session.update(|state| state.problem(format!("The {} encoder could not start.", stream.codec.kind().name())));
                            host.command(Command::Disconnect);
                        }
                    }
                }
                Event::Stream(None) => {
                    media = None;
                    shared.output(false);
                    session.update(|state| {
                        state.output = None;
                        // An open stream that closes is the headphones going (switched off, out of
                        // range): the player pauses, and there is no problem to report.
                        let refused = state.stream.take().is_none();
                        if refused && state.connected().is_some() && state.notice.is_empty() { state.problem("These headphones did not accept audio."); }
                    });
                }
                Event::Streaming(on) => session.update(|state| if let Some(addr) = state.connected() { state.link = if on { Link::Playing(addr) } else { Link::Connected(addr) }; }),
                Event::AbsoluteVolume => log::info!("Bluetooth: the headphones take absolute volume"),
                Event::AmplifierLevel(level) => {
                    amplifier.level = Some(level);
                    amplifier.follow(session, limit());
                }
                Event::Volume { level, initial } => {
                    let Some(addr) = linked else { continue };
                    let cap = limit();
                    let known = store.saved(addr).volume;
                    let restore = known.filter(|_| initial);
                    let volume = restore.unwrap_or_else(|| from_level(level)).min(cap);
                    // Compare in AVRCP units: even a raw level that rounds to the cap can exceed it.
                    if restore.is_some() || level > to_level(cap) {
                        host.command(Command::SetVolume(to_level(volume)));
                        flush_volume = true;
                    }
                    commit_volume(&session, &mut store, addr, volume, &mut save_at);
                }
                Event::DeviceId(id) => {
                    log::info!("Bluetooth: Device ID source {} vendor {:#06x} product {:#06x}", id.source, id.vendor, id.product);
                    let Some(addr) = linked else { continue };
                    if store.devices.iter().any(|d| d.addr == addr) && store.saved(addr).id != Some(id) { store.saved_mut(addr).id = Some(id); save(&store); }
                    session.update(|state| if let Some(output) = state.output.as_mut().filter(|o| o.addr == addr) { output.saved.id = Some(id); });
                }
                Event::Button(button) => {
                    // Play while playing and pause while paused are stale presses, not toggles; volume keys go to the player's volume.
                    let playing = shared.playing.load(Ordering::Relaxed);
                    let stale = matches!((button, playing), (Button::Play, true) | (Button::Pause | Button::Stop, false));
                    let buttons = shared.buttons.lock().unwrap().clone();
                    if let Some(buttons) = buttons.filter(|_| !stale) { buttons(button); }
                }
            }
        }
        // The scan flag follows the host, so no refused or lost request leaves it set.
        if shown_scanning != host.scanning() { shown_scanning = host.scanning(); session.update(|state| state.scanning = shown_scanning); }
        let latest = (host.media_dropped(), host.transport_counters().0, shared.endpoint.underruns());
        let diagnostics = host.diagnostics();
        telemetry.uart_errors(now, || wire.errors(blocking));
        // Every 250 ms the audio numbers too, and a copy of the state for telemetry.
        let publish = telemetry.due(now);
        let mut snapshot = None;
        if publish || latest != counters || diagnostics != last_diagnostics {
            let mut state = shared.state.lock().unwrap();
            state.counters = latest; state.diagnostics = diagnostics.clone();
            if publish {
                state.link_stats = host.link_stats(); state.media = media.as_mut().map(Media::stats); state.uart_errors = telemetry.uart();
                snapshot = Some(state.clone());
            }
            drop(state);
            shared.revision.fetch_add(1, Ordering::Release);
            counters = latest; last_diagnostics = diagnostics;
        }
        telemetry.update(now, &host, media.as_mut(), snapshot, latest.0, latest.2);
        // The restore/correction was queued after host.poll. Write it before sleeping on an idle link.
        if flush_volume { continue }
        let mut timeout = deadline.map_or(1000, |at| at.saturating_sub(now)).min(1000);
        if let Some(at) = save_at { timeout = timeout.min(at.saturating_duration_since(Instant::now()).as_millis() as u64 + 1); }
        if let Some(at) = wake_at_us { timeout = timeout.min(at.saturating_sub(now_us).div_ceil(1000)); }
        if stopping.is_some() { timeout = timeout.min(20); }
        // Held requests and the next discovery round are looked at again soon.
        if held_count != 0 { timeout = timeout.min(100); }
        if let Some(at) = next_round { timeout = timeout.min(at.saturating_duration_since(Instant::now()).as_millis() as u64 + 1); }
        let waited = Instant::now();
        match uart::wait_readable(blocking, [wire.readable(), woken.as_fd()], timeout as i32) {
            Ok([data, wake]) => {
                // Woken by the timeout: how late, against the time asked for.
                if !data && !wake { telemetry.woke((waited.elapsed().as_millis() as u64).saturating_sub(timeout)); }
                if wake { let mut drain = [0u8; 64]; while matches!(uart::read_ready(blocking, woken, &mut drain), Ok(n) if n > 0) {} }
                if data {
                    loop {
                        match wire.read(blocking, &mut buffer) {
                            Ok(0) => break,
                            Ok(n) => host.rx(&buffer[..n], start.elapsed().as_millis() as u64),
                            Err(error) => { shared.output(false); return failed(format!("Bluetooth port: {error}")) }
                        }
                    }
                }
            }
            Err(error) => { shared.output(false); return failed(format!("Bluetooth port: {error}")) }
        }
    }
    if save_at.is_some() { save(&store); }
    shared.output(false);
    drop(wire);
    release_power(blocking, options, Duration::ZERO);
    Exit::Stopped
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod sim_tests;
