//! The host against the fake controller and headphones, byte for byte through H5.
use crate::avrcp::Button;
use crate::fake::{CONTROLLER_ADDR, Fake, Headphones};
use crate::codec::{self, Codec, Kind};
use crate::host::{Command, Event, Host, INITIAL_BAUD, Key, Settings, Stream};
use crate::{rtl, sbc};

struct SbcStream { sbc: sbc::Params, payload: usize }
fn sbc_of(stream: Stream) -> sbc::Params { match stream.codec { Codec::Sbc(p) => p, other => panic!("not SBC: {other:?}") } }

struct Rig { host: Host, fake: Fake, now: u64, baud: u32, events: Vec<Event>, lose_every: u32, sent: u32, deaf: bool }

fn settings() -> Settings {
    Settings { name: "Q2".into(), baud: 1_500_000, flow: false, previous: None, firmware: Some(rtl::fixture::firmware(&[0, 2], 3000)),
        config: Some(rtl::fixture::config()), keys: Vec::new(), codecs: Vec::new(), bench_peer: None }
}

impl Rig {
    fn with(settings: Settings, fake: Fake) -> Self { Self { host: Host::new(settings, 0), fake, now: 0, baud: INITIAL_BAUD, events: Vec::new(), lose_every: 0, sent: 0, deaf: false } }
    fn new() -> Self { Self::with(settings(), Fake::new(0)) }
    fn step(&mut self) {
        let mut wire = Vec::new();
        self.host.poll(self.now, &mut wire);
        self.sent += 1;
        let lost = self.deaf || self.lose_every != 0 && self.sent % self.lose_every == 0;
        if !wire.is_empty() && !lost { self.fake.rx(&wire, self.baud, self.now); }
        self.collect();
        let mut wire = Vec::new();
        self.fake.poll(self.now, &mut wire);
        if !wire.is_empty() && !self.deaf && self.fake.tx_baud() == self.baud { self.host.rx(&wire, self.now); }
        self.collect();
        self.now += 1;
    }
    fn collect(&mut self) {
        for event in self.host.events().collect::<Vec<_>>() {
            if let Event::Uart { baud, .. } = event { self.baud = baud; }
            self.events.push(event);
        }
    }
    fn run(&mut self, ms: u64) { for _ in 0..ms { self.step(); } }
    fn until(&mut self, limit: u64, mut done: impl FnMut(&mut Rig) -> bool) { for _ in 0..limit { if done(self) { return } self.step(); } panic!("not reached in {limit} ms: {:?}", self.events); }
    fn saw(&self, wanted: impl Fn(&Event) -> bool) -> bool { self.events.iter().any(wanted) }
    fn ready() -> Self { let mut rig = Self::new(); rig.until(5000, |r| r.host.ready()); rig }
    fn addr(&self) -> [u8; 6] { self.fake.devices[0].addr }
    fn open_stream(&mut self) -> Stream {
        let addr = self.addr();
        self.host.command(Command::Connect(addr));
        self.until(5000, |r| r.host.stream().is_some());
        self.host.stream().unwrap()
    }
    /// Encodes and sends `frames` SBC frames of the generated tone; returns the PCM.
    fn play(&mut self, stream: Stream, frames: usize) -> Vec<i16> {
        let Codec::Sbc(params) = stream.codec else { panic!("an SBC stream") };
        let stream = SbcStream { sbc: params, payload: stream.mtu - 13 };
        let pcm = sbc::tests::tone(stream.sbc.samples() * frames);
        let mut encoder = sbc::Encoder::new(stream.sbc);
        let per_packet = (stream.payload / stream.sbc.frame_len()).min(15);
        for packet in pcm.chunks(stream.sbc.samples() * 2 * per_packet) {
            let mut payload = Vec::new();
            let count = packet.len() / (stream.sbc.samples() * 2);
            for frame in packet.chunks(stream.sbc.samples() * 2) { encoder.encode(frame, &mut payload); }
            self.until(1000, |r| r.host.media_room());
            self.host.send_media(&payload, count as u8, (count * stream.sbc.samples()) as u32);
            self.run(3);
        }
        self.run(50);
        pcm
    }
}

#[test]
fn bring_up_downloads_the_patch_at_the_fast_rate_and_configures_the_controller() {
    let mut rig = Rig::ready();
    let patch = rtl::select_patch(&rtl::fixture::firmware(&[0, 2], 3000), 2).unwrap();
    let mut image = patch.data;
    image.extend(rtl::with_uart(&rtl::fixture::config(), 1_500_000, false).unwrap());
    assert_eq!(rig.fake.log.image, image);
    assert_eq!((rig.baud, rig.fake.baud()), (1_500_000, 1_500_000));
    assert!(rig.fake.patched && rig.fake.log.local_name == "Q2" && rig.fake.log.scan_enable == 2);
    assert!(rig.saw(|e| matches!(e, Event::Controller(v) if v.rom_version == Some(2) && v.lmp_subversion == 0x8723 && !v.already_patched)));
    assert!(rig.saw(|e| *e == Event::Ready { addr: CONTROLLER_ADDR }));
    // The download ran after the rate change: the version queries are the only commands before it.
    assert_eq!(rig.fake.log.commands[..4], [0x1001, 0xfc6d, 0xfc17, 0xfc20]);
    assert_eq!(rig.host.diagnostics().patched_version, Some((0x1143, 0xd5c8)), "the version read after the download is the patch's");
    rig.host.command(Command::Raw(0x1407, vec![0, 0, 0]));
    rig.run(50);
    let d = rig.host.diagnostics();
    assert!(d.replies.len() == 1 && d.replies[0].0 == 0x1407 && d.replies[0].1.len() == 9 && d.replies[0].1[0] == 0, "{d:?}");
    assert!(d.by_code.iter().any(|c| c.0 == 0x0e) && d.timeline.last().is_some_and(|t| (t.1, t.2, t.3) == (0x0e, 0x1407, 0)), "{d:?}");
    assert!(rig.now < 1500, "bring-up took {} ms", rig.now);
    rig.run(1000);
    assert!(!rig.saw(|e| matches!(e, Event::Failed(_))));
}

#[test]
fn a_controller_that_kept_its_patch_is_found_at_the_fast_rate_without_a_download() {
    let mut rig = Rig::with(settings(), Fake::new(0).already_patched(1_500_000));
    rig.until(6000, |r| r.host.ready());
    assert!(rig.fake.log.image.is_empty());
    assert!(rig.saw(|e| matches!(e, Event::Controller(v) if v.already_patched && v.rom_version.is_none())));
}

#[test]
fn a_controller_left_at_a_previous_setting_is_found_there_and_moved_to_the_new_rate() {
    // A bring-up at 3 Mbaud failed after the controller took the rate, and nothing reset it.
    let mut rig = Rig::with(Settings { previous: Some((3_000_000, true)), ..settings() }, Fake::new(0).already_patched(3_000_000));
    rig.until(12_000, |r| r.host.ready());
    assert!(rig.fake.log.image.is_empty(), "no second download");
    assert_eq!((rig.baud, rig.fake.baud()), (1_500_000, 1_500_000));
    // The sync tried 115200 and 1.5 Mbaud first, then 3 Mbaud with RTS/CTS.
    let rates: Vec<_> = rig.events.iter().filter_map(|e| match e { Event::Uart { baud, flow } => Some((*baud, *flow)), _ => None }).collect();
    assert_eq!(rates, [(1_500_000, false), (3_000_000, true), (1_500_000, false)]);
    assert_eq!(rig.fake.log.commands[..3], [0x1001, 0xfc17, 0x0c03], "version, the rate change, then the start-up commands");
    assert!(rig.saw(|e| matches!(e, Event::Controller(v) if v.already_patched)));
    // Without a previous setting the sync tries only 115200 and the fast rate, and gives up.
    let mut lost = Rig::with(settings(), Fake::new(0).already_patched(3_000_000));
    lost.until(12_000, |r| r.saw(|e| matches!(e, Event::Failed(_))));
    // A previous setting the sync tries anyway is not tried twice.
    let mut same = Rig::with(Settings { previous: Some((1_500_000, false)), ..settings() }, Fake::new(0));
    same.until(5000, |r| r.host.ready());
    assert_eq!(same.fake.log.commands[..4], [0x1001, 0xfc6d, 0xfc17, 0xfc20]);
}

#[test]
fn a_check_on_a_live_controller_is_answered_and_reinit_reruns_start_up_without_a_download() {
    let mut rig = Rig::ready();
    let (image, resets) = (rig.fake.log.image.len(), |rig: &Rig| rig.fake.log.commands.iter().filter(|&&c| c == 0x0c03).count());
    assert_eq!(resets(&rig), 1);
    rig.events.clear();
    rig.host.command(Command::Check);
    rig.until(200, |r| r.saw(|e| *e == Event::Alive));
    rig.run(crate::host::CHECK_MS + 100);
    assert!(!rig.saw(|e| *e == Event::Lost), "{:?}", rig.events);
    rig.host.command(Command::Reinit);
    assert!(!rig.host.ready(), "the start-up commands run again");
    rig.until(500, |r| r.host.ready());
    assert_eq!((resets(&rig), rig.fake.log.image.len()), (2, image), "one more HCI Reset, no second download");
    assert!(rig.saw(|e| *e == Event::Ready { addr: CONTROLLER_ADDR }) && rig.fake.log.scan_enable == 2 && rig.fake.log.local_name == "Q2");
    // Never under an open link: the reset would drop the headphones.
    rig.open_stream();
    rig.host.command(Command::Reinit);
    rig.run(200);
    assert!(rig.host.ready() && rig.fake.connected() && resets(&rig) == 2);
}

#[test]
fn a_check_after_the_controller_lost_power_reports_it_lost_and_a_new_host_brings_it_up() {
    let mut rig = Rig::ready();
    let image = rig.fake.log.image.clone();
    let now = rig.now;
    rig.fake.power_cycle(now);
    rig.events.clear();
    rig.host.command(Command::Check);
    rig.until(crate::host::CHECK_MS + 200, |r| r.saw(|e| *e == Event::Lost));
    assert!(rig.host.ready() && !rig.saw(|e| matches!(e, Event::Failed(_) | Event::Alive)), "{:?}", rig.events);
    // The player starts a new host on the same port: sync at the initial rate and a download.
    rig.host = Host::new(settings(), rig.now);
    rig.baud = INITIAL_BAUD;
    rig.fake.log.image.clear();
    rig.until(6000, |r| r.host.ready());
    assert_eq!(rig.fake.log.image, image);
    assert!(rig.fake.patched && rig.fake.baud() == 1_500_000);
}

#[test]
fn bring_up_survives_a_link_restart_after_the_patch_and_lost_bytes() {
    let mut fake = Fake::new(0);
    fake.restart_link_after_patch = true;
    let mut rig = Rig::with(settings(), fake);
    rig.lose_every = 23;
    rig.until(20_000, |r| r.host.ready());
    assert!(rig.fake.patched && rig.host.transport_counters().0 > 0);
}

#[test]
fn a_controller_whose_uart_sleeps_is_woken_and_answered_through_scan_pairing_and_audio() {
    let mut fake = Fake::new(0).sleepy(30);
    fake.devices[0].named_in_inquiry = false;
    let mut rig = Rig::with(settings(), fake);
    rig.until(8000, |r| r.host.ready());
    rig.run(500);
    rig.host.command(Command::Scan);
    rig.until(3000, |r| r.saw(|e| *e == Event::ScanFinished));
    assert!(rig.saw(|e| matches!(e, Event::Found { name: Some(_), .. })));
    let d = rig.host.diagnostics();
    assert!(d.inquiry_results == 1 && d.inquiry_completes == 1 && d.name_replies == 1 && d.command_errors.is_empty() && d.command_timeouts == 0, "{d:?}");
    assert!(d.h5_sleep[0] > 0 && d.h5_sleep[1] > 0 && d.h5_sleep[2] > 0, "sleep, wakeup and woken were all exchanged: {d:?}");
    let stream = rig.open_stream();
    rig.run(1500);
    rig.host.command(Command::Start);
    rig.until(1000, |r| r.host.streaming());
    rig.play(stream, 70);
    assert_eq!((rig.fake.log.sbc_frames, rig.fake.log.media_sequence_gaps), (70, 0));
}

#[test]
fn an_empty_first_inquiry_is_followed_by_a_second_and_a_stalled_one_is_retried() {
    let mut rig = Rig::ready();
    rig.fake.devices[0].pairing_mode = false;
    rig.host.command(Command::Scan);
    rig.until(3000, |r| r.saw(|e| *e == Event::ScanFinished));
    assert_eq!(rig.fake.log.commands.iter().filter(|&&c| c == 0x0401).count(), 2);
    assert_eq!(rig.host.diagnostics().inquiry_completes, 2);
    // Headphones that enter pairing mode during the first inquiry are found by the second.
    rig.events.clear();
    rig.host.command(Command::Scan);
    rig.run(150);
    rig.fake.devices[0].pairing_mode = true;
    rig.until(3000, |r| r.saw(|e| *e == Event::ScanFinished));
    assert!(rig.saw(|e| matches!(e, Event::Found { .. })));
    // A controller that never reports the inquiry complete: cancel, one more inquiry, then give up.
    rig.events.clear();
    rig.fake.swallow_inquiry_complete = true;
    rig.fake.devices[0].pairing_mode = false;
    let before = rig.fake.log.commands.len();
    rig.host.command(Command::Scan);
    rig.until(40_000, |r| r.saw(|e| *e == Event::ScanFinished));
    assert_eq!(rig.fake.log.commands[before..].iter().filter(|&&c| matches!(c, 0x0401 | 0x0402)).copied().collect::<Vec<_>>(), [0x0401, 0x0402, 0x0401, 0x0402]);
    assert_eq!(rig.host.diagnostics().scan_timeouts, 2);
}

#[test]
fn a_refused_or_unanswered_inquiry_ends_the_scan_and_is_counted() {
    let mut rig = Rig::ready();
    rig.host.command(Command::Scan);
    rig.deaf = true;
    rig.until(7000, |r| r.saw(|e| *e == Event::ScanFinished));
    assert_eq!(rig.host.diagnostics().command_timeouts, 1);
}

#[test]
fn a_silent_uart_fails_within_ten_seconds_and_bad_firmware_is_reported() {
    let mut rig = Rig::new();
    rig.deaf = true;
    rig.until(10_000, |r| r.saw(|e| matches!(e, Event::Failed(m) if m.contains("does not answer"))));
    rig.host.command(Command::Scan);
    rig.run(100);
    let mut bad = settings();
    bad.firmware = Some(b"Realtech nothing".to_vec());
    let mut rig = Rig::with(bad, Fake::new(0));
    rig.until(3000, |r| r.saw(|e| matches!(e, Event::Failed(m) if m.starts_with("Bluetooth firmware"))));
    let mut wrong_rom = Fake::new(0);
    wrong_rom.rom_version = 1;
    let mut rig = Rig::with(settings(), wrong_rom);
    rig.until(3000, |r| r.saw(|e| matches!(e, Event::Failed(m) if m.contains("no patch for this ROM version"))));
}

#[test]
fn scan_reports_each_device_once_and_asks_for_missing_names() {
    let mut rig = Rig::ready();
    let mut second = Headphones::new(0x12, "Second Fixture");
    second.named_in_inquiry = false;
    rig.fake.devices.push(second);
    let mut hidden = Headphones::new(0x13, "Not Pairing");
    hidden.pairing_mode = false;
    rig.fake.devices.push(hidden);
    rig.host.command(Command::Scan);
    rig.until(3000, |r| r.saw(|e| *e == Event::ScanFinished));
    let found: Vec<_> = rig.events.iter().filter_map(|e| match e { Event::Found { addr, class, name, rssi, .. } => Some((addr[0], *class, name.clone(), *rssi)), _ => None }).collect();
    assert_eq!(found, [(0x11, 0x24_0404, Some("Fixture Headphones".into()), -56), (0x12, 0x24_0404, None, -80), (0x12, 0x24_0404, Some("Second Fixture".into()), -80)]);
    // A second scan can start, and connecting stops it.
    rig.events.clear();
    rig.host.command(Command::Scan);
    rig.run(20);
    let addr = rig.addr();
    rig.host.command(Command::Connect(addr));
    rig.until(3000, |r| r.saw(|e| matches!(e, Event::Connected { .. })));
    assert!(rig.saw(|e| *e == Event::ScanFinished) && rig.fake.log.commands.contains(&0x0402));
}

#[test]
fn an_inquiry_never_reported_complete_ends_the_scan_with_what_it_found() {
    let mut rig = Rig::ready();
    rig.fake.swallow_inquiry_complete = true;
    rig.fake.devices[0].named_in_inquiry = false;
    let start = rig.now;
    rig.host.command(Command::Scan);
    rig.until(16_000, |r| r.saw(|e| *e == Event::ScanFinished));
    // One inquiry, cancelled at the 14 s deadline; no second round, since a device answered;
    // then the name request its response called for.
    assert_eq!(rig.fake.log.commands.iter().filter(|&&c| matches!(c, 0x0401 | 0x0402 | 0x0419)).copied().collect::<Vec<_>>(), [0x0401, 0x0402, 0x0419]);
    assert!(rig.saw(|e| matches!(e, Event::Found { name: Some(_), .. })));
    let d = rig.host.diagnostics();
    assert_eq!((d.scan_timeouts, d.inquiry_completes, d.inquiry_results, d.name_replies, d.unparsed), (1, 0, 1, 1, 0));
    assert!((14_000..14_100).contains(&(rig.now - start)), "ended after {} ms", rig.now - start);
}

/// A computer that takes audio: class 0x7c0104 (computer, desktop), an A2DP sink in SDP.
fn computer_rig(sdp_sink: bool, eir: &[u16]) -> Rig {
    let mut rig = Rig::ready();
    let mut computer = Headphones::computer(0x21, "Fixture Computer");
    computer.sdp_sink = sdp_sink;
    computer.eir_uuids = eir.to_vec();
    rig.fake.devices = vec![computer];
    rig
}

#[test]
fn a_computer_whose_sdp_has_an_audio_sink_is_checked_then_paired_and_streams() {
    let mut rig = computer_rig(true, &[]);
    rig.host.command(Command::Scan);
    rig.until(3000, |r| r.saw(|e| *e == Event::ScanFinished));
    assert!(rig.saw(|e| matches!(e, Event::Found { class: 0x7c_0104, sink: false, name: Some(_), .. })), "nothing in its EIR names the sink");
    let addr = rig.addr();
    rig.host.command(Command::ConnectSink(addr));
    // The search runs on the unauthenticated link, before any pairing command.
    rig.until(2000, |r| r.fake.log.sdp_searches == 1);
    assert!(!rig.fake.log.commands.contains(&0x0411) && rig.fake.devices[0].key.is_none());
    rig.until(5000, |r| r.host.stream().is_some());
    assert!(rig.fake.devices[0].key.is_some() && rig.saw(|e| matches!(e, Event::LinkKey(_))));
    assert!(rig.saw(|e| matches!(e, Event::Connected { incoming: false, .. })) && !rig.saw(|e| matches!(e, Event::NotSink(_))));
    assert_eq!(rig.host.diagnostics().sink_checks, (1, 1, 0));
    let stream = rig.host.stream().unwrap();
    rig.host.command(Command::Start);
    rig.until(1000, |r| r.host.streaming());
    rig.play(stream, 70);
    assert_eq!((rig.fake.log.sbc_frames, rig.fake.log.media_sequence_gaps), (70, 0));
    // Once paired it reconnects as headphones do: a plain connect with its key.
    rig.host.command(Command::Disconnect);
    rig.until(1000, |r| r.saw(|e| matches!(e, Event::Disconnected { .. })));
    rig.events.clear();
    rig.host.command(Command::Connect(addr));
    rig.until(5000, |r| r.host.stream().is_some());
    assert!(!rig.saw(|e| matches!(e, Event::LinkKey(_))) && rig.fake.log.sdp_searches == 1);
}

#[test]
fn a_device_without_an_audio_sink_is_not_paired_and_eir_sinks_are_reported() {
    let mut rig = computer_rig(false, &[0x1112, 0x111f]);
    let addr = rig.addr();
    rig.host.command(Command::Scan);
    rig.until(3000, |r| r.saw(|e| *e == Event::ScanFinished));
    assert!(rig.saw(|e| matches!(e, Event::Found { sink: false, .. })));
    rig.host.command(Command::ConnectSink(addr));
    rig.until(3000, |r| r.saw(|e| matches!(e, Event::ConnectFailed { .. })));
    assert!(rig.saw(|e| *e == Event::NotSink(addr)));
    assert!(!rig.fake.log.commands.contains(&0x0411) && rig.fake.devices[0].key.is_none() && !rig.fake.connected());
    assert!(!rig.saw(|e| matches!(e, Event::LinkKey(_) | Event::Connected { .. })));
    assert_eq!(rig.host.diagnostics().sink_checks, (1, 0, 1));
    // The sink UUID in the extended inquiry response, 16-bit list.
    let mut rig = computer_rig(true, &[0x110b, 0x110e]);
    rig.host.command(Command::Scan);
    rig.until(3000, |r| r.saw(|e| *e == Event::ScanFinished));
    assert!(rig.saw(|e| matches!(e, Event::Found { sink: true, class: 0x7c_0104, .. })));
    // A device that does not answer the search in time is left unpaired.
    let mut rig = computer_rig(true, &[]);
    rig.fake.mute_sdp = true;
    let addr = rig.addr();
    let start = rig.now;
    rig.host.command(Command::ConnectSink(addr));
    rig.until(12_000, |r| r.saw(|e| matches!(e, Event::ConnectFailed { .. })));
    assert!(rig.now - start >= 10_000 && rig.fake.log.sdp_searches == 1 && rig.fake.devices[0].key.is_none());
    assert!(!rig.saw(|e| matches!(e, Event::NotSink(_) | Event::LinkKey(_))));
    assert_eq!(rig.host.diagnostics().sink_checks, (1, 0, 0));
}

#[test]
fn pairing_opens_an_sbc_stream_and_the_audio_arrives_intact() {
    let mut rig = Rig::ready();
    let stream = rig.open_stream();
    assert_eq!(stream, Stream { codec: Codec::Sbc(sbc::Params { rate: 44100, blocks: 16, joint: true, snr: false, bitpool: 53 }), mtu: 895 });
    assert_eq!(rig.fake.log.configured, Some([0x21, 0x15, 2, 53]));
    let key = rig.fake.devices[0].key.unwrap();
    assert!(rig.saw(|e| *e == Event::LinkKey(Key { addr: [0x11, 0x22, 0x33, 0x44, 0x55, 0x66], key, kind: 4 })));
    assert!(rig.saw(|e| matches!(e, Event::Connected { incoming: false, .. })));
    assert!(!rig.host.streaming());
    rig.host.command(Command::Start);
    rig.until(1000, |r| r.host.streaming());
    assert!(rig.fake.log.streaming && rig.saw(|e| *e == Event::Streaming(true)));
    let pcm = rig.play(stream, 280);
    assert_eq!((rig.fake.log.sbc_frames, rig.fake.log.media_sequence_gaps, rig.host.media_dropped()), (280, 0, 0));
    assert_eq!(rig.fake.log.media_packets, 40, "seven 119-byte frames fit the 895-byte MTU");
    assert_eq!(rig.fake.log.last_timestamp, 39 * 7 * 128);
    let mut decoder = sbc::tests::Decoder::new();
    let mut decoded = Vec::new();
    for frame in rig.fake.log.sbc.chunks(sbc_of(stream).frame_len()) { decoder.decode(frame, &mut decoded); }
    let (mut signal, mut noise) = (0f64, 0f64);
    for i in 1000..pcm.len() / 2 - 73 {
        let (a, b) = (f64::from(pcm[i * 2]), f64::from(decoded[(i + 73) * 2]));
        signal += a * a; noise += (a - b) * (a - b);
    }
    assert!(10.0 * (signal / noise).log10() > 30.0);
    // The headphones' buttons arrive once the control channel is up (opened by us after a second).
    rig.until(3000, |r| r.fake.press(0x46));
    rig.until(500, |r| r.saw(|e| *e == Event::Button(Button::Pause)));
    assert_eq!(rig.events.iter().filter(|e| matches!(e, Event::Button(_))).count(), 1);
    rig.host.command(Command::Suspend);
    rig.until(1000, |r| !r.fake.log.streaming);
    assert!(rig.saw(|e| *e == Event::Streaming(false)) && rig.host.stream().is_some());
    rig.host.command(Command::Disconnect);
    rig.until(1000, |r| r.saw(|e| matches!(e, Event::Disconnected { reason: 0x16, .. })));
    assert!(rig.saw(|e| *e == Event::Stream(None)) && rig.host.stream().is_none());
}

#[test]
fn stored_key_reconnects_without_pairing_from_either_side() {
    let mut rig = Rig::ready();
    rig.open_stream();
    let key = rig.events.iter().find_map(|e| match e { Event::LinkKey(k) => Some(k.clone()), _ => None }).unwrap();
    // A new player process with the stored key; the headphones are no longer in pairing mode.
    let mut fake = Fake::new(0);
    fake.devices[0].key = Some(key.key);
    fake.devices[0].pairing_mode = false;
    let mut again = Rig::with(Settings { keys: vec![key], ..settings() }, fake);
    again.until(5000, |r| r.host.ready());
    again.open_stream();
    assert!(!again.saw(|e| matches!(e, Event::LinkKey(_))) && !again.fake.log.commands.contains(&0x042b));
    // The link drops (out of range); later the headphones page the player.
    again.host.command(Command::Start);
    again.until(1000, |r| r.host.streaming());
    again.events.clear();
    again.fake.drop_link();
    again.until(1000, |r| r.saw(|e| matches!(e, Event::Disconnected { reason: 0x08, .. })));
    let order: Vec<_> = again.events.iter().filter(|e| matches!(e, Event::Streaming(false) | Event::Stream(None) | Event::Disconnected { .. })).collect();
    assert_eq!(order.len(), 3);
    assert!(matches!(order[0], Event::Streaming(false)) && matches!(order[2], Event::Disconnected { .. }));
    again.events.clear();
    assert!(again.fake.reconnect(0));
    again.until(6000, |r| r.host.stream().is_some());
    assert!(again.saw(|e| matches!(e, Event::Connected { incoming: true, .. })));
    assert_eq!(sbc_of(again.host.stream().unwrap()).bitpool, 53);
    again.host.command(Command::Start);
    again.until(1000, |r| r.host.streaming() && r.fake.log.streaming);
    let stream = again.host.stream().unwrap();
    again.play(stream, 70);
    assert_eq!((again.fake.log.sbc_frames, again.fake.log.media_sequence_gaps), (70, 0));
    again.until(3000, |r| r.fake.press(0x4b));
    again.until(500, |r| r.saw(|e| *e == Event::Button(Button::Next)));
}

#[test]
fn headphones_that_start_the_stream_themselves_are_followed() {
    let mut rig = Rig::ready();
    rig.open_stream();
    rig.fake.drop_link();
    rig.until(1000, |r| r.saw(|e| matches!(e, Event::Disconnected { .. })));
    rig.fake.devices[0].starts_stream = true;
    assert!(rig.fake.reconnect(0));
    rig.until(6000, |r| r.host.streaming() && r.fake.log.streaming);
}

#[test]
fn a_stranger_is_refused_and_a_forgotten_key_is_reported() {
    let mut rig = Rig::ready();
    rig.fake.devices[0].key = Some([9; 16]);
    assert!(rig.fake.reconnect(0));
    rig.run(500);
    assert!(rig.fake.log.commands.contains(&0x040a) && !rig.saw(|e| matches!(e, Event::Connecting(_))));
    // We hold a key the headphones no longer have.
    let mut fake = Fake::new(0);
    fake.devices[0].pairing_mode = true;
    let addr = fake.devices[0].addr;
    let mut rig = Rig::with(Settings { keys: vec![Key { addr, key: [7; 16], kind: 4 }], ..settings() }, fake);
    rig.until(5000, |r| r.host.ready());
    rig.host.command(Command::Connect(addr));
    rig.until(3000, |r| r.saw(|e| *e == Event::KeyRejected(addr)));
    rig.until(3000, |r| r.saw(|e| matches!(e, Event::ConnectFailed { .. })));
    // Connecting again pairs afresh.
    rig.events.clear();
    rig.open_stream();
    assert!(rig.saw(|e| matches!(e, Event::LinkKey(_))));
}

#[test]
fn legacy_pin_pairing_and_restricted_sinks_are_served() {
    let mut rig = Rig::ready();
    rig.fake.devices[0].simple_pairing = false;
    // 48 kHz only, stereo only, 16 blocks, 8 subbands, SNR only, bitpool at most 35, small MTU.
    rig.fake.devices[0].sbc = [0x12, 0x16, 10, 35];
    rig.fake.devices[0].media_mtu = 300;
    let stream = rig.open_stream();
    assert_eq!(stream, Stream { codec: Codec::Sbc(sbc::Params { rate: 48000, blocks: 16, joint: false, snr: true, bitpool: 35 }), mtu: 300 });
    assert!(rig.saw(|e| matches!(e, Event::LinkKey(k) if k.kind == 0)) && rig.fake.log.commands.contains(&0x040d));
    rig.host.command(Command::Start);
    rig.until(1000, |r| r.host.streaming());
    rig.play(stream, 30);
    assert_eq!(rig.fake.log.sbc_frames, 30);
    assert_eq!(rig.fake.log.sbc.len(), 30 * sbc_of(stream).frame_len());
}

#[test]
fn failures_end_cleanly_absent_headphones_no_sbc_and_a_hung_sink() {
    let mut rig = Rig::ready();
    rig.fake.devices[0].powered = false;
    let addr = rig.addr();
    rig.host.command(Command::Connect(addr));
    rig.until(2000, |r| r.saw(|e| *e == Event::ConnectFailed { addr, status: 0x04 }));
    // Connecting works again afterwards; a sink without 8 subbands gives no stream.
    rig.fake.devices[0].powered = true;
    rig.fake.devices[0].sbc = [0x3f, 0xfb, 2, 53];
    rig.events.clear();
    rig.host.command(Command::Connect(addr));
    rig.until(3000, |r| r.saw(|e| *e == Event::Stream(None)));
    assert!(rig.host.stream().is_none());
    rig.host.command(Command::Disconnect);
    rig.until(1000, |r| r.saw(|e| matches!(e, Event::Disconnected { .. })));
    // A sink that stops answering is disconnected after the signalling timeout.
    rig.fake.devices[0].sbc = [0x3f, 0xff, 2, 53];
    rig.events.clear();
    rig.fake.mute_signalling = true;
    rig.host.command(Command::Connect(addr));
    rig.until(8000, |r| r.saw(|e| matches!(e, Event::Disconnected { .. })));
    assert!(rig.now > 4000);
}

#[test]
fn forget_drops_the_key_and_the_link() {
    let mut rig = Rig::ready();
    rig.open_stream();
    let addr = rig.addr();
    rig.host.command(Command::Forget(addr));
    rig.until(1000, |r| r.saw(|e| matches!(e, Event::Disconnected { .. })));
    rig.fake.devices[0].pairing_mode = false;
    assert!(rig.fake.reconnect(0));
    rig.run(300);
    assert!(rig.fake.log.commands.contains(&0x040a), "an unknown device is not accepted");
}

#[test]
fn the_sdp_server_answers_over_the_link() {
    let mut rig = Rig::ready();
    rig.open_stream();
    // ServiceSearchAttribute for the audio source class, all attributes.
    assert!(rig.fake.sdp_query(vec![0x06, 0, 1, 0, 15, 0x35, 3, 0x19, 0x11, 0x0a, 0x02, 0x00, 0x35, 5, 0x0a, 0, 0, 0xff, 0xff, 0]));
    rig.until(1000, |r| !r.fake.log.sdp_responses.is_empty());
    let response = &rig.fake.log.sdp_responses[0];
    assert_eq!(response[0], 0x07);
    assert!(response.windows(3).any(|w| w == [0x19, 0x11, 0x0d]), "A2DP profile descriptor");
}

#[test]
fn late_audio_is_dropped_instead_of_queued_without_bound() {
    let mut rig = Rig::ready();
    let stream = rig.open_stream();
    rig.host.command(Command::Start);
    rig.until(1000, |r| r.host.streaming());
    let frame = vec![0x9c; sbc_of(stream).frame_len()];
    // The controller stops taking packets: nothing is polled while audio keeps arriving.
    for _ in 0..200 { rig.host.send_media(&frame, 1, 128); }
    assert_eq!(rig.host.media_dropped(), 200 - 12);
    assert!(!rig.host.media_room());
    rig.run(200);
    assert!(rig.host.media_room() && rig.fake.log.media_packets == 12 && rig.fake.log.media_sequence_gaps == 0, "{:?}", (rig.fake.log.media_packets, rig.fake.log.media_sequence_gaps));
}

#[test]
fn random_bytes_and_random_packets_never_panic() {
    let mut seed = 0x1234_5678u32;
    let mut next = move || { seed ^= seed << 13; seed ^= seed >> 17; seed ^= seed << 5; seed };
    // Raw UART noise before and after bring-up.
    let mut rig = Rig::ready();
    rig.open_stream();
    for _ in 0..300 {
        let bytes: Vec<u8> = (0..next() % 64).map(|_| next() as u8).collect();
        rig.host.rx(&bytes, rig.now);
        rig.step();
    }
    // Well-framed events and ACL data with random contents, through a second H5 peer state.
    let mut peer = crate::h5::H5::new(0);
    let mut host = Host::new(settings(), 0);
    let mut now = 0;
    for round in 0..6000u32 {
        let (mut to_peer, mut to_host) = (Vec::new(), Vec::new());
        host.poll(now, &mut to_peer);
        let mut sink = Vec::new();
        peer.rx(&to_peer, now, &mut sink);
        if round > 40 {
            let len = (next() % 40) as usize;
            let mut body: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            if next() % 3 == 0 {
                let code = [0x03, 0x04, 0x05, 0x06, 0x08, 0x0e, 0x0f, 0x13, 0x17, 0x18, 0x22, 0x2f, 0x07, 0x02, 0x31][(next() % 15) as usize];
                let mut packet = vec![code, body.len() as u8]; packet.append(&mut body);
                peer.send(crate::h5::EVT, packet);
            } else {
                let mut packet = vec![0x0b, 0x20, body.len() as u8, 0];
                if body.len() >= 4 { body[0] = (body.len() - 4) as u8; body[1] = 0; body[2] = [1, 0x40, 0x41, 0x42][(next() % 4) as usize]; body[3] = 0; }
                packet.append(&mut body);
                peer.send(crate::h5::ACL, packet);
            }
        }
        peer.poll(now, &mut to_host);
        host.rx(&to_host, now);
        let _ = host.events().count();
        now += 7;
    }
}

fn with_codecs(ours: &[Kind], theirs: &[Kind]) -> Rig {
    let mut fake = Fake::new(0);
    fake.devices[0].codecs = theirs.iter().map(|kind| codec::capabilities(*kind)).collect();
    Rig::with(Settings { codecs: ours.to_vec(), ..settings() }, fake)
}

#[test]
fn the_best_codec_both_sides_have_is_configured_and_sbc_is_the_fallback() {
    let all = Kind::BY_QUALITY;
    for (ours, theirs, expected) in [
        (&all[..], &[Kind::Aac, Kind::AptX, Kind::AptXHd, Kind::Ldac][..], Kind::Ldac),
        (&all[..], &[Kind::Aac, Kind::AptX, Kind::AptXHd][..], Kind::AptXHd),
        (&all[..], &[Kind::Aac, Kind::AptX][..], Kind::AptX),
        (&all[..], &[Kind::Aac][..], Kind::Aac),
        (&all[..], &[][..], Kind::Sbc),
        // The listener's override: one codec first, SBC behind it.
        (&[Kind::Aac, Kind::Sbc][..], &[Kind::Aac, Kind::AptXHd, Kind::Ldac][..], Kind::Aac),
        (&[Kind::Ldac, Kind::Sbc][..], &[Kind::Aac, Kind::AptX][..], Kind::Sbc),
        // A build without the other encoders takes SBC from any headphones.
        (&[][..], &[Kind::Aac, Kind::AptXHd, Kind::Ldac][..], Kind::Sbc),
    ] {
        let mut rig = with_codecs(ours, theirs);
        rig.until(5000, |r| r.host.ready());
        let stream = rig.open_stream();
        assert_eq!(stream.codec.kind(), expected, "ours {ours:?}, theirs {theirs:?}");
        assert_eq!(codec::choose(&rig.fake.log.configured_element, true).map(|c| c.kind()), Some(expected));
        assert_eq!(rig.fake.log.configured.is_some(), expected == Kind::Sbc);
        rig.host.command(Command::Start);
        rig.until(1000, |r| r.host.streaming() && r.fake.log.streaming);
    }
}

#[test]
fn media_packets_carry_each_codec_in_its_own_framing() {
    // LDAC and aptX HD: an RTP header, then the payload as given. aptX: the payload alone.
    for (kind, bare) in [(Kind::Ldac, false), (Kind::AptXHd, false), (Kind::AptX, true), (Kind::Aac, false)] {
        let mut rig = with_codecs(&[kind, Kind::Sbc], &[kind]);
        rig.until(5000, |r| r.host.ready());
        let stream = rig.open_stream();
        assert_eq!((stream.codec.kind(), stream.codec.rtp()), (kind, !bare));
        rig.host.command(Command::Start);
        rig.until(1000, |r| r.host.streaming());
        let payload: Vec<u8> = (0..200u8).collect();
        rig.host.send_media_payload(&payload, 1024, false);
        rig.host.send_media_payload(&payload[..50], 0, true);
        rig.run(100);
        assert_eq!(rig.fake.log.media_packets, 2, "{kind:?}");
        assert_eq!(rig.fake.log.media, [&payload[..], &payload[..50]].concat(), "{kind:?}");
        assert_eq!((rig.fake.log.markers, rig.fake.log.media_sequence_gaps), (u32::from(!bare), 0), "{kind:?}");
        if !bare { assert_eq!(rig.fake.log.last_timestamp, 1024); }
    }
}

#[test]
fn headphones_that_reconnect_and_configure_a_codec_of_ours_themselves_are_followed() {
    for kind in [Kind::Ldac, Kind::AptXHd, Kind::AptX, Kind::Aac] {
        let mut rig = with_codecs(&Kind::BY_QUALITY, &[kind]);
        rig.until(5000, |r| r.host.ready());
        rig.open_stream();
        rig.fake.drop_link();
        rig.until(1000, |r| r.saw(|e| matches!(e, Event::Disconnected { .. })));
        assert!(rig.fake.reconnect(0));
        rig.until(6000, |r| r.host.stream().is_some());
        assert_eq!(rig.host.stream().unwrap().codec.kind(), kind);
        assert!(rig.saw(|e| matches!(e, Event::Connected { incoming: true, .. })));
    }
    // A codec this build does not have is refused on its endpoint: no stream from that configuration.
    let mut rig = with_codecs(&[Kind::Sbc], &[Kind::Ldac]);
    rig.until(5000, |r| r.host.ready());
    rig.open_stream();
    assert_eq!(rig.host.stream().unwrap().codec.kind(), Kind::Sbc);
    rig.fake.drop_link();
    rig.until(1000, |r| r.saw(|e| matches!(e, Event::Disconnected { .. })));
    assert!(rig.fake.reconnect(0));
    rig.run(1200);
    assert!(rig.host.stream().is_none(), "the LDAC endpoint does not exist in an SBC-only build");
    // Our own turn as initiator comes 1.5 s after the channel opened, and takes SBC.
    rig.until(6000, |r| r.host.stream().is_some());
    assert_eq!(rig.host.stream().unwrap().codec.kind(), Kind::Sbc);
}

fn volume_rig(absolute: bool) -> Rig {
    let mut fake = Fake::new(0);
    fake.devices[0].absolute_volume = absolute;
    let mut rig = Rig::with(settings(), fake);
    rig.until(5000, |r| r.host.ready());
    rig.open_stream();
    rig
}
fn volumes(rig: &Rig) -> Vec<(u8, bool)> {
    rig.events.iter().filter_map(|e| if let Event::Volume { level, initial } = e { Some((*level, *initial)) } else { None }).collect()
}

#[test]
fn absolute_volume_is_registered_set_and_followed_both_ways() {
    let mut rig = volume_rig(true);
    // The control channel opens a second after the stream; then events, registration, interim.
    rig.until(3000, |r| r.saw(|e| *e == Event::AbsoluteVolume));
    assert_eq!(rig.fake.log.avrcp_commands, [0x10, 0x31], "GetCapabilities, then RegisterNotification");
    assert_eq!(volumes(&rig), [(0x20, true)], "the headphones' own level, reported once as initial");
    // The player sets the level: SetAbsoluteVolume, and nothing reported back.
    rig.host.command(Command::SetVolume(0x50));
    rig.until(500, |r| r.fake.devices[0].volume == 0x50);
    rig.run(50);
    assert_eq!((rig.fake.log.volumes_set.clone(), volumes(&rig).len()), (vec![0x50], 1));
    // A level already held is not sent again.
    rig.host.command(Command::SetVolume(0x50));
    rig.run(50);
    assert_eq!(rig.fake.log.volumes_set, [0x50]);
    // The headphones' buttons: CHANGED, reported as theirs, and the registration renewed.
    assert!(rig.fake.volume_button(true));
    rig.until(500, |r| volumes(r).len() == 2);
    assert_eq!(volumes(&rig)[1], (0x58, false));
    rig.run(50);
    assert_eq!(rig.fake.log.avrcp_commands, [0x10, 0x31, 0x50, 0x31]);
    assert!(rig.fake.volume_button(false));
    assert!(!rig.fake.volume_button(false), "one change per registration until it is renewed");
    rig.until(500, |r| volumes(r).len() == 3);
    rig.run(50);
    assert!(rig.fake.volume_button(false));
    rig.until(500, |r| volumes(r).len() == 4);
    assert_eq!(volumes(&rig)[2..], [(0x50, false), (0x48, false)]);
    // Passthrough volume keys still arrive as buttons.
    rig.fake.press(0x41);
    rig.until(500, |r| r.saw(|e| *e == Event::Button(Button::VolumeUp)));
}

#[test]
fn quick_volume_changes_send_the_latest_and_an_echo_or_rounding_is_not_reported() {
    let mut rig = volume_rig(true);
    rig.fake.devices[0].volume_steps = Some(30);
    rig.fake.devices[0].echo_volume = true;
    rig.until(3000, |r| r.saw(|e| *e == Event::AbsoluteVolume));
    // A wheel spin: many levels before the first response; the first and the last are sent.
    for level in 40..=60 { rig.host.command(Command::SetVolume(level)); }
    rig.run(200);
    let set = rig.fake.log.volumes_set.clone();
    assert_eq!(set.len(), 2, "{set:?}");
    // Thirty steps: 60 becomes step 14 of 30, level 59.
    assert_eq!(*set.last().unwrap(), 59);
    assert_eq!(volumes(&rig).len(), 1, "the echoed CHANGED of our own level is not the headphones' change");
    // The echo ended the registration; it was renewed.
    assert_eq!(rig.fake.log.avrcp_commands.iter().filter(|p| **p == 0x31).count(), 2);
    // A level beyond 127 is clamped.
    rig.host.command(Command::SetVolume(200));
    rig.run(100);
    assert_eq!(*rig.fake.log.volumes_set.last().unwrap(), 127);
}

#[test]
fn lower_limit_bounds_a_set_already_waiting_for_an_avrcp_response() {
    let mut rig = volume_rig(true);
    rig.until(3000, |r| r.saw(|e| *e == Event::AbsoluteVolume));
    rig.host.command(Command::SetVolume(30));
    rig.host.command(Command::SetVolume(120));
    rig.host.set_volume_limit(45);
    rig.host.set_volume_limit(127); // Raising the ceiling must not revive the old queued level.
    rig.run(300);
    assert_eq!(rig.fake.log.volumes_set, [30, 45]);
    assert!(rig.saw(|e| *e == Event::AmplifierLevel(45)));
}

#[test]
fn without_absolute_volume_levels_stay_local_and_the_keys_are_buttons() {
    let mut rig = volume_rig(false);
    rig.until(3000, |r| !r.fake.log.avrcp_commands.is_empty());
    rig.run(200);
    assert_eq!(rig.fake.log.avrcp_commands, [0x10], "no registration without the volume event");
    assert!(!rig.saw(|e| *e == Event::AbsoluteVolume));
    rig.host.command(Command::SetVolume(0x60));
    rig.run(100);
    assert!(rig.fake.log.volumes_set.is_empty() && rig.fake.log.avrcp_commands == [0x10]);
    assert!(rig.fake.volume_button(true) && rig.fake.volume_button(false));
    rig.until(500, |r| r.saw(|e| *e == Event::Button(Button::VolumeDown)));
    assert!(rig.saw(|e| *e == Event::Button(Button::VolumeUp)) && volumes(&rig).is_empty());
}

#[test]
fn headphones_that_ask_the_player_for_its_volume_are_answered_and_notified() {
    let mut rig = volume_rig(false);
    rig.until(3000, |r| !r.fake.log.avrcp_commands.is_empty());
    rig.host.command(Command::SetVolume(0x30));
    // Registration at the player's target: interim with the player's level, then a change.
    assert!(rig.fake.send_control(&crate::avrcp::register_volume(7)));
    rig.until(500, |r| !r.fake.log.avrcp_responses.is_empty());
    assert_eq!(crate::avrcp::reply(&rig.fake.log.avrcp_responses[0]), Some((7, crate::avrcp::Reply::VolumeInterim(0x30))));
    rig.host.command(Command::SetVolume(0x34));
    rig.until(500, |r| r.fake.log.avrcp_responses.len() == 2);
    assert_eq!(crate::avrcp::reply(&rig.fake.log.avrcp_responses[1]), Some((7, crate::avrcp::Reply::VolumeChanged(0x34))));
    // SetAbsoluteVolume sent to the player: accepted and reported as the headphones' level.
    assert!(rig.fake.send_control(&crate::avrcp::set_volume(8, 0x11)));
    rig.until(500, |r| volumes(r) == [(0x11, false)] && r.fake.log.avrcp_responses.len() == 3);
    assert_eq!(crate::avrcp::reply(&rig.fake.log.avrcp_responses[2]), Some((8, crate::avrcp::Reply::VolumeSet(0x11))));
}

#[test]
fn the_device_id_record_is_read_once_after_the_stream_opens() {
    let id = crate::sdp::DeviceId { source: 1, vendor: 0x0abc, product: 0x1234, version: 0x0101 };
    let mut fake = Fake::new(0);
    fake.devices[0].device_id = Some(id);
    let mut rig = Rig::with(settings(), fake);
    rig.until(5000, |r| r.host.ready());
    rig.open_stream();
    assert_eq!(rig.fake.log.device_id_queries, 0);
    rig.until(5000, |r| r.saw(|e| *e == Event::DeviceId(id)));
    rig.run(1000);
    assert_eq!(rig.fake.log.device_id_queries, 1);
    // A device without the record: one query, no event, the stream unaffected.
    let mut rig = volume_rig(false);
    rig.run(5000);
    assert_eq!(rig.fake.log.device_id_queries, 1);
    assert!(!rig.saw(|e| matches!(e, Event::DeviceId(_))) && rig.host.stream().is_some());
}

#[test]
fn a_stream_reports_a2dp_to_the_controller_and_reads_rssi_and_link_quality() {
    let mut rig = Rig::ready();
    rig.fake.rssi = -9;
    let id = crate::sdp::DeviceId { source: 1, vendor: 0x0abc, product: 0x1234, version: 0x0107 };
    rig.fake.devices[0].device_id = Some(id);
    let stream = rig.open_stream();
    // Connected: A2DP on the link, not active.
    rig.run(20);
    assert_eq!(rig.fake.log.profile_reports, [vec![1, 0x0b, 0, 0x04, 0]]);
    assert_eq!(rig.host.link_stats().rssi, None, "read only while streaming");
    rig.host.command(Command::Start);
    rig.until(1000, |r| r.host.streaming());
    rig.play(stream, 70);
    rig.run(2100);
    assert_eq!(rig.fake.log.profile_reports.last(), Some(&vec![1, 0x0b, 0, 0x04, 0x04]));
    rig.run(1000);
    let stats = rig.host.link_stats();
    assert_eq!((stats.rssi, stats.link_quality, stats.acl_buffers, stats.tx_power, stats.afh_channels), (Some(-9), Some(255), 8, Some(4), Some(76)));
    // The Device ID record, read over SDP a few seconds after the stream opened.
    assert!(rig.saw(|e| *e == Event::DeviceId(id)));
    // Two reads in about 2.2 s of streaming, and none of them fills the event timeline.
    assert!(rig.fake.log.commands.iter().filter(|&&c| c == 0x1405).count() >= 2);
    assert!(rig.host.diagnostics().timeline.iter().all(|t| t.2 != 0x1405 && t.2 != 0x1403));
    rig.host.command(Command::Suspend);
    rig.until(1000, |r| !r.fake.log.streaming);
    rig.run(20);
    assert_eq!(rig.fake.log.profile_reports.last(), Some(&vec![1, 0x0b, 0, 0x04, 0]));
    rig.host.command(Command::Disconnect);
    rig.until(1000, |r| r.saw(|e| matches!(e, Event::Disconnected { .. })));
    rig.run(20);
    assert_eq!(rig.fake.log.profile_reports.last(), Some(&vec![0, 0]));
    assert_eq!(rig.fake.log.profile_reports.len(), 4);
    assert_eq!(rig.host.link_stats().rssi, None);
}

#[test]
fn a_controller_that_stops_sending_over_the_air_is_counted_as_a_credit_stall() {
    let mut rig = Rig::ready();
    let stream = rig.open_stream();
    rig.host.command(Command::Start);
    rig.until(1000, |r| r.host.streaming());
    // The radio sends nothing for 300 ms (interference, Wi-Fi taking the air).
    let now = rig.now;
    rig.fake.air.stalls = vec![(now, now + 300)];
    let frame = vec![0x9c; sbc_of(stream).frame_len()];
    for _ in 0..30 { if rig.host.media_room() { rig.host.send_media(&frame, 1, 128); } rig.run(10); }
    let during = rig.host.link_stats();
    assert_eq!(during.acl_in_use, 8, "every controller buffer is waiting for the air");
    assert!(rig.host.acl_backlog() >= 8);
    rig.run(200);
    let after = rig.host.link_stats();
    assert_eq!(after.credit_stalls, 1);
    assert!((150..=320).contains(&after.credit_stall_max_ms), "{after:?}");
    assert_eq!((after.acl_in_use, rig.fake.log.acl_overflow), (0, 0));
}

#[test]
fn a_lost_frame_is_resent_after_the_fast_timeout_and_counted() {
    let mut rig = Rig::ready();
    let stream = rig.open_stream();
    rig.host.command(Command::Start);
    rig.until(1000, |r| r.host.streaming());
    let frame = vec![0x9c; sbc_of(stream).frame_len()];
    let before = rig.host.link_stats();
    // The next write is lost on the line (the controller's receive FIFO overran).
    rig.host.send_media(&frame, 1, 128);
    rig.lose_every = 1;
    rig.step();
    rig.lose_every = 0;
    let lost_at = rig.now;
    rig.until(400, |r| r.fake.log.media_packets > 0);
    let resent_after = rig.now - lost_at;
    assert!((90..=120).contains(&resent_after), "{resent_after} ms");
    let after = rig.host.link_stats();
    assert_eq!(after.h5_resends - before.h5_resends, 1);
    assert!(after.h5_retransmissions > before.h5_retransmissions);
}

#[test]
fn a_pairing_that_stalls_times_out_closes_the_link_and_an_immediate_retry_pairs() {
    use crate::host::{SECURE_LIMIT_MS, Step};
    let mut rig = Rig::ready();
    rig.fake.devices[0].stalls_pairing = true;
    let addr = rig.addr();
    rig.host.command(Command::Connect(addr));
    rig.until(3000, |r| r.saw(|e| *e == Event::Step { addr, step: Step::Authentication }));
    assert!(!rig.host.can_connect() && !rig.host.can_scan(), "the attempt is open");
    rig.until(SECURE_LIMIT_MS + 1000, |r| r.saw(|e| *e == Event::TimedOut { addr, step: Step::Authentication }));
    assert!(rig.now >= SECURE_LIMIT_MS);
    rig.until(4000, |r| r.host.can_connect());
    // The headphones are in pairing mode again: the next attempt pairs, step by step.
    rig.fake.devices[0].stalls_pairing = false;
    rig.events.clear();
    rig.host.command(Command::Connect(addr));
    rig.until(5000, |r| r.host.stream().is_some());
    let steps: Vec<_> = rig.events.iter().filter_map(|e| match e { Event::Step { step, .. } => Some(*step), _ => None }).collect();
    assert_eq!(steps, [Step::Authentication, Step::Encryption]);
}

#[test]
fn a_page_without_an_answer_is_cancelled_at_its_limit_and_a_cancel_ends_one_at_once() {
    use crate::host::{PAGE_LIMIT_MS, Step};
    let mut rig = Rig::ready();
    rig.fake.devices[0].powered = false;
    // A controller that never reports the page timeout.
    rig.fake.page_ms = 60_000;
    let addr = rig.addr();
    rig.host.command(Command::Connect(addr));
    rig.until(PAGE_LIMIT_MS + 1000, |r| r.saw(|e| *e == Event::TimedOut { addr, step: Step::Page }));
    assert!(rig.now >= PAGE_LIMIT_MS && rig.fake.log.commands.contains(&0x0408));
    rig.run(50);
    assert!(rig.host.can_connect() && rig.host.can_scan());
    assert!(!rig.saw(|e| matches!(e, Event::ConnectFailed { .. })), "the cancelled page is not reported twice");
    // The listener asks for something else during a page: the cancel ends it, and a scan runs.
    rig.fake.page_ms = 5120;
    rig.events.clear();
    let started = rig.now;
    rig.host.command(Command::Connect(addr));
    rig.run(30);
    assert_eq!(rig.host.paging(), Some(addr));
    assert!(!rig.host.can_scan());
    rig.host.command(Command::CancelConnect);
    rig.until(500, |r| r.saw(|e| *e == Event::ConnectFailed { addr, status: 0x02 }));
    assert!(rig.host.can_scan());
    rig.host.command(Command::Scan);
    rig.until(3000, |r| r.saw(|e| *e == Event::ScanFinished));
    assert!(rig.now - started < 5120, "the page did not run to its timeout");
}

#[test]
fn a_later_scan_knows_names_without_asking_again() {
    let mut rig = Rig::ready();
    let mut second = Headphones::new(0x12, "Second Fixture");
    second.named_in_inquiry = false;
    rig.fake.devices.push(second);
    let asked = |rig: &Rig| rig.fake.log.commands.iter().filter(|&&c| c == 0x0419).count();
    rig.host.command(Command::Scan);
    rig.until(3000, |r| r.saw(|e| *e == Event::ScanFinished));
    assert_eq!(asked(&rig), 1);
    rig.events.clear();
    rig.host.command(Command::Scan);
    rig.until(3000, |r| r.saw(|e| *e == Event::ScanFinished));
    assert_eq!(asked(&rig), 1, "no second name request");
    assert!(rig.saw(|e| matches!(e, Event::Found { addr, name: Some(name), .. } if addr[0] == 0x12 && name == "Second Fixture")));
}

#[test]
fn a_connection_during_a_name_request_cancels_that_request() {
    let mut rig = Rig::ready();
    let mut second = Headphones::new(0x12, "Second Fixture");
    second.named_in_inquiry = false;
    rig.fake.devices.push(second);
    rig.host.command(Command::Scan);
    rig.until(3000, |r| r.fake.log.commands.contains(&0x0419));
    let addr = rig.addr();
    rig.host.command(Command::Connect(addr));
    rig.until(3000, |r| r.saw(|e| matches!(e, Event::Connected { .. })));
    assert!(rig.fake.log.commands.contains(&0x041a), "Remote Name Request Cancel sent");
    assert!(rig.saw(|e| *e == Event::ScanFinished));
}

#[test]
fn automatic_takes_the_best_codec_both_sides_have_aac_before_sbc() {
    // Product feedback, V0.133: AirPods Pro 2 (SBC and AAC) got SBC in Automatic.
    for (theirs, expected) in [(&[Kind::Aac][..], Kind::Aac), (&[Kind::Aac, Kind::AptX][..], Kind::AptX), (&[Kind::Aac, Kind::Ldac][..], Kind::Ldac), (&[][..], Kind::Sbc)] {
        let mut rig = with_codecs(&Kind::AUTOMATIC, theirs);
        rig.until(5000, |r| r.host.ready());
        assert_eq!(rig.open_stream().codec.kind(), expected, "theirs {theirs:?}");
        assert_eq!(rig.host.origin(), Some(crate::host::Origin::Ours));
    }
}

#[test]
fn headphones_that_set_up_sbc_themselves_are_moved_to_the_best_codec_before_audio() {
    use crate::host::Origin;
    for (theirs, expected) in [(&[Kind::Aac][..], Kind::Aac), (&[Kind::Aac, Kind::Ldac][..], Kind::Ldac), (&[][..], Kind::Sbc)] {
        let mut rig = with_codecs(&Kind::AUTOMATIC, theirs);
        rig.fake.devices[0].configures_sbc = true;
        rig.until(5000, |r| r.host.ready());
        rig.open_stream();
        rig.fake.drop_link();
        rig.until(1000, |r| r.saw(|e| matches!(e, Event::Disconnected { .. })));
        rig.events.clear();
        // They reconnect and configure SBC on our SBC endpoint themselves.
        assert!(rig.fake.reconnect(0));
        rig.until(8000, |r| r.host.stream().is_some() && r.saw(|e| matches!(e, Event::Stream(Some(_)))));
        let stream = rig.host.stream().unwrap();
        assert_eq!(stream.codec.kind(), expected, "theirs {theirs:?}");
        let opened: Vec<_> = rig.events.iter().filter_map(|e| match e { Event::Stream(s) => Some(s.map(|s| s.codec.kind())), _ => None }).collect();
        assert_eq!(opened, [Some(expected)], "one stream reported, the final one, and none closed");
        let origin = if expected == Kind::Sbc { Origin::Theirs { checked: true } } else { Origin::Upgraded { from: Kind::Sbc } };
        assert_eq!(rig.host.origin(), Some(origin));
        assert_eq!(crate::codec::identify(&rig.fake.log.configured_element).map(|c| c.0), Some(expected));
        rig.host.command(Command::Start);
        rig.until(1000, |r| r.host.streaming() && r.fake.log.streaming);
    }
}

#[test]
fn bench_sink_accepts_only_exact_peer_and_sbc_offer() {
    let mut fake = Fake::new(0);
    fake.devices.push(Headphones::new(0x12, "Foreign fixture"));
    let allowed = fake.devices[0].addr;
    let foreign = fake.devices[1].addr;
    fake.devices[0].codecs = vec![crate::host::BENCH_SBC_OFFER.to_vec()];
    fake.devices[1].key = Some([7; 16]);
    let mut rig = Rig::with(Settings { bench_peer: Some(allowed),
        keys: vec![Key { addr: foreign, key: [7; 16], kind: 4 }], ..settings() }, fake);
    rig.until(5000, |r| r.host.ready());
    rig.host.command(Command::Connect(foreign));
    rig.run(200);
    assert!(!rig.saw(|event| *event == Event::Connecting(foreign)));
    assert!(rig.fake.reconnect(1));
    rig.until(500, |r| r.fake.log.commands.contains(&0x040a));
    assert!(!rig.saw(|event| *event == Event::Connecting(foreign)));

    let stream = rig.open_stream();
    assert_eq!(rig.fake.log.configured_element, crate::host::BENCH_SBC_OFFER);
    assert_eq!(rig.fake.log.configured, None, "the default SBC endpoint must not be selected");
    rig.run(250);
    assert!(!rig.host.streaming() && rig.fake.log.media_packets == 0);
    rig.host.command(Command::Start);
    rig.until(1000, |r| r.host.streaming());
    rig.play(stream, 20);
    assert_eq!(rig.fake.log.sbc_frames, 20);

    let mut unmatched = Rig::with(Settings { bench_peer: Some(allowed), ..settings() }, Fake::new(0));
    unmatched.until(5000, |r| r.host.ready());
    unmatched.host.command(Command::Connect(allowed));
    unmatched.until(5000, |r| r.saw(|event| *event == Event::Stream(None)));
    assert!(unmatched.host.stream().is_none());
    assert_eq!(unmatched.fake.log.media_packets, 0);
}

#[test]
fn bench_sink_admits_an_inbound_page_from_the_exact_unbonded_peer() {
    let mut fake = Fake::new(0);
    let allowed = fake.devices[0].addr;
    // Set only the fake side's key so it emits the inbound page. The Q2-side
    // store is empty, which is the production refusal the bench mode changes.
    fake.devices[0].key = Some([3; 16]);
    let mut rig = Rig::with(Settings { bench_peer: Some(allowed), ..settings() }, fake);
    rig.until(5000, |r| r.host.ready());
    assert!(rig.fake.reconnect(0));
    rig.until(1000, |r| r.fake.log.commands.contains(&0x0409));
    assert!(rig.saw(|event| *event == Event::Connecting(allowed)));
}
