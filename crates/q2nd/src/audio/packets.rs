#![forbid(unsafe_code)]
//! A bounded demux owner for spoken streams. Pausing interrupts the audio
//! owner's wait, never the demuxer's partially read packet. Seek/drop explicitly
//! supersede work. One request is outstanding at a time: a fill reads up to
//! `FILL_PACKETS` packets ahead into its single reply, which the audio owner
//! takes as packets arrive, so the demuxer wakes once per fill instead of once
//! per packet (about 40 packets/s for spoken MP3 or AAC).
use crate::audio::interrupt::Interrupt;
use std::collections::VecDeque;
use std::sync::{
    atomic::Ordering,
    Arc, Condvar, Mutex,
};
#[cfg(test)]
use std::time::Duration;
use symphonia::core::{
    errors::{Error, Result},
    formats::{FormatReader, Packet, SeekMode, SeekTo, SeekedTo},
};

/// Packets one fill reads ahead: about 0.5–1.5 s of spoken audio.
const FILL_PACKETS: usize = 32;
/// The audio owner asks for the next fill when fewer packets than this remain.
const REFILL_BELOW: usize = 8;
/// A fill also ends once its packets hold this many bytes.
const FILL_BYTES: usize = 256 * 1024;

enum Request {
    /// Read up to this many packets.
    Next(usize),
    Seek(SeekMode, SeekTo),
}
enum Response {
    /// Packets of a fill in read order; true once the fill has ended (its
    /// count, its byte bound or an error, which is always its last entry).
    Packets(VecDeque<Result<Packet>>, bool),
    Seek(Result<SeekedTo>),
}
#[derive(Default)]
struct State {
    serial: u64,
    request: Option<(u64, Request)>,
    result: Option<(u64, Response)>,
    closed: bool,
}
type Shared = Arc<(Mutex<State>, Condvar, Arc<super::interrupt::Event>)>;
struct Exit(Shared);
impl Drop for Exit {
    fn drop(&mut self) {
        let (lock, cv, ready) = &*self.0;
        lock.lock().unwrap().closed = true;
        cv.notify_all();
        ready.notify();
    }
}
pub struct Background {
    shared: Shared,
    io_cancel: Arc<Interrupt>,
    /// The outstanding fill, until its last packet has been taken.
    pending: Option<u64>,
    /// Packets taken from fills and not yet returned.
    queue: VecDeque<Result<Packet>>,
}
impl Background {
    fn new(format: Box<dyn FormatReader>, io_cancel: Arc<Interrupt>) -> std::io::Result<Self> {
        let shared = Arc::new((Mutex::new(State::default()), Condvar::new(), Arc::default()));
        let task = shared.clone();
        let cancel = io_cancel.clone();
        std::thread::Builder::new()
            .name("spoken-demux".into())
            .spawn(move || {
                let blocking = crate::rt::Blocking::dedicated_thread();
                Self::serve(&blocking, format, task, cancel);
            })?;
        Ok(Self {
            shared,
            io_cancel,
            pending: None,
            queue: VecDeque::new(),
        })
    }
    fn serve(blocking: &crate::rt::Blocking, mut format: Box<dyn FormatReader>, task: Shared, cancel: Arc<Interrupt>) {
        let _exit = Exit(task.clone());
        let (lock, cv, ready) = &*task;
        loop {
            let (id, request) = {
                let mut state = lock.lock().unwrap();
                while state.request.is_none() && !state.closed {
                    state = cv.wait(state).unwrap();
                }
                if state.closed {
                    return;
                }
                cancel.store(false, Ordering::Relaxed);
                state.request.take().unwrap()
            };
            match request {
                Request::Next(count) => {
                    if !Self::fill(blocking, format.as_mut(), &task, id, count) {
                        return;
                    }
                }
                Request::Seek(mode, to) => {
                    let response = Response::Seek(super::callback::with(blocking, || format.seek(mode, to)));
                    let mut state = lock.lock().unwrap();
                    if state.closed {
                        return;
                    }
                    if state.serial == id {
                        state.result = Some((id, response));
                    }
                    cv.notify_all();
                    ready.notify();
                }
            }
        }
    }
    /// Reads up to `count` packets into request `id`'s reply. A newer request
    /// supersedes the fill and the packet in hand is dropped. False once closed.
    fn fill(blocking: &crate::rt::Blocking, format: &mut dyn FormatReader, task: &Shared, id: u64, count: usize) -> bool {
        let (lock, _, ready) = &**task;
        let mut bytes = 0;
        for n in 1..=count.max(1) {
            let packet = super::callback::with(blocking, || format.next_packet());
            let mut state = lock.lock().unwrap();
            if state.closed {
                return false;
            }
            if state.serial != id {
                return true;
            }
            let last = match &packet {
                Ok(packet) => {
                    bytes += packet.data.len();
                    bytes >= FILL_BYTES || n >= count
                }
                Err(_) => true,
            };
            match &mut state.result {
                // The owner has not taken this reply yet, so it is not
                // waiting for it: no notification.
                Some((reply, Response::Packets(packets, done))) if *reply == id => {
                    packets.push_back(packet);
                    *done = last;
                }
                _ => {
                    state.result = Some((id, Response::Packets(VecDeque::from([packet]), last)));
                    ready.notify();
                }
            }
            if last {
                break;
            }
        }
        true
    }

    fn request(&mut self, request: Request) -> u64 {
        let (lock, cv, ready) = &*self.shared;
        let mut state = lock.lock().unwrap();
        if matches!(request, Request::Seek(..)) {
            self.io_cancel.store(true, Ordering::Relaxed);
        }
        state.serial = state.serial.wrapping_add(1);
        let id = state.serial;
        state.result = None;
        state.request = Some((id, request));
        cv.notify_all();
        ready.notify();
        id
    }
    fn receive(&self, blocking: &crate::rt::Blocking, id: u64, interrupt: &Interrupt) -> Result<Response> {
        let (lock, _, ready) = &*self.shared;
        let mut wait = super::interrupt::Waiter::new(interrupt, ready);
        loop {
            let mut state = lock.lock().unwrap();
            if interrupt.load(Ordering::Relaxed) {
                return Err(Error::IoError(std::io::Error::other(super::source::Cut)));
            }
            if state.result.as_ref().is_some_and(|(received, _)| *received == id) {
                return Ok(state.result.take().unwrap().1);
            }
            if state.closed {
                return Err(Error::IoError(std::io::Error::other("Spoken decoder worker stopped")));
            }
            drop(state);
            super::callback::with(blocking, || {
                super::source::wait_context()?;
                crate::health::pulse();
                wait.wait(super::interrupt::health_interval())
            }).map_err(Error::IoError)?;
        }
    }
    fn next(&mut self, blocking: &crate::rt::Blocking, interrupt: &Interrupt) -> Result<Packet> {
        loop {
            if let Some(packet) = self.queue.pop_front() {
                // The next fill starts while packets remain, unless one is
                // outstanding or the queue already ends in an error.
                if packet.is_ok() && self.pending.is_none() && self.queue.len() < REFILL_BELOW
                    && !self.queue.back().is_some_and(|last| last.is_err()) {
                    self.pending = Some(self.request(Request::Next(FILL_PACKETS)));
                }
                return packet;
            }
            let id = match self.pending {
                Some(id) => id,
                None => {
                    let id = self.request(Request::Next(FILL_PACKETS));
                    self.pending = Some(id);
                    id
                }
            };
            match self.receive(blocking, id, interrupt)? {
                Response::Packets(packets, done) => {
                    if done {
                        self.pending = None;
                    }
                    self.queue.extend(packets);
                }
                _ => return Err(Error::IoError(std::io::Error::other(
                    "Unexpected spoken decoder response",
                ))),
            }
        }
    }
    fn seek(&mut self, blocking: &crate::rt::Blocking, mode: SeekMode, to: SeekTo, interrupt: &Interrupt) -> Result<SeekedTo> {
        self.pending = None;
        self.queue.clear();
        let id = self.request(Request::Seek(mode, to));
        match self.receive(blocking, id, interrupt)? {
            Response::Seek(result) => result,
            _ => Err(Error::IoError(std::io::Error::other(
                "Unexpected spoken seek response",
            ))),
        }
    }
}
impl Drop for Background {
    fn drop(&mut self) {
        let (lock, cv, ready) = &*self.shared;
        let mut state = lock.lock().unwrap();
        state.closed = true;
        state.request = None;
        state.result = None;
        self.io_cancel.store(true, Ordering::Relaxed);
        cv.notify_all();
        ready.notify();
    }
}

pub enum Packets {
    Direct(Box<dyn FormatReader>),
    Buffered(Background),
}
impl Packets {
    pub fn new(
        format: Box<dyn FormatReader>,
        background: bool,
        cancel: Arc<Interrupt>,
    ) -> std::io::Result<Self> {
        if background {
            Ok(Self::Buffered(Background::new(format, cancel)?))
        } else {
            Ok(Self::Direct(format))
        }
    }
    pub fn next_packet(&mut self, blocking: &crate::rt::Blocking, interrupt: &Interrupt) -> Result<Packet> {
        if crate::rt::on_executor() { return Err(Error::IoError(std::io::Error::new(std::io::ErrorKind::WouldBlock,"packet operation on executor"))); }
        match self {
            Self::Direct(format) => super::callback::with(blocking, || format.next_packet()),
            Self::Buffered(worker) => worker.next(blocking, interrupt),
        }
    }
    pub fn seek(&mut self, blocking: &crate::rt::Blocking, mode: SeekMode, to: SeekTo, interrupt: &Interrupt) -> Result<SeekedTo> {
        if crate::rt::on_executor() { return Err(Error::IoError(std::io::Error::new(std::io::ErrorKind::WouldBlock,"packet operation on executor"))); }
        match self {
            Self::Direct(format) => super::callback::with(blocking, || format.seek(mode, to)),
            Self::Buffered(worker) => worker.seek(blocking, mode, to, interrupt),
        }
    }
    pub fn retains_interrupted_packet(&self) -> bool {
        matches!(self, Self::Buffered(_))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use symphonia::core::{
        formats::{Cue, FormatOptions, Track},
        io::MediaSourceStream,
        meta::{Metadata, MetadataLog},
    };
    struct Generated {
        metadata: MetadataLog,
        cancel: Arc<Interrupt>,
        began: mpsc::Sender<u64>,
        gate: Arc<(Mutex<bool>, Condvar)>,
        position: u64,
    }
    impl FormatReader for Generated {
        fn try_new(_: MediaSourceStream, _: &FormatOptions) -> Result<Self> {
            unreachable!()
        }
        fn cues(&self) -> &[Cue] {
            &[]
        }
        fn metadata(&mut self) -> Metadata<'_> {
            self.metadata.metadata()
        }
        fn tracks(&self) -> &[Track] {
            &[]
        }
        fn into_inner(self: Box<Self>) -> MediaSourceStream {
            unreachable!()
        }
        fn seek(&mut self, _: SeekMode, to: SeekTo) -> Result<SeekedTo> {
            let SeekTo::TimeStamp { ts, .. } = to else {
                unreachable!()
            };
            self.position = ts;
            Ok(SeekedTo {
                track_id: 0,
                required_ts: ts,
                actual_ts: ts,
            })
        }
        fn next_packet(&mut self) -> Result<Packet> {
            self.began.send(self.position).ok();
            let (lock, cv) = &*self.gate;
            let mut allowed = lock.lock().unwrap();
            while !*allowed {
                if self.cancel.load(Ordering::Relaxed) {
                    return Err(Error::IoError(std::io::Error::other(
                        super::super::source::Cut,
                    )));
                }
                allowed = cv
                    .wait_timeout(allowed, Duration::from_millis(5))
                    .unwrap()
                    .0;
            }
            let n = self.position;
            self.position += 1;
            Ok(Packet::new_from_slice(0, n, 1, &n.to_le_bytes()))
        }
    }
    fn fixture() -> (Background, mpsc::Receiver<u64>, Arc<(Mutex<bool>, Condvar)>) {
        let (tx, rx) = mpsc::channel();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let cancel = Arc::new(Interrupt::new(false));
        let generated = Generated {
            metadata: Default::default(),
            cancel: cancel.clone(),
            began: tx,
            gate: gate.clone(),
            position: 0,
        };
        (
            Background::new(Box::new(generated), cancel).unwrap(),
            rx,
            gate,
        )
    }
    fn release(gate: &(Mutex<bool>, Condvar)) {
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
    }
    #[test]
    fn pause_interrupts_wait_but_preserves_inflight_packet_and_io() {
        let (mut worker, began, gate) = fixture();
        let id = worker.request(Request::Next(FILL_PACKETS));
        worker.pending = Some(id);
        assert_eq!(began.recv_timeout(Duration::from_secs(1)).unwrap(), 0);
        let interrupt = Interrupt::new(true);
        assert!(
            matches!(worker.next(&crate::rt::Blocking::dedicated_thread(), &interrupt),Err(Error::IoError(e)) if super::super::source::is_cut(&e))
        );
        assert!(!worker.io_cancel.load(Ordering::Relaxed));
        assert_eq!(worker.pending, Some(id));
        release(&gate);
        interrupt.store(false, Ordering::Relaxed);
        assert_eq!(worker.next(&crate::rt::Blocking::dedicated_thread(), &interrupt).unwrap().ts, 0);
        assert_eq!(worker.next(&crate::rt::Blocking::dedicated_thread(), &interrupt).unwrap().ts, 1);
    }
    #[test]
    fn newer_seek_cancels_and_discards_old_packet_not_new_generation() {
        let (mut worker, began, gate) = fixture();
        let id = worker.request(Request::Next(FILL_PACKETS));
        worker.pending = Some(id);
        assert_eq!(began.recv_timeout(Duration::from_secs(1)).unwrap(), 0);
        let interrupt = Interrupt::new(false);
        let seek = worker
            .seek(&crate::rt::Blocking::dedicated_thread(),
                SeekMode::Accurate,
                SeekTo::TimeStamp {
                    ts: 42,
                    track_id: 0,
                },
                &interrupt,
            )
            .unwrap();
        assert_eq!(seek.actual_ts, 42);
        release(&gate);
        assert_eq!(worker.next(&crate::rt::Blocking::dedicated_thread(), &interrupt).unwrap().ts, 42);
    }
    #[test]
    fn drop_cancels_owned_io_and_terminates_waiting_worker() {
        let (mut worker, began, _gate) = fixture();
        worker.request(Request::Next(FILL_PACKETS));
        assert_eq!(began.recv_timeout(Duration::from_secs(1)).unwrap(), 0);
        let shared = worker.shared.clone();
        let cancel = worker.io_cancel.clone();
        drop(worker);
        assert!(cancel.load(Ordering::Relaxed));
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while Arc::strong_count(&shared) > 1 && std::time::Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert_eq!(Arc::strong_count(&shared), 1);
    }
    /// Positions the demuxer started reading, waiting at most `limit` for each.
    fn began_within(began: &mpsc::Receiver<u64>, limit: Duration) -> Vec<u64> {
        let mut seen = Vec::new();
        while let Ok(position) = began.recv_timeout(limit) {
            seen.push(position);
        }
        seen
    }
    #[test]
    fn one_request_reads_a_fill_ahead_and_refills_only_when_low() {
        let (mut worker, began, gate) = fixture();
        release(&gate);
        let blocking = crate::rt::Blocking::dedicated_thread();
        let interrupt = Interrupt::new(false);
        // The first fill completes before the owner takes it.
        let id = worker.request(Request::Next(FILL_PACKETS));
        worker.pending = Some(id);
        let first = began_within(&began, Duration::from_millis(200));
        assert_eq!(first, (0..FILL_PACKETS as u64).collect::<Vec<_>>(), "one fill, no read past it");
        // Taking packets down to the refill mark starts no read; crossing it starts one fill.
        let mark = (FILL_PACKETS - REFILL_BELOW) as u64;
        for ts in 0..mark {
            assert_eq!(worker.next(&blocking, &interrupt).unwrap().ts, ts);
        }
        assert!(began_within(&began, Duration::from_millis(50)).is_empty(), "no read above the refill mark");
        let ts = mark;
        assert_eq!(worker.next(&blocking, &interrupt).unwrap().ts, ts);
        let second = began_within(&began, Duration::from_millis(200));
        assert_eq!(second, (FILL_PACKETS as u64..2 * FILL_PACKETS as u64).collect::<Vec<_>>());
        // Every packet arrives once and in order across fills.
        for ts in ts + 1..3 * FILL_PACKETS as u64 {
            assert_eq!(worker.next(&blocking, &interrupt).unwrap().ts, ts);
        }
    }
    #[test]
    fn seek_discards_read_ahead_packets() {
        let (mut worker, began, gate) = fixture();
        release(&gate);
        let blocking = crate::rt::Blocking::dedicated_thread();
        let interrupt = Interrupt::new(false);
        assert_eq!(worker.next(&blocking, &interrupt).unwrap().ts, 0);
        assert_eq!(began_within(&began, Duration::from_millis(200)).len(), FILL_PACKETS);
        let to = SeekTo::TimeStamp { ts: 1000, track_id: 0 };
        assert_eq!(worker.seek(&blocking, SeekMode::Accurate, to, &interrupt).unwrap().actual_ts, 1000);
        assert_eq!(worker.next(&blocking, &interrupt).unwrap().ts, 1000);
        assert_eq!(worker.next(&blocking, &interrupt).unwrap().ts, 1001);
    }
    #[test]
    fn an_error_ends_the_fill_after_its_packets_and_nothing_reads_past_it() {
        struct Ending(u64);
        impl FormatReader for Ending {
            fn try_new(_: MediaSourceStream, _: &FormatOptions) -> Result<Self> { unreachable!() }
            fn cues(&self) -> &[Cue] { &[] }
            fn metadata(&mut self) -> Metadata<'_> { unreachable!() }
            fn tracks(&self) -> &[Track] { &[] }
            fn into_inner(self: Box<Self>) -> MediaSourceStream { unreachable!() }
            fn seek(&mut self, _: SeekMode, _: SeekTo) -> Result<SeekedTo> { unreachable!() }
            fn next_packet(&mut self) -> Result<Packet> {
                let n = self.0;
                self.0 += 1;
                if n == 3 { return Err(Error::IoError(std::io::ErrorKind::UnexpectedEof.into())); }
                assert!(n < 3, "read past the end");
                Ok(Packet::new_from_slice(0, n, 1, &n.to_le_bytes()))
            }
        }
        let mut worker = Background::new(Box::new(Ending(0)), Arc::new(Interrupt::new(false))).unwrap();
        let blocking = crate::rt::Blocking::dedicated_thread();
        let interrupt = Interrupt::new(false);
        let mut seen = Vec::new();
        loop {
            match worker.next(&blocking, &interrupt) {
                Ok(packet) => seen.push(packet.ts),
                Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
                Err(e) => panic!("{e}"),
            }
        }
        assert_eq!(seen, [0, 1, 2]);
        assert!(worker.pending.is_none() && worker.queue.is_empty());
    }
}
