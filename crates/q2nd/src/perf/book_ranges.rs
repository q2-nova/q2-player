//! Generated decoder cadence experiment. The sink is a bounded clock model,
//! not ALSA: missed deadlines must never be reported as physical underruns.
use super::*;

pub(super) fn run(blocking: &crate::rt::Blocking, spec: &Spec) -> Result<Value, String> {
    if spec.first_byte_ms > 2000 || !(20..=500).contains(&spec.cadence_ms) {
        return Err("range delay must be <=2000 ms and queue 20..500 ms".into());
    }
    let file = std::fs::File::open(&spec.fixture).map_err(|e| e.to_string())?;
    let total = file.metadata().map_err(|e| e.to_string())?.len();
    if total > 32 * 1024 * 1024 { return Err("generated fixture exceeds 32 MiB".into()); }
    let begin = Instant::now();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let source: Box<dyn MediaSource> = if spec.scene == "local" { Box::new(file) } else {
        if spec.scene != "ranges" { return Err("scene must be local or ranges".into()); }
        let path = spec.fixture.clone();
        let record = requests.clone();
        let delay = Duration::from_millis(spec.first_byte_ms);
        Box::new(crate::audio::source::RangeReader::new_async(total, Arc::new(crate::audio::interrupt::Interrupt::new(false)),
            Arc::new(move |offset, length| {
                let (path,record)=(path.clone(),record.clone());
                Box::pin(async move {
                    let started=us(begin);
                    tokio::time::sleep(delay).await;
                    crate::rt::spawn_blocking(move |_| {
                        let mut input=std::fs::File::open(&path)?;
                        input.seek(SeekFrom::Start(offset))?;
                        let mut data=Vec::new(); input.take(length as u64).read_to_end(&mut data)?;
                        record.lock().unwrap().push(json!({"offset":offset,"length":length,
                            "started_us":started,"completed_us":us(begin)}));
                        Ok(Some(data))
                    }).await.map_err(|_|std::io::Error::other("fixture reader stopped"))?
                })
            })))
    };
    let mut format = crate::audio::decoder::probe(blocking, source,
        spec.fixture.extension().and_then(|s|s.to_str()), false).map_err(|e|e.to_string())?.format;
    let track = format.tracks().iter().find(|t|t.codec_params.codec != CODEC_TYPE_NULL).ok_or("no audio")?;
    let id = track.id;
    let rate = track.codec_params.sample_rate.ok_or("no sample rate")?;
    let mut decoder = symphonia::default::get_codecs().make(&track.codec_params,
        &DecoderOptions::default()).map_err(|e|e.to_string())?;
    let mut conversion = crate::audio::decoder::Samples::default();
    let queue = Duration::from_millis(spec.cadence_ms);
    let mut deadline = None;
    let mut frames = 0u64;
    let mut packets = 0usize;
    let mut missed = Vec::new();
    let mut reads = Vec::new();
    let mut hash = Md5::new();
    loop {
        let started = Instant::now();
        let packet = match crate::audio::callback::with(blocking, || format.next_packet()) {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.to_string()),
        };
        if packet.track_id() != id { continue; }
        let decoded = decoder.decode(&packet).map_err(|e|e.to_string())?;
        let count = decoded.frames() as u64;
        if count == 0 { continue; }
        let wait = us(started);
        reads.push(wait);
        for s in conversion.convert(decoded, 1.0) { hash.update(s.to_le_bytes()); }
        let now = Instant::now();
        let mut end = deadline.unwrap_or(now);
        if packets > 0 && now > end {
            missed.push(json!({"media_frame":frames,"gap_us":now.duration_since(end).as_micros() as u64,
                "decode_wait_us":wait}));
            end = now;
        }
        end += Duration::from_secs_f64(count as f64 / rate as f64);
        deadline = Some(end);
        frames += count; packets += 1;
        if frames > rate as u64 * 120 { return Err("fixture exceeds two minutes".into()); }
        // Submit at most one decoded packet beyond this model's queue budget.
        // Each later decode must finish before these submitted frames drain.
        if let Some(wait) = end.checked_duration_since(Instant::now()).and_then(|d|d.checked_sub(queue)) {
            std::thread::sleep(wait);
        }
    }
    if let Some(end) = deadline { if let Some(wait) = end.checked_duration_since(Instant::now()) {
        std::thread::sleep(wait);
    }}
    Ok(json!({"sink":"paced decoder with modeled queue, not ALSA", "hardware_audio":false,
        "queue_ms":spec.cadence_ms,"range_delay_ms":spec.first_byte_ms,"frames":frames,"rate":rate,
        "pcm_md5":format!("{:x}",hash.finalize()),"packets":packets,"missed_deadlines":missed,
        "max_decode_wait_us":reads.iter().max(),"requests":*requests.lock().unwrap()}))
}
