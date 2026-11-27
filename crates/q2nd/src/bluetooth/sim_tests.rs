//! The stream through a simulated link, millisecond by millisecond: the host,
//! pacing and the encoder against the fake controller, with a UART that
//! carries 8E1 bytes at its rate, lost frames, and a radio that stalls or slows.
//! A model of the headphones' buffer says whether the listener hears a gap.
use super::media::Media;
use super::{coder, pcm};
use crate::rt::Blocking;
use q2_bt::codec::{self, Kind};
use q2_bt::fake::{Air, Fake};
use q2_bt::host::{Command, Event, Host, Settings};
use std::collections::VecDeque;

/// Air time of one 2-DH5 packet with its return slot: 6 slots of 625 µs.
const DH5_US: u64 = 3750;

struct Link {
    /// The UART's rate after bring-up (8E1: 11 bits a byte).
    baud: u32,
    air: Air,
    /// Host-to-controller writes lost on the line at these milliseconds (the controller's FIFO overran).
    lose_at: Vec<u64>,
    ldac: coder::LdacQuality,
    adaptive: bool,
    seconds: u64,
    kind: Kind,
    /// [start, end) in ms: the Bluetooth thread does not run (other work has the CPU).
    starve: Vec<(u64, u64)>,
}

struct Outcome {
    /// (ms, RTP timestamp, LDAC frames) as the radio sent each packet.
    arrivals: Vec<(u64, u32, u8)>,
    stats: super::MediaStats,
    resends: u32,
    overflow: u32,
}

fn run(link: Link) -> Outcome {
    let blocking = Blocking::dedicated_thread();
    let firmware = q2_bt::rtl::fixture::firmware(&[0, 2], 3000);
    let settings = Settings { name: "Q2".into(), baud: link.baud, flow: false, previous: None, firmware: Some(firmware), config: Some(q2_bt::rtl::fixture::config()),
        keys: Vec::new(), codecs: vec![link.kind, Kind::Sbc], bench_peer: None };
    let mut host = Host::new(settings, 0);
    let mut fake = Fake::new(0);
    fake.devices[0].codecs = vec![codec::capabilities(link.kind)];
    let mut inbox = Vec::new();
    let endpoint = std::sync::Arc::new(pcm::Endpoint::new());
    let (mut baud, mut now, mut media, mut writer) = (q2_bt::host::INITIAL_BAUD, 0u64, None::<Media>, None::<pcm::Writer>);
    let mut line: VecDeque<u8> = VecDeque::new();
    let (mut streaming_since, mut phase) = (None, 0);
    let tone = coder::tone(44_100);
    let mut tone_at = 0;
    let mut lose = false;
    loop {
        // Bring-up, pairing and the stream start; then the stream for `seconds`.
        if phase == 0 && host.ready() { host.command(Command::Connect(fake.devices[0].addr)); phase = 1; }
        if phase == 2 && streaming_since.is_none() && host.streaming() { streaming_since = Some(now); fake.air = link.air.clone(); }
        if streaming_since.is_some_and(|t| now >= t + link.seconds * 1000) { break }
        assert!(now < 30_000 + link.seconds * 1000, "the stream did not start");
        // The player keeps the PCM queue full.
        if let Some(writer) = &writer {
            while writer.delay() + 441 <= writer.capacity() {
                let block: Vec<i16> = (0..441 * 2).map(|i| tone[(tone_at + i) % tone.len()]).collect();
                tone_at = (tone_at + 441 * 2) % tone.len();
                assert_eq!(writer.write_s16(&blocking, &block), 441);
            }
        }
        let now_us = now * 1000;
        let starved = link.starve.iter().any(|&(a, b)| (a..b).contains(&now));
        if !starved && !inbox.is_empty() { host.rx(&std::mem::take(&mut inbox), now); }
        if let Some(media) = media.as_mut().filter(|_| !starved) { media.tick(&mut host, &endpoint, now_us); }
        let mut wire = Vec::new();
        if !starved { host.poll(now, &mut wire); }
        if streaming_since.is_some() {
            if link.lose_at.contains(&now) { lose = true; }
            // The first write with a media packet after a loss time is lost.
            if lose && wire.len() > 600 { wire.clear(); lose = false; }
            line.extend(wire);
            // What the line carries in a millisecond.
            let budget = (u64::from(baud) / 11 / 1000) as usize;
            let sent: Vec<u8> = line.drain(..budget.min(line.len())).collect();
            if !sent.is_empty() { fake.rx(&sent, baud, now); }
        } else if !wire.is_empty() { fake.rx(&wire, baud, now); }
        for event in host.events().collect::<Vec<_>>() {
            match event {
                Event::Uart { baud: b, .. } => baud = b,
                Event::Stream(Some(stream)) => {
                    media = Some(Media::new(stream, link.ldac, link.adaptive).unwrap());
                    endpoint.set_available(true);
                    writer = Some(pcm::Writer::open(&endpoint, 44_100, 2, 400_000).unwrap());
                }
                _ => {}
            }
        }
        if phase == 1 && media.is_some() { phase = 2; }
        let mut back = Vec::new();
        fake.poll(now, &mut back);
        // While starved, what arrives waits in the tty's buffer.
        if !back.is_empty() && fake.tx_baud() == baud { if starved { inbox.extend(back); } else { host.rx(&back, now); } }
        now += 1;
    }
    let stats = media.as_mut().unwrap().stats();
    let start = streaming_since.unwrap();
    Outcome { arrivals: fake.log.arrivals.iter().filter(|a| a.0 >= start).copied().collect(), stats, resends: host.link_stats().h5_resends, overflow: fake.log.acl_overflow }
}

/// The headphones: they start playing once `prebuffer_ms` of audio has arrived
/// and play 44.1 kHz from then on; an empty buffer is a gap, after which they
/// fill again. Returns (gaps, the lowest buffer level in ms while playing).
fn listen(arrivals: &[(u64, u32, u8)], prebuffer_ms: u64) -> (u32, i64) {
    let Some(&(first, first_ts, _)) = arrivals.first() else { return (1, 0) };
    let last = arrivals.last().unwrap().0;
    // Audio received: up to the RTP timestamp of the newest packet (conservative by one packet).
    let mut received = 0u64;
    let (mut played, mut playing, mut gaps, mut lowest) = (0u64, false, 0u32, i64::MAX);
    let mut next = 0;
    for t in first..=last {
        while next < arrivals.len() && arrivals[next].0 <= t { received = received.max(u64::from(arrivals[next].1.wrapping_sub(first_ts))); next += 1; }
        let level = received as i64 - played as i64;
        if !playing { if level as u64 * 1000 / 44_100 >= prebuffer_ms { playing = true; } else { continue } }
        // One millisecond of playback: 44.1 samples.
        let step = 44_100 / 1000 + u64::from(t % 10 == 0);
        if level < step as i64 { gaps += 1; playing = false; played = received; continue }
        played += step;
        lowest = lowest.min((received - played) as i64 * 1000 / 44_100);
    }
    (gaps, lowest)
}

fn ldac(seconds: u64, air: Air, lose_at: Vec<u64>, adaptive: bool) -> Link {
    Link { baud: 1_500_000, air: Air { packet_us: DH5_US, ..air }, lose_at, ldac: coder::LdacQuality::Standard, adaptive, seconds, kind: Kind::Ldac, starve: Vec::new() }
}

#[test]
fn aac_rides_out_lost_frames_air_stalls_and_a_starved_thread() {
    // Bose QC45 on AAC (2026-11-22): 256 kbit/s, far inside the UART. What remains
    // is bursts: a lost frame (120 ms resend), Wi-Fi taking the air, the thread not running.
    let air = Air { packet_us: DH5_US, stalls: (0..20).map(|i| (8000 + i * 100, 8030 + i * 100)).collect(), ..Air::default() };
    let starve = (0..4).map(|i| (12_000 + i * 2000, 12_080 + i * 2000)).collect();
    let lose = (0..4).map(|i| 5000 + i * 700).collect();
    let out = run(Link { kind: Kind::Aac, starve, ..ldac(22, air, lose, true) });
    let (gaps, lowest) = listen(&out.arrivals, PREBUFFER_MS);
    eprintln!("sim: aac gaps={gaps} lowest_ms={lowest} resends={} {:?}", out.resends, out.stats);
    assert_eq!(gaps, 0, "lowest {lowest} ms, {:?}", out.stats);
    assert!(out.resends >= 3 && out.stats.ldac_level.is_none() && out.stats.kbps == 256);
}
/// The headphones' buffer before they start: well under what real sinks keep (150–250 ms).
const PREBUFFER_MS: u64 = 100;

#[test]
fn a_clean_link_streams_ldac_by_the_clock_without_gaps_or_rate_changes() {
    let out = run(ldac(6, Air::default(), Vec::new(), true));
    let (gaps, lowest) = listen(&out.arrivals, PREBUFFER_MS);
    eprintln!("sim: gaps={gaps} lowest_ms={lowest} longest_silence_ms={} {:?}", out.arrivals.windows(2).map(|w| w[1].0 - w[0].0).max().unwrap_or(0), out.stats);
    assert_eq!(gaps, 0);
    // The lead builds to 40 ms above the prebuffer.
    assert!(lowest >= PREBUFFER_MS as i64 - 10, "lowest {lowest} ms");
    assert_eq!((out.stats.late, out.stats.restarts, out.stats.ldac_changes, out.stats.ldac_level, out.overflow), (0, 0, 0, Some(1), 0), "{:?}", out.stats);
    // 606 kbit/s: 3 frames of 128 samples a packet, about 115 packets a second.
    let rate = out.arrivals.len() as u64 / 6;
    assert!((110..=122).contains(&rate), "{rate} packets/s");
}

#[test]
fn frames_lost_on_the_uart_are_resent_and_caught_up_before_the_headphones_run_dry() {
    // One lost write every 1.5 s: each stalls the window for the resend timeout.
    let lose: Vec<u64> = (1..10).map(|i| 3000 + i * 1500).collect();
    let out = run(ldac(18, Air::default(), lose, true));
    assert!(out.resends >= 9, "{}", out.resends);
    let (gaps, lowest) = listen(&out.arrivals, PREBUFFER_MS);
    eprintln!("sim: gaps={gaps} lowest_ms={lowest} longest_silence_ms={} {:?}", out.arrivals.windows(2).map(|w| w[1].0 - w[0].0).max().unwrap_or(0), out.stats);
    assert_eq!(gaps, 0, "lowest {lowest} ms, {:?}", out.stats);
    assert_eq!(out.stats.restarts, 0);
}

#[test]
fn short_repeated_air_stalls_are_absorbed() {
    // 30 ms of no air every 100 ms for 5 s (Wi-Fi taking its slots).
    let stalls = (0..50).map(|i| (8000 + i * 100, 8030 + i * 100)).collect();
    let out = run(ldac(16, Air { stalls, ..Air::default() }, Vec::new(), true));
    let (gaps, lowest) = listen(&out.arrivals, PREBUFFER_MS);
    eprintln!("sim: gaps={gaps} lowest_ms={lowest} longest_silence_ms={} {:?}", out.arrivals.windows(2).map(|w| w[1].0 - w[0].0).max().unwrap_or(0), out.stats);
    assert_eq!(gaps, 0, "lowest {lowest} ms, {:?}", out.stats);
}

#[test]
fn ldac_lowers_its_rate_when_the_air_slows_and_returns_after_it_clears_and_without_it_the_headphones_run_dry() {
    // For 8 s each packet takes three times the air time (retries): 606 kbit/s no longer fits.
    let slow = || Air { slow: vec![(9000, 17_000, 3)], ..Air::default() };
    let fixed = run(ldac(14, slow(), Vec::new(), false));
    let (fixed_gaps, _) = listen(&fixed.arrivals, PREBUFFER_MS);
    assert!(fixed_gaps > 0, "the control: a fixed 606 kbit/s runs dry ({:?})", fixed.stats);
    let out = run(ldac(48, slow(), Vec::new(), true));
    let (gaps, lowest) = listen(&out.arrivals, PREBUFFER_MS);
    eprintln!("sim: gaps={gaps} lowest_ms={lowest} longest_silence_ms={} {:?}", out.arrivals.windows(2).map(|w| w[1].0 - w[0].0).max().unwrap_or(0), out.stats);
    assert_eq!(gaps, 0, "lowest {lowest} ms, {:?}", out.stats);
    assert!(out.stats.ldac_changes >= 2, "{:?}", out.stats);
    assert_eq!(out.stats.ldac_level, Some(1), "back to 606 kbit/s: {:?}", out.stats);
}
