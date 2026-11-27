//! Generated endpoint workload. Hardware is opt-in through a separate CLI;
//! ordinary --bench never opens ALSA. No owner configuration is loaded.
use crate::audio::{alsa::{AudioHw, MainlineHw, SoftHw, FAKE}, player::{self, Cmd, PlayState}};
use serde::Deserialize;
use serde_json::json;
use std::{path::Path, sync::Arc, time::{Duration, Instant}};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Spec {
    audio_url: String,
    metadata_url: String,
    proxy: Option<String>,
    scratch: std::path::PathBuf,
    seconds: u64,
}
struct Source {
    audio: String,
    endpoint: String,
    proxy: Option<String>,
}
impl player::TrackSource for Source {
    fn open(&self, _blocking: &crate::rt::Blocking, _: &subsonic::Song, interrupt: &Arc<crate::audio::interrupt::Interrupt>) -> Result<player::OpenedTrack,String> {
        let radio = crate::audio::radio::Radio::open_fixture(self.audio.clone(), self.proxy.clone(), interrupt.clone(), self.endpoint.clone());
        Ok(player::OpenedTrack::radio(radio))
    }
    fn upcoming(&self, _: &[subsonic::Song]) {}
    fn now_playing(&self, _: &subsonic::Song) {}
    fn played(&self, _: &subsonic::Song, _: u64) {}
    fn save_queue(&self, _: &[subsonic::Song], _: usize, _: u64) {}
}
fn marker(name: &str, value: serde_json::Value) {
    println!("RADIO_FIXTURE {name} {value}");
}
pub(crate) fn run(blocking: &crate::rt::Blocking, path: &Path, pcm: bool) -> Result<(),String> {
    let spec: Spec = super::read_spec(blocking, path)?;
    if !(30..=45).contains(&spec.seconds) || !spec.audio_url.starts_with("http://") || !spec.metadata_url.starts_with("http://") {
        return Err("radio fixture requires generated HTTP endpoints and30..45 seconds".into());
    }
    // The maintained producer supplies an exclusively owned empty scratch.
    // Markers share captured stdout, avoiding extra files and sampler owners.
    crate::runtime_log::init(&spec.scratch.join("player.log"));
    crate::metrics::activate();
    crate::runtime_log::start();
    crate::rt::io();
    subsonic::http::set_executor(|task| { crate::rt::io().spawn(task); });
    let source = Arc::new(Source {audio:spec.audio_url,endpoint:spec.metadata_url,proxy:spec.proxy});
    let hw: Box<dyn AudioHw> = if pcm {Box::new(MainlineHw("hw:Q2,0".into()))} else {Box::new(SoftHw(FAKE.into()))};
    let settings = player::Settings {eq:Default::default(),volume:0,headphone_power: crate::config::HeadphonePower::Standard,filter:0,replay_gain:crate::config::ReplayGainMode::Off,gapless:true};
    let player = player::spawn(source.clone(),hw,settings,Arc::new(||{}));
    player.send(Cmd::Play {songs:std::sync::Arc::new(vec![subsonic::Song {id:"generated-radio-metadata".into(),radio_url:Some(source.audio.clone()),..Default::default()}]),index:0,shuffle:false});
    let start = Instant::now();
    let mut first = None;
    let mut settled = false;
    let mut title_seen = false;
    let runtime = crate::rt::Ui::new().map_err(|e|e.to_string())?;
    let result = runtime.block_on(async {
        loop {
            let state = player.status();
            if let Some(error) = state.error { return Err(error); }
            title_seen |= state.radio_title.is_some();
            if state.state == PlayState::Playing && state.position_ms >= 1000 && first.is_none() {
                first = Some(start.elapsed().as_micros() as u64);
                marker("started.json",json!({"pid":std::process::id(),"t_us":crate::playtrace::boottime_us(),"position_ms":state.position_ms,"pcm":pcm}));
            }
            if first.is_some() && start.elapsed() >= Duration::from_secs(12) && !settled {
                marker("settled.json",json!({"pid":std::process::id(),"t_us":crate::playtrace::boottime_us(),"position_ms":state.position_ms,"title_seen":title_seen}));
                settled = true;
            }
            if first.is_some() && start.elapsed() >= Duration::from_secs(spec.seconds) {
                if state.state != PlayState::Playing || state.position_ms < (spec.seconds-5)*1000 { return Err("radio playback did not progress".into()); }
                return Ok(state.position_ms);
            }
            if start.elapsed() > Duration::from_secs(spec.seconds+15) { return Err("radio fixture timed out".into()); }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    });
    let close = Instant::now();
    player.shutdown();
    let shutdown_us = close.elapsed().as_micros() as u64;
    let position_ms = result?;
    marker("closed.json",json!({"pid":std::process::id(),"t_us":crate::playtrace::boottime_us(),"position_ms":position_ms,"first_progress_us":first,"shutdown_us":shutdown_us,"title_seen":title_seen,"pcm":pcm}));
    println!("{}",json!({"passed":title_seen,"position_ms":position_ms,"first_progress_us":first,"shutdown_us":shutdown_us,"title_seen":title_seen,"pcm":pcm}));
    if !title_seen { return Err("generated metadata was not observed".into()); }
    Ok(())
}
