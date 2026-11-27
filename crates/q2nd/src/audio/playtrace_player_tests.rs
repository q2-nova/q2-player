//! The playback trace through the real player thread, decoder and fake PCM:
//! an injected read stall, the output running dry behind it, and the
//! request/completion endpoints of play, pause, resume and skip.
use super::*;
use crate::audio::alsa::{FAKE, Sink};
use crate::playtrace::{Event, Kind, Trace, NONE};
use std::io::{Cursor, Read, Seek, SeekFrom};

/// 16-bit stereo 8 kHz silence.
fn wav(secs: f32) -> Vec<u8> {
    let rate = 8000u32;
    let data = (rate as f32 * secs) as u32 * 4;
    let mut v = Vec::new();
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&2u16.to_le_bytes());
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&(rate * 4).to_le_bytes());
    v.extend_from_slice(&4u16.to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data.to_le_bytes());
    v.resize(v.len() + data as usize, 0);
    v
}

/// A file whose first read at or past `stall_at` blocks for `stall`: a card
/// read queued behind writeback.
struct Stalling {
    inner: Cursor<Vec<u8>>,
    stall_at: u64,
    stall: Duration,
}

impl Read for Stalling {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if !self.stall.is_zero() && self.inner.position() >= self.stall_at {
            std::thread::sleep(std::mem::take(&mut self.stall));
        }
        self.inner.read(buf)
    }
}
impl Seek for Stalling {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> { self.inner.seek(pos) }
}
impl MediaSource for Stalling {
    fn is_seekable(&self) -> bool { true }
    fn byte_len(&self) -> Option<u64> { Some(self.inner.get_ref().len() as u64) }
}

struct Files {
    audio: Vec<u8>,
    /// Song ID that stalls, where and for how long.
    stall: Option<(&'static str, u64, Duration)>,
}

impl TrackSource for Files {
    fn open(&self, _blocking: &crate::rt::Blocking, song: &Song, _: &Arc<crate::audio::interrupt::Interrupt>) -> Result<OpenedTrack, String> {
        let (stall_at, stall) = self.stall.filter(|(id, ..)| *id == song.id).map_or((0, Duration::ZERO), |(_, at, d)| (at, d));
        Ok(OpenedTrack { source: Box::new(Stalling { inner: Cursor::new(self.audio.clone()), stall_at, stall }),
            ext: Some("wav".into()), origin: Origin::Cached, radio_metadata: None })
    }
    fn upcoming(&self, _: &[Song]) {}
    fn now_playing(&self, _: &Song) {}
    fn played(&self, _: &Song, _: u64) {}
    fn save_queue(&self, _: &[Song], _: usize, _: u64) {}
}

/// The fake PCM with its underrun model on: a write that finds the queue
/// empty after audio was queued is a recovery, as EPIPE is on the device.
struct UnderrunHw;
impl AudioHw for UnderrunHw {
    fn device(&mut self) -> String { FAKE.into() }
    fn open_sink<'a>(&mut self, blocking: &'a crate::rt::Blocking, device: &str, rate: u32, channels: u32, latency: u32, format:crate::audio::pcm_format::PcmFormat) -> Result<Sink<'a>,String> {
        let mut sink = Sink::open_format(blocking,device,rate,channels,latency,format)?;
        sink.model_underruns(-32);
        Ok(sink)
    }
    fn power_on(&mut self, _blocking: &crate::rt::Blocking) {}
    fn power_off(&mut self, _blocking: &crate::rt::Blocking) {}
    fn mute(&mut self, _blocking: &crate::rt::Blocking, _: bool) {}
    fn set_attenuation(&mut self, _blocking: &crate::rt::Blocking, _: u8) -> Result<bool, String> { Ok(false) }
    fn set_filter(&mut self, _blocking: &crate::rt::Blocking, _: u8) {}
}

fn song(id: &str) -> Song { Song { id: id.into(), title: id.into(), duration: 4, ..Default::default() } }

fn player(files: Files) -> (Arc<Trace>, PlayerHandle) {
    let trace = Arc::new(Trace::new(true));
    let settings = Settings { eq: Default::default(), volume: 100, headphone_power: crate::config::HeadphonePower::Standard, filter: 0, replay_gain: ReplayGainMode::Off, gapless: true };
    let handle = spawn_traced(Arc::new(files), Box::new(UnderrunHw), settings, Arc::new(|| {}), None, trace.clone());
    (trace, handle)
}

fn wait_for(p: &PlayerHandle, secs: u64, f: impl Fn(&Status) -> bool) -> Status {
    let end = Instant::now() + Duration::from_secs(secs);
    loop {
        let st = p.status();
        if f(&st) || Instant::now() > end { return st; }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Waits on the trace itself: a status snapshot is rate-limited and need
/// not be published at the instant a track ends.
fn wait_event(trace: &Trace, secs: u64, f: impl Fn(&[Event]) -> bool) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    loop {
        if f(&trace.snapshot().events) { return true; }
        if Instant::now() > end { return false; }
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn of(events: &[Event], kind: Kind) -> Vec<Event> { events.iter().filter(|e| e.kind == kind).copied().collect() }

#[test]
fn injected_read_stall_runs_the_output_dry_and_the_trace_places_the_recovery_after_it() {
    const STALL: Duration = Duration::from_millis(700);
    // The decoder's stream reads ahead in growing blocks; a stall armed at
    // 40 KiB hits the read that follows the block ending at 63 KiB, about
    // two seconds in, with the output's queue (under 400 ms) all that is left.
    let (trace, p) = player(Files { audio: wav(4.0), stall: Some(("stalls", 40 * 1024, STALL)) });
    p.send(Cmd::Play { songs: std::sync::Arc::new(vec![song("stalls")]), index: 0, shuffle: false });
    assert!(wait_event(&trace, 15, |events| events.iter().any(|e| e.kind == Kind::TrackEnd && e.b == 1)), "the track plays to its end");
    p.shutdown();
    let snapshot = trace.snapshot();
    let events = &snapshot.events;
    assert_eq!((snapshot.events_lost, snapshot.windows_lost), (0, 0));

    let slow = of(events, Kind::SlowRead).into_iter().max_by_key(|e| e.a).expect("the stall is an event");
    assert!(u128::from(slow.a) >= STALL.as_micros() && slow.a < 3_000_000, "{slow:?}");
    assert_eq!((slow.b, slow.c), (0, 1), "a file read, not a wait for download bytes; track 1");

    // The first write after the stall finds the queue empty: a recovery,
    // stamped after the stalled read returned.
    let recovery = of(events, Kind::Recovery).into_iter().find(|e| e.seq > slow.seq).expect("a recovery after the stall");
    assert_eq!((recovery.a, recovery.b, recovery.c), (32, 0, 1), "EPIPE with nothing queued, track 1");
    assert!(recovery.seq > slow.seq && recovery.t_us >= slow.t_us && recovery.t_us - slow.t_us < 250_000, "{slow:?} {recovery:?}");
    let low = of(events, Kind::LowBuffer).into_iter().find(|e| e.seq > slow.seq).expect("the queue crossing under 100 ms");
    assert_eq!(low.a, 0);
    // The stall is also the longest hole between two output writes.
    let gap = of(events, Kind::WriteGap).into_iter().max_by_key(|e| e.a).expect("a write gap");
    assert!(gap.a >= slow.a && gap.a < slow.a + 400_000 && (gap.b, gap.c) == (0, 1), "{gap:?} {slow:?}");

    // The window in which the stalled packet completed carries the read time
    // and the buffer minimum; earlier windows had audio queued.
    let windows: Vec<_> = snapshot.windows.iter().chain(snapshot.current.iter()).copied().collect();
    let stalled = windows.iter().position(|w| w.read_max >= slow.a).expect("a window with the stall");
    // (The write follows the decode by microseconds; allow a window boundary between them.)
    assert!(windows[stalled..].iter().take(2).any(|w| w.buf_min_ms == 0), "{:?}", &windows[stalled..]);
    assert!(windows[stalled].fill_max == 0 && windows.iter().all(|w| w.rate == 8000 || w.frames == 0));
    assert!(windows.iter().map(|w| w.gap_max).max().unwrap() >= slow.a);
    assert!(windows.iter().all(|w| w.dirty_kb == NONE && w.xfer_bytes == 0), "no transfer: writeback is not sampled");
    assert_eq!(windows.iter().map(|w| u64::from(w.frames)).sum::<u64>(), 32000, "output progress: every frame of four seconds");
    assert!(windows.iter().filter(|w| w.read_max < 20_000 && w.buf_min_ms != NONE).count() >= 1);

    // Endpoints of the one request, in order, sharing serial 1.
    let request = of(events, Kind::Request)[0];
    let handled = of(events, Kind::Handled)[0];
    let pcm = of(events, Kind::FirstPcm)[0];
    let submit = of(events, Kind::FirstSubmit)[0];
    assert_eq!(((request.a, request.b), (handled.a, handled.b), (pcm.a, pcm.b), (submit.a, submit.b)), ((1, 1), (1, 1), (1, 1), (1, 1)));
    assert!(request.t_us <= handled.t_us && handled.t_us <= pcm.t_us && pcm.t_us <= submit.t_us && submit.t_us < slow.t_us);
    let playing = of(events, Kind::Status).into_iter().find(|e| e.b == 1).expect("the UI was told it is playing");
    assert!(playing.a == 1 && playing.t_us >= submit.t_us);
    let end = of(events, Kind::TrackEnd);
    assert_eq!(end.iter().map(|e| (e.a, e.b)).collect::<Vec<_>>(), [(1, 1)], "one track, ended at end of file");
    assert_eq!(of(events, Kind::Open).iter().map(|e| (e.a, e.b)).collect::<Vec<_>>(), [(1, 2)], "track 1 from the cache");
    assert_eq!(of(events, Kind::SinkOpen).iter().map(|e| (e.a, e.b)).collect::<Vec<_>>(), [(8000, 2)]);

    // The dump a rig reads back carries the same placement.
    let text = snapshot.text();
    assert!(text.contains(&format!("E {} {} recovery 32 0 1\n", recovery.seq, recovery.t_us)));
    assert!(text.contains(&format!("E {} {} slow-read {} 0 1\n", slow.seq, slow.t_us, slow.a)));
}

#[test]
fn pause_resume_and_skip_requests_reach_their_named_endpoints_in_order() {
    let (trace, p) = player(Files { audio: wav(1.5), stall: None });
    p.send(Cmd::Play { songs: std::sync::Arc::new(vec![song("first"), song("second")]), index: 0, shuffle: false });
    let playing = wait_for(&p, 5, |s| s.state == PlayState::Playing && s.position_ms > 100);
    assert_eq!(playing.state, PlayState::Playing);
    p.send(Cmd::Pause);
    assert_eq!(wait_for(&p, 5, |s| s.state == PlayState::Paused).state, PlayState::Paused);
    p.send(Cmd::PlayPause);
    assert_eq!(wait_for(&p, 5, |s| s.state == PlayState::Playing).state, PlayState::Playing);
    p.send(Cmd::SetVolume(40)); // traced volume serial; does not invent first PCM/submission
    p.send(Cmd::Next);
    let second = wait_for(&p, 5, |s| s.index == 1 && s.state == PlayState::Playing);
    assert_eq!((second.index, second.state), (1, PlayState::Playing));
    assert!(wait_event(&trace, 8, |events| events.iter().any(|e| e.kind == Kind::TrackEnd && e.b == 1)), "the second song plays to its end");
    p.shutdown();
    let events = trace.snapshot().events;
    let pairs = |kind: Kind| of(&events, kind).iter().map(|e| (e.a, e.b)).collect::<Vec<_>>();
    // Play 1, Pause 4, PlayPause 2, Volume 10, Next 5: numbered identically where
    // they were sent and where the player applied them.
    assert_eq!(pairs(Kind::Request), [(1, 1), (4, 2), (2, 3), (10, 4), (5, 5)]);
    assert_eq!(pairs(Kind::Handled), [(1, 1), (4, 2), (2, 3), (10, 4), (5, 5)]);
    for serial in 1..=5 {
        let request = events.iter().find(|e| e.kind == Kind::Request && e.b == serial).unwrap();
        let handled = events.iter().find(|e| e.kind == Kind::Handled && e.b == serial).unwrap();
        assert!(request.seq < handled.seq && request.t_us <= handled.t_us);
    }
    let after = |serial: u32| events.iter().find(|e| e.kind == Kind::Handled && e.b == serial).unwrap().seq;
    // Pause: the output's pause returned (serial 2), then the UI status.
    let paused = of(&events, Kind::Paused);
    assert_eq!(paused.iter().map(|e| e.a).collect::<Vec<_>>(), [2]);
    assert!(paused[0].seq > after(2));
    assert!(events.iter().any(|e| e.kind == Kind::Status && e.b == 2 && e.a == 2 && e.seq > paused[0].seq), "paused status after the output pause");
    // Resume: the output's resume returned (serial 3), then the status.
    let resumed = of(&events, Kind::Resumed);
    assert_eq!(resumed.iter().map(|e| e.a).collect::<Vec<_>>(), [3]);
    assert!(events.iter().any(|e| e.kind == Kind::Status && e.b == 1 && e.a == 3 && e.seq > resumed[0].seq));
    let volume_end=events.iter().find(|e|e.kind==Kind::VolumeEnd && e.a==4).unwrap();
    assert_eq!((volume_end.b,volume_end.c),(0,0));
    assert!(!events.iter().any(|e|matches!(e.kind,Kind::FirstPcm|Kind::FirstSubmit) && e.b==4));
    // Skip: the old track closes, the new one opens, decodes, then submits.
    let opened = events.iter().find(|e| e.kind == Kind::Open && e.seq > after(5)).unwrap();
    let track = opened.a;
    let closed = events.iter().find(|e| e.kind == Kind::TrackEnd && e.seq > after(5)).unwrap();
    let pcm = events.iter().find(|e| e.kind == Kind::FirstPcm && e.a == track).unwrap();
    let submit = events.iter().find(|e| e.kind == Kind::FirstSubmit && e.a == track).unwrap();
    assert_eq!((closed.a, closed.b, pcm.b, submit.b), (track - 1, 2, 5, 5), "closed by the skip; completions carry the skip's serial");
    assert!(closed.seq < opened.seq && opened.seq < pcm.seq && pcm.seq < submit.seq && pcm.t_us <= submit.t_us);
    assert_eq!(pairs(Kind::FirstPcm)[0], (1, 1));
    assert_eq!(of(&events, Kind::TrackEnd).last().map(|e| (e.a, e.b)), Some((track, 1)), "the second song ended at end of file");
}

/// The recording path itself, with no model in between: a recovery noted by
/// the sink during a scripted write is taken by `submit_rendered` and lands
/// in the trace with the queue depth observed when that write began.
#[test]
fn a_recovery_inside_a_scripted_write_reaches_the_trace_through_the_real_submit_path() {
        let blocking = crate::rt::Blocking::dedicated_thread();
    use crate::audio::alsa::WriteFixture;
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;
    let fixture = |steps: Vec<Result<usize, String>>, queued: usize| WriteFixture {
        steps: VecDeque::from(steps), accepted: Arc::new(Mutex::new(Vec::new())), queued: Arc::new(AtomicUsize::new(queued)),
        drained: Arc::new(AtomicBool::new(false)), consumed_on_error: VecDeque::from([0]), on_error: None, first_write: None, queue_limit: None,
    };
    let trace = Arc::new(Trace::new(true));
    let mut tap = crate::playtrace::Tap::new(trace.clone());
    tap.opened(2, 0, 0);
    let mut sink = Sink::open(&blocking, FAKE, 8000, 2, 400_000).unwrap();
    let pcm = vec![0i32; 800]; // 400 stereo frames

    // 150 ms queued; the PCM takes the packet in two parts and recovers once.
    sink.script_writes(fixture(vec![Ok(100), Ok(300)], 1200));
    sink.script_recoveries([-32]);
    assert_eq!(submit_rendered(&blocking, &mut sink, &pcm, &mut tap).ok(), Some(400));
    assert!(sink.recovered().0.is_empty() && sink.recovered().1 == 0, "taken by the player");
    assert_eq!(sink.pre_write_delay(), Some(1200));

    // The queue is empty; the endpoint fails after 60 frames, having recovered once.
    sink.script_writes(fixture(vec![Ok(60), Err("Synthetic unplug".into())], 0));
    sink.script_recoveries([-77]);
    let failure = submit_rendered(&blocking, &mut sink, &pcm, &mut tap).err().expect("the write fails");
    assert_eq!((failure.accepted, failure.pending.frames()), (60, 340));
    assert!(sink.recovered().0.is_empty());

    let snapshot = trace.snapshot();
    let recoveries: Vec<_> = of(&snapshot.events, Kind::Recovery).iter().map(|e| (e.a, e.b, e.c)).collect();
    assert_eq!(recoveries, [(32, 150, 1), (77, 0, 1)], "code, audio queued before the write in ms, track");
    assert_eq!(of(&snapshot.events, Kind::FirstSubmit).iter().map(|e| (e.a, e.c)).collect::<Vec<_>>(), [(1, 150)]);
    let stamps: Vec<_> = snapshot.events.iter().map(|e| e.t_us).collect();
    assert!(stamps.iter().all(|t| *t > 0));
    let window = snapshot.current.expect("the open window");
    assert_eq!((window.frames, window.rate, window.buf_min_ms, window.buf_last_ms), (460, 8000, 0, 0), "accepted frames only");
    let summary = tap.end_track(2).unwrap();
    assert_eq!((summary.recoveries, summary.frames, summary.buf_min_ms), (2, 460, 0));
}
