#![forbid(unsafe_code)]
//! Bounded RAM playback telemetry: what the player, the output and the cache
//! transfers were doing around an output recovery, on CLOCK_BOOTTIME (the
//! base of `metrics::uptime` and /proc/uptime) so a rig aligns it with meter
//! samples. Two fixed rings: events (requests, completions, recoveries,
//! stalls, transfer lifecycle) and one-second activity windows (read, decode
//! and write time, buffered audio, transfer bytes, dirty/writeback pages).
//! Numbers only: serials assigned here, durations, counts and fixed codes.
//! No song IDs, titles, paths, addresses or log text.
//!
//! Off unless `[diagnostics] playtrace = true` in config.toml (or
//! `Q2ND_PLAYTRACE=1`): the Q2 cost is not measured yet.
//!
//! Nothing here owns a timer or a polling thread. Windows advance only when
//! the player decodes or a transfer writes; a paused, idle player records
//! nothing. The dump thread blocks in `open` on a FIFO until asked.
//!
//! The one shared lock guards fixed-size memory updates only: no file I/O,
//! allocation or formatting happens under it, so the player's per-packet
//! path never waits behind a file read. /proc/meminfo is read by transfer
//! threads outside the lock.
//!
//! Clocks: every timestamp is CLOCK_BOOTTIME. Durations taken around the
//! player's own steps are differences of those timestamps. Source-read
//! durations use `Instant` (CLOCK_MONOTONIC), which does not advance in
//! suspend: a suspend inside a read is absent from the read's duration and
//! present in the write gap that spans it.
//!
//! Endpoints (docs/playtrace.md has the full table):
//! - `request`: `PlayerHandle::send` queued a play-control command.
//! - `handled`: the player thread began applying it (same serial).
//! - `first-pcm`: the decoder returned the first frames after it.
//! - `first-submit`: the output accepted the first frames after it; `c` is
//!   the audio already queued, so audible time is no earlier than `t + c`.
//! - `paused` / `resumed`: the output's pause or resume call returned.
//! - `status`: a status snapshot with a changed play state was stored for the
//!   UI and its notify callback ran. Not a drawn frame.
//!
//! Analog output is not observed by any of these.
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{FileExt, FileTypeExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

pub(crate) const EVENTS: usize = 512;
pub(crate) const WINDOWS: usize = 512;
pub(crate) const WINDOW_US: u64 = 1_000_000;
/// At most one event of each rate-limited kind (slow read, slow decode,
/// slow write, low buffer, write gap) in this long; the windows keep the
/// maxima and the track summary counts what was withheld.
pub(crate) const EVENT_INTERVAL_US: u64 = 1_000_000;
/// A wait for download or network bytes this long is a slow-read event.
pub(crate) const SLOW_FILL_US: u32 = 100_000;
/// Stalls at or over these become events with their own timestamp.
pub(crate) const SLOW_READ_US: u32 = 20_000;
pub(crate) const SLOW_DECODE_US: u32 = 20_000;
pub(crate) const SLOW_WRITE_US: u32 = 300_000;
/// Longest tolerated interval between two output writes of one uninterrupted run.
pub(crate) const WRITE_GAP_US: u32 = 300_000;
pub(crate) const SLOW_CARD_WRITE_US: u32 = 50_000;
pub(crate) const LOW_BUFFER_MS: u32 = 100;
/// Absent value in a window column.
pub(crate) const NONE: u32 = u32::MAX;

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// a = command code, b = request serial.
    Request = 1,
    /// a = command code, b = request serial.
    Handled,
    /// a = track serial, b = origin code, c = source open ms << 16 | probe ms (each capped at 65535).
    Open,
    /// a = track serial, b = request serial.
    FirstPcm,
    /// a = track serial, b = request serial, c = audio queued before it, ms.
    FirstSubmit,
    /// a = serial of the pause or play-pause request being applied (0 when
    /// the pause has another cause: sleep, an output fault, a failed save),
    /// b = 1 when the queued audio was retained, c = frames held.
    Paused,
    /// a = serial of the play, play-pause or resume request being applied
    /// (0 for a resume after an output route change).
    Resumed,
    /// a = request serial, b = play state code, c = track serial.
    Status,
    /// a = track serial, b = reason code (1 end of file, 2 closed, 3 output released).
    TrackEnd,
    /// a = ALSA code (negated errno, as a positive number), b = audio queued before the write, ms, c = track serial.
    Recovery,
    /// The file part of one read reached SLOW_READ_US, or its wait for
    /// download or network bytes reached SLOW_FILL_US. a = read
    /// microseconds, b = of which waiting for bytes (at most a), c = track serial.
    SlowRead,
    /// a = decode microseconds, c = track serial.
    SlowDecode,
    /// a = microseconds blocked in the output write, b = audio queued before it, ms, c = track serial.
    SlowWrite,
    /// a = audio queued before the write, ms (crossed under LOW_BUFFER_MS), c = track serial.
    LowBuffer,
    /// a = sample rate, b = channels.
    SinkOpen,
    SinkClose,
    /// a = transfer serial, b = 1 for a player stream, 0 for fetch-ahead.
    XferStart,
    /// a = transfer serial, b = readers after joining, c = 1 when it was orphaned (a rejoin).
    XferJoin,
    /// a = transfer serial, b = percent downloaded (255 unknown), c = KiB written.
    XferOrphan,
    /// Cancel requested. a = transfer serial, b = percent, c = KiB written.
    XferCancel,
    /// Transport ended. a = transfer serial, b = outcome (1 complete, 2 stopped by cancel, 3 failed), c = KiB written.
    /// Outcome 1 has never certified cache publication; see CachePublish.
    XferEnd,
    /// a = songs in the burst.
    FetchAhead,
    /// a = reason (1 complete, 2 newer request, 3 offline).
    FetchAheadEnd,
    /// One card write of a transfer blocked. a = microseconds, b = KiB written so far by that transfer.
    SlowCardWrite,
    /// The player thread took this long from the end of one output write to
    /// the start of the next, with no request, pause or resume between:
    /// everything it did there, timed or not. a = microseconds,
    /// b = audio queued before the later write, ms, c = track serial.
    WriteGap,
    /// The radio went down for cached playback. a = audio queued, ms.
    Doze,
    /// The radio was asked up. a = reason (q2_machines::doze::WakeReason, in
    /// declaration order from 1), b = audio queued, ms.
    Wake,
    /// After rename and checked SQLite insertion. a = transfer serial,
    /// b = CachePublication (1 published, 2 rename failed, 3 index failed),
    /// c = KiB written. Hits/joins do not emit another publication.
    CachePublish,
    /// a = volume command serial (0 for automatic gain selection), b = volume,
    /// c = cached PREVIOUS write's delay in frames, NONE if unavailable; not current occupancy.
    VolumeBegin,
    /// a = serial, b = logical half-dB attenuation steps, c = flags:
    /// headphones1, running-retarget2, already-paused4, previous-hardware8,
    /// same-compensation16, running-PCM-ramp32, mixed-ramp64.
    /// c bits 8..15 carry the planned physical codec attenuation.
    VolumePlan,
    /// a = serial, b/c = old/planned f32 software coefficient IEEE bits.
    VolumeGain,
    /// a = serial, b = VolumePhase (bit31 marks successful return),
    /// c = retained frames (NONE at entry or when unavailable). Paired timestamps
    /// bound owner work, not analog/audible completion. Plan begins at VolumeBegin.
    VolumePhase,
    /// a = serial, b = 0 on success or failed VolumePhase, c = 0 success / 1 error.
    VolumeEnd,
}

impl Kind {
    #[cfg(test)]
    const ALL: [Kind; 33] = [Kind::Request, Kind::Handled, Kind::Open, Kind::FirstPcm, Kind::FirstSubmit, Kind::Paused,
        Kind::Resumed, Kind::Status, Kind::TrackEnd, Kind::Recovery, Kind::SlowRead, Kind::SlowDecode, Kind::SlowWrite,
        Kind::LowBuffer, Kind::SinkOpen, Kind::SinkClose, Kind::XferStart, Kind::XferJoin, Kind::XferOrphan,
        Kind::XferCancel, Kind::XferEnd, Kind::FetchAhead, Kind::FetchAheadEnd, Kind::SlowCardWrite, Kind::WriteGap,
        Kind::Doze, Kind::Wake, Kind::CachePublish, Kind::VolumeBegin, Kind::VolumePlan, Kind::VolumeGain, Kind::VolumePhase, Kind::VolumeEnd];
    pub(crate) fn name(self) -> &'static str {
        match self {
            Kind::Request => "request", Kind::Handled => "handled", Kind::Open => "open",
            Kind::FirstPcm => "first-pcm", Kind::FirstSubmit => "first-submit", Kind::Paused => "paused",
            Kind::Resumed => "resumed", Kind::Status => "status", Kind::TrackEnd => "track-end",
            Kind::Recovery => "recovery", Kind::SlowRead => "slow-read", Kind::SlowDecode => "slow-decode",
            Kind::SlowWrite => "slow-write", Kind::LowBuffer => "low-buffer", Kind::SinkOpen => "sink-open",
            Kind::SinkClose => "sink-close", Kind::XferStart => "xfer-start", Kind::XferJoin => "xfer-join",
            Kind::XferOrphan => "xfer-orphan", Kind::XferCancel => "xfer-cancel", Kind::XferEnd => "xfer-end",
            Kind::FetchAhead => "fetch-ahead", Kind::FetchAheadEnd => "fetch-ahead-end",
            Kind::SlowCardWrite => "slow-card-write",
            Kind::WriteGap => "write-gap",
            Kind::Doze => "doze", Kind::Wake => "wake", Kind::CachePublish => "cache-publish",
            Kind::VolumeBegin => "volume-begin", Kind::VolumePlan => "volume-plan",
            Kind::VolumeGain => "volume-gain", Kind::VolumePhase => "volume-phase", Kind::VolumeEnd => "volume-end",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Event {
    pub seq: u32,
    pub t_us: u64,
    pub kind: Kind,
    pub a: u32,
    pub b: u32,
    pub c: u32,
}

/// One second of activity. Sums and maxima are microseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Window {
    pub seq: u32,
    pub t_us: u64,
    pub dur_us: u32,
    pub packets: u32,
    pub frames: u32,
    pub rate: u32,
    pub read_sum: u32,
    pub read_max: u32,
    pub fill_sum: u32,
    pub fill_max: u32,
    pub decode_sum: u32,
    pub decode_max: u32,
    pub render_max: u32,
    pub write_sum: u32,
    pub write_max: u32,
    pub buf_min_ms: u32,
    pub buf_last_ms: u32,
    pub ahead_min_kib: u32,
    pub xfer_bytes: u32,
    pub xfer_active: u32,
    pub card_write_max: u32,
    pub dirty_kb: u32,
    pub writeback_kb: u32,
    /// Longest interval between two output writes (see `Kind::WriteGap`).
    pub gap_max: u32,
}

impl Window {
    fn new(t_us: u64) -> Window {
        Window { seq: 0, t_us, dur_us: 0, packets: 0, frames: 0, rate: 0, read_sum: 0, read_max: 0, fill_sum: 0, fill_max: 0,
            decode_sum: 0, decode_max: 0, render_max: 0, write_sum: 0, write_max: 0, buf_min_ms: NONE, buf_last_ms: NONE,
            ahead_min_kib: NONE, xfer_bytes: 0, xfer_active: 0, card_write_max: 0, dirty_kb: NONE, writeback_kb: NONE, gap_max: 0 }
    }
    fn used(&self) -> bool { self.packets > 0 || self.frames > 0 || self.xfer_bytes > 0 }
}

pub(crate) const WINDOW_COLUMNS: &str = "seq t_us dur_us packets frames rate read_sum_us read_max_us fill_sum_us fill_max_us \
decode_sum_us decode_max_us render_max_us write_sum_us write_max_us buf_min_ms buf_last_ms ahead_min_kib xfer_bytes \
xfer_active card_write_max_us dirty_kb writeback_kb gap_max_us";

/// A fixed-capacity ring that counts what it overwrote.
struct Ring<T: Copy> {
    items: Vec<T>,
    capacity: usize,
    total: u32,
}

impl<T: Copy> Ring<T> {
    fn new(capacity: usize) -> Ring<T> { Ring { items: Vec::with_capacity(capacity), capacity, total: 0 } }
    /// The sequence number the pushed item gets.
    fn push(&mut self, item: impl FnOnce(u32) -> T) {
        let seq = self.total;
        let item = item(seq);
        if self.items.len() < self.capacity { self.items.push(item); }
        else { self.items[seq as usize % self.capacity] = item; }
        self.total = self.total.wrapping_add(1);
    }
    fn lost(&self) -> u32 { self.total.saturating_sub(self.capacity as u32) }
    /// Oldest first.
    fn iter(&self) -> impl Iterator<Item = &T> {
        let split = if self.items.len() < self.capacity { 0 } else { self.total as usize % self.capacity };
        self.items[split..].iter().chain(self.items[..split].iter())
    }
}

struct Inner {
    events: Ring<Event>,
    windows: Ring<Window>,
    window: Window,
    /// The last activity in the open window.
    window_last: u64,
    requests: u32,
    transfers: u32,
    xfer_active: u32,
    /// When a transfer thread last took a Dirty/Writeback sample.
    mem_sampled_us: u64,
}

/// /proc/meminfo, kept open. Only transfer threads lock this.
#[derive(Default)]
struct MemSampler {
    file: Option<File>,
    failed: bool,
}

pub(crate) struct Trace {
    enabled: AtomicBool,
    inner: Mutex<Inner>,
    sampler: Mutex<MemSampler>,
    meminfo: PathBuf,
    capacity: (usize, usize),
    clock: Option<Arc<dyn Fn() -> u64 + Send + Sync>>,
}

pub(crate) fn boottime_us() -> u64 {crate::platform::system::boottime_us().unwrap_or(0)}

fn micros(from: u64, to: u64) -> u32 { to.saturating_sub(from).min(u64::from(u32::MAX - 1)) as u32 }

/// `Dirty:` and `Writeback:` in kB from /proc/meminfo text.
fn parse_meminfo(text: &[u8]) -> Option<(u32, u32)> {
    let field = |name: &[u8]| -> Option<u32> {
        let at = text.windows(name.len()).enumerate().find(|(i, w)| *w == name && (*i == 0 || text[i - 1] == b'\n'))?.0;
        let digits: Vec<u8> = text[at + name.len()..].iter().copied().skip_while(|b| *b == b' ' || *b == b'\t')
            .take_while(u8::is_ascii_digit).collect();
        std::str::from_utf8(&digits).ok()?.parse().ok()
    };
    Some((field(b"Dirty:")?, field(b"Writeback:")?))
}

impl Trace {
    pub(crate) fn new(enabled: bool) -> Trace { Trace::with(enabled, EVENTS, WINDOWS, "/proc/meminfo".into(), None) }

    pub(crate) fn with(enabled: bool, events: usize, windows: usize, meminfo: PathBuf,
        clock: Option<Arc<dyn Fn() -> u64 + Send + Sync>>) -> Trace {
        Trace { enabled: AtomicBool::new(enabled), meminfo, clock, capacity: (events.max(1), windows.max(1)),
            sampler: Mutex::default(), inner: Mutex::new(Inner {
            events: Ring::new(events.max(1)), windows: Ring::new(windows.max(1)), window: Window::new(0), window_last: 0, requests: 0,
            transfers: 0, xfer_active: 0, mem_sampled_us: 0 }) }
    }

    pub(crate) fn enabled(&self) -> bool { self.enabled.load(Ordering::Relaxed) }

    /// Before the player and the library start: nothing recorded earlier is kept consistent across a change.
    pub(crate) fn set_enabled(&self, on: bool) { self.enabled.store(on, Ordering::Relaxed); }

    /// Boottime microseconds; 0 when disabled (callers skip their arithmetic).
    pub(crate) fn now(&self) -> u64 {
        if !self.enabled() { return 0; }
        match &self.clock { Some(clock) => clock(), None => boottime_us() }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> { self.inner.lock().unwrap_or_else(|e| e.into_inner()) }

    pub(crate) fn event(&self, kind: Kind, a: u32, b: u32, c: u32) {
        if !self.enabled() { return; }
        self.event_at(self.now(), kind, a, b, c);
    }

    pub(crate) fn event_at(&self, t_us: u64, kind: Kind, a: u32, b: u32, c: u32) {
        if !self.enabled() { return; }
        self.lock().events.push(|seq| Event { seq, t_us, kind, a, b, c });
    }

    /// A play-control command was queued. Call in the order commands are
    /// sent; the player numbers what it handles the same way.
    pub(crate) fn request(&self, code: u32) -> u32 {
        if !self.enabled() { return 0; }
        let t_us = self.now();
        let mut inner = self.lock();
        inner.requests = inner.requests.wrapping_add(1);
        let serial = inner.requests;
        inner.events.push(|seq| Event { seq, t_us, kind: Kind::Request, a: code, b: serial, c: 0 });
        serial
    }

    /// Closes the current window when its second is over. A measurement
    /// belongs to the window in which it completed, so a stall longer than a
    /// window shows in the window that starts when it ends; `dur_us` runs to
    /// the window's last activity. Memory updates only.
    fn roll(&self, inner: &mut Inner, now: u64) {
        let last = std::mem::replace(&mut inner.window_last, now);
        if inner.window.t_us == 0 { inner.window = Window::new(now); return; }
        if now.saturating_sub(inner.window.t_us) < WINDOW_US { return; }
        let mut done = std::mem::replace(&mut inner.window, Window::new(now));
        if !done.used() { return; }
        done.dur_us = micros(done.t_us, last);
        done.xfer_active = inner.xfer_active;
        inner.windows.push(|seq| Window { seq, ..done });
    }

    /// Called by a transfer thread with the trace lock released.
    fn sample_meminfo(&self, _blocking: &crate::rt::Blocking) -> Option<(u32, u32)> {
        let mut sampler = self.sampler.lock().unwrap_or_else(|e| e.into_inner());
        if sampler.failed { return None; }
        if sampler.file.is_none() {
            match File::open(&self.meminfo) { Ok(file) => sampler.file = Some(file), Err(_) => { sampler.failed = true; return None; } }
        }
        let mut text = [0u8; 2048];
        // Dirty and Writeback are within the first kilobyte; one pread, no seek.
        let n = sampler.file.as_ref()?.read_at(&mut text, 0).ok()?;
        let parsed = parse_meminfo(&text[..n]);
        if parsed.is_none() { sampler.failed = true; }
        parsed
    }

    /// One demuxed and decoded packet.
    pub(crate) fn decoded(&self, now: u64, read: ReadSample, decode_us: u32) {
        if !self.enabled() { return; }
        let mut inner = self.lock();
        self.roll(&mut inner, now);
        let w = &mut inner.window;
        w.packets += 1;
        w.read_sum = w.read_sum.saturating_add(read.sum_us);
        w.read_max = w.read_max.max(read.max_us);
        w.fill_sum = w.fill_sum.saturating_add(read.fill_us);
        w.fill_max = w.fill_max.max(read.fill_us);
        w.decode_sum = w.decode_sum.saturating_add(decode_us);
        w.decode_max = w.decode_max.max(decode_us);
        if read.ahead_min_kib != NONE { w.ahead_min_kib = w.ahead_min_kib.min(read.ahead_min_kib); }
    }

    /// One output write.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn written(&self, now: u64, frames: u32, rate: u32, render_us: u32, write_us: u32, buffered_ms: u32, gap_us: u32) {
        if !self.enabled() { return; }
        let mut inner = self.lock();
        self.roll(&mut inner, now);
        let w = &mut inner.window;
        w.frames = w.frames.saturating_add(frames);
        w.rate = rate;
        w.render_max = w.render_max.max(render_us);
        w.write_sum = w.write_sum.saturating_add(write_us);
        w.write_max = w.write_max.max(write_us);
        w.gap_max = w.gap_max.max(gap_us);
        if buffered_ms != NONE { w.buf_min_ms = w.buf_min_ms.min(buffered_ms); w.buf_last_ms = buffered_ms; }
    }

    /// A transfer started; its serial.
    pub(crate) fn transfer_start(&self, playback: bool) -> u32 {
        if !self.enabled() { return 0; }
        let t_us = self.now();
        let mut inner = self.lock();
        inner.transfers = inner.transfers.wrapping_add(1);
        inner.xfer_active += 1;
        let serial = inner.transfers;
        inner.events.push(|seq| Event { seq, t_us, kind: Kind::XferStart, a: serial, b: playback as u32, c: 0 });
        serial
    }

    pub(crate) fn transfer_end(&self, serial: u32, outcome: u32, written: u64) {
        if !self.enabled() { return; }
        let t_us = self.now();
        let mut inner = self.lock();
        inner.xfer_active = inner.xfer_active.saturating_sub(1);
        inner.events.push(|seq| Event { seq, t_us, kind: Kind::XferEnd, a: serial, b: outcome, c: kib(written) });
    }

    /// Bytes a transfer wrote to the card and how long that write blocked.
    /// Called on the transfer's own thread, which also takes the
    /// Dirty/Writeback sample (at most one a second, outside the lock): the
    /// newest sample taken during a window is that window's value.
    pub(crate) fn transfer_wrote(&self, blocking: &crate::rt::Blocking, started: u64, bytes: usize, total_written: u64) {
        if !self.enabled() { return; }
        let now = self.now();
        let blocked = micros(started, now);
        let sample_due = {
            let mut inner = self.lock();
            self.roll(&mut inner, now);
            let w = &mut inner.window;
            w.xfer_bytes = w.xfer_bytes.saturating_add(bytes as u32);
            w.card_write_max = w.card_write_max.max(blocked);
            if blocked >= SLOW_CARD_WRITE_US {
                inner.events.push(|seq| Event { seq, t_us: now, kind: Kind::SlowCardWrite, a: blocked, b: kib(total_written), c: 0 });
            }
            let due = inner.mem_sampled_us == 0 || now.saturating_sub(inner.mem_sampled_us) >= WINDOW_US;
            if due { inner.mem_sampled_us = now.max(1); }
            due
        };
        if !sample_due { return; }
        let Some((dirty, writeback)) = self.sample_meminfo(blocking) else { return };
        let mut inner = self.lock();
        inner.window.dirty_kb = dirty;
        inner.window.writeback_kb = writeback;
    }

    pub(crate) fn snapshot(&self) -> Snapshot {
        // Allocated before the lock: under it, only bounded copies.
        let mut events = Vec::with_capacity(self.capacity.0);
        let mut windows = Vec::with_capacity(self.capacity.1);
        let now_us = match &self.clock { Some(clock) => clock(), None => boottime_us() };
        let inner = self.lock();
        events.extend(inner.events.iter().copied());
        windows.extend(inner.windows.iter().copied());
        Snapshot {
            now_us, enabled: self.enabled(),
            events, events_total: inner.events.total, events_lost: inner.events.lost(),
            events_capacity: inner.events.capacity as u32,
            windows, windows_total: inner.windows.total, windows_lost: inner.windows.lost(),
            windows_capacity: inner.windows.capacity as u32,
            current: inner.window.used().then_some(Window { dur_us: micros(inner.window.t_us, inner.window_last), ..inner.window }),
        }
    }

    pub(crate) fn losses(&self) -> (u32, u32) {
        let inner = self.lock();
        (inner.events.lost(), inner.windows.lost())
    }
}

fn kib(bytes: u64) -> u32 { (bytes / 1024).min(u64::from(u32::MAX - 1)) as u32 }

pub(crate) struct Snapshot {
    pub now_us: u64,
    pub enabled: bool,
    pub events: Vec<Event>,
    pub events_total: u32,
    pub events_lost: u32,
    pub events_capacity: u32,
    pub windows: Vec<Window>,
    pub windows_total: u32,
    pub windows_lost: u32,
    pub windows_capacity: u32,
    /// The window still open at the snapshot.
    pub current: Option<Window>,
}

fn window_line(prefix: &str, w: &Window) -> String {
    let n = |v: u32| if v == NONE { "-1".to_string() } else { v.to_string() };
    let dur = w.dur_us;
    format!("{prefix} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {}\n", w.seq, w.t_us, dur, w.packets,
        w.frames, w.rate, w.read_sum, w.read_max, w.fill_sum, w.fill_max, w.decode_sum, w.decode_max, w.render_max,
        w.write_sum, w.write_max, n(w.buf_min_ms), n(w.buf_last_ms), n(w.ahead_min_kib), w.xfer_bytes, w.xfer_active,
        w.card_write_max, n(w.dirty_kb), n(w.writeback_kb), w.gap_max)
}

impl Snapshot {
    /// The dump: line-oriented, numbers and fixed names only. `lost` counts
    /// records overwritten before this dump; the `end` line carries the
    /// number of lines written, so a truncated file is detectable.
    pub(crate) fn text(&self) -> String {
        let mut out = String::with_capacity(96 * (self.events.len() + self.windows.len() + 8));
        out.push_str(&format!("playtrace 1 pid={} now_us={} enabled={} window_us={WINDOW_US}\n", std::process::id(),
            self.now_us, self.enabled as u8));
        out.push_str(&format!("events capacity={} total={} lost={}\n", self.events_capacity, self.events_total, self.events_lost));
        for e in &self.events { out.push_str(&format!("E {} {} {} {} {} {}\n", e.seq, e.t_us, e.kind.name(), e.a, e.b, e.c)); }
        out.push_str(&format!("windows capacity={} total={} lost={}\n", self.windows_capacity, self.windows_total, self.windows_lost));
        out.push_str(&format!("# W {WINDOW_COLUMNS}\n"));
        for w in &self.windows { out.push_str(&window_line("W", w)); }
        if let Some(w) = &self.current { out.push_str(&window_line("P", &Window { seq: self.windows_total, ..*w })); }
        out.push_str(&format!("end {} {}\n", self.events.len(), self.windows.len() + self.current.is_some() as usize));
        out
    }
}

static GLOBAL: OnceLock<Arc<Trace>> = OnceLock::new();

/// The process's trace. Off until `configure` turns it on.
pub(crate) fn global() -> Arc<Trace> {
    GLOBAL.get_or_init(|| Arc::new(Trace::new(false))).clone()
}

/// `Q2ND_PLAYTRACE` = 1 or 0 overrides the configuration key (rigs that
/// control the app's environment); otherwise `[diagnostics] playtrace`.
pub(crate) fn wanted(configured: bool, environment: Option<&std::ffi::OsStr>) -> bool {
    match environment.and_then(|v| v.to_str()) { Some("1") => true, Some("0") => false, _ => configured }
}

/// Call once at start, before the library and the player exist.
pub(crate) fn configure(configured: bool) {
    global().set_enabled(wanted(configured, std::env::var_os("Q2ND_PLAYTRACE").as_deref()));
}

/// What a read measured, moved from the reading thread to the player.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ReadSample {
    pub sum_us: u32,
    pub max_us: u32,
    /// Of `sum_us`, waiting for a download to reach the read position.
    pub fill_us: u32,
    /// Least downloaded-but-unread KiB seen ahead of the read position.
    pub ahead_min_kib: u32,
}

#[derive(Default)]
pub(crate) struct ReadStats {
    sum_us: AtomicU32,
    max_us: AtomicU32,
    fill_us: AtomicU32,
    ahead_min_kib: AtomicU32,
}

impl ReadStats {
    pub(crate) fn new() -> Arc<ReadStats> {
        Arc::new(ReadStats { ahead_min_kib: AtomicU32::new(NONE), ..Default::default() })
    }
    pub(crate) fn take(&self) -> ReadSample {
        ReadSample { sum_us: self.sum_us.swap(0, Ordering::Relaxed), max_us: self.max_us.swap(0, Ordering::Relaxed),
            fill_us: self.fill_us.swap(0, Ordering::Relaxed), ahead_min_kib: self.ahead_min_kib.swap(NONE, Ordering::Relaxed) }
    }
}

thread_local! {
    /// Set by a reader inside a timed read: (wait for bytes us, KiB ahead).
    static FILL: std::cell::Cell<(u32, u32)> = const { std::cell::Cell::new((0, NONE)) };
    /// This thread is inside a `TimedSource` read.
    static TIMING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Start of a wait for download or network bytes; None (and no clock read)
/// unless this read is being timed.
pub(crate) fn fill_timer() -> Option<std::time::Instant> {
    TIMING.with(|t| t.get()).then(std::time::Instant::now)
}

/// The wait begun at `fill_timer` ended.
pub(crate) fn note_fill_wait(started: Option<std::time::Instant>) {
    let Some(started) = started else { return };
    let us = started.elapsed().as_micros().min(u128::from(u32::MAX - 1)) as u32;
    FILL.with(|f| { let (wait, ahead) = f.get(); f.set((wait.saturating_add(us), ahead)); });
}

/// Downloaded bytes beyond the read position when this read was served.
pub(crate) fn note_ahead(bytes: u64) {
    if !TIMING.with(|t| t.get()) { return; }
    FILL.with(|f| { let (wait, ahead) = f.get(); f.set((wait, ahead.min(kib(bytes)))); });
}

/// Times a playback source's reads and seeks for the player.
pub(crate) struct TimedSource {
    inner: Box<dyn symphonia::core::io::MediaSource>,
    stats: Arc<ReadStats>,
}

impl TimedSource {
    pub(crate) fn new(inner: Box<dyn symphonia::core::io::MediaSource>, stats: Arc<ReadStats>) -> TimedSource {
        TimedSource { inner, stats }
    }
    fn timed<T>(&mut self, work: impl FnOnce(&mut dyn symphonia::core::io::MediaSource) -> T) -> T {
        FILL.with(|f| f.set((0, NONE)));
        TIMING.with(|t| t.set(true));
        let started = std::time::Instant::now();
        let result = work(self.inner.as_mut());
        let us = started.elapsed().as_micros().min(u128::from(u32::MAX - 1)) as u32;
        TIMING.with(|t| t.set(false));
        let (fill, ahead) = FILL.with(|f| f.get());
        // Microseconds since the player last took them: far from wrapping.
        self.stats.sum_us.fetch_add(us, Ordering::Relaxed);
        self.stats.fill_us.fetch_add(fill.min(us), Ordering::Relaxed);
        self.stats.max_us.fetch_max(us, Ordering::Relaxed);
        self.stats.ahead_min_kib.fetch_min(ahead, Ordering::Relaxed);
        result
    }
}

impl Read for TimedSource {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> { crate::audio::callback::require()?; self.timed(|inner| inner.read(buf)) }
}
impl Seek for TimedSource {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> { crate::audio::callback::require()?; self.timed(|inner| inner.seek(pos)) }
}
impl symphonia::core::io::MediaSource for TimedSource {
    fn is_seekable(&self) -> bool { crate::audio::callback::assert_allowed(); self.inner.is_seekable() }
    fn byte_len(&self) -> Option<u64> { crate::audio::callback::assert_allowed(); self.inner.byte_len() }
}

/// What one track accumulated, for its summary line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TrackSummary {
    pub t_us: u64,
    pub track: u32,
    pub origin: u32,
    pub reason: u32,
    pub dur_ms: u32,
    pub packets: u32,
    pub frames: u64,
    pub rate: u32,
    pub read_max_us: u32,
    pub read_sum_ms: u32,
    pub fill_max_us: u32,
    pub decode_max_us: u32,
    pub decode_sum_ms: u32,
    pub write_max_us: u32,
    pub gap_max_us: u32,
    pub buf_min_ms: u32,
    pub recoveries: u32,
    pub slow_reads: u32,
    /// Rate-limited events withheld during this track.
    pub events_limited: u32,
    pub events_lost: u32,
    pub windows_lost: u32,
}

impl TrackSummary {
    /// One line per track at its end. `t` is boottime, as `metrics::uptime`.
    pub(crate) fn line(&self) -> String {
        let n = |v: u32| if v == NONE { "-1".to_string() } else { v.to_string() };
        format!("PLAYTRACE_TRACK v=1 t={}.{:03} track={} origin={} reason={} dur_ms={} packets={} frames={} rate={} \
read_max_us={} read_sum_ms={} fill_max_us={} decode_max_us={} decode_sum_ms={} write_max_us={} gap_max_us={} buf_min_ms={} recoveries={} \
slow_reads={} ev_limited={} ev_lost={} win_lost={}",
            self.t_us / 1_000_000, self.t_us / 1000 % 1000, self.track, self.origin, self.reason, self.dur_ms, self.packets,
            self.frames, self.rate, self.read_max_us, self.read_sum_ms, self.fill_max_us, self.decode_max_us,
            self.decode_sum_ms, self.write_max_us, self.gap_max_us, n(self.buf_min_ms), self.recoveries, self.slow_reads, self.events_limited, self.events_lost,
            self.windows_lost)
    }
}

struct Track {
    serial: u32,
    origin: u32,
    started: u64,
    packets: u32,
    frames: u64,
    rate: u32,
    read_max: u32,
    read_sum: u64,
    fill_max: u32,
    decode_max: u32,
    decode_sum: u64,
    write_max: u32,
    gap_max: u32,
    buf_min: u32,
    recoveries: u32,
    slow_reads: u32,
    limited: u32,
}

/// Rate-limited event kinds, as indexes into `Tap::emitted`.
const LIMIT_READ: usize = 0;
const LIMIT_DECODE: usize = 1;
const LIMIT_WRITE: usize = 2;
const LIMIT_LOW: usize = 3;
const LIMIT_GAP: usize = 4;

/// The player thread's side: numbers handled requests and tracks, turns
/// per-packet measurements into windows, threshold events and the summary.
pub(crate) struct Tap {
    pub(crate) trace: Arc<Trace>,
    track: Option<Track>,
    tracks: u32,
    handled: u32,
    play_handled: u32,
    /// The play-control command being applied right now: (code, serial).
    command: Option<(u32, u32)>,
    /// When each rate-limited kind last produced an event.
    emitted: [u64; 5],
    awaiting_pcm: bool,
    awaiting_submit: bool,
    last_state: u32,
    decoded_at: u64,
    /// When the latest output write returned; 0 after a request, pause,
    /// resume or output release, which end a run of writes.
    wrote_at: u64,
    low: bool,
    pub(crate) reads: Arc<ReadStats>,
}

/// Existing owner operations, never extra polling or a new executor.
#[derive(Clone, Copy)]
pub(crate) enum VolumePhase { Plan = 1, Pause = 2, Codec = 3, Retarget = 4, Resume = 5, Rebase = 6 }

fn trace_frames(frames: Option<u64>) -> u32 {
    frames.map_or(NONE, |n| n.min(u64::from(NONE - 1)) as u32)
}

pub(crate) struct VolumeTrace {
    trace: Option<Arc<Trace>>,
    serial: u32,
    stage: VolumePhase,
}
impl VolumeTrace {
    fn emit(&self, kind: Kind, b: u32, c: u32) {
        if let Some(trace) = &self.trace { trace.event(kind, self.serial, b, c); }
    }
    pub(crate) fn stage(&mut self, stage: VolumePhase) {
        self.stage = stage;
        self.emit(Kind::VolumePhase, stage as u32, NONE);
    }
    pub(crate) fn plan(&self, steps: u8, flags: u32, old: f32, new: f32) {
        self.emit(Kind::VolumePlan, u32::from(steps), flags);
        self.emit(Kind::VolumeGain, old.to_bits(), new.to_bits());
        self.completed(None);
    }
    pub(crate) fn completed(&self, frames: Option<u64>) {
        self.emit(Kind::VolumePhase, (self.stage as u32) | 0x8000_0000, trace_frames(frames));
    }
    pub(crate) fn finish(mut self) {
        self.emit(Kind::VolumeEnd, 0, 0);
        self.trace = None;
    }
}
impl Drop for VolumeTrace {
    fn drop(&mut self) { self.emit(Kind::VolumeEnd, self.stage as u32, 1); }
}

/// A recovery the output performed inside one write.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Recovered {
    pub t_us: u64,
    pub code: i32,
}

impl Tap {
    pub(crate) fn new(trace: Arc<Trace>) -> Tap {
        Tap { trace, track: None, tracks: 0, handled: 0, play_handled: 0, command: None, emitted: [0; 5], awaiting_pcm: false, awaiting_submit: false, last_state: NONE,
            decoded_at: 0, wrote_at: 0, low: false, reads: ReadStats::new() }
    }
    pub(crate) fn now(&self) -> u64 { self.trace.now() }
    fn track_serial(&self) -> u32 { self.track.as_ref().map_or(0, |t| t.serial) }

    pub(crate) fn handled(&mut self, code: u32) {
        if !self.trace.enabled() { return; }
        self.handled = self.handled.wrapping_add(1);
        // Volume is observable without inventing a new playback start or
        // hiding its occupancy inside the uninterrupted feeder write gap.
        if code != 10 {
            self.play_handled = self.handled;
            self.awaiting_pcm = true;
            self.awaiting_submit = true;
            self.wrote_at = 0;
        }
        self.command = Some((code, self.handled));
        self.trace.event(Kind::Handled, code, self.handled, 0);
    }

    /// The command passed to `handled` has been applied.
    pub(crate) fn command_done(&mut self) { self.command = None; }

    /// The serial of the command being applied when its code is one of `codes`, else 0.
    fn command_serial(&self, codes: &[u32]) -> u32 {
        self.command.filter(|(code, _)| codes.contains(code)).map_or(0, |(_, serial)| serial)
    }

    /// Memory-only scope; disabled tracing does not clone the Arc or read a clock.
    pub(crate) fn volume(&self, volume: u8, cached_delay: Option<u64>) -> VolumeTrace {
        let trace = self.trace.enabled().then(|| self.trace.clone());
        let scope = VolumeTrace { trace, serial: self.command_serial(&[10]), stage: VolumePhase::Plan };
        scope.emit(Kind::VolumeBegin, u32::from(volume), trace_frames(cached_delay));
        scope
    }

    /// An event of a rate-limited kind: recorded at most once an interval, counted otherwise.
    fn limited(&mut self, slot: usize, now: u64, t_us: u64, kind: Kind, a: u32, b: u32) {
        if self.emitted[slot] != 0 && now.saturating_sub(self.emitted[slot]) < EVENT_INTERVAL_US {
            if let Some(track) = &mut self.track { track.limited += 1; }
            return;
        }
        self.emitted[slot] = now.max(1);
        self.trace.event_at(t_us, kind, a, b, self.track_serial());
    }

    /// A track's source opened and probed; the summary of one still open is
    /// written first.
    pub(crate) fn opened(&mut self, origin: u32, source_ms: u64, probe_ms: u64) -> Option<TrackSummary> {
        if !self.trace.enabled() { return None; }
        let previous = self.end_track(2);
        self.tracks = self.tracks.wrapping_add(1);
        self.reads.take();
        let now = self.trace.now();
        self.track = Some(Track { serial: self.tracks, origin, started: now, packets: 0, frames: 0, rate: 0, read_max: 0,
            read_sum: 0, fill_max: 0, decode_max: 0, decode_sum: 0, write_max: 0, gap_max: 0, buf_min: NONE, recoveries: 0, slow_reads: 0, limited: 0 });
        self.awaiting_pcm = true;
        self.awaiting_submit = true;
        self.trace.event_at(now, Kind::Open, self.tracks, origin, (source_ms.min(65535) as u32) << 16 | probe_ms.min(65535) as u32);
        previous
    }

    pub(crate) fn end_track(&mut self, reason: u32) -> Option<TrackSummary> {
        let track = self.track.take()?;
        if reason == 3 { self.wrote_at = 0; }
        let now = self.trace.now();
        let (events_lost, windows_lost) = self.trace.losses();
        self.trace.event_at(now, Kind::TrackEnd, track.serial, reason, 0);
        Some(TrackSummary { t_us: now, track: track.serial, origin: track.origin, reason,
            dur_ms: (now.saturating_sub(track.started) / 1000).min(u64::from(u32::MAX)) as u32, packets: track.packets,
            frames: track.frames, rate: track.rate, read_max_us: track.read_max, read_sum_ms: (track.read_sum / 1000) as u32,
            fill_max_us: track.fill_max, decode_max_us: track.decode_max, decode_sum_ms: (track.decode_sum / 1000) as u32,
            write_max_us: track.write_max, gap_max_us: track.gap_max, buf_min_ms: track.buf_min, recoveries: track.recoveries,
            slow_reads: track.slow_reads, events_limited: track.limited, events_lost, windows_lost })
    }

    /// After a packet was demuxed (`started`..`read_done`) and decoded
    /// (..`now`); `frames` is what the decoder returned.
    pub(crate) fn decoded(&mut self, read_done: u64, now: u64, frames: usize) {
        if !self.trace.enabled() { return; }
        let read = self.reads.take();
        let decode_us = micros(read_done, now);
        self.decoded_at = now;
        self.trace.decoded(now, read, decode_us);
        let serial = self.track_serial();
        // A network reader (radio, a starving stream) waits for bytes on
        // most reads: only the file part, or a long wait, is an event.
        let waited = read.fill_us.min(read.max_us);
        let slow = read.max_us - waited >= SLOW_READ_US || waited >= SLOW_FILL_US;
        if let Some(track) = &mut self.track {
            track.packets += 1;
            track.read_max = track.read_max.max(read.max_us);
            track.read_sum += u64::from(read.sum_us);
            track.fill_max = track.fill_max.max(read.fill_us);
            track.decode_max = track.decode_max.max(decode_us);
            track.decode_sum += u64::from(decode_us);
            if slow { track.slow_reads += 1; }
        }
        if slow { self.limited(LIMIT_READ, now, read_done, Kind::SlowRead, read.max_us, waited); }
        if decode_us >= SLOW_DECODE_US { self.limited(LIMIT_DECODE, now, now, Kind::SlowDecode, decode_us, 0); }
        if frames > 0 && std::mem::take(&mut self.awaiting_pcm) {
            self.trace.event_at(now, Kind::FirstPcm, serial, self.play_handled, 0);
        }
    }

    /// After an output write that began at `started`: `frames` accepted,
    /// `buffered` frames were queued before it, `recovered` inside it.
    pub(crate) fn written(&mut self, started: u64, frames: u64, rate: u32, buffered: Option<u64>, recovered: &[Recovered], dropped: u32) {
        if !self.trace.enabled() { return; }
        let now = self.trace.now();
        let write_us = micros(started, now);
        let render_us = if self.decoded_at == 0 { 0 } else { micros(self.decoded_at, started) };
        self.decoded_at = 0;
        let gap_us = if self.wrote_at == 0 { 0 } else { micros(self.wrote_at, started) };
        self.wrote_at = now;
        let buffered_ms = buffered.map_or(NONE, |f| (f.saturating_mul(1000) / u64::from(rate.max(1))).min(u64::from(u32::MAX - 1)) as u32);
        self.trace.written(now, frames.min(u64::from(u32::MAX)) as u32, rate, render_us, write_us, buffered_ms, gap_us);
        let serial = self.track_serial();
        if gap_us >= WRITE_GAP_US { self.limited(LIMIT_GAP, now, started, Kind::WriteGap, gap_us, buffered_ms); }
        if let Some(track) = &mut self.track {
            track.frames += frames;
            track.rate = rate;
            track.write_max = track.write_max.max(write_us);
            track.gap_max = track.gap_max.max(gap_us);
            track.buf_min = track.buf_min.min(buffered_ms);
            track.recoveries += recovered.len() as u32 + dropped;
        }
        for r in recovered {
            self.trace.event_at(r.t_us, Kind::Recovery, r.code.unsigned_abs(), buffered_ms, serial);
        }
        // More recoveries in one write than the output keeps: still counted.
        for _ in 0..dropped.min(4) { self.trace.event_at(now, Kind::Recovery, 0, buffered_ms, serial); }
        if write_us >= SLOW_WRITE_US { self.limited(LIMIT_WRITE, now, now, Kind::SlowWrite, write_us, buffered_ms); }
        if frames > 0 && std::mem::take(&mut self.awaiting_submit) {
            self.trace.event_at(now, Kind::FirstSubmit, serial, self.play_handled, if buffered_ms == NONE { 0 } else { buffered_ms });
            // The first write after a start finds an empty queue by design.
            self.low = true;
        } else if buffered_ms != NONE {
            let low = buffered_ms < LOW_BUFFER_MS;
            if low && !self.low { self.limited(LIMIT_LOW, now, started, Kind::LowBuffer, buffered_ms, 0); }
            self.low = low;
        }
    }

    pub(crate) fn paused(&mut self, retained: bool, frames: u64) {
        self.wrote_at = 0;
        // 2 play-pause, 4 pause.
        self.trace.event(Kind::Paused, self.command_serial(&[2, 4]), retained as u32, frames.min(u64::from(u32::MAX)) as u32);
    }
    pub(crate) fn resumed(&mut self) {
        self.wrote_at = 0;
        // 1 play, 2 play-pause, 3 resume.
        self.trace.event(Kind::Resumed, self.command_serial(&[1, 2, 3]), 0, 0);
    }

    /// A status snapshot was stored and the UI notified.
    pub(crate) fn published(&mut self, state: u32) {
        if state == self.last_state || !self.trace.enabled() { return; }
        self.last_state = state;
        self.trace.event(Kind::Status, self.play_handled, state, self.track_serial());
    }
}

/// Dumps on request: a FIFO `playtrace.ctl` in the runtime directory; any
/// write to it produces `playtrace.dump` there (replaced atomically). The
/// FIFO waits run on the I/O reactor; snapshot/publication use a finite worker.
pub(crate) fn serve(_blocking: &crate::rt::Blocking, runtime: &Path, trace: Arc<Trace>) {
    let control = runtime.join("playtrace.ctl");
    // Left by an earlier process (or made as a plain file by a writer while
    // the trace was off): never mistaken for this process's. Removed whether
    // or not the trace is on, so a FIFO present means a reader is serving.
    let _ = std::fs::remove_file(&control);
    let _ = std::fs::remove_file(runtime.join("playtrace.dump"));
    let _ = std::fs::remove_file(runtime.join("playtrace.dump.tmp"));
    if !trace.enabled() && !crate::metrics::enabled(runtime) { return; }
    let ready = match std::fs::symlink_metadata(&control) {
        Ok(meta) => meta.file_type().is_fifo(),
        Err(_) => {
            crate::platform::system::create_fifo(&control,0o600).is_ok()
        }
    };
    if !ready { log::warn!("playtrace: no control FIFO"); return; }
    let runtime = runtime.to_path_buf();
    crate::rt::io().spawn(serve_fifo(runtime,control,trace, #[cfg(test)] None));
}

fn numeric_nonce(command: &[u8]) -> Option<u64> {
    let value = std::str::from_utf8(command).ok()?.strip_prefix("numeric ")?.strip_suffix('\n')?;
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) { return None; }
    value.parse::<u64>().ok().filter(|nonce| *nonce != 0)
}

async fn serve_fifo(runtime:PathBuf,control:PathBuf,trace:Arc<Trace>,
    #[cfg(test)] reader_ready: Option<Arc<tokio::sync::Notify>>) {
        loop {
            let path=control.clone();
            let opened=crate::rt::spawn_blocking(move |_| {
                let file=std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW|libc::O_NONBLOCK).open(path)?;
                if !file.metadata()?.file_type().is_fifo() {return Err(std::io::Error::other("not a control FIFO"));}
                Ok(file)
            }).await;
            let Ok(Ok(file))=opened else {return};
            let Ok(file)=crate::rt::Fd::new(file.into()) else {return};
            // A fixture may cancel this reactor-owned phase independently of
            // the preceding uncancellable finite open/publication jobs.
            #[cfg(test)]
            if let Some(ready) = &reader_ready { ready.notify_one(); }
            let mut request = [0u8; 64];
            let mut command = Vec::with_capacity(64);
            let mut oversized = false;
            loop {
                match file.read(&mut request).await {
                    Ok(0)=>break,
                    Ok(count)=>{
                        if command.len() + count <= 64 { command.extend_from_slice(&request[..count]); }
                        else { oversized = true; }
                    },
                    Err(error) if error.kind()==std::io::ErrorKind::Interrupted=>{},
                    Err(_)=>return,
                }
            }
            // Closing and reopening removes persistent POLLHUP after the last
            // writer closes. A newly opened reader waits for the next writer;
            // it does not poll empty EOF or occupy a blocking-pool thread.
            drop(file);
            if let Some(nonce) = numeric_nonce(&command).filter(|_| !oversized) {
                crate::metrics::request_boundary(&runtime, nonce);
                continue;
            }
            if !trace.enabled() { continue; }
            let (runtime,trace)=(runtime.clone(),trace.clone());
            match crate::rt::spawn_blocking(move |blocking|dump(blocking, &runtime,&trace)).await {
                Ok(Err(error))=>log::warn!("playtrace: dump failed ({:?})",error.kind()),
                Err(_)=>return,
                Ok(Ok(_))=>{},
            }
        }
}

pub(crate) fn dump(_blocking: &crate::rt::Blocking, runtime: &Path, trace: &Trace) -> std::io::Result<PathBuf> {
    let text = trace.snapshot().text();
    let partial = runtime.join("playtrace.dump.tmp");
    let _ = std::fs::remove_file(&partial);
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&partial)?;
    file.write_all(text.as_bytes())?;
    drop(file);
    let path = runtime.join("playtrace.dump");
    std::fs::rename(&partial, &path)?;
    Ok(path)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn stepped(start: u64, step: u64) -> Arc<dyn Fn() -> u64 + Send + Sync> {
        let at = Mutex::new(start);
        Arc::new(move || { let mut at = at.lock().unwrap(); *at += step; *at })
    }

    fn workspace(tag: &str) -> PathBuf {
        let base=std::env::var_os("Q2_GENERATED_TEST_ROOT").map(PathBuf::from)
            .unwrap_or_else(||Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build/perf/tests"));
        let root = base.join(format!("playtrace-{tag}-{}-{}", std::process::id(), fastrand::u64(..)));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn event_ring_overflow_keeps_the_newest_in_order_and_counts_what_it_lost() {
        let trace = Trace::with(true, 8, 4, "/nonexistent".into(), Some(stepped(1000, 10)));
        for i in 0..8 { trace.event(Kind::SlowRead, i, 0, 0); }
        let full = trace.snapshot();
        assert_eq!((full.events_total, full.events_lost, full.events.len()), (8, 0, 8));
        for i in 8..21 { trace.event(Kind::SlowRead, i, 0, 0); }
        let s = trace.snapshot();
        assert_eq!((s.events_total, s.events_lost, s.events_capacity), (21, 13, 8));
        assert_eq!(s.events.iter().map(|e| e.seq).collect::<Vec<_>>(), (13..21).collect::<Vec<_>>());
        assert_eq!(s.events.iter().map(|e| e.a).collect::<Vec<_>>(), (13..21).collect::<Vec<_>>());
        assert!(s.events.windows(2).all(|p| p[0].t_us < p[1].t_us));
        assert_eq!(trace.losses(), (13, 0));
        let text = s.text();
        assert!(text.contains("events capacity=8 total=21 lost=13\n"));
        assert!(text.lines().next().unwrap().starts_with("playtrace 1 pid="));
        assert_eq!(text.lines().last().unwrap(), "end 8 0");
        assert_eq!(text.lines().filter(|l| l.starts_with("E ")).count(), 8);
    }

    #[test]
    fn windows_close_each_second_overflow_is_counted_and_idle_time_records_nothing() {
        let clock = Arc::new(Mutex::new(5_000_000u64));
        let at = clock.clone();
        let trace = Trace::with(true, 8, 3, "/nonexistent".into(), Some(Arc::new(move || *at.lock().unwrap())));
        let read = ReadSample { sum_us: 300, max_us: 200, fill_us: 50, ahead_min_kib: 700 };
        for second in 0..6u64 {
            for packet in 0..4u64 {
                let now = 5_000_000 + second * 1_000_000 + packet * 250_000;
                *clock.lock().unwrap() = now;
                trace.decoded(now, read, 1500 + second as u32);
                trace.written(now, 1024, 44100, 40, 9000, 380 - packet as u32, 7000 + packet as u32);
            }
        }
        let s = trace.snapshot();
        // Six seconds of activity: five closed windows (three kept), one open.
        assert_eq!((s.windows_total, s.windows_lost, s.windows.len()), (5, 2, 3));
        assert_eq!(s.windows.iter().map(|w| w.seq).collect::<Vec<_>>(), [2, 3, 4]);
        let w = s.windows[0];
        assert_eq!((w.packets, w.frames, w.rate, w.dur_us), (4, 4096, 44100, 750_000));
        assert_eq!((w.read_sum, w.read_max, w.fill_sum, w.fill_max), (1200, 200, 200, 50));
        assert_eq!((w.decode_sum, w.decode_max, w.write_sum, w.write_max), (4 * 1502, 1502, 36000, 9000));
        assert_eq!((w.buf_min_ms, w.buf_last_ms, w.ahead_min_kib, w.gap_max), (377, 377, 700, 7003));
        assert_eq!((w.dirty_kb, w.writeback_kb), (NONE, NONE), "not sampled without a transfer");
        assert_eq!(s.current.unwrap().packets, 4);
        // A long idle gap adds no windows: the next activity opens a new one.
        *clock.lock().unwrap() = 500_000_000;
        trace.decoded(500_000_000, read, 10);
        let after = trace.snapshot();
        assert_eq!(after.windows_total, 6);
        assert_eq!(after.current.unwrap().t_us, 500_000_000);
        let text = after.text();
        assert!(text.contains("windows capacity=3 total=6 lost=3\n"));
        assert_eq!(text.lines().filter(|l| l.starts_with("W ")).count(), 3);
        assert_eq!(text.lines().filter(|l| l.starts_with("P ")).count(), 1);
        assert_eq!(text.lines().last().unwrap(), "end 0 4");
        let columns = text.lines().find(|l| l.starts_with("# W ")).unwrap().split_whitespace().count() - 2;
        assert!(text.lines().filter(|l| l.starts_with("W ") || l.starts_with("P ")).all(|l| l.split_whitespace().count() - 1 == columns));
    }

    #[test]
    fn writeback_is_sampled_by_transfer_threads_only_and_at_most_once_a_second() {
        let root = workspace("meminfo");
        let meminfo = root.join("meminfo");
        std::fs::write(&meminfo, "MemTotal:  118000 kB\nActive(file):  9 kB\nDirty:              4321 kB\nWriteback:           77 kB\nWritebackTmp: 5 kB\n").unwrap();
        let clock = Arc::new(Mutex::new(1_000_000u64));
        let at = clock.clone();
        let trace = Trace::with(true, 16, 16, meminfo.clone(), Some(Arc::new(move || *at.lock().unwrap())));
        let step = |us: u64| { *clock.lock().unwrap() += us; *clock.lock().unwrap() };
        // The player closing windows never reads the file.
        trace.decoded(step(0), ReadSample::default(), 1);
        trace.decoded(step(1_000_000), ReadSample::default(), 1);
        assert_eq!(trace.snapshot().windows[0].dirty_kb, NONE);
        assert!(trace.sampler.lock().unwrap().file.is_none());
        let id = trace.transfer_start(true);
        let started = step(0);
        step(60_000);
        trace.transfer_wrote(&crate::rt::Blocking::dedicated_thread(), started, 65536, 65536);
        assert!(trace.sampler.lock().unwrap().file.is_some());
        // A second write 100 ms later does not sample again.
        std::fs::write(&meminfo, "Dirty: 9999 kB\nWriteback: 1 kB\n").unwrap();
        let again = step(100_000);
        trace.transfer_wrote(&crate::rt::Blocking::dedicated_thread(), again, 10, 65546);
        trace.decoded(step(1_000_000), ReadSample::default(), 1);
        let s = trace.snapshot();
        let w = s.windows[1];
        assert_eq!((w.dirty_kb, w.writeback_kb, w.xfer_bytes, w.xfer_active, w.card_write_max), (4321, 77, 65546, 1, 60_000));
        assert!(s.events.iter().any(|e| e.kind == Kind::SlowCardWrite && e.a == 60_000 && e.b == 64));
        // A second later the transfer thread samples again, into the open window.
        let later = step(0);
        trace.transfer_wrote(&crate::rt::Blocking::dedicated_thread(), later, 1, 65547);
        trace.transfer_end(id, 1, 65547);
        trace.decoded(step(1_000_000), ReadSample::default(), 1);
        assert_eq!(trace.snapshot().windows[2].dirty_kb, 9999);
        // With no transfer thread running nothing is sampled.
        trace.decoded(step(1_000_000), ReadSample::default(), 1);
        assert_eq!(trace.snapshot().windows[3].dirty_kb, NONE);
        assert_eq!(parse_meminfo(b"NotDirty: 9 kB\nWriteback: 1 kB\n"), None);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn disabled_trace_records_nothing_and_reads_no_clock() {
        let calls = Arc::new(AtomicU32::new(0));
        let counted = calls.clone();
        let trace = Arc::new(Trace::with(false, 8, 8, "/nonexistent".into(), Some(Arc::new(move || u64::from(counted.fetch_add(1, Ordering::Relaxed))))));
        let mut tap = Tap::new(trace.clone());
        assert_eq!(trace.request(1), 0);
        tap.handled(1);
        assert!(tap.opened(1, 3, 4).is_none());
        let t = tap.now();
        tap.decoded(t, t, 100);
        tap.written(t, 100, 8000, Some(0), &[Recovered { t_us: 1, code: -32 }], 0);
        tap.published(2);
        let mut volume = tap.volume(50, None);
        volume.plan(30, 1, 1.0, 0.5);
        volume.stage(VolumePhase::Codec);
        volume.completed(None);
        volume.finish();
        drop(tap.volume(0, Some(0)));
        assert!(tap.end_track(1).is_none());
        trace.transfer_wrote(&crate::rt::Blocking::dedicated_thread(), 0, 10, 10);
        assert_eq!(trace.transfer_start(true), 0);
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        let s = trace.snapshot();
        assert!(s.events.is_empty() && s.windows.is_empty() && s.current.is_none());
    }

    #[test]
    fn track_summary_line_is_fixed_numeric_and_carries_the_ring_losses() {
        let trace = Arc::new(Trace::with(true, 4, 4, "/nonexistent".into(), Some(stepped(90_000_000, 1000))));
        let mut tap = Tap::new(trace.clone());
        tap.handled(5);
        assert!(tap.opened(2, 70, 12).is_none());
        tap.reads.sum_us.store(45_000, Ordering::Relaxed);
        tap.reads.max_us.store(41_000, Ordering::Relaxed);
        tap.reads.fill_us.store(3_000, Ordering::Relaxed);
        let read_done = tap.now();
        let now = tap.now();
        tap.decoded(read_done, now, 1152);
        let started = tap.now();
        tap.written(started, 1152, 44100, Some(2205), &[Recovered { t_us: started + 1, code: -32 }], 0);
        let summary = tap.end_track(1).unwrap();
        let line = summary.line();
        let fields: Vec<&str> = line.split(' ').collect();
        assert_eq!(fields[0], "PLAYTRACE_TRACK");
        let keys: Vec<&str> = fields[1..].iter().map(|f| f.split_once('=').unwrap().0).collect();
        assert_eq!(keys, ["v", "t", "track", "origin", "reason", "dur_ms", "packets", "frames", "rate", "read_max_us",
            "read_sum_ms", "fill_max_us", "decode_max_us", "decode_sum_ms", "write_max_us", "gap_max_us", "buf_min_ms", "recoveries",
            "slow_reads", "ev_limited", "ev_lost", "win_lost"]);
        // Every value is a number; `t` is seconds.milliseconds of boottime.
        assert!(fields[1..].iter().all(|f| f.split_once('=').unwrap().1.chars().all(|c| c.is_ascii_digit() || c == '.' || c == '-')));
        assert!(line.contains(" track=1 origin=2 reason=1 ") && line.contains(" packets=1 frames=1152 rate=44100 "));
        assert!(line.contains(" read_max_us=41000 read_sum_ms=45 fill_max_us=3000 ") && line.contains(" buf_min_ms=50 recoveries=1 slow_reads=1 "));
        let t = line.split(" t=").nth(1).unwrap().split(' ').next().unwrap();
        assert_eq!(t, format!("{}.{:03}", summary.t_us / 1_000_000, summary.t_us / 1000 % 1000));
        // handled, open, slow-read, first-pcm, recovery, first-submit, track-end into four slots.
        assert_eq!((summary.events_lost, summary.windows_lost), (2, 0));
        assert!(line.ends_with(" ev_limited=0 ev_lost=2 win_lost=0"));
        let kinds: Vec<Kind> = trace.snapshot().events.iter().map(|e| e.kind).collect();
        assert_eq!(kinds, [Kind::FirstPcm, Kind::Recovery, Kind::FirstSubmit, Kind::TrackEnd]);
        let absent = TrackSummary { buf_min_ms: NONE, ..summary }.line();
        assert!(absent.contains(" buf_min_ms=-1 "));
    }

    #[test]
    fn request_and_completion_endpoints_share_a_serial_and_stall_events_are_edge_triggered_and_rate_limited() {
        let clock = Arc::new(Mutex::new(1_000_000u64));
        let at = clock.clone();
        let trace = Arc::new(Trace::with(true, 64, 8, "/nonexistent".into(), Some(Arc::new(move || *at.lock().unwrap()))));
        let adv = |us: u64| { *clock.lock().unwrap() += us; *clock.lock().unwrap() };
        let mut tap = Tap::new(trace.clone());
        assert_eq!(trace.request(5), 1);
        tap.handled(5);
        tap.opened(3, 1, 2);
        tap.command_done();
        let t = adv(1000);
        tap.decoded(t, t, 0); // no frames yet: not the first PCM
        let t = adv(1000);
        tap.decoded(t, t, 512);
        tap.written(adv(1000), 512, 8000, Some(0), &[], 0);
        tap.published(1);
        tap.published(1);
        // 200 ms; 50 ms (a crossing: event); 37 ms; 200 ms; 12 ms (a second
        // crossing inside the same second: counted, no event); 200 ms.
        for buffered in [1600u64, 400, 300, 1600, 100, 1600] { tap.written(adv(10_000), 512, 8000, Some(buffered), &[], 0); }
        // 1.5 s without a write: a write gap, and a crossing that is an event again.
        tap.written(adv(1_500_000), 512, 8000, Some(160), &[], 0);
        // A 400 ms hole 400 ms later: a second gap inside the interval, counted only.
        tap.written(adv(400_000), 512, 8000, Some(0), &[], 0);
        // A pause with no pause command being applied (sleep, an output fault).
        tap.paused(true, 7);
        assert_eq!(trace.request(4), 2);
        tap.handled(4);
        tap.paused(true, 900);
        tap.command_done();
        assert_eq!(trace.request(3), 3);
        tap.handled(3);
        tap.resumed();
        tap.command_done();
        // The same hole across a pause and resume is not a write gap.
        tap.written(adv(2_000_000), 512, 8000, Some(900), &[], 0);
        tap.resumed(); // after an output route change
        let events = trace.snapshot().events;
        let of = |kind: Kind| events.iter().filter(|e| e.kind == kind).copied().collect::<Vec<_>>();
        assert_eq!(of(Kind::Request).iter().map(|e| (e.a, e.b)).collect::<Vec<_>>(), [(5, 1), (4, 2), (3, 3)]);
        assert_eq!(of(Kind::Handled).iter().map(|e| (e.a, e.b)).collect::<Vec<_>>(), [(5, 1), (4, 2), (3, 3)]);
        assert_eq!(of(Kind::FirstPcm).iter().map(|e| (e.a, e.b)).collect::<Vec<_>>(), [(1, 1)]);
        // The first write after the resume request is that request's submission.
        assert_eq!(of(Kind::FirstSubmit).iter().map(|e| (e.a, e.b, e.c)).collect::<Vec<_>>(), [(1, 1, 0), (1, 3, 112)]);
        assert_eq!(of(Kind::Status).len(), 1);
        assert_eq!(of(Kind::LowBuffer).iter().map(|e| e.a).collect::<Vec<_>>(), [50, 20]);
        assert_eq!(of(Kind::WriteGap).iter().map(|e| (e.a, e.b, e.c)).collect::<Vec<_>>(), [(1_500_000, 20, 1)]);
        // Pause and resume carry a serial only for the command that caused them.
        assert_eq!(of(Kind::Paused).iter().map(|e| (e.a, e.b, e.c)).collect::<Vec<_>>(), [(0, 1, 7), (2, 1, 900)]);
        assert_eq!(of(Kind::Resumed).iter().map(|e| e.a).collect::<Vec<_>>(), [3, 0]);
        let order = |kind: Kind| events.iter().position(|e| e.kind == kind).unwrap();
        assert!(order(Kind::Request) < order(Kind::Handled) && order(Kind::Handled) < order(Kind::FirstPcm)
            && order(Kind::FirstPcm) < order(Kind::FirstSubmit) && order(Kind::FirstSubmit) < order(Kind::Status));
        assert!(Kind::ALL.iter().enumerate().all(|(i, k)| *k as usize == i + 1 && k.name().chars().all(|c| c.is_ascii_lowercase() || c == '-')));
        // The two withheld events (a low-buffer crossing, a write gap) are counted.
        let summary = tap.end_track(2).unwrap();
        assert_eq!((summary.events_limited, summary.gap_max_us), (2, 1_500_000));
        assert!(summary.line().contains(" ev_limited=2 "));
    }

    #[test]
    fn volume_trace_preserves_playback_serial_and_uninterrupted_write_gap() {
        let clock = Arc::new(Mutex::new(1_000_000u64));
        let at = clock.clone();
        let trace = Arc::new(Trace::with(true, 64, 8, "/nonexistent".into(), Some(Arc::new(move || *at.lock().unwrap()))));
        let mut tap = Tap::new(trace.clone());
        assert_eq!(trace.request(5), 1); tap.handled(5); tap.opened(3, 1, 2); tap.command_done();
        assert_eq!(trace.request(10), 2); tap.handled(10);
        tap.volume(21, None).finish(); tap.command_done();
        tap.decoded(1_000_000, 1_000_000, 512);
        tap.written(1_000_000, 512, 8000, Some(0), &[], 0);
        assert_eq!(trace.request(10), 3); tap.handled(10);
        tap.volume(22, Some(0)).finish(); tap.command_done();
        *clock.lock().unwrap() += 400_000;
        tap.decoded(1_400_000, 1_400_000, 512);
        tap.written(1_400_000, 512, 8000, None, &[], 0);
        tap.volume(22, Some(u64::MAX)).finish(); // automatic, not the last command
        let events = trace.snapshot().events;
        for kind in [Kind::FirstPcm, Kind::FirstSubmit] {
            let first:Vec<_> = events.iter().filter(|e|e.kind==kind).collect();
            assert_eq!(first.len(),1); assert_eq!(first[0].b,1);
        }
        assert!(events.iter().any(|e|e.kind==Kind::WriteGap && e.a==400_000));
        assert_eq!(events.iter().filter(|e|e.kind==Kind::VolumeBegin).map(|e|(e.a,e.b,e.c)).collect::<Vec<_>>(),
            [(2,21,NONE),(3,22,0),(0,22,NONE-1)]);
    }

    #[test]
    fn volume_trace_failure_phase_and_overflow_are_explicit() {
        let trace=Arc::new(Trace::with(true,4,4,"/nonexistent".into(),Some(stepped(1_000_000,10))));
        let mut tap=Tap::new(trace.clone());
        tap.handled(10);
        {
            let mut volume=tap.volume(92,None);
            volume.plan(0,11,0.5,0.7);
            volume.stage(VolumePhase::Pause); volume.completed(Some(4));
            volume.stage(VolumePhase::Codec);
            // Actual early-return shape: no success endpoint and no later phase.
        }
        let snap=trace.snapshot();
        assert_eq!((snap.events_total,snap.events_lost,snap.events.len()),(9,5,4));
        assert_eq!(snap.events.iter().map(|e|e.seq).collect::<Vec<_>>(),[5,6,7,8]);
        assert_eq!(snap.events.iter().map(|e|(e.kind,e.a,e.b,e.c)).collect::<Vec<_>>(),[
            (Kind::VolumePhase,1,2,NONE),(Kind::VolumePhase,1,0x8000_0002,4),
            (Kind::VolumePhase,1,3,NONE),(Kind::VolumeEnd,1,3,1)]);
        assert!(snap.events.windows(2).all(|p|p[0].t_us<p[1].t_us));
    }

    #[test]
    fn network_reads_cannot_flood_the_event_ring() {
        let clock = Arc::new(Mutex::new(1_000_000u64));
        let at = clock.clone();
        let trace = Arc::new(Trace::with(true, 64, 64, "/nonexistent".into(), Some(Arc::new(move || *at.lock().unwrap()))));
        let adv = |us: u64| { *clock.lock().unwrap() += us; *clock.lock().unwrap() };
        let mut tap = Tap::new(trace.clone());
        tap.opened(4, 0, 0);
        let read = |tap: &mut Tap, max: u32, fill: u32, t: u64| {
            tap.reads.sum_us.store(max, Ordering::Relaxed);
            tap.reads.max_us.store(max, Ordering::Relaxed);
            tap.reads.fill_us.store(fill, Ordering::Relaxed);
            tap.decoded(t, t, 100);
        };
        // A radio waiting 40 ms for bytes on every read for ten seconds: no event.
        for _ in 0..250 { let t = adv(40_000); read(&mut tap, 40_000, 39_500, t); }
        assert!(trace.snapshot().events.iter().all(|e| e.kind != Kind::SlowRead));
        // A stream starving 150 ms on every read for ten seconds: one event a second.
        for _ in 0..66 { let t = adv(150_000); read(&mut tap, 150_000, 150_000, t); }
        let starving = trace.snapshot().events.iter().filter(|e| e.kind == Kind::SlowRead).count();
        assert!((9..=11).contains(&starving), "{starving}");
        // File reads of 25 ms with no wait: events, at the same limit.
        adv(2_000_000);
        for _ in 0..40 { let t = adv(25_000); read(&mut tap, 25_000, 0, t); }
        // The wait reported never exceeds the read (waits are summed over a
        // packet's reads; the read is its longest one).
        let t = adv(1_000_000);
        read(&mut tap, 150_000, 400_000, t);
        let events = trace.snapshot().events;
        let last = events.iter().rev().find(|e| e.kind == Kind::SlowRead).unwrap();
        assert_eq!((last.a, last.b), (150_000, 150_000));
        assert!(events.iter().filter(|e| e.kind == Kind::SlowRead).all(|e| e.b <= e.a));
        let summary = tap.end_track(1).unwrap();
        assert_eq!(summary.slow_reads, 66 + 40 + 1, "every slow read is counted for the track");
        assert_eq!(summary.events_limited + events.iter().filter(|e| e.kind == Kind::SlowRead).count() as u32, 107);
        assert_eq!(trace.losses(), (0, 0), "the ring kept everything else");
    }

    #[test]
    fn the_trace_is_off_unless_configured_and_the_environment_overrides() {
        use std::ffi::OsStr;
        assert!(!wanted(false, None));
        assert!(wanted(true, None));
        assert!(wanted(false, Some(OsStr::new("1"))));
        assert!(!wanted(true, Some(OsStr::new("0"))));
        assert!(wanted(true, Some(OsStr::new("yes"))) && !wanted(false, Some(OsStr::new(""))));
        let trace = Trace::new(false);
        trace.event(Kind::Recovery, 32, 0, 0);
        trace.set_enabled(true);
        trace.event(Kind::Recovery, 32, 0, 0);
        assert_eq!(trace.snapshot().events_total, 1);
        // The device-operable switch: a config.toml key, absent from a
        // saved configuration until it is set.
        let config = crate::config::Config::default();
        assert!(!config.diagnostics.playtrace);
        assert!(!toml::to_string_pretty(&config).unwrap().contains("diagnostics"));
        let on: crate::config::Config = toml::from_str("[diagnostics]\nplaytrace = true\n").unwrap();
        assert!(on.diagnostics.playtrace);
        assert!(toml::to_string_pretty(&on).unwrap().contains("[diagnostics]\nplaytrace = true"));
    }

    #[test]
    fn timed_source_separates_download_waits_from_file_reads() {
        crate::audio::callback::test_scope(|| {
        struct Slow(std::io::Cursor<Vec<u8>>);
        impl Read for Slow {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let waiting = fill_timer();
                assert!(waiting.is_some(), "inside a timed read");
                std::thread::sleep(std::time::Duration::from_millis(30));
                note_fill_wait(waiting);
                note_ahead(96 * 1024);
                std::thread::sleep(std::time::Duration::from_millis(25));
                self.0.read(buf)
            }
        }
        impl Seek for Slow { fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> { self.0.seek(pos) } }
        impl symphonia::core::io::MediaSource for Slow {
            fn is_seekable(&self) -> bool { true }
            fn byte_len(&self) -> Option<u64> { Some(4) }
        }
        let stats = ReadStats::new();
        let mut source = TimedSource::new(Box::new(Slow(std::io::Cursor::new(vec![1, 2, 3, 4]))), stats.clone());
        let mut byte = [0u8; 2];
        source.read_exact(&mut byte).unwrap();
        source.seek(SeekFrom::Start(0)).unwrap();
        assert_eq!(symphonia::core::io::MediaSource::byte_len(&source), Some(4));
        let sample = stats.take();
        assert!(sample.max_us >= 55_000 && sample.sum_us >= sample.max_us, "{sample:?}");
        assert!(sample.fill_us >= 30_000 && sample.fill_us <= sample.sum_us - 25_000, "{sample:?}");
        assert_eq!(sample.ahead_min_kib, 96);
        assert_eq!(stats.take(), ReadSample { ahead_min_kib: NONE, ..Default::default() });
        // Outside a timed read (the trace off: no TimedSource) a reader takes no clock reading.
        assert!(fill_timer().is_none());
        note_fill_wait(None);
        note_ahead(5);
        source.seek(SeekFrom::Start(0)).unwrap();
        assert_eq!(stats.take().ahead_min_kib, NONE);
            })
    }

    #[test]
    fn control_fifo_dumps_atomically_and_stale_control_and_dump_files_are_removed_at_start() {
        let root = workspace("fifo");
        // Left by an earlier process: a dump, and a plain file where the FIFO goes.
        std::fs::write(root.join("playtrace.dump"), b"playtrace 1 pid=1 now_us=1 stale\n").unwrap();
        std::fs::write(root.join("playtrace.ctl"), b"a regular file").unwrap();
        let trace = Arc::new(Trace::with(true, 8, 8, "/nonexistent".into(), None));
        trace.event(Kind::Recovery, 32, 7, 1);
        serve(&crate::rt::Blocking::dedicated_thread(), &root, trace.clone());
        let control = root.join("playtrace.ctl");
        assert!(std::fs::symlink_metadata(&control).unwrap().file_type().is_fifo());
        assert!(!root.join("playtrace.dump").exists(), "no dump until one is asked for");
        let mut last_now = 0u64;
        for round in 0..2 {
            trace.event(Kind::SlowRead, 25_000 + round, 0, 1);
            std::fs::OpenOptions::new().write(true).open(&control).unwrap().write_all(b"dump\n").unwrap();
            let dumped = root.join("playtrace.dump");
            let wanted = format!(" slow-read {} 0 1\n", 25_000 + round);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !std::fs::read_to_string(&dumped).is_ok_and(|t| t.contains(&wanted)) {
                assert!(std::time::Instant::now() < deadline, "no dump");
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            let text = std::fs::read_to_string(&dumped).unwrap();
            assert!(text.contains(" recovery 32 7 1\n") && text.lines().last().unwrap().starts_with("end "));
            assert_eq!(std::os::unix::fs::PermissionsExt::mode(&std::fs::metadata(&dumped).unwrap().permissions()) & 0o777, 0o600);
            // A reader tells a new dump from the previous one by its now_us.
            let now: u64 = text.split("now_us=").nth(1).unwrap().split(' ').next().unwrap().parse().unwrap();
            assert!(now > last_now);
            last_now = now;
        }
        // Off: stale files are still removed, and no FIFO is made, so a
        // writer that checks for a FIFO does not block on a dead one.
        let off = workspace("fifo-off");
        std::fs::write(off.join("playtrace.dump"), b"stale").unwrap();
        crate::platform::system::create_fifo(&off.join("playtrace.ctl"),0o600).unwrap();
        serve(&crate::rt::Blocking::dedicated_thread(), &off, Arc::new(Trace::new(false)));
        assert!(!off.join("playtrace.ctl").exists() && !off.join("playtrace.dump").exists());
        let _ = std::fs::remove_dir_all(off);
        // The first workspace keeps an async reader on its FIFO; leave the directory.
    }

    #[test]
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn async_fifo_waits_for_writer_close_rearms_without_idle_dumps_and_aborts_cleanly() {
        // ENXIO concerns every process holding this FIFO, including unrelated
        // concurrent fixtures between fork and exec despite FD_CLOEXEC. Run
        // this control alone after exec, so its new FIFO cannot be inherited
        // by another libtest case. Retain a deliberate inherited reader below.
        crate::rt::test_support::isolated_pool_test(
            "playtrace::tests::async_fifo_waits_for_writer_close_rearms_without_idle_dumps_and_aborts_cleanly",
            "Q2_PLAYTRACE_FIFO_ISOLATED",
            || crate::rt::Ui::new().unwrap().block_on(async {
        let root=workspace("async-fifo");
        let control=root.join("playtrace.ctl");
        crate::platform::system::create_fifo(&control,0o600).unwrap();
        let trace=Arc::new(Trace::with(true,8,8,"/nonexistent".into(),None));
        let reader_ready=Arc::new(tokio::sync::Notify::new());
        let task=crate::rt::io().spawn(serve_fifo(root.clone(),control.clone(),trace.clone(),Some(reader_ready.clone())));
        let open=||std::fs::OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(&control);
        for round in 0..3 {
            tokio::time::timeout(std::time::Duration::from_secs(3),reader_ready.notified()).await.unwrap();
            let mut writer=tokio::time::timeout(std::time::Duration::from_secs(3),async {
                loop {if let Ok(writer)=open() {break writer;}tokio::task::yield_now().await;}
            }).await.unwrap();
            trace.event(Kind::SlowRead,round,0,0);
            let dump=root.join("playtrace.dump");
            let previous=std::fs::read(&dump).ok();
            writer.write_all(b"dump\n").unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            assert_eq!(std::fs::read(&dump).ok(),previous,"request remains open");
            drop(writer);
            let expected=format!(" slow-read {round} 0 0\n");
            tokio::time::timeout(std::time::Duration::from_secs(3),async {
                while !std::fs::read_to_string(&dump).is_ok_and(|s|s.contains(&expected)) {tokio::task::yield_now().await;}
            }).await.unwrap();
            let saved=std::fs::read(&dump).unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            assert_eq!(std::fs::read(&dump).unwrap(),saved,"idle EOF must not trigger repeated dumps");
        }
        // Observing the dump does not join its worker or the following open.
        // Wait for the next reactor-owned reader and retain a writer so EOF
        // cannot start another finite job before cancellation. Joining this
        // task must close its reader; the explicit pre-exec child below can
        // retain a separate inherited reference until exec/reap.
        tokio::time::timeout(std::time::Duration::from_secs(3),reader_ready.notified()).await.unwrap();
        let writer=open().unwrap();
        // Reproduce the cross-test hazard: CLOEXEC closes this inherited
        // reader at exec, not fork. The existing bounded barrier runs only
        // async-signal-safe operations in the child before exec.
        let (barrier, child_barrier)=std::os::unix::net::UnixStream::pair().unwrap();
        barrier.set_nonblocking(true).unwrap();
        let mut barrier=tokio::net::UnixStream::from_std(barrier).unwrap();
        let mut command=std::process::Command::new("/bin/sh");
        command.args(["-c","exit 0"]);
        crate::platform::process::generated::exec_barrier(&mut command,child_barrier);
        let spawned=crate::rt::spawn_blocking(move |_| {
            tokio::process::Command::from(command).kill_on_drop(true).spawn()
        });
        let mut ready=[0u8;4];
        let ready_result=tokio::time::timeout(std::time::Duration::from_secs(3),
            tokio::io::AsyncReadExt::read_exact(&mut barrier,&mut ready)).await;
        task.abort();
        let cancelled=task.await;
        let inherited_reader=open();
        // Release and reap before asserting any observations. On failed ready
        // or write, dropping the barrier also releases its bounded child wait.
        let released=tokio::time::timeout(std::time::Duration::from_secs(3),
            tokio::io::AsyncWriteExt::write_all(&mut barrier,&[1])).await;
        drop(barrier);
        let mut child=spawned.await.unwrap().unwrap();
        let child_pid=child.id();
        let exited=tokio::time::timeout(std::time::Duration::from_secs(3),child.wait()).await;
        if !matches!(&exited,Ok(Ok(_))) { child.kill().await.unwrap(); }
        let no_reader=open();
        drop(writer);
        assert!(matches!(ready_result,Ok(Ok(_))),"child did not enter bounded pre-exec barrier");
        assert_eq!(child_pid,Some(u32::try_from(i32::from_ne_bytes(ready)).unwrap()),"barrier must belong to the reaped child");
        assert!(matches!(released,Ok(Ok(()))),"child release failed");
        assert!(cancelled.unwrap_err().is_cancelled());
        assert!(inherited_reader.is_ok(),"pre-exec child must retain inherited reader after task cancellation");
        assert!(matches!(exited,Ok(Ok(status)) if status.success()),"owned child did not exit cleanly");
        assert_eq!(no_reader.unwrap_err().raw_os_error(),Some(libc::ENXIO));
        drop(inherited_reader);
        std::fs::remove_dir_all(root).unwrap();
            }),
        );
    }

    #[tokio::test]
    async fn async_fifo_refuses_regular_and_symlink_controls() {
        for symlink in [false,true] {
            let root=workspace("async-refusal");
            let regular=root.join("regular");std::fs::write(&regular,"generated").unwrap();
            let control=root.join("playtrace.ctl");
            if symlink {std::os::unix::fs::symlink(&regular,&control).unwrap();}
            else {std::fs::write(&control,"generated").unwrap();}
            let trace=Arc::new(Trace::with(true,8,8,"/nonexistent".into(),None));
            tokio::time::timeout(std::time::Duration::from_secs(3),serve_fifo(root.clone(),control,trace,None)).await.unwrap();
            assert!(!root.join("playtrace.dump").exists());
            assert_eq!(std::fs::read_to_string(regular).unwrap(),"generated");
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    /// Host bound on the per-packet cost: the calls the player adds around
    /// one packet (five clock reads, two timed source reads, two ring
    /// updates), against the same loop with the trace disabled.
    #[test]
    fn overhead_per_packet_is_bounded_on_the_host() {
        crate::audio::callback::test_scope(|| {
        struct Fast;
        impl Read for Fast { fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> { Ok(buf.len()) } }
        impl Seek for Fast { fn seek(&mut self, _: SeekFrom) -> std::io::Result<u64> { Ok(0) } }
        impl symphonia::core::io::MediaSource for Fast {
            fn is_seekable(&self) -> bool { true }
            fn byte_len(&self) -> Option<u64> { None }
        }
        let run = |enabled: bool| {
            let trace = Arc::new(Trace::with(enabled, EVENTS, WINDOWS, "/nonexistent".into(), None));
            let mut tap = Tap::new(trace.clone());
            tap.opened(1, 0, 0);
            let mut source = TimedSource::new(Box::new(Fast), tap.reads.clone());
            let mut buf = [0u8; 16];
            const PACKETS: u32 = 200_000;
            // Thread CPU time, not wall time: a loaded host or emulator
            // deschedules the thread, and that time is not the trace's cost.
            let cpu = || crate::platform::system::clock_nanos(libc::CLOCK_THREAD_CPUTIME_ID).unwrap();
            let started = cpu();
            for _ in 0..PACKETS {
                let _t0 = tap.now();
                if enabled { source.read(&mut buf).unwrap(); source.read(&mut buf).unwrap(); }
                let t1 = tap.now();
                let t2 = tap.now();
                tap.decoded(t1, t2, 1024);
                let t3 = tap.now();
                tap.written(t3, 1024, 44100, Some(17000), &[], 0);
            }
            ((cpu() - started) / u64::from(PACKETS), trace.snapshot().events_total)
        };
        let (on, events) = run(true);
        let (off, _) = run(false);
        // 43 packets a second is 1024-frame FLAC at 44.1 kHz.
        let line = format!("PLAYTRACE_OVERHEAD enabled_ns_per_packet={on} disabled_ns_per_packet={off} cpu_ppm_at_43_packets_per_s={} events={events} event_ring_bytes={} window_ring_bytes={} profile={}\n",
            on * 43 / 1000, EVENTS * std::mem::size_of::<Event>(), WINDOWS * std::mem::size_of::<Window>(),
            if cfg!(debug_assertions) { "debug" } else { "release" });
        // The receipt of this run (cargo captures a passing test's output).
        let receipt = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build/perf/playtrace-overhead.txt");
        let _ = std::fs::write(receipt, &line);
        print!("{line}");
        assert!(on < 50_000, "{on} ns a packet in a debug build");
        assert!(off < 5_000, "{off} ns a packet disabled");
        assert!(EVENTS * std::mem::size_of::<Event>() + WINDOWS * std::mem::size_of::<Window>() <= 96 * 1024);
            })
    }
}
