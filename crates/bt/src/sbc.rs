//! SBC encoder (A2DP 1.3 appendix B): 8 subbands, 4-16 blocks, stereo or
//! joint stereo, loudness or SNR allocation. Arithmetic is IEEE f64 with
//! literal tables, so the output bytes are the same on every target.

/// Analysis prototype filter for 8 subbands (A2DP table 12.8).
#[rustfmt::skip]
const PROTO8: [f64; 80] = [
    0.00000000E+00, 1.56575398E-04, 3.43256425E-04, 5.54620202E-04, 8.23919506E-04, 1.13992507E-03, 1.47640169E-03, 1.78371725E-03,
    2.01182542E-03, 2.10371989E-03, 1.99454554E-03, 1.61656283E-03, 9.02154502E-04, -1.78805361E-04, -1.64973098E-03, -3.49717454E-03,
    5.65949473E-03, 8.02941163E-03, 1.04584443E-02, 1.27472335E-02, 1.46525263E-02, 1.59045603E-02, 1.62208471E-02, 1.53184106E-02,
    1.29371806E-02, 8.85757540E-03, 2.92408442E-03, -4.91578024E-03, -1.46404076E-02, -2.61098752E-02, -3.90751381E-02, -5.31873032E-02,
    6.79989431E-02, 8.29847578E-02, 9.75753918E-02, 1.11196689E-01, 1.23264548E-01, 1.33264415E-01, 1.40753505E-01, 1.45389847E-01,
    1.46955068E-01, 1.45389847E-01, 1.40753505E-01, 1.33264415E-01, 1.23264548E-01, 1.11196689E-01, 9.75753918E-02, 8.29847578E-02,
    -6.79989431E-02, -5.31873032E-02, -3.90751381E-02, -2.61098752E-02, -1.46404076E-02, -4.91578024E-03, 2.92408442E-03, 8.85757540E-03,
    1.29371806E-02, 1.53184106E-02, 1.62208471E-02, 1.59045603E-02, 1.46525263E-02, 1.27472335E-02, 1.04584443E-02, 8.02941163E-03,
    -5.65949473E-03, -3.49717454E-03, -1.64973098E-03, -1.78805361E-04, 9.02154502E-04, 1.61656283E-03, 1.99454554E-03, 2.10371989E-03,
    2.01182542E-03, 1.78371725E-03, 1.47640169E-03, 1.13992507E-03, 8.23919506E-04, 5.54620202E-04, 3.43256425E-04, 1.56575398E-04,
];
/// cos(n pi / 16) for n in 0..=8.
const COS16: [f64; 9] = [1.0, 0.980_785_280_403_230_4, 0.923_879_532_511_286_7, 0.831_469_612_302_545_2,
    0.707_106_781_186_547_6, 0.555_570_233_019_602_2, 0.382_683_432_365_089_8, 0.195_090_322_016_128_25, 0.0];
/// cos(n pi / 16) for any integer n.
pub(crate) fn cos16(n: i32) -> f64 {
    let m = n.rem_euclid(32);
    let m = if m > 16 { 32 - m } else { m };
    if m > 8 { -COS16[(16 - m) as usize] } else { COS16[m as usize] }
}
/// Loudness offsets for 8 subbands, by sampling frequency code.
const OFFSET8: [[i32; 8]; 4] = [[-2, 0, 0, 0, 0, 0, 0, 1], [-3, 0, 0, 0, 0, 0, 1, 2], [-4, 0, 0, 0, 0, 0, 1, 2], [-4, 0, 0, 0, 0, 0, 1, 2]];
pub const SUBBANDS: usize = 8;

/// A negotiated stream configuration. Two channels, eight subbands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Params { pub rate: u32, pub blocks: u8, pub joint: bool, pub snr: bool, pub bitpool: u8 }
impl Params {
    pub fn freq_code(&self) -> u8 { match self.rate { 16000 => 0, 32000 => 1, 44100 => 2, _ => 3 } }
    /// PCM frames consumed by one SBC frame.
    pub fn samples(&self) -> usize { self.blocks as usize * SUBBANDS }
    pub fn frame_len(&self) -> usize {
        4 + SUBBANDS + (usize::from(self.joint) * SUBBANDS + self.blocks as usize * self.bitpool as usize).div_ceil(8)
    }
}

/// Bit allocation for both channels sharing one bitpool (appendix B, 12.6.3.4).
pub(crate) fn allocate(p: &Params, scf: &[[u8; 8]; 2]) -> [[u8; 8]; 2] {
    let mut need = [[0i32; 8]; 2];
    let mut max_need = 0;
    for ch in 0..2 { for sb in 0..8 {
        let s = scf[ch][sb] as i32;
        need[ch][sb] = if p.snr { s } else if s == 0 { -5 } else {
            let loudness = s - OFFSET8[p.freq_code() as usize][sb];
            if loudness > 0 { loudness / 2 } else { loudness }
        };
        max_need = max_need.max(need[ch][sb]);
    } }
    let pool = p.bitpool as i32;
    let (mut count, mut slice_count, mut slice) = (0, 0, max_need + 1);
    loop {
        slice -= 1; count += slice_count; slice_count = 0;
        for ch in 0..2 { for sb in 0..8 {
            let n = need[ch][sb];
            if n > slice + 1 && n < slice + 16 { slice_count += 1 } else if n == slice + 1 { slice_count += 2 }
        } }
        if count + slice_count >= pool { break }
    }
    if count + slice_count == pool { count += slice_count; slice -= 1; }
    let mut bits = [[0u8; 8]; 2];
    for ch in 0..2 { for sb in 0..8 {
        bits[ch][sb] = if need[ch][sb] < slice + 2 { 0 } else { (need[ch][sb] - slice).min(16) as u8 };
    } }
    let (mut ch, mut sb) = (0, 0);
    while count < pool && sb < 8 {
        if (2..16).contains(&bits[ch][sb]) { bits[ch][sb] += 1; count += 1 }
        else if need[ch][sb] == slice + 1 && pool > count + 1 { bits[ch][sb] = 2; count += 2 }
        if ch == 1 { ch = 0; sb += 1 } else { ch = 1 }
    }
    let (mut ch, mut sb) = (0, 0);
    while count < pool && sb < 8 {
        if bits[ch][sb] < 16 { bits[ch][sb] += 1; count += 1 }
        if ch == 1 { ch = 0; sb += 1 } else { ch = 1 }
    }
    bits
}

pub(crate) fn crc8(header: [u8; 2], tail: &[u8], tail_bits: usize) -> u8 {
    let mut crc = 0x0fu8;
    let mut bit = |b: bool| { let top = crc & 0x80 != 0; crc <<= 1; if top != b { crc ^= 0x1d } };
    for byte in header { for i in (0..8).rev() { bit(byte >> i & 1 != 0) } }
    for i in 0..tail_bits { bit(tail[i / 8] >> (7 - i % 8) & 1 != 0) }
    crc
}

struct Bits<'a> { out: &'a mut Vec<u8>, acc: u32, n: u32 }
impl Bits<'_> {
    fn put(&mut self, value: u32, bits: u32) {
        self.acc = self.acc << bits | (value & ((1u32 << bits) - 1)); self.n += bits;
        while self.n >= 8 { self.n -= 8; self.out.push((self.acc >> self.n) as u8); }
    }
    fn flush(&mut self) { if self.n > 0 { let pad = 8 - self.n; self.put(0, pad); } }
}

fn scale_factor(max: f64) -> u8 {
    let mut scf = 0u8;
    while scf < 15 && max >= f64::from(1u32 << (scf + 1)) { scf += 1 }
    scf
}

pub struct Encoder {
    params: Params,
    /// The last 80 input samples of each channel, newest first.
    x: [[f64; 80]; 2],
    matrix: [[f64; 16]; 8],
}
impl Encoder {
    pub fn new(params: Params) -> Self {
        assert!(matches!(params.blocks, 4 | 8 | 12 | 16) && params.bitpool >= 2, "invalid SBC parameters");
        let mut matrix = [[0.0; 16]; 8];
        for (i, row) in matrix.iter_mut().enumerate() { for (k, m) in row.iter_mut().enumerate() {
            *m = cos16((2 * i as i32 + 1) * (k as i32 - 4));
        } }
        Self { params, x: [[0.0; 80]; 2], matrix }
    }
    pub fn params(&self) -> &Params { &self.params }
    /// Encodes `params.samples()` interleaved stereo frames and appends one SBC frame.
    pub fn encode(&mut self, pcm: &[i16], out: &mut Vec<u8>) {
        let p = self.params;
        let blocks = p.blocks as usize;
        assert_eq!(pcm.len(), blocks * 16, "one SBC frame of interleaved stereo");
        let mut sb = [[[0.0f64; 8]; 16]; 2];
        for blk in 0..blocks { for ch in 0..2 {
            let x = &mut self.x[ch];
            x.copy_within(0..72, 8);
            for i in 0..8 { x[i] = f64::from(pcm[(blk * 8 + 7 - i) * 2 + ch]); }
            let mut y = [0.0f64; 16];
            for (i, y) in y.iter_mut().enumerate() { for k in 0..5 { *y += PROTO8[i + k * 16] * x[i + k * 16]; } }
            for i in 0..8 { sb[ch][blk][i] = (0..16).map(|k| self.matrix[i][k] * y[k]).sum(); }
        } }
        let mut scf = [[0u8; 8]; 2];
        let peak = |sb: &[[[f64; 8]; 16]; 2], ch: usize, band: usize| (0..blocks).map(|b| sb[ch][b][band].abs()).fold(0.0, f64::max);
        for ch in 0..2 { for band in 0..8 { scf[ch][band] = scale_factor(peak(&sb, ch, band)); } }
        let mut join = 0u8;
        if p.joint { for band in 0..7 {
            let (mut mid, mut side) = (0.0f64, 0.0f64);
            for b in 0..blocks {
                mid = mid.max(((sb[0][b][band] + sb[1][b][band]) / 2.0).abs());
                side = side.max(((sb[0][b][band] - sb[1][b][band]) / 2.0).abs());
            }
            let (m, s) = (scale_factor(mid), scale_factor(side));
            if scf[0][band] + scf[1][band] > m + s {
                join |= 0x80 >> band; scf[0][band] = m; scf[1][band] = s;
                for b in 0..blocks {
                    let (l, r) = (sb[0][b][band], sb[1][b][band]);
                    sb[0][b][band] = (l + r) / 2.0; sb[1][b][band] = (l - r) / 2.0;
                }
            }
        } }
        let bits = allocate(&p, &scf);
        let start = out.len();
        let header = [p.freq_code() << 6 | (p.blocks / 4 - 1) << 4 | (if p.joint { 3 } else { 2 }) << 2 | u8::from(p.snr) << 1 | 1, p.bitpool];
        out.extend_from_slice(&[0x9c, header[0], header[1], 0]);
        let mut w = Bits { out, acc: 0, n: 0 };
        if p.joint { w.put(join.into(), 8); }
        for ch in 0..2 { for band in 0..8 { w.put(scf[ch][band].into(), 4); } }
        let tail_bits = usize::from(p.joint) * 8 + 64;
        debug_assert_eq!(w.n, 0);
        let crc = crc8(header, &w.out[start + 4..], tail_bits);
        w.out[start + 3] = crc;
        for blk in 0..blocks { for ch in 0..2 { for band in 0..8 {
            let n = bits[ch][band] as u32;
            if n == 0 { continue }
            let levels = f64::from((1u32 << n) - 1);
            let norm = sb[ch][blk][band] / f64::from(1u32 << (scf[ch][band] + 1));
            let q = ((norm + 1.0) * levels / 2.0).floor().clamp(0.0, levels);
            w.put(q as u32, n);
        } } }
        w.flush();
        debug_assert_eq!(out.len() - start, p.frame_len());
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A decoder for these frames, written from the synthesis side of the spec.
    pub(crate) struct Decoder { v: [[f64; 160]; 2] }
    impl Decoder {
        pub(crate) fn new() -> Self { Self { v: [[0.0; 160]; 2] } }
        pub(crate) fn decode(&mut self, frame: &[u8], pcm: &mut Vec<i16>) -> Params {
            assert_eq!(frame[0], 0x9c);
            let rate = [16000, 32000, 44100, 48000][(frame[1] >> 6) as usize];
            let blocks = ((frame[1] >> 4 & 3) + 1) * 4;
            let mode = frame[1] >> 2 & 3;
            assert!(mode >= 2 && frame[1] & 1 == 1, "stereo, 8 subbands");
            let p = Params { rate, blocks, joint: mode == 3, snr: frame[1] & 2 != 0, bitpool: frame[2] };
            assert_eq!(frame.len(), p.frame_len());
            let mut pos = 32;
            let mut get = |n: usize| { let mut v = 0u32; for _ in 0..n { v = v << 1 | u32::from(frame[pos / 8] >> (7 - pos % 8) & 1); pos += 1; } v };
            let join = if p.joint { get(8) as u8 } else { 0 };
            let mut scf = [[0u8; 8]; 2];
            for ch in 0..2 { for band in 0..8 { scf[ch][band] = get(4) as u8; } }
            assert_eq!(crc8([frame[1], frame[2]], &frame[4..], usize::from(p.joint) * 8 + 64), frame[3], "CRC");
            let bits = allocate(&p, &scf);
            let mut sb = [[[0.0f64; 8]; 16]; 2];
            for blk in 0..blocks as usize { for ch in 0..2 { for band in 0..8 {
                let n = bits[ch][band] as usize;
                if n == 0 { continue }
                let levels = f64::from((1u32 << n) - 1);
                sb[ch][blk][band] = f64::from(1u32 << (scf[ch][band] + 1)) * ((f64::from(get(n)) * 2.0 + 1.0) / levels - 1.0);
            } } }
            for blk in 0..blocks as usize {
                for band in 0..8 { if join & (0x80 >> band) != 0 {
                    let (m, s) = (sb[0][blk][band], sb[1][blk][band]);
                    sb[0][blk][band] = m + s; sb[1][blk][band] = m - s;
                } }
                let mut frame_pcm = [[0i16; 8]; 2];
                for ch in 0..2 {
                    let v = &mut self.v[ch];
                    v.copy_within(0..144, 16);
                    for k in 0..16 { v[k] = (0..8).map(|i| cos16((2 * i as i32 + 1) * (k as i32 + 4)) * sb[ch][blk][i]).sum(); }
                    for j in 0..8 {
                        let mut x = 0.0;
                        for i in 0..5 {
                            x += v[i * 32 + j] * -8.0 * PROTO8[i * 16 + j];
                            x += v[i * 32 + 24 + j] * -8.0 * PROTO8[i * 16 + 8 + j];
                        }
                        frame_pcm[ch][j] = x.round().clamp(-32768.0, 32767.0) as i16;
                    }
                }
                for j in 0..8 { pcm.push(frame_pcm[0][j]); pcm.push(frame_pcm[1][j]); }
            }
            p
        }
    }

    /// Two tones and noise from integer arithmetic: the vectors do not depend on libm.
    pub(crate) fn tone(frames: usize) -> Vec<i16> {
        let mut out = Vec::with_capacity(frames * 2);
        let (mut a, mut b, mut seed) = ((12000i64, 0i64), (0i64, 9000i64), 0x2545_f491u32);
        // Rotations by fixed angles: about 1 kHz and 3.1 kHz at 44.1 kHz.
        let rot = |(x, y): (i64, i64), c: i64, s: i64| ((x * c - y * s) >> 15, (x * s + y * c) >> 15);
        for _ in 0..frames {
            a = rot(a, 32435, 4653); b = rot(b, 29621, 14010);
            seed ^= seed << 13; seed ^= seed >> 17; seed ^= seed << 5;
            let noise = (seed >> 22) as i64 - 512;
            out.push((a.0 + b.1 / 2 + noise) as i16); out.push((a.1 - b.0 / 2 - noise) as i16);
        }
        out
    }

    fn snr(p: Params) -> f64 {
        let pcm = tone(p.samples() * 60 + 4000);
        let (mut enc, mut dec) = (Encoder::new(p), Decoder::new());
        let mut decoded = Vec::new();
        for chunk in pcm.chunks_exact(p.samples() * 2) {
            let mut frame = Vec::new();
            enc.encode(chunk, &mut frame);
            assert_eq!(frame.len(), p.frame_len());
            assert_eq!(dec.decode(&frame, &mut decoded), p);
        }
        // Analysis plus synthesis delay is 73 samples for 8 subbands.
        let delay = 73;
        let (mut signal, mut noise) = (0.0f64, 0.0f64);
        for i in 0..decoded.len() / 2 - 2000 { for ch in 0..2 {
            let (a, b) = (f64::from(pcm[(i + 2000 - delay) * 2 + ch]), f64::from(decoded[(i + 2000) * 2 + ch]));
            signal += a * a; noise += (a - b) * (a - b);
        } }
        10.0 * (signal / noise).log10()
    }

    #[test]
    fn frame_lengths_match_the_a2dp_recommended_settings() {
        assert_eq!(Params { rate: 44100, blocks: 16, joint: true, snr: false, bitpool: 53 }.frame_len(), 119);
        assert_eq!(Params { rate: 44100, blocks: 16, joint: false, snr: false, bitpool: 53 }.frame_len(), 118);
        assert_eq!(Params { rate: 48000, blocks: 16, joint: true, snr: false, bitpool: 35 }.frame_len(), 83);
    }

    #[test]
    fn decoded_audio_matches_the_input_in_every_mode() {
        for (joint, snr_alloc, bitpool, blocks, floor) in [(true, false, 53, 16, 30.0), (false, false, 53, 16, 30.0),
            (true, true, 53, 16, 28.0), (true, false, 35, 16, 20.0), (false, false, 53, 8, 28.0), (true, false, 20, 4, 6.0)] {
            for rate in [44100, 48000] {
                let p = Params { rate, blocks, joint, snr: snr_alloc, bitpool };
                let db = snr(p);
                assert!(db > floor, "{p:?}: {db:.1} dB");
            }
        }
    }

    #[test]
    fn allocation_never_exceeds_the_bitpool() {
        let mut seed = 7u32;
        for _ in 0..2000 {
            let mut scf = [[0u8; 8]; 2];
            for ch in 0..2 { for band in 0..8 { seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345); scf[ch][band] = (seed >> 16) as u8 % 16; } }
            for bitpool in [2u8, 17, 35, 53, 64, 128, 250] { for snr in [false, true] {
                let p = Params { rate: 44100, blocks: 16, joint: true, snr, bitpool };
                let bits = allocate(&p, &scf);
                let total: u32 = bits.iter().flatten().map(|&b| b as u32).sum();
                assert!(bits.iter().flatten().all(|&b| b <= 16));
                assert!(total <= bitpool as u32, "{total} of {bitpool}");
            } }
        }
    }

    fn stream(p: Params, frames: usize) -> (Vec<i16>, Vec<u8>) {
        let pcm = tone(p.samples() * frames);
        let mut enc = Encoder::new(p);
        let mut out = Vec::new();
        for chunk in pcm.chunks(p.samples() * 2) { enc.encode(chunk, &mut out); }
        (pcm, out)
    }
    const VECTOR_PARAMS: [(&str, Params); 3] = [
        ("joint53", Params { rate: 44100, blocks: 16, joint: true, snr: false, bitpool: 53 }),
        ("stereo51", Params { rate: 48000, blocks: 16, joint: false, snr: false, bitpool: 51 }),
        ("snr35", Params { rate: 44100, blocks: 16, joint: true, snr: true, bitpool: 35 }),
    ];

    /// FNV-1a of 40 encoded frames of the generated input. FFmpeg's SBC decoder
    /// decoded these streams to the input (docs/bluetooth.md has the figures).
    #[test]
    fn encoder_vectors() {
        let got: Vec<u64> = VECTOR_PARAMS.iter().map(|(_, p)| stream(*p, 40).1.iter()
            .fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3))).collect();
        assert_eq!(got, VECTORS, "{got:#x?}");
    }
    const VECTORS: [u64; 3] = [0x423c_5ec8_a616_2e01, 0xad18_6ca0_299a_78da, 0xcbd9_1813_a7be_6318];

    /// Writes the vector streams for an external decoder:
    /// `Q2_SBC_DUMP=dir cargo test -p q2-bt dump_streams`.
    #[test]
    fn dump_streams_for_an_external_decoder() {
        let Ok(dir) = std::env::var("Q2_SBC_DUMP") else { return };
        for (name, p) in VECTOR_PARAMS {
            let (pcm, out) = stream(p, 400);
            std::fs::write(format!("{dir}/{name}.sbc"), out).unwrap();
            std::fs::write(format!("{dir}/{name}.s16"), pcm.iter().flat_map(|s| s.to_le_bytes()).collect::<Vec<_>>()).unwrap();
        }
    }
}
