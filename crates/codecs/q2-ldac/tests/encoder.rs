//! Host tests for the LDAC encoder wrapper. None of them needs a decoder.

#[path = "common/signal.rs"]
mod signal;

use q2_ldac::{Encoder, Packet, Quality};
use signal::Signal;

const QUALITIES: [Quality; 3] = [Quality::High, Quality::Standard, Quality::Mobile];
const RATES: [u32; 4] = [44_100, 48_000, 88_200, 96_000];
/// RTP header plus the one-byte LDAC media payload header.
const PACKET_OVERHEAD: usize = 12 + 1;
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// Feeds `calls` blocks of the test signal and returns the packets with the
/// PCM that went in.
fn run(rate: u32, quality: Quality, mtu: usize, calls: usize) -> (Vec<Packet>, Vec<i16>) {
    let mut enc = Encoder::new(rate, quality, mtu).expect("encoder");
    let mut signal = Signal::new(rate);
    let mut block = vec![0i16; enc.frame_samples() * 2];
    let mut packets = Vec::new();
    let mut pcm = Vec::new();
    for _ in 0..calls {
        signal.fill(&mut block);
        pcm.extend_from_slice(&block);
        if let Some(packet) = enc.encode(&block).expect("encode") {
            packets.push(packet);
        }
    }
    (packets, pcm)
}

/// FNV-1a, 64-bit, over the concatenated payloads.
fn payload_hash(packets: &[Packet]) -> u64 {
    let mut hash = FNV_OFFSET;
    for &b in packets.iter().flat_map(|p| p.data.iter()) {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

fn frames_per_packet(quality: Quality) -> usize {
    match quality {
        Quality::High => 2,
        Quality::Standard => 3,
        Quality::Mobile => 6,
    }
}

#[test]
fn encoder_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<Encoder>();
}

#[test]
fn frame_samples_and_bitrates() {
    for rate in RATES {
        for (quality, kbps_48, kbps_44) in [
            (Quality::High, 990, 909),
            (Quality::Standard, 660, 606),
            (Quality::Mobile, 330, 303),
        ] {
            let enc = Encoder::new(rate, quality, 679).expect("encoder");
            let (samples, kbps) = match rate {
                44_100 => (128, kbps_44),
                48_000 => (128, kbps_48),
                88_200 => (256, kbps_44),
                _ => (256, kbps_48),
            };
            assert_eq!(enc.frame_samples(), samples, "{quality:?} {rate}");
            assert_eq!(enc.bitrate(), kbps, "{quality:?} {rate}");
        }
    }
}

#[test]
fn packets_respect_mtu_and_frame_counts() {
    const CALLS: usize = 600;
    for mtu in [679, 895, 1005] {
        for rate in RATES {
            for quality in QUALITIES {
                let per_packet = frames_per_packet(quality);
                let frame_len = 660 / per_packet;
                let (packets, _) = run(rate, quality, mtu, CALLS);
                assert!(!packets.is_empty());
                // One LDAC frame per call, a fixed number of frames per packet.
                assert!(
                    packets.len() <= CALLS / per_packet,
                    "{quality:?} {rate} mtu {mtu}"
                );
                assert!(
                    packets.len() + 2 > CALLS / per_packet,
                    "{quality:?} {rate} mtu {mtu}"
                );
                for packet in &packets {
                    assert!(PACKET_OVERHEAD + packet.data.len() <= mtu);
                    assert_eq!(usize::from(packet.frames), per_packet);
                    assert_eq!(packet.data.len(), 660);
                    for frame in packet.data.chunks_exact(frame_len) {
                        assert_eq!(frame[0], 0xAA, "LDAC frame sync byte");
                    }
                }
            }
        }
    }
}

#[test]
fn bad_arguments_are_errors() {
    assert!(Encoder::new(32_000, Quality::High, 679).is_err());
    assert!(Encoder::new(192_000, Quality::High, 679).is_err());
    assert!(Encoder::new(44_100, Quality::High, 678).is_err());
    let mut enc = Encoder::new(44_100, Quality::High, 679).expect("encoder");
    assert!(enc.encode(&[0i16; 255]).is_err());
    assert!(enc.encode(&[0i16; 512]).is_err());
    assert!(enc.encode(&[0i16; 256]).is_ok());
}

/// Encoders built and run on several threads at once produce the same bytes
/// as one alone.
#[test]
fn threads_do_not_interfere() {
    let hash_of = |rate: u32| payload_hash(&run(rate, Quality::High, 679, 200).0);
    let alone: Vec<u64> = RATES.iter().map(|&r| hash_of(r)).collect();
    std::thread::scope(|scope| {
        let threads: Vec<_> = (0..8)
            .map(|i| scope.spawn(move || hash_of(RATES[i % 4])))
            .collect();
        for (i, t) in threads.into_iter().enumerate() {
            assert_eq!(t.join().expect("thread"), alone[i % 4]);
        }
    });
}

/// FNV-1a (64-bit) of the concatenated payloads of 600 calls of the test
/// signal, mtu 679. Generated on x86-64 with the default (float) build.
#[cfg(not(feature = "fixed-point"))]
const VECTORS: [(u32, Quality, u64); 8] = [
    (44_100, Quality::High, 0x97a7_2220_392f_f408),
    (44_100, Quality::Standard, 0xa71c_6a6c_c925_31bc),
    (44_100, Quality::Mobile, 0x8041_5abe_248d_fe05),
    (48_000, Quality::High, 0x5f3a_49ec_341d_cc28),
    (48_000, Quality::Standard, 0xf2ca_a135_9da7_fadb),
    (48_000, Quality::Mobile, 0x04bf_419a_3701_8536),
    (88_200, Quality::High, 0x709a_cf27_849e_04b8),
    (96_000, Quality::High, 0xc1dd_f63f_b450_020f),
];

#[cfg(not(feature = "fixed-point"))]
#[test]
fn encoder_vectors() {
    let mut failed = false;
    for (rate, quality, want) in VECTORS {
        let hash = payload_hash(&run(rate, quality, 679, 600).0);
        if hash != want {
            failed = true;
            eprintln!("    ({rate}, Quality::{quality:?}, {hash:#018x}),");
        }
    }
    assert!(
        !failed,
        "encoder output changed; the actual hashes are printed above"
    );
}

/// With `Q2_LDAC_DUMP=<dir>`, writes 5 s of every rate and quality for an
/// external decoder: `ldac_<quality>_<rate>.ldac` (the payloads concatenated,
/// which is a plain sequence of LDAC frames) and `.pcm` (the input,
/// interleaved s16le).
#[test]
fn dump_for_external_decoder() {
    let Some(dir) = std::env::var_os("Q2_LDAC_DUMP") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).expect("dump directory");
    for rate in RATES {
        for quality in QUALITIES {
            let calls = (rate as usize * 5) / if rate > 48_000 { 256 } else { 128 };
            let (packets, pcm) = run(rate, quality, 679, calls);
            let name = format!("ldac_{}_{rate}", format!("{quality:?}").to_lowercase());
            let stream: Vec<u8> = packets
                .iter()
                .flat_map(|p| p.data.iter().copied())
                .collect();
            let pcm: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
            std::fs::write(dir.join(format!("{name}.ldac")), stream).expect("write stream");
            std::fs::write(dir.join(format!("{name}.pcm")), pcm).expect("write pcm");
        }
    }
}

/// Splits an LDAC payload into frames by the length in each 3-byte frame
/// header (sync 0xAA, then 3 bits rate, 2 bits channels, 9 bits length - 1).
fn frame_lengths(data: &[u8]) -> Option<Vec<usize>> {
    let (mut at, mut lengths) = (0, Vec::new());
    while at < data.len() {
        let header = data.get(at..at + 3)?;
        if header[0] != 0xAA {
            return None;
        }
        let length = ((usize::from(header[1]) & 0x07) << 6 | usize::from(header[2]) >> 2) + 1 + 3;
        lengths.push(length);
        at += length;
    }
    (at == data.len()).then_some(lengths)
}

#[test]
fn levels_change_the_bit_rate_mid_stream_and_every_packet_stays_whole() {
    use q2_ldac::LEVELS;
    for rate in [44_100u32, 48_000] {
        let mut enc = Encoder::new(rate, Quality::Standard, 679).unwrap();
        assert_eq!(enc.level(), 1);
        let mut signal = Signal::new(rate);
        let mut block = vec![0i16; enc.frame_samples() * 2];
        // Down through every level, then back up: 200 calls (0.53 s at 48 kHz) at each.
        let plan = [1u8, 2, 3, 4, 3, 2, 1, 0, 1];
        // The frame sizes of the last packet at each step: by then the switch is done.
        let mut frame_sizes = Vec::new();
        for &level in &plan {
            enc.set_level(level).unwrap();
            assert_eq!(enc.level(), level);
            let kbps = if rate == 48_000 { LEVELS[usize::from(level)].1 } else { LEVELS[usize::from(level)].2 };
            let mut last = Vec::new();
            for _ in 0..200 {
                signal.fill(&mut block);
                if let Some(packet) = enc.encode(&block).unwrap() {
                    // A switch may emit frames of a transitional size; every packet is still whole frames.
                    let lengths = frame_lengths(&packet.data).expect("whole LDAC frames");
                    assert_eq!(lengths.len(), usize::from(packet.frames));
                    assert!(packet.data.len() + PACKET_OVERHEAD <= 679, "{}", packet.data.len());
                    last = lengths;
                }
            }
            // libldac reports the rate in use, which follows the target at the next frame.
            assert_eq!(enc.bitrate(), kbps, "level {level} at {rate}");
            assert!(!last.is_empty() && last.iter().all(|&l| l == last[0]), "level {level}: {last:?}");
            frame_sizes.push(last[0]);
        }
        // Lower levels use smaller frames.
        let last = |i: usize| frame_sizes[i];
        assert!(last(0) > last(1) && last(1) > last(2) && last(2) > last(3), "{frame_sizes:?}");
    }
}
