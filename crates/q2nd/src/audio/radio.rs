#![forbid(unsafe_code)]
//! Live radio: bounded RAM, no card writes, interruptible decoder reads.
use crate::audio::interrupt::Interrupt;
use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{
    Arc, Mutex,
    atomic::Ordering,
};
use std::time::Duration;
use symphonia::core::io::MediaSource;

/// Ten seconds of compressed audio, not decoded PCM. Unknown bitrate: 256 kb/s.
/// Bound unusual/malicious headers as well as ordinary streams.
fn capacity(bitrate: Option<u32>) -> usize {
    (bitrate.unwrap_or(256) as u64 * 125 * 10).clamp(64 * 1024, 512 * 1024) as usize
}
struct Buffer {
    bytes: VecDeque<u8>,
    capacity: usize,
    done: bool,
    error: Option<String>,
    cancelled: bool,
    ready: bool,
    metadata_interval: Option<usize>,
}
struct Shared {
    buffer: Mutex<Buffer>,
    cv: Arc<super::interrupt::Event>,
    stopped: tokio::sync::Notify,
    space: tokio::sync::Notify,
    cancel_producer: tokio::sync::Notify,
}
/// Strip metadata and normalize AAC only as the decoder consumes the RAM queue.
/// This keeps a station's next title from jumping ahead by the network reserve.
pub struct Radio {
    buffered: Option<Buffered>,
    // MediaSource requires Sync; exclusive reads use get_mut, without locking.
    reader: Option<Mutex<AdtsCompat>>,
    metadata: subsonic::radio::Metadata,
    external_metadata: bool,
}
impl Radio {
    pub fn open(url: String, proxy: Option<String>, interrupt: Arc<Interrupt>) -> Self {
        Self::open_with_metadata(url, proxy, interrupt, None)
    }
    /// Only the explicit generated radio fixture passes an alternate feed.
    pub(crate) fn open_fixture(url: String, proxy: Option<String>, interrupt: Arc<Interrupt>, endpoint: String) -> Self {
        Self::open_with_metadata(url, proxy, interrupt, Some(endpoint))
    }
    fn open_with_metadata(url: String, proxy: Option<String>, interrupt: Arc<Interrupt>, endpoint: Option<String>) -> Self {
        let metadata = subsonic::radio::Metadata::default();
        Self {
            external_metadata: subsonic::radio::kcrw::matches_stream(&url),
            buffered: Some(Buffered::open(url, proxy, interrupt, metadata.clone(), endpoint)),
            reader: None,
            metadata,
        }
    }
    pub fn metadata(&self) -> subsonic::radio::Metadata { self.metadata.clone() }
}
impl Read for Radio {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        super::callback::require()?;
        if out.is_empty() { return Ok(0); }
        if self.reader.is_none() {
            let buffered = self.buffered.as_ref().ok_or_else(|| io::Error::other("Radio stream disconnected"))?;
            let interval = {
                let mut wait = super::interrupt::Waiter::new(&buffered.interrupt, &buffered.shared.cv);
                loop {
                    let b = buffered.shared.buffer.lock().unwrap();
                    if b.ready || b.done { break b.metadata_interval; }
                    if buffered.interrupt.load(Ordering::Relaxed) {
                        return Err(io::Error::other(super::source::Cut));
                    }
                    drop(b);
                    super::source::wait_context()?;
                    crate::health::pulse();
                    wait.wait(super::interrupt::health_interval())?;
                }
            };
            let reader = subsonic::radio::audio_reader(
                Box::new(self.buffered.take().unwrap()), interval,
                if self.external_metadata { Default::default() } else { self.metadata.clone() });
            self.reader = Some(Mutex::new(AdtsCompat::new(reader)?));
        }
        // Every radio read is a wait for the network, never a card read
        // (crate::playtrace).
        let waiting = crate::playtrace::fill_timer();
        let result = self.reader.as_mut().unwrap().get_mut().unwrap().read(out);
        crate::playtrace::note_fill_wait(waiting);
        result
    }
}
impl Seek for Radio {
    fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
        Err(io::Error::new(io::ErrorKind::Unsupported, "Live radio cannot seek"))
    }
}
impl MediaSource for Radio {
    fn is_seekable(&self) -> bool { false }
    fn byte_len(&self) -> Option<u64> { None }
}

struct Buffered {
    shared: Arc<Shared>,
    interrupt: Arc<Interrupt>,
}
impl Buffered {
    fn empty(interrupt: Arc<Interrupt>) -> Self {
        Self {
            shared: Arc::new(Shared {
                buffer: Mutex::new(Buffer {
                    bytes: VecDeque::new(),
                    capacity: capacity(None),
                    done: false,
                    error: None,
                    cancelled: false,
                    ready: false,
                    metadata_interval: None,
                }),
                cv: Arc::default(),
                stopped: tokio::sync::Notify::new(),
                space: tokio::sync::Notify::new(),
                cancel_producer: tokio::sync::Notify::new(),
            }),
            interrupt,
        }
    }
    fn open(url: String, proxy: Option<String>, interrupt: Arc<Interrupt>, metadata: subsonic::radio::Metadata, endpoint: Option<String>) -> Self {
        let radio = Self::empty(interrupt);
        crate::rt::io().spawn(producer(radio.shared.clone(), url, proxy, metadata, endpoint));
        radio
    }
}

// Constructed before spawning so even an unpolled/aborted future completes the
// reader-facing lifetime. It never needs a socket or blocking worker to retire.
struct ProducerDone {
    shared: Arc<Shared>,
    error: Option<String>,
}
impl Drop for ProducerDone {
    fn drop(&mut self) {
        let mut buffer = self.shared.buffer.lock().unwrap();
        buffer.done = true;
        if !buffer.cancelled {
            buffer.error = self.error.take();
        }
        self.shared.cv.notify();
        self.shared.stopped.notify_one();
    }
}
async fn producer_cancelled(shared: &Shared) {
    loop {
        let notified = shared.cancel_producer.notified();
        if shared.buffer.lock().unwrap().cancelled {
            return;
        }
        notified.await;
    }
}
fn producer(
    shared: Arc<Shared>,
    url: String,
    proxy: Option<String>,
    metadata: subsonic::radio::Metadata,
    endpoint: Option<String>,
) -> impl std::future::Future<Output = ()> + Send {
    let completion = ProducerDone {
        shared: shared.clone(),
        error: Some("Radio stream disconnected".into()),
    };
    async move {
        let mut completion = completion;
        let result = tokio::select! {
            biased;
            ()=producer_cancelled(&shared)=>Ok(()),
            result=fill_radio(&shared,url,proxy,metadata,endpoint)=>result,
        };
        completion.error = result.err().map(|error| safe_error(&error));
    }
}
async fn fill_radio(
    shared: &Arc<Shared>,
    url: String,
    proxy: Option<String>,
    metadata: subsonic::radio::Metadata,
    endpoint: Option<String>,
) -> io::Result<()> {
    let stream = subsonic::radio::open_async(&url, proxy.as_deref()).await?;
    let kcrw = subsonic::radio::kcrw::matches_stream(&url);
    let has_metadata = stream.metadata_interval.is_some();
    {
        let mut buffer = shared.buffer.lock().unwrap();
        if buffer.cancelled {
            return Ok(());
        }
        buffer.capacity = capacity(stream.bitrate_kbps);
        buffer.metadata_interval = stream.metadata_interval;
        buffer.ready = true;
        shared.cv.notify();
    }
    if !has_metadata && (endpoint.is_some() || subsonic::radio::france::matches_stream(&url)) {
        spawn_fip_metadata(shared.clone(), proxy, metadata, endpoint);
    } else if kcrw {
        // Strip ICY blocks from audio, but their empty titles must never
        // reset the separately polled playlist title.
        let feed = metadata.clone();
        crate::rt::io().spawn(poll_station_metadata(shared.clone(), feed, move || {
            let proxy = proxy.clone();
            async move { subsonic::radio::kcrw::fetch_async(proxy.as_deref(), epoch_seconds()).await }
        }, epoch_seconds));
    }
    let mut body = stream.into_decoded();
    let mut pending = Vec::new();
    let mut offset = 0;
    loop {
        let notified = shared.space.notified();
        let room = {
            let buffer = shared.buffer.lock().unwrap();
            if buffer.cancelled {
                return Ok(());
            }
            (buffer.capacity - buffer.bytes.len()).min(16384)
        };
        if room == 0 {
            notified.await;
            continue;
        }
        if offset == pending.len() {
            let Some(chunk) = body.chunk().await else {
                return Ok(());
            };
            pending = chunk?;
            offset = 0;
            if pending.is_empty() {
                continue;
            }
        }
        let mut buffer = shared.buffer.lock().unwrap();
        if buffer.cancelled {
            return Ok(());
        }
        // There is one producer; a reader can only increase the reserved room.
        let count = room.min(pending.len() - offset);
        buffer.bytes.extend(&pending[offset..offset + count]);
        offset += count;
        shared.cv.notify();
    }
}

fn epoch_seconds() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs()
}

/// Independent from decoding: metadata failure must never stall audio. Both
/// requests use the same configured route, and closing the stream stops polls.
fn spawn_fip_metadata(shared: Arc<Shared>, proxy: Option<String>, metadata: subsonic::radio::Metadata, endpoint: Option<String>) {
    crate::rt::io().spawn(poll_station_metadata(shared, metadata, move || {
        let proxy = proxy.clone();
        let endpoint = endpoint.clone();
        async move {
            match endpoint {
                Some(endpoint) => subsonic::radio::france::fetch_from_async(&endpoint, proxy.as_deref()).await,
                None => subsonic::radio::france::fetch_async(proxy.as_deref()).await,
            }
        }
    }, epoch_seconds));
}

async fn metadata_stopped(shared: &Shared) {
    loop {
        let notified = shared.stopped.notified();
        {
            let buffer = shared.buffer.lock().unwrap();
            if buffer.cancelled || buffer.done { return; }
        }
        // One owner consumes this retained notification; a close between the
        // state check and waiter registration cannot be lost.
        notified.await;
    }
}

async fn poll_station_metadata<F, Fut, N>(shared: Arc<Shared>, metadata: subsonic::radio::Metadata, mut fetch: F, now: N)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = io::Result<subsonic::radio::france::Update>>,
    N: Fn() -> u64,
{
    let mut expires = 0;
    let mut retry = 15;
    loop {
        {
            let buffer = shared.buffer.lock().unwrap();
            if buffer.cancelled || buffer.done { return; }
            if now() >= expires { *metadata.lock().unwrap() = None; }
        }
        let update = tokio::select! {
            biased;
            () = metadata_stopped(&shared) => return,
            update = fetch() => update,
        };
        let delay = {
            // Serialize publication with close, including simultaneous ready
            // response/cancellation. No lock spans an await.
            let buffer = shared.buffer.lock().unwrap();
            if buffer.cancelled || buffer.done { return; }
            match update {
                Ok(update) => {
                    expires = update.expires;
                    retry = 15;
                    *metadata.lock().unwrap() = update.title;
                    update.refresh
                }
                Err(_) => {
                    if now() >= expires { *metadata.lock().unwrap() = None; }
                    let delay = Duration::from_secs(retry);
                    retry = (retry * 2).min(60);
                    delay
                }
            }
        };
        tokio::select! {
            biased;
            () = metadata_stopped(&shared) => return,
            () = tokio::time::sleep(delay) => {},
        }
    }
}

fn safe_error(error: &io::Error) -> String {
    let message = error.to_string();
    // Only our URL-free errors may cross the HTTP/codec boundary. Never show
    // arbitrary transport error text, which may include a private stream URL.
    if ["Radio connection failed (", "Radio server returned HTTP ", "HLS radio "]
        .iter().any(|prefix| message.starts_with(prefix))
        || ["Invalid radio proxy", "Radio requires an HTTP or HTTPS stream",
            "Could not read radio playlist", "Radio playlist is too large",
            "Radio playlist nesting is too deep", "Empty radio playlist",
            "Invalid radio URL", "Invalid playlist URL", "Unsupported playlist URL scheme",
            "Radio needs AAC-LC with one block per frame", "Invalid AAC frame"].contains(&message.as_str()) {
        message
    } else {
        "Radio stream disconnected".into()
    }
}
impl Drop for Buffered {
    fn drop(&mut self) {
        self.shared.buffer.lock().unwrap().cancelled = true;
        self.shared.cv.notify();
        self.shared.stopped.notify_one();
        self.shared.cancel_producer.notify_one();
        self.shared.space.notify_one();
    }
}
impl Read for Buffered {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        super::callback::require()?;
        if out.is_empty() {
            return Ok(0);
        }
        let mut wait = super::interrupt::Waiter::new(&self.interrupt, &self.shared.cv);
        loop {
            let mut b = self.shared.buffer.lock().unwrap();
            if self.interrupt.load(Ordering::Relaxed) {
                return Err(io::Error::other(super::source::Cut));
            }
            if !b.bytes.is_empty() {
                let n = out.len().min(b.bytes.len());
                let (first, second) = b.bytes.as_slices();
                let a = n.min(first.len());
                out[..a].copy_from_slice(&first[..a]);
                out[a..n].copy_from_slice(&second[..n - a]);
                b.bytes.drain(..n);
                self.shared.space.notify_one();
                return Ok(n);
            }
            if b.done {
                return b
                    .error
                    .as_ref()
                    .map_or(Ok(0), |e| Err(io::Error::other(e.clone())));
            }
            drop(b);
            super::source::wait_context()?;
            crate::health::pulse();
            wait.wait(super::interrupt::health_interval())?;
        }
    }
}
impl Seek for Buffered {
    fn seek(&mut self, _: SeekFrom) -> io::Result<u64> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Live radio cannot seek",
        ))
    }
}
impl MediaSource for Buffered {
    fn is_seekable(&self) -> bool {
        false
    }
    fn byte_len(&self) -> Option<u64> {
        None
    }
}

/// Symphonia 0.5's ADTS reader only recognizes MPEG-4, CRC-free headers
/// (FF F1). MPEG-2 AAC-LC stations such as KEXP use FF F9 with the same
/// raw AAC-LC payload. Normalize *frame headers only*, never payload bytes.
struct AdtsCompat {
    reader: Box<dyn Read + Send>,
    pending: io::Cursor<Vec<u8>>,
    adts: bool,
}
impl AdtsCompat {
    fn new(mut reader: Box<dyn Read + Send>) -> io::Result<Self> {
        super::callback::require()?;
        let mut header = vec![0; 7];
        reader.read_exact(&mut header)?;
        let adts = header[0] == 0xff && header[1] & 0xf6 == 0xf0;
        let mut r = Self {
            reader,
            pending: io::Cursor::new(Vec::new()),
            adts,
        };
        if adts {
            r.frame(header)?;
        } else {
            r.pending = io::Cursor::new(header);
        }
        Ok(r)
    }
    fn frame(&mut self, mut h: Vec<u8>) -> io::Result<()> {
        super::callback::require()?;
        if h[0] != 0xff || h[1] & 0xf6 != 0xf0 || h[2] >> 6 != 1 || h[6] & 3 != 0 {
            return Err(io::Error::other(
                "Radio needs AAC-LC with one block per frame",
            ));
        }
        let len = ((h[3] as usize & 3) << 11) | ((h[4] as usize) << 3) | (h[5] as usize >> 5);
        let crc = h[1] & 1 == 0;
        if len < if crc { 9 } else { 7 } {
            return Err(io::Error::other("Invalid AAC frame"));
        }
        if crc {
            let mut checksum = [0; 2];
            self.reader.read_exact(&mut checksum)?;
        }
        let normalized_len = len - if crc { 2 } else { 0 };
        h[1] = (h[1] & !8) | 1;
        h[3] = (h[3] & 0xfc) | ((normalized_len >> 11) as u8 & 3);
        h[4] = (normalized_len >> 3) as u8;
        h[5] = (h[5] & 0x1f) | (((normalized_len & 7) as u8) << 5);
        h.resize(normalized_len, 0);
        self.reader.read_exact(&mut h[7..])?;
        self.pending = io::Cursor::new(h);
        Ok(())
    }
}
impl Read for AdtsCompat {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        super::callback::require()?;
        if out.is_empty() {
            return Ok(0);
        }
        let n = self.pending.read(out)?;
        if n > 0 {
            return Ok(n);
        }
        if !self.adts {
            return self.reader.read(out);
        }
        let mut h = vec![0; 7];
        let n = self.reader.read(&mut h[..1])?;
        if n == 0 {
            return Ok(0);
        }
        self.reader.read_exact(&mut h[1..])?;
        self.frame(h)?;
        self.pending.read(out)
    }
}
#[cfg(test)]
mod tests {
    /// One ADTS frame: MPEG-2 or MPEG-4, with or without a CRC, AAC-LC.
    fn adts_frame(mpeg2: bool, crc: bool, payload: &[u8]) -> Vec<u8> {
        let len = payload.len() + if crc { 9 } else { 7 };
        let mut frame = vec![0xff, 0xf0 | u8::from(mpeg2) << 3 | u8::from(!crc), 0x50, 0x80 | (len >> 11) as u8 & 3, (len >> 3) as u8, ((len & 7) as u8) << 5 | 0x1f, 0xfc];
        if crc { frame.extend([0xab, 0xcd]); }
        frame.extend_from_slice(payload);
        frame
    }
    /// What the normaliser must give Symphonia: MPEG-4, no CRC, same payload.
    fn normalized(payload: &[u8]) -> Vec<u8> { adts_frame(false, false, payload) }
    struct Pieces(io::Cursor<Vec<u8>>, usize);
    impl Read for Pieces {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> { let len = out.len().min(self.1); self.0.read(&mut out[..len]) }
    }
    fn through_compat(bytes: Vec<u8>, piece: usize, step: usize) -> (Vec<u8>, bool, crate::test_alloc::Stats) {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let ((out, clean), heap) = crate::test_alloc::measure(|| crate::audio::callback::with(&blocking, || {
            let mut out = Vec::new();
            let mut reader = match AdtsCompat::new(Box::new(Pieces(io::Cursor::new(bytes), piece))) { Ok(reader) => reader, Err(_) => return (out, false) };
            let mut buffer = vec![0; step];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => return (out, true),
                    Ok(n) => out.extend_from_slice(&buffer[..n]),
                    Err(_) => return (out, false),
                }
            }
        }));
        (out, clean, heap)
    }

    #[test]
    fn adts_streams_of_any_framing_normalize_and_damaged_ones_end_bounded() {
        let mut rng = fastrand::Rng::with_seed(0x4144_5453);
        for case in 0..600 {
            let frames: Vec<(bool, bool, Vec<u8>)> = (0..rng.usize(1..12)).map(|_| (rng.bool(), rng.bool(), (0..rng.usize(0..700)).map(|_| rng.u8(..)).collect())).collect();
            let wire: Vec<u8> = frames.iter().flat_map(|(mpeg2, crc, payload)| adts_frame(*mpeg2, *crc, payload)).collect();
            let expected: Vec<u8> = frames.iter().flat_map(|(_, _, payload)| normalized(payload)).collect();
            let (piece, step) = (rng.usize(1..900), rng.usize(1..3000));
            let (out, clean, _) = through_compat(wire.clone(), piece, step);
            assert!(clean && out == expected, "case {case}: {} frames, {} of {} bytes", frames.len(), out.len(), expected.len());
            // Damage: whole normalized frames come out, then an error or the
            // end; never more than was sent, and no allocation beyond a frame
            // (13 bits of length) and the collected output.
            let mut damaged = wire.clone();
            for _ in 0..rng.usize(1..4) {
                let at = rng.usize(..damaged.len());
                match rng.u8(..4) {
                    0 => damaged[at] ^= 1 << rng.u8(..8),
                    1 => damaged.truncate(at),
                    2 => { let end = (at + rng.usize(1..16)).min(damaged.len()); damaged.drain(at..end); }
                    _ => damaged[at] = 0xff,
                }
            }
            let sent = damaged.len();
            let (out, _, heap) = through_compat(damaged, piece, step);
            assert!(out.len() <= sent, "case {case}: {} bytes out of {sent}", out.len());
            assert!(heap.largest <= 8192.max(2 * out.len().next_power_of_two()).max(step), "case {case}: {heap:?}");
        }
        // A stream that is not ADTS passes through untouched.
        let other: Vec<u8> = (0..5000u32).map(|n| (n * 7) as u8).collect();
        let (out, clean, _) = through_compat(other.clone(), 333, 1000);
        assert!(clean && out == other);
    }

    #[test]
    fn finite_radio_jobs_read_ready_bytes_but_refuse_header_and_body_waits() {
        let interrupt=Arc::new(Interrupt::new(false));
        let cancel=interrupt.clone();
        let (started, entered)=tokio::sync::oneshot::channel();
        let mut job=crate::rt::jobs().run_isolated(move |blocking| {
            started.send(()).unwrap();
            crate::audio::callback::with(blocking, || {
                let mut radio=Radio {buffered:Some(Buffered::empty(cancel.clone())),reader:None,metadata:Default::default(),external_metadata:false};
                assert_eq!(radio.read(&mut [0]).unwrap_err().kind(),io::ErrorKind::WouldBlock);
                assert!(radio.reader.is_none(),"refused header wait must retain initial owner");
                {let mut state=radio.buffered.as_ref().unwrap().shared.buffer.lock().unwrap();state.ready=true;state.bytes.extend(b"radio ready");}
                let mut ready=[0;4]; assert_eq!(radio.read(&mut ready).unwrap(),4); assert_eq!(&ready,b"radi");
                let mut bytes=Buffered::empty(cancel);
                {let mut state=bytes.shared.buffer.lock().unwrap();state.ready=true;state.bytes.extend(b"data");}
                let mut out=[0;4]; assert_eq!(bytes.read(&mut out).unwrap(),4); assert_eq!(&out,b"data");
                assert_eq!(bytes.read(&mut out).unwrap_err().kind(),io::ErrorKind::WouldBlock);
                bytes.shared.buffer.lock().unwrap().done=true;
                assert_eq!(bytes.read(&mut out).unwrap(),0);
            });
        });
        let ui=crate::rt::Ui::new().unwrap();
        let result=ui.block_on(async {
            entered.await.unwrap(); // Queue admission is not a callback latency assertion.
            tokio::time::timeout(Duration::from_secs(2), &mut job).await
        });
        if result.is_err() {
            interrupt.store(true,Ordering::Relaxed);
            let _=ui.block_on(job); // Retire the bounded-wait fixture before failure.
        }
        result.expect("finite radio callback parked waiting for a producer").unwrap();
    }

    use super::*;
    #[test]
    fn buffered_titles_wait_for_audio_consumption() {
        crate::audio::callback::test_scope(|| {
        let buffered = Buffered::empty(Arc::new(Interrupt::new(false)));
        {
            let mut b = buffered.shared.buffer.lock().unwrap();
            b.ready = true;
            b.metadata_interval = Some(8);
            b.bytes.extend(b"abcdefgh");
            b.bytes.push_back(2);
            let mut metadata = b"StreamTitle='Artist - Song';".to_vec();
            metadata.resize(32, 0);
            b.bytes.extend(metadata);
            b.bytes.extend(b"ijklmnop");
            b.done = true;
        }
        let mut radio = Radio { buffered: Some(buffered), reader: None, metadata: Default::default(), external_metadata: false };
        let title = radio.metadata();
        assert_eq!(*title.lock().unwrap(), None);
        let mut audio = [0; 8];
        radio.read_exact(&mut audio).unwrap();
        assert_eq!(&audio, b"abcdefgh");
        assert_eq!(*title.lock().unwrap(), None);
        radio.read_exact(&mut audio).unwrap();
        assert_eq!(&audio, b"ijklmnop");
        assert_eq!(title.lock().unwrap().as_deref(), Some("Artist - Song"));
            })
    }
    #[test]
    fn ten_seconds_has_a_hard_memory_bound() {
        crate::audio::callback::test_scope(|| {
        assert_eq!(capacity(Some(128)), 160000);
        assert_eq!(capacity(Some(320)), 400000);
        assert_eq!(capacity(None), 320000);
        assert_eq!(capacity(Some(u32::MAX)), 512 * 1024);
            })
    }
    #[test]
    fn interrupted_wait_is_bounded_and_not_retried_as_eintr() {
        crate::audio::callback::test_scope(|| {
        let flag = Arc::new(Interrupt::new(false));
        let signal = flag.clone();
        let mut r = Buffered::empty(flag);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            signal.store(true, Ordering::Relaxed);
        });
        let start = std::time::Instant::now();
        let error = r.read(&mut [0; 1]).unwrap_err();
        assert!(super::super::source::is_cut(&error));
        assert!(start.elapsed() < Duration::from_millis(500));
        assert!(!r.is_seekable());
        assert_eq!(r.byte_len(), None);
            })
    }
    #[test]
    fn reads_wrapped_buffer_then_reports_disconnect() {
        crate::audio::callback::test_scope(|| {
        let mut r = Buffered::empty(Arc::new(Interrupt::new(false)));
        {
            let mut b = r.shared.buffer.lock().unwrap();
            b.bytes.extend(0..8);
            b.bytes.drain(..6);
            b.bytes.extend(8..14);
            b.done = true;
            b.error = Some("lost".into());
        }
        let mut bytes = [0; 8];
        r.read_exact(&mut bytes).unwrap();
        assert_eq!(bytes, [6, 7, 8, 9, 10, 11, 12, 13]);
        assert!(r.read(&mut bytes).is_err());
            })
    }
    #[test]
    fn mpeg2_adts_normalizes_headers_without_touching_payload() {
        crate::audio::callback::test_scope(|| {
        // Synthetic 11-byte AAC-LC frames with an ADTS-like sequence in payload.
        for crc in [false, true] {
            let len = if crc { 13 } else { 11 };
            let mut frame = vec![
                0xff,
                if crc { 0xf8 } else { 0xf9 },
                0x50,
                0x80,
                (len >> 3) as u8,
                ((len & 7) << 5) as u8 | 0x1f,
                0xfc,
            ];
            if crc {
                frame.extend_from_slice(&[0, 0]);
            }
            frame.extend_from_slice(&[0xff, 0xf9, 1, 2]);
            let mut bytes = frame.clone();
            bytes.extend(frame);
            let mut reader = AdtsCompat::new(Box::new(io::Cursor::new(bytes))).unwrap();
            let mut out = Vec::new();
            reader.read_to_end(&mut out).unwrap();
            assert_eq!(out.len(), 22);
            assert_eq!(out[1], 0xf1);
            assert_eq!(out[12], 0xf1);
            assert_eq!(&out[7..11], &[0xff, 0xf9, 1, 2]);
            assert_eq!(&out[18..], &[0xff, 0xf9, 1, 2]);
        }
            })
    }
}

#[cfg(test)]
#[path = "radio_metadata_tests.rs"]
mod metadata_tests;

#[cfg(test)]
#[path = "radio_transport_tests.rs"]
mod transport_tests;
