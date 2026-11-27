#![forbid(unsafe_code)]
//! The PCM the player opens for Bluetooth output. It behaves as an ALSA
//! playback device does: a bounded queue, a start threshold, blocking writes,
//! an underrun state and a delay in source frames. The Bluetooth thread is
//! its hardware: it takes frames at the stream's pace.
use std::collections::VecDeque;
use std::ffi::c_int;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};
use crate::rt::Blocking;

/// The device name of this PCM (`audio::output::Route::Bluetooth`).
pub const DEVICE: &str = "q2bt";
const STATE_PREPARED: c_int = 2;
const STATE_RUNNING: c_int = 3;
const STATE_XRUN: c_int = 4;
const STATE_DISCONNECTED: c_int = 8;
/// A write that makes no progress for this long reports an I/O error.
const WRITE_STALL: Duration = Duration::from_secs(3);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Run { Prepared, Running, Xrun }

struct Inner {
    /// The headphones' stream is open.
    available: bool,
    /// Counts opens, so the Bluetooth side notices a new format or a flush.
    serial: u64,
    open: bool,
    rate: u32,
    channels: usize,
    capacity: usize,
    run: Run,
    /// Interleaved samples in the source's rate and channel count.
    queue: VecDeque<i16>,
    /// Source frames the Bluetooth side took but has not sent yet.
    staged: u64,
    draining: bool,
    underruns: u32,
}

pub struct Endpoint { inner: Mutex<Inner>, changed: Condvar, wake: Mutex<Option<Arc<dyn Fn() + Send + Sync>>> }

/// The process's endpoint: the player's sink and the Bluetooth thread meet here.
pub fn endpoint() -> &'static Arc<Endpoint> {
    static ENDPOINT: OnceLock<Arc<Endpoint>> = OnceLock::new();
    ENDPOINT.get_or_init(|| Arc::new(Endpoint::new()))
}

/// What the Bluetooth side took: stereo samples at `rate`.
pub struct Taken { pub serial: u64, pub rate: u32, pub stereo: Vec<i16>, pub underrun: bool }

impl Endpoint {
    pub fn new() -> Self {
        Self { inner: Mutex::new(Inner { available: false, serial: 0, open: false, rate: 0, channels: 0, capacity: 0, run: Run::Prepared,
            queue: VecDeque::new(), staged: 0, draining: false, underruns: 0 }), changed: Condvar::new(), wake: Mutex::new(None) }
    }
    /// Called when written audio is ready to be taken.
    pub fn set_wake(&self, wake: Option<Arc<dyn Fn() + Send + Sync>>) { *self.wake.lock().unwrap() = wake; }
    fn wake(&self) { let wake = self.wake.lock().unwrap().clone(); if let Some(wake) = wake { wake(); } }

    /// The stream opened or closed. Closing fails blocked and later writes.
    pub fn set_available(&self, available: bool) {
        let mut inner = self.inner.lock().unwrap();
        inner.available = available;
        if !available { inner.staged = 0; }
        drop(inner);
        self.changed.notify_all();
    }
    /// Whether the player has audio running that the stream should carry.
    pub fn running(&self) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.open && inner.run == Run::Running
    }
    pub fn underruns(&self) -> u32 { self.inner.lock().unwrap().underruns }
    /// Tells `delay` how many taken source frames are not sent yet.
    pub fn set_staged(&self, frames: u64) { self.inner.lock().unwrap().staged = frames; }

    /// Takes up to `frames` source frames. An empty queue while running is an underrun.
    pub fn take(&self, frames: usize) -> Option<Taken> {
        let mut inner = self.inner.lock().unwrap();
        if !inner.open || inner.run != Run::Running { return None }
        let channels = inner.channels;
        let n = frames.min(inner.queue.len() / channels);
        let mut stereo = Vec::with_capacity(n * 2);
        for _ in 0..n {
            let left = inner.queue.pop_front().unwrap();
            let right = if channels > 1 { inner.queue.pop_front().unwrap() } else { left };
            for _ in 2..channels { inner.queue.pop_front(); }
            stereo.push(left); stereo.push(right);
        }
        let underrun = n < frames && inner.queue.is_empty();
        if underrun {
            // A drain ends in an empty queue by design; anything else is an underrun.
            if !inner.draining { inner.underruns += 1; inner.run = Run::Xrun; } else { inner.run = Run::Prepared; }
        }
        let taken = Taken { serial: inner.serial, rate: inner.rate, stereo, underrun };
        drop(inner);
        self.changed.notify_all();
        Some(taken)
    }
}

/// The player's handle; one at a time. Method names follow `pcm_ffi::Handle`.
pub struct Writer { endpoint: Arc<Endpoint>, channels: usize }

fn errno(code: c_int) -> i64 { -i64::from(code) }

impl Writer {
    pub fn open(endpoint: &Arc<Endpoint>, rate: u32, channels: u32, latency_us: u32) -> Result<Self, String> {
        if !(8000..=384_000).contains(&rate) || !(1..=8).contains(&channels) { return Err("Bluetooth output does not support this format".into()) }
        let mut inner = endpoint.inner.lock().unwrap();
        if !inner.available { return Err("Bluetooth headphones are not connected".into()) }
        if inner.open { return Err("Bluetooth output is busy".into()) }
        inner.serial += 1;
        inner.open = true; inner.rate = rate; inner.channels = channels as usize; inner.run = Run::Prepared; inner.draining = false;
        inner.capacity = ((u64::from(rate) * u64::from(latency_us) / 1_000_000) as usize).max(1024);
        inner.queue.clear(); inner.staged = 0;
        Ok(Self { endpoint: endpoint.clone(), channels: channels as usize })
    }
    pub fn capacity(&self) -> u64 { self.endpoint.inner.lock().unwrap().capacity as u64 }

    fn push(&self, _blocking: &Blocking, frames: usize, mut sample: impl FnMut(usize) -> i16) -> i64 {
        let endpoint = &*self.endpoint;
        let mut inner = endpoint.inner.lock().unwrap();
        let mut written = 0;
        let mut waited_since: Option<Instant> = None;
        while written < frames {
            if !inner.available { return if written > 0 { written as i64 } else { errno(libc::ENODEV) } }
            if inner.run == Run::Xrun { return if written > 0 { written as i64 } else { errno(libc::EPIPE) } }
            let room = inner.capacity.saturating_sub(inner.queue.len() / self.channels);
            if room == 0 {
                // A full queue starts playback, as a start threshold does.
                if inner.run == Run::Prepared { inner.run = Run::Running; endpoint.wake(); }
                let since = *waited_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= WRITE_STALL { return if written > 0 { written as i64 } else { errno(libc::EIO) } }
                inner = endpoint.changed.wait_timeout(inner, Duration::from_millis(100)).unwrap().0;
                continue;
            }
            waited_since = None;
            let n = room.min(frames - written);
            for i in written * self.channels..(written + n) * self.channels { inner.queue.push_back(sample(i)); }
            written += n;
            if inner.run == Run::Prepared && inner.queue.len() / self.channels >= inner.capacity / 2 { inner.run = Run::Running; }
            if inner.run == Run::Running { endpoint.wake(); }
        }
        written as i64
    }
    /// Full-scale 32-bit samples; the top 16 bits are sent.
    pub fn write(&self, blocking: &Blocking, samples: &[i32]) -> i64 {
        if samples.len() % self.channels != 0 { return errno(libc::EINVAL) }
        self.push(blocking, samples.len() / self.channels, |i| (samples[i] >> 16) as i16)
    }
    pub fn write_s16(&self, blocking: &Blocking, samples: &[i16]) -> i64 {
        if samples.len() % self.channels != 0 { return errno(libc::EINVAL) }
        self.push(blocking, samples.len() / self.channels, |i| samples[i])
    }
    pub fn recover(&self, code: c_int) -> c_int {
        if code != -libc::EPIPE { return code }
        let mut inner = self.endpoint.inner.lock().unwrap();
        if !inner.available { return -libc::ENODEV }
        inner.run = Run::Prepared;
        0
    }
    pub fn error(code: c_int) -> String {
        match -code {
            libc::ENODEV => "Bluetooth headphones disconnected".into(),
            libc::EIO => "Bluetooth headphones stopped taking audio".into(),
            code => std::io::Error::from_raw_os_error(code).to_string(),
        }
    }
    /// Source frames written and not yet sent to the headphones.
    pub fn delay(&self) -> u64 {
        let inner = self.endpoint.inner.lock().unwrap();
        (inner.queue.len() / self.channels) as u64 + inner.staged
    }
    pub fn state(&self) -> c_int {
        let inner = self.endpoint.inner.lock().unwrap();
        if !inner.available { return STATE_DISCONNECTED }
        match inner.run { Run::Prepared => STATE_PREPARED, Run::Running => STATE_RUNNING, Run::Xrun => STATE_XRUN }
    }
    /// The drop (`prepare` false) or prepare half of a reset.
    pub fn reset_step(&self, prepare: bool) -> c_int {
        let mut inner = self.endpoint.inner.lock().unwrap();
        if !prepare {
            // A new serial tells the Bluetooth side to discard what it staged.
            inner.queue.clear(); inner.staged = 0; inner.serial += 1;
        }
        inner.run = Run::Prepared; inner.draining = false;
        drop(inner);
        self.endpoint.changed.notify_all();
        self.endpoint.wake();
        0
    }
    /// Plays out the queue, bounded by its length in time plus a margin.
    pub fn drain(&self, _blocking: &Blocking) {
        let endpoint = &*self.endpoint;
        let mut inner = endpoint.inner.lock().unwrap();
        if inner.queue.is_empty() || !inner.available || inner.run == Run::Xrun { return }
        inner.run = Run::Running; inner.draining = true;
        endpoint.wake();
        let limit = Instant::now() + Duration::from_millis(1000 * inner.queue.len() as u64 / self.channels as u64 / u64::from(inner.rate.max(1)) + 1500);
        while !inner.queue.is_empty() && inner.available && inner.run == Run::Running && Instant::now() < limit {
            inner = endpoint.changed.wait_timeout(inner, Duration::from_millis(50)).unwrap().0;
        }
        inner.draining = false;
        if inner.queue.is_empty() { inner.run = Run::Prepared; }
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        let mut inner = self.endpoint.inner.lock().unwrap();
        inner.open = false; inner.queue.clear(); inner.staged = 0; inner.run = Run::Prepared; inner.serial += 1;
        drop(inner);
        self.endpoint.changed.notify_all();
        self.endpoint.wake();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(rate: u32, channels: u32) -> (Arc<Endpoint>, Writer) {
        let endpoint = Arc::new(Endpoint::new());
        endpoint.set_available(true);
        let writer = Writer::open(&endpoint, rate, channels, 400_000).unwrap();
        (endpoint, writer)
    }

    #[test]
    fn opens_only_while_the_stream_is_available_and_one_at_a_time() {
        let endpoint = Arc::new(Endpoint::new());
        assert_eq!(Writer::open(&endpoint, 44100, 2, 400_000).err().unwrap(), "Bluetooth headphones are not connected");
        endpoint.set_available(true);
        let first = Writer::open(&endpoint, 44100, 2, 400_000).unwrap();
        assert_eq!(first.capacity(), 17640);
        assert!(Writer::open(&endpoint, 44100, 2, 400_000).is_err());
        drop(first);
        assert!(Writer::open(&endpoint, 48000, 1, 400_000).is_ok());
        assert!(Writer::open(&endpoint, 0, 2, 400_000).is_err());
    }

    #[test]
    fn playback_starts_at_half_a_buffer_and_delay_counts_queued_and_staged_frames() {
        let (endpoint, writer) = open(44100, 2);
        assert_eq!(writer.write_s16(&Blocking::dedicated_thread(), &[1, 2, 3, 4]), 2);
        assert_eq!((writer.state(), writer.delay()), (STATE_PREPARED, 2));
        assert!(endpoint.take(64).is_none(), "nothing is consumed before the start threshold");
        assert_eq!(writer.write(&Blocking::dedicated_thread(), &vec![0x7fff_0000; 2 * 8820]), 8820);
        assert_eq!(writer.state(), STATE_RUNNING);
        let taken = endpoint.take(3).unwrap();
        assert_eq!((taken.rate, taken.stereo, taken.underrun), (44100, vec![1, 2, 3, 4, 0x7fff, 0x7fff], false));
        endpoint.set_staged(3);
        assert_eq!(writer.delay(), 8822);
    }

    #[test]
    fn mono_is_doubled_and_extra_channels_are_dropped() {
        let (endpoint, writer) = open(48000, 1);
        writer.write_s16(&Blocking::dedicated_thread(), &vec![5; 9600]);
        assert_eq!(endpoint.take(2).unwrap().stereo, [5, 5, 5, 5]);
        drop(writer);
        let writer = Writer::open(&endpoint, 48000, 4, 400_000).unwrap();
        writer.write_s16(&Blocking::dedicated_thread(), &(0..4 * 9600).map(|n| (n % 4) as i16).collect::<Vec<_>>());
        assert_eq!(endpoint.take(2).unwrap().stereo, [0, 1, 0, 1]);
    }

    #[test]
    fn an_empty_queue_is_an_underrun_that_recover_clears() {
        let (endpoint, writer) = open(44100, 2);
        writer.write_s16(&Blocking::dedicated_thread(), &vec![0; 2 * 8820]);
        let taken = endpoint.take(10_000).unwrap();
        assert!(taken.underrun && taken.stereo.len() == 2 * 8820);
        assert_eq!((writer.state(), endpoint.underruns()), (STATE_XRUN, 1));
        assert_eq!(writer.write_s16(&Blocking::dedicated_thread(), &[0, 0]), -i64::from(libc::EPIPE));
        assert_eq!(writer.recover(-libc::EPIPE), 0);
        assert_eq!(writer.write_s16(&Blocking::dedicated_thread(), &[0, 0]), 1);
        assert_eq!(writer.recover(-libc::EIO), -libc::EIO);
    }

    #[test]
    fn a_full_queue_blocks_the_writer_until_frames_are_taken() {
        let (endpoint, writer) = open(8000, 2);
        let capacity = writer.capacity() as usize;
        let consumer = { let endpoint = endpoint.clone(); std::thread::spawn(move || {
            let mut total = 0;
            while total < capacity * 3 {
                match endpoint.take(256) { Some(taken) => total += taken.stereo.len() / 2, None => std::thread::sleep(Duration::from_millis(1)) }
                std::thread::sleep(Duration::from_micros(200));
            }
            total
        }) };
        let pcm: Vec<i16> = (0..capacity * 3 * 2).map(|n| n as i16).collect();
        assert_eq!(writer.write_s16(&Blocking::dedicated_thread(), &pcm), (capacity * 3) as i64);
        assert!(consumer.join().unwrap() >= capacity * 3);
    }

    #[test]
    fn disconnect_fails_a_blocked_write_with_the_accepted_prefix_then_enodev() {
        let (endpoint, writer) = open(8000, 2);
        let capacity = writer.capacity() as usize;
        let closer = { let endpoint = endpoint.clone(); std::thread::spawn(move || { std::thread::sleep(Duration::from_millis(50)); endpoint.set_available(false); }) };
        assert_eq!(writer.write_s16(&Blocking::dedicated_thread(), &vec![0; (capacity + 100) * 2]), capacity as i64);
        closer.join().unwrap();
        assert_eq!(writer.write_s16(&Blocking::dedicated_thread(), &[0, 0]), -i64::from(libc::ENODEV));
        assert_eq!((writer.state(), writer.recover(-libc::EPIPE)), (STATE_DISCONNECTED, -libc::ENODEV));
        assert_eq!(Writer::error(-libc::ENODEV), "Bluetooth headphones disconnected");
        // The unsent tail is still counted, so the player can retain it.
        assert_eq!(writer.delay(), capacity as u64);
    }

    #[test]
    fn drop_discards_the_queue_and_changes_the_serial_and_drain_plays_a_short_tail() {
        let (endpoint, writer) = open(44100, 2);
        writer.write_s16(&Blocking::dedicated_thread(), &vec![0; 2 * 9000]);
        let before = endpoint.take(1).unwrap().serial;
        assert_eq!(writer.reset_step(false), 0);
        assert_eq!((writer.delay(), writer.state()), (0, STATE_PREPARED));
        assert_eq!(writer.reset_step(true), 0);
        writer.write_s16(&Blocking::dedicated_thread(), &vec![0; 2 * 100]);
        assert!(endpoint.take(1).is_none(), "below the start threshold");
        let consumer = { let endpoint = endpoint.clone(); std::thread::spawn(move || {
            let limit = Instant::now() + Duration::from_secs(2);
            loop {
                if let Some(taken) = endpoint.take(64) { if taken.underrun { return taken.serial } }
                assert!(Instant::now() < limit);
                std::thread::sleep(Duration::from_millis(1));
            }
        }) };
        writer.drain(&Blocking::dedicated_thread());
        assert!(consumer.join().unwrap() > before);
        assert_eq!((writer.state(), endpoint.underruns()), (STATE_PREPARED, 0));
    }
}
