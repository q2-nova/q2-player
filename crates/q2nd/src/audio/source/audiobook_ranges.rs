//! Bounded sequential read-ahead, fetched by one async owner in small chunks. Seeking
//! replaces queued work; a late response can never become the new seek's data.
use crate::audio::interrupt::Interrupt;
use super::{RangeFetch, RangeResult, RANGE_BYTES, cut};
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex, atomic::Ordering};
#[cfg(test)]
use std::time::Duration;
use std::collections::VecDeque;

#[derive(Clone, Copy)]
struct Request { id: u64, offset: u64, length: usize }
#[derive(Default)]
struct State {
    request: Option<Request>,
    results: VecDeque<(u64,u64,RangeResult)>,
    next:Option<Request>,
    generation:u64,
    closed: bool,
}
struct Shared {
    state: Mutex<State>,
    ready: Arc<crate::audio::interrupt::Event>,
    changed: tokio::sync::Notify,
}
struct Worker { shared: Arc<Shared>, task: tokio::task::JoinHandle<()> }
struct WorkerExit(Arc<Shared>);
impl Drop for WorkerExit {
    fn drop(&mut self) {
        self.0.state.lock().unwrap().closed = true;
        self.0.ready.notify();
    }
}
enum Next { Request(Request), Wait, Stop }
impl Shared {
    fn take(&self, depth:usize)->Next {
        let mut state=self.state.lock().unwrap();
        if state.closed {return Next::Stop;}
        if let Some(request)=state.request.take() {state.next=Some(request);}
        if state.results.len()<depth {
            if let Some(request)=state.next.take() {return Next::Request(request);}
        }
        Next::Wait
    }
    fn current(&self,id:u64)->bool {
        let state=self.state.lock().unwrap();
        !state.closed && state.generation==id
    }
    fn publish(&self,request:Request,result:RangeResult,total:u64) {
        let mut state=self.state.lock().unwrap();
        if state.closed || state.generation!=request.id {return;}
        let next=request.offset+request.length as u64;
        if result.as_ref().is_ok_and(|b|b.is_some()) && next<total {
            state.next=Some(Request{id:request.id,offset:next,length:(total-next).min(RANGE_BYTES as u64) as usize});
        }
        state.results.push_back((request.id,request.offset,result));
        self.ready.notify();
    }
}
/// How long a failing range request keeps retrying before playback reports it.
#[cfg(not(test))]
const RETRY_FOR: std::time::Duration = std::time::Duration::from_secs(60);
#[cfg(test)]
const RETRY_FOR: std::time::Duration = std::time::Duration::from_millis(300);
fn retry_delay(attempt: u32) -> std::time::Duration {
    #[cfg(test)]
    { let _ = attempt; std::time::Duration::from_millis(20) }
    #[cfg(not(test))]
    { std::time::Duration::from_millis(500u64 << attempt.min(4)) }
}
/// Network trouble and a busy or failing server are worth retrying; a changed
/// account or a malformed answer is not.
fn retryable(error: &io::Error) -> bool {
    if super::is_cut(error) { return false; }
    if matches!(error.kind(), io::ErrorKind::TimedOut | io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::ConnectionRefused | io::ErrorKind::NotConnected | io::ErrorKind::BrokenPipe
        | io::ErrorKind::NetworkDown | io::ErrorKind::NetworkUnreachable | io::ErrorKind::HostUnreachable) { return true; }
    let text = error.to_string().to_ascii_lowercase();
    !text.contains("changed") && !text.contains("panic") && !text.contains("stopped")
        && ["network", "unavailable", "timed out", "timeout", "connection", "range failed", "reset", "dns", "unreachable", "had a problem"]
            .iter().any(|word| text.contains(word))
}

impl Worker {
    fn new(fetch: RangeFetch,total:u64,depth:usize) -> Self {
        let shared = Arc::new(Shared {state:Mutex::new(State::default()),ready:Arc::default(),changed:tokio::sync::Notify::new()});
        // Construct before spawning: panic, abort and unpolled task retirement
        // all publish closure to the decoder's existing condition variable.
        let exit=WorkerExit(shared.clone());
        let task=crate::rt::io().spawn(async move {
            let _exit=exit;
            let shared=&_exit.0;
            loop {
                let request=match shared.take(depth) {
                    Next::Stop=>return,
                    Next::Wait=>{shared.changed.notified().await;continue;},
                    Next::Request(request)=>request,
                };
                // UX-006: a transient network failure is retried with backoff
                // for RETRY_FOR while the request is current; playback buffers
                // instead of stopping. Account/settings changes are final.
                let started=std::time::Instant::now();
                let mut attempt=0u32;
                let result='attempts: loop {
                    let future=fetch(request.offset,request.length);
                    tokio::pin!(future);
                    let result=loop {
                        tokio::select! {
                            result=&mut future=>break Some(result),
                            ()=shared.changed.notified()=>{
                                if !shared.current(request.id) {break None;}
                            }
                        }
                    };
                    match result {
                        Some(Err(error)) if retryable(&error) && started.elapsed()<RETRY_FOR => {
                            attempt+=1;
                            let code=crate::ux::Code::classify(&error.to_string()).unwrap_or(crate::ux::Code::Unknown) as u32;
                            crate::ux::record(crate::ux::Mark::Retry, code<<8 | attempt.min(255));
                            let pause=tokio::time::sleep(retry_delay(attempt));
                            tokio::pin!(pause);
                            loop {
                                tokio::select! {
                                    ()=&mut pause=>break,
                                    ()=shared.changed.notified()=>{
                                        if !shared.current(request.id) {break 'attempts None;}
                                    }
                                }
                            }
                        }
                        Some(Ok(bytes)) if attempt>0 => {
                            crate::ux::recovered(crate::ux::Code::StreamInterrupted, crate::ux::Journey::PlaySpoken);
                            break Some(Ok(bytes));
                        }
                        other=>break other,
                    }
                };
                // A newer seek/drop discards the actual HTTP future here;
                // it never waits for the obsolete transport's timeout.
                if let Some(result)=result {
                    let result=result.and_then(|bytes| {
                        if bytes.as_ref().is_some_and(|b|b.len()!=request.length) {
                            return Err(io::Error::other("Incomplete audiobook range"));
                        }
                        Ok(bytes)
                    });
                    shared.publish(request,result,total);
                }
            }
        });
        Self {shared,task}
    }
    fn request(&self, request: Request) {
        let retired = {
            let mut state = self.shared.state.lock().unwrap();
            // Keep the queue allocation, but never destroy payloads/errors
            // under the state lock shared with the I/O owner.
            let retired: Vec<_> = state.results.drain(..).collect();
            state.next=None;state.generation=request.id;
            state.request = Some(request);
            retired
        };
        self.shared.changed.notify_one();
        drop(retired);
    }
    fn receive(&self, id: u64, offset:u64, interrupt: &Interrupt) -> RangeResult {
        let mut wait = crate::audio::interrupt::Waiter::new(interrupt, &self.shared.ready);
        loop {
            let mut state = self.shared.state.lock().unwrap();
            if interrupt.load(Ordering::Relaxed) { return Err(cut()); }
            if let Some((received,at,result))=state.results.pop_front() {
                self.shared.changed.notify_one();
                if received==id {
                    if at!=offset {return Err(io::Error::other("Unexpected audiobook range offset"));}
                    return result;
                }
                // More retained results may already belong to this generation.
                continue;
            }
            if state.closed { return Err(io::Error::other("Audiobook range worker stopped")); }
            drop(state);
            super::wait_context()?;
            crate::health::pulse();
            wait.wait(crate::audio::interrupt::health_interval())?;
        }
    }

}
impl Drop for Worker {
    fn drop(&mut self) {
        let retired = {
            let mut state = self.shared.state.lock().unwrap();
            state.closed = true;
            state.request = None;
            let retired: Vec<_> = state.results.drain(..).collect();
            state.next=None;
            retired
        };
        self.shared.ready.notify();
        self.shared.changed.notify_one();
        self.task.abort();
        drop(retired);
    }
}

const MAX_WINDOW_BYTES:usize=4*1024*1024;
/// Aim for five wall seconds per window even at the supported 3x speed.
/// Duration/size estimate the encoded rate, not guaranteed VBR peak coverage.
/// Prefetch is bounded to 4 MiB, plus current/in-flight 256 KiB chunks; unknown duration keeps the old minimum.
pub fn spoken_window_bytes(total:u64,duration_seconds:u32)->usize {
    if duration_seconds==0 {return RANGE_BYTES;}
    let wanted=(u128::from(total)*15).div_ceil(u128::from(duration_seconds));
    let aligned=wanted.div_ceil(64*1024)*(64*1024);
    aligned.clamp(RANGE_BYTES as u128,MAX_WINDOW_BYTES as u128) as usize
}

pub struct RangeReader {
    pos: u64,
    total: u64,
    start: u64,
    bytes: Vec<u8>,
    sequence: u64,
    pending: Option<Request>,
    worker: Option<Worker>,
    fetch: RangeFetch,
    interrupt: Arc<Interrupt>,
    window_bytes:usize,
}
impl RangeReader {
    pub fn new_async(total: u64, interrupt: Arc<Interrupt>, fetch: RangeFetch) -> Self {
        Self::with_async_window(total,interrupt,fetch,RANGE_BYTES)
    }
    #[cfg(test)]
    pub fn new(total:u64,interrupt:Arc<Interrupt>,fetch:super::SyncRangeFetch)->Self {
        Self::new_async(total,interrupt,super::generated_ranges(fetch))
    }
    #[cfg(test)]
    pub fn with_window(total:u64,interrupt:Arc<Interrupt>,fetch:super::SyncRangeFetch,window_bytes:usize)->Self {
        Self::with_async_window(total,interrupt,super::generated_ranges(fetch),window_bytes)
    }
    pub fn with_async_window(total:u64,interrupt:Arc<Interrupt>,fetch:RangeFetch,window_bytes:usize)->Self {
        Self { pos:0,total,start:0,bytes:Vec::new(),sequence:0,pending:None,worker:None,fetch,interrupt,
            window_bytes:window_bytes.clamp(RANGE_BYTES,MAX_WINDOW_BYTES) }
    }
    fn request(&mut self, offset: u64) -> io::Result<()> {
        if offset >= self.total { return Ok(()); }
        if self.worker.is_none() { self.worker = Some(Worker::new(self.fetch.clone(),self.total,self.window_bytes.div_ceil(RANGE_BYTES))); }
        self.sequence = self.sequence.wrapping_add(1);
        let request = Request { id: self.sequence, offset,
            length: (self.total - offset).min(RANGE_BYTES as u64) as usize };
        self.worker.as_ref().unwrap().request(request);
        self.pending = Some(request);
        Ok(())
    }
}
impl Read for RangeReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        super::reader_context()?;
        if self.interrupt.load(Ordering::Relaxed) { return Err(cut()); }
        if buf.is_empty() || self.pos >= self.total { return Ok(0); }
        if self.pos < self.start || self.pos - self.start >= self.bytes.len() as u64 {
            self.bytes = Vec::new();
            if !self.pending.is_some_and(|r| self.pos >= r.offset && self.pos - r.offset < r.length as u64) {
                self.request(self.pos)?;
            }
            let request = self.pending.unwrap();
            // A wait for a range from the server, not a card read (crate::playtrace).
            let waiting = crate::playtrace::fill_timer();
            let result = self.worker.as_ref().unwrap().receive(request.id,request.offset, &self.interrupt);
            crate::playtrace::note_fill_wait(waiting);
            // A cut preserves a pending response for an eventual resume. All
            // completed responses (including errors) must permit a fresh retry.
            if result.as_ref().is_err_and(super::is_cut) { return Err(result.unwrap_err()); }
            self.pending = None;
            let bytes = result?.ok_or_else(|| io::Error::other("Server does not support audiobook ranges"))?;
            if bytes.len() != request.length {
                return Err(io::Error::other("Incomplete audiobook range"));
            }
            self.start = request.offset;
            self.bytes = bytes;
            let next=self.start+self.bytes.len() as u64;
            if next<self.total {self.pending=Some(Request{id:request.id,offset:next,length:(self.total-next).min(RANGE_BYTES as u64) as usize});}
        }
        let offset = (self.pos - self.start) as usize;
        let n = buf.len().min(self.bytes.len() - offset);
        buf[..n].copy_from_slice(&self.bytes[offset..offset + n]);
        self.pos += n as u64;
        Ok(n)
    }
}
impl Seek for RangeReader {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        super::reader_context()?;
        let pos = match to { SeekFrom::Start(p) => i128::from(p),
            SeekFrom::Current(d) => i128::from(self.pos) + i128::from(d),
            SeekFrom::End(d) => i128::from(self.total) + i128::from(d) };
        if pos < 0 || pos > i128::from(u64::MAX) { return Err(io::Error::other("invalid audiobook seek")); }
        self.pos = pos as u64;
        Ok(self.pos)
    }
}
impl symphonia::core::io::MediaSource for RangeReader {
    fn is_seekable(&self) -> bool { true }
    fn byte_len(&self) -> Option<u64> { Some(self.total) }
}

#[cfg(test)]
#[path = "range_async_tests.rs"]
mod async_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    fn bytes(offset: u64, length: usize) -> Vec<u8> {
        (0..length).map(|i| ((offset + i as u64) % 251) as u8).collect()
    }
    #[test]
    fn window_budget_targets_three_x_playback_and_bounds_untrusted_metadata() {
        crate::audio::callback::test_scope(|| {
        assert_eq!(spoken_window_bytes(0,0),RANGE_BYTES);
        assert_eq!(spoken_window_bytes(u64::MAX,1),MAX_WINDOW_BYTES);
        let mut rng=fastrand::Rng::with_seed(0x42554646);
        for _ in 0..10000 {
            let size=rng.u64(..);let seconds=rng.u32(1..);
            let n=spoken_window_bytes(size,seconds);
            assert!((RANGE_BYTES..=MAX_WINDOW_BYTES).contains(&n));
            if n<MAX_WINDOW_BYTES {assert!((n as u128)*u128::from(seconds)>=u128::from(size)*15);}
        }
        assert_eq!(spoken_window_bytes(100*1024*1024,3600),448*1024);
            })
    }
    #[test]
    fn adaptive_prefetch_uses_small_requests_and_stops_at_budget_while_paused() {
        crate::audio::callback::test_scope(|| {
        use std::sync::atomic::AtomicUsize;
        let count=Arc::new(AtomicUsize::new(0));let calls=count.clone();
        let mut reader=RangeReader::with_window((RANGE_BYTES*20) as u64,Arc::new(Interrupt::new(false)),Arc::new(move |o,n| {
            assert!(n<=RANGE_BYTES,"warmup never waits for a multi-megabyte request");
            calls.fetch_add(1,Ordering::Relaxed);Ok(Some(bytes(o,n)))
        }),RANGE_BYTES*3);
        let mut first=[0;1];reader.read_exact(&mut first).unwrap();
        let deadline=std::time::Instant::now()+Duration::from_secs(2);
        while count.load(Ordering::Relaxed)<4 && std::time::Instant::now()<deadline {std::thread::yield_now();}
        assert_eq!(count.load(Ordering::Relaxed),4);
        std::thread::sleep(Duration::from_millis(30));assert_eq!(count.load(Ordering::Relaxed),4);
        let mut buffered=vec![0;RANGE_BYTES*4-1];reader.read_exact(&mut buffered).unwrap();
        assert_eq!(buffered,bytes(1,buffered.len()));
            })
    }
    #[test]
    fn next_network_window_starts_before_current_audio_is_consumed() {
        crate::audio::callback::test_scope(|| {
        let (began, received) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let wait = Mutex::new(wait);
        let mut reader = RangeReader::new((RANGE_BYTES * 2) as u64, Arc::new(Interrupt::new(false)),
            Arc::new(move |offset, length| {
                if offset != 0 { began.send(offset).unwrap(); wait.lock().unwrap().recv().unwrap(); }
                Ok(Some(bytes(offset, length)))
            }));
        let mut first = [0; 16]; reader.read_exact(&mut first).unwrap();
        assert_eq!(received.recv_timeout(Duration::from_secs(2)).unwrap(), RANGE_BYTES as u64,
            "read-ahead begins with nearly an entire window still buffered");
        let mut rest = vec![0; RANGE_BYTES - first.len()]; reader.read_exact(&mut rest).unwrap();
        assert_eq!(rest, bytes(16, rest.len())); // Never waits for blocked future I/O.
        release.send(()).unwrap();
        let mut next = vec![0; RANGE_BYTES]; reader.read_exact(&mut next).unwrap();
        assert_eq!(next, bytes(RANGE_BYTES as u64, RANGE_BYTES));
        assert_eq!(reader.read(&mut first).unwrap(), 0);
            })
    }
    #[test]
    fn arbitrary_seek_read_sequences_match_the_original_file() {
        crate::audio::callback::test_scope(|| {
        let total = (RANGE_BYTES * 3 + 137) as u64;
        let mut reader = RangeReader::new(total, Arc::new(Interrupt::new(false)), Arc::new(|o,n| Ok(Some(bytes(o,n)))));
        let mut rng = fastrand::Rng::with_seed(0xB00C_2026);
        for _ in 0..1200 {
            let position = rng.u64(0..total+100);
            reader.seek(SeekFrom::Start(position)).unwrap();
            let mut buffer = vec![0; rng.usize(0..8192)];
            let count = reader.read(&mut buffer).unwrap();
            assert_eq!(buffer[..count], bytes(position, count));
            assert!(count as u64 <= total.saturating_sub(position));
        }
            })
    }
    #[test]
    fn failed_prefetch_does_not_damage_current_window_and_can_retry() {
        crate::audio::callback::test_scope(|| {
        let failures = Arc::new(std::sync::atomic::AtomicBool::new(true)); let fail = failures.clone();
        let mut reader = RangeReader::new((RANGE_BYTES * 2) as u64, Arc::new(Interrupt::new(false)), Arc::new(move |o,n| {
            if o != 0 && fail.swap(false, Ordering::Relaxed) { return Err(io::Error::other("generated network failure")); }
            Ok(Some(bytes(o,n)))
        }));
        let mut buffer = vec![0; RANGE_BYTES]; reader.read_exact(&mut buffer).unwrap();
        assert_eq!(buffer, bytes(0,RANGE_BYTES));
        // UX-006: the transient failure is retried; the read succeeds.
        reader.read_exact(&mut buffer).unwrap();
        assert_eq!(buffer, bytes(RANGE_BYTES as u64,RANGE_BYTES));
            })
    }
    #[test]
    fn persistent_network_failure_is_reported_after_the_retry_window() {
        crate::audio::callback::test_scope(|| {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0)); let counted = calls.clone();
        let mut reader = RangeReader::new(128, Arc::new(Interrupt::new(false)), Arc::new(move |_,_| {
            counted.fetch_add(1, Ordering::Relaxed);
            Err(io::Error::new(io::ErrorKind::TimedOut, "generated timeout"))
        }));
        let started = std::time::Instant::now();
        assert!(reader.read(&mut [0;1]).is_err());
        assert!(started.elapsed() >= RETRY_FOR, "{:?}", started.elapsed());
        assert!(calls.load(Ordering::Relaxed) > 1, "retried");
        let mut changed = RangeReader::new(128, Arc::new(Interrupt::new(false)), Arc::new(|_,_| Err(io::Error::other("Audiobookshelf settings changed; select the item again"))));
        let started = std::time::Instant::now();
        assert!(changed.read(&mut [0;1]).is_err());
        assert!(started.elapsed() < RETRY_FOR, "a changed account is not retried");
            })
    }
    #[test]
    #[cfg_attr(panic = "abort", ignore = "recovers from a panic; a panic = \"abort\" build, as on the Q2, aborts")]
    fn worker_panic_and_short_response_report_errors_instead_of_hanging() {
        crate::audio::callback::test_scope(|| {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || { crate::audio::callback::test_scope(|| {
            let mut reader = RangeReader::new(128, Arc::new(Interrupt::new(false)), Arc::new(|_,_| panic!("generated transport panic")));
            tx.send(reader.read(&mut [0;1]).unwrap_err().to_string()).unwrap();
         }) });
        assert!(rx.recv_timeout(Duration::from_secs(2)).unwrap().contains("worker stopped"));
        let mut reader = RangeReader::new(128, Arc::new(Interrupt::new(false)), Arc::new(|_,_| Ok(Some(vec![0;1]))));
        assert!(reader.read(&mut [0;1]).unwrap_err().to_string().contains("Incomplete"));
            })
    }

    #[test]
    fn interrupted_wait_keeps_one_worker_and_late_seek_results_are_rejected() {
        crate::audio::callback::test_scope(|| {
        let (started, started_rx) = mpsc::channel(); let (release, release_rx) = mpsc::channel();
        let release_rx = Mutex::new(release_rx); let interrupt = Arc::new(Interrupt::new(false));
        let flag = interrupt.clone();
        let mut reader = RangeReader::new(1_000_000, interrupt, Arc::new(move |o,n| {
            if o == 0 { started.send(()).unwrap(); release_rx.lock().unwrap().recv().unwrap(); }
            Ok(Some(bytes(o,n)))
        }));
        reader.request(0).unwrap(); started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        flag.store(true,Ordering::Relaxed);
        assert!(super::super::is_cut(&reader.read(&mut [0;1]).unwrap_err()));
        reader.seek(SeekFrom::Start(700_000)).unwrap();
        flag.store(false,Ordering::Relaxed); release.send(()).unwrap();
        let mut buffer = [0;512]; reader.read_exact(&mut buffer).unwrap();
        assert_eq!(buffer.as_slice(),bytes(700_000,512));
            })
    }
}
