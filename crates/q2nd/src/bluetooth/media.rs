#![forbid(unsafe_code)]
//! The stream being fed: PCM from the endpoint, rate conversion, the
//! encoder, pacing ([`super::pacing::Pacer`]) and LDAC's adaptive bit rate
//! ([`super::pacing::Abr`]).
use super::coder;
use super::pacing::{Abr, Pacer};
use super::pcm;
use q2_bt::host::{Command, Host, Stream};
use q2_bt::resample::Resampler;
use std::time::{Duration, Instant};

/// How often the ABR looks at the queue.
const ABR_MS: u64 = 20;

/// Numbers for telemetry and the probe.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MediaStats {
    /// Packets handed over more than 40 ms behind schedule.
    pub late: u32,
    /// Schedule restarts after falling more than 300 ms behind (the headphones ran dry).
    pub restarts: u32,
    pub behind_max_ms: u32,
    /// LDAC: the level in use (0 the highest rate) and level changes.
    pub ldac_level: Option<u8>,
    pub ldac_changes: u32,
    /// The stream's bit rate now.
    pub kbps: u32,
    /// The deepest queue between the encoder and the air seen, in packets.
    pub queue_max: u32,
    pub encode_errors: u32,
    /// The slowest encode of one chunk, in µs.
    pub encode_max_us: u32,
}

pub(super) struct Media {
    stream: Stream,
    coder: coder::Coder,
    /// The stream's sampling rate.
    rate: u32,
    resampler: Option<(u32, Resampler)>,
    serial: u64,
    /// Stereo samples at the stream's rate, not yet encoded.
    staged: Vec<i16>,
    source_rate: u32,
    pacer: Pacer,
    abr: Option<Abr>,
    abr_at: u64,
    start_requested: bool,
    idle_since: Option<Instant>,
    encode_errors: u32,
    queue_max: u32,
    encode_max_us: u32,
    /// LDAC level changes not yet taken by telemetry: (level, kbit/s).
    level_changes: Vec<(u8, u32)>,
}

impl Media {
    /// `adaptive`: LDAC lowers its rate under load, never above `ldac`.
    pub(super) fn new(stream: Stream, ldac: coder::LdacQuality, adaptive: bool) -> Result<Self, String> {
        let rate = stream.codec.rate();
        let mut coder = coder::Coder::new(stream, ldac)?;
        let abr = coder.ldac_level().filter(|_| adaptive).map(|ceiling| Abr::new(ceiling, (q2_ldac::LEVELS.len() - 1) as u8));
        Ok(Self { stream, coder, rate, resampler: None, serial: 0, staged: Vec::new(), source_rate: rate, pacer: Pacer::new(rate), abr, abr_at: 0,
            start_requested: false, idle_since: None, encode_errors: 0, queue_max: 0, encode_max_us: 0, level_changes: Vec::new() })
    }

    pub(super) fn stats(&mut self) -> MediaStats {
        let ldac_level = self.coder.ldac_level();
        MediaStats { late: self.pacer.late, restarts: self.pacer.restarts, behind_max_ms: (self.pacer.behind_max_us / 1000).min(u64::from(u32::MAX)) as u32,
            ldac_level, ldac_changes: self.abr.as_ref().map_or(0, |abr| abr.changes), kbps: coder::kbps(&self.stream.codec, ldac_level),
            queue_max: self.queue_max, encode_errors: self.encode_errors, encode_max_us: self.encode_max_us }
    }
    /// A stall that ended: worst lateness in ms and whether the schedule restarted.
    pub(super) fn take_stall(&mut self) -> Option<(u32, bool)> { self.pacer.take_stall().map(|(us, restarted)| ((us / 1000) as u32, restarted)) }
    pub(super) fn take_level_changes(&mut self) -> Vec<(u8, u32)> { std::mem::take(&mut self.level_changes) }

    /// One media packet's audio in microseconds at the current rate (LDAC packets are 660 bytes).
    fn packet_us(&mut self) -> u64 {
        match self.stream.codec {
            q2_bt::codec::Codec::Ldac { .. } => 5_280_000 / u64::from(coder::kbps(&self.stream.codec, self.coder.ldac_level()).max(1)),
            _ => self.coder.chunk() as u64 * 1_000_000 / u64::from(self.rate),
        }
    }

    /// Moves audio from the endpoint to the host. Returns when to run again, in microseconds.
    pub(super) fn tick(&mut self, host: &mut Host, endpoint: &pcm::Endpoint, now_us: u64) -> Option<u64> {
        let running = endpoint.running();
        if !running {
            self.pacer.reset();
            self.staged.clear();
            endpoint.set_staged(0);
            // Keep the stream up across track changes and seeks; suspend when playback has stopped.
            let idle = *self.idle_since.get_or_insert_with(Instant::now);
            if host.streaming() && idle.elapsed() >= Duration::from_secs(2) && self.start_requested { self.start_requested = false; host.command(Command::Suspend); }
            return None;
        }
        self.idle_since = None;
        if !host.streaming() {
            if !self.start_requested { self.start_requested = true; host.command(Command::Start); }
            return Some(now_us + 20_000);
        }
        // One step is one chunk of PCM: a media packet's worth for SBC and aptX, one codec frame for AAC and LDAC.
        let (chunk, unit) = (self.coder.chunk(), self.coder.unit());
        while host.media_room() && self.pacer.due(now_us) {
            let mut flush = false;
            while self.staged.len() < chunk * 2 {
                let wanted = chunk - self.staged.len() / 2;
                let source_frames = (wanted as u64 * u64::from(self.source_rate)).div_ceil(u64::from(self.rate)) as usize + 4;
                let Some(taken) = endpoint.take(source_frames) else { flush = true; break };
                if taken.serial != self.serial || taken.rate != self.source_rate {
                    // A new PCM or a flush: nothing staged from the old one may be heard.
                    self.serial = taken.serial; self.source_rate = taken.rate; self.staged.clear();
                    self.resampler = (taken.rate != self.rate).then(|| (taken.rate, Resampler::new(taken.rate, self.rate)));
                }
                match &mut self.resampler { Some((_, resampler)) => resampler.process(&taken.stereo, &mut self.staged), None => self.staged.extend_from_slice(&taken.stereo) }
                if taken.underrun { flush = true; break }
            }
            if flush {
                // The end of the audio: pad the last codec frame with silence so the tail is heard.
                let padded = self.staged.len().div_ceil(unit * 2) * unit * 2;
                self.staged.resize(padded, 0);
            }
            let frames = (self.staged.len() / 2).min(chunk) / unit * unit;
            if frames == 0 { break }
            let started = Instant::now();
            if self.coder.encode(&self.staged[..frames * 2], &mut |payload, samples, marker| host.send_media_payload(payload, samples, marker)).is_err() { self.encode_errors += 1; }
            self.encode_max_us = self.encode_max_us.max(started.elapsed().as_micros().min(u128::from(u32::MAX)) as u32);
            self.staged.drain(..frames * 2);
            self.pacer.sent(frames as u64, now_us);
            if flush { break }
        }
        self.adapt(host, now_us);
        endpoint.set_staged((self.staged.len() as u64 / 2 * u64::from(self.source_rate)) / u64::from(self.rate));
        Some(self.pacer.next_us(now_us))
    }

    /// The queue between the encoder and the air, in packets: what the host and
    /// the controller hold, plus the schedule's lateness in packets.
    fn depth(&mut self, host: &Host, now_us: u64) -> u32 {
        let behind = self.pacer.behind_us(now_us).max(0) as u64;
        (host.acl_backlog() as u64 + behind / self.packet_us().max(1)).min(u64::from(u32::MAX)) as u32
    }

    fn adapt(&mut self, host: &Host, now_us: u64) {
        let now_ms = now_us / 1000;
        if now_ms < self.abr_at { return }
        self.abr_at = now_ms + ABR_MS;
        let depth = self.depth(host, now_us);
        self.queue_max = self.queue_max.max(depth);
        let Some(level) = self.abr.as_mut().and_then(|abr| abr.observe(now_ms, depth)) else { return };
        match self.coder.set_ldac_level(level) {
            Ok(()) => {
                let kbps = coder::kbps(&self.stream.codec, Some(level));
                log::info!("Bluetooth: LDAC {} kbit/s (queue {depth} packets)", kbps);
                if self.level_changes.len() < 16 { self.level_changes.push((level, kbps)); }
            }
            Err(error) => { log::warn!("Bluetooth: LDAC rate change: {error}"); self.abr = None; }
        }
    }
}
