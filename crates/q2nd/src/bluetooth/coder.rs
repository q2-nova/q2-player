#![forbid(unsafe_code)]
//! The negotiated codec's encoder and its media packet framing (A2DP):
//! SBC and LDAC count frames in a one-byte header, AAC is one LATM element
//! per access unit (fragmented to the MTU, marker on the last fragment),
//! aptX and aptX HD are bare codewords.
use q2_bt::codec::Codec;
use q2_bt::host::Stream;
use q2_bt::sbc;

pub use q2_ldac::Quality as LdacQuality;

/// RTP header bytes in every packet except aptX's.
const RTP: usize = 12;

pub enum Coder {
    Sbc { encoder: sbc::Encoder, per_packet: usize },
    Aac { encoder: q2_aac::Encoder, rate: u32, room: usize },
    AptX { encoder: q2_aptx::Encoder, frames: usize },
    Ldac { encoder: q2_ldac::Encoder, unit: usize },
}

impl Coder {
    pub fn new(stream: Stream, ldac: LdacQuality) -> Result<Coder, String> {
        let mtu = stream.mtu;
        Ok(match stream.codec {
            Codec::Sbc(params) => Coder::Sbc { encoder: sbc::Encoder::new(params), per_packet: (mtu.saturating_sub(RTP + 1) / params.frame_len()).clamp(1, 15) },
            Codec::Aac { rate, bitrate } => Coder::Aac { encoder: q2_aac::Encoder::new(rate, bitrate)?, rate, room: mtu.saturating_sub(RTP).max(64) },
            Codec::AptX { rate } | Codec::AptXHd { rate } => {
                let hd = matches!(stream.codec, Codec::AptXHd { .. });
                // About 10 ms a packet, in whole codewords (4 PCM frames each), within the MTU.
                let (bytes, header) = if hd { (6, RTP) } else { (4, 0) };
                let fit = mtu.saturating_sub(header) / bytes * 4;
                Coder::AptX { encoder: q2_aptx::Encoder::new(hd)?, frames: (rate as usize / 100 / 4 * 4).min(fit).max(4) }
            }
            Codec::Ldac { rate } => {
                let encoder = q2_ldac::Encoder::new(rate, ldac, mtu)?;
                let unit = encoder.frame_samples();
                Coder::Ldac { encoder, unit }
            }
        })
    }
    /// PCM frames a call to `encode` must be given whole multiples of.
    pub fn unit(&self) -> usize {
        match self { Coder::Sbc { encoder, .. } => encoder.params().samples(), Coder::Aac { .. } => q2_aac::Encoder::FRAME_SAMPLES, Coder::AptX { .. } => q2_aptx::Encoder::FRAME_SAMPLES, Coder::Ldac { unit, .. } => *unit }
    }
    /// PCM frames per pacing step: one media packet's worth where the codec lets us decide.
    pub fn chunk(&self) -> usize {
        match self { Coder::Sbc { per_packet, .. } => self.unit() * per_packet, Coder::AptX { frames, .. } => *frames, _ => self.unit() }
    }

    /// LDAC's place in `q2_ldac::LEVELS` (0 the highest rate); None for the other codecs.
    pub fn ldac_level(&mut self) -> Option<u8> {
        match self { Coder::Ldac { encoder, .. } => Some(encoder.level()), _ => None }
    }
    /// Changes LDAC's bit rate from the next frame (adaptive bit rate). Other codecs: no change.
    pub fn set_ldac_level(&mut self, level: u8) -> Result<(), String> {
        match self { Coder::Ldac { encoder, .. } => encoder.set_level(level), _ => Ok(()) }
    }

    /// Encodes interleaved stereo PCM, a whole number of units and at most one chunk.
    /// `send(payload, samples, marker)` is called for each media packet produced.
    pub fn encode(&mut self, pcm: &[i16], send: &mut dyn FnMut(&[u8], u32, bool)) -> Result<(), String> {
        let frames = pcm.len() / 2;
        match self {
            Coder::Sbc { encoder, .. } => {
                let unit = encoder.params().samples();
                let mut payload = vec![(frames / unit) as u8 & 0x0f];
                for frame in pcm.chunks_exact(unit * 2) { encoder.encode(frame, &mut payload); }
                send(&payload, frames as u32, false);
            }
            Coder::Aac { encoder, rate, room } => {
                for unit in pcm.chunks_exact(q2_aac::Encoder::FRAME_SAMPLES * 2) {
                    let mut access_unit = Vec::new();
                    encoder.encode(unit, &mut access_unit)?;
                    let element = q2_aac::latm(&access_unit, *rate);
                    let count = element.chunks(*room).count();
                    // The timestamp advances once per access unit: on its first fragment.
                    for (i, fragment) in element.chunks(*room).enumerate() {
                        send(fragment, if i == 0 { q2_aac::Encoder::FRAME_SAMPLES as u32 } else { 0 }, i + 1 == count);
                    }
                }
            }
            Coder::AptX { encoder, .. } => {
                let mut payload = Vec::with_capacity(frames * 3 / 2);
                encoder.encode(pcm, &mut payload);
                send(&payload, frames as u32, false);
            }
            Coder::Ldac { encoder, unit } => {
                for block in pcm.chunks_exact(*unit * 2) {
                    if let Some(packet) = encoder.encode(block)? {
                        let mut payload = Vec::with_capacity(1 + packet.data.len());
                        payload.push(packet.frames & 0x0f);
                        payload.extend_from_slice(&packet.data);
                        send(&payload, u32::from(packet.frames) * *unit as u32, false);
                    }
                }
            }
        }
        Ok(())
    }
}

/// The stream's bit rate in kbit/s; for LDAC at `ldac_level` of `q2_ldac::LEVELS`.
pub fn kbps(codec: &Codec, ldac_level: Option<u8>) -> u32 {
    match *codec {
        Codec::Sbc(params) => (params.frame_len() as u64 * 8 * u64::from(params.rate) / params.samples() as u64 / 1000) as u32,
        Codec::Aac { bitrate, .. } => bitrate / 1000,
        // 4 bits per sample, two channels; aptX HD 6.
        Codec::AptX { rate } => rate * 8 / 1000,
        Codec::AptXHd { rate } => rate * 12 / 1000,
        Codec::Ldac { rate } => {
            let level = q2_ldac::LEVELS[usize::from(ldac_level.unwrap_or(1)).min(q2_ldac::LEVELS.len() - 1)];
            if rate % 48_000 == 0 { level.1 } else { level.2 }
        }
    }
}

/// Deterministic stereo test audio: two tones and noise, from integer arithmetic.
pub fn tone(frames: usize) -> Vec<i16> {
    let mut out = Vec::with_capacity(frames * 2);
    let (mut a, mut b, mut seed) = ((12000i64, 0i64), (0i64, 9000i64), 0x2545_f491u32);
    let rot = |(x, y): (i64, i64), c: i64, s: i64| ((x * c - y * s) >> 15, (x * s + y * c) >> 15);
    for _ in 0..frames {
        a = rot(a, 32435, 4653); b = rot(b, 29621, 14010);
        seed ^= seed << 13; seed ^= seed >> 17; seed ^= seed << 5;
        let noise = (seed >> 22) as i64 - 512;
        out.push((a.0 + b.1 / 2 + noise) as i16); out.push((a.1 - b.0 / 2 - noise) as i16);
    }
    out
}

/// Encodes `seconds` of test audio with `stream`'s codec; returns (packets, payload bytes, PCM frames accounted).
pub fn run(stream: Stream, ldac: LdacQuality, seconds: u32) -> Result<(u32, u64, u64), String> {
    let mut coder = Coder::new(stream, ldac)?;
    let (chunk, rate) = (coder.chunk(), stream.codec.rate() as usize);
    let pcm = tone(chunk * (rate / chunk + 1));
    let (mut packets, mut bytes, mut samples) = (0u32, 0u64, 0u64);
    let mut fed = 0usize;
    while fed < rate * seconds as usize {
        for block in pcm.chunks_exact(chunk * 2) {
            coder.encode(block, &mut |payload, advance, _| { packets += 1; bytes += payload.len() as u64; samples += u64::from(advance); })?;
            fed += chunk;
        }
    }
    Ok((packets, bytes, samples))
}

#[cfg(test)]
mod tests {
    use super::*;
    use q2_bt::codec::{self, Kind};

    fn stream(kind: Kind, mtu: usize) -> Stream { Stream { codec: codec::choose(&codec::capabilities(kind), false).unwrap(), mtu } }

    #[test]
    fn every_codec_fills_packets_within_the_mtu_and_accounts_for_every_sample() {
        for mtu in [895usize, 672, 300] {
            for kind in Kind::BY_QUALITY {
                // libldac needs a 679-byte media MTU (it sizes packets for 2-DH5).
                if kind == Kind::Ldac && mtu < 679 { assert!(Coder::new(stream(kind, mtu), LdacQuality::Standard).is_err()); continue }
                let stream = stream(kind, mtu);
                let mut coder = Coder::new(stream, LdacQuality::Standard).unwrap();
                let (chunk, unit) = (coder.chunk(), coder.unit());
                assert!(chunk % unit == 0 && chunk > 0, "{kind:?}");
                let pcm = tone(chunk * 60);
                let header = if stream.codec.rtp() { RTP } else { 0 };
                let (mut sizes, mut samples, mut markers) = (Vec::new(), 0u64, 0u32);
                for block in pcm.chunks_exact(chunk * 2) {
                    coder.encode(block, &mut |payload, advance, marker| { sizes.push(payload.len()); samples += u64::from(advance); markers += u32::from(marker); }).unwrap();
                }
                assert!(!sizes.is_empty() && sizes.iter().all(|&size| size > 0 && size + header <= mtu), "{kind:?} at {mtu}: {:?}", sizes.iter().max());
                // LDAC holds frames until a packet is full, so the last few blocks may still be inside it.
                let pending = if kind == Kind::Ldac { 16 * unit as u64 } else { 0 };
                assert!(samples <= (chunk * 60) as u64 && samples + pending >= (chunk * 60) as u64, "{kind:?} at {mtu}: {samples} of {}", chunk * 60);
                assert_eq!(markers > 0, kind == Kind::Aac, "{kind:?}");
            }
        }
    }

    #[test]
    fn aac_fragments_reassemble_to_one_latm_element_per_access_unit() {
        let mut coder = Coder::new(stream(Kind::Aac, 300), LdacQuality::Standard).unwrap();
        let pcm = tone(1024 * 20);
        let (mut current, mut units, mut fragmented) = (Vec::new(), 0, 0);
        for block in pcm.chunks_exact(2048) {
            let mut parts = 0;
            coder.encode(block, &mut |payload, advance, marker| {
                assert_eq!(advance, if current.is_empty() { 1024 } else { 0 });
                current.extend_from_slice(payload); parts += 1;
                if marker { assert!(q2_aac::unlatm(&current).is_some_and(|unit| !unit.is_empty())); current.clear(); units += 1; }
            }).unwrap();
            fragmented += u32::from(parts > 1);
        }
        assert_eq!(units, 20);
        assert!(fragmented > 0, "256 kbit/s access units exceed a 288-byte payload");
    }

    #[test]
    fn bit_rates_are_what_each_codec_states() {
        for (kind, low, high) in [(Kind::Sbc, 320_000, 345_000), (Kind::AptX, 352_000, 353_600), (Kind::AptXHd, 529_000, 530_000), (Kind::Aac, 230_000, 290_000), (Kind::Ldac, 560_000, 680_000)] {
            let (_, bytes, samples) = run(stream(kind, 895), LdacQuality::Standard, 4).unwrap();
            let bitrate = bytes * 8 * 44100 / samples.max(1);
            assert!((low..=high).contains(&bitrate), "{kind:?}: {bitrate} bit/s");
        }
    }
}
