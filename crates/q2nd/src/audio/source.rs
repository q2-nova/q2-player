#![forbid(unsafe_code)]
//! A file that is still being downloaded, read as it fills: playback starts
//! once the first bytes land, and the finished file is the cached copy.

use crate::audio::interrupt::Interrupt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
#[cfg(test)]
use std::sync::Condvar;
use std::sync::mpsc::{self, TryRecvError};
#[cfg(test)]
use std::time::Duration;

/// The error a read returns when a command cut its wait. Not
/// `ErrorKind::Interrupted`, which readers take as "try again".
#[derive(Debug)]
pub struct Cut;

impl std::fmt::Display for Cut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "interrupted")
    }
}

impl std::error::Error for Cut {}

pub(super) fn cut() -> io::Error {
    io::Error::other(Cut)
}

/// Whether `e` is a cut wait.
pub fn is_cut(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|inner| inner.is::<Cut>())
}

/// Foreign Read/Seek callbacks cannot accept a Blocking argument (and their
/// MediaSource must remain Send + Sync for transfer to the demux owner). Never
/// mint a witness here: reject executor use before any lock or file operation.
pub(super) fn reader_context() -> io::Result<()> {
    super::callback::require()
}
/// A finite worker may read available bytes; it must not wait synchronously
/// for producer work which may need the I/O runtime or a finite pool itself.
pub(super) fn wait_context() -> io::Result<()> {
    reader_context()?;
    if crate::rt::on_pool() {
        Err(io::Error::new(io::ErrorKind::WouldBlock, "audio producer wait on finite worker"))
    } else { Ok(()) }
}

/// Cache publication is separate from readable transport completion. A failed
/// publication must not invalidate bytes a progressive reader already owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CachePublication {
    Published = 1,
    RenameFailed = 2,
    IndexFailed = 3,
    Hit = 4,
}

#[derive(Debug, Default, Clone)]
pub struct Fill {
    pub written: u64,
    /// The full length, when the server said.
    pub total: Option<u64>,
    pub done: bool,
    pub error: Option<String>,
    pub publication: Option<CachePublication>,
}

#[derive(Debug, Default)]
pub struct Progress {
    state: Mutex<Fill>,
    #[cfg(test)]
    cv: Condvar,
    readable_event: Arc<super::interrupt::Event>,
    finished: tokio::sync::Notify,
    changed: tokio::sync::Notify,
}

impl Progress {
    pub fn new() -> Arc<Progress> {
        Arc::new(Progress::default())
    }

    pub fn snapshot(&self) -> Fill {
        self.state.lock().unwrap().clone()
    }

    pub fn set_total(&self, total: Option<u64>) {
        self.state.lock().unwrap().total = total;
        #[cfg(test)]
        self.cv.notify_all();
        self.readable_event.notify();
        self.changed.notify_waiters();
    }

    /// Synchronous fixture coordination; production schedulers await finished.
    #[cfg(test)]
    pub fn wait_finished_for(&self, limit: std::time::Duration) -> bool {
        let state = self.state.lock().unwrap();
        self.cv.wait_timeout_while(state, limit, |s| !s.done).unwrap().0.done
    }

    /// Async coordinators wait for completion without occupying a worker.
    /// Register before inspecting state so a concurrent terminal cannot be lost.
    pub async fn finished(&self) {
        loop {
            let finished = self.finished.notified();
            tokio::pin!(finished);
            finished.as_mut().enable();
            if self.snapshot().done { return; }
            finished.await;
        }
    }

    pub fn wait_complete(&self, blocking: &crate::rt::Blocking, interrupt: &Interrupt) -> io::Result<()> {
        super::callback::with(blocking, || self.wait_complete_owned(blocking, interrupt))
    }
    fn wait_complete_owned(&self, _blocking: &crate::rt::Blocking, interrupt: &Interrupt) -> io::Result<()> {
        reader_context()?;
        let mut wait = super::interrupt::Waiter::new(interrupt, &self.readable_event);
        loop {
            let s = self.state.lock().unwrap();
            if interrupt.load(Ordering::Relaxed) { return Err(cut()); }
            if let Some(e) = &s.error { return Err(io::Error::other(e.clone())); }
            if s.done { return Ok(()); }
            drop(s);
            wait_context()?;
            crate::health::pulse();
            wait.wait(super::interrupt::health_interval())?;
        }
    }

    pub fn advance(&self, written: u64) {
        self.state.lock().unwrap().written = written;
        #[cfg(test)]
        self.cv.notify_all();
        self.readable_event.notify();
        self.changed.notify_waiters();
    }

    pub fn finish(&self) {
        self.finish_with_publication(None);
    }

    pub fn finish_cached(&self, publication: CachePublication) {
        self.finish_with_publication(Some(publication));
    }

    fn finish_with_publication(&self, publication: Option<CachePublication>) {
        let mut s = self.state.lock().unwrap();
        s.done = true;
        s.total = Some(s.written);
        s.publication = publication;
        #[cfg(test)]
        self.cv.notify_all();
        self.readable_event.notify();
        self.finished.notify_waiters();
        self.changed.notify_waiters();
    }

    pub fn fail(&self, e: String) {
        let mut s = self.state.lock().unwrap();
        s.error = Some(e);
        s.done = true;
        #[cfg(test)]
        self.cv.notify_all();
        self.readable_event.notify();
        self.finished.notify_waiters();
        self.changed.notify_waiters();
    }

    /// Progressive async producers await actual published bytes, never occupy
    /// a blocking worker while their transfer needs the I/O runtime.
    async fn readable(&self, pos: u64) -> io::Result<bool> {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let state = self.state.lock().unwrap();
                if state.written > pos {
                    return Ok(true);
                }
                if let Some(error) = &state.error {
                    return Err(io::Error::other(error.clone()));
                }
                if state.done {
                    return Ok(false);
                }
            }
            changed.await;
        }
    }

    /// Waits until byte `pos` is written (true) or the file ends before it
    /// (false). Gives up when `interrupt` is set.
    fn wait_for(&self, pos: u64, interrupt: &Interrupt) -> io::Result<bool> {
        reader_context()?;
        let mut wait = super::interrupt::Waiter::new(interrupt, &self.readable_event);
        loop {
            let s = self.state.lock().unwrap();
            if s.written > pos { return Ok(true); }
            if let Some(e) = &s.error { return Err(io::Error::other(e.clone())); }
            if s.done { return Ok(false); }
            if interrupt.load(Ordering::Relaxed) { return Err(cut()); }
            drop(s);
            wait_context()?;
            crate::health::pulse();
            wait.wait(super::interrupt::health_interval())?;
        }
    }

    pub(super) fn wait_total(&self, interrupt: &Interrupt) -> io::Result<u64> {
        reader_context()?;
        let mut wait = super::interrupt::Waiter::new(interrupt, &self.readable_event);
        loop {
            let s = self.state.lock().unwrap();
            if let Some(t) = s.total { return Ok(t); }
            if s.done { return Ok(s.written); }
            if interrupt.load(Ordering::Relaxed) { return Err(cut()); }
            drop(s);
            wait_context()?;
            crate::health::pulse();
            wait.wait(super::interrupt::health_interval())?;
        }
    }

}

/// Reads a file being filled by a download.
pub struct ProgressiveReader {
    file: File,
    pos: u64,
    progress: Arc<Progress>,
    interrupt: Arc<Interrupt>,
    ranges: Option<Mutex<Ranges>>,
    seeked: bool,
}

const RANGE_BYTES: usize = 256 * 1024;
type RangeFetch = Arc<dyn Fn(u64, usize) -> std::pin::Pin<Box<dyn std::future::Future<Output=RangeResult> + Send>> + Send + Sync>;
#[cfg(test)]
type SyncRangeFetch = Arc<dyn Fn(u64, usize) -> RangeResult + Send + Sync>;
#[cfg(test)]
fn generated_ranges(fetch: SyncRangeFetch) -> RangeFetch {
    Arc::new(move |offset,length| {
        let fetch=fetch.clone();
        Box::pin(async move {crate::rt::spawn_blocking(move |_|fetch(offset,length)).await
            .map_err(|_|io::Error::other("generated range worker stopped"))?})
    })
}
struct RangeTask(tokio::task::JoinHandle<()>);
impl Drop for RangeTask {
    fn drop(&mut self) {self.0.abort();}
}
type RangeResult = io::Result<Option<Vec<u8>>>;

struct RangeReply {
    sender: Option<mpsc::SyncSender<RangeResult>>,
    ready: Arc<super::interrupt::Event>,
}
impl Drop for RangeReply {
    fn drop(&mut self) {
        // Publish channel closure before waking, including panic or an unpolled
        // task abort. Otherwise a reader could see Empty then sleep forever.
        drop(self.sender.take());
        self.ready.notify();
    }
}


/// One cancellable async request at a time. The decoder side stays synchronous
/// and interruptible; it never owns a dedicated socket-waiting thread.
struct Ranges {
    fetch: RangeFetch,
    start: u64,
    bytes: Vec<u8>,
    unsupported: bool,
}

impl Ranges {
    fn read(&mut self, pos: u64, buf: &mut [u8], interrupt: &Interrupt) -> io::Result<Option<usize>> {
        reader_context()?;
        if self.unsupported { return Ok(None); }
        if pos < self.start || pos - self.start >= self.bytes.len() as u64 {
            self.bytes.clear();
            wait_context()?;
            let (tx,rx)=mpsc::sync_channel(1);
            let ready = Arc::new(super::interrupt::Event::default());
            let mut wait = super::interrupt::Waiter::new(interrupt, &ready);
            let reply = RangeReply { sender: Some(tx), ready: ready.clone() };
            let fetch=self.fetch.clone();
            let _task=RangeTask(crate::rt::io().spawn(async move {
                let result=fetch(pos,RANGE_BYTES).await;
                // Exactly one result into an empty one-slot mailbox. The guard
                // closes the sender and publishes the event on every exit path.
                let _=reply.sender.as_ref().unwrap().try_send(result);
                drop(reply);
            }));
            let bytes = loop {
                if interrupt.load(Ordering::Relaxed) { return Err(cut()); }
                crate::health::pulse();
                match rx.try_recv() {
                    Ok(result) => break result?,
                    Err(TryRecvError::Empty) => wait.wait(super::interrupt::health_interval())?,
                    Err(TryRecvError::Disconnected) => return Err(io::Error::other("seek worker stopped")),
                }
            };
            match bytes {
                Some(bytes) if !bytes.is_empty() && bytes.len() <= RANGE_BYTES => self.bytes = bytes,
                Some(_) => return Err(io::Error::other("invalid seek block")),
                None => { self.unsupported = true; return Ok(None); }
            }
            self.start = pos;
        }
        let offset = (pos - self.start) as usize;
        let n = buf.len().min(self.bytes.len() - offset);
        buf[..n].copy_from_slice(&self.bytes[offset..offset + n]);
        Ok(Some(n))
    }
}

impl ProgressiveReader {
    pub fn open(_blocking: &crate::rt::Blocking, path: &Path, progress: Arc<Progress>, interrupt: Arc<Interrupt>) -> io::Result<ProgressiveReader> {
        Ok(ProgressiveReader { file: File::open(path)?, pos: 0, progress, interrupt, ranges: None, seeked: false })
    }

    pub fn with_async_ranges(mut self, fetch: RangeFetch) -> Self {
        self.ranges = Some(Mutex::new(Ranges { fetch, start: 0, bytes: Vec::new(), unsupported: false }));
        self
    }
    #[cfg(test)]
    pub fn with_ranges(self, fetch: SyncRangeFetch) -> Self {
        self.with_async_ranges(generated_ranges(fetch))
    }
}

/// A transferable read-ahead file whose final descriptor close belongs to a
/// finite/ordinary owner, even when the last async input or prepared reply is
/// abandoned. Clone retains the same open descriptor, never a new card handle.
#[derive(Clone)]
pub(super) struct ReadAheadFile(Arc<ReadAheadFileOwner>);
struct ReadAheadFileOwner {
    file: Option<File>,
    #[cfg(test)]
    retired: Option<mpsc::Sender<(bool, bool)>>,
}
impl ReadAheadFile {
    pub(super) fn new(file: File) -> Self {
        Self(Arc::new(ReadAheadFileOwner { file: Some(file), #[cfg(test)] retired: None }))
    }
    pub(super) fn read_at(&self, _blocking: &crate::rt::Blocking, out: &mut [u8], at: u64) -> io::Result<usize> {
        self.0.file.as_ref().expect("live read-ahead file").read_at(out, at)
    }
    #[cfg(test)]
    pub(super) fn observe_close(&mut self, retired: mpsc::Sender<(bool, bool)>) {
        Arc::get_mut(&mut self.0).expect("observe before sharing file").retired = Some(retired);
    }
}
impl Drop for ReadAheadFileOwner {
    fn drop(&mut self) {
        let file = self.file.take();
        #[cfg(test)]
        let file = ObservedReadAheadClose { file, retired: self.retired.take() };
        crate::rt::drop_off_executor(file);
    }
}
#[cfg(test)]
struct ObservedReadAheadClose { file: Option<File>, retired: Option<mpsc::Sender<(bool, bool)>> }
#[cfg(test)]
impl Drop for ObservedReadAheadClose {
    fn drop(&mut self) {
        drop(self.file.take());
        if let Some(retired) = self.retired.take() {
            let _ = retired.send((crate::rt::on_executor(), crate::rt::on_pool()));
        }
    }
}

/// Typed preparation for the music read-ahead controller. Only the returned
/// file operation may enter a card lane; range/progress waits stay async.
pub(super) enum PreparedRead {
    Bytes(usize),
    File {
        file: ReadAheadFile,
        length: usize,
        available: u64,
    },
}
pub(super) struct AsyncProgressive {
    file: ReadAheadFile,
    progress: Arc<Progress>,
    ranges: Option<Ranges>,
    seeked: bool,
    pos: u64,
    interrupt: Arc<Interrupt>,
}
impl ProgressiveReader {
    pub(super) fn into_async(self) -> AsyncProgressive {
        AsyncProgressive {
            file: ReadAheadFile::new(self.file),
            progress: self.progress,
            ranges: self.ranges.map(|ranges| ranges.into_inner().unwrap()),
            seeked: self.seeked,
            pos: self.pos,
            interrupt: self.interrupt,
        }
    }
}
impl AsyncProgressive {
    #[cfg(test)]
    pub(super) fn observe_file_close(&mut self, retired: mpsc::Sender<(bool, bool)>) {
        self.file.observe_close(retired);
    }
    pub(super) fn reposition(&mut self, at: u64) {
        if self.pos != at {
            self.seeked = true;
        }
        self.pos = at;
    }
    pub(super) fn completed(&mut self, at: u64, count: usize) {
        self.pos = at + count as u64;
    }
    pub(super) async fn prepare_read(&mut self, pos: u64, out: &mut [u8]) -> io::Result<PreparedRead> {
        if self.interrupt.load(Ordering::Relaxed) {
            return Err(cut());
        }
        if self.seeked {
            let fill = self.progress.snapshot();
            if fill.total.is_some_and(|total| pos >= total) {
                return Ok(PreparedRead::Bytes(0));
            }
            if pos >= fill.written {
                if let Some(ranges) = self.ranges.as_mut().filter(|ranges| !ranges.unsupported) {
                    if pos < ranges.start || pos - ranges.start >= ranges.bytes.len() as u64 {
                        ranges.bytes.clear();
                        match (ranges.fetch)(pos, RANGE_BYTES).await? {
                            Some(bytes) if !bytes.is_empty() && bytes.len() <= RANGE_BYTES => {
                                ranges.bytes = bytes;
                                ranges.start = pos;
                            }
                            Some(_) => return Err(io::Error::other("invalid seek block")),
                            None => ranges.unsupported = true,
                        }
                    }
                    if !ranges.unsupported {
                        let at = (pos - ranges.start) as usize;
                        let count = out.len().min(ranges.bytes.len() - at);
                        out[..count].copy_from_slice(&ranges.bytes[at..at + count]);
                        return Ok(PreparedRead::Bytes(count));
                    }
                }
            }
        }
        let timer = crate::playtrace::fill_timer();
        let result = self.progress.readable(pos).await;
        crate::playtrace::note_fill_wait(timer);
        if !result? {
            return Ok(PreparedRead::Bytes(0));
        }
        let available = self.progress.snapshot().written - pos;
        Ok(PreparedRead::File {
            file: self.file.clone(),
            length: (out.len() as u64).min(available) as usize,
            available,
        })
    }
}

impl Read for ProgressiveReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        reader_context()?;
        if self.interrupt.load(Ordering::Relaxed) { return Err(cut()); }
        if !buf.is_empty() && self.seeked {
            let fill = self.progress.snapshot();
            if fill.total.is_some_and(|total| self.pos >= total) { return Ok(0); }
            if self.pos >= fill.written {
                if let Some(ranges) = &mut self.ranges {
                    if let Some(n) = ranges.get_mut().unwrap().read(self.pos, buf, &self.interrupt)? {
                        self.pos += n as u64;
                        return Ok(n);
                    }
                }
            }
        }
        if buf.is_empty() { return Ok(0); }
        // The wait for download bytes is reported apart from the file read
        // (crate::playtrace): a network stall is not a card stall.
        let waiting = crate::playtrace::fill_timer();
        let ready = self.progress.wait_for(self.pos, &self.interrupt);
        crate::playtrace::note_fill_wait(waiting);
        if !ready? {
            return Ok(0);
        }
        let avail = self.progress.snapshot().written - self.pos;
        let n = (buf.len() as u64).min(avail) as usize;
        let n = self.file.read_at(&mut buf[..n], self.pos)?;
        crate::playtrace::note_ahead(avail - n as u64);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Seek for ProgressiveReader {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        reader_context()?;
        let pos = match to {
            SeekFrom::Start(p) => p as i64,
            SeekFrom::Current(d) => self.pos as i64 + d,
            SeekFrom::End(d) => self.progress.wait_total(&self.interrupt)? as i64 + d,
        };
        if pos < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before start"));
        }
        if self.pos != pos as u64 { self.seeked = true; }
        self.pos = pos as u64;
        Ok(self.pos)
    }
}

impl symphonia::core::io::MediaSource for ProgressiveReader {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        super::callback::assert_allowed();
        let f = self.progress.snapshot();
        f.total.or(if f.done { Some(f.written) } else { None })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn ready_file_reads_allow_finite_jobs_but_producer_waits_do_not() {
        let root = crate::library::generated_test_directory("reader-owner");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("generated.part");
        std::fs::write(&path, b"data").unwrap();
        let progress = Progress::new(); progress.set_total(Some(8)); progress.advance(4);
        let interrupt = Arc::new(Interrupt::new(false));
        let mut reader = ProgressiveReader::open(&crate::rt::Blocking::dedicated_thread(), &path, progress.clone(), interrupt.clone()).unwrap();
        let ui = crate::rt::Ui::new().unwrap();
        let error = ui.block_on(async { reader.read(&mut [0; 4]).unwrap_err() });
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(reader.pos, 0, "executor rejection cannot consume file data");
        let observed = progress.clone();
        let (reader, bytes) = ui.block_on(crate::rt::jobs().run_isolated(move |blocking| crate::audio::callback::with(blocking, || {
            let mut bytes = [0; 4];
            assert_eq!(reader.read(&mut bytes).unwrap(), 4, "finite card reads are valid pool work");
            assert_eq!(reader.seek(SeekFrom::End(0)).unwrap(), 8, "known length must not wait");
            reader.seek(SeekFrom::Start(4)).unwrap();
            assert_eq!(reader.read(&mut [0]).unwrap_err().kind(), io::ErrorKind::WouldBlock);
            assert_eq!(observed.wait_complete(blocking, &interrupt).unwrap_err().kind(), io::ErrorKind::WouldBlock);
            assert_eq!(reader.pos, 4, "refused wait cannot advance the reader");
            (reader, bytes)
        }))).unwrap();
        assert_eq!(&bytes, b"data");
        assert!(!progress.snapshot().done);
        progress.finish();
        progress.wait_complete(&crate::rt::Blocking::dedicated_thread(), &Interrupt::new(false)).unwrap();
        drop(reader); std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn async_completion_wakes_all_waiters_and_preserves_terminal_state() {
        crate::audio::callback::test_scope(|| {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        runtime.block_on(async {
            for failed in [false, true] {
                let progress = Progress::new();
                let mut waiters = Vec::new();
                for _ in 0..3 {
                    let observed = progress.clone();
                    waiters.push(tokio::spawn(async move { observed.finished().await; observed.snapshot() }));
                }
                tokio::task::yield_now().await;
                assert!(waiters.iter().all(|waiter| !waiter.is_finished()));
                progress.advance(99);
                if failed { progress.fail("generated failure".into()); }
                else { progress.finish_cached(CachePublication::Published); }
                for waiter in waiters {
                    let fill = tokio::time::timeout(Duration::from_secs(1), waiter).await.unwrap().unwrap();
                    assert!(fill.done);
                    assert_eq!(fill.written, 99);
                    assert_eq!(fill.error.is_some(), failed);
                    assert_eq!(fill.publication, if failed { None } else { Some(CachePublication::Published) });
                }
                // A terminal before registration must also complete immediately.
                tokio::time::timeout(Duration::from_secs(1), progress.finished()).await.unwrap();
            }
        });
            })
    }

    #[test]
    fn cancelling_an_async_completion_wait_does_not_finish_the_transfer() {
        crate::audio::callback::test_scope(|| {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        runtime.block_on(async {
            let progress = Progress::new();
            let observed = progress.clone();
            let waiter = tokio::spawn(async move { observed.finished().await; });
            tokio::task::yield_now().await;
            waiter.abort();
            assert!(waiter.await.unwrap_err().is_cancelled());
            assert!(!progress.snapshot().done);
            progress.finish();
            tokio::time::timeout(Duration::from_secs(1), progress.finished()).await.unwrap();
        });
            })
    }

    #[test]
    fn seek_reads_distant_ranges_without_waiting_for_sequential_download() {
        crate::audio::callback::test_scope(|| {
        let root = crate::library::generated_test_directory("ranges");
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("partial");
        std::fs::write(&path, b"head").unwrap();
        let progress = Progress::new();
        progress.advance(4);
        progress.set_total(Some(2_000_000));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let seen = calls.clone();
        let mut reader = ProgressiveReader::open(&crate::rt::Blocking::dedicated_thread(), &path, progress.clone(), Arc::new(Interrupt::new(false))).unwrap()
            .with_ranges(Arc::new(move |pos, size| {
                seen.lock().unwrap().push(pos);
                Ok(Some((0..size).map(|i| ((pos + i as u64) % 251) as u8).collect()))
            }));
        reader.seek(SeekFrom::Start(1_000_000)).unwrap();
        let mut bytes = [0; 100];
        reader.read_exact(&mut bytes).unwrap();
        assert_eq!(bytes[0], (1_000_000 % 251) as u8);
        reader.seek(SeekFrom::Start(1_000_010)).unwrap();
        reader.read_exact(&mut bytes).unwrap();
        assert_eq!(bytes[0], (1_000_010 % 251) as u8);
        assert_eq!(*calls.lock().unwrap(), vec![1_000_000], "reuse the block for nearby seeks");
        reader.seek(SeekFrom::Start(0)).unwrap();
        let mut head = [0; 4];
        reader.read_exact(&mut head).unwrap();
        assert_eq!(&head, b"head");
        assert_eq!(progress.snapshot().written, 4, "range data must not publish holes as a complete cache");
        reader.seek(SeekFrom::Start(2_000_000)).unwrap();
        assert_eq!(reader.read(&mut bytes).unwrap(), 0);
            })
    }

    #[test]
    fn stalled_range_and_ignored_range_remain_interruptible() {
        crate::audio::callback::test_scope(|| {
        for ignored in [false, true] {
            let root = crate::library::generated_test_directory("range-cut");
            std::fs::create_dir_all(&root).unwrap();
            let path = root.join("partial");
            std::fs::write(&path, b"head").unwrap();
            let progress = Progress::new(); progress.advance(4);
            let interrupt = Arc::new(Interrupt::new(false));
            let (entered_tx, entered) = mpsc::channel();
            let mut reader = ProgressiveReader::open(&crate::rt::Blocking::dedicated_thread(), &path, progress, interrupt.clone()).unwrap()
                .with_ranges(Arc::new(move |_, _| {
                    entered_tx.send(()).unwrap();
                    if !ignored { std::thread::sleep(Duration::from_millis(500)); }
                    Ok(None)
                }));
            reader.seek(SeekFrom::Start(1_000_000)).unwrap();
            let worker = std::thread::spawn(move || crate::audio::callback::test_scope(|| reader.read(&mut [0; 16])));
            entered.recv_timeout(Duration::from_secs(1)).unwrap();
            let start = std::time::Instant::now();
            interrupt.store(true, Ordering::Relaxed);
            assert!(is_cut(&worker.join().unwrap().unwrap_err()));
            assert!(start.elapsed() < Duration::from_millis(300));
        }
            })
    }

    #[test]
    fn reads_as_the_file_fills() {
        crate::audio::callback::test_scope(|| {
        let dir = crate::library::generated_test_directory("progressive-reader");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("song.part");
        let mut w = File::create(&path).unwrap();
        let progress = Progress::new();
        let interrupt = Arc::new(Interrupt::new(false));
        let mut r = ProgressiveReader::open(&crate::rt::Blocking::dedicated_thread(), &path, progress.clone(), interrupt.clone()).unwrap();

        let p2 = progress.clone();
        let writer = std::thread::spawn(move || {
            for chunk in [b"hello ".as_slice(), b"world"] {
                std::thread::sleep(Duration::from_millis(30));
                w.write_all(chunk).unwrap();
                let len = w.metadata().unwrap().len();
                p2.advance(len);
            }
            p2.finish();
        });
        let mut out = String::new();
        r.read_to_string(&mut out).unwrap();
        writer.join().unwrap();
        assert_eq!(out, "hello world");
        assert_eq!(r.seek(SeekFrom::End(-5)).unwrap(), 6);
        let mut tail = String::new();
        r.read_to_string(&mut tail).unwrap();
        assert_eq!(tail, "world");
        std::fs::remove_dir_all(&dir).unwrap();
            })
    }

    #[test]
    fn interrupt_unblocks_and_errors_surface() {
        crate::audio::callback::test_scope(|| {
        let dir = crate::library::generated_test_directory("progressive-reader-interrupt");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("song.part");
        File::create(&path).unwrap();
        let progress = Progress::new();
        let interrupt = Arc::new(Interrupt::new(true));
        let mut r = ProgressiveReader::open(&crate::rt::Blocking::dedicated_thread(), &path, progress.clone(), interrupt).unwrap();
        let mut b = [0u8; 4];
        assert!(is_cut(&r.read(&mut b).unwrap_err()));

        let mut r = ProgressiveReader::open(&crate::rt::Blocking::dedicated_thread(), &path, progress.clone(), Arc::new(Interrupt::new(false))).unwrap();
        progress.fail("HTTP 500".into());
        assert!(r.read(&mut b).unwrap_err().to_string().contains("500"));
        std::fs::remove_dir_all(&dir).unwrap();
            })
    }
}

mod audiobook_ranges;
pub use audiobook_ranges::{RangeReader,spoken_window_bytes};
