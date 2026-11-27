//! Generated Wi-Fi doze controls: fixture songs and cache files only.
use super::*;
use crate::audio::{alsa::SoftHw, player};
use crate::platform::Headless;
use subsonic::Song;

fn fixture(doze: bool) -> (App, PlayerHandle, std::path::PathBuf) { fixture_mode(doze, 0) }

/// `fetch_minutes` 0 keeps the fixed doze/wake thresholds these tests
/// exercise; non-zero selects dynamic doze (doze after a fetch).
fn fixture_mode(doze: bool, fetch_minutes: u32) -> (App, PlayerHandle, std::path::PathBuf) {
    let root = crate::library::generated_test_directory("doze");
    std::fs::create_dir_all(&root).unwrap();
    let paths = Paths::new(root.clone());
    paths.create_all(&crate::rt::Blocking::dedicated_thread()).unwrap();
    let mut config = Config::default();
    config.power.radio_doze = doze;
    config.power.sleep_minutes = 0;
    config.storage.fetch_minutes = fetch_minutes;
    let lib = Library::open(paths.clone(), &config, Arc::new(|_| {}));
    let player = player::spawn(Arc::new(crate::library::Tracks(lib.clone())), Box::new(SoftHw("null".into())),
        player::Settings { volume: config.playback.volume, headphone_power: crate::config::HeadphonePower::Standard, filter: 0,
            replay_gain: config.playback.replay_gain, gapless: true, eq: config.playback.eq.clone() }, Arc::new(|| {}));
    let net = Net::new(root.join("unused-wifi.sh"), true, Arc::new(|| {}));
    let tailscale = crate::tailscale::Tailscale::new(root.clone(), root.clone(), true, false);
    let (tx, _receive) = channel();
    let app = App::new(lib, player.clone(), config, paths, net, tailscale, Fonts::bundled(), tx);
    (app, player, root)
}

/// `cached` songs of `seconds` each are complete in the stream cache; the
/// rest exist only on the (absent) server.
fn queue_files(_blocking: &crate::rt::Blocking, lib: &Library, cached: usize, total: usize, seconds: u32)
    -> (Vec<Song>, Vec<(String, std::path::PathBuf)>) {
    let mut files = Vec::new();
    let songs = (0..total).map(|n| {
        let song = Song { id: format!("doze-{n}"), suffix: Some("mp3".into()), bit_rate: Some(128), duration: seconds, ..Default::default() };
        if n < cached {
            let path = lib.paths.stream_cache().join(format!("doze-{n}.raw.mp3"));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, b"generated cached audio").unwrap();
            files.push((song.id.clone(), path));
        }
        song
    }).collect();
    (songs, files)
}
fn queue(app: &App, cached: usize, total: usize, seconds: u32) -> Vec<Song> {
    let blocking = crate::rt::Blocking::dedicated_thread();
    let (songs, files) = queue_files(&blocking, &app.lib, cached, total, seconds);
    for (id, path) in files { app.lib.db().put_cached_song(&blocking, &id, "raw", &path, 22).unwrap(); }
    songs
}
async fn queue_async(lib: &Arc<Library>, cached: usize, total: usize, seconds: u32) -> Vec<Song> {
    let owner = lib.clone();
    let (songs, files) = crate::rt::jobs().run_isolated(move |blocking| queue_files(blocking, &owner, cached, total, seconds)).await.unwrap();
    lib.db_run(move |blocking, db| {
        for (id, path) in files { db.put_cached_song(blocking, &id, "raw", &path, 22).unwrap(); }
    }).await;
    songs
}

fn playing(app: &mut App, songs: Vec<Song>, position_ms: u64) {
    app.status.queue = Arc::new(songs);
    app.status.index = 0;
    app.status.position_ms = position_ms;
    app.status.state = PlayState::Playing;
    app.screen_on = false;
    app.last_input = Instant::now() - doze::SETTLE;
    app.doze_checked = None;
}

#[test]
fn local_audio_ahead_stops_at_the_first_song_needing_the_network() {
    let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|e| e.into_inner());
    let (app, _player, root) = fixture(true);
    let songs = queue(&app, 2, 4, 100);
    assert_eq!(app.lib.local_ahead_ms(&songs, 0, 30_000, 6), 170_000);
    assert_eq!(app.lib.local_ahead_ms(&songs, 1, 0, 6), 100_000);
    assert_eq!(app.lib.local_ahead_ms(&songs, 2, 0, 6), 0, "uncached current song");
    assert_eq!(app.lib.local_ahead_ms(&songs, 0, 0, 0), 100_000, "lookahead bound");
    let mut live = songs.clone();
    live[1].radio_url = Some("http://192.0.2.1/stream".into());
    assert_eq!(app.lib.local_ahead_ms(&live, 0, 0, 6), 100_000, "live station is never local");
    assert!(crate::rt::Ui::new().unwrap().block_on(app.settle_telemetry()));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn app_dozes_with_cached_audio_ahead_and_wakes_for_the_screen() {
    let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _player, root) = fixture(true);
    let mut platform = Headless { last: None };
    let songs = queue(&app, 3, 3, 1000);
    let trace = app.lib.traced_for_test();
    playing(&mut app, songs, 0);
    app.tick(&mut platform);
    assert!(app.net.dozing());
    assert!(app.lib.radio_dozing() && !app.lib.is_online());
    assert!(app.net.wanted(), "the saved Wi-Fi choice is unchanged");
    // Input lighting the screen ends it at once and arms the refill.
    app.screen_on = true;
    app.last_input = Instant::now();
    app.tick(&mut platform);
    assert!(!app.net.dozing() && !app.lib.radio_dozing());
    assert!(app.doze_refill);
    // Both are in the playtrace, for the power timeline (boot clock, ms).
    use crate::playtrace::Kind;
    let marks: Vec<(Kind, u32)> = trace.snapshot().events.iter().filter(|e| matches!(e.kind, Kind::Doze | Kind::Wake)).map(|e| (e.kind, e.a)).collect();
    assert_eq!(marks.len(), 2, "{marks:?}");
    assert_eq!(marks[0].0, Kind::Doze);
    assert!(marks[0].1 > 0, "the doze records the audio queued");
    assert_eq!(marks[1], (Kind::Wake, q2_machines::doze::WakeReason::Screen as u32 + 1));
    assert!(crate::rt::Ui::new().unwrap().block_on(app.settle_telemetry()));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn app_wakes_before_cached_audio_runs_out() {
    let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _player, root) = fixture(true);
    let mut platform = Headless { last: None };
    let songs = queue(&app, 2, 3, 1000);
    playing(&mut app, songs, 0);
    app.tick(&mut platform);
    assert!(app.net.dozing(), "2000 s of local audio ahead");
    // Later in the second (last cached) song: under the wake threshold.
    app.status.index = 1;
    app.status.position_ms = 1_000_000 - doze::WAKE_AHEAD_MS + 1;
    app.tick(&mut platform);
    assert!(!app.net.dozing());
    assert!(crate::rt::Ui::new().unwrap().block_on(app.settle_telemetry()));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn app_dozes_while_paused_only_when_configured_and_the_next_minutes_are_local() {
    let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|e| e.into_inner());
    let mut platform = Headless { last: None };
    // Dark and untouched for `idle`.
    let paused = |app: &mut App, songs: Vec<Song>, idle: Duration| {
        playing(app, songs, 0);
        app.status.state = PlayState::Paused;
        app.last_input = Instant::now() - idle;
        app.dark_since = Some(app.last_input);
    };
    // 0: a pause keeps the radio up however long it lasts.
    let (mut off, _p1, root1) = fixture(true);
    off.config.power.doze_paused_minutes = 0;
    let songs = queue(&off, 3, 3, 1000);
    paused(&mut off, songs, Duration::from_secs(3600));
    off.tick(&mut platform);
    assert!(!off.net.dozing(), "doze_paused_minutes 0");
    // Two minutes: not before, then down once the screen has been dark that long.
    let (mut on, _p2, root2) = fixture(true);
    on.config.power.doze_paused_minutes = 2;
    let songs = queue(&on, 1, 3, 1000);
    paused(&mut on, songs.clone(), Duration::from_secs(119));
    on.tick(&mut platform);
    assert!(!on.net.dozing(), "dark under two minutes");
    on.last_input = Instant::now() - Duration::from_secs(120);
    on.dark_since = Some(on.last_input);
    on.doze_checked = None;
    on.tick(&mut platform);
    assert!(on.net.dozing(), "1000 s local is over the 5 min wake margin");
    on.tick(&mut platform);
    assert!(on.net.dozing(), "the paused doze does not flap");
    on.screen_on = true;
    on.last_input = Instant::now();
    on.tick(&mut platform);
    assert!(!on.net.dozing(), "the screen wakes it");
    // A resume from the current song would stream: stay up.
    let (mut short, _p3, root3) = fixture(true);
    short.config.power.doze_paused_minutes = 2;
    let songs = queue(&short, 0, 3, 1000);
    paused(&mut short, songs, Duration::from_secs(600));
    short.tick(&mut platform);
    assert!(!short.net.dozing(), "nothing local ahead");
    for app in [&off, &on, &short] {
        assert!(crate::rt::Ui::new().unwrap().block_on(app.settle_telemetry()));
    }
    for root in [root1, root2, root3] { std::fs::remove_dir_all(root).unwrap(); }
}

/// Product feedback, 2026-11-22: doze the radio while paused, but only while paused
/// with the screen off; a lit screen may be the owner browsing.
#[test]
fn by_default_a_pause_dozes_only_after_a_minute_of_dark_screen() {
    let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|e| e.into_inner());
    let mut platform = Headless { last: None };
    let (mut app, _player, root) = fixture(true);
    assert_eq!(app.config.power.doze_paused_minutes, 1, "the default");
    let songs = queue(&app, 3, 3, 1000);
    playing(&mut app, songs.clone(), 0);
    app.status.state = PlayState::Paused;
    let tick = |app: &mut App, platform: &mut Headless| { app.doze_checked = None; app.tick(platform); app.net.dozing() };
    // Paused and lit, untouched for an hour (the backlight timeout off): up.
    app.screen_on = true;
    app.dark_since = None;
    app.config.display.backlight_seconds = 0;
    app.last_input = Instant::now() - Duration::from_secs(3600);
    assert!(!tick(&mut app, &mut platform), "paused and lit");
    // The backlight timeout darkens the screen; the delay starts there.
    app.config.display.backlight_seconds = 30;
    assert!(!tick(&mut app, &mut platform), "just dark");
    assert!(!app.screen_on && app.dark_since.is_some(), "the timeout turned the screen off");
    app.dark_since = Some(Instant::now() - Duration::from_secs(59));
    assert!(!tick(&mut app, &mut platform), "dark under a minute");
    // Input that leaves the screen dark (the wheel's volume) restarts the delay.
    app.dark_since = Some(Instant::now() - Duration::from_secs(600));
    app.last_input = Instant::now() - Duration::from_secs(59);
    assert!(!tick(&mut app, &mut platform), "touched under a minute ago");
    app.last_input = Instant::now() - Duration::from_secs(3600);
    assert!(tick(&mut app, &mut platform), "paused and dark for the delay: down");
    assert!(app.lib.radio_dozing() && app.net.wanted());
    assert!(tick(&mut app, &mut platform), "no flapping");
    // Paused and dozed, then the screen comes on: up at once.
    app.wake_at(&mut platform, Instant::now());
    app.last_input = Instant::now();
    assert!(app.screen_on && app.dark_since.is_none());
    assert!(!tick(&mut app, &mut platform), "the screen wakes it");
    assert!(!app.lib.radio_dozing() && app.doze_refill);
    assert!(!tick(&mut app, &mut platform), "and lit, it stays up");
    // Stopped, with the queue cleared or ended: never down, however dark.
    app.screen_on = false;
    app.dark_since = Some(Instant::now() - Duration::from_secs(3600));
    app.last_input = Instant::now() - Duration::from_secs(3600);
    app.status.state = PlayState::Stopped;
    assert!(!tick(&mut app, &mut platform), "stopped with the queue");
    app.status.queue = Arc::new(Vec::new());
    app.status.index = 0;
    assert!(!tick(&mut app, &mut platform), "stopped with no queue");
    // Playing: the doze threshold as before. 1000 s cached is under the
    // 30 min threshold, so playing keeps the radio up; the same queue paused
    // dozes (over the 5 min wake margin).
    let (mut short, _player2, root2) = fixture(true);
    let songs = queue(&short, 1, 3, 1000);
    playing(&mut short, songs, 0);
    short.dark_since = Some(Instant::now() - Duration::from_secs(3600));
    short.last_input = Instant::now() - Duration::from_secs(3600);
    assert!(!tick(&mut short, &mut platform), "playing below the doze threshold");
    short.status.state = PlayState::Paused;
    assert!(tick(&mut short, &mut platform), "paused over the wake margin");
    for app in [&app, &short] { assert!(crate::rt::Ui::new().unwrap().block_on(app.settle_telemetry())); }
    for root in [root, root2] { std::fs::remove_dir_all(root).unwrap(); }
}

#[test]
fn a_player_stream_waiting_for_the_radio_wakes_the_doze() {
    let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _player, root) = fixture(true);
    let mut platform = Headless { last: None };
    let songs = queue(&app, 3, 3, 1000);
    playing(&mut app, songs, 0);
    app.tick(&mut platform);
    assert!(app.net.dozing(), "3000 s of local audio ahead");
    app.lib.want_radio_for_test(true);
    app.tick(&mut platform);
    assert!(!app.net.dozing(), "the stream's wait wakes the radio although the cache ahead is long");
    app.lib.want_radio_for_test(false);
    // The same from a paused doze.
    app.status.state = PlayState::Paused;
    app.dark_since = Some(Instant::now() - Duration::from_secs(3600));
    app.last_input = Instant::now() - Duration::from_secs(3600);
    app.doze_checked = None;
    app.tick(&mut platform);
    assert!(app.net.dozing(), "paused and dark");
    app.lib.want_radio_for_test(true);
    app.tick(&mut platform);
    assert!(!app.net.dozing(), "a stream's wait wakes a paused doze");
    app.lib.want_radio_for_test(false);
    assert!(crate::rt::Ui::new().unwrap().block_on(app.settle_telemetry()));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn app_never_dozes_when_disabled_short_of_audio_or_offline() {
    let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|e| e.into_inner());
    let mut platform = Headless { last: None };
    let (mut off, _p1, root1) = fixture(false);
    let songs = queue(&off, 3, 3, 1000);
    playing(&mut off, songs, 0);
    off.tick(&mut platform);
    assert!(!off.net.dozing(), "disabled");
    let (mut short, _p2, root2) = fixture(true);
    let songs = queue(&short, 1, 3, 1000);
    playing(&mut short, songs, 0);
    short.tick(&mut platform);
    assert!(!short.net.dozing(), "1000 s ahead is under the 30 min doze threshold");
    let (mut offline, _p3, root3) = fixture(true);
    let songs = queue(&offline, 3, 3, 1000);
    offline.net.set_offline_mode(true, true, &[]);
    playing(&mut offline, songs, 0);
    offline.tick(&mut platform);
    assert!(!offline.net.dozing(), "Offline Mode already owns the radio");
    for app in [&off, &short, &offline] {
        assert!(crate::rt::Ui::new().unwrap().block_on(app.settle_telemetry()));
    }
    for root in [root1, root2, root3] { std::fs::remove_dir_all(root).unwrap(); }
}

#[test]
fn app_wakes_from_an_estimated_position_while_dark_and_when_the_queue_ends() {
    let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _player, root) = fixture(true);
    let mut platform = Headless { last: None };
    let songs = queue(&app, 2, 2, 1000);
    playing(&mut app, songs, 0);
    let start = Instant::now();
    app.tick_at(&mut platform, start);
    assert!(app.net.dozing(), "2000 s ahead");
    // Last song starts; the dark player reports no position after this.
    app.status.index = 1;
    app.status.position_ms = 0;
    app.tick_at(&mut platform, start + Duration::from_secs(1));
    assert!(app.net.dozing(), "1000 s ahead at its start");
    // 702 s later the reported position is still 0, but 298 s remain.
    app.tick_at(&mut platform, start + Duration::from_secs(703));
    assert!(!app.net.dozing(), "estimated position wakes the radio");
    // A doze never outlives the queue: Stopped wakes even with audio "ahead".
    let (mut ended, _p2, root2) = fixture(true);
    let songs = queue(&ended, 3, 3, 1000);
    playing(&mut ended, songs, 0);
    ended.tick(&mut platform);
    assert!(ended.net.dozing());
    ended.status.state = PlayState::Stopped;
    ended.tick(&mut platform);
    assert!(!ended.net.dozing());
    // Stopped closes the doze cycle and queues its finite writer; no fixture
    // directory may be deleted until that accepted telemetry has settled.
    for app in [&app, &ended] {
        assert!(crate::rt::Ui::new().unwrap().block_on(app.settle_telemetry()));
    }
    for root in [root, root2] { std::fs::remove_dir_all(root).unwrap(); }
}

#[test]
fn dynamic_doze_dozes_with_the_low_mark_plus_four_minutes_and_wakes_at_the_low_mark() {
    let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _player, root) = fixture_mode(true, 10);
    let mut platform = Headless { last: None };
    // Two 400 s songs: 800 s ahead, above the 3 + 4 minute doze point.
    let songs = queue(&app, 2, 2, 400);
    playing(&mut app, songs, 0);
    app.tick(&mut platform);
    assert!(app.net.dozing(), "800 s ahead after a fetch");
    app.status.index = 1;
    app.status.position_ms = 400_000 - 3 * 60_000 + 1_000;
    app.tick(&mut platform);
    assert!(!app.net.dozing(), "under 3 minutes left wakes it to fetch more");
    let log = std::fs::read_to_string(app.lib.paths.database().with_file_name("doze-cycles.log")).unwrap_or_default();
    assert!(log.is_empty(), "the cycle stays open until the next doze or the end: {log}");
    assert!(crate::rt::Ui::new().unwrap().block_on(app.settle_telemetry()));
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_doze_ended_outside_the_policy_brings_the_library_back_online() {
    let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _player, root) = fixture(true);
    let mut platform = Headless { last: None };
    let songs = queue(&app, 3, 3, 1000);
    playing(&mut app, songs, 0);
    app.tick(&mut platform);
    assert!(app.net.dozing() && app.lib.radio_dozing());
    // The radio's intent can leave doze without the policy's wake: an
    // explicit Wi-Fi choice ends it, and the Wi-Fi worker rolls a failed
    // radio-down back. The library's flag must follow on the next tick,
    // whatever the policy then decides.
    app.net.set_enabled(true, &[]);
    assert!(!app.net.dozing());
    app.status.state = PlayState::Paused;
    app.tick(&mut platform);
    assert!(!app.lib.radio_dozing(), "the library still treats the radio as dozing");
    assert!(crate::rt::Ui::new().unwrap().block_on(app.settle_telemetry()));
    std::fs::remove_dir_all(root).unwrap();
}

// Real UI policy ticks and cache lookup, with only the intervening fetch
// observation constructed. No network or decodable audio is involved.
fn closing_refill_sample(fetch_observed: bool) {
    let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _player, root) = fixture_mode(true, 10);
    let songs = queue(&app, 2, 4, 400);
    playing(&mut app, songs, 0);
    let start = Instant::now();
    app.doze_tick(start);
    assert!(app.net.dozing());
    app.status.index = 1;
    app.status.position_ms = 221_000;
    app.doze_tick(start + Duration::from_secs(600));
    assert!(!app.net.dozing(), "179 s remaining wakes the radio");
    app.doze_log.awake(start + Duration::from_secs(610), true, fetch_observed, 169_000, true);
    app.doze_fetching = fetch_observed;
    // Publish the remaining cache entries before the very next UI tick.
    app.status.queue = Arc::new(queue(&app, 4, 4, 400));
    app.status.position_ms = 251_000;
    app.doze_tick(start + Duration::from_secs(630));
    assert!(app.net.dozing(), "the completed refill immediately permits doze");
    assert!(crate::rt::Ui::new().unwrap().block_on(app.settle_telemetry()));
    let log = std::fs::read_to_string(app.lib.paths.database().with_file_name("doze-cycles.log")).unwrap();
    std::fs::remove_dir_all(root).unwrap();
    drop(_exclusive);
    let records: Vec<_> = log.lines().collect();
    assert_eq!(records.len(), 1, "{log}");
    let refill = if fetch_observed { "30000" } else { "na" };
    assert!(records[0].ends_with(&format!("ahead_ms=800000 dozed_s=600 wake=low wake_ahead_ms=179000 reconnect_ms=10000 refill_ms={refill} min_awake_ahead_ms=169000 ran_out=0 next_ahead_ms=949000")), "{log}");
}

#[test]
fn closing_doze_samples_the_just_completed_refill() { closing_refill_sample(true); }

#[test]
fn closing_doze_does_not_invent_an_unobserved_refill() { closing_refill_sample(false); }

#[test]
fn closing_doze_uses_the_last_refill_after_a_superseded_burst() {
    let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _player, root) = fixture_mode(true, 10);
    let songs = queue(&app, 2, 4, 400);
    playing(&mut app, songs, 0);
    let start = Instant::now();
    app.doze_tick(start);
    app.status.index = 1;
    app.status.position_ms = 221_000;
    app.doze_tick(start + Duration::from_secs(600));
    assert!(!app.net.dozing());
    app.doze_log.awake(start + Duration::from_secs(610), true, true, 169_000, true);
    app.doze_fetching = true;
    // The scheduler yields to a newer queue before that refill is complete.
    app.status.position_ms = 233_000;
    app.doze_tick(start + Duration::from_secs(612));
    assert!(!app.net.dozing());
    app.doze_log.awake(start + Duration::from_secs(620), true, true, 159_000, true);
    app.doze_fetching = true;
    app.status.queue = Arc::new(queue(&app, 4, 4, 400));
    app.status.position_ms = 251_000;
    app.doze_tick(start + Duration::from_secs(630));
    assert!(app.net.dozing());
    assert!(crate::rt::Ui::new().unwrap().block_on(app.settle_telemetry()));
    let log = std::fs::read_to_string(app.lib.paths.database().with_file_name("doze-cycles.log")).unwrap();
    std::fs::remove_dir_all(root).unwrap();
    assert_eq!(log.lines().count(), 1, "{log}");
    assert!(log.contains(" refill_ms=30000 "), "{log}");
    assert!(log.contains(" reconnect_ms=10000 ") && log.contains(" ran_out=0 "), "{log}");
}

#[test]
fn actual_ui_ticks_doze_wake_and_close_a_refill_through_the_network_task() {
    let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, _player, root) = fixture_mode(true, 10);
    app.net = Net::task_for_ui_test();
    let (tx, mut receive) = channel(); app.tx = tx;
    async fn tick_and_deliver(app: &mut App, platform: &mut Headless, receive: &mut UiRx, now: Instant) {
        app.tick_at_async(platform, now).await;
        while app.doze_cache.pending() {
            let message = tokio::time::timeout(Duration::from_secs(3), receive.recv()).await.unwrap().unwrap();
            if let UiMsg::DozeCacheReady(token) = message {
                // Same production completion handler, at this fixture's
                // constructed clock, after the real owner reply arrives.
                app.doze_cache_ready(token, now).await;
            }
        }
    }
    let songs = queue(&app, 2, 4, 400);
    playing(&mut app, songs, 0);
    let log = crate::rt::Ui::new().unwrap().block_on(async {
        app.net.set_enabled_async(true, &[]).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while app.net.state().ip.is_none() { tokio::task::yield_now().await; }
        }).await.unwrap();
        let start = Instant::now();
        let mut platform = Headless { last: None };
        tick_and_deliver(&mut app, &mut platform, &mut receive, start).await;
        assert!(app.net.dozing() && app.lib.radio_dozing());
        app.status.index = 1;
        app.status.position_ms = 221_000;
        tick_and_deliver(&mut app, &mut platform, &mut receive, start + Duration::from_secs(600)).await;
        assert!(!app.net.dozing() && !app.lib.radio_dozing());
        app.doze_log.awake(start + Duration::from_secs(610), true, true, 169_000, true);
        app.doze_fetching = true;
        app.status.queue = Arc::new(queue_async(&app.lib, 4, 4, 400).await);
        app.status.position_ms = 251_000;
        tick_and_deliver(&mut app, &mut platform, &mut receive, start + Duration::from_secs(630)).await;
        assert!(app.net.dozing() && app.lib.radio_dozing());
        assert!(app.settle_telemetry().await);
        std::fs::read_to_string(app.lib.paths.database().with_file_name("doze-cycles.log")).unwrap()
    });
    std::fs::remove_dir_all(root).unwrap();
    drop(_exclusive);
    assert_eq!(log.lines().count(), 1, "{log}");
    assert!(log.contains(" refill_ms=30000 ") && log.trim_end().ends_with("next_ahead_ms=949000"), "{log}");
}


#[test]
fn music_cursor_forwarding_completes_without_waiting_for_audio_or_waking_ui() {
    let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, real_player, root) = fixture(false);
    let (player, commands) = PlayerHandle::fixture(Status::default());
    app.player = player;
    app.screen_on = false;
    app.dirty = false;
    let mut platform = Headless { last: None };
    let (reply, mut received)=tokio::sync::oneshot::channel();
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut future = Box::pin(app.handle_async(UiMsg::MusicCursorProbe { deadline, reply, at_doze:None }, &mut platform));
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(std::future::Future::poll(future.as_mut(), &mut context).is_ready());
    drop(future);
    let Cmd::ObserveMusicCursor { deadline: forwarded, reply } = commands.try_recv().unwrap() else {
        panic!("the original request must reach the existing audio owner");
    };
    assert_eq!(forwarded, deadline);
    assert!(matches!(received.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)));
    assert!(!app.screen_on && !app.dirty && platform.last.is_none());
    reply.send(Err("generated audio refusal")).unwrap();
    assert_eq!(received.try_recv().unwrap(), Err("generated audio refusal"));
    // The cycle writer owns its generated path until its queued append joins.
    assert!(crate::rt::Ui::new().unwrap().block_on(app.settle_telemetry()));
    drop(app); real_player.shutdown();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn doze_cursor_arms_without_ui_work_and_observes_only_the_next_entry() {
    let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, real_player, root) = fixture(true);
    let (player, commands) = PlayerHandle::fixture(Status::default());
    app.player = player;
    let songs = queue(&app, 3, 3, 1000);
    playing(&mut app, songs, 0);
    // An unarmed entry must still count, so missing edges remain detectable.
    app.doze_tick(Instant::now());
    assert!(app.net.dozing());
    app.dirty = false;
    let mut platform = Headless {last:None};
    let boundary = Arc::new(std::sync::OnceLock::new());
    let (reply, mut received)=tokio::sync::oneshot::channel();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut future = Box::pin(app.handle_async(UiMsg::MusicCursorProbe {
        deadline, reply, at_doze:Some(boundary.clone()),
    }, &mut platform));
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(std::future::Future::poll(future.as_mut(), &mut context).is_ready());
    drop(future);
    assert!(!app.dirty && !app.screen_on && platform.last.is_none());
    app.doze_tick(Instant::now());
    assert!(boundary.get().is_none(), "already-dozing is not an entry");
    assert!(commands.try_recv().is_err());
    app.screen_on = true;
    app.doze_tick(Instant::now());
    assert!(!app.net.dozing());
    app.screen_on = false;
    let begin = crate::playtrace::boottime_us();
    app.doze_tick(Instant::now());
    let end = crate::playtrace::boottime_us();
    let observed = boundary.get().expect("the next automatic entry must be observed");
    assert_eq!(observed.ordinal, 2);
    assert!(begin <= observed.requested_us && observed.requested_us <= observed.confirmed_us && observed.confirmed_us <= end);
    assert!(observed.estimated_ahead_ms > 0);
    let Cmd::ObserveMusicCursor {deadline: forwarded, reply} = commands.try_recv().unwrap() else {panic!("cursor request missing")};
    assert!(forwarded <= deadline && forwarded <= Instant::now() + Duration::from_secs(2));
    assert!(matches!(received.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)));
    reply.send(Err("generated audio refusal")).unwrap();
    assert_eq!(received.try_recv().unwrap(), Err("generated audio refusal"));
    app.doze_tick(Instant::now());
    assert!(commands.try_recv().is_err(), "one arm permits one request");
    // The cycle writer owns its generated path until its queued append joins.
    assert!(crate::rt::Ui::new().unwrap().block_on(app.settle_telemetry()));
    drop(app); real_player.shutdown();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn doze_cursor_expires_even_when_policy_is_disabled() {
    let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, real_player, root) = fixture(false);
    let now = Instant::now();
    let deadline = now + Duration::from_secs(1);
    let boundary = Arc::new(std::sync::OnceLock::new());
    let (reply, mut received) = tokio::sync::oneshot::channel();
    app.doze_probe.arm(now, deadline, boundary.clone(), reply);
    app.doze_tick(deadline);
    assert!(received.try_recv().unwrap().is_err());
    assert!(boundary.get().is_none() && !app.doze_probe.armed());
    // The cycle writer owns its generated path until its queued append joins.
    assert!(crate::rt::Ui::new().unwrap().block_on(app.settle_telemetry()));
    drop(app); real_player.shutdown();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn pending_doze_snapshot_keeps_input_live_and_content_invalidation_rejects_its_reply() {
    let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, player, root) = fixture(true);
    let (tx, mut receive) = channel(); app.tx = tx;
    app.config.playback.volume = 30; app.config.playback.max_volume = 100;
    let songs = queue(&app, 3, 3, 1000); playing(&mut app, songs, 0);
    let (entered, begun) = std::sync::mpsc::channel();
    let (release, held) = std::sync::mpsc::channel();
    app.lib.db_post(move |_, _| { entered.send(()).unwrap(); let _ = held.recv_timeout(Duration::from_secs(5)); });
    begun.recv_timeout(Duration::from_secs(2)).unwrap();
    let mut platform = Headless { last: None };
    crate::rt::Ui::new().unwrap().block_on(async {
        // Exercise production async tick, not the ordinary fixture's explicit
        // snapshot-settling helper. Storage remains held across input handling.
        tokio::time::timeout(Duration::from_secs(1), app.tick_async(&mut platform)).await.unwrap();
        assert!(app.doze_cache.pending() && !app.net.dozing());
        tokio::time::timeout(Duration::from_secs(1), app.handle_async(UiMsg::Input(Raw::WheelTicks(1)), &mut platform)).await.unwrap();
        assert!(!app.screen_on, "the dark wheel remains a volume control");
        assert_eq!(app.config.playback.volume, 31, "input executes while storage is held");
        app.handle_async(UiMsg::Lib(LibEvent::Downloads), &mut platform).await;
        // Keep this fixture from requesting a replacement after rejecting the
        // content-invalidated result. This is not a claim that wheel wakes.
        app.screen_on = true;
        release.send(()).unwrap();
        loop {
            let message = tokio::time::timeout(Duration::from_secs(3), receive.recv()).await.unwrap().unwrap();
            if matches!(message, UiMsg::DozeCacheReady(_)) {
                app.handle_async(message, &mut platform).await;
                break;
            }
        }
        assert!(!app.doze_cache.pending());
        assert_eq!(app.doze_ahead_ms, 0, "a same-account content change invalidates pending availability");
        assert!(app.doze_checked.is_none() && !app.net.dozing());
        assert!(app.settle_telemetry().await);
    });
    drop(app); player.shutdown(); std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn doze_snapshot_binds_queue_player_seek_account_and_position_and_subtracts_consumption() {
    let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|e| e.into_inner());
    for mode in 0..7 {
        let (mut app, player, root) = fixture(true);
        let (tx, mut receive) = channel(); app.tx = tx;
        let songs = queue(&app, 3, 3, 1000); playing(&mut app, songs, 1_000);
        let token = crate::rt::Ui::new().unwrap().block_on(async {
            app.doze_tick_async(Instant::now()).await;
            assert!(!app.net.dozing(), "pending work cannot authorize sleep");
            loop {
                let message = tokio::time::timeout(Duration::from_secs(3), receive.recv()).await.unwrap().unwrap();
                if let UiMsg::DozeCacheReady(token) = message { break token; }
            }
        });
        // The result is already complete, but has not been accepted by the UI.
        // Keeping the screen lit isolates admission from another lookup/sleep.
        app.screen_on = true;
        match mode {
            0 => app.status.queue = Arc::new((*app.status.queue).clone()),
            1 => app.status.index += 1,
            2 => app.status.generation += 1,
            3 => app.status.seek_revision += 1,
            4 => app.lib.configure(&app.config),
            5 => app.status.position_ms = 0,
            _ => app.status.position_ms += 100,
        }
        let mut platform = Headless { last: None };
        app.handle(UiMsg::DozeCacheReady(token), &mut platform);
        assert_eq!(app.doze_ahead_ms, if mode == 6 { 2_998_900 } else { 0 }, "mode {mode}");
        assert_eq!(app.doze_checked.is_some(), mode == 6);
        assert!(!app.net.dozing());
        assert!(crate::rt::Ui::new().unwrap().block_on(app.settle_telemetry()));
        drop(app); player.shutdown(); std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn already_dozing_track_advance_keeps_a_conservative_bound_and_wakes_low_even_with_sql_pending() {
    let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, player, root) = fixture(true);
    let (tx, mut receive) = channel(); app.tx = tx;
    let songs = queue(&app, 3, 3, 1000); playing(&mut app, songs, 0);
    let now = Instant::now();
    app.doze_tick(now); assert!(app.net.dozing());
    // Discard only the wake for the snapshot the fixture explicitly settled.
    while receive.try_recv().is_ok() {}
    let (entered, begun) = std::sync::mpsc::channel();
    let (release, held) = std::sync::mpsc::channel();
    app.lib.db_post(move |_, _| { entered.send(()).unwrap(); held.recv_timeout(Duration::from_secs(5)).unwrap(); });
    begun.recv_timeout(Duration::from_secs(2)).unwrap();
    crate::rt::Ui::new().unwrap().block_on(async {
        app.status.index = 1; app.status.generation += 1; app.status.position_ms = 0;
        app.doze_tick_async(now).await;
        assert!(app.doze_cache.pending() && app.net.dozing());
        assert_eq!(app.doze_ahead_ms, 2_000_000);
        app.status.index = 2; app.status.generation += 1;
        app.doze_tick_async(now).await;
        assert!(app.net.dozing()); assert_eq!(app.doze_ahead_ms, 1_000_000);
        app.doze_tick_async(now + Duration::from_secs(701)).await;
        assert!(!app.net.dozing(), "pending SQL cannot suppress a low-buffer wake");
        assert_eq!(app.doze_ahead_ms, 299_000);
        app.status.position_ms = 701_000; app.screen_on = true;
        release.send(()).unwrap();
        while app.doze_cache.pending() {
            let message = tokio::time::timeout(Duration::from_secs(3), receive.recv()).await.unwrap().unwrap();
            if let UiMsg::DozeCacheReady(_) = message {
                let mut platform = Headless { last: None };
                app.handle_async(message, &mut platform).await;
            }
        }
        assert!(app.settle_telemetry().await);
    });
    drop(app); player.shutdown(); std::fs::remove_dir_all(root).unwrap();
}

/// docs/bugs/wifi-lps-after-power-cycle.md: every time the helper powers the
/// combo chip up after a `down`, the Bluetooth thread hears of it once.
#[test]
fn each_wifi_power_up_reaches_bluetooth_once() {
    let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|e| e.into_inner());
    let (mut app, player, root) = fixture(true);
    app.bluetooth = crate::bluetooth::Bluetooth::new(crate::bluetooth::Options::simulated(&root), Arc::new(|| {}));
    app.bluetooth.set_enabled(true);
    let until = |app: &App, what: &str, done: &dyn Fn(&crate::bluetooth::State) -> bool| {
        let limit = Instant::now() + Duration::from_secs(20);
        while !done(&app.bluetooth.state()) { assert!(Instant::now() < limit, "{what}: {:?}", app.bluetooth.state().resync); std::thread::sleep(Duration::from_millis(5)); }
    };
    until(&app, "ready", &|s| s.phase == crate::bluetooth::Phase::Ready);
    app.tick_bluetooth_power();
    app.net.simulate_helper(false);
    app.tick_bluetooth_power();
    app.net.simulate_helper(false);
    app.tick_bluetooth_power();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(app.bluetooth.state().resync.checks, 0, "down only: nothing to tell");
    app.net.simulate_helper(true);
    app.tick_bluetooth_power();
    app.tick_bluetooth_power();
    until(&app, "resynced", &|s| s.resync.reinits == 1 && s.phase == crate::bluetooth::Phase::Ready);
    app.net.simulate_helper(true);
    app.tick_bluetooth_power();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(app.bluetooth.state().resync.checks, 1, "an up without a down before it powered nothing up");
    app.bluetooth.shutdown();
    drop(app); player.shutdown(); std::fs::remove_dir_all(root).unwrap();
}
