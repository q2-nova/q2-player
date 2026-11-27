#![forbid(unsafe_code)]
//! Isolated, deterministic workloads. Never reads user configuration or opens
//! audio hardware. Run with --bench SPEC.json; one process per measurement.
use crate::audio::source::{Progress, ProgressiveReader};
use serde::Deserialize;
use serde_json::{json, Value};
use std::{fs::File, io::{Read, Seek, SeekFrom, Write}};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::sync::atomic::Ordering;
use crate::audio::interrupt::Interrupt;
use std::time::{Duration, Instant};
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::io::MediaSource;
use md5::{Digest, Md5};
mod artwork_storage;
mod book_ranges;
mod convert;
mod jpeg_check;
mod library;
mod lossless_check;
mod mp3_check;
mod offline_download;
mod playback;
mod spoken_pause;
pub(crate) mod radio_metadata;

#[derive(Deserialize)]
#[serde(default)]
pub struct Spec {
    pub kind: String,
    pub url: String,
    pub proxy: Option<String>,
    pub api_token: String,
    pub radio_seconds: u64,
    pub fixture: PathBuf,
    pub scratch: PathBuf,
    pub bytes_per_second: u64,
    pub chunk_bytes: usize,
    pub first_byte_ms: u64,
    pub known_length: bool,
    pub legacy_seekable: bool,
    pub iterations: usize,
    pub tile: usize,
    pub tiles: Vec<usize>,
    pub scene: String,
    pub cadence_ms: u64,
    pub size: u32,
    pub seek_ms: Vec<u64>,
    pub speed_percent: u16,
    pub quality_volume: u8,
    pub volume: u8,
    pub codec: bool,
    pub gain_db: f32,
}
impl Default for Spec {
    fn default() -> Self {
        Self { kind: "startup".into(), url: String::new(), proxy: None, api_token:String::new(), radio_seconds: 2, fixture: PathBuf::new(), scratch: PathBuf::from("build/perf/scratch"),
            bytes_per_second: 65536, chunk_bytes: 4096, first_byte_ms: 50, known_length: true,
            legacy_seekable: false, iterations: 200, tile: 0, tiles: Vec::new(), scene: "list".into(), cadence_ms: 50, size: 150, speed_percent: 100, quality_volume: 80, volume: 0, codec: false, gain_db: 0.0, seek_ms: vec![0,5000,11000,1000,11000,0] }
    }
}
#[derive(Default, serde::Serialize)]
struct Reads { calls: usize, bytes: u64, seeks: usize, max_seek: u64, wait_us: u64 }
struct Observed {
    source: ProgressiveReader,
    reads: Arc<Mutex<Reads>>,
    legacy: bool,
}
impl Read for Observed {
    fn read(&mut self, b: &mut [u8]) -> std::io::Result<usize> {
        let t = Instant::now();
        let result = self.source.read(b);
        let mut r = self.reads.lock().unwrap();
        r.calls += 1; r.wait_us += t.elapsed().as_micros() as u64;
        if let Ok(n) = result { r.bytes += n as u64; }
        result
    }
}
impl Seek for Observed {
    fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
        let out = self.source.seek(p)?;
        let mut r = self.reads.lock().unwrap();
        r.seeks += 1; r.max_seek = r.max_seek.max(out);
        Ok(out)
    }
}
impl MediaSource for Observed {
    fn is_seekable(&self) -> bool { self.legacy || self.source.is_seekable() }
    fn byte_len(&self) -> Option<u64> { self.source.byte_len() }
}
fn us(t: Instant) -> u64 { t.elapsed().as_micros() as u64 }
fn usage() -> libc::rusage {crate::platform::system::process_usage()}

fn cpu(r: &libc::rusage) -> i64 {
    (r.ru_utime.tv_sec as i64 + r.ru_stime.tv_sec as i64)*1_000_000 + r.ru_utime.tv_usec as i64 + r.ru_stime.tv_usec as i64
}
fn decode(blocking: &crate::rt::Blocking, source: Box<dyn MediaSource>, spec: &Spec, all: bool, start: Instant, progress: Option<&Progress>) -> Result<Value, String> {
    let mut format = crate::audio::decoder::probe(blocking, source,spec.fixture.extension().and_then(|s|s.to_str()),
        !all && !spec.legacy_seekable && progress.is_some_and(|p|!p.snapshot().done)).map_err(|e|e.to_string())?.format;
    let probe_us = us(start);
    let track = format.tracks().iter().find(|t| t.codec_params.codec != CODEC_TYPE_NULL).ok_or("no audio")?;
    let id = track.id;
    let params = track.codec_params.clone();
    let mut decoder = symphonia::default::get_codecs().make(&params, &DecoderOptions::default()).map_err(|e| e.to_string())?;
    let mut frames = 0usize;
    let mut packets = 0usize;
    let mut decode_us = 0u64;
    let mut conversion_us = 0u64;
    let mut first = None;
    let mut hash = Md5::new();
    let mut conversion = crate::audio::decoder::Samples::default();
    loop {
        let packet = match crate::audio::callback::with(blocking, || format.next_packet()) {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.to_string()),
        };
        if packet.track_id() != id { continue; }
        let t = Instant::now();
        let decoded = decoder.decode(&packet).map_err(|e| e.to_string())?;
        decode_us += us(t);
        if decoded.frames() == 0 { continue; }
        frames += decoded.frames(); packets += 1;
        let t = Instant::now();
        // The exact same reusable conversion as the player, at non-unity gain.
        let samples = conversion.convert(decoded,0.31622776);
        conversion_us += us(t);
        if first.is_none() {
            let fill = progress.map(Progress::snapshot);
            first = Some(json!({"first_pcm_us":us(start), "bytes_available":fill.as_ref().map(|p|p.written),
                "download_complete":fill.as_ref().map(|p|p.done)}));
        }
        for s in samples { hash.update(s.to_le_bytes()); }
        if !all && (spec.kind != "radio" || frames >= params.sample_rate.unwrap_or(48000) as usize * 2) { break; }
    }
    #[allow(unused_mut)]
    let mut out = json!({"probe_us":probe_us,"first":first,"frames":frames,"packets":packets,
        "decode_us":decode_us,"conversion_us":conversion_us,"sample_md5":format!("{:x}",hash.finalize()),
        "audio_seconds":frames as f64 / params.sample_rate.unwrap_or(1) as f64});
    // Stage timers of the vendored MP3 decoder, only in a rig variant built
    // with `--cfg q2_mp3_profile` (crates/vendor/symphonia-bundle-mp3/README.q2.md).
    #[cfg(q2_mp3_profile)]
    {
        let (ns, calls, empty_ns) = symphonia_bundle_mp3::q2::profile_take();
        out["mp3_profile"] = json!({"ns":ns,"calls":calls,"empty_ns":empty_ns});
    }
    // The same for the vendored FLAC and ALAC decoders (`--cfg q2_flac_profile`,
    // `--cfg q2_alac_profile`; their README.q2.md).
    #[cfg(q2_flac_profile)]
    {
        let (ns, calls, empty_ns) = symphonia_bundle_flac::q2::profile_take();
        out["flac_profile"] = json!({"ns":ns,"calls":calls,"empty_ns":empty_ns});
    }
    #[cfg(q2_alac_profile)]
    {
        let (ns, calls, empty_ns) = symphonia_codec_alac::q2::profile_take();
        out["alac_profile"] = json!({"ns":ns,"calls":calls,"empty_ns":empty_ns});
    }
    Ok(out)
}
fn startup(blocking: &crate::rt::Blocking, spec: &Spec) -> Result<Value, String> {
    if spec.chunk_bytes == 0 || spec.chunk_bytes > 1024*1024 || spec.bytes_per_second == 0 { return Err("invalid throttle".into()); }
    std::fs::create_dir_all(&spec.scratch).map_err(|e|e.to_string())?;
    let path = spec.scratch.join(format!("stream-{}.part",std::process::id()));
    let writer = std::fs::File::create(&path).map_err(|e|e.to_string())?;
    let fixture = std::fs::File::open(&spec.fixture).map_err(|e|e.to_string())?;
    let total = fixture.metadata().map_err(|e|e.to_string())?.len();
    let progress = Progress::new();
    if spec.known_length { progress.set_total(Some(total)); }
    let stop = Arc::new(Interrupt::new(false));
    let reader = ProgressiveReader::open(blocking, &path,progress.clone(),stop.clone()).map_err(|e|e.to_string())?;
    let reads = Arc::new(Mutex::new(Reads::default()));
    let source = Observed { source:reader,reads:reads.clone(),legacy:spec.legacy_seekable };
    let (rate, chunk, delay) = (spec.bytes_per_second,spec.chunk_bytes,spec.first_byte_ms);
    let (p,s) = (progress.clone(),stop.clone());
    let start = Instant::now();
    let cancelled=Arc::new(tokio::sync::Notify::new());
    let cancellation=cancelled.clone();
    let (finished,worker)=std::sync::mpsc::sync_channel(1);
    let writer_future=throttled_writer(fixture,writer,chunk,rate,delay,start,p.clone(),s,cancellation);
    crate::rt::io().spawn(async move {
        let result=writer_future.await;
        if let Err(error)=&result {p.fail(error.to_string());}
        let _=finished.try_send(result);
    });
    let mut result = decode(blocking, Box::new(source),spec,false,start,Some(&progress));
    if result.as_ref().err().is_some_and(|e|e.contains("isomp4: missing moov atom")) {
        progress.wait_complete(blocking, &stop).map_err(|e|e.to_string())?;
        let file = std::fs::File::open(&path).map_err(|e|e.to_string())?;
        result = decode(blocking, Box::new(file),spec,false,start,Some(&progress));
    }
    stop.store(true,Ordering::Relaxed);
    cancelled.notify_one();
    worker.recv().map_err(|_|"writer stopped")?.map_err(|e|e.to_string())?;
    let _ = std::fs::remove_file(path);
    let mut output = result?;
    output["reads"] = serde_json::to_value(&*reads.lock().unwrap()).unwrap();
    output["total_bytes"] = total.into();
    Ok(output)
}

struct ThrottledFiles(Option<(File, File)>);
impl Drop for ThrottledFiles {
    fn drop(&mut self) { crate::rt::drop_off_executor(self.0.take()); }
}

fn throttled_writer(fixture:File,writer:File,chunk:usize,rate:u64,delay:u64,start:Instant,
    progress:Arc<Progress>,stop:Arc<Interrupt>,cancelled:Arc<tokio::sync::Notify>)
    -> impl std::future::Future<Output=std::io::Result<()>> + Send {
    // The timer/cancellation future can own files between finite jobs. Its
    // destructor must not close them on the executor, even if a reply is lost.
    let mut state=(ThrottledFiles(Some((fixture,writer))),vec![0;chunk],0u64);
    // Wrap before constructing the future: cancellation before its first poll
    // must have the same retirement behavior as cancellation during a timer.
    async move {
    loop {
        if stop.load(Ordering::Relaxed) {break;}
        let due=Duration::from_millis(delay)+Duration::from_secs_f64(state.2 as f64/rate as f64);
        if let Some(wait)=due.checked_sub(start.elapsed()) {
            tokio::select! {()=tokio::time::sleep(wait)=>{},()=cancelled.notified()=>break}
        }
        if stop.load(Ordering::Relaxed) {break;}
        let (next,n)=crate::rt::spawn_blocking(move |_|->std::io::Result<_> {
            let (fixture,writer)=state.0.0.as_mut().expect("owned throttle files");
            let n=fixture.read(&mut state.1)?;
            writer.write_all(&state.1[..n])?;
            state.2+=n as u64;
            Ok((state,n))
        }).await.map_err(|_|std::io::Error::other("generated writer stopped"))??;
        state=next;
        if n==0 {progress.finish();break;}
        progress.advance(state.2);
    }
    // Graceful completion acknowledges the actual close. An aborted future
    // still transfers cleanup through ThrottledFiles::drop without waiting.
    crate::rt::spawn_blocking(move |_|drop(state)).await
        .map_err(|_|std::io::Error::other("generated writer close stopped"))
    }
}

#[cfg(test)]
mod writer_tests {
    use super::*;
    fn files()->(PathBuf,std::fs::File,std::fs::File,Vec<u8>) {
        let base=std::env::var_os("Q2_GENERATED_TEST_ROOT").map(PathBuf::from)
            .unwrap_or_else(||PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build/perf/tests"));
        let root=base.join(format!("throttled-writer-{}-{}",std::process::id(),fastrand::u64(..)));
        std::fs::create_dir_all(&root).unwrap();
        let bytes:Vec<_>=(0..10_000).map(|n|(n%251) as u8).collect();
        std::fs::write(root.join("input"),&bytes).unwrap();
        let input=std::fs::File::open(root.join("input")).unwrap();
        let output=std::fs::File::create(root.join("output")).unwrap();
        (root,input,output,bytes)
    }
    #[tokio::test]
    async fn async_throttle_publishes_exact_chunks_and_eof() {
        let (root,input,output,bytes)=files();let progress=Progress::new();
        let observed=progress.clone();
        let task=crate::rt::io().spawn(throttled_writer(input,output,103,1_000_000,1,Instant::now(),progress,
            Arc::new(Interrupt::new(false)),Arc::new(tokio::sync::Notify::new())));
        tokio::time::timeout(crate::library::TEST_LIVENESS,task).await.unwrap().unwrap().unwrap();
        assert_eq!(std::fs::read(root.join("output")).unwrap(),bytes);
        let state=observed.snapshot();assert!(state.done);assert_eq!(state.written,10_000);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[tokio::test]
    async fn async_throttle_cancellation_interrupts_long_timer_and_retires_files() {
        let (root,input,output,_)=files();
        let stop=Arc::new(Interrupt::new(false));let cancelled=Arc::new(tokio::sync::Notify::new());
        let task=crate::rt::io().spawn(throttled_writer(input,output,103,1,60_000,Instant::now(),Progress::new(),stop.clone(),cancelled.clone()));
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(!task.is_finished());stop.store(true,Ordering::Relaxed);cancelled.notify_one();
        tokio::time::timeout(Duration::from_secs(2),task).await.unwrap().unwrap().unwrap();
        assert!(std::fs::read(root.join("output")).unwrap().is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
}
fn image_bench(_blocking: &crate::rt::Blocking, spec: &Spec) -> Result<Value,String> {
    let bytes = read_fixture(_blocking, &spec.fixture)?;
    let mut times = Vec::new();
    let mut checksum = 0u32;
    let mut pixel_md5 = String::new();
    for i in 0..spec.iterations+3 {
        let t = Instant::now();
        let image = crate::gfx::Image::decode_square(&bytes,spec.size).ok_or("image decode failed")?;
        let elapsed = us(t);
        checksum = image.pixels.iter().fold(0,|a,p|a.wrapping_add(*p));
        let mut digest = Md5::new();
        for pixel in &image.pixels { digest.update(pixel.to_le_bytes()); }
        pixel_md5 = format!("{:x}", digest.finalize());
        std::hint::black_box(&image);
        if i>=3 { times.push(elapsed); }
    }
    Ok(json!({"samples_us":times,"checksum":checksum,"pixel_md5":pixel_md5}))
}
/// Exercise random access after a streaming-style probe. Ogg must be
/// reopened with its complete seek bounds, exactly as in the player.
fn seek_bench(blocking: &crate::rt::Blocking, spec: &Spec) -> Result<Value, String> {
    use symphonia::core::formats::{SeekMode, SeekTo};
    use symphonia::core::units::Time;
    let ranges = Arc::new(Mutex::new(Vec::new()));
    let file = || -> Result<Box<dyn MediaSource>, String> {
        let mut original = std::fs::File::open(&spec.fixture).map_err(|e| e.to_string())?;
        if spec.kind != "seek-range" { return Ok(Box::new(original)); }
        std::fs::create_dir_all(&spec.scratch).map_err(|e| e.to_string())?;
        let partial = spec.scratch.join("seek-prefix.part");
        let total = original.metadata().map_err(|e| e.to_string())?.len();
        let mut prefix = vec![0; (64 * 1024).min(total as usize)];
        original.read_exact(&mut prefix).map_err(|e| e.to_string())?;
        std::fs::write(&partial, &prefix).map_err(|e| e.to_string())?;
        let progress = Progress::new();
        progress.set_total(Some(total)); progress.advance(prefix.len() as u64);
        let fixture = spec.fixture.clone();
        let requests = ranges.clone();
        let client = (!spec.url.is_empty()).then(|| subsonic::Client::new(subsonic::Credentials {
            server: spec.url.clone(), user: "fixture".into(), auth: subsonic::Auth::Password("synthetic".into()),
        }, "Q2SeekFixture"));
        let reader = ProgressiveReader::open(blocking, &partial, progress, Arc::new(crate::audio::interrupt::Interrupt::new(false))).map_err(|e| e.to_string())?
            .with_async_ranges(Arc::new(move |pos, size| {
                let response=client.as_ref().map(|client|client.read_range_async(&client.stream_request("fixture", &subsonic::StreamOptions { format:Some("raw".into()), max_bit_rate:None }),pos,size));
                let (requests,fixture)=(requests.clone(),fixture.clone());
                Box::pin(async move {
                    crate::rt::spawn_blocking(move |_|requests.lock().unwrap().push((pos,size))).await
                        .map_err(|_|std::io::Error::other("fixture recorder stopped"))?;
                    if let Some(response)=response {
                        response.await.map_err(|_|std::io::Error::other("fixture range failed"))
                    } else {
                        crate::rt::spawn_blocking(move |_| {
                            let mut file = std::fs::File::open(&fixture)?;
                            file.seek(SeekFrom::Start(pos))?;
                            let mut bytes = Vec::new(); file.take(size as u64).read_to_end(&mut bytes)?;
                            Ok(Some(bytes))
                        }).await.map_err(|_|std::io::Error::other("fixture reader stopped"))?
                    }
                })
            }));
        Ok(Box::new(reader))
    };
    let ext = spec.fixture.extension().and_then(|s|s.to_str());
    let mut format = match crate::audio::decoder::probe(blocking, file()?,ext,true) {
        Ok(p) => p.format,
        Err(e) if crate::audio::decoder::needs_complete_file(&e) => crate::audio::decoder::probe(blocking, file()?,ext,false).map_err(|e|e.to_string())?.format,
        Err(e) => return Err(e.to_string()),
    };
    if format.tracks().iter().any(|t|t.codec_params.codec == symphonia::core::codecs::CODEC_TYPE_VORBIS) {
        format = crate::audio::decoder::probe(blocking, file()?,ext,false).map_err(|e|e.to_string())?.format;
    }
    let track = format.tracks().iter().find(|t|t.codec_params.codec != CODEC_TYPE_NULL).ok_or("no track")?;
    let id = track.id;
    let mut decoder = symphonia::default::get_codecs().make(&track.codec_params,&DecoderOptions::default()).map_err(|e|e.to_string())?;
    let mut positions = Vec::new();
    for &ms in &spec.seek_ms {
        let t = Instant::now();
        let actual = crate::audio::callback::with(blocking, || format.seek(SeekMode::Accurate,SeekTo::Time { time:Time::new(ms/1000,(ms%1000) as f64/1000.0),track_id:Some(id) })).map_err(|e|e.to_string())?;
        decoder.reset();
        let pcm_md5 = loop {
            let packet = crate::audio::callback::with(blocking, || format.next_packet()).map_err(|e|e.to_string())?;
            if packet.track_id()!=id { continue; }
            let audio = decoder.decode(&packet).map_err(|e|e.to_string())?;
            if audio.frames()>0 {
                let mut samples = crate::audio::decoder::Samples::default();
                let mut hash = Md5::new();
                for sample in samples.convert(audio, 1.0) { hash.update(sample.to_le_bytes()); }
                break format!("{:x}", hash.finalize());
            }
        };
        positions.push(json!({"requested_ms":ms,"actual_ts":actual.actual_ts,"required_ts":actual.required_ts,"wall_us":us(t),"pcm_md5":pcm_md5}));
    }
    Ok(json!({"positions":positions,"range_requests":*ranges.lock().unwrap(),"range_transport":if spec.url.is_empty() { "local fixture" } else { "HTTP fixture" }}))
}

fn radio_player(_blocking: &crate::rt::Blocking, spec:&Spec) -> Result<Value,String> {
    use crate::audio::{alsa::{SoftHw,FAKE},player::{self,Cmd,PlayState}};
    struct Source { url:String,proxy:Option<String> }
    impl player::TrackSource for Source {
        fn open(&self, _blocking: &crate::rt::Blocking,_:&subsonic::Song,interrupt:&Arc<crate::audio::interrupt::Interrupt>)->Result<player::OpenedTrack,String> {
            Ok(player::OpenedTrack::radio(crate::audio::radio::Radio::open(self.url.clone(),self.proxy.clone(),interrupt.clone())))
        }
        fn upcoming(&self,_:&[subsonic::Song]){}
        fn now_playing(&self,_:&subsonic::Song){}
        fn played(&self,_:&subsonic::Song,_:u64){}
        fn save_queue(&self,_:&[subsonic::Song],_:usize,_:u64){}
    }
    let settings=player::Settings {eq:Default::default(),volume:0,headphone_power: crate::config::HeadphonePower::Standard,filter:0,replay_gain:crate::config::ReplayGainMode::Off,gapless:true};
    let p=player::spawn(Arc::new(Source {url:spec.url.clone(),proxy:spec.proxy.clone()}),Box::new(SoftHw(FAKE.into())),settings,Arc::new(||{}));
    p.send(Cmd::Play {songs:std::sync::Arc::new(vec![subsonic::Song {id:"public-radio-fixture".into(),title:"Public radio fixture".into(),radio_url:Some(spec.url.clone()),..Default::default()}]),index:0,shuffle:false});
    let start=Instant::now();
    let result=loop {
        let status=p.status();
        if let Some(error)=status.error {break Err(error);}
        if status.state==PlayState::Playing && status.position_ms>=spec.radio_seconds.clamp(2, 20)*1000 {break Ok(json!({"position_ms":status.position_ms,"format":status.format,"state":"playing","radio_title":status.radio_title}));}
        if start.elapsed()>Duration::from_secs(30) {break Err("Player did not reach the requested playback duration".into());}
        std::thread::sleep(Duration::from_millis(20));
    };
    p.send(Cmd::Quit);
    result
}

/// Timer wakeups that do no work, at `rate` per second for `seconds`: the
/// power cost of waking an idle Q2, for tests/power (iterations = rate).
fn wakeups(_blocking: &crate::rt::Blocking, rate:u32,seconds:u64) -> Result<Value,String> {
    let period=Duration::from_secs(1)/rate.max(1);
    let (start,mut next,mut late)=(Instant::now(),Instant::now(),Duration::ZERO);
    let mut n=0u64;
    while start.elapsed()<Duration::from_secs(seconds) {
        next+=period;
        std::thread::sleep(next.saturating_duration_since(Instant::now()));
        late=late.max(Instant::now().saturating_duration_since(next));
        n+=1;
    }
    Ok(json!({"wakeups":n,"max_late_us":late.as_micros() as u64}))
}

pub(crate) fn open_fixture(_blocking: &crate::rt::Blocking, path: &Path) -> Result<std::fs::File, String> { std::fs::File::open(path).map_err(|e|e.to_string()) }
pub(crate) fn read_fixture(_blocking: &crate::rt::Blocking, path: &Path) -> Result<Vec<u8>, String> { std::fs::read(path).map_err(|e|e.to_string()) }
fn read_spec<T: serde::de::DeserializeOwned>(_blocking: &crate::rt::Blocking, path: &Path) -> Result<T,String> {
    serde_json::from_slice(&std::fs::read(path).map_err(|e|e.to_string())?).map_err(|e|e.to_string())
}

pub fn run(path: &Path) -> Result<(),String> {
    assert!(crate::rt::may_wait_for_tasks(), "benchmark entry belongs to an ordinary thread");
    let blocking = &crate::rt::Blocking::dedicated_thread();
    run_with(blocking, path)
}
fn run_with(blocking: &crate::rt::Blocking, path: &Path) -> Result<(),String> {
    let spec:Spec = read_spec(blocking, path)?;
    if spec.iterations == 0 || spec.iterations>10000 { return Err("invalid iterations".into()); }
    let before = usage(); let started = Instant::now();
    let result = match spec.kind.as_str() {
        "artwork-storage" => artwork_storage::run(blocking, &spec),
        "artwork-worker" => artwork_storage::run_worker(blocking, &spec),
        "artwork-aliases" => artwork_storage::run_aliases(blocking, &spec),
        "artwork-migration" => artwork_storage::run_migration(blocking, &spec),
        "book-range-playback" => book_ranges::run(blocking, &spec),
        "download-owned" => offline_download::run(blocking, &spec),
        "spoken-pause" => spoken_pause::run(blocking, &spec),
        "spoken-native" => spoken_native(blocking, &spec),
        "spoken-quality-file" => spoken_quality_file(blocking, &spec),
        "spoken-server" => crate::library::validate_spoken_server(blocking, &spec.scratch,&spec.url,&spec.api_token,spec.proxy.as_deref().ok_or("Spoken fixture needs an explicit proxy")?),
        "playback" => playback::run(blocking, &spec),
        "eq" => eq_bench(&spec),
        "aead" => aead_bench(&spec),
        "convert" => convert::run(spec.iterations),
        "convert-check" => convert::check(),
        "jpeg-check" => jpeg_check::run(blocking, &spec.fixture),
        "lossless-check" => lossless_check::run(blocking, &spec.fixture),
        "mp3-check" => mp3_check::run(blocking, &spec.fixture),
        "autoeq" => {
            let (profile,cached)=crate::rt::Ui::new().map_err(|e|e.to_string())?.block_on(async {
                let entry=crate::audio::autoeq::search_async("sennheiser 650".into()).await?
                    .into_iter().find(|e|e.name()=="Sennheiser HD 650").ok_or("Headphone catalog entry missing")?;
                let profile=entry.clone().load_async(spec.scratch.clone(),false,spec.proxy.clone()).await?;
                let cached=entry.load_async(spec.scratch.clone(),true,None).await?;
                Ok::<_,String>((profile,cached))
            })?;
            Ok(json!({"name":profile.name,"filters":profile.filters.len(),"preamp_db":profile.preamp,"offline_equal":profile==cached,"source":profile.source}))
        }
        "startup" => startup(blocking, &spec),
        "decode" => open_fixture(blocking, &spec.fixture).and_then(|f|decode(blocking, Box::new(f),&spec,true,started,None)),
        "spoken-metadata" => (|| {
            let mut timings=Vec::new();let mut last=None;
            for _ in 0..spec.iterations {
                let start=Instant::now();let info=crate::library::spoken::read_m4b_info(blocking, &spec.fixture).map_err(|_|"Unable to read generated M4B metadata")?;
                timings.push(us(start));last=Some(info);
            }
            timings.sort_unstable();let info=last.unwrap();
            Ok(json!({"duration_ms":info.duration_ms,"title":info.title,"artist":info.artist,"chapters":info.chapters,"cover_bytes":info.cover.map(|v|v.len()),
                "iterations":spec.iterations,"metadata_us_p50":timings[timings.len()/2],"metadata_us_p95":timings[(timings.len()*95/100).min(timings.len()-1)]}))
        })(),
        "image" => image_bench(blocking, &spec),
        "seek" | "seek-range" => seek_bench(blocking, &spec),
        "radio" => {
            let source = crate::audio::radio::Radio::open(spec.url.clone(),spec.proxy.clone(),Arc::new(crate::audio::interrupt::Interrupt::new(false)));
            decode(blocking, Box::new(source),&spec,false,started,None)
        }
        "radio-player" => radio_player(blocking, &spec),
        "radio-metadata" => crate::rt::Ui::new().map_err(|e| e.to_string())?
            .block_on(subsonic::radio::france::fetch_async(spec.proxy.as_deref()))
            .map(|m| json!({"radio_title":m.title,"expires":m.expires,"refresh_seconds":m.refresh.as_secs(),
                "clock_seconds":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs()}))
            .map_err(|e| e.to_string()),
        "config-startup" => crate::config::startup_benchmark(blocking, &spec.scratch,spec.iterations,&spec.scene),
        "music-binding" => crate::library::db::Db::startup_binding_benchmark_ordinary( &spec.scratch,spec.iterations,&spec.scene),
        "library-db" => library::db(blocking, &spec),
        "library-sync" => library::sync(blocking, &spec),
        "app-startup" => library::startup(blocking, &spec),
        "ui" => crate::ui::benchmark(blocking, &spec.scratch,spec.iterations),
        "ui-check" => crate::ui::span_check(),
        "ui-parts" => crate::ui::parts_benchmark(blocking, spec.iterations),
        "storage-render" => crate::ui::screens::storage::benchmark(spec.iterations, &spec.scene),
        "wakeups" => wakeups(blocking, spec.iterations as u32, spec.radio_seconds),
        _ => Err("unknown benchmark kind".into()),
    };
    let after = usage();
    let hardware:serde_json::Map<String,Value> = [
        ("cpu_governor","/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"),
        ("cpu_khz","/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq"),
    ].into_iter().filter_map(|(k,p)|std::fs::read_to_string(p).ok().map(|s|(k.into(),s.trim().into()))).collect();
    let out = json!({"schema":1,"kind":spec.kind,"arch":std::env::consts::ARCH,"wall_us":us(started),
        "cpu_us":cpu(&after)-cpu(&before),"peak_rss_kib":after.ru_maxrss,"hardware":hardware,
        "result":result.as_ref().ok(),"error":result.as_ref().err()});
    println!("{out}");
    result.map(|_|())
}

/// Synthetic stereo audio through the same conversion and EQ as playback.
/// CPU cost is separate from decoding, network, ALSA, and device power.
/// TLS record ciphers in software: microseconds to seal and open 1 MiB with
/// each AEAD rustls's ring provider offers (the X2000 has no AES
/// instructions), and whether every record round-trips.
fn aead_bench(spec: &Spec) -> Result<Value,String> {
    use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_128_GCM, AES_256_GCM, CHACHA20_POLY1305};
    let mut out = serde_json::Map::new();
    let plain: Vec<u8> = (0..16384u32).map(|i| (i.wrapping_mul(2654435761) >> 24) as u8).collect();
    for (name, alg) in [("aes_128_gcm", &AES_128_GCM), ("aes_256_gcm", &AES_256_GCM), ("chacha20_poly1305", &CHACHA20_POLY1305)] {
        let key = LessSafeKey::new(UnboundKey::new(alg, &vec![7u8; alg.key_len()]).map_err(|e| e.to_string())?);
        let (mut seal, mut open, mut ok) = (Vec::new(), Vec::new(), true);
        for _ in 0..spec.iterations.max(1) {
            let (mut s, mut o) = (0u64, 0u64);
            // 64 records of 16 KiB (TLS's maximum record size): 1 MiB.
            for n in 0..64u32 {
                let nonce = || { let mut b = [0u8; 12]; b[8..].copy_from_slice(&n.to_be_bytes()); Nonce::assume_unique_for_key(b) };
                let mut buf = plain.clone();
                let t = Instant::now();
                key.seal_in_place_append_tag(nonce(), Aad::empty(), &mut buf).map_err(|e| e.to_string())?;
                s += t.elapsed().as_micros() as u64;
                let t = Instant::now();
                let opened = key.open_in_place(nonce(), Aad::empty(), &mut buf).map_err(|e| e.to_string())?;
                o += t.elapsed().as_micros() as u64;
                ok &= opened == &plain[..];
            }
            seal.push(s);
            open.push(o);
        }
        out.insert(name.into(), json!({"seal_us_per_mib": median(&seal), "open_us_per_mib": median(&open), "roundtrip": ok}));
    }
    fn median(v: &[u64]) -> u64 { let mut v = v.to_vec(); v.sort_unstable(); v[v.len() / 2] }
    Ok(out.into())
}

fn eq_bench(spec: &Spec) -> Result<Value,String> {
    use crate::audio::eq::{Equalizer, Profile, Settings};
    use symphonia::core::audio::{AudioBuffer,AudioBufferRef,SignalSpec,Channels,Signal};
    use std::borrow::Cow;
    let mut results=Vec::new();
    for rate in [44100,96000,192000] {
        let mut pcm=AudioBuffer::<f32>::new(1024,SignalSpec::new(rate,Channels::FRONT_LEFT|Channels::FRONT_RIGHT));
        pcm.render_silence(None);
        for ch in 0..2 { for (n,x) in pcm.chan_mut(ch).iter_mut().enumerate() { *x=(n as f32*0.13).sin()*0.3; } }
        for mode in ["off","graphic","combined"] {
            let mut settings=Settings::default();
            if mode!="off" {settings.graphic_enabled=true;settings.bands=[3,-2,4,-1,2,-3,1,3,-2,2];}
            if mode=="combined" {
                settings.headphone_enabled=true;
                settings.headphone=Some(Profile::parse("fixture".into(),"fixture".into(),include_str!("../assets/autoeq/hd650-fixture.txt"))?);
            }
            let mut eq=Equalizer::new(settings);
            let start=Instant::now();eq.prepare(rate,2);let setup_us=us(start);
            let mut conversion=crate::audio::decoder::Samples::default();
            conversion.convert_eq(AudioBufferRef::F32(Cow::Borrowed(&pcm)),0.31622776,&mut eq);
            let before=usage();let start=Instant::now();let mut checksum=0i64;
            for _ in 0..spec.iterations {
                conversion.convert_eq(AudioBufferRef::F32(Cow::Borrowed(&pcm)),0.31622776,&mut eq);
                checksum=checksum.wrapping_add(std::hint::black_box(conversion.samples())[0] as i64);
            }
            let cpu_us=cpu(&usage())-cpu(&before);
            let wall_us=us(start);
            let mut hash=Md5::new();
            for sample in conversion.samples() { hash.update(sample.to_le_bytes()); }
            let audio_seconds=spec.iterations as f64*1024.0/rate as f64;
            results.push(json!({"rate":rate,"mode":mode,"setup_us":setup_us,"wall_us":wall_us,"cpu_us":cpu_us,"pcm_md5":format!("{:x}",hash.finalize()),
                "audio_seconds":audio_seconds,"cpu_percent_realtime":cpu_us as f64/audio_seconds/10000.0,"checksum":checksum}));
        }
    }
    Ok(json!({"cases":results}))
}

/// Opt-in numeric-only main-loop tracing. Buffers bounded observations in RAM;
/// never includes input contents, titles, settings, credentials or screenshots.
pub struct InputTrace {
    path: PathBuf,
    pending: Vec<(Instant,Instant)>,
    rows: Vec<Value>,
    dropped: usize,
    frames: usize,
}
impl InputTrace {
    pub fn from_env() -> Option<Self> {
        std::env::var_os("Q2ND_PRESENT_TRACE").map(|p|Self {path:p.into(),pending:Vec::new(),rows:Vec::new(),dropped:0,frames:0})
    }
    pub fn input(&mut self, received: Instant) {
        if self.pending.len()<64 {self.pending.push((received,Instant::now()));} else {self.dropped+=1;}
    }
    pub fn frame(&mut self, started: Instant, drawn: Instant, presented: Instant) {
        self.frames+=1;
        for (received,handled) in self.pending.drain(..) {
            if self.rows.len()>=4096 {self.dropped+=1;continue;}
            self.rows.push(json!({"frame":self.frames,"queue_us":handled.duration_since(received).as_micros() as u64,
                "handling_to_draw_us":started.duration_since(handled).as_micros() as u64,
                "draw_us":drawn.duration_since(started).as_micros() as u64,
                "present_us":presented.duration_since(drawn).as_micros() as u64,
                "input_to_present_us":presented.duration_since(received).as_micros() as u64}));
        }
    }
    pub fn save(self, _blocking: &crate::rt::Blocking) {
        let result=json!({"method":"Input callback or script enqueue through actual application handling/rendering to presentation return; not optical latency. Multiple inputs can share one frame.",
            "rows":self.rows,"unpresented_inputs":self.pending.len(),"dropped_inputs":self.dropped});
        if let Ok(bytes)=serde_json::to_vec(&result) {let _=std::fs::write(self.path,bytes);}
    }
}

/// Linux sched debug output uses milliseconds plus six nanosecond digits.
/// This is the calling main thread, not aggregate process CPU time.
fn sched_runtime_ns(text: &str) -> Option<u64> {
    let value = text.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        (name.trim() == "se.sum_exec_runtime").then_some(value.trim())
    })?;
    let (ms, ns) = value.split_once('.')?;
    if ns.len() != 6 || !ns.bytes().all(|c| c.is_ascii_digit()) { return None; }
    ms.parse::<u64>().ok()?.checked_mul(1_000_000)?.checked_add(ns.parse().ok()?)
}

pub(crate) fn main_runtime_ns(_blocking: &crate::rt::Blocking) -> Option<u64> {
    if let Ok(text) = std::fs::read_to_string("/proc/self/schedstat") {
        if let Some(value) = text.split_whitespace().next().and_then(|s| s.parse().ok()) {
            return Some(value);
        }
    }
    sched_runtime_ns(&std::fs::read_to_string("/proc/self/sched").ok()?)
}

#[cfg(test)]
mod runtime_tests {
    use super::sched_runtime_ns;
    #[test]
    fn parses_kernel_units_without_floating_point_rounding() {
        assert_eq!(sched_runtime_ns("se.sum_exec_runtime : 3438.094344\n"), Some(3_438_094_344));
        assert_eq!(sched_runtime_ns("se.sum_exec_runtime : 9007199254.740993"), Some(9_007_199_254_740_993));
        for value in ["missing", "1.2", "1.0000001", "18446744073709551615.000000"] {
            assert_eq!(sched_runtime_ns(&format!("se.sum_exec_runtime : {value}")), None);
        }
    }
}


/// Generated local book through the actual mixer/decoder/ALSA graph. Call only
/// in the disposable emulator or an allocated device test slot, with owned data.
fn spoken_native(blocking: &crate::rt::Blocking, spec:&Spec)->Result<Value,String> {
    use crate::audio::{player::{self,Cmd,PlayState},alsa::MainlineHw};
    if !(50..=300).contains(&spec.speed_percent) || spec.speed_percent % 5 != 0 { return Err("spoken-native speed must be 50..300 in steps of 5".into()); }
    if spec.scene=="checkpoint" {
        if spec.speed_percent!=100 { return Err("checkpoint fixture needs normal speed".into()); }
        crate::playtrace::configure(true);
    }
    let paths=crate::config::Paths::new(spec.scratch.clone());paths.create_all(blocking).map_err(|e|e.to_string())?;
    crate::runtime_log::init(&paths.log());
    crate::metrics::activate();
    crate::runtime_log::start();
    std::fs::copy(&spec.fixture,paths.media_root().join("audiobooks/generated.m4b")).map_err(|e|e.to_string())?;
    let mut config=crate::config::Config::default();config.playback.volume=80;config.playback.scrobble=false;config.playback.sync_queue=false;config.spoken.speed_percent=spec.speed_percent;
    let lib=crate::library::Library::open(paths,&config,Arc::new(|_|{}));
    let (books,issues)=lib.local_audiobooks()?;if !issues.is_empty() || books.len()!=1{return Err("Generated native fixture must contain one valid M4B".into());}
    crate::audio::mixer::prepare_q2_playback()?;
    let source=Arc::new(crate::library::Tracks(lib.clone()));
    let settings=||player::Settings {eq:Default::default(),volume:80,headphone_power: crate::config::HeadphonePower::Standard,filter:0,replay_gain:crate::config::ReplayGainMode::Off,gapless:false};
    let player=player::spawn(source.clone(),Box::new(MainlineHw("hw:Q2,0".into())),settings(),Arc::new(||{}));
    let wait=|handle:&player::PlayerHandle,predicate:&dyn Fn(&player::Status)->bool|->Result<player::Status,String>{
        let end=Instant::now()+Duration::from_secs(45);
        loop{let status=handle.status();if let Some(error)=&status.error{return Err(error.clone());}
            if predicate(&status){return Ok(status);}if Instant::now()>end{return Err(format!("Generated native playback timed out: state={:?},position={}",status.state,status.position_ms));}
            std::thread::sleep(Duration::from_millis(10));}
    };
    let requested=Instant::now();
    player.send(Cmd::Play {songs:std::sync::Arc::new(books.clone()),index:0,shuffle:false});
    let active=wait(&player,&|s|s.state==PlayState::Playing && s.position_ms>=350)?;
    let first_playing_us=us(requested);
    if active.speed_percent != spec.speed_percent || active.speed_override.is_some() {
        player.shutdown();return Err("Generated native default speed was not applied".into());
    }
    let mut output=json!({"sample_rate":active.sample_rate,"format":active.format,"duration_ms":active.duration_ms,"scene":spec.scene,
        "requested_speed_percent":spec.speed_percent,"effective_speed_percent":active.speed_percent,"speed_override":active.speed_override});
    let result=(||->Result<(),String>{
        if spec.scene=="checkpoint" {
            if active.duration_ms<36000 { return Err("Checkpoint fixture needs at least36seconds of media".into()); }
            let observed=wait(&player,&|s|s.state==PlayState::Playing && s.position_ms>=32000)?;
            let logical=&books[0].spoken.as_ref().ok_or("Missing book identity")?.id;
            let before_read_us=us(requested);
            let periodic=lib.spoken_position(logical)?.ok_or("Missing periodic checkpoint")?;
            let after_read_us=us(requested);
            let after_read=wait(&player,&|_|true)?;
            if observed.generation!=active.generation || after_read.generation!=active.generation
                || after_read.state!=PlayState::Playing || periodic.position_ms<28000
                || periodic.position_ms>after_read.position_ms {
                return Err("Periodic checkpoint is not bound to the still-playing generation".into());
            }
            let pause_requested=Instant::now();player.send(Cmd::Pause);
            let paused=wait(&player,&|s|s.state==PlayState::Paused)?;
            let pause_ack_us=us(pause_requested);
            let durable=lib.spoken_position(logical)?.ok_or("Missing paused checkpoint")?;
            if durable.position_ms!=paused.position_ms { return Err("Pause acknowledged a different durable position".into()); }
            let resume_requested=Instant::now();player.send(Cmd::PlayPause);
            let resumed=wait(&player,&|s|s.state==PlayState::Playing && s.position_ms>=paused.position_ms+200)?;
            let resume_progress_us=us(resume_requested);
            player.send(Cmd::Pause);let stopped=wait(&player,&|s|s.state==PlayState::Paused)?;
            player.shutdown();lib.save_now();
            let saved=lib.spoken_position(logical)?.ok_or("Missing shutdown checkpoint")?;
            if saved.position_ms!=stopped.position_ms || resumed.generation!=active.generation {
                return Err("Shutdown checkpoint or selected generation changed".into());
            }
            output["checkpoint"]=json!({"generation":active.generation,"first_playing_us":first_playing_us,
                "periodic_observed_before_us":before_read_us,"periodic_observed_after_us":after_read_us,
                "periodic_saved_ms":periodic.position_ms,"observed_position_ms":after_read.position_ms,
                "pause_ack_us":pause_ack_us,"paused_ms":paused.position_ms,"paused_durable_ms":durable.position_ms,
                "resume_progress_us":resume_progress_us,"resumed_ms":resumed.position_ms,
                "stopped_ms":stopped.position_ms,"shutdown_saved_ms":saved.position_ms});
            output["playtrace"]=json!(crate::playtrace::global().snapshot().text());
        }else if spec.scene=="controls" {
            let chapter_starts=books[0].spoken.as_ref().ok_or("Missing chapters")?.chapters.iter().map(|c|c.start_ms).collect::<Vec<_>>();
            if chapter_starts.len()<3 || chapter_starts[0]!=0 || chapter_starts[1]<5000{return Err("Generated controls need three spaced chapters".into());}
            player.send(Cmd::Pause);let paused=wait(&player,&|s|s.state==PlayState::Paused)?;
            config.spoken.skip_forward_seconds=1;config.spoken.skip_backward_seconds=1;config.spoken.skip_chapter=false;lib.configure(&config);
            player.send(Cmd::Next);let forward=wait(&player,&|s|s.state==PlayState::Paused && s.position_ms==paused.position_ms+1000)?;
            player.send(Cmd::Prev);let backward=wait(&player,&|s|s.state==PlayState::Paused && s.position_ms==paused.position_ms)?;
            config.spoken.skip_chapter=true;lib.configure(&config);
            player.send(Cmd::Scrub {generation:backward.generation,position:Some(chapter_starts[2]+100),active:false});
            wait(&player,&|s|s.state==PlayState::Paused && s.position_ms==chapter_starts[2]+100)?;
            player.send(Cmd::PlayPause);wait(&player,&|s|s.state==PlayState::Playing && s.position_ms>=chapter_starts[2]+150)?;
            player.send(Cmd::Prev);let one=wait(&player,&|s|s.state==PlayState::Playing && s.position_ms>=chapter_starts[1]+100 && s.position_ms<chapter_starts[1]+1500)?;
            player.send(Cmd::Prev);let two=wait(&player,&|s|s.state==PlayState::Playing && s.position_ms>=100 && s.position_ms<1500)?;
            player.send(Cmd::Pause);let stopped=wait(&player,&|s|s.state==PlayState::Paused)?;
            player.send(Cmd::Release);let released=wait(&player,&|s|s.state==PlayState::Paused && s.sample_rate.is_none())?;
            if released.position_ms!=stopped.position_ms{return Err("Release changed audible book location".into());}
            player.send(Cmd::PlayPause);wait(&player,&|s|s.state==PlayState::Playing && s.position_ms>=released.position_ms+100)?;
            std::fs::copy(&spec.fixture,lib.paths.media_root().join("audiobooks/generated-other.m4b")).map_err(|e|e.to_string())?;
            let(other,issues)=lib.local_audiobooks()?;if !issues.is_empty(){return Err("Generated switch fixture failed".into());}
            let other=other.into_iter().find(|s|s.id!=books[0].id).ok_or("Missing generated second book")?;
            let old=books[0].spoken.as_ref().unwrap().id.clone();let new=other.id.clone();
            let before=player.status().position_ms;
            player.send(Cmd::Play {songs:std::sync::Arc::new(vec![other]),index:0,shuffle:false});
            wait(&player,&|s|s.current().is_some_and(|book|book.id==new) && s.state==PlayState::Playing && s.position_ms>=100)?;
            let saved=lib.spoken_position(&old)?.ok_or("Switch did not save old book")?;
            if saved.position_ms<before || saved.position_ms>before+1500{return Err("Switch did not preserve audible old-book location".into());}
            player.send(Cmd::Release);wait(&player,&|s|s.state==PlayState::Paused && s.sample_rate.is_none())?;
            let original=books[0].id.clone();player.send(Cmd::Play {songs:std::sync::Arc::new(books.clone()),index:0,shuffle:false});
            let resumed=wait(&player,&|s|s.current().is_some_and(|book|book.id==original) && s.state==PlayState::Playing && s.position_ms>=saved.position_ms+100)?;
            if resumed.position_ms>saved.position_ms+1500{return Err("Switched book resumed at wrong location".into());}
            player.send(Cmd::Pause);wait(&player,&|s|s.state==PlayState::Paused)?;
            output["time_before_ms"]=json!(paused.position_ms);output["time_forward_ms"]=json!(forward.position_ms);output["time_back_ms"]=json!(backward.position_ms);
            output["chapter_back_one_ms"]=json!(one.position_ms);output["chapter_back_two_ms"]=json!(two.position_ms);
            output["released_ms"]=json!(released.position_ms);output["switch_saved_ms"]=json!(saved.position_ms);output["switch_resumed_ms"]=json!(resumed.position_ms);
        }else if spec.scene=="pause-seek-resume" {
            player.send(Cmd::Pause);let paused=wait(&player,&|s|s.state==PlayState::Paused)?;
            std::thread::sleep(Duration::from_millis(150));
            if player.status().position_ms!=paused.position_ms{return Err("Paused source position advanced".into());}
            player.send(Cmd::Scrub {generation:paused.generation,position:Some(1700),active:false});
            wait(&player,&|s|s.state==PlayState::Paused && s.position_ms==1700)?;
            player.send(Cmd::PlayPause);wait(&player,&|s|s.state==PlayState::Playing && s.position_ms>=1900)?;
            player.send(Cmd::Pause);let stopped=wait(&player,&|s|s.state==PlayState::Paused)?;
            player.shutdown();lib.save_now();let logical=&books[0].spoken.as_ref().ok_or("Missing book identity")?.id;
            let saved=lib.spoken_position(logical)?.ok_or("Missing saved native progress")?;
            if saved.position_ms!=stopped.position_ms{return Err("Saved native audible position differs".into());}
            let resumed=player::spawn(source,Box::new(MainlineHw("hw:Q2,0".into())),settings(),Arc::new(||{}));
            // Ordinary startup restores a generic queue snapshot. Its stale
            // position must lose to the durable per-book checkpoint.
            resumed.send(Cmd::Restore {songs:std::sync::Arc::new(books),index:0,position_ms:100});
            let after=(||->Result<_,String>{
                let restored=wait(&resumed,&|s|s.state==PlayState::Paused)?;
                if restored.position_ms!=saved.position_ms{return Err("Stale queue position replaced durable spoken bookmark".into());}
                if restored.speed_percent!=spec.speed_percent{return Err("Restored book lost its effective speed".into());}
                resumed.send(Cmd::PlayPause);
                let after=wait(&resumed,&|s|s.state==PlayState::Playing && s.position_ms>=saved.position_ms+100)?;
                Ok((restored,after))
            })();
            resumed.shutdown();let(restored,after)=after?;
            if after.position_ms>saved.position_ms+1500{return Err("Resumed source position jumped".into());}
            output["paused_ms"]=json!(paused.position_ms);output["saved_ms"]=json!(saved.position_ms);output["restored_ms"]=json!(restored.position_ms);output["resumed_ms"]=json!(after.position_ms);
        }else{
            let ended=wait(&player,&|s|s.state==PlayState::Stopped)?;
            player.shutdown();lib.save_now();output["ended_ms"]=json!(ended.position_ms);
            let logical=&books[0].spoken.as_ref().ok_or("Missing book identity")?.id;
            let saved=lib.spoken_position(logical)?.ok_or("Missing completed native progress")?;
            if saved.position_ms!=active.duration_ms || ended.position_ms!=active.duration_ms {
                return Err("Completed native bookmark differs from source duration".into());
            }
            output["saved_ms"]=json!(saved.position_ms);
        }Ok(())
    })();
    player.shutdown();result?;Ok(output)
}

#[cfg(test)]
mod spoken_native_spec_tests {
    use super::*;
    #[test]
    fn missing_speed_keeps_the_native_control_at_one_hundred_percent() {
        let spec: Spec = serde_json::from_str(r#"{"kind":"spoken-native"}"#).unwrap();
        assert_eq!(spec.speed_percent, 100);
        for percent in [0, 49, 51, 176, 301, u16::MAX] {
            let spec = Spec { speed_percent: percent, ..Default::default() };
            assert!(spoken_native(&crate::rt::Blocking::dedicated_thread(), &spec).unwrap_err().contains("50..300"));
        }
    }
}


/// Production library/player graph into an explicitly owned ALSA file sink.
/// This is an unpaced PCM/CPU fixture, not a physical deadline measurement.
fn spoken_quality_file(blocking: &crate::rt::Blocking, spec: &Spec) -> Result<Value,String> {
    use crate::audio::{player::{self,Cmd,PlayState},alsa::SoftHw};
    if !(50..=300).contains(&spec.speed_percent) || spec.speed_percent%5 != 0 || spec.quality_volume>100 {
        return Err("Invalid generated quality speed/volume".into());
    }
    let parent=spec.scratch.parent().ok_or("Quality scratch needs an owned parent")?;
    let alsa=parent.join("owned-alsa.conf");
    if std::env::var_os("ALSA_CONFIG_PATH") != Some(alsa.as_os_str().to_owned()) {
        return Err("Quality fixture requires its explicitly owned ALSA_CONFIG_PATH".into());
    }
    if spec.scratch.exists() { return Err("Quality scratch must be fresh".into()); }
    let paths=crate::config::Paths::new(spec.scratch.clone()); paths.create_all(blocking).map_err(|e|e.to_string())?;
    crate::runtime_log::init(&paths.log());
    crate::metrics::activate();
    crate::runtime_log::start();
    let input=paths.media_root().join("audiobooks/generated.m4b");
    std::fs::copy(&spec.fixture,&input).map_err(|e|e.to_string())?;
    let mut format=crate::audio::decoder::probe(blocking, Box::new(std::fs::File::open(&input).map_err(|e|e.to_string())?),Some("m4b"),false).map_err(|e|e.to_string())?.format;
    let track=format.tracks().iter().find(|track|track.codec_params.codec!=CODEC_TYPE_NULL).ok_or("Missing quality fixture track")?;
    let id=track.id;let params=track.codec_params.clone();
    let mut decoder=symphonia::default::get_codecs().make(&params,&DecoderOptions::default()).map_err(|e|e.to_string())?;
    let (rate,channels)=loop {
        let packet=crate::audio::callback::with(blocking, || format.next_packet()).map_err(|e|e.to_string())?;if packet.track_id()!=id {continue;}
        let decoded=decoder.decode(&packet).map_err(|e|e.to_string())?;
        if decoded.frames()>0 {break (decoded.spec().rate,decoded.spec().channels.count());}
    };
    drop(format);drop(decoder);
    let mut config=crate::config::Config::default();config.playback.volume=spec.quality_volume;
    config.playback.scrobble=false;config.playback.sync_queue=false;config.spoken.speed_percent=spec.speed_percent;
    let lib=crate::library::Library::open(paths,&config,Arc::new(|_|{}));
    let(books,issues)=lib.local_audiobooks()?;
    if !issues.is_empty() || books.len()!=1{return Err("Quality fixture must contain one generated book".into());}
    let logical=books[0].spoken.as_ref().ok_or("Missing quality fixture identity")?.id.clone();
    let duration=books[0].spoken.as_ref().unwrap().duration_ms;
    if duration>30_000{return Err("Quality fixture exceeds30seconds".into());}
    let player=player::spawn(Arc::new(crate::library::Tracks(lib.clone())),Box::new(SoftHw("q2-quality-file".into())),
        player::Settings {eq:Default::default(),volume:spec.quality_volume,headphone_power: crate::config::HeadphonePower::Standard,filter:0,
            replay_gain:crate::config::ReplayGainMode::Off,gapless:false},Arc::new(||{}));
    let began=Instant::now();player.send(Cmd::Play {songs:std::sync::Arc::new(books),index:0,shuffle:false});
    let result=(||->Result<Value,String>{
        loop {
            let status=player.status();
            if let Some(error)=&status.error{return Err(error.clone());}
            if status.state==PlayState::Stopped && status.position_ms==duration && status.duration_ms==duration {break;}
            if began.elapsed()>Duration::from_secs(30){return Err("Generated quality fixture timeout".into());}
            std::thread::sleep(Duration::from_millis(1));
        }
        player.shutdown();lib.save_now();
        let saved=lib.spoken_position(&logical)?.ok_or("Missing quality fixture bookmark")?;
        if saved.position_ms!=duration{return Err("Quality fixture bookmark mismatch".into());}
        Ok(json!({"scope":"actual Library/Engine to owned unpaced ALSA file; no hardware/deadline claim",
            "rate":rate,"channels":channels,"duration_ms":duration,"saved_ms":saved.position_ms,
            "speed_percent":spec.speed_percent,"volume":spec.quality_volume,"render_us":us(began)}))
    })();
    player.shutdown();result
}
