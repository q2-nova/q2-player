//! Generated-file experiment through the real player and its range reader.
//! `fake` is clock-model output; `native` requires an allocated Q2 test slot.
use super::*;
use crate::audio::{alsa::{SoftHw, FAKE}, player::{self, Cmd, Origin, OpenedTrack, PlayState, TrackSource}};
use std::sync::atomic::AtomicUsize;
use subsonic::{Song, Spoken};

struct Source {
    path: PathBuf, total: u64, speed: u16, opens: AtomicUsize,
    requests: Arc<AtomicUsize>, gate: Arc<Mutex<Option<Instant>>>,
}
impl TrackSource for Source {
    fn open(&self, _blocking: &crate::rt::Blocking, _: &Song, cancel: &Arc<crate::audio::interrupt::Interrupt>) -> Result<OpenedTrack, String> {
        self.opens.fetch_add(1, Ordering::Relaxed);
        let path = self.path.clone();
        let requests = self.requests.clone();
        let gate = self.gate.clone();
        let stop = cancel.clone();
        let reader = crate::audio::source::RangeReader::with_async_window(self.total, cancel.clone(), Arc::new(move |offset, length| {
            let (requests,gate,stop,path)=(requests.clone(),gate.clone(),stop.clone(),path.clone());
            Box::pin(async move {
                requests.fetch_add(1, Ordering::Relaxed);
                while gate.lock().unwrap().is_some_and(|deadline| Instant::now() < deadline) {
                    if stop.load(Ordering::Relaxed) { return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "Generated range cancelled")); }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                crate::rt::spawn_blocking(move |_| {
                    let mut file=std::fs::File::open(&path)?;
                    file.seek(SeekFrom::Start(offset))?;
                    let mut bytes=Vec::new(); file.take(length as u64).read_to_end(&mut bytes)?;
                    Ok(Some(bytes))
                }).await.map_err(|_|std::io::Error::other("fixture reader stopped"))?
            })
        }), crate::audio::source::spoken_window_bytes(self.total, 120));
        Ok(OpenedTrack { source: Box::new(reader), ext: self.path.extension().and_then(|s|s.to_str()).map(str::to_owned), origin: Origin::Streaming, radio_metadata: None })
    }
    fn upcoming(&self, _: &[Song]) {} fn now_playing(&self, _: &Song) {}
    fn played(&self, _: &Song, _: u64) {} fn save_queue(&self, _: &[Song], _: usize, _: u64) {}
    fn spoken_skip(&self) -> crate::config::SpokenSettings { crate::config::SpokenSettings { speed_percent: self.speed, ..Default::default() } }
}
fn wait(_blocking: &crate::rt::Blocking, player: &player::PlayerHandle, predicate: impl Fn(&player::Status)->bool) -> Result<player::Status,String> {
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        let status = player.status();
        if let Some(error) = status.error { return Err(error); }
        if predicate(&status) { return Ok(status); }
        if Instant::now() >= deadline { return Err(format!("Generated pause fixture timed out ({:?}, {} ms)", status.state,status.position_ms)); }
        std::thread::sleep(Duration::from_millis(5));
    }
}
pub(super) fn run(blocking: &crate::rt::Blocking, spec: &Spec) -> Result<Value,String> {
    if !matches!(spec.scene.as_str(), "fake" | "native") || !(50..=300).contains(&spec.speed_percent) || spec.speed_percent % 5 != 0 {
        return Err("pause fixture needs fake/native scene and speed 50..300 in steps of 5".into());
    }
    let total = std::fs::metadata(&spec.fixture).map_err(|e|e.to_string())?.len();
    if total == 0 || total > 32*1024*1024 { return Err("Generated pause file must be 1..32 MiB, 120 seconds".into()); }
    let source = Arc::new(Source { path:spec.fixture.clone(), total, speed:spec.speed_percent, opens:0.into(), requests:Arc::new(0.into()), gate:Arc::new(Mutex::new(None)) });
    let song = Song { id:"generated-pause".into(),title:"Generated pause fixture".into(),duration:120,
        spoken:Some(Box::new(Spoken { id:"generated-pause-book".into(),duration_ms:120000,..Default::default() })),..Default::default() };
    if spec.scene == "native" { crate::audio::mixer::prepare_q2_playback()?; }
    let hardware: Box<dyn crate::audio::alsa::AudioHw> = if spec.scene == "native" {
        Box::new(crate::audio::alsa::MainlineHw("hw:Q2,0".into()))
    } else { Box::new(SoftHw(FAKE.into())) };
    let player = player::spawn(source.clone(),hardware,
        player::Settings {eq:Default::default(), volume:40,headphone_power: crate::config::HeadphonePower::Standard,filter:0,replay_gain:crate::config::ReplayGainMode::Off,gapless:false},Arc::new(||{}));
    let result = (|| {
        player.send(Cmd::Play {songs:std::sync::Arc::new(vec![song]),index:0,shuffle:false});
        let active = wait(blocking, &player,|s|s.state==PlayState::Playing && s.position_ms>=350)?;
        if active.speed_percent!=spec.speed_percent { return Err("Wrong effective fixture speed".into()); }
        let mut pauses=Vec::new();
        for (index,pause_ms) in [150,21000,150].into_iter().enumerate() {
            player.send(if index==2 {Cmd::ParkOutput} else {Cmd::Pause});
            let paused=wait(blocking, &player,|s|s.state==PlayState::Paused)?;
            std::thread::sleep(Duration::from_millis(pause_ms));
            if player.status().position_ms!=paused.position_ms { return Err("Pause advanced audible position".into()); }
            // Existing prefetch remains valid; new range requests cannot finish
            // for seven seconds. Verify five seconds of buffered playback before
            // releasing the gate. No socket/owner credentials are involved.
            let resumed_at=Instant::now();
            *source.gate.lock().unwrap()=Some(resumed_at+Duration::from_secs(7));
            let before=source.requests.load(Ordering::Relaxed);
            player.send(Cmd::PlayPause);
            let resumed=wait(blocking, &player,|s|s.state==PlayState::Playing)?;
            let status_us=us(resumed_at);
            let progressed=wait(blocking, &player,|s|s.position_ms>=paused.position_ms+u64::from(spec.speed_percent)*30)?;
            let progress_us=us(resumed_at);
            if progress_us>4_500_000 { return Err(format!("Playback progress exceeded 4.5-second criterion: {progress_us} us; source opens={}, range requests={before}->{}",source.opens.load(Ordering::Relaxed),source.requests.load(Ordering::Relaxed))); }
            let covered=wait(blocking, &player,|s|s.position_ms>=paused.position_ms+u64::from(spec.speed_percent)*50)?;
            let coverage_us=us(resumed_at);
            if coverage_us>6_500_000 { return Err(format!("Five-second buffer coverage exceeded 6.5-second criterion: {coverage_us} us")); }
            if source.opens.load(Ordering::Relaxed)!=1 { return Err("Pause reopened media source".into()); }
            pauses.push(json!({"pause_ms":pause_ms,"paused_position_ms":paused.position_ms,"resumed_position_ms":resumed.position_ms,
                "progress_position_ms":progressed.position_ms,"playing_status_us":status_us,"three_seconds_progress_us":progress_us,"five_seconds_progress_us":coverage_us,"covered_position_ms":covered.position_ms,
                "requests_before_resume":before,"requests_at_progress":source.requests.load(Ordering::Relaxed)}));
            *source.gate.lock().unwrap()=None;
        }
        Ok(json!({"output":spec.scene,"physical_sound_verified":false,"opens":source.opens.load(Ordering::Relaxed),
            "speed_percent":spec.speed_percent,"range_window_bytes":crate::audio::source::spoken_window_bytes(total,120),"pauses":pauses}))
    })();
    *source.gate.lock().unwrap()=None;
    player.shutdown();
    result
}
