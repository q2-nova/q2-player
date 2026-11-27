//! The real player thread against the queue, gate and output machines
//! (`q2_machines`, docs/state-machines.md): regression tests for defects the
//! machines exposed, and property tests that drive generated command
//! sequences through the thread, the decoder and a fake PCM.
use super::*;
use crate::audio::alsa::{FAKE, Sink, SoftHw};
use std::io::Cursor;

/// 16-bit silence at `rate` with `channels`.
fn wav(secs: f32, rate: u32, channels: u16) -> Vec<u8> {
    let frame = u32::from(channels) * 2;
    let data = (rate as f32 * secs) as u32 * frame;
    let mut v = Vec::new();
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&channels.to_le_bytes());
    v.extend_from_slice(&rate.to_le_bytes());
    v.extend_from_slice(&(rate * frame).to_le_bytes());
    v.extend_from_slice(&(frame as u16).to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data.to_le_bytes());
    v.resize(v.len() + data as usize, 0);
    v
}

/// Serves an 8 kHz stereo WAV for every song, a 16 kHz mono one for IDs
/// starting with "wide" and nothing for IDs starting with "missing", and
/// records what the player asked of the library.
struct Tracks {
    audio: Vec<u8>,
    wide: Vec<u8>,
    opened: Mutex<Vec<String>>,
    saved: Mutex<Vec<(usize, usize, u64)>>,
}

impl Tracks {
    fn new(secs: f32) -> Arc<Self> {
        Arc::new(Tracks { audio: wav(secs, 8_000, 2), wide: wav(secs, 16_000, 1), opened: Mutex::new(Vec::new()), saved: Mutex::new(Vec::new()) })
    }
}

impl TrackSource for Tracks {
    fn open(&self, _blocking: &crate::rt::Blocking, song: &Song, _: &Arc<crate::audio::interrupt::Interrupt>) -> Result<OpenedTrack, String> {
        self.opened.lock().unwrap().push(song.id.clone());
        if song.id.starts_with("missing") { return Err("missing".into()); }
        let audio = if song.id.starts_with("wide") { &self.wide } else { &self.audio };
        Ok(OpenedTrack { source: Box::new(Cursor::new(audio.clone())), ext: Some("wav".into()), origin: Origin::Cached, radio_metadata: None })
    }
    fn upcoming(&self, _: &[Song]) {}
    fn now_playing(&self, _: &Song) {}
    fn played(&self, _: &Song, _: u64) {}
    fn save_queue(&self, queue: &[Song], index: usize, position_ms: u64) {
        self.saved.lock().unwrap().push((queue.len(), index, position_ms));
    }
}

fn song(id: &str) -> Song {
    Song { id: id.into(), title: id.into(), duration: 1, ..Default::default() }
}

fn start(source: Arc<Tracks>) -> PlayerHandle {
    let settings = Settings { eq: Default::default(), volume: 100, headphone_power: crate::config::HeadphonePower::Standard, filter: 0, replay_gain: ReplayGainMode::Off, gapless: true };
    spawn(source, Box::new(SoftHw(FAKE.into())), settings, Arc::new(|| {}))
}

fn wait_for(p: &PlayerHandle, secs: u64, f: impl Fn(&Status) -> bool) -> Status {
    let end = Instant::now() + Duration::from_secs(secs);
    loop {
        let st = p.status();
        if f(&st) || Instant::now() > end { return st; }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Waits until every command sent so far has been handled: a volume marker
/// travels the same channel and is published with the batch it ends.
fn settle(p: &PlayerHandle, marker: u8) -> Status {
    p.send(Cmd::SetVolume(marker));
    let st = wait_for(p, 5, |s| s.volume == marker);
    assert_eq!(st.volume, marker, "the player stopped handling commands");
    // One more pass of the player's loop, so an open that the batch started
    // has failed or begun before the status is read.
    std::thread::sleep(Duration::from_millis(60));
    p.status()
}

#[test]
fn consecutive_missing_tracks_stop_at_the_failure_limit_without_opening_the_fourth() {
    let source = Tracks::new(1.0);
    let p = start(source.clone());
    p.send(Cmd::Play { songs: std::sync::Arc::new((0..5).map(|i| song(&format!("missing{i}"))).collect()), index: 0, shuffle: false });
    let st = wait_for(&p, 5, |s| s.queue.len() == 5 && s.state == PlayState::Stopped);
    assert_eq!((st.state, st.index), (PlayState::Stopped, 2));
    assert_eq!(*source.opened.lock().unwrap(), ["missing0", "missing1", "missing2"]);
    assert!(st.error.is_some());
    p.send(Cmd::SetVolume(37));
    assert_eq!(wait_for(&p, 5, |s| s.volume == 37).volume, 37);
    p.shutdown();
}

#[test]
fn transport_keys_on_an_empty_queue_leave_it_stopped_without_an_error() {
    let p = start(Tracks::new(0.5));
    for (marker, cmd) in [(11, Cmd::Prev), (12, Cmd::SeekBy(5_000)), (13, Cmd::SeekBy(-5_000)),
        (14, Cmd::Scrub { generation: 0, position: Some(1_000), active: false }), (15, Cmd::Next), (16, Cmd::PlayPause)] {
        p.send(cmd);
        let st = settle(&p, marker);
        assert_eq!((st.state, st.error.as_deref()), (PlayState::Stopped, None), "command before marker {marker}");
    }
    p.shutdown();
}

#[test]
fn play_after_an_empty_player_was_held_paused_does_nothing() {
    // The sleep barrier and an output park pause the player whatever its
    // state. With nothing queued, Play afterwards has nothing to resume.
    let p = start(Tracks::new(0.5));
    p.send(Cmd::ParkOutput);
    let st = settle(&p, 21);
    assert!(st.queue.is_empty() && !st.is_active());
    for (marker, cmd) in [(22, Cmd::PlayPause), (23, Cmd::Resume { generation: st.generation })] {
        p.send(cmd);
        let st = settle(&p, marker);
        assert!(!st.is_active(), "an empty queue became active");
        assert_eq!(st.error, None, "an empty queue reported a playback error");
    }
    p.shutdown();
}

// ---------------------------------------------------------------------------
// Property tests: generated command sequences through the real player thread.
// ---------------------------------------------------------------------------

use proptest::prelude::*;
use proptest::test_runner::{Config as PropConfig, RngAlgorithm, TestRng, TestRunner};
use std::sync::atomic::AtomicUsize;

/// A fixed seed: a failure reproduces on every run, and shrinks.
fn runner(cases: u32) -> TestRunner {
    let config = PropConfig { cases, failure_persistence: None, max_shrink_iters: 60, ..PropConfig::default() };
    TestRunner::new_with_rng(config, TestRng::deterministic_rng(RngAlgorithm::ChaCha))
}

#[derive(Debug, Clone, PartialEq)]
enum Hw { Open { rate: u32, channels: u32 }, PowerOff }

/// The paced fake PCM, recording opens and counting accepted frames.
struct Rig { log: Arc<Mutex<Vec<Hw>>>, frames: Arc<AtomicUsize> }

impl AudioHw for Rig {
    fn device(&mut self) -> String { FAKE.into() }
    fn open_sink<'a>(&mut self, blocking: &'a crate::rt::Blocking, device: &str, rate: u32, channels: u32, latency: u32, format:crate::audio::pcm_format::PcmFormat) -> Result<Sink<'a>,String> {
        let mut sink = Sink::open_format(blocking,device,rate,channels,latency,format)?;
        sink.count_frames(self.frames.clone());
        self.log.lock().unwrap().push(Hw::Open { rate, channels });
        Ok(sink)
    }
    fn power_on(&mut self, _blocking: &crate::rt::Blocking) {}
    fn power_off(&mut self, _blocking: &crate::rt::Blocking) { self.log.lock().unwrap().push(Hw::PowerOff); }
    fn mute(&mut self, _blocking: &crate::rt::Blocking, _: bool) {}
    fn set_attenuation(&mut self, _blocking: &crate::rt::Blocking, _: u8) -> Result<bool, String> { Ok(false) }
    fn set_filter(&mut self, _blocking: &crate::rt::Blocking, _: u8) {}
}

struct Session { p: PlayerHandle, source: Arc<Tracks>, log: Arc<Mutex<Vec<Hw>>>, frames: Arc<AtomicUsize>, marker: u8 }

impl Session {
    fn new(secs: f32) -> Self {
        let source = Tracks::new(secs);
        let log = Arc::new(Mutex::new(Vec::new()));
        let frames = Arc::new(AtomicUsize::new(0));
        let settings = Settings { eq: Default::default(), volume: 100, headphone_power: crate::config::HeadphonePower::Standard, filter: 0, replay_gain: ReplayGainMode::Off, gapless: true };
        let p = spawn(source.clone(), Box::new(Rig { log: log.clone(), frames: frames.clone() }), settings, Arc::new(|| {}));
        Session { p, source, log, frames, marker: 0 }
    }

    /// Every command sent so far has been handled and the player's loop has
    /// run at least once since; an open (and any chain of failed opens) the
    /// commands started has finished.
    fn settle(&mut self) -> Result<Status, TestCaseError> {
        for _ in 0..2 {
            self.marker = self.marker % 90 + 1;
            let marker = self.marker;
            self.p.send(Cmd::SetVolume(marker));
            let st = wait_for(&self.p, 5, |s| s.volume == marker);
            prop_assert_eq!(st.volume, marker, "the player stopped handling commands");
        }
        let st = wait_for(&self.p, 5, |s| s.state != PlayState::Buffering);
        prop_assert!(st.state != PlayState::Buffering, "stuck buffering: {:?}", st.error);
        Ok(st)
    }

    /// The invariants every published status satisfies.
    fn check(&self, st: &Status, previous_generation: u64) -> Result<(), TestCaseError> {
        if st.queue.is_empty() {
            prop_assert!(!st.is_active() && st.index == 0, "an empty queue is active or indexed: {:?} {}", st.state, st.index);
        } else {
            prop_assert!(st.index < st.queue.len(), "index {} outside a queue of {}", st.index, st.queue.len());
        }
        prop_assert!(st.error.as_deref() != Some("empty queue"));
        prop_assert!(st.generation >= previous_generation, "the generation went back");
        if st.state == PlayState::Playing {
            let rate = st.sample_rate.expect("a playing song has a decoder");
            let opened = self.log.lock().unwrap().iter().rev().find_map(|e| match e { Hw::Open { rate, .. } => Some(*rate), _ => None });
            prop_assert_eq!(opened, Some(rate), "the PCM is not open at the playing song's rate");
            prop_assert!(st.output_owned);
        }
        Ok(())
    }

    /// No audio is written while the player is stopped or paused.
    fn quiet(&self) -> Result<(), TestCaseError> {
        let before = self.p.status();
        if before.is_active() { return Ok(()); }
        let written = self.frames.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(12));
        let after = self.p.status();
        if !after.is_active() && after.generation == before.generation {
            prop_assert_eq!(self.frames.load(Ordering::Relaxed), written, "audio was written while {:?}", after.state);
        }
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) { self.p.cancel_handoff(); self.p.shutdown(); }
}

#[derive(Debug, Clone)]
enum Op {
    Play { ids: Vec<u8>, index: usize },
    PlayPause,
    Pause,
    Resume { stale: bool },
    Next,
    Prev,
    Jump(usize),
    Insert { ids: Vec<u8>, next: bool },
    Remove(usize),
    Repeat(u8),
    Seek(i64),
    Park,
    Release,
    /// The sleep barrier, then commands that must be dropped, then its cancel.
    Sleep(Vec<u8>),
    /// Let playback run.
    Wait(u8),
}

/// 0..=4 are playable 8 kHz stereo songs, 5 a 16 kHz mono one, 6 does not open.
fn name(code: u8, n: usize) -> String {
    match code { 6 => format!("missing{n}"), 5 => format!("wide{n}"), c => format!("s{c}-{n}") }
}

fn songs(ids: &[u8]) -> Vec<Song> { ids.iter().enumerate().map(|(n, c)| song(&name(*c, n))).collect() }

fn op(playable_only: bool) -> impl Strategy<Value = Op> {
    let id = if playable_only { (0u8..6).boxed() } else { prop_oneof![8 => 0u8..6, 1 => Just(6u8)].boxed() };
    prop_oneof![
        3 => (prop::collection::vec(id.clone(), 0..5), 0usize..7).prop_map(|(ids, index)| Op::Play { ids, index }),
        3 => Just(Op::PlayPause),
        1 => Just(Op::Pause),
        1 => any::<bool>().prop_map(|stale| Op::Resume { stale }),
        4 => Just(Op::Next),
        2 => Just(Op::Prev),
        2 => (0usize..7).prop_map(Op::Jump),
        2 => (prop::collection::vec(id, 0..3), any::<bool>()).prop_map(|(ids, next)| Op::Insert { ids, next }),
        2 => (0usize..7).prop_map(Op::Remove),
        1 => (0u8..3).prop_map(Op::Repeat),
        1 => prop_oneof![Just(-2_000i64), Just(300), Just(900)].prop_map(Op::Seek),
        1 => Just(Op::Park),
        1 => Just(Op::Release),
        1 => prop::collection::vec(0u8..4, 0..3).prop_map(Op::Sleep),
        1 => (1u8..4).prop_map(Op::Wait),
    ]
}

fn repeat(code: u8) -> Repeat { [Repeat::Off, Repeat::All, Repeat::One][code as usize % 3] }

/// The proven queue machine run beside the player: what the queue, the
/// selection and the play state must be once the player has settled.
struct Oracle { ids: Vec<String>, q: qfsm::Queue, repeat: Repeat }

impl Oracle {
    fn apply(&mut self, event: qfsm::Event) -> qfsm::Step {
        let step = qfsm::transition(self.q, event);
        assert!(step.queue.valid());
        self.q = step.queue;
        step
    }
    /// Opening: songs that cannot open fail in turn; the first that opens plays.
    fn open(&mut self) {
        while self.q.play.active() && self.ids.get(self.q.index).is_some_and(|id| id.starts_with("missing")) {
            self.apply(qfsm::Event::Failed { track: qfsm::Track::Music });
        }
        if self.q.play.active() {
            self.apply(qfsm::Event::Opened);
            self.apply(qfsm::Event::Accepted);
        }
    }
}

/// Long songs, so nothing ends by itself: after every command the player's
/// queue, selection and play state equal the queue machine's.
#[test]
fn the_player_thread_refines_the_queue_machine_under_generated_commands() {
    let started = Instant::now();
    let mut runner = runner(24);
    let result = runner.run(&prop::collection::vec(op(false), 1..22), |ops| {
        let mut s = Session::new(10.0);
        let mut oracle = Oracle { ids: Vec::new(), q: qfsm::Queue { len: 0, index: 0, play: PlayState::Stopped, failures: 0 }, repeat: Repeat::Off };
        let mut generation = 0;
        for op in ops {
            let before = s.p.status();
            match &op {
                Op::Play { ids, index } => {
                    let list = songs(ids);
                    oracle.ids = list.iter().map(|x| x.id.clone()).collect();
                    oracle.apply(qfsm::Event::Load { len: list.len(), index: *index, autoplay: true });
                    s.p.send(Cmd::Play { songs: std::sync::Arc::new(list), index: *index, shuffle: false });
                }
                Op::PlayPause => { oracle.apply(qfsm::Event::PlayPause); s.p.send(Cmd::PlayPause); }
                Op::Pause => { oracle.apply(qfsm::Event::Pause); s.p.send(Cmd::Pause); }
                Op::Resume { stale } => {
                    oracle.apply(qfsm::Event::Resume { current: !stale });
                    s.p.send(Cmd::Resume { generation: before.generation.wrapping_add(u64::from(*stale)) });
                }
                Op::Next => { oracle.apply(qfsm::Event::Next { repeat: oracle.repeat }); s.p.send(Cmd::Next); }
                Op::Prev => {
                    // Near the three-second rule the player's own clock decides: leave those out.
                    if (2_000..4_000).contains(&before.position_ms) { continue; }
                    let step = oracle.apply(qfsm::Event::Prev { restart: before.position_ms > 3_000 });
                    if step.effect == qfsm::Effect::Restart { oracle.apply(qfsm::Event::Seek { radio: false }); }
                    s.p.send(Cmd::Prev);
                }
                Op::Jump(i) => { oracle.apply(qfsm::Event::Jump(*i)); s.p.send(Cmd::Jump(*i)); }
                Op::Insert { ids, next } => {
                    let list = songs(ids);
                    if let qfsm::Effect::Insert { at, .. } = oracle.apply(qfsm::Event::Insert { count: list.len(), next: *next }).effect {
                        oracle.ids.splice(at..at, list.iter().map(|x| x.id.clone()));
                    }
                    s.p.send(Cmd::Insert { songs: list, next: *next });
                }
                Op::Remove(i) => {
                    if let qfsm::Effect::Remove(_) = oracle.apply(qfsm::Event::Remove(*i)).effect { oracle.ids.remove(*i); }
                    s.p.send(Cmd::Remove(*i));
                }
                Op::Repeat(code) => { oracle.repeat = repeat(*code); s.p.send(Cmd::SetRepeat(oracle.repeat)); }
                Op::Seek(by) => { oracle.apply(qfsm::Event::Seek { radio: false }); s.p.send(Cmd::SeekBy(*by)); }
                Op::Park => { oracle.apply(qfsm::Event::Held); s.p.send(Cmd::ParkOutput); }
                Op::Release => { oracle.apply(qfsm::Event::Pause); s.p.send(Cmd::Release); }
                Op::Sleep(dropped) => {
                    let prepared = s.p.prepare_sleep();
                    oracle.apply(qfsm::Event::Held);
                    if prepared.is_ok() {
                        let frozen = s.p.status();
                        prop_assert!(!frozen.is_active() && !frozen.output_owned, "asleep with the output open or playing");
                        for code in dropped {
                            s.p.send(match code { 0 => Cmd::Next, 1 => Cmd::PlayPause, 2 => Cmd::Jump(0), _ => Cmd::Remove(0) });
                        }
                        std::thread::sleep(Duration::from_millis(15));
                        let after = s.p.status();
                        prop_assert!(after.same_non_position(&frozen), "a command ran behind the sleep barrier");
                        prop_assert!(s.p.prepare_sleep().is_err(), "the barrier was granted twice");
                    }
                    s.p.cancel_handoff();
                }
                Op::Wait(n) => std::thread::sleep(Duration::from_millis(u64::from(*n) * 4)),
            }
            oracle.open();
            let st = s.settle()?;
            s.check(&st, generation)?;
            generation = st.generation;
            let ids: Vec<String> = st.queue.iter().map(|x| x.id.clone()).collect();
            prop_assert_eq!(&ids, &oracle.ids, "queue contents after {:?}", op);
            prop_assert_eq!(st.index, oracle.q.index, "selection after {:?}", op);
            prop_assert_eq!(st.state, oracle.q.play, "play state after {:?} (error {:?})", op, st.error);
            s.quiet()?;
        }
        // Every position the player asked the library to save names an entry.
        for (len, index, _) in s.source.saved.lock().unwrap().iter() {
            prop_assert!(*index < (*len).max(1), "saved index {} of a queue of {}", index, len);
        }
        prop_assert!(s.source.opened.lock().unwrap().iter().all(|id| oracle_known(id)));
        Ok(())
    });
    if let Err(error) = result { panic!("{error}"); }
    record_time("player-refines-queue-machine", started);
}

/// Short songs that end by themselves, under skip storms and queue edits:
/// positions never go back while a song plays undisturbed, the invariants
/// hold after every command, and the queue's end is published as Stopped
/// with no audio after it.
#[test]
fn short_songs_under_skip_storms_end_stopped_and_silent() {
    let started = Instant::now();
    let mut runner = runner(10);
    let result = runner.run(&prop::collection::vec(op(true), 1..14), |ops| {
        let mut s = Session::new(0.2);
        let mut generation = 0;
        s.p.send(Cmd::Play { songs: std::sync::Arc::new(songs(&[0, 1, 5, 2])), index: 0, shuffle: false });
        for op in ops {
            let before = s.p.status();
            match &op {
                Op::Play { ids, index } => s.p.send(Cmd::Play { songs: std::sync::Arc::new(songs(ids)), index: *index, shuffle: false }),
                Op::PlayPause => s.p.send(Cmd::PlayPause),
                Op::Pause => s.p.send(Cmd::Pause),
                Op::Resume { stale } => s.p.send(Cmd::Resume { generation: before.generation.wrapping_add(u64::from(*stale)) }),
                // A skip storm: the same key several times with no wait.
                Op::Next => for _ in 0..3 { s.p.send(Cmd::Next); },
                Op::Prev => for _ in 0..2 { s.p.send(Cmd::Prev); },
                Op::Jump(i) => s.p.send(Cmd::Jump(*i)),
                Op::Insert { ids, next } => s.p.send(Cmd::Insert { songs: songs(ids), next: *next }),
                Op::Remove(i) => s.p.send(Cmd::Remove(*i)),
                Op::Repeat(code) => s.p.send(Cmd::SetRepeat(repeat(*code))),
                Op::Seek(by) => s.p.send(Cmd::SeekBy(*by / 10)),
                Op::Park => s.p.send(Cmd::ParkOutput),
                Op::Release => s.p.send(Cmd::Release),
                Op::Sleep(_) => { let _ = s.p.prepare_sleep(); s.p.cancel_handoff(); }
                Op::Wait(n) => {
                    // Undisturbed playback of one instance never moves back.
                    let mut last = s.p.status();
                    for _ in 0..*n {
                        std::thread::sleep(Duration::from_millis(8));
                        let now = s.p.status();
                        if now.state == PlayState::Playing && last.state == PlayState::Playing
                            && (now.generation, now.seek_revision, now.index) == (last.generation, last.seek_revision, last.index) {
                            prop_assert!(now.position_ms >= last.position_ms, "position went back: {} -> {}", last.position_ms, now.position_ms);
                        }
                        last = now;
                    }
                }
            }
            // Songs end by themselves here, so only the marker is awaited.
            s.marker = s.marker % 90 + 1;
            let marker = s.marker;
            s.p.send(Cmd::SetVolume(marker));
            let st = wait_for(&s.p, 5, |st| st.volume == marker);
            prop_assert_eq!(st.volume, marker, "the player stopped handling commands");
            s.check(&st, generation)?;
            generation = st.generation;
            s.quiet()?;
        }
        // Run the queue out: repeat off, playing. At most the whole queue
        // remains, well inside the 20 s after which an unpublished stop
        // would surface through the idle release.
        s.p.send(Cmd::SetRepeat(Repeat::Off));
        let st = s.p.status();
        if st.queue.is_empty() {
            prop_assert!(!st.is_active());
            return Ok(());
        }
        if st.state == PlayState::Paused { s.p.send(Cmd::Resume { generation: st.generation }); }
        let budget = 2 + st.queue.len() as u64 / 3;
        let end = wait_for(&s.p, budget, |st| st.state == PlayState::Stopped || (st.state == PlayState::Paused && st.error.is_some()));
        prop_assert!(end.state == PlayState::Stopped || end.error.is_some(), "the queue's end was not published: {:?} at {}/{}", end.state, end.index, end.queue.len());
        s.check(&end, generation)?;
        s.quiet()?;
        Ok(())
    });
    if let Err(error) = result { panic!("{error}"); }
    record_time("player-skip-storms", started);
}

/// Every ID the fixtures generate.
fn oracle_known(id: &str) -> bool { id.starts_with('s') || id.starts_with("wide") || id.starts_with("missing") }

/// Writes a property test's wall time under build/perf for the report.
fn record_time(name: &str, started: Instant) {
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build/perf/property-test-times");
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join(name), format!("{:.2}\n", started.elapsed().as_secs_f64()));
}

// ---------------------------------------------------------------------------
// Boundary validation.
// ---------------------------------------------------------------------------

#[test]
fn replay_gain_from_the_server_never_yields_a_non_finite_or_clipping_factor() {
    assert!((replay_gain_factor(-6.0, None) - 0.501).abs() < 0.001);
    assert_eq!(replay_gain_factor(6.0, Some(1.0)), 1.0, "limited by the peak");
    assert!((replay_gain_factor(6.0, Some(0.5)) - 1.995).abs() < 0.001, "room under the peak is used");
    for db in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] { assert_eq!(replay_gain_factor(db, Some(0.5)), 1.0); }
    for peak in [f32::NAN, f32::INFINITY, 0.0, -1.0] {
        assert!((replay_gain_factor(-6.0, Some(peak)) - 0.501).abs() < 0.001, "a meaningless peak is ignored");
    }
    let mut runner = runner(256);
    let result = runner.run(&(any::<f32>(), proptest::option::of(any::<f32>())), |(db, peak)| {
        let factor = replay_gain_factor(db, peak);
        prop_assert!(factor.is_finite() && factor > 0.0, "{} dB, peak {:?} -> {}", db, peak, factor);
        if let Some(p) = peak.filter(|p| p.is_finite() && *p > 0.0) {
            if db.is_finite() && factor > replay_gain_floor() { prop_assert!(factor * p <= 1.0 + 1e-3); }
        }
        Ok(())
    });
    if let Err(error) = result { panic!("{error}"); }
}

fn replay_gain_floor() -> f32 { 10f32.powf(REPLAY_GAIN_MIN_DB / 20.0) }

/// Review finding 6 (2026-11-21): +60 dB without a peak multiplied by 1000.
#[test]
fn replay_gain_has_a_ceiling_and_boosts_only_under_a_known_peak() {
    let ceiling = 10f32.powf(REPLAY_GAIN_MAX_DB / 20.0);
    assert_eq!(replay_gain_factor(60.0, None), 1.0, "no peak: no boost");
    assert_eq!(replay_gain_factor(1.0e9, None), 1.0, "an overflowing gain without a peak");
    assert!((replay_gain_factor(60.0, Some(0.001)) - ceiling).abs() < 1e-4, "a tiny peak cannot lift the ceiling");
    assert!((replay_gain_factor(-200.0, None) - replay_gain_floor()).abs() < 1e-7, "a damaged tag does not silence");
    assert!((replay_gain_factor(-6.0, Some(1.0e30)) - replay_gain_floor()).abs() < 1e-7, "nor does an absurd peak");
    let mut runner = runner(1024);
    let gains = prop_oneof![-100.0f32..100.0, any::<f32>()];
    let peaks = proptest::option::of(prop_oneof![0.0f32..2.0, any::<f32>()]);
    let result = runner.run(&(gains, peaks, -100.0f32..100.0), |(db, peak, other)| {
        let factor = replay_gain_factor(db, peak);
        prop_assert!(factor <= ceiling * (1.0 + 1e-6) && factor >= replay_gain_floor() * (1.0 - 1e-6),
            "{} dB, peak {:?} -> {} outside the clamp", db, peak, factor);
        let usable = peak.filter(|p| p.is_finite() && *p > 0.0);
        if usable.is_none() { prop_assert!(factor <= 1.0, "{} dB without a peak boosted to {}", db, factor); }
        if db.is_finite() {
            // More gain in the tag never gives a quieter song.
            let (low, high) = if db <= other { (db, other) } else { (other, db) };
            prop_assert!(replay_gain_factor(low, peak) <= replay_gain_factor(high, peak) * (1.0 + 1e-6));
            // Inside the clamp and under the peak, the tag is applied exactly.
            let exact = 10f32.powf(db / 20.0);
            let room = usable.map_or(1.0, |p| 1.0 / p);
            if (REPLAY_GAIN_MIN_DB..=REPLAY_GAIN_MAX_DB).contains(&db) && exact <= room && exact >= replay_gain_floor() {
                prop_assert!((factor - exact).abs() <= exact * 1e-5, "{} dB, peak {:?}: {} != {}", db, peak, factor, exact);
            }
        }
        Ok(())
    });
    if let Err(error) = result { panic!("{error}"); }
}

/// A song whose header declares a format no PCM can be opened for.
struct OddFormat { rate: u32, channels: u16 }

impl TrackSource for OddFormat {
    fn open(&self, _blocking: &crate::rt::Blocking, _: &Song, _: &Arc<crate::audio::interrupt::Interrupt>) -> Result<OpenedTrack, String> {
        Ok(OpenedTrack { source: Box::new(Cursor::new(wav(0.2, self.rate, self.channels))), ext: Some("wav".into()), origin: Origin::Cached, radio_metadata: None })
    }
    fn upcoming(&self, _: &[Song]) {}
    fn now_playing(&self, _: &Song) {}
    fn played(&self, _: &Song, _: u64) {}
    fn save_queue(&self, _: &[Song], _: usize, _: u64) {}
}

#[test]
fn a_format_no_pcm_can_open_ends_playback_with_an_error_and_never_reaches_the_output() {
    for (rate, channels) in [(1_000_000u32, 2u16), (48_000, 12)] {
        let log = Arc::new(Mutex::new(Vec::new()));
        let settings = Settings { eq: Default::default(), volume: 100, headphone_power: crate::config::HeadphonePower::Standard, filter: 0, replay_gain: ReplayGainMode::Off, gapless: true };
        let p = spawn(Arc::new(OddFormat { rate, channels }), Box::new(Rig { log: log.clone(), frames: Arc::new(AtomicUsize::new(0)) }), settings, Arc::new(|| {}));
        p.send(Cmd::Play { songs: std::sync::Arc::new(vec![song("odd")]), index: 0, shuffle: false });
        // The decoder refuses it (stopped) or the output boundary does (held paused).
        let st = wait_for(&p, 5, |s| !s.is_active() && s.error.is_some());
        assert!(!st.is_active(), "{rate} Hz, {channels} ch: still {:?}", st.state);
        assert!(st.error.is_some(), "{rate} Hz, {channels} ch: no error shown");
        assert!(log.lock().unwrap().iter().all(|e| !matches!(e, Hw::Open { .. })), "a PCM was opened for {rate} Hz, {channels} ch");
        p.shutdown();
    }
}

#[test]
fn out_of_range_indices_positions_and_volume_are_clamped_or_ignored() {
    let source = Tracks::new(10.0);
    let p = start(source.clone());
    // An index beyond the queue selects its last entry.
    p.send(Cmd::Play { songs: std::sync::Arc::new(vec![song("s0"), song("s1")]), index: usize::MAX, shuffle: false });
    let st = wait_for(&p, 5, |s| s.state == PlayState::Playing);
    assert_eq!((st.state, st.index), (PlayState::Playing, 1));
    // Indices outside the queue are ignored.
    p.send(Cmd::Jump(2));
    p.send(Cmd::Jump(usize::MAX));
    p.send(Cmd::Remove(usize::MAX));
    let st = settle(&p, 31);
    assert_eq!((st.index, st.queue.len(), st.error.as_deref()), (1, 2, None));
    // Seeks beyond either end stay inside the song.
    p.send(Cmd::SeekBy(i64::MAX));
    let st = settle(&p, 32);
    assert!(st.position_ms <= st.duration_ms && st.duration_ms == 10_000, "{} of {}", st.position_ms, st.duration_ms);
    p.send(Cmd::SeekBy(i64::MIN));
    let st = settle(&p, 33);
    assert!(st.position_ms < 1_000, "{}", st.position_ms);
    p.send(Cmd::Scrub { generation: st.generation, position: Some(u64::MAX), active: false });
    let st = settle(&p, 34);
    assert!(st.position_ms <= st.duration_ms);
    // The volume is capped at 100.
    p.send(Cmd::SetVolume(255));
    let st = wait_for(&p, 5, |s| s.volume == 100);
    assert_eq!(st.volume, 100);
    // A restored position beyond the song's end is held inside it.
    p.send(Cmd::Restore { songs: std::sync::Arc::new(vec![song("s0")]), index: 9, position_ms: u64::MAX });
    let st = wait_for(&p, 5, |s| s.state == PlayState::Paused && s.queue.len() == 1);
    assert_eq!((st.state, st.index), (PlayState::Paused, 0));
    p.send(Cmd::PlayPause);
    let st = wait_for(&p, 5, |s| s.state != PlayState::Paused && s.state != PlayState::Buffering);
    assert!(st.error.is_none() && st.position_ms <= 10_000, "{:?} at {}", st.error, st.position_ms);
    for (len, index, _) in source.saved.lock().unwrap().iter() { assert!(*index < (*len).max(1)); }
    p.shutdown();
}
