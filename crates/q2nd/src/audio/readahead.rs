#![forbid(unsafe_code)]
//! Bounded read-ahead for music files, including progressive downloads.
//!
//! The player thread decodes between ALSA writes with about 400 ms buffered.
//! A read of the playing file that queues behind a fetch-ahead burst's card
//! writeback can outlast that and underrun (docs/bugs/stream-cache-underrun.md:
//! every timestamped recovery fell inside or just after a burst). Here a
//! async controller keeps up to `CAP` bytes of the file in memory ahead of the
//! decoder, so a stall of seconds reaches the PCM only if it outlasts the
//! buffer. It refills only once the buffer has fallen below half, in large
//! reads, so the card wakes about once per `CAP / 2` consumed instead of per
//! packet. Only finite card reads enter an independent per-reader worker lane;
//! progress and range requests await I/O, and an idle card worker retires.
use crate::audio::interrupt::Interrupt;
use std::collections::VecDeque;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{
    atomic::Ordering,
    Arc, Mutex,
};
use std::time::{Duration, Instant};
use symphonia::core::io::MediaSource;

/// About 50 s of MP3 320 or 17 s of FLAC at 920 kbit/s when full; a refill
/// starts at half, so a stalled refill still leaves `CAP / 2` (about 26 s
/// and 9 s).
pub const CAP: usize = 2 * 1024 * 1024;
const CHUNK: usize = 256 * 1024;
/// After a seek reads start small and double up to CHUNK: decoders that
/// bisect (FLAC, Ogg) seek several times per user seek.
const FIRST: usize = 32 * 1024;
/// A read() that sees no progress this long stops feeding the health
/// watchdog, so a hung card shows up as a hang (as a blocked File read did).
const STALL: Duration = Duration::from_secs(5);

struct Buf {
    data: VecDeque<u8>,
    /// File offset of `data[0]`.
    start: u64,
    eof: bool,
    error: Option<(io::ErrorKind, String)>,
    /// A seek the worker has not served yet.
    seek: Option<u64>,
    closed: bool,
}

struct Shared {
    buffer: Mutex<Buf>,
    ready: Arc<super::interrupt::Event>,
    changed: tokio::sync::Notify,
    #[cfg(test)]
    card_hook: CardHook,
}
impl Shared {
    async fn superseded(&self) {
        loop {
            let changed = self.changed.notified();
            {
                let buffer = self.buffer.lock().unwrap();
                if buffer.closed || buffer.seek.is_some() {
                    return;
                }
            }
            changed.await;
        }
    }
}
struct Exit(Arc<Shared>);
impl Drop for Exit {
    fn drop(&mut self) {
        let mut buffer = self.0.buffer.lock().unwrap();
        if !buffer.closed && !buffer.eof && buffer.error.is_none() {
            buffer.error = Some((io::ErrorKind::Other, "Read-ahead owner stopped".into()));
        }
        buffer.closed = true;
        self.0.ready.notify();
    }
}
// Each reader has independent card capacity, including while an obsolete
// reader's uncancellable syscall finishes. Never queue a new song behind it.
struct CardLane(Option<crate::rt::Jobs>);
impl CardLane {
    fn new() -> io::Result<Self> {
        Ok(Self(Some(crate::rt::Jobs::serial("read-ahead")?)))
    }
    async fn read(
        &self,
        card: Card,
        at: u64,
        want: usize,
        mut chunk: Vec<u8>,
        shared: &Shared,
    ) -> (Vec<u8>, io::Result<usize>) {
        #[cfg(test)]
        let hook = shared.card_hook.snapshot();
        #[cfg(not(test))]
        let _ = shared;
        let job = self.0.as_ref().unwrap().run_isolated(move |blocking| {
            #[cfg(test)]
            if let Some(hook) = hook {
                hook(at);
            }
            let result = card.read_at(blocking, &mut chunk[..want], at);
            (chunk, result)
        });
        match job.await {
            Ok(result) => result,
            Err(_) => (
                vec![0; CHUNK],
                Err(io::Error::other("Card read task stopped")),
            ),
        }
    }
}
impl Drop for CardLane {
    fn drop(&mut self) {
        if let Some(lane) = self.0.take() {
            lane.shutdown_background();
        }
    }
}
#[cfg(test)]
#[derive(Default)]
struct CardHook(Mutex<Option<Arc<dyn Fn(u64) + Send + Sync>>>);
#[cfg(test)]
impl CardHook {
    fn snapshot(&self) -> Option<Arc<dyn Fn(u64) + Send + Sync>> {
        self.0.lock().unwrap().clone()
    }
}
#[cfg(test)]
#[derive(Clone)]
struct GeneratedCard(Arc<Mutex<Box<dyn GeneratedRead>>>);
#[cfg(test)]
impl GeneratedCard {
    fn new(reader: impl GeneratedRead + 'static) -> Self {
        Self(Arc::new(Mutex::new(Box::new(reader))))
    }
    fn read_at(&self, out: &mut [u8], at: u64) -> io::Result<usize> {
        let mut reader = self.0.lock().unwrap();
        reader
            .seek(SeekFrom::Start(at))
            .and_then(|_| reader.read(out))
    }
}
#[cfg(test)]
trait GeneratedRead: Read + Seek + Send {}
#[cfg(test)]
impl<T: Read + Seek + Send> GeneratedRead for T {}
enum Card {
    File(super::source::ReadAheadFile),
    #[cfg(test)]
    Generated(GeneratedCard),
}
enum Input {
    File(super::source::ReadAheadFile),
    Progressive(super::source::AsyncProgressive),
    #[cfg(test)]
    Generated(GeneratedCard),
}
enum Ready {
    Bytes(usize),
    Card(Card, usize, Option<u64>),
}
impl Input {
    fn reposition(&mut self, at: u64) {
        if let Self::Progressive(reader) = self {
            reader.reposition(at);
        }
    }
    fn completed(&mut self, at: u64, count: usize) {
        if let Self::Progressive(reader) = self {
            reader.completed(at, count);
        }
    }
    async fn prepare_read(&mut self, at: u64, out: &mut [u8]) -> io::Result<Ready> {
        match self {
            Self::File(file) => Ok(Ready::Card(Card::File(file.clone()), out.len(), None)),
            Self::Progressive(reader) => {
                // A wait here is the stream's download (or range request)
                // falling behind playback: UX telemetry marks the long ones.
                let waiting = std::time::Instant::now();
                let prepared = reader.prepare_read(at, out).await;
                crate::ux::stream_waited(waiting.elapsed());
                match prepared? {
                    super::source::PreparedRead::Bytes(bytes) => Ok(Ready::Bytes(bytes)),
                    super::source::PreparedRead::File {
                        file,
                        length,
                        available,
                    } => Ok(Ready::Card(Card::File(file), length, Some(available))),
                }
            }
            #[cfg(test)]
            Self::Generated(reader) => Ok(Ready::Card(
                Card::Generated(reader.clone()),
                out.len(),
                None,
            )),
        }
    }
}

impl Card {
    fn read_at(&self, _blocking: &crate::rt::Blocking, out: &mut [u8], at: u64) -> io::Result<usize> {
        match self {
            Self::File(file) => file.read_at(_blocking, out, at),
            #[cfg(test)]
            Self::Generated(reader) => reader.read_at(out, at),
        }
    }
}

pub struct ReadAhead {
    shared: Arc<Shared>,
    pos: u64,
    len: Length,
    interrupt: Arc<Interrupt>,
    stop_io: Option<Arc<Interrupt>>,
}

#[derive(Clone)]
enum Length {
    Fixed(u64),
    Download(Arc<super::source::Progress>),
}
impl Length {
    fn get(&self) -> Option<u64> {
        match self {
            Self::Fixed(n) => Some(*n),
            Self::Download(progress) => {
                let fill = progress.snapshot();
                fill.total
                    .or(if fill.done { Some(fill.written) } else { None })
            }
        }
    }
    fn wait(&self, interrupt: &Interrupt) -> io::Result<u64> {
        match self {
            Self::Fixed(n) => Ok(*n),
            Self::Download(progress) => progress.wait_total(interrupt),
        }
    }
}

impl ReadAhead {
    /// The only production card input is File; progress/range waits are typed
    /// async inputs and cannot be sent to a blocking lane as generic Read.
    pub fn source(_blocking: &crate::rt::Blocking, file: File, interrupt: Arc<Interrupt>) -> io::Result<Box<dyn MediaSource>> {
        let len = file.metadata()?.len();
        match Self::start_input(
            Input::File(super::source::ReadAheadFile::new(file.try_clone()?)),
            Length::Fixed(len),
            interrupt,
            None,
        ) {
            Ok(reader) => Ok(Box::new(reader)),
            Err(error) => {
                log::warn!("read-ahead unavailable: {error}");
                Ok(Box::new(file))
            }
        }
    }
    pub fn progressive(
        reader: super::source::ProgressiveReader,
        progress: Arc<super::source::Progress>,
        interrupt: Arc<Interrupt>,
        stop_io: Arc<Interrupt>,
    ) -> io::Result<Self> {
        if Arc::ptr_eq(&interrupt, &stop_io) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "independent read-ahead cancellation required",
            ));
        }
        Self::start_input(
            Input::Progressive(reader.into_async()),
            Length::Download(progress),
            interrupt,
            Some(stop_io),
        )
    }
    #[cfg(test)]
    fn start<R: Read + Seek + Send + 'static>(
        reader: R,
        len: u64,
        interrupt: Arc<Interrupt>,
    ) -> io::Result<Self> {
        Self::start_input(
            Input::Generated(GeneratedCard::new(reader)),
            Length::Fixed(len),
            interrupt,
            None,
        )
    }
    fn start_input(
        input: Input,
        len: Length,
        interrupt: Arc<Interrupt>,
        stop_io: Option<Arc<Interrupt>>,
    ) -> io::Result<Self> {
        let lane = CardLane::new()?;
        let shared = Arc::new(Shared {
            buffer: Mutex::new(Buf {
                data: VecDeque::with_capacity(CAP),
                start: 0,
                eof: false,
                error: None,
                seek: None,
                closed: false,
            }),
            ready: Arc::default(),
            changed: tokio::sync::Notify::new(),
            #[cfg(test)]
            card_hook: CardHook::default(),
        });
        crate::rt::io().spawn(worker(input, shared.clone(), lane));
        Ok(Self {
            shared,
            pos: 0,
            len,
            interrupt,
            stop_io,
        })
    }
}

/// Generated fault journeys only (`fault-descriptor` script step, UXI-006/022):
/// the next card read of every open read-ahead fails with EBADF, as a file
/// descriptor lost across a suspend would. Each reader acts on a request once.
static DESCRIPTOR_FAULTS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
pub(crate) fn inject_descriptor_fault() { DESCRIPTOR_FAULTS.fetch_add(1, Ordering::Relaxed); }

fn worker(
    mut input: Input,
    shared: Arc<Shared>,
    lane: CardLane,
) -> impl std::future::Future<Output = ()> + Send {
    // Completion exists before the first poll, including abort/panic paths.
    let exit = Exit(shared.clone());
    async move {
        let _exit = exit;
        let lane = lane;
        let mut chunk = vec![0; CHUNK];
        let mut faults_seen = DESCRIPTOR_FAULTS.load(Ordering::Relaxed);
        let mut filling = true;
        let mut size = FIRST;
        loop {
            let changed = shared.changed.notified();
            let next = {
                let mut buffer = shared.buffer.lock().unwrap();
                if buffer.closed {
                    return;
                }
                if let Some(at) = buffer.seek.take() {
                    buffer.data.clear();
                    buffer.start = at;
                    buffer.eof = false;
                    buffer.error = None;
                    filling = true;
                    size = FIRST;
                    input.reposition(at);
                }
                if buffer.data.len() < CAP / 2 {
                    filling = true;
                }
                if filling && !buffer.eof && buffer.error.is_none() && buffer.data.len() < CAP {
                    Some((
                        buffer.start + buffer.data.len() as u64,
                        size.min(CAP - buffer.data.len()),
                    ))
                } else {
                    filling = false;
                    None
                }
            };
            let Some((at, want)) = next else {
                changed.await;
                continue;
            };
            let ready = tokio::select! {
                biased;
                ()=shared.superseded()=>continue,
                ready=input.prepare_read(at,&mut chunk[..want])=>ready,
            };
            let result = match ready {
                Ok(Ready::Bytes(count)) => Ok(count),
                Ok(Ready::Card(card, want, available)) => {
                    // A card syscall cannot be cancelled. Join it even after
                    // close/new seek, then reject superseded bytes below.
                    let (returned, mut result) = lane.read(card, at, want, chunk, &shared).await;
                    chunk = returned;
                    let faults = DESCRIPTOR_FAULTS.load(Ordering::Relaxed);
                    if faults != faults_seen {
                        faults_seen = faults;
                        result = Err(io::Error::from_raw_os_error(libc::EBADF));
                    }
                    if let (Some(available), Ok(count)) = (available, &result) {
                        crate::playtrace::note_ahead(available - *count as u64);
                    }
                    result
                }
                Err(error) => Err(error),
            };
            if let Ok(count) = &result {
                input.completed(at, *count);
            }
            size = (size * 2).min(CHUNK);
            let mut buffer = shared.buffer.lock().unwrap();
            if buffer.closed {
                return;
            }
            if buffer.seek.is_some() || buffer.start + buffer.data.len() as u64 != at {
                continue;
            }
            match result {
                Ok(0) => buffer.eof = true,
                Ok(count) => buffer.data.extend(&chunk[..count]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => buffer.error = Some((error.kind(), error.to_string())),
            }
            shared.ready.notify();
        }
    }
}

impl Read for ReadAhead {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        super::source::reader_context()?;
        if out.is_empty() {
            return Ok(0);
        }
        let (lock, cv) = (&self.shared.buffer, &self.shared.ready);
        let mut wait = super::interrupt::Waiter::new(&self.interrupt, cv);
        let waiting = Instant::now();
        loop {
            let mut b = lock.lock().unwrap();
            if self.interrupt.load(Ordering::Relaxed) {
                return Err(super::source::cut());
            }
            if b.seek.is_none() && b.start <= self.pos {
                let skip = (self.pos - b.start) as usize;
                if skip < b.data.len() {
                    // Consumed bytes leave the buffer: the window only moves forward.
                    b.data.drain(..skip);
                    b.start = self.pos;
                    let n = {
                        let (front, back) = b.data.as_slices();
                        let a = out.len().min(front.len());
                        out[..a].copy_from_slice(&front[..a]);
                        let c = (out.len() - a).min(back.len());
                        out[a..a + c].copy_from_slice(&back[..c]);
                        a + c
                    };
                    b.data.drain(..n);
                    b.start += n as u64;
                    self.pos += n as u64;
                    // A full worker sleeps until a refill is useful. Waking it
                    // on every decoder read defeats the half-buffer policy.
                    // At the end of the file (or after a read error) there is
                    // nothing to refill: do not wake the I/O thread per read.
                    if b.data.len() < CAP / 2 && !b.eof && b.error.is_none() {
                        self.shared.changed.notify_one();
                    }
                    return Ok(n);
                }
                if b.eof && skip >= b.data.len() {
                    return Ok(0);
                }
                if let Some((kind, msg)) = &b.error {
                    return Err(io::Error::new(*kind, msg.clone()));
                }
            }
            if b.closed {
                return Err(io::Error::other("Read-ahead owner stopped"));
            }
            // A skip or stop must get through a stalled card (as with ProgressiveReader).
            if self.interrupt.load(Ordering::Relaxed) {
                return Err(super::source::cut());
            }
            drop(b);
            super::source::wait_context()?;
            let health = if waiting.elapsed() < STALL {
                crate::health::pulse();
                super::interrupt::health_interval()
            } else { None };
            wait.wait(health)?;
        }
    }
}

impl Seek for ReadAhead {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        super::source::reader_context()?;
        let target = match to {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::End(d) => self.len.wait(&self.interrupt)? as i128 + d as i128,
            SeekFrom::Current(d) => self.pos as i128 + d as i128,
        };
        if target < 0 || target > u64::MAX as i128 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "seek outside file offset range",
            ));
        }
        let target = target as u64;
        let (lock, cv) = (&self.shared.buffer, &self.shared.ready);
        let mut b = lock.lock().unwrap();
        let end = b.start + b.data.len() as u64;
        // Inside the buffered window (or its end): no new read is needed.
        if b.seek.is_none() && b.start <= target && target <= end {
            self.pos = target;
        } else {
            b.seek = Some(target);
            self.pos = target;
            cv.notify();
            self.shared.changed.notify_one();
        }
        Ok(target)
    }
}

impl MediaSource for ReadAhead {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        super::callback::assert_allowed();
        self.len.get()
    }
}

impl Drop for ReadAhead {
    fn drop(&mut self) {
        if let Some(stop) = &self.stop_io {
            stop.store(true, Ordering::Relaxed);
        }
        let (lock, cv) = (&self.shared.buffer, &self.shared.ready);
        let mut state = lock.lock().unwrap();
        state.closed = true;
        let data = std::mem::take(&mut state.data);
        drop(state);
        drop(data);
        cv.notify();
        self.shared.changed.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn file(len: usize) -> (std::path::PathBuf, Vec<u8>) {
        let dir = std::env::var_os("Q2_GENERATED_TEST_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build")
            })
            .join("test-readahead");
        std::fs::create_dir_all(&dir).unwrap();
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let path = dir.join(format!(
            "{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let data: Vec<u8> = (0..len).map(|i| (i * 7 + i / 251) as u8).collect();
        std::fs::File::create(&path)
            .unwrap()
            .write_all(&data)
            .unwrap();
        (path, data)
    }

    fn open(path: &std::path::Path) -> ReadAhead {
        let f = File::open(path).unwrap();
        let len = f.metadata().unwrap().len();
        ReadAhead::start(f, len, Arc::new(Interrupt::new(false))).unwrap()
    }

    /// A reader whose reads block while `stall` is set.
    struct Slow {
        inner: std::io::Cursor<Vec<u8>>,
        stall: Arc<std::sync::atomic::AtomicBool>,
    }
    impl Read for Slow {
        fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
            while self.stall.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(5));
            }
            self.inner.read(b)
        }
    }
    impl Seek for Slow {
        fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
            self.inner.seek(to)
        }
    }

    #[test]
    fn buffered_bytes_carry_a_stall_and_an_interrupt_ends_the_wait() {
        crate::audio::callback::test_scope(|| {
        let data: Vec<u8> = (0..CAP * 2).map(|i| i as u8).collect();
        let stall = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let interrupt = Arc::new(Interrupt::new(false));
        let mut r = ReadAhead::start(
            Slow {
                inner: std::io::Cursor::new(data.clone()),
                stall: stall.clone(),
            },
            data.len() as u64,
            interrupt.clone(),
        )
        .unwrap();
        // Let it fill, then stall the "card": the buffered window still reads.
        let mut b = vec![0u8; 1000];
        r.read_exact(&mut b).unwrap();
        for _ in 0..200 {
            if r.shared.buffer.lock().unwrap().data.len() >= CAP - 1000 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        stall.store(true, Ordering::Relaxed);
        let mut got = 1000;
        while got < CAP / 2 {
            got += r.read(&mut b).unwrap();
        }
        assert_eq!(&b[..10], &data[got - 1000..got - 990]);
        assert!(
            r.shared.buffer.lock().unwrap().data.capacity() <= CAP,
            "one allocation of CAP"
        );
        // A seek beyond the window waits on the stalled reader; the interrupt ends it.
        r.seek(SeekFrom::Start((CAP * 2 - 10) as u64)).unwrap();
        interrupt.store(true, Ordering::Relaxed);
        let e = r.read(&mut b).unwrap_err();
        assert!(e
            .get_ref()
            .is_some_and(|e| e.is::<crate::audio::source::Cut>()));
        interrupt.store(false, Ordering::Relaxed);
        stall.store(false, Ordering::Relaxed);
        r.read_exact(&mut b[..10]).unwrap();
        assert_eq!(&b[..10], &data[CAP * 2 - 10..]);
            })
    }

    #[test]
    fn sequential_reads_match_the_file_across_refills() {
        crate::audio::callback::test_scope(|| {
        let (path, data) = file(CAP * 3 + 12345);
        let mut r = open(&path);
        let mut got = Vec::new();
        let mut buf = vec![0u8; 3000];
        loop {
            let n = r.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
        }
        assert_eq!(got, data);
        assert_eq!(r.byte_len(), Some(data.len() as u64));
        std::fs::remove_file(path).unwrap();
            })
    }

    #[test]
    fn seeks_inside_and_outside_the_window_and_from_the_end() {
        crate::audio::callback::test_scope(|| {
        let (path, data) = file(CAP * 2 + 999);
        let mut r = open(&path);
        let mut b = [0u8; 100];
        r.read_exact(&mut b).unwrap();
        assert_eq!(&b[..], &data[..100]);
        // Forward inside what is buffered.
        r.seek(SeekFrom::Current(5000)).unwrap();
        r.read_exact(&mut b).unwrap();
        assert_eq!(&b[..], &data[5100..5200]);
        // Backward, outside the window (consumed bytes are gone): a re-read.
        r.seek(SeekFrom::Start(10)).unwrap();
        r.read_exact(&mut b).unwrap();
        assert_eq!(&b[..], &data[10..110]);
        // Far forward, then from the end.
        r.seek(SeekFrom::Start(CAP as u64 + 7)).unwrap();
        r.read_exact(&mut b).unwrap();
        assert_eq!(&b[..], &data[CAP + 7..CAP + 107]);
        let at = r.seek(SeekFrom::End(-50)).unwrap();
        assert_eq!(at, data.len() as u64 - 50);
        let mut tail = Vec::new();
        r.read_to_end(&mut tail).unwrap();
        assert_eq!(tail, &data[data.len() - 50..]);
        assert!(r.seek(SeekFrom::Current(-(data.len() as i64) - 1)).is_err());
        std::fs::remove_file(path).unwrap();
            })
    }

    #[test]
    fn empty_file_and_drop_ends_the_worker() {
        crate::audio::callback::test_scope(|| {
        let (path, _) = file(0);
        let mut r = open(&path);
        assert_eq!(r.read(&mut [0u8; 10]).unwrap(), 0);
        let weak = Arc::downgrade(&r.shared);
        drop(r);
        for _ in 0..100 {
            if weak.upgrade().is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            weak.upgrade().is_none(),
            "the worker exits and releases the buffer"
        );
        std::fs::remove_file(path).unwrap();
            })
    }
}

#[cfg(test)]
mod progressive_tests {
    use super::super::source::{Progress, ProgressiveReader};
    use super::*;
    use std::sync::mpsc;

    fn generated(len: usize) -> (std::path::PathBuf, Vec<u8>) {
        let dir = std::env::var_os("Q2_GENERATED_TEST_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build")
            })
            .join("test-progressive-buffer");
        std::fs::create_dir_all(&dir).unwrap();
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = dir.join(format!(
            "{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let bytes = (0..len)
            .map(|n| (n.wrapping_mul(7) % 251) as u8)
            .collect::<Vec<_>>();
        std::fs::write(&path, &bytes).unwrap();
        (path, bytes)
    }
    pub(super) fn full(reader: &ReadAhead) {
        let (lock, ready) = (&reader.shared.buffer, &reader.shared.ready);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let version = ready.version();
            let state = lock.lock().unwrap();
            if state.data.len() >= CAP || state.error.is_some() || state.eof {
                assert_eq!(state.data.len(), CAP);
                return;
            }
            drop(state);
            let remaining = deadline.saturating_duration_since(Instant::now());
            assert!(!remaining.is_zero(), "buffer did not become ready");
            super::super::callback::test_scope(|| ready.wait_after(version, Some(remaining))).unwrap();
        }
    }
    #[test]
    fn progressive_cached_bytes_feed_the_consumer_while_card_read_is_blocked() {
        crate::audio::callback::test_scope(|| {
        let (path, bytes) = generated(CAP * 3);
        let progress = Progress::new();
        progress.advance(bytes.len() as u64);
        progress.finish();
        let io_cancel = Arc::new(Interrupt::new(false));
        let consumer_cancel = Arc::new(Interrupt::new(false));
        let file = ProgressiveReader::open(&crate::rt::Blocking::dedicated_thread(), &path, progress.clone(), io_cancel.clone()).unwrap();
        let (entered_tx, entered) = mpsc::channel();
        let (release, release_rx) = mpsc::channel();
        let mut reader =
            ReadAhead::progressive(file, progress, consumer_cancel.clone(), io_cancel.clone())
                .unwrap();
        full(&reader);
        let entered_hook = Mutex::new(Some(entered_tx));
        let release_rx = Mutex::new(release_rx);
        *reader.shared.card_hook.0.lock().unwrap() = Some(Arc::new(move |at| {
            if at >= CAP as u64 {
                if let Some(entered) = entered_hook.lock().unwrap().take() {
                    entered.send(()).unwrap();
                    release_rx.lock().unwrap().recv().unwrap();
                }
            }
        }));
        let first = CAP / 2 + 1024;
        let mut prefix = vec![0; first];
        reader.read_exact(&mut prefix).unwrap();
        assert_eq!(prefix, &bytes[..first]);
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        let (done_tx, done) = mpsc::channel();
        let worker = std::thread::spawn(move || { crate::audio::callback::test_scope(|| {
            let mut next = [0; 4096];
            let read = reader.read_exact(&mut next);
            done_tx.send((read, next)).unwrap();
            reader
         }) });
        let immediate = done.recv_timeout(Duration::from_millis(250));
        // Always release the injected syscall before asserting, including failure.
        release.send(()).unwrap();
        let reader = worker.join().unwrap();
        let (read, next) = immediate.expect("buffered playback waited for the stalled card");
        read.unwrap();
        assert_eq!(&next, &bytes[first..first + 4096]);
        assert!(reader.shared.buffer.lock().unwrap().data.len() <= CAP);
        let shared = reader.shared.clone();
        drop(reader);
        assert!(io_cancel.load(Ordering::Relaxed));
        assert!(!consumer_cancel.load(Ordering::Relaxed));
        assert_eq!(shared.buffer.lock().unwrap().data.capacity(), 0);
        std::fs::remove_file(path).unwrap();
            })
    }

    #[test]
    fn progressive_length_reads_and_end_seeks_follow_the_download() {
        crate::audio::callback::test_scope(|| {
        use std::io::Write;
        let (path, bytes) = generated(4);
        let progress = Progress::new();
        progress.advance(4);
        let io_cancel = Arc::new(Interrupt::new(false));
        let consumer = Arc::new(Interrupt::new(false));
        let source = ProgressiveReader::open(&crate::rt::Blocking::dedicated_thread(), &path, progress.clone(), io_cancel.clone()).unwrap();
        let mut reader =
            ReadAhead::progressive(source, progress.clone(), consumer, io_cancel).unwrap();
        assert_eq!(reader.byte_len(), None);
        let mut first = [0; 4];
        reader.read_exact(&mut first).unwrap();
        assert_eq!(&first, &bytes[..]);
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"next")
            .unwrap();
        progress.advance(8);
        progress.finish();
        let mut next = [0; 4];
        reader.read_exact(&mut next).unwrap();
        assert_eq!(&next, b"next");
        assert_eq!(reader.byte_len(), Some(8));
        assert_eq!(reader.seek(SeekFrom::End(-2)).unwrap(), 6);
        let mut tail = [0; 2];
        reader.read_exact(&mut tail).unwrap();
        assert_eq!(&tail, b"xt");
        assert_eq!(reader.read(&mut tail).unwrap(), 0);
        drop(reader);
        std::fs::remove_file(path).unwrap();
            })
    }

    #[test]
    fn command_interrupt_does_not_consume_buffered_bytes_or_cancel_the_new_song() {
        crate::audio::callback::test_scope(|| {
        let (path, bytes) = generated(CAP * 2);
        let progress = Progress::new();
        progress.advance(bytes.len() as u64);
        progress.finish();
        let io_cancel = Arc::new(Interrupt::new(false));
        let consumer = Arc::new(Interrupt::new(false));
        let source = ProgressiveReader::open(&crate::rt::Blocking::dedicated_thread(), &path, progress.clone(), io_cancel.clone()).unwrap();
        let mut reader =
            ReadAhead::progressive(source, progress, consumer.clone(), io_cancel.clone()).unwrap();
        full(&reader);
        consumer.store(true, Ordering::Relaxed);
        assert!(super::super::source::is_cut(
            &reader.read(&mut [0; 16]).unwrap_err()
        ));
        assert_eq!(reader.pos, 0);
        consumer.store(false, Ordering::Relaxed);
        let mut prefix = [0; 16];
        reader.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix, &bytes[..16]);
        drop(reader);
        assert!(io_cancel.load(Ordering::Relaxed));
        assert!(!consumer.load(Ordering::Relaxed));
        std::fs::remove_file(path).unwrap();
            })
    }

    #[test]
    fn dropping_an_empty_progressive_reader_retires_its_own_download_wait() {
        crate::audio::callback::test_scope(|| {
        let (path, _) = generated(0);
        let progress = Progress::new();
        let io_cancel = Arc::new(Interrupt::new(false));
        let consumer = Arc::new(Interrupt::new(false));
        let source = ProgressiveReader::open(&crate::rt::Blocking::dedicated_thread(), &path, progress.clone(), io_cancel.clone()).unwrap();
        let reader =
            ReadAhead::progressive(source, progress, consumer.clone(), io_cancel.clone()).unwrap();
        let weak = Arc::downgrade(&reader.shared);
        drop(reader);
        for _ in 0..100 {
            if weak.upgrade().is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(weak.upgrade().is_none());
        assert!(io_cancel.load(Ordering::Relaxed));
        assert!(!consumer.load(Ordering::Relaxed));
        std::fs::remove_file(path).unwrap();
            })
    }

    #[test]
    fn shared_consumer_and_worker_cancellation_is_refused() {
        crate::audio::callback::test_scope(|| {
        let flag = Arc::new(Interrupt::new(false));
        let (path, _) = generated(3);
        let progress = Progress::new();
        let source = ProgressiveReader::open(&crate::rt::Blocking::dedicated_thread(), &path, progress.clone(), flag.clone()).unwrap();
        let result = ReadAhead::progressive(source, progress, flag.clone(), flag.clone());
        assert!(result.is_err());
        assert!(!flag.load(Ordering::Relaxed));
        std::fs::remove_file(path).unwrap();
            })
    }
}

#[cfg(test)]
#[path = "readahead_async_tests.rs"]
mod async_tests;

#[cfg(test)]
#[path = "readahead_event_tests.rs"]
mod event_tests;
