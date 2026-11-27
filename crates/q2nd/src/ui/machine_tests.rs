//! Generated world events through the real App (headless platform, the real
//! library, player and simulated Wi-Fi) against the proven doze policy and
//! power machine (`q2_machines`, docs/state-machines.md).
use super::*;
use crate::audio::{alsa::SoftHw, player};
use crate::platform::Headless;
use proptest::prelude::*;
use proptest::test_runner::{Config as PropConfig, RngAlgorithm, TestRng, TestRunner};
use q2_machines::doze as policy;
use subsonic::Song;

const SONG_SECONDS: u32 = 100;
const CACHED: usize = 6;
const TOTAL: usize = 8;
/// `[power] doze_ahead_minutes` / `doze_wake_minutes` for these tests: the
/// six cached songs (600 s) span both thresholds.
const DOZE_MINUTES: u32 = 6;
const WAKE_MINUTES: u32 = 2;

struct World {
    app: App,
    _player: PlayerHandle,
    inbox: crate::ui::UiRx,
    root: std::path::PathBuf,
    wifi_on: bool,
    offline: bool,
}

fn world() -> World {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(format!("../../build/perf/app-machines-{}-{}", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
    std::fs::create_dir_all(&root).unwrap();
    let paths = Paths::new(root.clone());
    paths.create_all(&crate::rt::Blocking::dedicated_thread()).unwrap();
    let mut config = Config::default();
    config.power.radio_doze = true;
    config.power.sleep_minutes = 0;
    config.power.doze_ahead_minutes = DOZE_MINUTES;
    config.power.doze_wake_minutes = WAKE_MINUTES;
    // The paused doze has its own app test (doze_tests.rs); here the
    // generated screen changes carry no dark time.
    config.power.doze_paused_minutes = 0;
    config.storage.fetch_minutes = 0;
    // The generated idle times must not also switch the backlight.
    config.display.backlight_seconds = 0;
    let lib = Library::open(paths.clone(), &config, Arc::new(|_| {}));
    let player = player::spawn(Arc::new(crate::library::Tracks(lib.clone())), Box::new(SoftHw("null".into())),
        player::Settings { volume: config.playback.volume, headphone_power: crate::config::HeadphonePower::Standard, filter: 0,
            replay_gain: config.playback.replay_gain, gapless: true, eq: config.playback.eq.clone() }, Arc::new(|| {}));
    let net = Net::new(root.join("unused-wifi.sh"), true, Arc::new(|| {}));
    net.simulate_address(Some("192.0.2.10"));
    let tailscale = crate::tailscale::Tailscale::new(root.clone(), root.clone(), true, false);
    let (tx, inbox) = crate::ui::channel();
    let mut app = App::new(lib, player.clone(), config, paths, net, tailscale, Fonts::bundled(), tx);
    let songs: Vec<Song> = (0..TOTAL).map(|n| {
        let song = Song { id: format!("app-{n}"), suffix: Some("mp3".into()), bit_rate: Some(128), duration: SONG_SECONDS, ..Default::default() };
        if n < CACHED {
            let path = app.lib.paths.stream_cache().join(format!("app-{n}.raw.mp3"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"generated cached audio").unwrap();
            app.lib.db().put_cached_song(&crate::rt::Blocking::dedicated_thread(), &song.id, "raw", &path, 22).unwrap();
        }
        song
    }).collect();
    app.status.queue = Arc::new(songs);
    // Each world starts ready to doze: playing in the dark, long untouched.
    app.status.state = PlayState::Playing;
    app.screen_on = false;
    app.last_input = Instant::now() - 2 * doze::SETTLE;
    World { app, _player: player, inbox, root, wifi_on: true, offline: false }
}

#[derive(Debug, Clone)]
enum Happening {
    Screen(bool),
    /// The owner last touched the device long ago, or just now.
    Idle(bool),
    State(u8),
    /// Playback is at `seconds` into song `index`.
    Position { index: usize, seconds: u32 },
    /// The link gained or lost its address.
    Link(bool),
    Wifi(bool),
    Offline(bool),
    /// A whole sleep transaction: prepare, enter, wake.
    Sleep,
    Tick,
}

fn happening() -> impl Strategy<Value = Happening> {
    // Weighted towards the conditions a doze needs, so generated worlds
    // doze and wake many times rather than almost never.
    prop_oneof![
        3 => prop::bool::weighted(0.25).prop_map(Happening::Screen),
        3 => prop::bool::weighted(0.8).prop_map(Happening::Idle),
        3 => prop_oneof![4 => Just(0u8), 1 => Just(1u8), 1 => Just(2u8)].prop_map(Happening::State),
        5 => (prop_oneof![3 => 0usize..4, 1 => 4usize..TOTAL], 0u32..SONG_SECONDS).prop_map(|(index, seconds)| Happening::Position { index, seconds }),
        2 => prop::bool::weighted(0.7).prop_map(Happening::Link),
        1 => prop::bool::weighted(0.7).prop_map(Happening::Wifi),
        1 => prop::bool::weighted(0.3).prop_map(Happening::Offline),
        1 => Just(Happening::Sleep),
        3 => Just(Happening::Tick),
    ]
}

/// The sleep transaction as the main loop runs it, checking the power
/// machine's state at each step.
fn sleep(w: &mut World, platform: &mut Headless) -> Result<(), TestCaseError> {
    use q2_machines::power::Power;
    let dozing = w.app.net.dozing();
    w.app.prepare_power(false);
    let saved_deadline = Instant::now() + Duration::from_secs(3);
    while w.app.settings_pending() {
        let message = crate::ui::recv_within(&mut w.inbox, saved_deadline.saturating_duration_since(Instant::now())).expect("settings barrier reports");
        w.app.handle(message, platform);
    }
    prop_assert_eq!(w.app.power, Power::Preparing);
    w.app.prepare_power(true);
    prop_assert_eq!(w.app.power, Power::Preparing, "a second request started during a transaction");
    // Timers are held while a transaction is pending.
    w.app.tick(platform);
    prop_assert!(w.app.take_requests().is_empty());
    // Other workers (the home screen's loads) report on the same channel.
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let message = crate::ui::recv_within(&mut w.inbox, deadline.saturating_duration_since(Instant::now())).expect("the preparation reports");
        let prepared = matches!(message, UiMsg::PowerPrepared { .. });
        if prepared { prop_assert!(matches!(message, UiMsg::PowerPrepared { off: false, result: Ok(()) }), "sleep preparation failed"); }
        w.app.handle(message, platform);
        if prepared { break; }
        prop_assert_eq!(w.app.power, Power::Preparing, "another message changed the transaction");
    }
    prop_assert_eq!(w.app.power, Power::Ready { off: false });
    prop_assert!(w.app.player.prepare_sleep().is_err(), "the player's barrier is not held");
    w.app.tick(platform);
    prop_assert_eq!(w.app.take_requests(), vec![Request::EnterPower { off: false }]);
    prop_assert_eq!(w.app.power, Power::Entering);
    w.app.tick(platform);
    prop_assert!(w.app.take_requests().is_empty(), "sleep was entered twice");
    prop_assert_eq!(w.app.net.dozing(), dozing, "the doze policy ran during a power transaction");
    // The platform's suspend returned.
    w.app.finish_power(platform, Ok(()), None);
    prop_assert_eq!(w.app.power, Power::Idle);
    prop_assert!(w.app.screen_on);
    prop_assert!(w.app.player.prepare_sleep().is_ok(), "the barrier was not released on wake");
    w.app.player.cancel_handoff();
    Ok(())
}

/// After every generated event and tick, the radio dozes exactly when the
/// proven policy says so for the world as the test set it, the library's
/// view agrees with the radio's, and a sleep transaction walks the power
/// machine's states and releases its barrier.
#[test]
fn the_app_follows_the_doze_policy_and_power_machine_under_generated_events() {
    let started = Instant::now();
    let (dozes, wakes) = (std::cell::Cell::new(0u32), std::cell::Cell::new(0u32));
    // Time spent in cases, without the waits for the shared receiver lock.
    let active = std::cell::Cell::new(Duration::ZERO);
    let config = PropConfig { cases: 24, failure_persistence: None, max_shrink_iters: 60, ..PropConfig::default() };
    let mut runner = TestRunner::new_with_rng(config, TestRng::deterministic_rng(RngAlgorithm::ChaCha));
    let thresholds = policy::Thresholds::from_minutes(DOZE_MINUTES, WAKE_MINUTES);
    let result = runner.run(&prop::collection::vec(happening(), 1..30), |happenings| {
        // UI fixtures run one at a time.
        let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|e| e.into_inner());
        let case_started = Instant::now();
        let mut w = world();
        let mut platform = Headless { last: None };
        for happening in happenings {
            match &happening {
                Happening::Screen(on) => {
                    w.app.screen_on = *on;
                    if *on { w.app.last_input = Instant::now(); }
                }
                Happening::Idle(long) => {
                    w.app.last_input = if *long { Instant::now() - 2 * doze::SETTLE } else { Instant::now() };
                }
                Happening::State(code) => w.app.status.state = [PlayState::Playing, PlayState::Paused, PlayState::Stopped][*code as usize],
                Happening::Position { index, seconds } => {
                    w.app.status.index = *index;
                    w.app.status.position_ms = u64::from(*seconds) * 1000;
                }
                Happening::Link(up) => w.app.net.simulate_address(up.then_some("192.0.2.10")),
                Happening::Wifi(on) => {
                    w.wifi_on = *on;
                    if *on { w.offline = false; w.app.lib.set_offline_mode(false); }
                    w.app.net.set_enabled(*on, &[]);
                }
                Happening::Offline(on) => {
                    w.offline = *on;
                    w.app.lib.set_offline_mode(*on);
                    w.app.net.set_offline_mode(*on, w.wifi_on, &[]);
                }
                Happening::Sleep => sleep(&mut w, &mut platform)?,
                Happening::Tick => {}
            }
            // Recompute the audio ahead on this tick, from this position.
            w.app.doze_checked = None;
            w.app.doze_seen = None;
            let wifi = w.app.net.state();
            let playing = w.app.status.state == PlayState::Playing;
            let inputs = policy::Inputs {
                enabled: true,
                wifi_wanted: w.wifi_on && !w.offline,
                connected: wifi.ssid.is_some() && wifi.ip.is_some(),
                dozing: w.app.net.dozing(),
                playing,
                stopped: w.app.status.state == PlayState::Stopped,
                screen_on: w.app.screen_on,
                airplay: false,
                idle_ms: Instant::now().saturating_duration_since(w.app.last_input).as_millis() as u64,
                ahead_ms: w.app.lib.local_ahead_ms(&w.app.status.queue, w.app.status.index, w.app.status.position_ms, doze::LOOKAHEAD_SONGS),
                fetching: w.app.lib.fetching(),
                // The generated app sets `doze_paused_minutes` 0.
                paused_doze: false,
            };
            prop_assert_eq!(w.app.net.wanted(), inputs.wifi_wanted, "the saved Wi-Fi choice after {:?}", happening);
            let (expected, effect) = policy::transition(inputs, thresholds);
            w.app.tick(&mut platform);
            prop_assert_eq!(w.app.net.dozing(), expected, "radio after {:?} with {:?}", happening, inputs);
            prop_assert_eq!(w.app.lib.radio_dozing(), w.app.net.dozing(), "library and radio disagree after {:?}", happening);
            if w.app.net.dozing() {
                prop_assert!(!w.app.lib.is_online() && w.app.net.wanted() && !w.app.screen_on);
                prop_assert!(inputs.ahead_ms >= thresholds.wake_ms, "dozing with {} ms ahead", inputs.ahead_ms);
            }
            if effect == policy::Effect::Doze {
                dozes.set(dozes.get() + 1);
                prop_assert!(!inputs.fetching && inputs.ahead_ms >= thresholds.doze_ms);
            }
            if matches!(effect, policy::Effect::Wake(_)) { wakes.set(wakes.get() + 1); }
            prop_assert_eq!(w.app.power, q2_machines::power::Power::Idle);
        }
        let root = w.root.clone();
        w.app.player.shutdown();
        drop(w);
        let _ = std::fs::remove_dir_all(root);
        active.set(active.get() + case_started.elapsed());
        Ok(())
    });
    if let Err(error) = result { panic!("{error}"); }
    // The generated worlds must reach both transitions, or the test is vacuous.
    assert!(dozes.get() >= 3 && wakes.get() >= 3, "{} dozes, {} wakes", dozes.get(), wakes.get());
    record_time("app-doze-power", started);
    record_time("app-doze-power-active", Instant::now() - active.get());
}

/// Writes a property test's wall time under build/perf for the report.
pub(crate) fn record_time(name: &str, started: Instant) {
    let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build/perf/property-test-times");
    let _ = std::fs::create_dir_all(&dir);
    let _ = std::fs::write(dir.join(name), format!("{:.2}\n", started.elapsed().as_secs_f64()));
}
