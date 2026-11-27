//! One generated file through the real player thread: read-ahead, demux,
//! decode, conversion, gain and the sink.
//!
//! Scenes:
//! - `fake`: the paced fake sink (real time, no libasound);
//! - `file`: an explicitly owned ALSA file PCM (`q2-audio-path-file`, unpaced);
//!   the result carries the MD5 of the bytes the sink accepted;
//! - `native`: `hw:Q2,0` on the Q2 in an allocated test slot, the installed
//!   player's PCM closed. Volume 0 by default, so nothing is audible.
//!
//! `codec` makes the generated backend accept codec attenuation, as the Q2's
//! DACs do, so the hardware-volume path runs without the device. `gain_db`
//! plays the song with that ReplayGain track gain.
use super::*;
use crate::audio::{alsa::{AudioHw, MainlineHw, FAKE}, player::{self, Cmd, Origin, OpenedTrack, PlayState, TrackSource}};
use subsonic::Song;

const FILE_PCM: &str = "q2-audio-path-file";

struct Source(PathBuf);
impl TrackSource for Source {
    fn open(&self, blocking: &crate::rt::Blocking, _: &Song, interrupt: &Arc<Interrupt>) -> Result<OpenedTrack, String> {
        // The same reader a cached or downloaded song gets (library/streaming.rs).
        let file = open_fixture(blocking, &self.0)?;
        let source = crate::audio::readahead::ReadAhead::source(blocking, file, interrupt.clone()).map_err(|e| e.to_string())?;
        Ok(OpenedTrack { source, ext: self.0.extension().and_then(|s| s.to_str()).map(str::to_owned), origin: Origin::Cached, radio_metadata: None })
    }
    fn upcoming(&self, _: &[Song]) {}
    fn now_playing(&self, _: &Song) {}
    fn played(&self, _: &Song, _: u64) {}
    fn save_queue(&self, _: &[Song], _: usize, _: u64) {}
}

/// A generated backend: the named PCM, and codec attenuation accepted or not.
struct Generated { device: String, codec: bool }
impl AudioHw for Generated {
    fn device(&mut self) -> String { self.device.clone() }
    fn power_on(&mut self, _: &crate::rt::Blocking) {}
    fn power_off(&mut self, _: &crate::rt::Blocking) {}
    fn mute(&mut self, _: &crate::rt::Blocking, _: bool) {}
    fn set_attenuation(&mut self, _: &crate::rt::Blocking, _: u8) -> Result<bool, String> { Ok(self.codec) }
    fn set_filter(&mut self, _: &crate::rt::Blocking, _: u8) {}
}

/// The Q2's DACs, leaving the owner's Headphone Power and DAC Filter as the
/// installed player set them. That player writes its own attenuation again
/// before it next opens the PCM.
struct Native(MainlineHw);
impl AudioHw for Native {
    fn device(&mut self) -> String { self.0.device() }
    fn power_on(&mut self, blocking: &crate::rt::Blocking) { self.0.power_on(blocking) }
    fn power_off(&mut self, blocking: &crate::rt::Blocking) { self.0.power_off(blocking) }
    fn mute(&mut self, blocking: &crate::rt::Blocking, on: bool) { self.0.mute(blocking, on) }
    fn set_attenuation(&mut self, blocking: &crate::rt::Blocking, steps: u8) -> Result<bool, String> { self.0.set_attenuation(blocking, steps) }
    fn set_filter(&mut self, _: &crate::rt::Blocking, _: u8) {}
}

/// A bounded file's text through the crate's existing reader.
fn text(blocking: &crate::rt::Blocking, path: &Path) -> Option<String> {
    String::from_utf8(crate::resources::read(blocking, path, 65536).ok()?).ok()
}

/// Per-thread CPU time and context switches of this process, by thread id.
/// Thread ids follow the process id; the benchmark starts under ten threads.
fn threads(blocking: &crate::rt::Blocking) -> std::collections::BTreeMap<u32, (String, u64, u64, u64)> {
    let mut out = std::collections::BTreeMap::new();
    let pid = std::process::id();
    for tid in pid..pid.saturating_add(512) {
        let base = PathBuf::from(format!("/proc/self/task/{tid}"));
        let Some(comm) = text(blocking, &base.join("comm")) else { continue; };
        // schedstat needs CONFIG_SCHED_INFO, which the Q2's kernel lacks; the
        // scheduler's debug file carries the same runtime there.
        let run_ns = text(blocking, &base.join("schedstat")).and_then(|v| v.split_whitespace().next().and_then(|v| v.parse().ok()))
            .or_else(|| text(blocking, &base.join("sched")).and_then(|v| sched_runtime_ns(&v))).unwrap_or(0);
        let status = text(blocking, &base.join("status")).unwrap_or_default();
        let field = |name: &str| status.lines().find_map(|line| line.strip_prefix(name)).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
        out.insert(tid, (comm.trim().to_owned(), run_ns, field("voluntary_ctxt_switches:"), field("nonvoluntary_ctxt_switches:")));
    }
    out
}

fn io_counters(blocking: &crate::rt::Blocking) -> Value {
    let text = text(blocking, Path::new("/proc/self/io")).unwrap_or_default();
    let field = |name: &str| text.lines().find_map(|line| line.strip_prefix(name)).and_then(|v| v.trim().parse::<u64>().ok());
    json!({"rchar": field("rchar:"), "syscr": field("syscr:"), "syscw": field("syscw:"), "read_bytes": field("read_bytes:")})
}

pub(super) fn run(blocking: &crate::rt::Blocking, spec: &Spec) -> Result<Value, String> {
    if !matches!(spec.scene.as_str(), "fake" | "file" | "native") || spec.volume > 100 || !spec.gain_db.is_finite() {
        return Err("playback needs scene fake/file/native, volume 0..100 and a finite gain".into());
    }
    let capture = spec.scratch.join("pcm.raw");
    let hardware: Box<dyn AudioHw> = match spec.scene.as_str() {
        "native" => {
            // Another owner's open PCM keeps its codec attenuation: touch the
            // mixer only when the Q2's playback PCM is closed.
            let state = text(blocking, Path::new("/proc/asound/Q2/pcm0p/sub0/status")).ok_or("Q2 PCM status is unreadable")?;
            if state.trim() != "closed" { return Err("The Q2 PCM is open: pause the installed player for 25 s first".into()); }
            crate::audio::mixer::prepare_q2_playback()?;
            Box::new(Native(MainlineHw("hw:Q2,0".into())))
        }
        "file" => {
            if std::env::var_os("ALSA_CONFIG_PATH").is_none() { return Err("The file scene needs its owned ALSA_CONFIG_PATH".into()); }
            Box::new(Generated { device: FILE_PCM.into(), codec: spec.codec })
        }
        _ => Box::new(Generated { device: FAKE.into(), codec: spec.codec }),
    };
    let duration = spec.radio_seconds.clamp(1, 600);
    let replay_gain = (spec.gain_db != 0.0).then(|| subsonic::ReplayGain { track_gain: Some(spec.gain_db), ..Default::default() });
    let song = Song { id: "generated-audio-path".into(), title: "Generated audio path fixture".into(), duration: duration as u32, replay_gain, ..Default::default() };
    let settings = player::Settings { eq: Default::default(), volume: spec.volume, headphone_power: crate::config::HeadphonePower::Standard, filter: 0,
        replay_gain: if spec.gain_db != 0.0 { crate::config::ReplayGainMode::Track } else { crate::config::ReplayGainMode::Off }, gapless: true };
    let player = player::spawn(Arc::new(Source(spec.fixture.clone())), hardware, settings, Arc::new(|| {}));
    let (before, io_before, began) = (threads(blocking), io_counters(blocking), Instant::now());
    player.send(Cmd::Play { songs: std::sync::Arc::new(vec![song]), index: 0, shuffle: false });
    let mut played = false;
    let outcome = loop {
        let status = player.status();
        if let Some(error) = status.error { break Err(error); }
        played |= status.state == PlayState::Playing;
        if played && status.state == PlayState::Stopped { break Ok(status); }
        if began.elapsed() > Duration::from_secs(duration + 60) { break Err("Generated playback timed out".into()); }
        std::thread::sleep(Duration::from_millis(20));
    };
    // The player thread is still alive here: read it before the join.
    let (after, io_after, wall_us) = (threads(blocking), io_counters(blocking), us(began));
    player.shutdown();
    let status = outcome?;
    let mut rows = Vec::new();
    let (mut total_ns, mut player_ns, mut player_wakeups) = (0u64, 0u64, 0u64);
    for (tid, (comm, run_ns, voluntary, involuntary)) in &after {
        let (_, run0, voluntary0, involuntary0) = before.get(tid).cloned().unwrap_or_default();
        let cpu_ns = run_ns.saturating_sub(run0);
        total_ns += cpu_ns;
        if comm == "player" { player_ns += cpu_ns; player_wakeups += voluntary.saturating_sub(voluntary0); }
        rows.push(json!({"comm": comm, "cpu_us": cpu_ns / 1000, "voluntary": voluntary.saturating_sub(voluntary0), "involuntary": involuntary.saturating_sub(involuntary0)}));
    }
    let sample_md5 = if spec.scene == "file" {
        let bytes = crate::resources::read(blocking, &capture, 64 * 1024 * 1024).map_err(|e| format!("captured PCM: {e}"))?;
        Some((format!("{:x}", Md5::digest(&bytes)), bytes.len()))
    } else { None };
    Ok(json!({"scene": spec.scene, "volume": spec.volume, "codec": spec.codec, "gain_db": spec.gain_db,
        "wall_us": wall_us, "duration_ms": status.duration_ms, "sample_rate": status.sample_rate,
        "player_cpu_us": player_ns / 1000, "player_wakeups": player_wakeups, "threads_cpu_us": total_ns / 1000,
        "threads": rows, "io_before": io_before, "io_after": io_after,
        "sample_md5": sample_md5.as_ref().map(|v| v.0.clone()), "sample_bytes": sample_md5.map(|v| v.1)}))
}
