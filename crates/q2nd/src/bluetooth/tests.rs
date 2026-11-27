//! The Bluetooth thread against the in-process controller and headphones.
use super::*;
use std::sync::atomic::AtomicU32;

struct Fixture { bluetooth: Bluetooth, endpoint: Arc<pcm::Endpoint>, route: Arc<Mutex<Vec<bool>>>, buttons: Arc<Mutex<Vec<Button>>>, notified: Arc<AtomicU32>, store: PathBuf, off: Arc<AtomicBool>,
    keys: Arc<AtomicU32>, power_cycle: Arc<AtomicBool>, status: PathBuf, _dir: tempdir::Dir }

mod tempdir {
    pub struct Dir(pub std::path::PathBuf);
    impl Dir {
        pub fn new(name: &str) -> Self {
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build/bluetooth-tests").join(format!("{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Dir { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }
}

fn fixture(name: &str) -> Fixture { fixture_with(name, "sbc") }
fn fixture_with(name: &str, codec: &str) -> Fixture { fixture_setup(name, codec, None) }
fn fixture_setup(name: &str, codec: &str, setup: Option<fn(&mut q2_bt::fake::Fake)>) -> Fixture {
    let dir = tempdir::Dir::new(name);
    let mut options = Options::simulated(&dir.0);
    options.reconnect = true;
    options.codecs = codecs(codec);
    options.fake_setup = setup;
    let store = options.store.clone();
    let off = options.fake_off.clone();
    let keys = options.fake_keys.clone();
    let power_cycle = options.fake_power_cycle.clone();
    let status = dir.0.join("bluetooth-status");
    options.status_file = Some(status.clone());
    let endpoint = Arc::new(pcm::Endpoint::new());
    let notified = Arc::new(AtomicU32::new(0));
    let counter = notified.clone();
    let bluetooth = Bluetooth::with_endpoint(options, Arc::new(move || { counter.fetch_add(1, Ordering::Relaxed); }), endpoint.clone());
    let (route, buttons) = (Arc::new(Mutex::new(Vec::new())), Arc::new(Mutex::new(Vec::new())));
    { let route = route.clone(); bluetooth.set_output(Arc::new(move |open| route.lock().unwrap().push(open))); }
    { let buttons = buttons.clone(); bluetooth.set_buttons(Arc::new(move |button| buttons.lock().unwrap().push(button))); }
    Fixture { bluetooth, endpoint, route, buttons, notified, store, off, keys, power_cycle, status, _dir: dir }
}

impl Fixture {
    fn until(&self, what: &str, done: impl Fn(&State) -> bool) -> State {
        let limit = Instant::now() + Duration::from_secs(20);
        loop {
            let state = self.bluetooth.state();
            if done(&state) { return state }
            assert!(Instant::now() < limit, "{what}: {:?} {:?} {:?}", state.phase, state.link, state.notice);
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    /// `holds` stays true for `period`.
    fn stays(&self, what: &str, period: Duration, holds: impl Fn(&State) -> bool) {
        let end = Instant::now() + period;
        while Instant::now() < end {
            let state = self.bluetooth.state();
            assert!(holds(&state), "{what}: {:?} {:?} {:?}", state.phase, state.link, state.notice);
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    fn pair(&self) -> State {
        self.bluetooth.set_enabled(true);
        self.until("ready", |s| s.phase == Phase::Ready);
        self.bluetooth.scan();
        let found = self.until("scan", |s| !s.scanning && s.found.len() == 3 && s.found.iter().all(|d| d.name != "Unnamed Device"));
        self.bluetooth.connect(found.found[0].addr);
        self.until("stream", |s| s.stream.is_some())
    }
}

#[test]
fn store_round_trips_and_drops_incomplete_or_malformed_records() {
    let (a, b) = ([1, 2, 3, 4, 5, 6], [6, 5, 4, 3, 2, 1]);
    let device = |addr, name: &str| Device { addr, name: name.into(), class: 0x24_0404, rssi: 0, sink: false };
    let key = |addr| Key { addr, key: [0xab; 16], kind: 4 };
    let chosen = DeviceEq { choice: EqChoice::Chosen, enabled: false, path: "generated/over-ear/Generated%20Model".into(), name: "Generated Model (variant)".into() };
    let store = Store { devices: vec![device(a, "Generated Name With Spaces"), device(b, "Second")], keys: vec![key(a), key(b)], last: Some(a),
        saved: vec![(a, Saved { volume: Some(37), eq: Some(chosen), id: Some(DeviceId { source: 2, vendor: 0xabcd, product: 0x0102, version: 0x0300 }) }),
            (b, Saved { volume: None, eq: Some(DeviceEq { choice: EqChoice::Automatic, enabled: true, path: String::new(), name: String::new() }), id: None })],
        seen: vec![(a, 1_795_000_000)] };
    assert_eq!(Store::parse(&store.text()), store);
    // A file written before `seen` records existed reads with none.
    let older: String = store.text().lines().filter(|l| !l.starts_with("seen ")).map(|l| format!("{l}\n")).collect();
    assert_eq!(Store::parse(&older), Store { seen: Vec::new(), ..Store::parse(&store.text()) });
    let orphan = "device 010203040506 240404 No Key\nkey 0a0b0c0d0e0f 4 000102030405060708090a0b0c0d0e0f\nlast 0a0b0c0d0e0f\nseen 0a0b0c0d0e0f 12\nkey zz 4 00\ngarbage\n\n\
        volume 0a0b0c0d0e0f 40\neq 0a0b0c0d0e0f auto on generated/x X\n";
    assert_eq!(Store::parse(orphan), Store::default());
    assert_eq!(Store::parse(""), Store::default());
    // Per-device records out of range or malformed are dropped; the device stays.
    let paired = "device 010203040506 240404 One\nkey 010203040506 4 abababababababababababababababab\n";
    let bad = format!("{paired}volume 010203040506 101\neq 010203040506 sometimes on p n\neq 010203040506 auto maybe p n\nid 010203040506 1 2 3\nid 010203040506 0001 zzzz 0003 0004\n");
    let parsed = Store::parse(&bad);
    assert_eq!((parsed.devices.len(), parsed.saved.len()), (1, 0));
    // Forgetting a device forgets what was kept for it.
    let mut store = store;
    store.forget(a);
    assert_eq!((store.devices.len(), store.saved.len(), store.last), (1, 1, None));
}

#[test]
fn volume_levels_map_both_ways_without_drift() {
    assert_eq!((to_level(0), to_level(50), to_level(100), to_level(255)), (0, 64, 127, 127));
    assert_eq!((from_level(0), from_level(64), from_level(127), from_level(200)), (0, 50, 100, 100));
    for volume in 0..=100u8 { assert_eq!(from_level(to_level(volume)), volume, "{volume}"); }
    for level in 0..=127u8 { assert!(to_level(from_level(level)).abs_diff(level) <= 1, "{level}"); }
}

#[test]
fn absolute_volume_follows_the_headphones_saves_per_device_and_restores_on_connect() {
    let f = fixture("volume");
    let state = f.pair();
    let addr = state.connected().unwrap();
    // No saved volume: the headphones' own level (0x20 of 127) is taken, once.
    let state = f.until("absolute volume", |s| s.output.as_ref().is_some_and(|o| o.absolute && o.saved.volume == Some(from_level(0x20))));
    let output = state.output.unwrap();
    assert_eq!((output.addr, output.name.as_str(), output.saved.volume), (addr, "Fixture Headphones", Some(from_level(0x20))));
    // Player and headphone changes both publish a revision.
    f.bluetooth.set_volume(addr, 70);
    f.until("volume set", |s| s.output.as_ref().is_some_and(|o| o.saved.volume == Some(70)));
    // A press on the headphones: their new level, counted.
    f.keys.fetch_add(1, Ordering::AcqRel);
    let volume = from_level(to_level(70) + 8);
    let state = f.until("headphone key", |s| s.output.as_ref().is_some_and(|o| o.saved.volume == Some(volume)));
    let before_reconnect = state.output.as_ref().unwrap().volume_revision;
    assert_eq!(state.output.unwrap().saved.volume, Some(volume));
    // The Device ID record arrives after the stream opened.
    f.until("device id", |s| s.output.as_ref().is_some_and(|o| o.saved.id.is_some_and(|id| id.vendor == 0x7ffe)));
    // An EQ choice for this device.
    let eq = DeviceEq { choice: EqChoice::Chosen, enabled: true, path: "generated/over-ear/Generated%20Model".into(), name: "Generated Model".into() };
    f.bluetooth.set_device_eq(addr, Some(eq.clone()));
    f.until("eq saved", |s| s.output.as_ref().is_some_and(|o| o.saved.eq.as_ref() == Some(&eq)));
    // Written within the save delay, privately.
    let limit = Instant::now() + Duration::from_secs(5);
    let saved = loop {
        let saved = Store::load(&Blocking::dedicated_thread(), &f.store).saved(addr);
        if saved.volume == Some(volume) || Instant::now() > limit { break saved }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!((saved.volume, saved.eq.as_ref(), saved.id.map(|id| id.product)), (Some(volume), Some(&eq), Some(1)));
    // Disconnected and connected again: the saved volume and choice come back with the
    // output; the headphones' report at connection does not override the player's level.
    f.bluetooth.disconnect();
    f.until("disconnected", |s| s.link == Link::Idle && s.output.is_none());
    f.bluetooth.connect(addr);
    let state = f.until("reconnected", |s| s.output.as_ref().is_some_and(|o| o.absolute));
    std::thread::sleep(Duration::from_millis(300));
    let output = f.bluetooth.state().output.unwrap();
    assert_ne!(output.volume_revision, before_reconnect);
    assert_eq!((output.saved.volume, output.saved.eq.as_ref()), (Some(volume), Some(&eq)), "{:?}", state.output);
    f.bluetooth.shutdown();
}

#[test]
fn volume_limit_reaches_the_headphone_amplifier_at_pairing_on_keys_and_after_reconnect() {
    let mut f = fixture_setup("volume-limit", "sbc", Some(|fake| fake.devices[0].volume = 120));
    let kept = KeptFake::default();
    f.bluetooth.options.fake_kept = Some(kept.clone());
    f.bluetooth.set_volume_limit(40);
    let addr = f.pair().connected().unwrap();
    let at = |level| move |s: &State| s.output.as_ref().is_some_and(|o| o.absolute && o.saved.volume == Some(level));
    f.until("initial high amplifier corrected", at(40));
    f.bluetooth.set_volume_limit(23);
    f.until("live reduction confirmed", at(23));
    f.bluetooth.set_volume_limit(90);
    f.keys.fetch_add(1, Ordering::AcqRel);
    f.until("key starts from the capped level", at(from_level(to_level(23) + 8)));
    f.bluetooth.set_volume(addr, 80);
    f.until("higher listening level", at(80));
    f.bluetooth.disconnect();
    f.until("disconnected", |s| s.link == Link::Idle && s.output.is_none());
    f.bluetooth.set_volume_limit(16);
    f.bluetooth.connect(addr);
    f.until("saved high amplifier corrected on reconnect", at(16));
    f.keys.fetch_add(1, Ordering::AcqRel);
    f.until("key corrected at the amplifier", |s| kept.volume_sets.load(Ordering::Acquire) >= 5 && at(16)(s));
    f.bluetooth.shutdown();
    let (fake, _) = kept.take().unwrap();
    assert_eq!(fake.log.volumes_set, [40, 23, 80, 16, 16].map(to_level));
    assert_eq!(fake.devices[0].volume, to_level(16));
    assert_eq!(Store::load(&Blocking::dedicated_thread(), &f.store).saved(addr).volume, Some(16));
}

#[test]
fn software_volume_headphones_restore_only_the_bounded_saved_level() {
    let f = fixture_setup("software-volume-limit", "sbc", Some(|fake| fake.devices[0].absolute_volume = false));
    let addr = f.pair().connected().unwrap();
    f.bluetooth.set_volume(addr, 75);
    f.until("software volume saved", |s| s.output.as_ref().is_some_and(|o| !o.absolute && o.saved.volume == Some(75)));
    f.bluetooth.disconnect();
    f.until("disconnected", |s| s.link == Link::Idle && s.output.is_none());
    f.bluetooth.set_volume_limit(32);
    f.bluetooth.connect(addr);
    let state = f.until("software output reopened", |s| s.output.is_some());
    assert_eq!(state.output.unwrap().saved.volume, Some(32));
    f.bluetooth.set_volume_limit(22);
    f.until("active software volume capped", |s| s.output.as_ref().is_some_and(|o| !o.absolute && o.saved.volume == Some(22)));
    f.bluetooth.shutdown();
    assert_eq!(Store::load(&Blocking::dedicated_thread(), &f.store).saved(addr).volume, Some(22));
}

#[test]
fn rounded_above_limit_keeps_software_attenuation() {
    let mut f = fixture_setup("rounded-volume-limit", "sbc", Some(|fake| {
        fake.devices[0].volume = 127;
        fake.devices[0].volume_steps = Some(5);
    }));
    let kept = KeptFake::default();
    f.bluetooth.options.fake_kept = Some(kept.clone());
    f.bluetooth.set_volume_limit(35);
    let addr = f.pair().connected().unwrap();
    f.until("bounded listening level", |s| s.output.as_ref().is_some_and(|o| o.saved.volume == Some(35)));
    f.until("rounded set reached amplifier", |s| kept.volume_sets.load(Ordering::Acquire) >= 1
        && s.output.as_ref().is_some_and(|o| !o.absolute));
    f.bluetooth.disconnect();
    f.until("disconnected", |s| s.link == Link::Idle && s.output.is_none());
    // Even with a relaxed cap on reconnect, the actual 51 still exceeds the quiet saved level.
    f.bluetooth.set_volume_limit(100);
    f.bluetooth.connect(addr);
    f.until("restored quiet volume", |s| s.output.as_ref().is_some_and(|o| o.saved.volume == Some(35)));
    f.until("quiet restore reached amplifier", |s| kept.volume_sets.load(Ordering::Acquire) >= 2
        && s.output.as_ref().is_some_and(|o| !o.absolute));
    // After the rounded acknowledgement, software still owns gain.
    f.stays("quiet restore keeps software gain", Duration::from_millis(100),
        |s| s.output.as_ref().is_some_and(|o| !o.absolute));
    f.bluetooth.shutdown();
    let (fake, _) = kept.take().unwrap();
    assert_eq!(fake.log.volumes_set, [51, 51]); // 2 of the amplifier's 5 steps, above the requested 44.
}

#[test]
fn pairing_saves_the_key_privately_routes_audio_and_plays() {
    let f = fixture("pair");
    assert_eq!(f.bluetooth.state().summary(), "Off");
    let state = f.pair();
    assert_eq!(state.stream.unwrap().codec, q2_bt::codec::Codec::Sbc(q2_bt::sbc::Params { rate: 44100, blocks: 16, joint: true, snr: false, bitpool: 53 }));
    assert_eq!((state.paired.len(), state.summary()), (1, "Fixture Headphones".into()));
    assert!(state.controller.is_some_and(|v| v.rom_version == Some(2)));
    assert_eq!(*f.route.lock().unwrap(), [true]);
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(std::fs::metadata(&f.store).unwrap().permissions().mode() & 0o777, 0o600);
    let saved = Store::load(&Blocking::dedicated_thread(), &f.store);
    assert_eq!((saved.devices.len(), saved.keys.len(), saved.last.is_some()), (1, 1, true));
    // The player's PCM: 48 kHz mono is converted to the stream's 44.1 kHz stereo.
    let writer = pcm::Writer::open(&f.endpoint, 48000, 1, 400_000).unwrap();
    let block = vec![1000i16; 4800];
    let blocking = Blocking::dedicated_thread();
    let started = Instant::now();
    for _ in 0..12 {
        let mut written = 0;
        while written < block.len() {
            let n = writer.write_s16(&blocking, &block[written..]);
            if n > 0 { written += n as usize; } else { assert_eq!(writer.recover(n as i32), 0, "write failed with {n}"); }
        }
    }
    f.until("playing", |s| matches!(s.link, Link::Playing(_)));
    writer.drain(&blocking);
    // 1.2 s of audio is paced by the stream, not accepted at once.
    assert!(started.elapsed() >= Duration::from_millis(900), "{:?}", started.elapsed());
    assert_eq!(f.bluetooth.state().counters.0, 0, "no media packets dropped");
    drop(writer);
    // Two seconds without audio suspends the stream; the link stays.
    f.until("suspended", |s| matches!(s.link, Link::Connected(_)));
    assert!(f.notified.load(Ordering::Relaxed) > 5 && f.buttons.lock().unwrap().is_empty());
    // Switching off does not wait for the thread; the state is Off at once.
    f.bluetooth.set_enabled(false);
    assert_eq!(f.bluetooth.state().phase, Phase::Off);
    f.bluetooth.shutdown();
    assert_eq!(f.bluetooth.state().phase, Phase::Off, "a retired thread publishes nothing");
    assert_eq!(*f.route.lock().unwrap(), [true, false]);
    assert!(pcm::Writer::open(&f.endpoint, 44100, 2, 400_000).is_err());
}

#[test]
fn headphones_switched_off_mid_stream_fail_the_write_and_leave_no_notice() {
    let f = fixture("switched-off");
    f.pair();
    let blocking = Blocking::dedicated_thread();
    let writer = pcm::Writer::open(&f.endpoint, 44100, 2, 400_000).unwrap();
    let block = coder::tone(4410);
    let limit = Instant::now() + Duration::from_secs(20);
    let failed = loop {
        assert!(Instant::now() < limit, "the write did not fail: {:?}", f.bluetooth.state().link);
        // Once audio is playing, the headphones are switched off.
        if matches!(f.bluetooth.state().link, Link::Playing(_)) { f.off.store(true, Ordering::Release); }
        let n = writer.write_s16(&blocking, &block);
        if n < 0 && writer.recover(n as i32) != 0 { break n }
    };
    assert_eq!(failed, -i64::from(libc::ENODEV));
    assert_eq!(pcm::Writer::error(failed as i32), "Bluetooth headphones disconnected");
    let state = f.until("disconnected", |s| s.link == Link::Idle);
    assert_eq!((state.stream.is_none(), state.notice.as_str()), (true, ""), "switching headphones off is not a problem to report");
    assert_eq!(*f.route.lock().unwrap(), [true, false]);
    assert!(writer.delay() > 0, "the unsent audio is still counted, for the player to keep");
    drop(writer);
    f.bluetooth.shutdown();
}

#[test]
fn a_restart_reconnects_to_the_last_headphones_and_forget_removes_them() {
    let f = fixture("reconnect");
    f.pair();
    f.bluetooth.disconnect();
    f.until("disconnected", |s| s.link == Link::Idle && s.stream.is_none());
    assert_eq!(*f.route.lock().unwrap(), [true, false]);
    // A new fake controller has headphones that are not bonded: the stored key is refused,
    // which is what a factory-reset pair of headphones does.
    f.bluetooth.set_enabled(false);
    f.bluetooth.set_enabled(true);
    let state = f.until("key refused", |s| s.phase == Phase::Ready && s.paired.is_empty() && !s.notice.is_empty());
    assert!(state.notice.contains("Pair them again"));
    assert_eq!(Store::load(&Blocking::dedicated_thread(), &f.store), Store::default());
    // Pairing again works, and Forget disconnects and clears the store.
    f.bluetooth.scan();
    let found = f.until("scan", |s| !s.scanning && !s.found.is_empty());
    f.bluetooth.connect(found.found[0].addr);
    let state = f.until("stream", |s| s.stream.is_some());
    f.bluetooth.forget(state.paired[0].addr);
    f.until("forgotten", |s| s.paired.is_empty() && s.link == Link::Idle);
    assert_eq!(Store::load(&Blocking::dedicated_thread(), &f.store), Store::default());
}

impl Fixture {
    fn status_line(&self) -> String { std::fs::read_to_string(&self.status).unwrap_or_default() }
    fn until_status(&self, what: &str, done: impl Fn(&str) -> bool) -> String {
        let limit = Instant::now() + Duration::from_secs(20);
        loop {
            let line = self.status_line();
            if done(&line) { return line }
            assert!(Instant::now() < limit, "{what}: {line:?}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    /// The fake controller's own log is not reachable from here; its effects are: a
    /// reconnect would page and leave `Connecting` behind.
    fn plays(&self, blocking: &Blocking, writer: &pcm::Writer, blocks: usize) {
        let block = coder::tone(4410);
        for _ in 0..blocks {
            let mut written = 0;
            while written < block.len() {
                let n = writer.write_s16(blocking, &block[written..]);
                if n > 0 { written += n as usize * 2; } else { assert_eq!(writer.recover(n as i32), 0, "write failed with {n}"); }
            }
        }
    }
}

#[test]
fn a_wifi_power_up_reinitialises_an_idle_controller_without_paging_the_headphones() {
    let f = fixture("resync-idle");
    f.pair();
    f.bluetooth.disconnect();
    f.until("disconnected", |s| s.link == Link::Idle && s.stream.is_none());
    let before = f.bluetooth.state();
    f.bluetooth.wifi_powered_up();
    let state = f.until("resynced", |s| s.resync.reinits == 1 && s.phase == Phase::Ready);
    assert_eq!(state.resync, Resync { checks: 1, alive: 1, lost: 0, reinits: 1, busy: 0 });
    // The same session and controller: no new download, the pairing kept, no page to the headphones.
    std::thread::sleep(Duration::from_millis(300));
    let state = f.bluetooth.state();
    assert_eq!((state.link.clone(), state.paired.len(), state.notice.as_str()), (Link::Idle, 1, ""));
    assert_eq!(state.controller, before.controller, "the version reported again is the patched controller's");
    assert_eq!(*f.route.lock().unwrap(), [true, false], "no route change");
    let line = f.until_status("status", |l| l.contains("reinits=1"));
    assert!(line.starts_with("phase=ready link=idle stream=none ") && line.contains("wifi_checks=1 alive=1 lost=0"), "{line}");
    assert!(!line.contains("Fixture") && !line.contains(':'), "no names or addresses: {line}");
    // Headphones connect again as before.
    f.bluetooth.connect(state.paired[0].addr);
    f.until("stream", |s| s.stream.is_some());
    // Off: the file says so.
    f.bluetooth.shutdown();
    assert!(f.status_line().starts_with("phase=off "), "{}", f.status_line());
    // While Bluetooth is off a power-up is not queued for the next start.
    f.bluetooth.wifi_powered_up();
    f.bluetooth.set_enabled(true);
    let state = f.until("ready", |s| s.phase == Phase::Ready);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(f.bluetooth.state().resync, Resync::default(), "{:?}", state.resync);
    f.bluetooth.shutdown();
}

#[test]
fn connected_headphones_keep_playing_through_a_wifi_power_up() {
    let f = fixture("resync-playing");
    f.pair();
    let blocking = Blocking::dedicated_thread();
    let writer = pcm::Writer::open(&f.endpoint, 44100, 2, 400_000).unwrap();
    f.plays(&blocking, &writer, 3);
    f.until("playing", |s| matches!(s.link, Link::Playing(_)));
    f.bluetooth.wifi_powered_up();
    f.plays(&blocking, &writer, 5);
    let state = f.until("checked", |s| s.resync.alive == 1);
    assert_eq!(state.resync, Resync { checks: 1, alive: 1, lost: 0, reinits: 0, busy: 1 }, "the link is never reset under the headphones");
    assert!(matches!(state.link, Link::Playing(_)) && state.stream.is_some());
    writer.drain(&blocking);
    assert_eq!((f.bluetooth.state().counters.0, f.bluetooth.state().notice.clone()), (0, String::new()), "no packets dropped, nothing to report");
    assert_eq!(*f.route.lock().unwrap(), [true], "the route stayed on the headphones");
    drop(writer);
    f.bluetooth.shutdown();
}

#[test]
fn a_controller_that_lost_power_with_the_wifi_side_is_brought_up_again() {
    let f = fixture("resync-lost");
    f.bluetooth.set_enabled(true);
    f.until("ready", |s| s.phase == Phase::Ready);
    // BT_DIS_N dropped with the Wi-Fi slot: the controller is in its ROM at 115200 and
    // the host's link at 1.5 Mbaud hears nothing. Without the check nothing would notice.
    f.power_cycle.store(true, Ordering::Release);
    std::thread::sleep(Duration::from_millis(50));
    f.bluetooth.wifi_powered_up();
    let state = f.until("brought up again", |s| s.resync.lost == 1 && s.phase == Phase::Ready);
    assert_eq!((state.resync.checks, state.resync.alive, state.resync.reinits), (1, 0, 0));
    assert!(state.controller.is_some_and(|v| v.rom_version == Some(2) && !v.already_patched), "a new download: {:?}", state.controller);
    f.until_status("status", |l| l.starts_with("phase=ready ") && l.contains(" lost=1 "));
    // The new bring-up works: a scan finds the fixture devices.
    f.bluetooth.scan();
    f.until("scan", |s| !s.scanning && s.found.len() == 3);
    f.bluetooth.shutdown();
}

#[test]
fn a_computer_that_takes_audio_is_checked_paired_and_played_to() {
    let f = fixture_with("computer", "aac");
    f.bluetooth.set_enabled(true);
    f.until("ready", |s| s.phase == Phase::Ready);
    f.bluetooth.scan();
    let found = f.until("scan", |s| !s.scanning && s.found.len() == 3 && s.found.iter().all(|d| d.name != "Unnamed Device"));
    let computer = found.found.iter().find(|d| d.class == 0x7c_0104).unwrap().clone();
    assert!(!computer.is_audio() && !computer.takes_audio(), "only its SDP records show the sink");
    f.bluetooth.connect_checked(computer.addr);
    let state = f.until("stream", |s| s.stream.is_some());
    assert_eq!((state.stream.unwrap().codec.kind(), state.summary(), state.notice.as_str()), (Kind::Aac, "Fixture Computer".into(), ""));
    assert_eq!(state.paired.iter().map(|d| d.class).collect::<Vec<_>>(), [0x7c_0104]);
    assert_eq!(state.diagnostics.sink_checks, (1, 1, 0));
    let blocking = Blocking::dedicated_thread();
    let writer = pcm::Writer::open(&f.endpoint, 44100, 2, 400_000).unwrap();
    let block = coder::tone(4410);
    for _ in 0..5 {
        let mut written = 0;
        while written < block.len() {
            let n = writer.write_s16(&blocking, &block[written..]);
            if n > 0 { written += n as usize * 2; } else { assert_eq!(writer.recover(n as i32), 0, "write failed with {n}"); }
        }
    }
    f.until("playing", |s| matches!(s.link, Link::Playing(_)));
    writer.drain(&blocking);
    assert_eq!(f.bluetooth.state().counters.0, 0);
    drop(writer);
    f.bluetooth.shutdown();
}

#[test]
fn each_codec_is_negotiated_encoded_and_paced_through_the_thread() {
    for (setting, kind) in [("auto", Kind::Ldac), ("ldac", Kind::Ldac), ("aptx-hd", Kind::AptXHd), ("aptx", Kind::AptX), ("aac", Kind::Aac), ("sbc", Kind::Sbc)] {
        let f = fixture_with(&format!("codec-{setting}"), setting);
        let state = f.pair();
        assert_eq!(state.stream.unwrap().codec.kind(), kind, "{setting}");
        let blocking = Blocking::dedicated_thread();
        let writer = pcm::Writer::open(&f.endpoint, 44100, 2, 400_000).unwrap();
        let block = coder::tone(4410);
        let started = Instant::now();
        for _ in 0..8 {
            let mut written = 0;
            while written < block.len() {
                let n = writer.write_s16(&blocking, &block[written..]);
                if n > 0 { written += n as usize * 2; } else { assert_eq!(writer.recover(n as i32), 0, "{setting}: write failed with {n}"); }
            }
        }
        f.until("playing", |s| matches!(s.link, Link::Playing(_)));
        writer.drain(&blocking);
        assert!(started.elapsed() >= Duration::from_millis(550), "{setting}: {:?}", started.elapsed());
        let state = f.bluetooth.state();
        assert_eq!((state.counters.0, state.notice.as_str()), (0, ""), "{setting}");
        drop(writer);
        f.bluetooth.shutdown();
    }
    assert_eq!(codecs("auto"), Kind::AUTOMATIC);
    assert_eq!(codecs("aac"), [Kind::Aac, Kind::Sbc]);
    assert_eq!(codecs("nonsense"), Kind::AUTOMATIC);
}

#[test]
fn an_absent_controller_answers_at_once_without_a_thread() {
    let dir = tempdir::Dir::new("absent");
    let bluetooth = Bluetooth::with_endpoint(Options::absent(&dir.0), Arc::new(|| {}), Arc::new(pcm::Endpoint::new()));
    bluetooth.set_enabled(true);
    assert_eq!(bluetooth.state().phase, Phase::Failed("Bluetooth is not available here".into()));
    assert!(bluetooth.worker.lock().unwrap().is_none());
    bluetooth.scan();
    bluetooth.set_enabled(false);
    assert_eq!(bluetooth.state().phase, Phase::Off);
}

#[test]
fn a_missing_port_fails_with_a_message_and_can_be_switched_off() {
    let dir = tempdir::Dir::new("missing");
    let mut options = Options::simulated(&dir.0);
    options.port = Port::Tty("/dev/q2-no-such-tty".into());
    let bluetooth = Bluetooth::with_endpoint(options, Arc::new(|| {}), Arc::new(pcm::Endpoint::new()));
    bluetooth.set_enabled(true);
    let limit = Instant::now() + Duration::from_secs(5);
    while !matches!(bluetooth.state().phase, Phase::Failed(_)) { assert!(Instant::now() < limit); std::thread::sleep(Duration::from_millis(5)); }
    assert_eq!(bluetooth.state().summary(), "Unavailable");
    bluetooth.scan();
    bluetooth.set_enabled(false);
    assert_eq!(bluetooth.state().phase, Phase::Off);
}

#[test]
fn auto_mode_tries_3_mbaud_with_flow_control_then_1_5_mbaud_with_and_without_and_manual_mode_its_setting_then_1_5_mbaud() {
    // Device result 2026-11-22 (kernel pm-q2-perf-v134 with patch 0192): 3 Mbaud with RTS/CTS
    // comes up; 1.5 Mbaud with and without RTS/CTS too.
    let options = Options::mainline(&crate::config::Bluetooth::default());
    assert!(options.uart_auto && options.uart_fallback);
    assert_eq!(uart_settings(&options), [(3_000_000, true), (1_500_000, true), (1_500_000, false)]);
    let manual = Options::mainline(&crate::config::Bluetooth { uart_mode: "manual".into(), baud: 2_000_000, flow_control: true, ..Default::default() });
    assert_eq!(uart_settings(&manual), [(2_000_000, true), (1_500_000, false)]);
    let plain = Options::mainline(&crate::config::Bluetooth { uart_mode: "manual".into(), ..Default::default() });
    assert_eq!(uart_settings(&plain), [(1_500_000, false)]);
    // A probe that names its setting tries only that.
    assert_eq!(uart_settings(&Options { uart_auto: false, uart_fallback: false, baud: 3_000_000, flow: true, ..options }), [(3_000_000, true)]);
    // The Realtek rate codes: the stock configuration's 3 Mbaud, and 1.5 Mbaud.
    assert_eq!((q2_bt::rtl::baud_code(FAST_BAUD), q2_bt::rtl::baud_code(SAFE_BAUD)), (Some(0x0492_8001), Some(0x0492_8002)));
}

/// A fixture in `uart_mode = "auto"` whose fake line carries garbage at the settings `broken` names.
fn uart_fixture(name: &str, broken: Option<fn(u32, bool) -> bool>) -> (Bluetooth, tempdir::Dir) { uart_fixture_with(name, |options| options.fake_uart_broken = broken) }
fn uart_fixture_with(name: &str, change: impl FnOnce(&mut Options)) -> (Bluetooth, tempdir::Dir) {
    let dir = tempdir::Dir::new(name);
    let mut options = Options::simulated(&dir.0);
    options.uart_auto = true;
    change(&mut options);
    (Bluetooth::with_endpoint(options, Arc::new(|| {}), Arc::new(pcm::Endpoint::new())), dir)
}
fn ready(bluetooth: &Bluetooth) -> State {
    let limit = Instant::now() + Duration::from_secs(45);
    loop {
        let state = bluetooth.state();
        if state.phase == Phase::Ready { return state }
        assert!(Instant::now() < limit && !matches!(state.phase, Phase::Failed(_)), "{:?} {:?}", state.phase, state.last_error);
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn failed(bluetooth: &Bluetooth) -> State {
    let limit = Instant::now() + Duration::from_secs(60);
    loop {
        let state = bluetooth.state();
        if matches!(state.phase, Phase::Failed(_)) { return state }
        assert!(Instant::now() < limit, "{:?}", state.phase);
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn fallbacks(bluetooth: &Bluetooth) -> u32 { bluetooth.shared.uart_fallbacks.load(Ordering::Relaxed) }
/// Off and on again: the next start of the same player.
fn restart(bluetooth: &Bluetooth) -> State { bluetooth.set_enabled(false); bluetooth.set_enabled(true); ready(bluetooth) }

#[test]
fn auto_mode_comes_up_at_3_mbaud_with_flow_control_and_no_fallback() {
    let (bluetooth, _dir) = uart_fixture("uart-auto", None);
    bluetooth.set_enabled(true);
    let state = ready(&bluetooth);
    assert_eq!((state.uart, state.last_error.as_str(), fallbacks(&bluetooth)), ((3_000_000, true), "", 0));
}

#[test]
fn a_kernel_without_3_mbaud_goes_to_1_5_mbaud_with_flow_control_without_an_error() {
    let (bluetooth, _dir) = uart_fixture_with("uart-no-3m", |options| options.fake_uart_accepts = Some(|baud| baud <= 1_500_000));
    bluetooth.set_enabled(true);
    let state = ready(&bluetooth);
    assert_eq!((state.uart, state.last_error.as_str(), fallbacks(&bluetooth)), ((1_500_000, true), "", 0));
    assert_eq!((restart(&bluetooth).uart, fallbacks(&bluetooth)), ((1_500_000, true), 0));
}

#[test]
fn a_failed_bring_up_at_3_mbaud_falls_back_to_1_5_mbaud_with_flow_control_and_later_starts_use_it() {
    // The line does not run at 3 Mbaud: the controller takes the rate, then hears nothing.
    let (bluetooth, _dir) = uart_fixture("uart-fallback", Some(|baud, _| baud == 3_000_000));
    bluetooth.set_enabled(true);
    let state = ready(&bluetooth);
    assert_eq!((state.uart, fallbacks(&bluetooth)), ((1_500_000, true), 1));
    assert!(!state.last_error.is_empty(), "the failed attempt stays for the diagnostics page");
    // The next start uses the setting that worked at once.
    assert_eq!((restart(&bluetooth).uart, fallbacks(&bluetooth)), ((1_500_000, true), 1));
}

#[test]
fn when_3_mbaud_and_1_5_mbaud_with_flow_control_fail_1_5_mbaud_without_it_comes_up() {
    // RTS/CTS does not work on this line, at either rate.
    let (bluetooth, _dir) = uart_fixture("uart-fallback-twice", Some(|_, flow| flow));
    bluetooth.set_enabled(true);
    let state = ready(&bluetooth);
    assert_eq!((state.uart, fallbacks(&bluetooth)), ((1_500_000, false), 2));
    assert_eq!((restart(&bluetooth).uart, fallbacks(&bluetooth)), ((1_500_000, false), 2));
}

#[test]
fn a_controller_the_wi_fi_side_keeps_powered_is_moved_from_the_rate_it_took() {
    // The bring-up at 3 Mbaud fails after the patch at a start-up command (HCI Reset refused at
    // 3 Mbaud), and dropping the hold does not reset the controller: it stays patched at
    // 3 Mbaud. The next bring-up finds it there and moves it to 1.5 Mbaud.
    let (bluetooth, _dir) = uart_fixture_with("uart-kept", |options| {
        options.fake_kept = Some(KeptFake::default());
        options.fake_setup = Some(|fake| fake.refuse_at = Some((3_000_000, 0x0c03)));
    });
    bluetooth.set_enabled(true);
    let state = ready(&bluetooth);
    assert_eq!((state.uart, fallbacks(&bluetooth)), ((1_500_000, true), 1));
    assert!(state.last_error.contains("0x0c03"), "{}", state.last_error);
    assert!(state.controller.is_some_and(|v| v.already_patched), "no second download");
}

#[test]
fn when_every_setting_fails_bluetooth_fails_and_a_probe_naming_a_rate_does_not_fall_back() {
    // Nothing above 115200 works: each fallback is tried once, then the start fails.
    let (bluetooth, _dir) = uart_fixture("uart-fallback-fails", Some(|baud, _| baud != q2_bt::host::INITIAL_BAUD));
    bluetooth.set_enabled(true);
    assert_eq!((failed(&bluetooth).uart, fallbacks(&bluetooth)), ((1_500_000, false), 2));
    assert_eq!(bluetooth.shared.uart_step.load(Ordering::Acquire), 0, "no setting reached Ready: Try Again starts at 3 Mbaud");
    // `--bt-probe --baud 3000000 --flow` tests exactly that setting.
    let dir = tempdir::Dir::new("uart-probe");
    let mut options = Options::simulated(&dir.0);
    (options.baud, options.flow, options.uart_fallback) = (3_000_000, true, false);
    options.fake_uart_broken = Some(|baud, _| baud == 3_000_000);
    let probe = Bluetooth::with_endpoint(options, Arc::new(|| {}), Arc::new(pcm::Endpoint::new()));
    probe.set_enabled(true);
    assert_eq!((failed(&probe).uart, fallbacks(&probe)), ((3_000_000, true), 0));
}

/// The fixture headphones' address (`fake_loop`, `q2_bt::fake::Headphones::new(0x11, …)`).
const FIXTURE_HEADPHONES: Addr = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];

/// A controller that takes about a second to start: each bring-up reply comes 40 ms late.
fn slow_start(fake: &mut q2_bt::fake::Fake) { fake.bring_up_delay = 40; }

#[test]
fn scan_and_pair_asked_while_the_controller_starts_run_once_it_is_ready() {
    // A scan or connection asked during bring-up waits for the host. Sent to a host that was not
    // ready, it was dropped and left the screen searching until Bluetooth was toggled.
    let f = fixture_setup("slow-start", "sbc", Some(slow_start));
    f.bluetooth.set_enabled(true);
    f.bluetooth.scan();
    f.bluetooth.connect(FIXTURE_HEADPHONES);
    assert_eq!(f.bluetooth.state().phase, Phase::Starting);
    std::thread::sleep(Duration::from_millis(300));
    let starting = f.bluetooth.state();
    assert_eq!(starting.phase, Phase::Starting, "the slow bring-up is still running");
    assert!(!starting.scanning && starting.link == Link::Idle, "nothing has started: the requests wait");
    let state = f.until("paired", |s| s.stream.is_some());
    assert_eq!((state.paired.len(), state.notice.as_str(), state.scanning, state.step), (1, "", false, None));
    assert_eq!(state.attempt, None, "a connected attempt leaves nothing to try again");
    assert!(state.seen(FIXTURE_HEADPHONES).is_some());
}

/// Pages to headphones that are off last as long as on a real controller.
fn real_page(fake: &mut q2_bt::fake::Fake) { fake.page_ms = 5120; }

#[test]
fn a_scan_during_the_reconnect_at_start_ends_that_page_and_finds_devices() {
    let f = fixture_setup("reconnect-race", "sbc", Some(real_page));
    f.pair();
    f.bluetooth.set_enabled(false);
    // The paired headphones are off: the reconnect at Ready pages them for 5.12 s.
    f.off.store(true, Ordering::Release);
    f.bluetooth.set_enabled(true);
    f.until("reconnecting", |s| matches!(s.link, Link::Connecting(_)));
    let asked = Instant::now();
    f.bluetooth.scan();
    let state = f.until("scan", |s| s.found.len() >= 2 && !s.scanning);
    assert!(asked.elapsed() < Duration::from_secs(4), "the scan waited for the page to time out: {:?}", asked.elapsed());
    assert!(state.notice.is_empty(), "a reconnect the listener cut short is no problem");
    // The scan over, the reconnect is not resumed.
    f.stays("no reconnect after the scan", Duration::from_millis(1500), |s| s.link == Link::Idle);
}

#[test]
fn a_failed_attempt_leaves_a_plain_notice_and_try_again_pairs_without_toggling() {
    let f = fixture("retry-connect");
    f.off.store(true, Ordering::Release);
    f.bluetooth.set_enabled(true);
    f.until("ready", |s| s.phase == Phase::Ready);
    f.bluetooth.connect(FIXTURE_HEADPHONES);
    let failed = f.until("failed", |s| !s.notice.is_empty());
    assert_eq!(failed.notice, "Headphones not found. Turn them on, or put them in pairing mode.");
    assert_eq!((failed.attempt, failed.link, failed.last_error.as_str()), (Some((FIXTURE_HEADPHONES, false)), Link::Idle, failed.notice.as_str()));
    // The headphones are switched on; the same attempt is asked again at once.
    f.off.store(false, Ordering::Release);
    let (addr, _) = failed.attempt.unwrap();
    f.bluetooth.connect(addr);
    let state = f.until("paired", |s| s.stream.is_some());
    assert_eq!((state.notice.as_str(), state.attempt), ("", None));
    assert!(!state.last_error.is_empty(), "the last error stays for the diagnostics page");
}

static FIRST_START: AtomicBool = AtomicBool::new(true);
/// The first controller reports a ROM version the firmware has no patch for; later ones start.
fn fails_first(fake: &mut q2_bt::fake::Fake) { if FIRST_START.swap(false, Ordering::AcqRel) { fake.rom_version = 9; } }

#[test]
fn a_failed_start_is_tried_again_without_switching_off() {
    let f = fixture_setup("retry-start", "sbc", Some(fails_first));
    f.bluetooth.set_enabled(true);
    let failed = f.until("failed", |s| matches!(s.phase, Phase::Failed(_)));
    assert!(failed.last_error.starts_with("Bluetooth firmware"), "{:?}", failed.last_error);
    f.bluetooth.retry();
    f.until("ready", |s| s.phase == Phase::Ready);
    // Retry does nothing to a controller that is not failed.
    f.bluetooth.retry();
    assert_eq!(f.bluetooth.state().phase, Phase::Ready);
}

#[test]
fn discovery_keeps_searching_until_a_late_device_appears_and_stops_when_asked() {
    let f = fixture("discovery");
    // The headphones are not in range yet; the earbuds and the computer are.
    f.off.store(true, Ordering::Release);
    f.bluetooth.set_enabled(true);
    // Asked during bring-up: the search starts when the controller is ready.
    f.bluetooth.discover();
    let first = f.until("first round", |s| s.discovering && s.found.len() == 2 && s.found.iter().all(|d| d.name != "Unnamed Device"));
    assert!(first.found.iter().all(|d| d.addr != FIXTURE_HEADPHONES));
    let rounds = f.bluetooth.state().diagnostics.inquiry_completes;
    f.off.store(false, Ordering::Release);
    let state = f.until("late device", |s| s.found.iter().any(|d| d.addr == FIXTURE_HEADPHONES));
    assert!(state.diagnostics.inquiry_completes > rounds, "a later inquiry found it");
    assert_eq!(state.found.iter().find(|d| d.addr == FIXTURE_HEADPHONES).map(|d| (d.kind(), d.signal())), Some(("Headphones", Some("Strong"))));
    f.bluetooth.stop_discovery();
    let stopped = f.until("stopped", |s| !s.discovering && !s.scanning);
    assert_eq!(stopped.found.len(), 3, "the devices found stay listed");
    let completes = stopped.diagnostics.inquiry_completes;
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(f.bluetooth.state().diagnostics.inquiry_completes, completes, "no round after the search stopped");
    assert!(!f.bluetooth.state().scanning);
}

#[test]
fn codec_details_name_the_rate_and_bit_rate() {
    use q2_bt::codec::Codec;
    let stream = |codec| Stream { codec, mtu: 895 };
    let sbc = q2_bt::sbc::Params { rate: 44100, blocks: 16, joint: true, snr: false, bitpool: 53 };
    assert_eq!(describe(&stream(Codec::Sbc(sbc)), coder::LdacQuality::Standard), "SBC · 328 kbit/s");
    assert_eq!(describe(&stream(Codec::Aac { rate: 44100, bitrate: 192_000 }), coder::LdacQuality::Standard), "AAC · 192 kbit/s");
    assert_eq!(describe(&stream(Codec::AptX { rate: 48000 }), coder::LdacQuality::Standard), "aptX · 384 kbit/s");
    assert_eq!(describe(&stream(Codec::AptXHd { rate: 44100 }), coder::LdacQuality::Standard), "aptX HD · 529 kbit/s");
    assert_eq!(describe(&stream(Codec::Ldac { rate: 48000 }), coder::LdacQuality::High), "LDAC · 990 kbit/s (High)");
    assert_eq!(describe(&stream(Codec::Ldac { rate: 44100 }), coder::LdacQuality::Mobile), "LDAC · 303 kbit/s (Mobile)");
    assert_eq!([Codec::Ldac { rate: 44100 }, Codec::AptX { rate: 48000 }].map(|codec| sample_rate(&stream(codec))), ["44.1 kHz", "48 kHz"]);
}

#[test]
fn connecting_the_headphones_the_start_up_reconnect_is_paging_takes_over_that_page() {
    let f = fixture_setup("adopt-page", "sbc", Some(real_page));
    f.pair();
    f.bluetooth.set_enabled(false);
    f.off.store(true, Ordering::Release);
    f.bluetooth.set_enabled(true);
    f.until("reconnecting", |s| matches!(s.link, Link::Connecting(_)));
    let paging = Instant::now();
    std::thread::sleep(Duration::from_millis(2000));
    f.bluetooth.connect(FIXTURE_HEADPHONES);
    let failed = f.until("failed", |s| !s.notice.is_empty());
    assert!(paging.elapsed() < Duration::from_millis(5120 + 1500), "a second page was started: {:?}", paging.elapsed());
    assert_eq!((failed.notice.as_str(), failed.attempt), ("Headphones not found. Turn them on, or put them in pairing mode.", Some((FIXTURE_HEADPHONES, false))));
}

#[test]
fn pair_new_device_during_the_reconnect_at_start_searches_at_once_and_the_reconnect_is_not_resumed() {
    // Pair New Device searches at once while a connection attempt is in progress, without
    // waiting for the page to time out. The attempt is cancelled and not resumed: a listener
    // pairing a new device does not want to connect to the other one.
    let f = fixture_setup("discover-during-reconnect", "sbc", Some(real_page));
    f.pair();
    f.bluetooth.set_enabled(false);
    f.off.store(true, Ordering::Release);
    f.bluetooth.set_enabled(true);
    f.until("reconnecting", |s| matches!(s.link, Link::Connecting(_)));
    let asked = Instant::now();
    f.bluetooth.discover();
    assert!(f.bluetooth.state().discovering, "Searching… shows at once");
    let found = f.until("found", |s| s.found.len() >= 2 && s.found.iter().all(|d| d.name != "Unnamed Device"));
    assert!(asked.elapsed() < Duration::from_secs(4), "the search waited for the page to time out: {:?}", asked.elapsed());
    assert!(found.notice.is_empty());
    // Left without pairing: the old reconnect does not come back.
    f.bluetooth.stop_discovery();
    f.until("stopped", |s| !s.discovering && !s.scanning);
    f.stays("no reconnect after the search", Duration::from_millis(1500), |s| s.link == Link::Idle && s.notice.is_empty());
    // The next start reconnects the last-used device as usual.
    f.bluetooth.set_enabled(false);
    f.bluetooth.set_enabled(true);
    f.until("reconnecting at the next start", |s| s.link == Link::Connecting(FIXTURE_HEADPHONES));
}

const FIXTURE_EARBUDS: Addr = [0x12, 0x22, 0x33, 0x44, 0x55, 0x66];

#[test]
fn pairing_a_new_device_during_the_reconnect_at_start_makes_it_the_active_one() {
    let f = fixture_setup("pair-during-reconnect", "sbc", Some(real_page));
    f.pair();
    f.bluetooth.set_enabled(false);
    f.off.store(true, Ordering::Release);
    f.bluetooth.set_enabled(true);
    f.until("reconnecting", |s| s.link == Link::Connecting(FIXTURE_HEADPHONES));
    f.bluetooth.discover();
    f.until("earbuds found", |s| s.found.iter().any(|d| d.addr == FIXTURE_EARBUDS && d.name != "Unnamed Device"));
    f.bluetooth.connect(FIXTURE_EARBUDS);
    let paired = f.until("earbuds paired", |s| s.stream.is_some());
    assert_eq!((paired.connected(), paired.notice.as_str(), paired.paired.len()), (Some(FIXTURE_EARBUDS), "", 2));
    f.bluetooth.stop_discovery();
    f.stays("the earbuds stay connected", Duration::from_millis(1500), |s| s.connected() == Some(FIXTURE_EARBUDS));
    // The next start reconnects the earbuds, not the headphones.
    f.bluetooth.set_enabled(false);
    f.bluetooth.set_enabled(true);
    let state = f.until("earbuds again", |s| s.connected().is_some() || matches!(s.link, Link::Connecting(_)));
    assert!(matches!(state.link, Link::Connecting(a) | Link::Connected(a) | Link::Playing(a) if a == FIXTURE_EARBUDS), "{:?}", state.link);
}

#[test]
fn pair_new_device_ends_a_connection_the_listener_asked_for_and_drops_one_still_waiting() {
    // A connection asked for during the page to headphones that are off: Pair New Device ends it
    // without a problem and does not come back to it.
    let f = fixture_setup("discover-ends-attempt", "sbc", Some(real_page));
    f.off.store(true, Ordering::Release);
    f.bluetooth.set_enabled(true);
    f.until("ready", |s| s.phase == Phase::Ready);
    f.bluetooth.connect(FIXTURE_HEADPHONES);
    f.until("connecting", |s| s.link == Link::Connecting(FIXTURE_HEADPHONES));
    let asked = Instant::now();
    f.bluetooth.discover();
    f.until("found", |s| s.found.len() >= 2 && s.found.iter().all(|d| d.name != "Unnamed Device"));
    assert!(asked.elapsed() < Duration::from_secs(4), "the search waited for the page to time out: {:?}", asked.elapsed());
    f.bluetooth.stop_discovery();
    f.stays("no attempt, no problem", Duration::from_millis(1500), |s| s.link == Link::Idle && s.notice.is_empty() && s.attempt.is_none());
    f.bluetooth.set_enabled(false);
    // A connection asked for while the controller starts, then Pair New Device: the connection
    // never starts, and the headphones (on now) are not connected.
    let f = fixture_setup("discover-drops-waiting", "sbc", Some(slow_start));
    f.bluetooth.set_enabled(true);
    f.bluetooth.connect(FIXTURE_HEADPHONES);
    f.bluetooth.discover();
    f.until("found", |s| s.phase == Phase::Ready && s.found.len() == 3);
    f.bluetooth.stop_discovery();
    f.stays("nothing connected", Duration::from_millis(1000), |s| s.link == Link::Idle && s.waiting == 0);
}

#[test]
fn only_connections_asked_for_before_the_last_search_are_dropped() {
    let addr = |last| [last, 0, 0, 0, 0, 0];
    let mut pending: VecDeque<(Request, Option<Instant>)> = [Request::Connect(addr(1)), Request::Scan, Request::ConnectSink(addr(2)),
        Request::Discover(Some(Instant::now())), Request::Disconnect, Request::Connect(addr(3)), Request::Discover(None)]
        .into_iter().map(|r| (r, None)).collect();
    drop_connects_before_search(&mut pending);
    let kinds: Vec<_> = pending.iter().map(|(r, _)| match r {
        Request::Scan => "scan".to_string(), Request::Connect(a) => format!("connect {}", a[0]), Request::ConnectSink(a) => format!("sink {}", a[0]),
        Request::Discover(Some(_)) => "search".into(), Request::Discover(None) => "stop".into(), Request::Disconnect => "disconnect".into(), _ => "other".into() }).collect();
    assert_eq!(kinds, ["scan", "search", "disconnect", "connect 3", "stop"]);
    // Without a search nothing is dropped.
    let mut plain: VecDeque<(Request, Option<Instant>)> = [Request::Connect(addr(1)), Request::Discover(None)].into_iter().map(|r| (r, None)).collect();
    drop_connects_before_search(&mut plain);
    assert_eq!(plain.len(), 2);
}

#[test]
fn the_codec_and_why_it_was_chosen_are_published() {
    let f = fixture_with("codec-reason", "auto");
    let state = f.pair();
    assert_eq!((state.stream.map(|s| s.codec.kind()), state.codec_reason.as_str()), (Some(Kind::Ldac), "The best both sides have"));
    let f = fixture_with("codec-reason-setting", "aac");
    let state = f.pair();
    assert_eq!((state.stream.map(|s| s.codec.kind()), state.codec_reason.as_str()), (Some(Kind::Aac), "Chosen in Settings"));
    assert_eq!(codec_reason(None, Kind::Sbc, &codecs("ldac")), "The headphones lack LDAC");
    assert_eq!(codec_reason(Some(q2_bt::host::Origin::Upgraded { from: Kind::Sbc }), Kind::Aac, &codecs("auto")), "Changed from SBC, which the headphones chose");
    assert_eq!(codec_reason(Some(q2_bt::host::Origin::Theirs { checked: true }), Kind::Sbc, &codecs("auto")), "Chosen by the headphones; nothing better in common");
}
