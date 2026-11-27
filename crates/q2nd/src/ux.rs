//! UX error presentation and UX telemetry (UX-006, TELEMETRY-005).
//!
//! The screen never shows error codes or raw backend text: [`present`] turns a
//! failure into a short plain message and reports its fixed [`Code`]. Journeys
//! (a user action and its outcome), playback-stop causes, stuck states and owner
//! problem reports go to the opt-in telemetry outbox with a numeric snapshot of
//! connectivity/playback state and a flight recorder of recent numeric events.
//!
//! Everything reported is numeric or a fixed enum. Free text is reduced to a
//! 32-bit hash of its non-digit characters, which `tools/ux_triage.py` maps back
//! to string literals in the source; titles and IDs never enter a report.
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};

pub mod bluetooth;

/// Fixed failure codes. Values are stable: add, never renumber.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
pub enum Code {
    Unknown = 1,
    Offline = 10,
    NetworkUnreachable = 11,
    NavidromeUnreachable = 12,
    AbsUnreachable = 13,
    AbsChanged = 14,
    AbsNotConfigured = 15,
    NavidromeNotConfigured = 16,
    ServerError = 17,
    Timeout = 18,
    StreamInterrupted = 19,
    NotDownloaded = 20,
    FileMissing = 21,
    CardUnavailable = 22,
    CardFull = 23,
    Descriptor = 24,
    CardError = 25,
    Auth = 26,
    /// Audiobookshelf answered but is busy or failing (408, 429, 5xx).
    AbsServerError = 27,
    NoSongs = 30,
    NoDownloadedSongs = 31,
    PlaybackChanged = 32,
    Decode = 33,
    Output = 34,
    Database = 35,
    CacheDelete = 36,
    Stuck = 40,
}

impl Code {
    pub const ALL: [Code; 27] = [Self::Unknown, Self::Offline, Self::NetworkUnreachable, Self::NavidromeUnreachable,
        Self::AbsUnreachable, Self::AbsChanged, Self::AbsNotConfigured, Self::NavidromeNotConfigured, Self::ServerError,
        Self::Timeout, Self::StreamInterrupted, Self::NotDownloaded, Self::FileMissing, Self::CardUnavailable, Self::CardFull,
        Self::Descriptor, Self::CardError, Self::Auth, Self::AbsServerError, Self::NoSongs, Self::NoDownloadedSongs, Self::PlaybackChanged,
        Self::Decode, Self::Output, Self::Database, Self::CacheDelete, Self::Stuck];

    /// What the listener sees: a few plain words, no codes (UX-006).
    pub fn message(self) -> &'static str {
        match self {
            Self::Unknown => "Something went wrong",
            Self::Offline => "You're offline",
            Self::NetworkUnreachable => "No network connection",
            Self::NavidromeUnreachable => "Can't reach Navidrome",
            Self::AbsUnreachable => "Can't reach Audiobookshelf",
            Self::AbsChanged => "Library changed. Choose it again",
            Self::AbsNotConfigured => "Set up Audiobookshelf in Settings",
            Self::NavidromeNotConfigured => "Set up Navidrome in Settings",
            Self::ServerError => "The server had a problem",
            Self::Timeout => "The server took too long",
            Self::StreamInterrupted => "Stream interrupted",
            Self::NotDownloaded => "Not downloaded",
            Self::FileMissing => "File is missing from the card",
            Self::CardUnavailable => "Insert a card",
            Self::CardFull => "The card is full",
            Self::Descriptor => "Playback was interrupted",
            Self::CardError => "Couldn't read the card",
            Self::Auth => "Wrong user name or password",
            Self::AbsServerError => "Audiobookshelf had a problem",
            Self::NoSongs => "No songs available",
            Self::NoDownloadedSongs => "No downloaded songs",
            Self::PlaybackChanged => "Playback changed. Try again",
            Self::Decode => "Can't play this file",
            Self::Output => "Audio output problem",
            Self::Database => "Couldn't read the library",
            Self::CacheDelete => "Couldn't remove the cached song",
            Self::Stuck => "This is taking too long",
        }
    }

    /// A network failure that can clear by itself: playback stopped by one is
    /// resumed automatically when the link returns (UX-006).
    pub fn transient(code: u16) -> bool {
        [Self::NetworkUnreachable, Self::NavidromeUnreachable, Self::AbsUnreachable, Self::Timeout,
         Self::StreamInterrupted, Self::ServerError, Self::AbsServerError, Self::Descriptor].iter().any(|c| *c as u16 == code)
    }

    /// Classifies backend text. `None`: not a failure (an ordinary notice).
    pub fn classify(raw: &str) -> Option<Self> {
        // A message already in listener words keeps its own code.
        // (also behind a prefix such as "open: ").
        if let Some(code) = Self::ALL.iter().find(|code| raw.contains(code.message())) { return Some(*code); }
        let s = raw.to_ascii_lowercase();
        let has = |words: &[&str]| words.iter().any(|w| s.contains(w));
        Some(if has(&["file descriptor", "bad file", "os error 9"]) { Self::Descriptor }
        else if has(&["insert a card"]) { Self::CardUnavailable }
        else if has(&["no space", "card is full", "card full", "storage full", "os error 28"]) { Self::CardFull }
        else if has(&["set up audiobookshelf"]) { Self::AbsNotConfigured }
        else if has(&["wrong user name", "password", "unauthorized", "http 401", "http 403", "forbidden"]) { Self::Auth }
        else if has(&["set up navidrome", "no server configured", "no server set up", "not configured"]) { Self::NavidromeNotConfigured }
        else if has(&["changed; select", "changed; choose", "changed during", "account changed", "settings changed", "audiobookshelf changed"]) { Self::AbsChanged }
        else if has(&["playback changed"]) { Self::PlaybackChanged }
        else if has(&["not downloaded"]) { Self::NotDownloaded }
        else if has(&["no downloaded"]) { Self::NoDownloadedSongs }
        else if has(&["no songs", "no albums"]) { Self::NoSongs }
        else if has(&["needs a connection", "not available offline", "you're offline", "offline mode"]) { Self::Offline }
        else if has(&["timed out", "timeout", "took too long"]) { Self::Timeout }
        else if has(&["audiobookshelf", "audiobook server"]) && has(&["unavailable", "unreachable", "disconnected", "connect", "stream:", "network", "worker stopped", "returned no"]) { Self::AbsUnreachable }
        else if has(&["file unavailable", "no such file", "os error 2)", "missing from the card"]) { Self::FileMissing }
        else if has(&["card:", "read-only file system", "input/output error", "os error 5"]) { Self::CardError }
        else if has(&["database", "sqlite", "index"]) && has(&["unable", "failed", "error", "unavailable"]) { Self::Database }
        else if has(&["decode", "unsupported", "symphonia", "malformed", "unknown format"]) { Self::Decode }
        else if has(&["pcm", "alsa", "audio output", "dac"]) { Self::Output }
        else if has(&["empty response", "incomplete stream", "stream:", "connection reset", "broken pipe", "unexpected eof"]) { Self::StreamInterrupted }
        else if has(&["dns", "resolve", "no route", "network is unreachable", "connection refused", "network"]) { Self::NetworkUnreachable }
        else if has(&["navidrome", "subsonic", "server unreachable", "offline:"]) { Self::NavidromeUnreachable }
        else if has(&["http ", "status ", "server error", "server returned", "server said", "5xx", "invalid response", "json"]) { Self::ServerError }
        else if has(&["error", "failed", "invalid", "unable", "cannot", "can't", "couldn't", "unavailable", "missing", "stopped", "os error", "panicked"]) { Self::Unknown }
        else { return None })
    }

    /// Messages already written for listeners (UI copy, not backend text) are
    /// kept verbatim for these codes; the code still reaches the report.
    fn keeps_text(self, raw: &str) -> bool {
        matches!(self, Self::NotDownloaded | Self::NoSongs | Self::NoDownloadedSongs | Self::PlaybackChanged
                | Self::CardUnavailable | Self::AbsNotConfigured | Self::NavidromeNotConfigured | Self::Offline | Self::AbsChanged | Self::Auth)
            && raw.len() <= 48 && !raw.contains(':') && !raw.contains('(') && !raw.contains('\n')
            && !raw.chars().any(|c| c.is_ascii_digit())
    }
}

/// User journeys whose outcome and latency are reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Journey {
    None = 0,
    ShuffleSongs = 1,
    ShuffleAlbums = 2,
    PlayMusic = 3,
    PlaySpoken = 4,
    Resume = 5,
    OpenPage = 6,
    OpenInfo = 7,
    Download = 8,
    Buffering = 9,
    Wake = 10,
    Skip = 11,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Outcome {
    None = 0,
    AudioStarted = 1,
    Shown = 2,
    Failed = 3,
    Cancelled = 4,
    TimedOut = 5,
    Superseded = 6,
}

/// Why playback left Playing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum StopCause {
    User = 1,
    Unplug = 2,
    SleepTimer = 3,
    EndOfQueue = 4,
    Error = 5,
    System = 6,
    Settings = 7,
    Unexplained = 8,
}

/// The kind of a UX report.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum What { Error, Recovered, Journey, Stop, Stuck, Problem }

/// Numeric state at the moment of a report.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    /// 0 off, 1 wanted but not associated, 2 associated with an address.
    pub wifi: u8,
    /// 0 not configured, 1 reachable, 2 unreachable; Navidrome also 3:
    /// checking (a request failed and the probe runs).
    pub navidrome: u8,
    pub abs: u8,
    pub doze: bool,
    pub offline: bool,
    pub screen_on: bool,
    /// 0 stopped, 1 playing, 2 paused, 3 buffering.
    pub play: u8,
    /// Page loads running.
    pub pages: u8,
    /// Seconds since the last input.
    pub idle_s: u32,
    /// The audio output: 1 headphone jack, 2 USB DAC, 3 Bluetooth; 0 in
    /// reports from builds before it was recorded.
    #[serde(default)]
    pub route: u8,
}

/// Recent numeric events: `[boottime ms (low 32 bits), kind, value]`.
pub type Recent = Vec<[u32; 3]>;

/// Flight-recorder event kinds.
#[derive(Clone, Copy, Debug)]
#[repr(u32)]
pub enum Mark {
    Input = 1,
    Toast = 2,
    Error = 3,
    JourneyBegin = 4,
    JourneyEnd = 5,
    PlayState = 6,
    Wifi = 7,
    Navidrome = 8,
    Abs = 9,
    Doze = 10,
    Screen = 11,
    PlayerCmd = 12,
    Recovered = 13,
    Stop = 14,
    Problem = 15,
    /// A failed network attempt that will be retried: `code << 8 | attempt`.
    Retry = 16,
    /// The player opened a song: [`track_open_value`].
    TrackOpen = 17,
    /// A music stream opened before its file was complete on the card:
    /// [`stream_open_value`].
    StreamOpen = 18,
    /// A stream read waited for download bytes: milliseconds (≥ [`STREAM_WAIT_MS`]).
    StreamWait = 19,
    /// The audio output underran and was restarted: errno.
    Underrun = 20,
    /// Fetch-ahead, [`tagged`]: tag 0 begin (songs planned), 1 one song done
    /// (seconds it took), 2 end (1 done, 2 newer queue, 3 offline), 3 a plan
    /// with nothing to fetch.
    Fetch = 21,
    /// Doze decision, [`tagged`]: tag 0 the radio dozed, 1–6 it woke
    /// (disabled, Wi-Fi off, screen, AirPlay, stopped, low); the quantity is
    /// the seconds of complete local audio ahead.
    DozeAhead = 22,
    /// A Navidrome request failed (library/health.rs): priority (0
    /// foreground, 1 background) in bits 8–9, `subsonic::Failure` below.
    /// Low-priority failures are not recorded.
    ServerFailure = 23,
    /// A health probe attempt ended: bit 0 answered, attempt (1, 2) in bits
    /// 4–7, the failure class in bits 8–15.
    ServerProbe = 24,
    /// Bluetooth audio fell behind, [`tagged`]: tag 0 the source ran behind its
    /// schedule and caught up (quantity: the worst lateness, ms), 1 it fell so
    /// far behind that the schedule restarted (the headphones ran dry), 2 the
    /// controller had no free ACL buffer for 50 ms or more (ms).
    BtStall = 25,
    /// Bluetooth transport errors since the last such mark (at most one per
    /// kind a second), [`tagged`]: tag 0 H5 resend timeouts, 1 damaged H5
    /// frames, 2 out-of-sequence H5 packets, 3 UART receive errors.
    BtTransport = 26,
    /// LDAC's adaptive bit rate moved, [`tagged`]: tag the level (0 HQ … 4 MQ), quantity kbit/s.
    BtRate = 27,
    /// The Bluetooth state changed, [`tagged`]: tag the state (as in
    /// `bluetooth::Bluetooth::state`), quantity the codec.
    BtState = 28,
    /// A Wi-Fi supplicant event: 0 scan started, 1 scan results, 2 association
    /// started, 3 connected, 4 disconnected, 5 saved network not found.
    WifiEvent = 29,
    /// The player asked for a Wi-Fi scan: 1 the hinted first scan found no saved
    /// network (all channels), 2 band steering while the Wi-Fi page is open (5 GHz
    /// channels), 3 the Wi-Fi page, 4 a reconnect while not associated (the supplicant scans).
    WifiScan = 30,
    /// Listener-initiated analog gain change, see analog_volume().
    AnalogVolume = 31,
}

/// Mark::TrackOpen value: origin (1 downloaded, 2 cached, 3 streaming,
/// 4 radio) in bits 29–31, spoken in bit 28, source-open and probe
/// milliseconds (each capped at 16383) in bits 14–27 and 0–13.
pub fn track_open_value(origin: u32, spoken: bool, source_ms: u64, probe_ms: u64) -> u32 {
    let cap = |ms: u64| ms.min(0x3fff) as u32;
    (origin & 7) << 29 | u32::from(spoken) << 28 | cap(source_ms) << 14 | cap(probe_ms)
}

/// Mark::StreamOpen value: percent of the file already written (127: size
/// unknown) in bits 24–31, KiB written (capped) below.
pub fn stream_open_value(written: u64, total: Option<u64>) -> u32 {
    let percent = match total { Some(t) if t > 0 => (written.min(t) * 100 / t) as u32, _ => 127 };
    percent << 24 | (written / 1024).min(0xff_ffff) as u32
}

/// An 8-bit tag above a 24-bit quantity (capped).
pub fn tagged(tag: u32, quantity: u64) -> u32 { (tag & 0xff) << 24 | quantity.min(0xff_ffff) as u32 }

/// Shortest stream wait recorded; shorter ones are covered by the read-ahead.
pub const STREAM_WAIT_MS: u64 = 100;
pub fn stream_waited(waited: std::time::Duration) {
    let ms = waited.as_millis() as u64;
    if ms >= STREAM_WAIT_MS { record(Mark::StreamWait, ms.min(u64::from(u32::MAX)) as u32); }
}

/// Seconds between underrun reports; every underrun is still marked.
const UNDERRUN_REPORT_S: u32 = 10;
/// The audio output underran and was restarted. Reported at most once per
/// [`UNDERRUN_REPORT_S`] as a recovery (code `output`), with the history.
pub fn underrun(errno: i32) {
    record(Mark::Underrun, errno.unsigned_abs());
    // +1: zero means never reported.
    let now_s = (now_us() / 1_000_000) as u32 + 1;
    let admitted = with_state(|state| {
        let last = state.underrun_s.load(Ordering::Relaxed);
        (last == 0 || now_s.wrapping_sub(last) >= UNDERRUN_REPORT_S)
            && state.underrun_s.compare_exchange(last, now_s, Ordering::Relaxed, Ordering::Relaxed).is_ok()
    });
    if !admitted { return; }
    send(What::Recovered, Code::Output as u16, Journey::None, Outcome::None, 0, errno.unsigned_abs());
}

// ---- state cells (32-bit atomics only: the Q2 target has no 64-bit atomics)
static WIFI: AtomicU8 = AtomicU8::new(0);
static NAVIDROME: AtomicU8 = AtomicU8::new(0);
static ABS: AtomicU8 = AtomicU8::new(0);
static DOZE: AtomicBool = AtomicBool::new(false);
static OFFLINE: AtomicBool = AtomicBool::new(false);
static SCREEN_ON: AtomicBool = AtomicBool::new(true);
static PLAY: AtomicU8 = AtomicU8::new(0);
static PAGES: AtomicU8 = AtomicU8::new(0);
static LAST_INPUT_S: AtomicU32 = AtomicU32::new(0);
/// `Snapshot::route`; the output policy starts on the headphone jack.
static ROUTE: AtomicU8 = AtomicU8::new(1);

fn now_us() -> u64 { crate::playtrace::boottime_us() }

fn changed(cell: &AtomicU8, value: u8, mark: Mark) {
    if cell.swap(value, Ordering::Relaxed) != value { record(mark, u32::from(value)); }
}
pub fn set_wifi(state: u8) { changed(&WIFI, state, Mark::Wifi); }
pub fn set_navidrome(state: u8) { changed(&NAVIDROME, state, Mark::Navidrome); }
pub fn set_abs(state: u8) { changed(&ABS, state, Mark::Abs); }
pub fn set_doze(on: bool) { if DOZE.swap(on, Ordering::Relaxed) != on { record(Mark::Doze, on as u32); } }
pub fn set_offline(on: bool) { OFFLINE.store(on, Ordering::Relaxed); }
pub fn set_screen(on: bool) { if SCREEN_ON.swap(on, Ordering::Relaxed) != on { record(Mark::Screen, on as u32); } }
pub fn set_play(state: u8) { changed(&PLAY, state, Mark::PlayState); }
/// The audio output changed (`audio::output::Policy`): 1 jack, 2 USB DAC, 3 Bluetooth.
pub fn set_route(route: u8) { ROUTE.store(route, Ordering::Relaxed); }
pub fn page_started() { PAGES.fetch_add(1, Ordering::Relaxed); }
#[allow(deprecated)]
pub fn page_finished() { let _ = PAGES.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| Some(n.saturating_sub(1))); }
pub fn input(code: u32) {
    LAST_INPUT_S.store((now_us() / 1_000_000) as u32, Ordering::Relaxed);
    record(Mark::Input, code);
}
pub fn player_cmd(code: u32) { record(Mark::PlayerCmd, code); }

pub fn snapshot() -> Snapshot {
    let now_s = (now_us() / 1_000_000) as u32;
    Snapshot {
        wifi: WIFI.load(Ordering::Relaxed), navidrome: NAVIDROME.load(Ordering::Relaxed),
        abs: ABS.load(Ordering::Relaxed), doze: DOZE.load(Ordering::Relaxed),
        offline: OFFLINE.load(Ordering::Relaxed), screen_on: SCREEN_ON.load(Ordering::Relaxed),
        play: PLAY.load(Ordering::Relaxed), pages: PAGES.load(Ordering::Relaxed),
        idle_s: now_s.saturating_sub(LAST_INPUT_S.load(Ordering::Relaxed)),
        route: ROUTE.load(Ordering::Relaxed),
    }
}

// ---- shared state: atomics only, so UI, player and workers never wait on it
const RECENT_LIMIT: usize = 64;
struct Slot { meta: AtomicU32, started_us: AtomicU32 }
struct Shared {
    ring: [AtomicU32; RECENT_LIMIT * 3],
    written: AtomicU32,
    slots: [Slot; 4],
    /// Boot second (+1) of the last underrun report; 0 none.
    underrun_s: AtomicU32,
    // Single player writer; copied into an owner report even after heavy wheel input.
    analog_version: AtomicU32,
    analog_at: AtomicU32,
    analog_value: AtomicU32,
}
const SHARED: Shared = Shared {
    ring: [const { AtomicU32::new(0) }; RECENT_LIMIT * 3],
    written: AtomicU32::new(0),
    slots: [const { Slot { meta: AtomicU32::new(0), started_us: AtomicU32::new(0) } }; 4],
    underrun_s: AtomicU32::new(0),
    analog_version: AtomicU32::new(0),
    analog_at: AtomicU32::new(0),
    analog_value: AtomicU32::new(0),
};
#[cfg(not(test))]
static STATE: Shared = SHARED;
// Tests run in parallel threads: each test thread has its own state.
#[cfg(test)]
thread_local! { static STATE: Shared = const { SHARED }; }
fn with_state<R>(f: impl FnOnce(&Shared) -> R) -> R {
    #[cfg(not(test))]
    { f(&STATE) }
    #[cfg(test)]
    { STATE.with(f) }
}

/// Never waits. A reader racing a writer may see one torn entry.
pub fn record(mark: Mark, value: u32) {
    let t = (now_us() / 1000) as u32;
    with_state(|state| {
        let i = state.written.fetch_add(1, Ordering::Relaxed) as usize % RECENT_LIMIT * 3;
        state.ring[i].store(t, Ordering::Relaxed);
        state.ring[i + 1].store(mark as u32, Ordering::Relaxed);
        state.ring[i + 2].store(value, Ordering::Release);
    });
}

pub fn recent() -> Recent {
    with_state(|state| {
        let written = state.written.load(Ordering::Acquire) as usize;
        let len = written.min(RECENT_LIMIT);
        (written - len..written).map(|k| {
            let i = k % RECENT_LIMIT * 3;
            [state.ring[i].load(Ordering::Relaxed), state.ring[i + 1].load(Ordering::Relaxed), state.ring[i + 2].load(Ordering::Acquire)]
        }).collect()
    })
}

/// Two 7-bit slider positions, compensation changed, operation failed, and the
/// total command time in milliseconds. These fields contain no media identity.
pub(crate) fn analog_volume(old: u8, new: u8, changed: bool, failed: bool, elapsed: std::time::Duration) {
    let value = u32::from(old) << 25 | u32::from(new) << 18 | u32::from(changed) << 17
        | u32::from(failed) << 16 | elapsed.as_millis().min(65_535) as u32;
    with_state(|s| {
        s.analog_version.fetch_add(1, Ordering::SeqCst);
        s.analog_at.store((now_us() / 1000) as u32, Ordering::SeqCst);
        s.analog_value.store(value, Ordering::SeqCst);
        s.analog_version.fetch_add(1, Ordering::SeqCst);
    });
}

fn problem_history(mut history: Recent) -> Recent {
    let now = (now_us() / 1000) as u32;
    with_state(|s| {
        let version = s.analog_version.load(Ordering::SeqCst);
        let at = s.analog_at.load(Ordering::SeqCst);
        let data = s.analog_value.load(Ordering::SeqCst);
        if version != 0 && version % 2 == 0 && version == s.analog_version.load(Ordering::SeqCst)
            && now.wrapping_sub(at) <= 300_000 {
            if history.len() == RECENT_LIMIT { history.remove(0); }
            let pos = history.partition_point(|e| now.wrapping_sub(e[0]) > now.wrapping_sub(at));
            history.insert(pos, [at, Mark::AnalogVolume as u32, data]);
        }
    });
    history
}

/// FNV-1a over non-digit bytes, so counts/positions in a message do not split it.
pub fn text_hash(text: &str) -> u32 {
    text.bytes().filter(|b| !b.is_ascii_digit())
        .fold(2166136261u32, |h, b| (h ^ u32::from(b)).wrapping_mul(16777619))
}

// ---- reports
/// One UX report as admitted to the telemetry outbox.
#[derive(Clone, Debug, PartialEq)]
pub struct Report {
    pub what: What,
    pub code: u16,
    pub journey: u8,
    pub outcome: u8,
    pub latency_us: u64,
    pub detail: u32,
    pub state: Snapshot,
    pub recent: Recent,
    /// While Bluetooth is on.
    pub bluetooth: Option<bluetooth::Bluetooth>,
}

#[cfg(test)]
thread_local! { static SENT: std::cell::RefCell<Vec<Report>> = const { std::cell::RefCell::new(Vec::new()) }; }
#[cfg(test)]
pub fn take_sent() -> Vec<Report> { SENT.with(|s| std::mem::take(&mut *s.borrow_mut())) }

fn send(what: What, code: u16, journey: Journey, outcome: Outcome, latency_us: u64, detail: u32) {
    let report = Report { what, code, journey: journey as u8, outcome: outcome as u8, latency_us, detail,
        state: snapshot(), recent: if what == What::Problem { problem_history(recent()) } else { recent() }, bluetooth: bluetooth::current() };
    log::info!("ux {what:?} code={code} journey={journey:?} outcome={outcome:?} latency_ms={} detail={detail:08x}", latency_us / 1000);
    #[cfg(test)]
    SENT.with(|s| s.borrow_mut().push(report.clone()));
    let _ = crate::diagnostics::admit_ux(report, now_us());
}

/// A failure shown to the listener. Returns the text to show.
pub fn present(raw: &str) -> String { present_in(raw, current_journey()) }

pub fn present_in(raw: &str, journey: Journey) -> String {
    record(Mark::Toast, text_hash(raw));
    let Some(code) = Code::classify(raw) else { return raw.to_owned() };
    fail(code, journey, text_hash(raw));
    if code.keeps_text(raw) { raw.to_owned() } else { code.message().to_owned() }
}

/// Reports a failure by code; ends the journey, if pending, as failed.
pub fn fail(code: Code, journey: Journey, detail: u32) {
    record(Mark::Error, code as u32);
    let latency = end_pending(journey).unwrap_or(0);
    send(What::Error, code as u16, journey, Outcome::Failed, latency, detail);
}

/// An automatic recovery that the listener did not have to act on.
pub fn recovered(code: Code, journey: Journey) {
    record(Mark::Recovered, code as u32);
    send(What::Recovered, code as u16, journey, Outcome::None, 0, 0);
}

/// The sender of a pause/stop names its cause before sending the command; the
/// player takes it when playback actually leaves Playing.
static EXPECTED_STOP: AtomicU8 = AtomicU8::new(0);
pub fn expect_stop(cause: StopCause) { EXPECTED_STOP.store(cause as u8, Ordering::Relaxed); }
pub fn take_expected_stop() -> Option<StopCause> {
    Some(match EXPECTED_STOP.swap(0, Ordering::Relaxed) {
        1 => StopCause::User, 2 => StopCause::Unplug, 3 => StopCause::SleepTimer, 4 => StopCause::EndOfQueue,
        5 => StopCause::Error, 6 => StopCause::System, 7 => StopCause::Settings, 8 => StopCause::Unexplained,
        _ => return None,
    })
}

/// The audio journey (Shuffle, Play, Resume, Skip) waiting for sound, if any.
pub fn pending_audio() -> Option<Journey> {
    pending().into_iter().find(|(_, _, meta)| meta & AUDIO != 0).map(|(journey, _, _)| journey)
}

pub fn stop(cause: StopCause, detail: u32) {
    record(Mark::Stop, cause as u32);
    if cause != StopCause::User && cause != StopCause::EndOfQueue {
        send(What::Stop, cause as u16, Journey::None, Outcome::None, 0, detail);
    }
}

/// Wake phases in the flight recorder (Mark::Screen values), for UXI-015.
pub const WAKE_BEGIN: u32 = 20;
pub const WAKE_BLANK_JOINED: u32 = 21;
pub const WAKE_BATTERY_READY: u32 = 22;
pub const WAKE_ENABLED: u32 = 23;

/// A screen wake reached its first presented frame. One slower than the
/// active-use target (UX-008, 100 ms) is reported with its phase marks.
pub fn wake_presented(latency_us: u64) {
    record(Mark::Screen, 24);
    if latency_us >= 100_000 { send(What::Journey, 0, Journey::Wake, Outcome::Shown, latency_us, 0); }
}

/// The owner's “Report a Problem”: marks the moment with the recent history.
pub fn problem() {
    record(Mark::Problem, 0);
    send(What::Problem, 0, current_journey(), Outcome::None, 0, 0);
}

// ---- journeys: begin on the action, end on its outcome; stuck after 3 s
pub const STUCK_US: u64 = 3_000_000;
pub const TIMEOUT_US: u64 = 30_000_000;
/// Slot meta: 0 empty, RESERVED while being filled, else OCCUPIED | flags | journey.
const RESERVED: u32 = 1 << 30;
const OCCUPIED: u32 = 1 << 31;
const AUDIO: u32 = 1 << 8;
const STUCK: u32 = 1 << 9;
fn journey_of(meta: u32) -> Journey {
    match meta & 0xff {
        1 => Journey::ShuffleSongs, 2 => Journey::ShuffleAlbums, 3 => Journey::PlayMusic, 4 => Journey::PlaySpoken,
        5 => Journey::Resume, 6 => Journey::OpenPage, 7 => Journey::OpenInfo, 8 => Journey::Download,
        9 => Journey::Buffering, 10 => Journey::Wake, 11 => Journey::Skip, _ => Journey::None,
    }
}
/// Pending journeys as (journey, started, meta), started in wrapping microseconds.
fn pending() -> Vec<(Journey, u32, u32)> {
    with_state(|state| state.slots.iter().filter_map(|slot| {
        let meta = slot.meta.load(Ordering::Acquire);
        (meta & OCCUPIED != 0).then(|| (journey_of(meta), slot.started_us.load(Ordering::Relaxed), meta))
    }).collect())
}
fn awaiting() -> bool { pending().iter().any(|(_, _, meta)| meta & AUDIO != 0) }

/// Starts timing `journey`. An audio journey ends at the next accepted PCM write.
pub fn begin(journey: Journey) { begin_at(journey, now_us()); }
pub fn begin_at(journey: Journey, started: u64) {
    record(Mark::JourneyBegin, journey as u32);
    let audio = matches!(journey, Journey::ShuffleSongs | Journey::ShuffleAlbums | Journey::PlayMusic | Journey::PlaySpoken | Journey::Resume | Journey::Skip);
    with_state(|state| {
        // A new audio action supersedes an older one: only the latest can start audio.
        for slot in &state.slots {
            let meta = slot.meta.load(Ordering::Acquire);
            if meta & OCCUPIED != 0 && ((audio && meta & AUDIO != 0) || journey_of(meta) == journey) {
                let _ = slot.meta.compare_exchange(meta, 0, Ordering::AcqRel, Ordering::Relaxed);
            }
        }
        let meta = OCCUPIED | if audio { AUDIO } else { 0 } | journey as u32;
        let claim = |slot: &Slot, from: u32| {
            if slot.meta.compare_exchange(from, RESERVED, Ordering::AcqRel, Ordering::Relaxed).is_err() { return false; }
            slot.started_us.store(started as u32, Ordering::Relaxed);
            slot.meta.store(meta, Ordering::Release);
            true
        };
        if state.slots.iter().any(|slot| claim(slot, 0)) { return; }
        // All four busy: replace the oldest.
        let now = now_us() as u32;
        if let Some(oldest) = state.slots.iter().max_by_key(|slot| now.wrapping_sub(slot.started_us.load(Ordering::Relaxed))) {
            let current = oldest.meta.load(Ordering::Acquire);
            if current & OCCUPIED != 0 { let _ = claim(oldest, current); }
        }
    });
}

/// Ends the pending `journey`, returning its latency; exactly one caller wins.
fn end_pending(journey: Journey) -> Option<u64> {
    let now = now_us() as u32;
    with_state(|state| state.slots.iter().find_map(|slot| {
        let meta = slot.meta.load(Ordering::Acquire);
        if meta & OCCUPIED == 0 || journey_of(meta) != journey { return None; }
        let started = slot.started_us.load(Ordering::Relaxed);
        slot.meta.compare_exchange(meta, 0, Ordering::AcqRel, Ordering::Relaxed).ok()?;
        Some(u64::from(now.wrapping_sub(started)))
    }))
}

/// Above this, a successful journey is reported in full (state and recent
/// history), not only as a distribution sample.
fn slow_us(journey: Journey) -> u64 {
    match journey {
        Journey::ShuffleSongs | Journey::ShuffleAlbums | Journey::Resume => 200_000,
        Journey::OpenPage | Journey::OpenInfo => 300_000,
        Journey::Skip => 500_000,
        _ => 1_000_000,
    }
}

fn metric(journey: Journey) -> Option<crate::diagnostics::health::Metric> {
    use crate::diagnostics::health::Metric as M;
    Some(match journey {
        Journey::ShuffleSongs => M::ShuffleToAudio, Journey::ShuffleAlbums => M::ShuffleAlbumsToAudio,
        Journey::PlayMusic => M::MusicPlayToAudio, Journey::PlaySpoken => M::SpokenPlayToAudio,
        Journey::Resume => M::ResumeToAudio, Journey::Skip => M::SkipToAudio,
        Journey::OpenPage => M::PageOpen, Journey::OpenInfo => M::InfoOpen,
        _ => return None,
    })
}

/// Ends `journey` with `outcome`, reporting its latency.
pub fn end(journey: Journey, outcome: Outcome) {
    if let Some(latency) = end_pending(journey) {
        record(Mark::JourneyEnd, journey as u32);
        let success = matches!(outcome, Outcome::AudioStarted | Outcome::Shown);
        if success { if let Some(metric) = metric(journey) { crate::diagnostics::health::journey(metric, latency); } }
        #[cfg(test)]
        SAMPLES.with(|s| s.borrow_mut().push((journey, latency)));
        if !success || latency >= slow_us(journey) {
            send(What::Journey, 0, journey, outcome, latency, 0);
        } else {
            log::info!("ux journey {journey:?} {outcome:?} {} ms", latency / 1000);
        }
    }
}

#[cfg(test)]
thread_local! { static SAMPLES: std::cell::RefCell<Vec<(Journey, u64)>> = const { std::cell::RefCell::new(Vec::new()) }; }
#[cfg(test)]
pub fn take_samples() -> Vec<(Journey, u64)> { SAMPLES.with(|s| std::mem::take(&mut *s.borrow_mut())) }

/// The player accepted PCM. Cheap when no audio journey is pending.
pub fn pcm_written() {
    if !awaiting() { return; }
    if let Some(journey) = pending_audio() { end(journey, Outcome::AudioStarted); }
}

fn current_journey() -> Journey {
    let now = now_us() as u32;
    pending().into_iter().min_by_key(|(_, started, _)| now.wrapping_sub(*started)).map_or(Journey::None, |(journey, _, _)| journey)
}

/// Stuck-state watchdog, run from the UI tick. Reports each journey once when it
/// passes [`STUCK_US`], and ends it as timed out at [`TIMEOUT_US`].
pub fn tick() {
    let now = now_us() as u32;
    let mut stuck = Vec::new();
    let mut expired = Vec::new();
    with_state(|state| {
        for slot in &state.slots {
            let meta = slot.meta.load(Ordering::Acquire);
            if meta & OCCUPIED == 0 { continue; }
            let age = u64::from(now.wrapping_sub(slot.started_us.load(Ordering::Relaxed)));
            if age >= TIMEOUT_US { expired.push(journey_of(meta)); }
            else if age >= STUCK_US && meta & STUCK == 0
                && slot.meta.compare_exchange(meta, meta | STUCK, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
                stuck.push((journey_of(meta), age));
            }
        }
    });
    for (journey, age) in stuck { send(What::Stuck, Code::Stuck as u16, journey, Outcome::None, age, 0); }
    for journey in expired { end(journey, Outcome::TimedOut); }
}

#[cfg(test)]
pub(crate) fn reset_for_test() {
    with_state(|state| {
        for slot in &state.slots { slot.meta.store(0, Ordering::Relaxed); }
        state.written.store(0, Ordering::Relaxed);
        state.underrun_s.store(0, Ordering::Relaxed);
        state.analog_version.store(0, Ordering::SeqCst);
    });
    take_sent();
    take_samples();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn problem_retains_last_analog_change_after_wheel_input_fills_the_history() {
        reset_for_test();
        analog_volume(61, 60, false, false, std::time::Duration::from_millis(3));
        for i in 0..100 { record(Mark::Input, i); }
        problem();
        let history = take_sent().pop().unwrap().recent;
        assert_eq!(history.len(), RECENT_LIMIT);
        assert!(history.iter().any(|e| e[1] == Mark::AnalogVolume as u32 && e[2] == (61 << 25 | 60 << 18 | 3)));
        assert!(history.iter().any(|e| e[1] == Mark::Problem as u32));
    }

    #[test]
    fn text_hash_matches_triage_tool() {
        // tools/ux_triage.py text_hash of the same literal.
        assert_eq!(text_hash("Audiobookshelf streaming needs a connection"), 0xabfb2ef2);
        assert_eq!(text_hash("page 12 of 40"), text_hash("page 3 of 7"));
    }

    #[test]
    fn raw_errors_become_plain_messages_with_codes() {
        reset_for_test();
        for (raw, code) in [
            ("Invalid file descriptor (os error 9)", Code::Descriptor),
            ("connection refused (os error 111)", Code::NetworkUnreachable),
            ("Unauthorized (HTTP 401)", Code::Auth),
            ("stream: connection reset by peer", Code::StreamInterrupted),
            ("Audiobookshelf account changed; select this book again (generation 3)", Code::AbsChanged),
            ("card: No space left on device (os error 28)", Code::CardFull),
            ("Audiobookshelf readiness worker stopped", Code::AbsUnreachable),
        ] {
            let shown = present_in(raw, Journey::PlaySpoken);
            assert_eq!(shown, code.message(), "{raw}");
            assert!(!shown.chars().any(|c| c.is_ascii_digit()), "{shown}");
            let sent = take_sent();
            assert_eq!(sent.len(), 1);
            assert_eq!((sent[0].what, sent[0].code), (What::Error, code as u16));
            assert_eq!(sent[0].detail, text_hash(raw));
        }
    }

    #[test]
    fn notices_pass_through_and_plain_failures_keep_their_words() {
        reset_for_test();
        assert_eq!(present("Sleep timer restarted"), "Sleep timer restarted");
        assert_eq!(present("Opening USB drive…"), "Opening USB drive…");
        assert!(take_sent().is_empty());
        assert_eq!(present("Asking the server to scan"), "Asking the server to scan");
        assert_eq!(present("Connect USB to a computer first"), "Connect USB to a computer first");
        assert!(take_sent().is_empty());
        assert_eq!(present("Radio needs a connection"), "Radio needs a connection");
        assert_eq!(take_sent()[0].code, Code::Offline as u16);
        assert_eq!(present("No server set up"), "No server set up");
        assert_eq!(take_sent()[0].code, Code::NavidromeNotConfigured as u16);
        assert_eq!(present("No downloaded songs"), "No downloaded songs");
        assert_eq!(take_sent()[0].code, Code::NoDownloadedSongs as u16);
        assert_eq!(present("Not downloaded"), "Not downloaded");
        assert_eq!(take_sent()[0].code, Code::NotDownloaded as u16);
    }

    /// A fixed message shown again (a toast of a page's own failure, a
    /// player error) keeps its code instead of reading as an ordinary notice.
    #[test]
    fn every_code_message_classifies_as_its_own_code() {
        for code in Code::ALL {
            assert_eq!(Code::classify(code.message()), Some(code), "{}", code.message());
            let prefixed = format!("open: {}", code.message());
            assert_eq!(Code::classify(&prefixed), Some(code), "{prefixed}");
            assert_eq!(present(&prefixed), code.message(), "the prefix is not shown");
        }
    }

    /// A 503 from Audiobookshelf is not "Can't reach Audiobookshelf".
    #[test]
    fn audiobookshelf_answering_with_a_failure_is_not_unreachable() {
        use subsonic::audiobookshelf::Error;
        assert_eq!(Code::classify(&Error::Server.to_string()), Some(Code::AbsServerError));
        assert_eq!(Code::classify(&Error::Network.to_string()), Some(Code::AbsUnreachable));
        assert!(Code::transient(Code::AbsServerError as u16), "playback stopped by it resumes by itself");
        assert_eq!(crate::ui::state::hint(Code::AbsServerError.message()), None, "the server is there: no offline hint");
    }

    #[test]
    fn every_code_message_is_short_plain_and_unique() {
        let all = [Code::Unknown, Code::Offline, Code::NetworkUnreachable, Code::NavidromeUnreachable, Code::AbsUnreachable,
            Code::AbsChanged, Code::AbsNotConfigured, Code::NavidromeNotConfigured, Code::ServerError, Code::Timeout,
            Code::StreamInterrupted, Code::NotDownloaded, Code::FileMissing, Code::CardUnavailable, Code::CardFull,
            Code::Descriptor, Code::CardError, Code::Auth, Code::AbsServerError, Code::NoSongs, Code::NoDownloadedSongs, Code::PlaybackChanged, Code::Decode,
            Code::Output, Code::Database, Code::CacheDelete, Code::Stuck];
        let mut seen = std::collections::HashSet::new();
        for code in all {
            let m = code.message();
            assert!(m.len() <= 36 && !m.chars().any(|c| c.is_ascii_digit()) && !m.contains(':'), "{m}");
            assert!(seen.insert(m), "{m}");
        }
    }

    #[test]
    fn audio_journey_ends_at_first_pcm_and_newer_action_supersedes() {
        reset_for_test();
        begin(Journey::PlayMusic);
        begin(Journey::ShuffleSongs);
        pcm_written();
        assert!(take_sent().is_empty(), "a fast success is a distribution sample only");
        let samples = take_samples();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].0, Journey::ShuffleSongs);
        pcm_written();
        assert!(take_samples().is_empty(), "one sample per journey");
        begin_at(Journey::ShuffleSongs, now_us().saturating_sub(250_000));
        pcm_written();
        let sent = take_sent();
        assert_eq!((sent[0].what, sent[0].outcome), (What::Journey, Outcome::AudioStarted as u8), "slow successes are reported in full");
    }

    #[test]
    fn watchdog_reports_stuck_once_then_times_out() {
        reset_for_test();
        let now = now_us();
        begin_at(Journey::OpenPage, now.saturating_sub(STUCK_US + 1));
        tick(); tick();
        let sent = take_sent();
        assert_eq!(sent.len(), 1);
        assert_eq!((sent[0].what, sent[0].journey), (What::Stuck, Journey::OpenPage as u8));
        begin_at(Journey::OpenInfo, now.saturating_sub(TIMEOUT_US + 1));
        tick();
        let sent = take_sent();
        assert!(sent.iter().any(|r| r.what == What::Journey && r.outcome == Outcome::TimedOut as u8 && r.journey == Journey::OpenInfo as u8));
    }

    #[test]
    fn failure_during_a_journey_ends_it_and_carries_recent_history() {
        reset_for_test();
        input(7);
        begin(Journey::PlaySpoken);
        let _ = present("Audiobookshelf streaming needs a connection");
        let sent = take_sent();
        assert_eq!(sent[0].journey, Journey::PlaySpoken as u8);
        assert!(sent[0].recent.iter().any(|e| e[1] == Mark::Input as u32 && e[2] == 7));
        pcm_written();
        assert!(take_sent().is_empty(), "the failed journey cannot later report audio");
    }

    #[test]
    fn user_stops_are_recorded_but_not_reported() {
        reset_for_test();
        stop(StopCause::User, 0);
        stop(StopCause::EndOfQueue, 0);
        assert!(take_sent().is_empty());
        stop(StopCause::Unexplained, 0);
        assert_eq!(take_sent()[0].code, StopCause::Unexplained as u16);
    }

    #[test]
    fn stall_marks_pack_their_fields() {
        let open = track_open_value(3, true, 20_000, 742);
        assert_eq!((open >> 29, open >> 28 & 1, open >> 14 & 0x3fff, open & 0x3fff), (3, 1, 0x3fff, 742));
        assert_eq!(stream_open_value(5 * 1024 * 1024, Some(20 * 1024 * 1024)), 25 << 24 | 5 * 1024);
        assert_eq!(stream_open_value(0, None) >> 24, 127);
        assert_eq!(tagged(6, 1 << 30), 6 << 24 | 0xff_ffff);
    }

    #[test]
    fn short_stream_waits_are_not_recorded() {
        reset_for_test();
        stream_waited(std::time::Duration::from_millis(STREAM_WAIT_MS - 1));
        stream_waited(std::time::Duration::from_millis(1_500));
        let waits: Vec<u32> = recent().iter().filter(|e| e[1] == Mark::StreamWait as u32).map(|e| e[2]).collect();
        assert_eq!(waits, vec![1_500]);
    }

    #[test]
    fn every_underrun_is_marked_and_reports_are_spaced() {
        reset_for_test();
        for _ in 0..5 { underrun(32); }
        let sent = take_sent();
        assert_eq!(sent.len(), 1, "a burst of underruns sends one report");
        assert_eq!((sent[0].what, sent[0].code, sent[0].detail), (What::Recovered, Code::Output as u16, 32));
        assert_eq!(recent().iter().filter(|e| e[1] == Mark::Underrun as u32 && e[2] == 32).count(), 5);
    }
}
