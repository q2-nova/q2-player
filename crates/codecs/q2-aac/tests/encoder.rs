//! Encoder behaviour: access-unit sizes, committed output hashes, and an
//! environment-gated dump for the FFmpeg decode check (README.q2.md).

mod common;

use q2_aac::{Encoder, latm, unlatm};

const FRAME: usize = Encoder::FRAME_SAMPLES;

/// Encodes whole frames of `pcm`; returns the access units.
fn encode(rate: u32, bitrate: u32, pcm: &[i16]) -> Vec<Vec<u8>> {
    let mut encoder = Encoder::new(rate, bitrate).unwrap();
    let mut units = Vec::new();
    for frame in pcm.as_chunks::<{ FRAME * 2 }>().0 {
        let mut unit = vec![0xEE]; // encode appends
        let length = encoder.encode(frame, &mut unit).unwrap();
        assert_eq!(unit.len(), 1 + length);
        assert_eq!(unit[0], 0xEE);
        units.push(unit.split_off(1));
    }
    units
}

fn fnv1a(units: &[Vec<u8>]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for &byte in units.iter().flatten() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[test]
fn rejects_bad_arguments() {
    assert!(Encoder::new(32000, 256_000).is_err());
    assert!(Encoder::new(44100, 400_000).is_err());
    assert!(Encoder::new(44100, 0).is_err());
    let mut encoder = Encoder::new(44100, 256_000).unwrap();
    let mut out = vec![7];
    assert!(encoder.encode(&[0; 2046], &mut out).is_err());
    assert!(encoder.encode(&[0; 2050], &mut out).is_err());
    assert_eq!(out, [7]);
}

#[test]
fn audio_specific_config() {
    assert_eq!(
        Encoder::new(44100, 256_000)
            .unwrap()
            .audio_specific_config(),
        [0x12, 0x10]
    );
    assert_eq!(
        Encoder::new(48000, 256_000)
            .unwrap()
            .audio_specific_config(),
        [0x11, 0x90]
    );
}

#[test]
fn encoder_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<Encoder>();
}

#[test]
fn access_unit_sizes_track_the_bitrate() {
    for rate in [44100u32, 48000] {
        let pcm = common::signal(rate as usize * 10);
        for bitrate in [128_000u32, 192_000, 256_000, 320_000] {
            let units = encode(rate, bitrate, &pcm);
            assert_eq!(units.len(), rate as usize * 10 / FRAME);
            let bytes: usize = units.iter().map(Vec::len).sum();
            let measured = bytes as f64 * 8.0 * f64::from(rate) / (units.len() * FRAME) as f64;
            let ratio = measured / f64::from(bitrate);
            assert!(
                (0.9..=1.1).contains(&ratio),
                "{rate} Hz {bitrate} bit/s: measured {measured:.0}"
            );
            // 6144 bits per channel bounds every access unit.
            assert!(units.iter().all(|u| !u.is_empty() && u.len() <= 1536));
        }
    }
}

/// The encoder is fixed point (integer C only), so these hashes do not depend on the
/// platform's floating point. They were recorded on x86_64; README.q2.md says what
/// was checked for mipsel.
#[test]
fn encoder_vectors() {
    let cases: [(u32, u32, u64); 4] = [
        (44100, 128_000, 0xd09277a3e26d8345),
        (44100, 256_000, 0x81d717d392b139fd),
        (44100, 320_000, 0xa170085c53b96bda),
        (48000, 256_000, 0x469a499183e4f419),
    ];
    let pcm = common::signal(FRAME * 100);
    for (rate, bitrate, expected) in cases {
        let units = encode(rate, bitrate, &pcm);
        assert_eq!(units.len(), 100);
        let hash = fnv1a(&units);
        assert_eq!(hash, expected, "{rate} Hz {bitrate} bit/s: {hash:#018x}");
    }
}

#[test]
fn every_access_unit_survives_latm() {
    let pcm = common::signal(FRAME * 50);
    for unit in encode(44100, 320_000, &pcm) {
        assert_eq!(unlatm(&latm(&unit, 44100)), Some(unit));
    }
}

/// 7-byte ADTS header (no CRC) for one AAC-LC stereo access unit.
fn adts_header(rate: u32, unit_len: usize) -> [u8; 7] {
    let index: u8 = if rate == 48000 { 3 } else { 4 };
    let length = unit_len + 7;
    [
        0xFF,
        0xF1,                                    // syncword, MPEG-4, layer 0, no CRC
        (1 << 6) | (index << 2),                 // profile LC, index, channel high bit 0
        (2 << 6) | ((length >> 11) as u8 & 0x3), // channel configuration 2, frame length
        (length >> 3) as u8,
        ((length as u8 & 0x7) << 5) | 0x1F, // buffer fullness 0x7FF
        0xFC,                               // one raw data block
    ]
}

/// `Q2_AAC_DUMP=<dir>` writes `input.s16` (10 s, 44.1 kHz stereo, little endian) and,
/// for 128, 256 and 320 kbit/s, `aac_<kbit>.adts` and `aac_<kbit>.loas` (each LATM
/// element behind a LOAS AudioSyncStream header) for the FFmpeg check in
/// build/codec-aac/ffmpeg_check.py. `Q2_AAC_INPUT=<file>` replaces the generated signal
/// with a headerless 44.1 kHz stereo s16le file. Without `Q2_AAC_DUMP` the test does
/// nothing.
#[test]
fn dump_for_ffmpeg() {
    let Some(dir) = std::env::var_os("Q2_AAC_DUMP") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    let rate = 44100;
    let pcm: Vec<i16> = match std::env::var_os("Q2_AAC_INPUT") {
        Some(path) => std::fs::read(path)
            .unwrap()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|b| i16::from_le_bytes(*b))
            .collect(),
        None => common::signal(rate as usize * 10),
    };
    let raw: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
    std::fs::write(dir.join("input.s16"), raw).unwrap();
    for kbit in [128u32, 256, 320] {
        let units = encode(rate, kbit * 1000, &pcm);
        let mut adts = Vec::new();
        let mut loas = Vec::new();
        for unit in &units {
            adts.extend_from_slice(&adts_header(rate, unit.len()));
            adts.extend_from_slice(unit);
            let element = latm(unit, rate);
            // AudioSyncStream: syncword 0x2B7 (11 bits), audioMuxLengthBytes (13 bits).
            let header = (0x2B7u32 << 13) | element.len() as u32;
            loas.extend_from_slice(&header.to_be_bytes()[1..]);
            loas.extend_from_slice(&element);
        }
        std::fs::write(dir.join(format!("aac_{kbit}.adts")), adts).unwrap();
        std::fs::write(dir.join(format!("aac_{kbit}.loas")), loas).unwrap();
    }
}
