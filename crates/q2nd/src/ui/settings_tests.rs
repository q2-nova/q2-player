//! Interaction regressions with generated configuration and simulated services.
use super::*;
use crate::audio::{eq, player};
use crate::config::{Quality, WifiNetwork};
use screens::settings::{Page, Settings};
use std::sync::atomic::{AtomicUsize, Ordering};

pub(crate) struct Fixture {
    app: App,
    inbox: crate::ui::UiRx,
    _receiver: std::sync::MutexGuard<'static, ()>,
}
impl std::ops::Deref for Fixture {
    type Target = App;
    fn deref(&self) -> &App { &self.app }
}
impl std::ops::DerefMut for Fixture {
    fn deref_mut(&mut self) -> &mut App { &mut self.app }
}

pub(crate) fn fixture() -> Fixture {
    fixture_with_startup(false)
}
pub(super) fn startup_fixture() -> Fixture {
    fixture_with_startup(true)
}
fn fixture_with_startup(retain_startup: bool) -> Fixture {
    let receiver = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let base = std::env::var_os("Q2_GENERATED_TEST_ROOT").map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build/perf"));
    let root = base.join(format!("settings-tests/{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    std::fs::create_dir_all(&root).unwrap();
    let paths = Paths::new(root.clone());
    let config = Config::default();
    let lib = Library::open(paths.clone(), &config, Arc::new(|_| {}));
    let player = player::spawn(Arc::new(crate::library::Tracks(lib.clone())),
        Box::new(crate::audio::alsa::SoftHw("null".into())), player::Settings {
            volume: config.playback.volume, headphone_power: crate::config::HeadphonePower::Standard, filter: 0,
            replay_gain: config.playback.replay_gain, gapless: true, eq: config.playback.eq.clone(),
        }, Arc::new(|| {}));
    let net = Net::new(root.join("unused-wifi.sh"), true, Arc::new(|| {}));
    let (tx, inbox) = channel();
    let notify = tx.clone();
    let tailscale = crate::tailscale::Tailscale::new_notifying(root.clone(), root.clone(), true, false,
        Arc::new(move || { let _ = notify.send(UiMsg::Tailscale); }));
    let mut app = App::new(lib, player, config, paths, net, tailscale, Fonts::bundled(), tx);
    if !retain_startup { app.stack.truncate(1); }
    Fixture { app, inbox, _receiver: receiver }
}

fn settle_home(app: &mut Fixture) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !app.stack[0].1.script_ready() {
        let message = crate::ui::recv_within(&mut app.inbox, deadline.saturating_duration_since(Instant::now())).expect("Home worker completes");
        app.handle(message, &mut crate::platform::Headless { last: None });
    }
}

fn home_labels(app: &App) -> Vec<String> {
    app.stack[0].1.audit().rows().iter().map(|row| row.title.clone()).collect()
}

#[test]
fn home_spoken_visibility_tracks_setup_and_real_disk_content_and_keeps_back_selection() {
    use subsonic::{Song, Spoken, SpokenSource};
    let mut app = fixture();
    settle_home(&mut app);
    assert!(!home_labels(&app).iter().any(|label| label == "Books" || label == "Podcasts"));
    select_label(&mut app, "Settings");

    // Unindexed bare M4B discovery uses real metadata, never extension alone.
    let root = app.paths.media_root().join("audiobooks");
    std::fs::write(root.join("bad.m4b"), b"generated invalid metadata").unwrap();
    let mut mvhd = vec![0_u8; 20];
    mvhd[12..16].copy_from_slice(&1000_u32.to_be_bytes());
    mvhd[16..20].copy_from_slice(&60000_u32.to_be_bytes());
    let atom = |kind: &[u8], body: &[u8]| [(body.len() as u32 + 8).to_be_bytes().as_slice(), kind, body].concat();
    let book = root.join("generated.m4b");
    std::fs::write(&book, atom(b"moov", &atom(b"mvhd", &mvhd))).unwrap();
    step(&mut app, Event::Center);
    step(&mut app, Event::Back);
    settle_home(&mut app);
    assert!(home_labels(&app).contains(&"Books".into()));
    assert!(!home_labels(&app).contains(&"Podcasts".into()));
    assert_eq!(app.stack[0].1.audit().rows()[app.stack[0].1.audit().selected()].title, "Settings");
    let parked = root.with_file_name("generated-unavailable-books");
    std::fs::rename(&root, &parked).unwrap();
    std::fs::write(&root, b"generated unreadable-directory state").unwrap();
    app.handle(UiMsg::Lib(LibEvent::Downloads), &mut crate::platform::Headless { last: None });
    settle_home(&mut app);
    assert!(home_labels(&app).contains(&"Books".into()), "actual scan error preserves known local availability");
    std::fs::remove_file(&root).unwrap();
    std::fs::rename(parked, &root).unwrap();
    // Exercise the existence probe itself: ENOTDIR beneath an unavailable
    // mount path was silently false with Path::exists(), losing known rows.
    let media = app.paths.media_root().to_path_buf();
    let parked_media = media.with_extension("generated-card-unavailable");
    std::fs::rename(&media, &parked_media).unwrap();
    std::fs::write(&media, b"generated mount-path obstruction").unwrap();
    app.handle(UiMsg::Lib(LibEvent::Downloads), &mut crate::platform::Headless { last: None });
    settle_home(&mut app);
    assert!(home_labels(&app).contains(&"Books".into()), "existence-probe errors are unknown, not absence");
    std::fs::remove_file(&media).unwrap();
    std::fs::rename(parked_media, &media).unwrap();
    select_label(&mut app, "Books");
    step(&mut app, Event::Center);
    assert_eq!(title(&app), "Books");
    step(&mut app, Event::Back);
    settle_home(&mut app);
    assert_eq!(app.stack[0].1.audit().rows()[app.stack[0].1.audit().selected()].title, "Books");

    let episode = Song { id: "generated-episode".into(), spoken: Some(Box::new(Spoken {
        id: "generated-logical".into(), source: SpokenSource::Audiobookshelf {
            namespace: "generated-account".into(), item: "generated-show".into(),
            episode: Some("generated-episode".into()), content_url: "generated.mp3".into(),
        }, ..Default::default()
    })), ..Default::default() };
    app.lib.db().put_spoken(&crate::rt::Blocking::dedicated_thread(), &episode).unwrap();
    app.handle(UiMsg::Lib(LibEvent::Downloads), &mut crate::platform::Headless { last: None });
    settle_home(&mut app);
    assert!(!home_labels(&app).contains(&"Podcasts".into()), "remote metadata is not a download");
    let downloaded = app.paths.media_root().join("generated-episode.mp3");
    std::fs::write(&downloaded, b"generated indexed media").unwrap();
    app.lib.db().put_spoken_downloads(&crate::rt::Blocking::dedicated_thread(), "generated-logical", &[(episode.id.clone(), downloaded.clone(), 23)]).unwrap();
    app.handle(UiMsg::Lib(LibEvent::Downloads), &mut crate::platform::Headless { last: None });
    settle_home(&mut app);
    assert!(home_labels(&app).contains(&"Podcasts".into()));
    let home = app.stack[0].0;
    app.handle(UiMsg::Loaded { screen: home, token: 1, data: Box::new(crate::library::spoken::Availability::default()) },
        &mut crate::platform::Headless { last: None });
    assert!(home_labels(&app).contains(&"Books".into()) && home_labels(&app).contains(&"Podcasts".into()),
        "temporary unknown card/index result preserves known availability");
    std::fs::remove_file(&book).unwrap();
    app.handle(UiMsg::Lib(LibEvent::Downloads), &mut crate::platform::Headless { last: None });
    settle_home(&mut app);
    assert!(!home_labels(&app).contains(&"Books".into()), "stale local catalog plus malformed M4B cannot retain Books");
    select_label(&mut app, "Podcasts");
    step(&mut app, Event::Center);
    assert_eq!(title(&app), "Podcasts");
    step(&mut app, Event::Back);
    settle_home(&mut app);
    assert_eq!(app.stack[0].1.audit().rows()[app.stack[0].1.audit().selected()].title, "Podcasts");
    std::fs::remove_file(downloaded).unwrap();
    app.handle(UiMsg::Lib(LibEvent::Downloads), &mut crate::platform::Headless { last: None });
    settle_home(&mut app);
    assert!(!home_labels(&app).contains(&"Podcasts".into()), "missing indexed file cannot retain Podcasts");
    assert!(app.stack[0].1.audit().selected() < home_labels(&app).len(), "removed selected row clamps safely");

    // Configured-but-offline ABS exposes both, including an invalidated client.
    app.config.audiobookshelf.url = "http://127.0.0.1:9".into();
    app.config.audiobookshelf.token = "generated-token".into();
    app.config_changed();
    app.lib.set_offline_mode(true);
    app.handle(UiMsg::Lib(LibEvent::SpokenChanged), &mut crate::platform::Headless { last: None });
    assert!(home_labels(&app).contains(&"Books".into()));
    assert!(home_labels(&app).contains(&"Podcasts".into()));
    assert!(app.lib.check_shelf_connection().is_err());
    app.config.audiobookshelf.token.clear();
    app.config_changed();
    app.handle(UiMsg::Lib(LibEvent::SpokenChanged), &mut crate::platform::Headless { last: None });
    assert!(!home_labels(&app).iter().any(|label| label == "Books" || label == "Podcasts"));
}

fn page(app: &mut App, p: Page) {
    app.stack.truncate(1);
    app.push(Box::new(Settings::new(p)));
}
fn step(app: &mut App, ev: Event) {
    app.event(ev, &mut crate::platform::Headless { last: None });
    // The main loop applies captured Offline intent before the policy tick.
    let requests = app.take_requests();
    for request in requests {
        if matches!(request, Request::OfflineMode { .. }) {
            crate::rt::Ui::new().unwrap().block_on(crate::request(app, &mut crate::platform::Headless { last: None }, request));
        } else { app.pending.push(request); }
    }
}
fn title(app: &App) -> String { app.stack.last().unwrap().1.title() }

fn select_label(app: &mut App, label: &str) {
    let surface = app.stack.last().unwrap().1.audit();
    let index = surface.rows().iter().position(|row| row.title == label).unwrap_or_else(|| panic!("missing {label} on {}", title(app)));
    let detents = surface.controls().iter().position(|&row| row == index).expect("actionable fixture row");
    step(app, Event::WheelEnd);
    step(app, Event::Scroll(i32::MIN));
    step(app, Event::WheelEnd);
    step(app, Event::Scroll(detents as i32));
    step(app, Event::WheelEnd);
}

#[test]
fn curve_navigation_clicks_the_new_frequency_and_respects_clicker_off() {
    struct SoundProbe(Vec<Option<u16>>);
    impl crate::platform::Platform for SoundProbe {
        fn present<'a>(&'a mut self, _: &'a Canvas) -> crate::platform::PresentFuture<'a> { Box::pin(async { Ok(()) }) }
        fn set_screen(&mut self, _: bool) {}
        fn set_brightness(&mut self, _: u8) {}
        fn battery(&mut self) -> Battery { Battery::default() }
        fn headphones(&mut self) -> Option<bool> { None }
        fn audio_hw(&mut self) -> Box<dyn crate::audio::alsa::AudioHw> {
            Box::new(crate::audio::alsa::SoftHw("null".into()))
        }
        fn suspend(&mut self) -> crate::platform::SuspendFuture<'_> { Box::pin(async { Ok(()) }) }
        fn power_off(&mut self) -> crate::platform::SuspendFuture<'_> { Box::pin(async { Ok(()) }) }
        fn is_device(&self) -> bool { false }
        fn click(&mut self) { self.0.push(None); }
        fn click_tone(&mut self, hz: u16) { self.0.push(Some(hz)); }
    }
    let mut app = fixture();
    let mut sound = SoundProbe(Vec::new());
    app.config.playback.eq.headphone = Some(eq::Profile::parse(
        "Generated headphones".into(), "fixture".into(),
        "Preamp: -3 dB\nFilter 1: ON PK Fc 1000 Hz Gain 3 dB Q 1").unwrap());
    app.push(Box::new(screens::equalizer::Equalizer::new()));
    step(&mut app, Event::Scroll(6));
    step(&mut app, Event::Center);
    assert_eq!(title(&app), "EQ Curves");
    let initial = app.stack.last().unwrap().1.click_frequency().unwrap();
    app.event(Event::Scroll(10), &mut sound);
    let higher = sound.0[0].unwrap();
    assert!(higher > initial, "sound follows the point after scrolling");
    app.event(Event::Scroll(-20), &mut sound);
    assert!(sound.0[1].unwrap() < initial);
    app.event(Event::Center, &mut sound);
    assert_eq!(sound.0[2], sound.0[1], "headroom changes do not change frequency pitch");
    app.config.display.clicker = false;
    app.event(Event::Scroll(10), &mut sound);
    assert_eq!(sound.0.len(), 3);
    app.config.display.clicker = true;
    app.pop();
    app.event(Event::Scroll(1), &mut sound);
    assert_eq!(sound.0.last(), Some(&None), "ordinary pages keep ordinary clicks");
}

#[test]
fn settings_choices_start_at_saved_value_and_cancel_preserves_parent_position() {
    let mut app = fixture();
    app.config.playback.download_quality = Quality::Mp3_192;
    page(&mut app, Page::Root);
    step(&mut app, Event::Scroll(1));
    step(&mut app, Event::Center);
    assert_eq!(title(&app), "Playback");
    select_label(&mut app, "Download Quality");
    step(&mut app, Event::Center);
    assert_eq!(title(&app), "Download Quality");
    step(&mut app, Event::Scroll(1));
    step(&mut app, Event::Back);
    assert_eq!(app.config.playback.download_quality, Quality::Mp3_192);
    step(&mut app, Event::Center);
    assert_eq!(title(&app), "Download Quality", "return keeps the parent row");
    step(&mut app, Event::Scroll(-1));
    step(&mut app, Event::Center);
    assert_eq!(app.config.playback.download_quality, Quality::Mp3_320, "choices start on the current value");
    step(&mut app, Event::Back);
    step(&mut app, Event::Center);
    assert_eq!(title(&app), "Playback", "root also remembers the category");
    app.save_settings();
    assert_eq!(Config::load(&crate::rt::Blocking::dedicated_thread(), &app.paths.config()).playback.download_quality, Quality::Mp3_320);
}

#[test]
fn headphone_power_offers_every_level_starting_at_the_saved_one_and_saves_the_choice() {
    use crate::config::HeadphonePower;
    let mut app = fixture();
    assert_eq!(app.config.playback.headphone_power(), HeadphonePower::Efficient, "the default");
    page(&mut app, Page::Sound);
    select_label(&mut app, "Headphone Power");
    step(&mut app, Event::Center);
    assert_eq!(title(&app), "Headphone Power");
    // Back changes nothing.
    step(&mut app, Event::Scroll(1));
    step(&mut app, Event::Back);
    assert_eq!(app.config.playback.headphone_power(), HeadphonePower::Efficient);
    for (scroll, level) in [(1, HeadphonePower::Standard), (2, HeadphonePower::Maximum), (-3, HeadphonePower::Efficient), (2, HeadphonePower::High)] {
        step(&mut app, Event::Center);
        assert_eq!(title(&app), "Headphone Power");
        step(&mut app, Event::Scroll(scroll));
        step(&mut app, Event::Center);
        assert_eq!(title(&app), "Sound");
        assert_eq!(app.config.playback.headphone_power(), level);
    }
    app.save_settings();
    let saved = Config::load(&crate::rt::Blocking::dedicated_thread(), &app.paths.config()).playback;
    assert_eq!((saved.headphone_power(), saved.high_gain), (HeadphonePower::High, true));
}

#[test]
fn volume_limit_slider_clamps_playback_without_raising_volume() {
    let mut app = fixture();
    app.config.playback.volume = 75;
    app.config.playback.max_volume = 100;
    page(&mut app, Page::VolumeLimit);
    step(&mut app, Event::Scroll(-40));
    assert_eq!((app.config.playback.max_volume, app.config.playback.volume), (60, 60));
    step(&mut app, Event::Scroll(20));
    assert_eq!((app.config.playback.max_volume, app.config.playback.volume), (80, 60));
    step(&mut app, Event::Scroll(i32::MAX));
    assert_eq!(app.config.playback.max_volume, 100);
    step(&mut app, Event::Scroll(i32::MIN));
    assert_eq!((app.config.playback.max_volume, app.config.playback.volume), (1, 1));
}

#[test]
fn time_zone_restart_requires_confirmation_and_same_zone_does_nothing() {
    let mut app = fixture();
    app.config.display.timezone = "Europe/London".into();
    page(&mut app, Page::TimeZone);
    step(&mut app, Event::Center);
    assert!(app.pending.iter().all(|r| !matches!(r, Request::Restart)));
    assert_eq!(app.stack.len(), 1, "current zone closes the single picker");
    page(&mut app, Page::TimeZone);
    select_label(&mut app, "GMT+5:45 Kathmandu");
    step(&mut app, Event::Center);
    assert_eq!(title(&app), "Change Time Zone?");
    assert_eq!(app.config.display.timezone, "Europe/London");
    assert!(app.pending.is_empty());
    step(&mut app, Event::Center); // Cancel is selected by default.
    assert_eq!(app.config.display.timezone, "Europe/London");
    step(&mut app, Event::Center);
    step(&mut app, Event::Back); // Back also cancels.
    assert!(app.pending.is_empty());
    step(&mut app, Event::Center);
    step(&mut app, Event::Scroll(1));
    step(&mut app, Event::Center);
    assert_eq!(app.config.display.timezone, "Asia/Kathmandu");
    assert_eq!(app.pending.iter().filter(|r| matches!(r, Request::Restart)).count(), 1);
}

#[test]
fn time_zone_picker_preserves_saved_legacy_aliases_on_back() {
    let mut app = fixture();
    for zone in ["UTC", "US/Eastern", "<+0330>-3:30"] {
        app.config.display.timezone = zone.into();
        page(&mut app, Page::TimeZone);
        step(&mut app, Event::Back);
        assert_eq!(app.config.display.timezone, zone);
        assert!(app.pending.iter().all(|r| !matches!(r, Request::Restart)));
    }
}

#[test]
fn eq_presets_reset_and_curve_link_preserve_the_headphone_layer() {
    let mut app = fixture();
    let profile = eq::Profile::parse("Test headphones".into(), "fixture".into(),
        "Preamp: -3 dB\nFilter 1: ON PK Fc 1000 Hz Gain 3 dB Q 1").unwrap();
    app.config.playback.eq.headphone = Some(profile.clone());
    app.config.playback.eq.headphone_enabled = true;
    app.config.playback.eq.set_preset(5); // Rock
    app.push(Box::new(screens::equalizer::Equalizer::new()));
    step(&mut app, Event::Scroll(1));
    step(&mut app, Event::Center);
    step(&mut app, Event::Scroll(1));
    step(&mut app, Event::Center);
    assert_eq!(app.config.playback.eq.preset_name(), "Pop");
    step(&mut app, Event::Scroll(2));
    step(&mut app, Event::Center);
    assert_eq!(title(&app), "Reset EQ Bands?");
    step(&mut app, Event::Center);
    assert_eq!(app.config.playback.eq.preset_name(), "Pop");
    step(&mut app, Event::Center);
    step(&mut app, Event::Scroll(1));
    step(&mut app, Event::Center);
    assert_eq!(app.config.playback.eq.bands, [0; 10]);
    assert_eq!(app.config.playback.eq.headphone, Some(profile));
    assert!(app.config.playback.eq.headphone_enabled);
    step(&mut app, Event::Scroll(3));
    step(&mut app, Event::Center);
    assert_eq!(title(&app), "EQ Curves");
}

#[test]
fn wifi_owner_updates_refresh_rows_at_once_and_pending_updates_preserve_clicked_identity() {
    let mut app = fixture();
    app.push(Box::new(screens::wifi::Wifi::new()));
    let networks = |names: &[&str]| names.iter().map(|name| crate::net::ScanResult {
        ssid: (*name).into(), signal: -50, secured: true, security: crate::config::WifiSecurity::Wpa2,
    }).collect();
    let mut platform = crate::platform::Headless { last: None };
    app.net.simulate_scan(networks(&["Alpha", "Beta"]));
    app.handle(UiMsg::Net, &mut platform);
    let labels: Vec<_> = app.stack.last().unwrap().1.audit().rows().iter().map(|row| row.title.clone()).collect();
    assert!(labels.iter().any(|label| label == "Beta"));
    assert!(!labels.iter().any(|label| label == "Neighbours"), "no two-second refresh delay");
    select_label(&mut app, "Beta");
    // Backend publication can precede its queued UI notification. Center must
    // still act on Beta, which was displayed at this row's index.
    app.net.simulate_scan(networks(&["Gamma", "Delta"]));
    step(&mut app, Event::Center);
    assert_eq!(title(&app), "Password for Beta");
    app.handle(UiMsg::Net, &mut platform);
    assert_eq!(title(&app), "Password for Beta", "background refresh cannot replace the active editor");
    step(&mut app, Event::Back);
    assert_eq!(title(&app), "Wi-Fi");
    assert!(app.config.wifi.networks.is_empty(), "Back cancels without saving credentials");
    app.player.shutdown();
}

#[test]
fn connection_and_exit_actions_only_run_after_confirmation() {
    let mut app = fixture();
    app.config.wifi.networks.push(WifiNetwork { ssid: "Simulator".into(), psk: "synthetic-password".into(), ..Default::default() });
    app.push(Box::new(screens::wifi::Wifi::new()));
    select_label(&mut app, "Forget This Network…");
    step(&mut app, Event::Center);
    assert_eq!(title(&app), "Forget Network?");
    step(&mut app, Event::Center);
    assert_eq!(app.config.wifi.networks.len(), 1);
    step(&mut app, Event::Center);
    step(&mut app, Event::Scroll(1));
    step(&mut app, Event::Center);
    assert!(app.config.wifi.networks.is_empty());
}

#[test]
fn system_menu_ends_at_date_and_time_then_about() {
    let mut app = fixture();
    let base = app.paths.root.clone();
    app.paths = Paths::installed(base.clone(), base.join("card"), base.join("run"), true, None);
    page(&mut app, Page::System);
    step(&mut app, Event::Scroll(99));
    step(&mut app, Event::Center);
    assert_eq!(title(&app), "About", "About is the last row under System");
    step(&mut app, Event::Back);
    step(&mut app, Event::Scroll(-1));
    step(&mut app, Event::Center);
    assert_eq!(title(&app), "Date & Time");
    assert!(app.pending.is_empty());
}

#[test]
fn settings_for_hardware_that_did_not_open_are_shown_unavailable() {
    let mut app = fixture();
    let row = |app: &App, label: &str| {
        let surface = app.stack.last().unwrap().1.audit();
        let index = surface.rows().iter().position(|r| r.title == label).expect(label);
        (surface.rows()[index].value.clone(), surface.controls().contains(&index))
    };
    page(&mut app, Page::System);
    assert_eq!(row(&app, "Clicker"), (Some("On".into()), true));
    app.hardware = crate::platform::Hardware { clicker: false, headphone_switch: false, touchscreen: false };
    page(&mut app, Page::System);
    assert_eq!(row(&app, "Clicker"), (Some("Unavailable".into()), false));
    page(&mut app, Page::Playback);
    assert_eq!(row(&app, "Pause on Unplug"), (Some("Unavailable".into()), false));
}

#[test]
fn an_unset_zone_reads_utc() {
    let mut app = fixture();
    let zone = |app: &App| app.stack.last().unwrap().1.audit().rows()[0].detail.clone();
    page(&mut app, Page::DateTime);
    assert_eq!(zone(&app).as_deref(), Some("UTC"));
    let base = app.paths.root.clone();
    app.paths = Paths::installed(base.clone(), base.join("card"), base.join("run"), true, None);
    page(&mut app, Page::DateTime);
    assert_eq!(zone(&app).as_deref(), Some("UTC"));
}

#[test]
fn home_power_menu_offers_sleep_and_power_off_only() {
    let mut app = fixture();
    for installed in [false, true] {
        app.stack.truncate(1);
        let root = app.paths.root.clone();
        app.paths = if !installed { Paths::new(root.clone()) } else { Paths::installed(root.clone(), root.join("generated-card"), root.join("generated-run"), true, None) };
        step(&mut app, Event::CenterLong);
        assert_eq!(title(&app), "Power");
        let menu = app.stack.last().unwrap().1.audit();
        assert_eq!(menu.rows().len(), 2);
        assert!(menu.rows().iter().any(|row| row.title == "Sleep"));
        assert!(menu.rows().iter().any(|row| row.title == "Power Off"));
        step(&mut app, Event::Back);
        assert!(app.pending.is_empty(), "cancel never requests a handover or power action");
    }
    app.player.shutdown();
}

#[test]
fn script_observers_publish_owner_changes_and_retire_without_normal_polling() {
    let mut app = fixture();
    let mut platform = crate::platform::Headless { last: None };
    assert!(app.script_observer.is_none() && app.numeric_observer.is_none());
    let (reply, mut subscribed) = tokio::sync::oneshot::channel();
    app.handle(UiMsg::ScriptSubscribe(reply), &mut platform);
    let mut states = subscribed.try_recv().unwrap();
    states.borrow_and_update();
    app.publish_script_observers();
    assert!(!states.has_changed().unwrap(), "unchanged state must not wake the script");
    step(&mut app, Event::CenterLong);
    app.publish_script_observers();
    assert!(states.has_changed().unwrap());
    assert_eq!(states.borrow_and_update().title, "Power");
    step(&mut app, Event::Back);
    app.publish_script_observers();
    assert!(states.has_changed().unwrap());
    assert!(!states.borrow_and_update().overlay);
    let (reply, mut subscribed) = tokio::sync::oneshot::channel();
    app.handle(UiMsg::NumericScriptSubscribe(reply), &mut platform);
    let mut numeric = subscribed.try_recv().unwrap();
    assert_eq!(*numeric.borrow_and_update(), crate::numeric_probe::Playback::from_status(&app.player.status()));
    app.publish_script_observers();
    assert!(!numeric.has_changed().unwrap());
    drop(states);
    drop(numeric);
    app.publish_script_observers();
    assert!(app.script_observer.is_none() && app.numeric_observer.is_none());
    // A cancelled subscription request creates no persistent observer.
    let (reply, cancelled) = tokio::sync::oneshot::channel();
    drop(cancelled);
    app.handle(UiMsg::ScriptSubscribe(reply), &mut platform);
    assert!(app.script_observer.is_none());
    app.player.shutdown();
}

#[test]
fn acquired_long_press_survives_a_coalesced_ui_backlog_without_selecting_a_menu_item() {
    use crate::input::{Key, Raw};
    let mut app = fixture();
    let acquired = Instant::now() - Duration::from_secs(10);
    let mut platform = crate::platform::Headless { last: None };
    app.handle(UiMsg::TimedInput { raw: Raw::Key { key: Key::Center, down: true }, received: acquired }, &mut platform);
    app.handle(UiMsg::TimedInput { raw: Raw::Key { key: Key::Center, down: false }, received: acquired + crate::input::LONG_PRESS }, &mut platform);
    assert_eq!(title(&app), "Power", "long Center opens the actual context menu even when both edges were queued");
    assert!(app.pending.is_empty(), "release cannot also choose Sleep");
    let (reply, mut state)=tokio::sync::oneshot::channel();
    app.handle(UiMsg::ScriptProbe(reply), &mut platform);
    assert_eq!(state.try_recv().unwrap(), ScriptState { title: "Power".into(), ready: true, idle: true, overlay: true, shuffle: app.status.shuffle, repeat: app.status.repeat });
    app.player.shutdown();
}

#[test]
fn long_held_center_does_not_request_busy_loop_wakes_after_opening_power() {
    use crate::input::{Key, Raw};
    let (immediate, before, after) = {
        let mut app = fixture();
        let mut platform = crate::platform::Headless { last: None };
        let acquired = Instant::now() - Duration::from_secs(10);
        app.handle(UiMsg::TimedInput { raw: Raw::Key { key: Key::Center, down: true }, received: acquired }, &mut platform);
        app.tick(&mut platform);
        assert_eq!(title(&app), "Power");
        let mut immediate = 0;
        for _ in 0..10_000 {
            if app.next_wake() == Duration::ZERO { immediate += 1; }
        }
        let before = app.next_wake();
        app.handle(UiMsg::TimedInput { raw: Raw::Key { key: Key::Center, down: false }, received: Instant::now() }, &mut platform);
        let after = app.next_wake();
        assert_eq!(title(&app), "Power", "release cannot select Sleep");
        assert!(app.pending.is_empty());
        app.player.shutdown();
        (immediate, before, after)
    }; // Release the shared test receiver even when the negative control fails.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build/ui");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("held-key-wakes.json"), serde_json::to_vec_pretty(&serde_json::json!({
        "method":"10000 actual App::next_wake requests after acquired long Center; no elapsed CPU/energy claim",
        "requests":10000,"zero_duration_requests":immediate,"held_wait_us":before.as_micros(),"released_wait_us":after.as_micros()
    })).unwrap()).unwrap();
    assert_eq!(immediate, 0, "a non-repeating held action cannot leave a stale immediate deadline");
    assert!(before > Duration::ZERO && after > Duration::ZERO);
}

#[test]
fn navidrome_settings_stay_in_connections_and_tailscale_keeps_its_own_controls() {
    fn preview(app: &mut App, name: &str) {
        let frame = app.render();
        let pixels: Vec<u8> = frame.pixels.iter().flat_map(|p|
            [(*p >> 16) as u8, (*p >> 8) as u8, *p as u8]).collect();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../../build/perf/tailscale-menu-{name}.png"));
        image::save_buffer(path, &pixels, crate::gfx::WIDTH as u32,
            crate::gfx::HEIGHT as u32, image::ColorType::Rgb8).unwrap();
    }
    let mut app = fixture();
    page(&mut app, Page::Connections);
    preview(&mut app, "connections");
    step(&mut app, Event::Scroll(1));
    step(&mut app, Event::Center);
    assert_eq!(title(&app), "Navidrome Server");
    step(&mut app, Event::Back);
    step(&mut app, Event::Scroll(2));
    step(&mut app, Event::Center);
    assert_eq!(title(&app), "Tailscale");
    assert!(!app.config.tailscale.enabled);
    preview(&mut app, "off");
    // Off has just its toggle: scrolling cannot select a server-settings link.
    step(&mut app, Event::Scroll(1));
    step(&mut app, Event::Center);
    assert_eq!(title(&app), "Tailscale");
    assert!(app.config.tailscale.enabled);
    preview(&mut app, "on");
    step(&mut app, Event::Scroll(1));
    step(&mut app, Event::Center);
    assert_eq!(title(&app), "Sign In", "the next enabled action is Tailscale login");
    step(&mut app, Event::Back);
    step(&mut app, Event::Back);
    assert_eq!(title(&app), "Connections");
    step(&mut app, Event::Scroll(-2));
    step(&mut app, Event::Center);
    assert_eq!(title(&app), "Navidrome Server", "the separate server entry stays reachable");
}

#[test]
fn cache_confirmation_keeps_downloads_and_cancelling_keeps_the_cache() {
    let mut app = fixture();
    let cache = app.paths.root.join("generated-cache.bin");
    let download = app.paths.root.join("generated-download.bin");
    std::fs::write(&cache, b"cached").unwrap();
    std::fs::write(&download, b"download").unwrap();
    app.lib.db().put_cached_song(&crate::rt::Blocking::dedicated_thread(), "cached", "original", &cache, 6).unwrap();
    app.lib.db().put_offline_song(&crate::rt::Blocking::dedicated_thread(), &subsonic::Song { id: "download".into(), ..Default::default() }, &download, 8);
    page(&mut app, Page::Storage);
    select_label(&mut app, "Clear Song Cache…");
    step(&mut app, Event::Center);
    assert_eq!(title(&app), "Clear Song Cache?");
    step(&mut app, Event::Center);
    assert!(cache.exists());
    assert_eq!(app.lib.storage_used(), (6, 8));
    step(&mut app, Event::Center);
    step(&mut app, Event::Scroll(1));
    let (entered, begun) = std::sync::mpsc::channel();
    let (release, held) = std::sync::mpsc::channel();
    app.lib.db_post(move |_, _| { entered.send(()).unwrap(); held.recv_timeout(Duration::from_secs(5)).unwrap(); });
    begun.recv_timeout(Duration::from_secs(3)).unwrap();
    step(&mut app, Event::Center);
    assert_ne!(title(&app), "Clear Song Cache?", "confirmation returns without waiting for storage");
    assert_eq!(app.toast.as_ref().map(|(text, _)| text.as_str()), Some("Clearing song cache…"));
    assert!(cache.exists(), "pending clear is not a completion acknowledgement");
    release.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while app.toast.as_ref().is_none_or(|(text, _)| text != "Song cache cleared") {
        let message = recv_within(&mut app.inbox, deadline.saturating_duration_since(Instant::now())).expect("cache clearing completion notice");
        app.handle(message, &mut crate::platform::Headless { last: None });
    }
    assert!(!cache.exists());
    assert!(download.exists());
    assert_eq!(app.lib.storage_used(), (0, 8));
    app.player.shutdown();
}

#[test]
fn equalizer_shortcut_returns_to_music_or_radio_without_replacing_playback() {
    let mut app = fixture();
    for radio in [false, true] {
        app.stack.truncate(1);
        app.status.queue = Arc::new(vec![subsonic::Song {
            id: "synthetic-track".into(), title: "Synthetic track".into(),
            radio_url: radio.then(|| "https://example.invalid/radio".into()), ..Default::default()
        }]);
        app.status.state = PlayState::Playing;
        let queue = app.status.queue.clone();
        app.apply(Action::NowPlaying);
        step(&mut app, Event::CenterLong);
        step(&mut app, Event::Scroll(1));
        step(&mut app, Event::Center);
        assert_eq!(title(&app), "Equalizer");
        step(&mut app, Event::Back);
        assert!(app.stack.last().unwrap().1.is_now_playing());
        assert!(Arc::ptr_eq(&app.status.queue, &queue));
        assert_eq!(app.status.state, PlayState::Playing);
    }
}

#[test]
fn dark_screen_waits_for_future_deadlines_only() {
    let mut app = fixture();
    app.screen_on = false;
    app.config.power.sleep_minutes = 1;
    let now = Instant::now();
    // Both deadlines passed without effect (sleep blocked, save failed):
    // wait for the regular tick, not 50 ms in a loop.
    app.last_input = now - Duration::from_secs(120);
    app.config_save = Some(now - Duration::from_secs(10));
    assert!(app.next_wake() >= IDLE_TICK - Duration::from_secs(1), "{:?}", app.next_wake());
    // A deadline still ahead is honoured.
    app.last_input = now - Duration::from_secs(55);
    let d = app.next_wake();
    assert!(d > Duration::from_secs(3) && d < Duration::from_secs(6), "{d:?}");
}

#[test]
fn about_links_to_system_information() {
    let mut app = fixture();
    page(&mut app, Page::About);
    step(&mut app, Event::Scroll(1)); // System Information, then Report a Problem.
    step(&mut app, Event::Scroll(-1));
    step(&mut app, Event::Center);
    assert_eq!(title(&app), "System Information");
    step(&mut app, Event::Back);
    assert_eq!(title(&app), "About");
}

#[test]
fn system_information_scrolls_content_immediately_and_keeps_about_focus() {
    fn preview(app: &mut App, name: &str) -> Vec<u32> {
        let frame = app.render();
        let pixels: Vec<u8> = frame.pixels.iter().flat_map(|p|
            [(*p >> 16) as u8, (*p >> 8) as u8, *p as u8]).collect();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../../build/perf/read-only-scroll-{name}.png"));
        image::save_buffer(path, &pixels, crate::gfx::WIDTH as u32,
            crate::gfx::HEIGHT as u32, image::ColorType::Rgb8).unwrap();
        frame.pixels.clone()
    }
    let mut app = fixture();
    page(&mut app, Page::About);
    step(&mut app, Event::Scroll(1));
    step(&mut app, Event::Scroll(-1));
    step(&mut app, Event::Center);
    assert_eq!(title(&app), "System Information");
    let id = app.top_id();
    app.with_screen(id, |screen, ctx| screen.loaded(0, Box::new(crate::sysinfo::SystemInfo {
        os: Some("Generated fixture OS".into()),
        kernel: Some("7.3.0-fixture".into()), kernel_build: Some("#1 Generated Test Build".into()),
        arch: Some("mips".into()), model: Some("Synthetic Q2".into()),
        soc: Some("Ingenic X2000 fixture".into()), cpu: Some("Synthetic XBurst CPU".into()),
        cores: 2, memory_bytes: Some(128 << 20), card_bytes: Some(64 << 30),
    }), ctx));
    let top = preview(&mut app, "top");
    step(&mut app, Event::Scroll(1));
    let moved = preview(&mut app, "one-detent");
    let width = crate::gfx::WIDTH as usize;
    // Inspect content away from the header, scrollbar and rounded edges.
    // One detent must translate the text immediately, rather than selecting
    // another invisible row while the viewport remains at the top.
    let mut changed = 0;
    for y in 80..220usize {
        for x in 40..280usize {
            changed += usize::from(top[y * width + x] != moved[y * width + x]);
            assert_eq!(moved[y * width + x], top[(y + 18) * width + x]);
        }
    }
    assert!(changed > 100, "the content itself moved on the first detent");
    step(&mut app, Event::Center);
    assert_eq!(title(&app), "System Information", "there is no control to activate");
    assert_eq!(preview(&mut app, "center"), moved, "Centre does not reset the reading position");
    step(&mut app, Event::Scroll(i32::MAX));
    let bottom = preview(&mut app, "bottom");
    step(&mut app, Event::Scroll(1));
    assert_eq!(app.render().pixels, bottom, "the viewport stops at the end");
    step(&mut app, Event::Scroll(i32::MIN));
    assert_eq!(app.render().pixels, top, "reverse scrolling reaches the original top");
    step(&mut app, Event::Back);
    assert_eq!(title(&app), "About");
    step(&mut app, Event::Center);
    assert_eq!(title(&app), "System Information", "About keeps its actionable link focused");
}

#[test]
fn sound_long_press_has_no_speaker_action_and_preserves_short_actions() {
    let mut app = fixture();
    page(&mut app, Page::Sound);
    let before = title(&app);
    let id = app.top_id();
    let action = app.with_screen(id, |screen, ctx| screen.event(Event::CenterLong, ctx));
    assert!(matches!(action, Some(Action::None)), "Sound hold has no hidden action");
    assert!(app.pending.is_empty(), "no speaker confirmation toast");
    assert_eq!(title(&app), before);
    assert!(app.config.display.clicker, "navigation clicker remains independent");
    step(&mut app, Event::Center);
    assert_ne!(title(&app), before, "ordinary Sound row action still opens");
    page(&mut app, Page::Display);
    let count = app.stack.len();
    step(&mut app, Event::CenterLong);
    assert_eq!(app.stack.len(), count);
}

#[test]
fn headphone_volume_controls_accumulate_and_enforce_saved_limit() {
    let mut app = fixture();
    app.config.playback.volume = 73;
    app.config.playback.max_volume = 80;
    assert_eq!(app.output_volume(), 73);
    app.change_volume(2);
    app.change_volume(3);
    assert_eq!(app.output_volume(), 78, "rapid steps accumulate locally");
    assert_eq!(app.config.playback.volume, 78);
    assert!(app.config_save.is_some());
    app.change_volume(i32::MAX);
    assert_eq!(app.output_volume(), 80, "headphone limit remains enforced");
    app.change_volume(i32::MIN);
    assert_eq!(app.output_volume(), 0);
}

#[test]
fn night_schedule_tints_the_complete_render_without_accumulating_or_extra_dark_wakes() {
    let mut app = fixture();
    // Disable changing clocks/artwork; all source pixels are a generated menu.
    app.config.display.night_warmth = true;
    page(&mut app, Page::Display);
    let neutral = app.render().pixels.clone();
    let start = Instant::now();
    let mut night = night::Night::new();
    // Start with ordinary ticks, then enable during the day: no visible change.
    assert!(!night.tick_at(false, 12 * 3600, start));
    assert!(!night.tick_at(true, 12 * 3600, start + Duration::from_secs(60)));
    app.night = night;
    assert_eq!(app.render().pixels, neutral);
    // Enabling during the scheduled night takes a minute, with an intermediate
    // rendered frame; schedule and pixel operations use the production path.
    assert!(app.night.tick_at(true, 22 * 3600, start + Duration::from_secs(90)));
    let halfway = app.render().pixels.clone();
    assert!(app.night.tick_at(true, 22 * 3600, start + Duration::from_secs(120)));
    let warm = app.render().pixels.clone();
    assert_eq!(app.render().pixels, warm, "rendering never tints the previous frame again");
    let mut preview = Vec::with_capacity((WIDTH * HEIGHT * 9) as usize);
    for y in 0..HEIGHT as usize {
        for frame in [&neutral, &halfway, &warm] {
            for &p in &frame[y * WIDTH as usize..(y + 1) * WIDTH as usize] {
                preview.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, p as u8]);
            }
        }
    }
    let preview_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../build/perf/night-warmth-generated.png");
    image::save_buffer(preview_path, &preview, (WIDTH * 3) as u32, HEIGHT as u32,
        image::ColorType::Rgb8).unwrap();
    let mut changed_header = 0;
    let mut changed_content = 0;
    for (i, ((&cold, &half), &full)) in neutral.iter().zip(&halfway).zip(&warm).enumerate() {
        assert_eq!(cold & 0xff0000, full & 0xff0000, "red is unchanged");
        for shift in [0, 8] {
            assert!(((cold >> shift) & 255) >= ((half >> shift) & 255));
            assert!(((half >> shift) & 255) >= ((full >> shift) & 255));
        }
        if cold != full {
            if i < (WIDTH * STATUS_H) as usize { changed_header += 1; }
            else { changed_content += 1; }
        }
    }
    assert!(changed_header > 100 && changed_content > 100, "tint covers header and content");
    // A long dark interval uses elapsed time on the first normal wake. No new
    // night timer or periodic dark-screen polling is introduced.
    app.screen_on = false;
    app.config.power.sleep_minutes = 0;
    app.config_save = None;
    assert!(app.next_wake() >= IDLE_TICK - Duration::from_secs(1));
    app.screen_on = true;
    assert!(app.night.tick_at(true, 12 * 3600, start + Duration::from_secs(8 * 3600)));
    assert_eq!(app.render().pixels, neutral, "a daytime wake removes the overnight tint");
    assert!(app.night.tick_at(true, 22 * 3600, start + Duration::from_secs(8 * 3600 + 60)));
    assert!(app.night.tick_at(false, 22 * 3600, start + Duration::from_secs(8 * 3600 + 120)));
    assert_eq!(app.render().pixels, neutral, "turning off restores original pixels exactly");
}

/// The HTTP fixture establishes a real authenticated account namespace. UI
/// completions remain deterministic; it never supplies playable owner media.
struct SpokenNavigationServer {
    url: String,
    stop: Arc<std::sync::atomic::AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}
impl SpokenNavigationServer {
    fn new() -> Self {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done = stop.clone();
        let worker = std::thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(1));
                        continue;
                    }
                    Err(error) => panic!("{error}"),
                };
                stream.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
                let mut request = Vec::new();
                while request.len() < 16384 && !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    if stream.read_exact(&mut byte).is_err() { break; }
                    request.extend(byte);
                }
                let request = String::from_utf8_lossy(&request);
                let (status, body) = if request.starts_with("GET /api/me ") {
                    ("200 OK", r#"{"id":"generated-navigation-user"}"#)
                } else if request.starts_with("GET /api/libraries ") {
                    ("200 OK", r#"{"libraries":[]}"#)
                } else { ("404 Not Found", "{}") };
                let _ = write!(stream, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        });
        Self { url, stop, worker: Some(worker) }
    }
}
impl Drop for SpokenNavigationServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.worker.take().unwrap().join().unwrap();
    }
}

#[derive(Clone)]
struct SpokenNavigationList {
    remote: bool,
    episode: bool,
    namespace: String,
}
impl screens::menu::Source for SpokenNavigationList {
    fn title(&self) -> String { if self.episode { "Generated Episodes" } else { "Generated Books" }.into() }
    fn local(&self) -> bool { true }
    fn load(self: Arc<Self>, _: Arc<Library>, _: crate::library::Policy) -> screens::menu::RowsTask {
        screens::menu::ready(Ok((0..30).map(|index| {
            let id = format!("generated-item-{index}");
            let source = if self.remote || self.episode { subsonic::SpokenSource::Audiobookshelf {
                namespace:self.namespace.clone(), item:id.clone(), episode:self.episode.then(||"generated-episode".into()), content_url:"api/generated/audio".into()
            }} else { subsonic::SpokenSource::Local {relative_path:"generated.m4b".into()} };
            let song=subsonic::Song {id:id.clone(),title:format!("Generated Title {index}"),spoken:Some(Box::new(subsonic::Spoken {
                id:id.clone(),duration_ms:10000,source,..Default::default()
            })),..Default::default()};
            list::Row::new(song.title.clone(),list::RowAction::Cmd(1)).key(id).song(song)
        }).collect()))
    }
    fn cmd(&self, _:u32, row:&list::Row, ctx:&mut Ctx) -> Action {
        if self.remote {
            Action::Push(Box::new(screens::spoken::Opening::shelf(row.key.clone(),self.episode.then(||"generated-episode".into()),row.title.clone())))
        } else {
            screens::spoken::SpokenPage(if self.episode {screens::spoken::Page::DownloadedPodcasts} else {screens::spoken::Page::Local}).cmd(1,row,ctx)
        }
    }
}

#[test]
fn spoken_playback_back_returns_to_the_original_selected_list_after_success_and_retry() {
    use md5::{Digest, Md5};
    let server=SpokenNavigationServer::new();
    let mut app=fixture();
    app.config.audiobookshelf.url=server.url.clone();
    app.config.audiobookshelf.token="generated-navigation-token".into();
    app.lib.configure(&app.config);
    app.lib.shelf_libraries(crate::library::shelf_browse::Media::Book,crate::library::Policy::NetworkOnly).unwrap();
    let namespace=format!("{:x}",Md5::digest(format!("{}/|generated-navigation-user",server.url)));
    for (remote,episode) in [(false,false),(false,true),(true,false),(true,true)] {
        for (fail_first, shortcut) in [(false,false),(false,true),(true,false),(true,true)] {
            app.stack.truncate(1);
            app.status.queue=Arc::new(vec![subsonic::Song {id:"retained-generated-song".into(),..Default::default()}]);
            app.status.index=0;
            let generation=app.player.status().generation;
            app.player.send(Cmd::Restore {songs:std::sync::Arc::new(app.status.queue.as_ref().clone()),index:0,position_ms:0});
            let deadline=Instant::now()+Duration::from_secs(2);
            while Instant::now()<deadline {
                let status=app.player.status();
                if status.generation!=generation && status.current().is_some_and(|song|song.id=="retained-generated-song") && status.state==PlayState::Paused {break;}
                std::thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(app.player.status().current().unwrap().id,"retained-generated-song");
            let source=SpokenNavigationList {remote,episode,namespace:namespace.clone()};
            let songs=screens::menu::load_now(&(Arc::new(source.clone()) as Arc<dyn screens::menu::Source>),&app.lib,crate::library::Policy::CacheOnly).unwrap();
            let expected=songs[17].song_item().cloned().unwrap();
            app.push(Box::new(screens::menu::Menu::new(source)));
            step(&mut app,Event::Scroll(17));
            let list_id=app.top_id();
            step(&mut app,Event::Center);
            if !remote && !episode {
                assert_eq!(title(&app),"Book Info");
                assert_eq!(app.status.current().unwrap().id,"retained-generated-song","the first book click only reads metadata");
                let preview=app.top_id();
                app.handle(UiMsg::Loaded {screen:preview,token:1,data:Box::new(screens::book_info::Loaded {result:Ok(crate::library::book_info::BookInfo {title:"Generated Title 17".into(),..Default::default()}),configuration:None,synced:None})},&mut crate::platform::Headless {last:None});
                step(&mut app,Event::Center);
            }
            assert_eq!(title(&app),"Generated Title 17");
            let opening=app.top_id();
            let mut token=1;
            let configuration=remote.then(||app.lib.shelf_generation());
            if fail_first {
                app.handle(UiMsg::Loaded {screen:opening,token,data:Box::new(screens::spoken::Opened {songs:Err("Generated retry failure".into()),configuration:configuration.clone()})},&mut crate::platform::Headless {last:None});
                assert_eq!(app.top_id(),opening,"errors stay on the retry screen");
                step(&mut app,Event::Center);
                token=2;
            }
            if shortcut {
                step(&mut app,Event::BackLong);
                assert!(app.stack.last().unwrap().1.is_now_playing());
                assert!(app.stack.iter().any(|(id,_)|*id==opening),"pending Opening is below the temporary playback shortcut");
            }
            app.handle(UiMsg::Loaded {screen:opening,token,data:Box::new(screens::spoken::Opened {songs:Ok(vec![expected]),configuration:configuration.clone()})},&mut crate::platform::Headless {last:None});
            assert!(app.stack.last().unwrap().1.is_now_playing());
            assert!(!app.stack.iter().any(|(id,_)|*id==opening),"completed Opening cannot remain below Now Playing, including shortcut={shortcut}");
            assert_eq!(app.stack.iter().filter(|(_,screen)|screen.is_now_playing()).count(),1,"completion reuses an existing playback shortcut");
            step(&mut app,Event::Back);
            assert_eq!(app.top_id(),list_id);
            let actual=app.stack.last().unwrap().1.audit();
            assert_eq!(actual.rows()[actual.selected()].key,"generated-item-17","Back restores the exact selected book/episode, including a retained resume");
        }
    }
    app.player.shutdown();
}

#[test]
fn cancelled_spoken_opening_cannot_reappear_or_open_playback_after_return() {
    let mut app=fixture();
    app.push(Box::new(screens::menu::Menu::new(SpokenNavigationList {remote:true,episode:true,namespace:String::new()})));
    step(&mut app,Event::Scroll(9));
    let list_id=app.top_id();
    step(&mut app,Event::Center);
    let opening=app.top_id();
    step(&mut app,Event::Back);
    app.handle(UiMsg::Loaded {screen:opening,token:1,data:Box::new(screens::spoken::Opened {songs:Ok(vec![subsonic::Song::default()]),configuration:None})},&mut crate::platform::Headless {last:None});
    assert_eq!(app.top_id(),list_id);
    assert!(!app.stack.last().unwrap().1.is_now_playing());
    step(&mut app,Event::Center);
    assert_eq!(title(&app),"Generated Title 9");
    app.player.shutdown();
}

#[test]
fn stale_remote_spoken_reply_cannot_replace_a_retained_play_queue() {
    let mut app=fixture();
    app.player.send(Cmd::Restore {songs:std::sync::Arc::new(vec![subsonic::Song {id:"retained-generated-music".into(),..Default::default()}]),index:0,position_ms:700});
    let configuration = app.lib.shelf_generation();
    app.push(Box::new(screens::spoken::Opening::shelf("old-generated-item".into(), None, "Old generated book".into())));
    let screen=app.top_id();
    let stale=vec![subsonic::Song {id:"old-generated-physical-file".into(),spoken:Some(Box::new(subsonic::Spoken {id:"old-account-book".into(),duration_ms:10000,
        source:subsonic::SpokenSource::Audiobookshelf {namespace:"old-account".into(),item:"old-generated-item".into(),episode:None,content_url:"api/items/old/file/a".into()},..Default::default()})),..Default::default()}];
    app.handle(UiMsg::Lib(crate::library::LibEvent::SpokenChanged),&mut crate::platform::Headless {last:None});
    app.handle(UiMsg::Loaded {screen,token:1,data:Box::new(screens::spoken::Opened {songs: Ok(stale), configuration: Some(configuration)})},&mut crate::platform::Headless {last:None});
    app.player.send(Cmd::SetVolume(99));
    for _ in 0..1000 {if app.player.status().volume==99{break;}std::thread::sleep(Duration::from_millis(1));}
    let status=app.player.status();assert_eq!(status.volume,99);assert_eq!(status.current().unwrap().id,"retained-generated-music");
    assert_eq!(app.stack.last().unwrap().1.title(),"Old generated book");
    app.player.shutdown();
}

#[test]
fn text_return_cancels_nonempty_edits_and_explicit_delete_and_save_remain_available() {
    use screens::text::{Kind, TextEntry};
    let mut app = fixture();
    for kind in [Kind::Search, Kind::Full, Kind::Url] {
        for secret in [false, true] {
            for initial in ["", "Generated initial", "音楽 é"] {
                let saved = Arc::new(std::sync::Mutex::new(Vec::new()));
                let sink = saved.clone();
                app.push(Box::new(TextEntry::new("Generated editor", initial, kind, secret,
                    Box::new(move |s, _| { sink.lock().unwrap().push(s); Action::Pop }))));
                let depth = app.stack.len();
                step(&mut app, Event::Center); // Type a character.
                step(&mut app, Event::Back);
                assert_eq!(app.stack.len(), depth - 1);
                assert!(saved.lock().unwrap().is_empty(), "cancel never calls save");
            }
        }
    }
    let saved = Arc::new(std::sync::Mutex::new(Vec::new()));
    for long in [false, true] {
        let sink = saved.clone();
        app.push(Box::new(TextEntry::new("Generated editor", "é音", Kind::Search, false,
            Box::new(move |s, _| { sink.lock().unwrap().push(s); Action::Pop }))));
        step(&mut app, Event::Scroll(-2)); // First character -> Delete.
        step(&mut app, Event::Center); // Delete one Unicode character.
        if long { step(&mut app, Event::CenterLong); }
        else { step(&mut app, Event::Scroll(-1)); step(&mut app, Event::Center); }
    }
    assert_eq!(*saved.lock().unwrap(), ["é", "é"]);
}

#[test]
fn spoken_settings_live_under_playback_and_cancelled_server_edit_is_unchanged() {
    let mut app = fixture();
    page(&mut app, Page::Playback);
    select_label(&mut app, "Book Skip Buttons");
    assert_eq!(title(&app), "Playback");
    let before = app.config.spoken.skip_chapter;
    step(&mut app, Event::Center);
    assert_eq!(app.config.spoken.skip_chapter, !before);
    page(&mut app, Page::Audiobookshelf);
    let original = app.config.audiobookshelf.url.clone();
    step(&mut app, Event::Center);
    assert_eq!(title(&app), "Audiobookshelf Address");
    step(&mut app, Event::Center);
    step(&mut app, Event::Back);
    assert_eq!(title(&app), "Audiobookshelf");
    assert_eq!(app.config.audiobookshelf.url, original);
}

#[test]
fn playback_defaults_are_flat_and_music_queue_controls_have_one_contextual_home() {
    let mut app = fixture();
    app.player.shutdown();
    let (player, commands) = PlayerHandle::fixture(Status::default());
    app.player = player;
    page(&mut app, Page::Root);
    select_label(&mut app, "Playback"); step(&mut app, Event::Center);
    let rows = app.stack.last().unwrap().1.audit();
    assert!(rows.rows().iter().all(|row| !["Shuffle", "Repeat", "Books & Podcasts"].contains(&row.title.as_str())));
    for label in ["Default Spoken Speed", "Skip Forward", "Skip Backward"] {
        select_label(&mut app, label);
        let parent = app.top_id();
        step(&mut app, Event::Center);
        assert_eq!(app.stack.len(), 4, "Q2 → Settings → Playback → picker; no spoken grouping layer");
        let before = app.config.spoken;
        if label != "Default Spoken Speed" { step(&mut app, Event::Scroll(1)); }
        step(&mut app, Event::Back);
        assert_eq!(app.top_id(), parent);
        assert_eq!(app.config.spoken, before, "cancelled choices do not alter independent defaults");
    }
    app.status.queue = Arc::new(vec![subsonic::Song { id: "generated-context-song".into(), ..Default::default() }]);
    app.player.fixture_status(app.status.clone());
    app.apply(Action::NowPlaying);
    let playback = app.top_id();
    let queue = app.status.queue.clone();
    for (before, after) in [(false, true), (true, false)] {
        app.status.shuffle = before;
        step(&mut app, Event::CenterLong);
        select_label(&mut app, if before { "Shuffle: Songs" } else { "Shuffle: Off" });
        while commands.try_recv().is_ok() {}
        step(&mut app, Event::Center);
        assert!(matches!(commands.try_recv().unwrap(), Cmd::SetShuffle(value) if value == after));
        assert_eq!(app.top_id(), playback);
        assert!(Arc::ptr_eq(&app.status.queue, &queue));
    }
    use crate::audio::player::Repeat;
    for (before, after, label) in [(Repeat::Off, Repeat::One, "Repeat: Off"), (Repeat::One, Repeat::All, "Repeat: One"), (Repeat::All, Repeat::Off, "Repeat: All")] {
        app.status.repeat = before;
        step(&mut app, Event::CenterLong); select_label(&mut app, label);
        while commands.try_recv().is_ok() {}
        step(&mut app, Event::Center);
        assert!(matches!(commands.try_recv().unwrap(), Cmd::SetRepeat(value) if value == after));
        assert_eq!(app.top_id(), playback);
    }
    step(&mut app, Event::CenterLong);
    while commands.try_recv().is_ok() {}
    step(&mut app, Event::Back);
    assert!(commands.try_recv().is_err(), "Back cancels the context menu without changing queue controls");
    app.player.shutdown();
}

fn settle_usb_probe(app: &mut Fixture) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while !app.stack.last().unwrap().1.script_ready() {
        let message = recv_within(&mut app.inbox, deadline.saturating_duration_since(Instant::now())).expect("USB availability completes");
        app.handle(message, &mut crate::platform::Headless { last: None });
    }
}

#[test]
fn held_usb_availability_keeps_input_live_and_back_discards_its_reply() {
    let mut app = fixture();
    let root = app.paths.root.clone();
    let (entered, observed) = std::sync::mpsc::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let wait = std::sync::Mutex::new(Some(wait));
    app.usb_drive = crate::usb::UsbDrive::fixture(&root).with_availability_probe(move || {
        assert!(crate::rt::on_pool() && !crate::rt::on_executor(), "USB sysfs discovery belongs to a finite worker");
        if let Some(wait) = wait.lock().unwrap().take() {
            let _ = entered.send(());
            let _ = wait.recv_timeout(Duration::from_secs(3));
        }
    });
    crate::rt::Ui::new().unwrap().block_on(async {
        page(&mut app, Page::Connections);
    });
    let old = app.top_id();
    let reached = observed.recv_timeout(Duration::from_secs(3));
    let pulse = crate::rt::Ui::new().unwrap().block_on(async {
        tokio::task::yield_now().await;
        app.event_async(Event::Scroll(1), &mut crate::platform::Headless { last: None }).await;
        app.event_async(Event::Back, &mut crate::platform::Headless { last: None }).await;
        app.top_id() != old
    });
    let _ = release.send(());
    reached.unwrap();
    assert!(pulse, "Back must not wait for sysfs discovery");
    let now = app.top_id();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let message = recv_within(&mut app.inbox, deadline.saturating_duration_since(Instant::now())).expect("retained USB reply");
        let old_reply = matches!(&message, UiMsg::Loaded { screen, .. } if *screen == old);
        app.handle(message, &mut crate::platform::Headless { last: None });
        if old_reply { break; }
    }
    assert_eq!(app.top_id(), now, "late availability cannot reopen the departed screen");
    assert!(!app.pending.iter().any(|request| matches!(request, Request::UsbDrive)));
}

#[test]
fn usb_activation_requires_unchanged_selection_and_context_until_reply() {
    use std::os::unix::fs::PermissionsExt;
    for navigation in ["stay", "selection", "deeper"] {
        let mut app = fixture();
        let root = app.paths.root.clone();
        std::fs::create_dir_all(root.join("udc/test")).unwrap();
        std::fs::write(root.join("udc/test/state"), "configured").unwrap();
        std::fs::write(root.join("helper"), "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(root.join("helper"), std::fs::Permissions::from_mode(0o700)).unwrap();
        let (entered, observed) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let wait = std::sync::Mutex::new(Some(wait));
        let reads = Arc::new(AtomicUsize::new(0));
        let worker_reads = reads.clone();
        app.usb_drive = crate::usb::UsbDrive::fixture(&root).with_availability_probe(move || {
            if worker_reads.fetch_add(1, Ordering::SeqCst) == 1 {
                let _ = entered.send(());
                if let Some(wait) = wait.lock().unwrap().take() {
                    let _ = wait.recv_timeout(Duration::from_secs(3));
                }
            }
        });
        page(&mut app, Page::Connections);
        settle_usb_probe(&mut app);
        select_label(&mut app, "USB Drive");
        let connections = app.top_id();
        step(&mut app, Event::Center);
        let reached = observed.recv_timeout(Duration::from_secs(3));
        for _ in 0..8 { step(&mut app, Event::Center); }
        let admitted_reads = reads.load(Ordering::SeqCst);
        match navigation {
            "selection" => select_label(&mut app, "Offline Mode"),
            "deeper" => { select_label(&mut app, "Wi-Fi"); step(&mut app, Event::Center); }
            _ => {}
        }
        let destination = app.top_id();
        let _ = release.send(());
        reached.unwrap();
        assert_eq!(admitted_reads, 2, "repeated Center must retain one activation read");
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let message = recv_within(&mut app.inbox, deadline.saturating_duration_since(Instant::now())).expect("USB activation reply");
            let activation = matches!(&message, UiMsg::Loaded { screen, token: 2, .. } if *screen == connections);
            app.handle(message, &mut crate::platform::Headless { last: None });
            if activation { break; }
        }
        assert_eq!(app.top_id(), destination);
        assert_eq!(app.pending.iter().any(|request| matches!(request, Request::UsbDrive)), navigation == "stay",
            "only unchanged USB intent may request handoff after availability completes");
        assert_eq!(reads.load(Ordering::SeqCst), 2);
    }
}

#[test]
fn usb_menu_tracks_computer_hotplug_and_rejects_stale_activation() {
    use std::os::unix::fs::PermissionsExt;
    let mut app = fixture();
    let root = app.paths.root.clone();
    let state = root.join("udc/test/state");
    std::fs::create_dir_all(state.parent().unwrap()).unwrap();
    std::fs::write(root.join("helper"), "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(root.join("helper"), std::fs::Permissions::from_mode(0o700)).unwrap();
    app.usb_drive = crate::usb::UsbDrive::fixture(&root);
    page(&mut app, Page::Connections);
    settle_usb_probe(&mut app);
    for index in 0..128 {
        let connected = index % 3 == 1;
        std::fs::write(&state, if connected { "configured" } else { "powered" }).unwrap();
        app.input_time = Some(Instant::now() + Duration::from_secs(3 * (index + 1)));
        let id = app.top_id();
        app.with_screen(id, |screen, ctx| { screen.tick(ctx); });
        settle_usb_probe(&mut app);
        // Without the USB row the last row is Offline Mode.
        step(&mut app, Event::Scroll(99));
        step(&mut app, Event::Center);
        settle_usb_probe(&mut app);
        if connected {
            assert_eq!(app.pending.pop(), Some(Request::UsbDrive));
        } else {
            assert!(app.pending.is_empty(),"unattached USB cannot request an export");
            assert!(!app.stack.last().unwrap().1.audit().rows().iter().any(|row|row.key=="usb-drive"));
        }
        assert_eq!(title(&app), "Connections");
    }
    std::fs::write(&state, "configured").unwrap();
    page(&mut app, Page::Connections);
    settle_usb_probe(&mut app);
    step(&mut app, Event::Scroll(99));
    std::fs::write(&state, "not attached").unwrap();
    step(&mut app, Event::Center);
    settle_usb_probe(&mut app);
    assert!(!app.pending.iter().any(|request| matches!(request, Request::UsbDrive)), "fresh activation must reject disconnect");
}

#[test]
fn audiobookshelf_uses_global_transport_without_a_second_route_selector() {
    let mut app=fixture();
    for enabled in [false,true] {
        app.config.tailscale.enabled=enabled;page(&mut app,Page::Audiobookshelf);
        let actual=app.stack.last().unwrap().1.audit();
        assert!(!actual.rows().iter().any(|row|row.title=="Connection"));
        step(&mut app,Event::Back);
        assert_eq!(app.config.tailscale.enabled,enabled);
    }
    app.player.shutdown();
}

#[test]
fn usb_export_never_discards_unsaved_edits_after_repeated_save_failures() {
    let mut app = fixture();
    let root = app.paths.root.clone();
    let installed = Paths::installed(root.join("internal"), root.join("card"), root.join("run"), true, None);
    let blocking = crate::rt::Blocking::dedicated_thread();
    installed.create_all(&blocking).unwrap();
    installed.load_config(&blocking);
    // The card's settings changed on the host during this run: the save is
    // refused, without any private data.
    let mut host = crate::config::Config::default();
    host.display.brightness = 7;
    host.save(&blocking, &installed.config()).unwrap();
    app.paths = installed;
    app.config.display.brightness = 42;
    app.config_save = Some(Instant::now());
    for _ in 0..2 {
        app.open_usb_drive();
        settle_settings(&mut app);
        assert!(app.config_save_failed, "failed save remains dirty for every explicit retry");
        assert!(app.config_save.is_none(), "permanent conflict does not introduce an idle retry timer");
        assert!(!app.usb_drive.busy(), "helper must not launch after failed save");
        assert_eq!(app.config.display.brightness, 42);
    }
}

#[test]
fn spoken_forward_and_backward_choices_are_independent() {
    let mut app = fixture();
    page(&mut app, Page::Playback);
    assert_eq!(app.config.spoken.skip_forward_seconds, 30);
    assert_eq!(app.config.spoken.skip_backward_seconds, 15);
    select_label(&mut app, "Skip Forward"); step(&mut app, Event::Center);
    assert_eq!(title(&app), "Skip Forward");
    step(&mut app, Event::Scroll(1)); step(&mut app, Event::Center);
    assert_eq!(app.config.spoken.skip_forward_seconds, 45);
    assert_eq!(app.config.spoken.skip_backward_seconds, 15);
    step(&mut app, Event::Scroll(1)); step(&mut app, Event::Center);
    assert_eq!(title(&app), "Skip Backward");
    step(&mut app, Event::Scroll(1)); step(&mut app, Event::Center);
    assert_eq!(app.config.spoken.skip_forward_seconds, 45);
    assert_eq!(app.config.spoken.skip_backward_seconds, 30);
}

#[test]
fn book_info_popup_returns_to_playback_without_replacing_queue() {
    let mut app = fixture();
    for episode in [false, true] {
        app.stack.truncate(1);
        let source = if episode { subsonic::SpokenSource::Audiobookshelf {
            namespace: "generated-info-account".into(), item: "generated-info-book".into(),
            episode: Some("generated-info-episode".into()), content_url: String::new(),
        } } else { subsonic::SpokenSource::Local { relative_path: "generated-info.m4b".into() } };
        app.status.queue = Arc::new(vec![subsonic::Song {
            id: "generated-info".into(), title: "Generated Info".into(),
            spoken: Some(Box::new(subsonic::Spoken { source, ..Default::default() })), ..Default::default()
        }]);
        app.status.state = PlayState::Playing;
        let queue = app.status.queue.clone();
        app.apply(Action::NowPlaying);
        step(&mut app, Event::CenterLong);
        select_label(&mut app, if episode { "Episode Info" } else { "Book Info" });
        step(&mut app, Event::Center);
        assert_eq!(title(&app), if episode {"Episode Info"} else {"Book Info"});
        step(&mut app, Event::Back);
        assert!(app.stack.last().unwrap().1.is_now_playing());
        assert!(Arc::ptr_eq(&app.status.queue, &queue));
        assert_eq!(app.status.state, PlayState::Playing);
    }
    app.player.shutdown();
}

#[test]
fn default_spoken_speed_visits_all_fifty_one_values_and_saves_live() {
    let mut app = fixture();
    page(&mut app, Page::Playback);
    select_label(&mut app, "Default Spoken Speed");
    step(&mut app, Event::Center);
    assert_eq!(title(&app), "Default Speed");
    step(&mut app, Event::Scroll(i32::MIN));
    for expected in (50..=300u16).step_by(5) {
        assert_eq!(app.config.spoken.speed_percent, expected);
        assert_eq!(crate::audio::player::TrackSource::spoken_skip(&crate::library::Tracks(app.lib.clone())).speed_percent, expected,
            "published library default follows the live UI before refresh");
        if expected < 300 { step(&mut app, Event::Scroll(1)); }
    }
    step(&mut app, Event::Scroll(i32::MAX));
    assert_eq!(app.config.spoken.speed_percent, 300);
    for expected in (50..=300u16).step_by(5).rev() {
        assert_eq!(app.config.spoken.speed_percent, expected);
        if expected > 50 { step(&mut app, Event::Scroll(-1)); }
    }
    step(&mut app, Event::Scroll(i32::MIN));
    assert_eq!(app.config.spoken.speed_percent, 50);
    step(&mut app, Event::Scroll(17));
    step(&mut app, Event::CenterLong);
    assert_eq!(app.config.spoken.speed_percent, 135, "default picker has no reset shortcut");
    step(&mut app, Event::Back);
    assert_eq!(title(&app), "Playback");
    step(&mut app, Event::Center);
    assert_eq!(title(&app), "Default Speed", "parent selection remains on the speed row");
    step(&mut app, Event::Center);
    assert!(app.save_settings());
    assert_eq!(Config::load(&crate::rt::Blocking::dedicated_thread(), &app.paths.config()).spoken.speed_percent, 135);
    app.player.shutdown();
}

#[test]
fn spoken_speed_customization_ack_reset_release_and_target_change_are_isolated() {
    let mut app = fixture();
    for episode in [false, true] {
        app.stack.truncate(1);
        app.config.spoken.speed_percent = 125;
        let source = if episode { subsonic::SpokenSource::Audiobookshelf {
            namespace: "generated-speed-account".into(), item: "generated-speed-series".into(),
            episode: Some("generated-speed-episode".into()), content_url: String::new(),
        } } else { subsonic::SpokenSource::Local { relative_path: "generated-speed.m4b".into() } };
        app.status.queue = Arc::new(vec![subsonic::Song {
            id: "generated-speed".into(), title: "Generated Speed".into(),
            spoken: Some(Box::new(subsonic::Spoken { source, ..Default::default() })), ..Default::default()
        }]);
        app.status.generation = 41;
        app.status.speed_percent = 125;
        app.status.speed_override = None;
        app.status.speed_request = 0;
        app.status.error = None;
        app.apply(Action::NowPlaying);
        step(&mut app, Event::CenterLong);
        step(&mut app, Event::Center);
        assert_eq!(title(&app), "Playback Speed");
        let picker = app.top_id();
        let request = app.spoken_speed.borrow().request_id();
        let before = app.render().pixels.clone();
        step(&mut app, Event::Scroll(1));
        assert_eq!(app.config.spoken.speed_percent, 125, "custom value never changes default");
        let custom = app.render().pixels.clone();
        assert_ne!(custom, before);
        // The player may still publish an older acknowledgement while a newer
        // request is in flight. It must not roll the live selection back.
        app.status.speed_request = request;
        app.with_screen(picker, |screen, ctx| { screen.tick(ctx); });
        assert_eq!(app.render().pixels, custom);
        app.status.speed_request = request + 1;
        app.status.speed_percent = 130;
        app.status.speed_override = Some(130);
        app.with_screen(picker, |screen, ctx| { screen.tick(ctx); });
        assert_eq!(app.render().pixels, custom);
        step(&mut app, Event::Scroll(1));
        let pending = app.render().pixels.clone();
        app.with_screen(picker, |screen, ctx| { screen.tick(ctx); });
        assert_eq!(app.render().pixels, pending, "previous successful ack cannot overwrite newer wheel value");
        app.status.speed_request = request + 2;
        app.status.error = Some("Generated speed save failure".into());
        app.with_screen(picker, |screen, ctx| { screen.tick(ctx); });
        assert_ne!(app.render().pixels, pending, "failed write restores prior effective selection and shows failure");
        // Feed the real key state machine through hold and release. The hold
        // resets live; its release must not become Centre and close the picker.
        let now = Instant::now();
        let mut events = Vec::new();
        app.keys.key(crate::input::Key::Center, true, now, &mut events);
        app.keys.tick(now + crate::input::LONG_PRESS, &mut events);
        assert_eq!(events, vec![Event::CenterLong]);
        for event in events.drain(..) { step(&mut app, event); }
        app.keys.key(crate::input::Key::Center, false, now + crate::input::LONG_PRESS, &mut events);
        assert!(events.is_empty());
        assert_eq!(app.top_id(), picker);
        assert_eq!(app.config.spoken.speed_percent, 125);
        app.status.speed_request = request + 3;
        app.status.speed_percent = 125;
        app.status.speed_override = None;
        app.status.error = None;
        app.with_screen(picker, |screen, ctx| { screen.tick(ctx); });
        assert_eq!(app.render().pixels, before, "Use Default restores the initial value and Default state");
        app.status.generation += 1;
        step(&mut app, Event::Scroll(1));
        assert!(app.stack.last().unwrap().1.is_now_playing(), "new playback instance dismisses stale editor");
        assert_eq!(app.config.spoken.speed_percent, 125);
    }
    app.player.shutdown();
}


fn generated_spoken_playback(app: &mut App, episode: bool, generation: u64) {
    app.stack.truncate(1);
    let source = if episode { subsonic::SpokenSource::Audiobookshelf {
        namespace: "generated-center-account".into(), item: "generated-center-series".into(),
        episode: Some("generated-center-episode".into()), content_url: String::new(),
    } } else { subsonic::SpokenSource::Local { relative_path: "generated-center.m4b".into() } };
    app.status.queue = Arc::new(vec![subsonic::Song {
        id: "generated-center".into(), title: "Generated Centre Speed".into(),
        spoken: Some(Box::new(subsonic::Spoken { source, ..Default::default() })), ..Default::default()
    }]);
    app.status.generation = generation;
    app.status.state = PlayState::Playing;
    app.status.duration_ms = 600_000;
    app.status.position_ms = 12_345;
    app.status.speed_percent = 125;
    app.status.speed_override = None;
    app.status.error = None;
    app.apply(Action::NowPlaying);
}

#[test]
fn spoken_center_cycle_routes_all_fifty_one_speeds_without_volume_or_default_changes() {
    let mut app = fixture();
    app.config.spoken.speed_percent = 125;
    for episode in [false, true] {
        generated_spoken_playback(&mut app, episode, 91 + u64::from(episode));
        let volume = app.config.playback.volume;
        assert!(app.stack.last().unwrap().1.wheel_is_volume());
        step(&mut app, Event::Center); // Seek.
        assert!(!app.stack.last().unwrap().1.wheel_is_volume());
        step(&mut app, Event::Center); // Speed.
        let screen = app.top_id();
        assert_eq!(app.spoken_speed.borrow().selected_percent(), 125);
        step(&mut app, Event::Scroll(i32::MIN));
        for expected in (50..=300u16).step_by(5) {
            assert_eq!(app.spoken_speed.borrow().selected_percent(), expected);
            assert_eq!(app.config.playback.volume, volume);
            assert_eq!(app.config.spoken.speed_percent, 125);
            assert_eq!(app.status.position_ms, 12_345);
            if expected < 300 { step(&mut app, Event::Scroll(1)); }
        }
        step(&mut app, Event::Scroll(i32::MAX));
        assert_eq!(app.spoken_speed.borrow().selected_percent(), 300);
        for expected in (50..=300u16).step_by(5).rev() {
            assert_eq!(app.spoken_speed.borrow().selected_percent(), expected);
            if expected > 50 { step(&mut app, Event::Scroll(-1)); }
        }
        let request = app.spoken_speed.borrow().request_id();
        step(&mut app, Event::Scroll(i32::MIN));
        assert_eq!(app.spoken_speed.borrow().request_id(), request, "saturated wheel sends no extra request");
        step(&mut app, Event::Center); // Volume, same screen.
        assert_eq!(app.top_id(), screen);
        assert!(app.stack.last().unwrap().1.wheel_is_volume());
        step(&mut app, Event::Center);
        step(&mut app, Event::Center);
        step(&mut app, Event::Back);
        assert_eq!(app.top_id(), screen, "Back in Speed returns to volume before leaving playback");
        assert!(app.stack.last().unwrap().1.wheel_is_volume());
        step(&mut app, Event::Center); step(&mut app, Event::Center);
        app.with_screen(screen, |screen, ctx| {
            ctx.now += Duration::from_secs(5);
            screen.tick(ctx);
        });
        assert!(app.stack.last().unwrap().1.wheel_is_volume(), "idle Speed returns to volume");
    }
    app.player.shutdown();
}

#[test]
fn spoken_speed_controller_survives_modes_popup_recreation_and_delayed_acknowledgements() {
    let mut app = fixture();
    generated_spoken_playback(&mut app, false, 101);
    step(&mut app, Event::Center); step(&mut app, Event::Center);
    step(&mut app, Event::Scroll(1));
    let first = app.spoken_speed.borrow().request_id();
    assert_eq!(app.spoken_speed.borrow().selected_percent(), 130);
    step(&mut app, Event::Center); // Leave before the player acknowledges.
    step(&mut app, Event::Center); step(&mut app, Event::Center);
    assert_eq!(app.spoken_speed.borrow().selected_percent(), 130, "re-entry retains pending selection");
    step(&mut app, Event::Scroll(1));
    assert_eq!(app.spoken_speed.borrow().request_id(), first + 1);
    step(&mut app, Event::CenterLong); step(&mut app, Event::Center);
    assert_eq!(title(&app), "Playback Speed");
    assert_eq!(app.spoken_speed.borrow().selected_percent(), 135, "menu and inline editor share their draft");
    step(&mut app, Event::Scroll(1));
    assert_eq!(app.spoken_speed.borrow().request_id(), first + 2);
    step(&mut app, Event::Center);
    app.stack.truncate(1); // Destroy NowPlaying, then recreate it.
    app.apply(Action::NowPlaying);
    step(&mut app, Event::Center); step(&mut app, Event::Center);
    assert_eq!(app.spoken_speed.borrow().selected_percent(), 140);
    step(&mut app, Event::Scroll(1));
    let newest = app.spoken_speed.borrow().request_id();
    assert_eq!(newest, first + 3, "requests never restart with a new view");
    let screen = app.top_id();
    for old_ack in [first + 2, first, first + 1, first + 2] {
        app.status.speed_request = old_ack;
        app.status.speed_percent = 50;
        app.status.speed_override = Some(50);
        app.with_screen(screen, |screen, ctx| { screen.tick(ctx); });
        assert_eq!(app.spoken_speed.borrow().selected_percent(), 145, "old acknowledgement cannot rewind a newer edit");
    }
    app.status.speed_request = newest;
    app.status.speed_percent = 140;
    app.status.speed_override = Some(140);
    app.status.error = Some("Generated persistence failure".into());
    app.with_screen(screen, |screen, ctx| { screen.tick(ctx); });
    assert_eq!(app.spoken_speed.borrow().selected_percent(), 140, "failed latest write restores confirmed effective speed");
    app.status.generation += 1;
    app.status.speed_percent = 100;
    app.status.speed_override = None;
    app.status.error = None;
    app.with_screen(screen, |screen, ctx| { screen.tick(ctx); });
    assert!(app.stack.last().unwrap().1.wheel_is_volume(), "target change returns to Volume");
    step(&mut app, Event::Center); step(&mut app, Event::Center);
    assert_eq!(app.spoken_speed.borrow().selected_percent(), 100, "new target discards old pending state");
    step(&mut app, Event::Scroll(1));
    assert_eq!(app.spoken_speed.borrow().request_id(), newest + 1);
    app.status.speed_request = newest + 1;
    app.status.speed_percent = 105;
    app.status.speed_override = Some(105);
    app.with_screen(screen, |screen, ctx| { screen.tick(ctx); });
    step(&mut app, Event::Center); step(&mut app, Event::Center); step(&mut app, Event::Center);
    step(&mut app, Event::Scroll(1));
    assert_eq!(app.spoken_speed.borrow().request_id(), newest + 2, "completed acknowledgements cannot cause request ID reuse on re-entry");
    app.with_screen(screen, |screen, ctx| { screen.tick(ctx); });
    assert_eq!(app.spoken_speed.borrow().selected_percent(), 110, "ack from completed prior visit cannot rewind new edit");
    app.player.shutdown();
}


#[test]
fn music_and_radio_center_controls_never_send_spoken_speed_requests() {
    let mut app = fixture();
    for radio in [false, true] {
        app.stack.truncate(1);
        app.status.queue = Arc::new(vec![subsonic::Song {
            id: "generated-music".into(), title: "Generated Music".into(),
            radio_url: radio.then(|| "http://127.0.0.1:1/generated-radio".into()), ..Default::default()
        }]);
        app.status.generation += 1;
        app.apply(Action::NowPlaying);
        let request = app.spoken_speed.borrow().request_id();
        for _ in 0..4 {
            step(&mut app, Event::Center);
            step(&mut app, Event::Scroll(1));
        }
        assert_eq!(app.spoken_speed.borrow().request_id(), request);
        if radio { assert!(app.stack.last().unwrap().1.wheel_is_volume()); }
    }
    app.player.shutdown();
}


#[test]
fn prepared_sleep_waits_for_release_and_failure_restores_visible_input() {
    let mut app = fixture();
    let mut platform = crate::platform::Headless { last: None };
    let now = Instant::now();
    app.keys.key(crate::input::Key::PlayPause, true, now, &mut Vec::new());
    app.power = q2_machines::power::Power::Preparing;
    app.handle(UiMsg::PowerPrepared { off: false, result: Ok(()) }, &mut platform);
    app.tick(&mut platform);
    assert!(app.take_requests().is_empty(), "long Play cannot suspend while held");
    app.handle(UiMsg::Input(Raw::Key { key: crate::input::Key::PlayPause, down: false }), &mut platform);
    app.tick(&mut platform);
    assert_eq!(app.take_requests(), vec![Request::EnterPower { off: false }]);
    app.tick(&mut platform);
    assert!(app.take_requests().is_empty(), "entry is issued exactly once");
    app.screen_on = false;
    app.finish_power(&mut platform, Err("Synthetic kernel refusal".into()), None);
    assert!(app.screen_on && !app.power.pending());
    assert!(!app.keys.any_held());
    assert_eq!(app.toast.as_ref().unwrap().0, "Synthetic kernel refusal");
    assert!(app.last_input.elapsed() < Duration::from_secs(1));
}


#[test]
fn idle_sleep_keeps_ten_minute_boundary_and_queued_wake_keys_cannot_navigate() {
    let mut app = fixture();
    let mut platform = crate::platform::Headless { last: None };
    assert_eq!(app.config.power.sleep_minutes, 10);
    app.screen_on = false;
    app.last_input = Instant::now() - Duration::from_secs(599);
    app.tick(&mut platform);
    assert!(!app.take_requests().contains(&Request::Sleep));
    app.last_input = Instant::now() - Duration::from_secs(601);
    app.tick(&mut platform);
    assert!(app.take_requests().contains(&Request::Sleep));
    let before = Instant::now();
    app.finish_power(&mut platform, Ok(()), None);
    let original_title = title(&app);
    for down in [true, false] {
        app.handle(UiMsg::TimedInput { raw: Raw::Key { key: crate::input::Key::Center, down }, received: before }, &mut platform);
    }
    assert_eq!(title(&app), original_title);
    assert!(!app.keys.any_held());
}


#[test]
fn ten_minute_idle_sleep_applies_on_usb_and_battery_but_not_during_playback() {
    struct Supply(bool);
    impl Platform for Supply {
        fn present<'a>(&'a mut self, _: &'a Canvas) -> crate::platform::PresentFuture<'a> { Box::pin(async { Ok(()) }) } fn set_screen(&mut self, _: bool) {}
        fn set_brightness(&mut self, _: u8) {}
        fn battery(&mut self) -> Battery { Battery { plugged: self.0, ..Default::default() } }
        fn headphones(&mut self) -> Option<bool> { None }
        fn audio_hw(&mut self) -> Box<dyn crate::audio::alsa::AudioHw> {
            Box::new(crate::audio::alsa::SoftHw(crate::audio::alsa::FAKE.into()))
        }
        fn suspend(&mut self) -> crate::platform::SuspendFuture<'_> { unreachable!() }
        fn power_off(&mut self) -> crate::platform::SuspendFuture<'_> { Box::pin(async { unreachable!() }) }
        fn is_device(&self) -> bool { true }
    }
    let mut app = fixture();
    for plugged in [false, true] {
        let mut platform = Supply(plugged);
        for playing in [false, true] {
            for seconds in [599, 601] {
                app.screen_on = false;
                app.last_input = Instant::now() - Duration::from_secs(seconds);
                app.status.state = if playing { PlayState::Playing } else { PlayState::Paused };
                app.tick(&mut platform);
                assert_eq!(app.take_requests().contains(&Request::Sleep), !playing && seconds > 600,
                    "plugged={plugged} playing={playing} idle={seconds}");
            }
        }
    }
}

#[test]
fn charging_holds_idle_sleep_until_the_last_download_finishes() {
    let mut app = fixture();
    let lib = app.lib.clone();
    let mut platform = crate::platform::Headless { last: None };
    let mut idle = |app: &mut App| {
        app.screen_on = false;
        app.battery_polled = Some(Instant::now());
        app.last_input = Instant::now() - Duration::from_secs(601);
        app.tick(&mut platform);
        app.take_requests().contains(&Request::Sleep)
    };
    let first = lib.downloads.start_transfer();
    let last = lib.downloads.start_transfer();
    app.battery = Battery::default();
    assert!(idle(&mut app));
    app.battery.charging = true;
    assert!(!idle(&mut app));
    drop(first);
    app.battery.charging = false;
    app.battery.plugged = true;
    assert!(!idle(&mut app));
    drop(last);
    assert!(idle(&mut app));
}

#[test]
fn wake_metric_waits_for_real_presentation_and_preserves_first_acquired_edge() {
    let mut app=fixture();let mut platform=crate::platform::Headless { last: None };
    app.screen_on=false;
    let acquired=Instant::now()-Duration::from_millis(25);
    app.handle(UiMsg::TimedInput {raw:Raw::Key {key:crate::input::Key::Back,down:true},received:acquired},&mut platform);
    assert!(app.screen_on);assert_eq!(app.wake_measurement,Some(acquired));
    // Taking a diagnostic screenshot is not presenting to the display.
    let _=app.render();assert_eq!(app.wake_measurement,Some(acquired));
    app.handle(UiMsg::TimedInput {raw:Raw::Key {key:crate::input::Key::Center,down:true},received:Instant::now()},&mut platform);
    assert_eq!(app.wake_measurement,Some(acquired));
    app.presentation_completed();assert!(app.wake_measurement.is_none());
    app.presentation_completed();assert!(app.wake_measurement.is_none());
}

#[test]
fn resume_metric_preserves_helper_return_boundary_until_successful_presentation() {
    let mut app = fixture();
    let mut platform = crate::platform::Headless { last: None };
    let returned = Instant::now() - Duration::from_secs(2);
    app.finish_power(&mut platform, Ok(()), Some(returned));
    assert_eq!(app.resume_measurement, Some(returned), "input reopening and UI delay must stay inside the measurement");
    let _ = app.render();
    assert_eq!(app.resume_measurement, Some(returned), "a screenshot is not presentation");
    app.presentation_completed();
    assert!(app.resume_measurement.is_none());
    app.presentation_completed();
    assert!(app.resume_measurement.is_none(), "a later frame cannot duplicate this sample");
    app.finish_power(&mut platform, Ok(()), None);
    assert!(app.resume_measurement.is_none(), "an uninstrumented backend stays unknown");
    app.finish_power(&mut platform, Err("Synthetic suspend refusal".into()), Some(returned));
    assert!(app.resume_measurement.is_none(), "refused suspend never becomes a resume sample");
    app.player.shutdown();
}

#[test]
fn shutdown_helper_failure_restores_controls_without_a_global_status_file() {
    struct FailedShutdown(Option<String>);
    impl Platform for FailedShutdown {
        fn present<'a>(&'a mut self, _: &'a Canvas) -> crate::platform::PresentFuture<'a> { Box::pin(async { Ok(()) }) } fn set_screen(&mut self, _: bool) {}
        fn set_brightness(&mut self, _: u8) {}
        fn battery(&mut self) -> Battery { Default::default() }
        fn headphones(&mut self) -> Option<bool> { None }
        fn audio_hw(&mut self) -> Box<dyn crate::audio::alsa::AudioHw> {
            Box::new(crate::audio::alsa::SoftHw(crate::audio::alsa::FAKE.into()))
        }
        fn suspend(&mut self) -> crate::platform::SuspendFuture<'_> { unreachable!() }
        fn power_off(&mut self) -> crate::platform::SuspendFuture<'_> { Box::pin(async { unreachable!() }) }
        fn power_off_error(&mut self) -> Option<String> { self.0.take() }
        fn is_device(&self) -> bool { true }
    }
    let mut app = fixture();
    app.player.prepare_sleep().unwrap();
    app.power = q2_machines::power::Power::Entering;
    app.screen_on = false;
    app.await_power_off();
    let mut platform = FailedShutdown(Some("Synthetic early helper exit".into()));
    app.tick(&mut platform);
    assert!(app.screen_on && app.power == q2_machines::power::Power::Idle);
    assert_eq!(app.toast.as_ref().unwrap().0, "Synthetic early helper exit");
    app.player.send(Cmd::SetVolume(71));
    let deadline = Instant::now() + Duration::from_secs(2);
    while app.player.status().volume != 71 {
        assert!(Instant::now() < deadline, "failed Power Off left audio commands frozen");
        std::thread::yield_now();
    }
    app.player.shutdown();
}


#[test]
fn failed_power_preparation_cannot_cancel_foreign_usb_barrier() {
    let mut app = fixture();
    let mut platform = crate::platform::Headless { last: None };
    app.player.prepare_handoff().unwrap();
    app.power = q2_machines::power::Power::Preparing;
    let result = crate::rt::Ui::new().unwrap().block_on(prepare_power_state(&app.player, &app.lib));
    assert!(result.is_err());
    app.handle(UiMsg::PowerPrepared { off: false, result }, &mut platform);
    assert!(!app.power.pending());
    assert!(app.player.prepare_sleep().is_err(), "USB still owns the original acknowledged barrier");
    app.player.cancel_handoff(); // Only its actual owner releases it.
    app.player.shutdown();
}

#[test]
fn library_failure_cancels_only_its_own_successful_power_barrier() {
    let app = fixture();
    let database = rusqlite::Connection::open(app.paths.database()).unwrap();
    database.execute_batch("CREATE TRIGGER reject_sleep_queue BEFORE INSERT ON kv WHEN NEW.key='queue' BEGIN SELECT RAISE(ABORT,'synthetic queue failure'); END;").unwrap();
    assert!(crate::rt::Ui::new().unwrap().block_on(prepare_power_state(&app.player, &app.lib)).is_err());
    assert_eq!(app.player.prepare_sleep(), Ok(()), "failed library durability must release our own barrier for retry");
    app.player.cancel_handoff();
    database.execute_batch("DROP TRIGGER reject_sleep_queue;").unwrap();
    app.player.shutdown();
}

#[test]
fn custom_studio_real_chooser_applies_all_steps_offline_and_preserves_graphic_eq() {
    let mut app=fixture();
    app.lib.set_offline_mode(true);
    app.config.playback.eq.set_preset(5);
    let bands=app.config.playback.eq.bands;
    app.push(Box::new(screens::equalizer::Equalizer::new()));
    select_label(&mut app,"Choose Headphones");step(&mut app,Event::Center);
    let deadline=Instant::now()+Duration::from_secs(5);
    while !app.stack.last().unwrap().1.script_ready() {
        let msg=crate::ui::recv_within(&mut app.inbox, deadline.saturating_duration_since(Instant::now())).expect("packaged catalog worker finishes");
        app.handle(msg,&mut crate::platform::Headless {last:None});
    }
    for n in 1..=4 {
        let name=format!("Beyerdynamic Custom Studio (step {n})");
        select_label(&mut app,&name);step(&mut app,Event::Center);
        let deadline=Instant::now()+Duration::from_secs(5);
        while !app.stack.last().unwrap().1.script_ready() {
            let msg=crate::ui::recv_within(&mut app.inbox, deadline.saturating_duration_since(Instant::now())).expect("bundled profile worker finishes");
            app.handle(msg,&mut crate::platform::Headless {last:None});
        }
        let expected=crate::audio::autoeq::search(&name).remove(0).load(&app.paths.root,true,None).unwrap();
        assert_eq!(app.config.playback.eq.headphone,Some(expected));
        assert!(app.config.playback.eq.headphone_enabled && app.config.playback.eq.graphic_enabled);
        assert_eq!(app.config.playback.eq.bands,bands);
        let surface=app.stack.last().unwrap().1.audit();
        assert_eq!(surface.rows()[surface.selected()].title,name);
        let path=app.paths.root.join("generated-profile-roundtrip.toml");
        app.config.save(&crate::rt::Blocking::dedicated_thread(), &path).unwrap();
        assert_eq!(Config::load(&crate::rt::Blocking::dedicated_thread(), &path).playback.eq,app.config.playback.eq);
        app.render();
    }
    step(&mut app,Event::Back);assert_eq!(title(&app),"Equalizer");
    step(&mut app,Event::Center);assert_eq!(title(&app),"Headphone EQ");
}

#[test]
fn offline_home_shuffle_plays_downloads_instead_of_rejecting_the_action() {
    let mut app = fixture();
    settle_home(&mut app);
    app.player.shutdown();
    let (player, commands) = PlayerHandle::fixture(Status::default());
    app.player = player;
    app.lib.set_offline_mode(true);
    for n in 0..3 {
        let path = app.paths.media_root().join(format!("generated-shuffle-{n}.wav"));
        std::fs::write(&path, b"generated fixture audio").unwrap();
        app.lib.db().put_offline_song(&crate::rt::Blocking::dedicated_thread(), &subsonic::Song {
            id: format!("generated-shuffle-{n}"), title: format!("Generated {n}"),
            suffix: Some("wav".into()), ..Default::default()
        }, &path, 23);
    }
    select_label(&mut app, "Shuffle Songs");
    step(&mut app, Event::Center);
    assert_eq!(title(&app), "Shuffle Songs", "downloaded shuffle must be available offline");
    let deadline = Instant::now() + Duration::from_secs(5);
    while title(&app) == "Shuffle Songs" {
        let message = crate::ui::recv_within(&mut app.inbox, deadline.saturating_duration_since(Instant::now())).expect("download inventory completes");
        app.handle(message, &mut crate::platform::Headless { last: None });
    }
    assert!(app.stack.last().unwrap().1.is_now_playing());
    let command = commands.recv_timeout(Duration::from_secs(5)).expect("local shuffle queues playback");
    match command {
        // UX-004: one random download starts at once; the rest queue behind it.
        Cmd::Play { songs, index, shuffle } => {
            assert!(!shuffle);
            assert_eq!(index, 0);
            assert_eq!(songs.len(), 1);
            assert!(["generated-shuffle-0", "generated-shuffle-1", "generated-shuffle-2"].contains(&songs[0].id.as_str()));
        }
        _ => panic!("expected actual Play command"),
    }
}

#[test]
fn offline_shuffle_empty_and_cancelled_scans_preserve_queue_and_home_selection() {
    let mut app = fixture(); settle_home(&mut app);
    app.player.shutdown(); let (player, commands) = PlayerHandle::fixture(Status::default()); app.player = player;
    app.lib.set_offline_mode(true);
    select_label(&mut app, "Shuffle Songs"); step(&mut app, Event::Center);
    let deadline = Instant::now() + Duration::from_secs(5);
    while title(&app) == "Shuffle Songs" {
        let message = crate::ui::recv_within(&mut app.inbox, deadline.saturating_duration_since(Instant::now())).unwrap();
        app.handle(message, &mut crate::platform::Headless { last: None });
    }
    assert_eq!(title(&app), "Q2"); assert!(commands.try_recv().is_err());
    assert_eq!(app.toast.as_ref().map(|t| t.0.as_str()), Some("No downloaded songs"));
    assert_eq!(app.stack[0].1.audit().rows()[app.stack[0].1.audit().selected()].title, "Shuffle Songs");
    step(&mut app, Event::Center); let cancelled = app.top_id(); step(&mut app, Event::Back);
    app.handle(UiMsg::Loaded {screen: cancelled, token: 1, data: Box::new(Ok::<_, String>(super::screens::sources::ShuffleSelection::Seed(Box::new(subsonic::Song {id: "generated-cancelled".into(), ..Default::default()}))))}, &mut crate::platform::Headless {last: None});
    assert_eq!(title(&app), "Q2"); assert!(commands.try_recv().is_err());
}

/// Puts a complete cached copy of `id` on the card, with its metadata.
fn cache_generated_song(app: &Fixture, id: &str) {
    let blocking = crate::rt::Blocking::dedicated_thread();
    let directory = app.lib.paths.root.join("cache-generated");
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join(format!("{id}.mp3"));
    std::fs::write(&path, b"generated audio").unwrap();
    app.lib.db().put_cached_song(&blocking, id, "raw", &path, 15).unwrap();
    app.lib.db().put_cached_song_meta(&blocking, &subsonic::Song { id: id.into(), title: id.into(), duration: 60, ..Default::default() }).unwrap();
}

fn shuffle_commands(app: &mut Fixture, commands: &std::sync::mpsc::Receiver<Cmd>) -> Vec<Cmd> {
    select_label(app, "Shuffle Songs"); step(app, Event::Center);
    let deadline = Instant::now() + Duration::from_secs(5);
    while title(app) == "Shuffle Songs" {
        let message = crate::ui::recv_within(&mut app.inbox, deadline.saturating_duration_since(Instant::now())).unwrap();
        app.handle(message, &mut crate::platform::Headless { last: None });
    }
    let mut sent = Vec::new();
    while let Ok(command) = commands.recv_timeout(Duration::from_millis(200)) { sent.push(command); }
    sent
}

/// UX-009: offline, shuffle plays downloaded and cached songs.
#[test]
fn offline_shuffle_includes_cached_songs() {
    let mut app = fixture(); settle_home(&mut app);
    app.player.shutdown(); let (player, commands) = PlayerHandle::fixture(Status::default()); app.player = player;
    for id in ["generated-cached-a", "generated-cached-b"] { cache_generated_song(&app, id); }
    app.lib.set_offline_mode(true);
    let sent = shuffle_commands(&mut app, &commands);
    let Some(Cmd::Play { songs, .. }) = sent.first() else { panic!("no play: {}", sent.len()) };
    assert_eq!(songs.len(), 1);
    assert!(["generated-cached-a", "generated-cached-b"].contains(&songs[0].id.as_str()));
    assert!(app.stack.last().unwrap().1.is_now_playing());
}

/// UX-004: the first song comes from a small random sample, checking files
/// only until one is present; a missing file is never chosen.
#[test]
fn random_local_song_skips_missing_files() {
    let app = fixture();
    for id in ["generated-cached-a", "generated-cached-b"] { cache_generated_song(&app, id); }
    std::fs::remove_file(app.lib.paths.root.join("cache-generated/generated-cached-a.mp3")).unwrap();
    let ui = crate::rt::Ui::new().unwrap();
    for _ in 0..8 {
        let song = ui.block_on(app.lib.random_local_song_async(None)).expect("a present song");
        assert_eq!(song.id, "generated-cached-b");
    }
    std::fs::remove_file(app.lib.paths.root.join("cache-generated/generated-cached-b.mp3")).unwrap();
    assert!(ui.block_on(app.lib.random_local_song_async(None)).is_none());
}

/// UX-004: online, Shuffle Songs starts with a song on the card at once; the
/// server's random queue is not on the path to audio.
#[test]
fn online_shuffle_starts_from_the_card_without_waiting_for_the_server() {
    let mut app = fixture(); settle_home(&mut app);
    app.player.shutdown(); let (player, commands) = PlayerHandle::fixture(Status::default()); app.player = player;
    for id in ["generated-cached-a", "generated-cached-b", "generated-cached-c"] { cache_generated_song(&app, id); }
    app.lib.mark_online_for_test(true);
    let started = Instant::now();
    let sent = shuffle_commands(&mut app, &commands);
    let Some(Cmd::Play { songs, index: 0, .. }) = sent.first() else { panic!("no play: {}", sent.len()) };
    assert_eq!(songs.len(), 1);
    assert!(songs[0].id.starts_with("generated-cached-"));
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(app.stack.last().unwrap().1.is_now_playing());
}

#[test]
fn offline_shuffle_late_scan_cannot_replace_new_playback_or_another_account() {
    for account_changed in [false, true] {
        let mut app = fixture(); settle_home(&mut app);
        app.player.shutdown(); let (player, commands) = PlayerHandle::fixture(Status::default()); app.player = player;
        app.lib.set_offline_mode(true);
        select_label(&mut app, "Shuffle Songs"); step(&mut app, Event::Center);
        let screen = app.top_id();
        if account_changed { app.config.server.user = "different-generated-user".into(); }
        else { app.player.fixture_status(Status { generation: 23, ..Default::default() }); }
        app.handle(UiMsg::Loaded { screen, token: 1, data: Box::new(Ok::<_, String>(super::screens::sources::ShuffleSelection::Seed(Box::new(subsonic::Song { id: "generated-stale-download".into(), ..Default::default() })))) }, &mut crate::platform::Headless { last: None });
        assert!(commands.try_recv().is_err(), "late card result cannot replace newer intent");
        assert_eq!(title(&app), "Q2");
    }
}

#[test]
fn script_present_barrier_requires_a_new_successful_presentation_not_rendering() {
    let mut app=fixture();let mut platform=crate::platform::Headless {last:None};
    app.presentation_completed();
    let (reply, mut completed)=tokio::sync::oneshot::channel();
    app.handle(UiMsg::PresentBarrier(reply),&mut platform);
    assert!(app.dirty);
    assert!(completed.try_recv().is_err());
    let _=app.render();
    assert!(completed.try_recv().is_err(),"a screenshot cannot acknowledge DRM presentation");
    app.presentation_completed();assert_eq!(completed.try_recv(),Ok(()));
    app.presentation_completed();assert!(completed.try_recv().is_err());
    let (reply, mut later)=tokio::sync::oneshot::channel();
    app.handle(UiMsg::PresentBarrier(reply),&mut platform);
    assert!(later.try_recv().is_err(),"old presents cannot acknowledge a later barrier");
    app.presentation_completed();assert_eq!(later.try_recv(),Ok(()));
}


#[test]
fn deliberate_offline_suppresses_radio_without_saving_preferences_and_wifi_on_wins() {
    let mut app = fixture();
    app.config.wifi.networks = vec![WifiNetwork { ssid: "Generated network".into(), psk: "generated-test-only".into(), ..Default::default() }];
    let wifi = app.config.wifi.clone();
    let tailscale = app.config.tailscale.clone();
    page(&mut app, Page::Connections);
    select_label(&mut app, "Offline Mode"); step(&mut app, Event::Center);
    assert!(app.lib.offline_mode()); assert!(!app.net.state().enabled);
    assert_eq!(app.config.wifi, wifi); assert_eq!(app.config.tailscale, tailscale);
    assert!(app.config_save.is_none(), "temporary suppression is not persisted");
    step(&mut app, Event::Center);
    assert!(!app.lib.offline_mode()); assert!(app.net.state().enabled);
    step(&mut app, Event::Center);
    app.push(Box::new(screens::wifi::Wifi::new()));
    step(&mut app, Event::Center);
    for r in app.take_requests() {
        crate::rt::Ui::new().unwrap().block_on(crate::request(&mut app, &mut crate::platform::Headless { last: None }, r));
    }
    assert!(!app.lib.offline_mode()); assert!(app.net.state().enabled);
    assert_eq!(app.config.wifi, wifi); assert_eq!(app.config.tailscale, tailscale);
    step(&mut app, Event::Back);
    assert_eq!(title(&app), "Connections");
    app.player.shutdown();
}

#[test]
fn offline_leave_restores_wifi_off_and_explicit_tailscale_on_exits_without_enabling_wifi() {
    let mut app = fixture();
    app.config.wifi.enabled = false;
    app.net.set_enabled(false, &[]);
    page(&mut app, Page::Connections);
    select_label(&mut app, "Offline Mode"); step(&mut app, Event::Center);
    assert!(app.lib.offline_mode()); assert!(!app.net.state().enabled);
    step(&mut app, Event::Center);
    assert!(!app.lib.offline_mode()); assert!(!app.net.state().enabled);
    step(&mut app, Event::Center);
    app.push(Box::new(screens::tailscale::Tailscale::new()));
    step(&mut app, Event::Center);
    assert!(app.config.tailscale.enabled);
    assert!(!app.lib.offline_mode()); assert!(!app.net.state().enabled);
    assert!(!app.config.wifi.enabled);
    app.player.shutdown();
}

#[test]
fn failed_offline_down_reverts_library_selection_and_keeps_actionable_row_error() {
    let mut app = fixture();
    let wifi = app.config.wifi.clone();
    let tailscale = app.config.tailscale.clone();
    page(&mut app, Page::Connections);
    select_label(&mut app, "Offline Mode"); step(&mut app, Event::Center);
    app.net.fail_current_radio_request();
    app.handle(UiMsg::Net, &mut crate::platform::Headless { last: None });
    assert!(!app.lib.offline_mode());
    assert!(app.net.state().error.as_deref().unwrap().contains("restore failed"));
    assert!(app.toast.as_ref().unwrap().0.contains("Wi-Fi"));
    assert_eq!(app.config.wifi, wifi); assert_eq!(app.config.tailscale, tailscale);
    app.push(Box::new(screens::wifi::Wifi::new()));
    assert!(app.stack.last().unwrap().1.audit().rows()[0].detail.as_ref().unwrap().contains("restore failed"));
    app.player.shutdown();
}


#[test]
fn restart_preparation_yields_while_database_barrier_is_busy() {
    let mut app = fixture();
    let (entered, started) = std::sync::mpsc::channel();
    let (release, wait) = std::sync::mpsc::channel();
    app.lib.db_post(move |_, _| { entered.send(()).unwrap(); wait.recv_timeout(Duration::from_secs(5)).unwrap(); });
    started.recv_timeout(Duration::from_secs(3)).unwrap();
    crate::rt::Ui::new().unwrap().block_on(async {
        let prepare = app.prepare_restart();
        tokio::pin!(prepare);
        tokio::select! {
            result = &mut prepare => panic!("restart skipped pending storage: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(5)) => {}
        }
        release.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(3), prepare).await.unwrap().unwrap();
    });
    app.player.shutdown();
}


#[test]
fn status_bar_draw_does_not_wait_for_download_database_counts() {
    let mut app = fixture();
    app.config.display.clock_in_title = false;
    app.draw_status_bar();
    let expected = app.canvas.pixels.clone();
    let (entered, started) = std::sync::mpsc::channel();
    let (release, wait) = std::sync::mpsc::channel();
    let (finished, completion) = std::sync::mpsc::channel();
    app.lib.db_post(move |_, _| {
        entered.send(()).unwrap();
        // On the old render path this expires, releases SQLite, and records
        // the failure instead of wedging the suite or poisoning the DB owner.
        let released = wait.recv_timeout(Duration::from_secs(5)).is_ok();
        finished.send(released).unwrap();
    });
    started.recv_timeout(Duration::from_secs(3)).unwrap();
    app.draw_status_bar();
    let _ = release.send(());
    let released_before_deadline = completion.recv_timeout(Duration::from_secs(3)).unwrap();
    app.player.shutdown();
    assert!(released_before_deadline, "status bar waited for SQLite despite needing only live activity");
    assert_eq!(app.canvas.pixels, expected, "stored queue counts cannot change the live activity icon");
}

#[test]
fn heart_popup_closes_immediately_but_acknowledges_only_durable_storage() {
    for fail in [false, true] {
        let mut app = fixture();
        settle_home(&mut app);
        let song = subsonic::Song { id: "generated-heart-popup".into(), title: "Generated".into(), ..Default::default() };
        let screen = app.stack.last().unwrap().0;
        let popup = app.with_screen(screen, |_, ctx| screens::popup::song_menu(song.clone(), ctx)).unwrap();
        app.push(Box::new(popup));
        select_label(&mut app, "Love");
        let (entered, started) = std::sync::mpsc::channel();
        let (release, wait) = std::sync::mpsc::channel();
        app.lib.db_post(move |_, db| {
            if fail { db.reject_music_actions(&crate::rt::Blocking::dedicated_thread(), true); }
            entered.send(()).unwrap();
            wait.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        started.recv_timeout(Duration::from_secs(3)).unwrap();
        app.toast = None;
        step(&mut app, Event::Center);
        assert_eq!(app.stack.last().unwrap().0, screen, "same pop-up Back destination");
        assert!(app.lib.loved(&song), "feedback does not wait for SQLite");
        assert!(app.toast.is_none(), "no false acknowledgement while storage is blocked");
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let expected = if fail { "Could not save music action" } else { "Loved" };
        while app.toast.as_ref().is_none_or(|(text, _)| text != expected) {
            let message = recv_within(&mut app.inbox, deadline.saturating_duration_since(Instant::now())).expect("music completion notice");
            app.handle(message, &mut crate::platform::Headless { last: None });
        }
        assert_eq!(app.lib.loved(&song), !fail);
        assert_eq!(app.lib.db().pending(&crate::rt::Blocking::dedicated_thread(), 10).len(), usize::from(!fail));
        app.player.shutdown();
    }
}

#[test]
fn abandoned_power_preparation_drains_database_and_releases_its_player_barrier() {
    let app = fixture();
    assert!(app.lib.db().get(&crate::rt::Blocking::dedicated_thread(), "queue").is_none());
    let (entered, started) = std::sync::mpsc::channel();
    let (release, wait) = std::sync::mpsc::channel();
    app.lib.db_post(move |_, _| { entered.send(()).unwrap(); wait.recv_timeout(Duration::from_secs(5)).unwrap(); });
    started.recv_timeout(Duration::from_secs(3)).unwrap();
    let preparation = prepare_power_state(&app.player, &app.lib);
    // The admitted work owns acquisition and cleanup even before this observer
    // is polled, and cannot report success while SQLite is held.
    drop(preparation);
    release.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while crate::rt::Ui::new().unwrap().block_on(app.lib.saved_queue_async()).is_none() {
        assert!(Instant::now() < deadline, "abandoned preparation lost its queue write");
        std::thread::sleep(Duration::from_millis(1));
    }
    // Persistence proves the original acquisition already happened. Refused
    // attempts leave its barrier alone; success proves its guard released it.
    while app.player.prepare_sleep().is_err() {
        assert!(Instant::now() < deadline, "abandoned preparation retained player ownership");
        std::thread::sleep(Duration::from_millis(1));
    }
    app.player.cancel_handoff();
    app.player.shutdown();
}


fn settle_settings(app: &mut Fixture) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while app.settings_pending() {
        let message = crate::ui::recv_within(&mut app.inbox, deadline.saturating_duration_since(Instant::now())).expect("owned settings completion");
        app.handle(message, &mut crate::platform::Headless { last: None });
    }
}

#[test]
fn held_settings_save_keeps_input_live_and_old_completion_cannot_clear_newer_edits() {
    let mut app = fixture();
    let release = app.paths.hold_config_saves();
    app.config.playback.volume = 25;
    app.settings_changed(); app.start_settings_save();
    assert!(app.settings_pending());
    app.screen_on = false;
    app.input(Raw::WheelTicks(4), Instant::now(), &mut crate::platform::Headless { last: None });
    assert_eq!(app.config.playback.volume, 29);
    assert!(app.config_save.is_some());
    crate::rt::Ui::new().unwrap().block_on(async {
        assert_eq!(tokio::time::timeout(Duration::from_secs(2), crate::rt::jobs().run_isolated(|_| 47)).await.unwrap().unwrap(), 47);
    });
    release.send(()).unwrap();
    settle_settings(&mut app);
    assert_eq!(Config::load(&crate::rt::Blocking::dedicated_thread(), &app.paths.config()).playback.volume, 25);
    assert!(app.config_save.is_some(), "only the captured revision was durable");
    assert!(app.save_settings());
    assert_eq!(Config::load(&crate::rt::Blocking::dedicated_thread(), &app.paths.config()).playback.volume, 29);
    assert!(app.config_save.is_none());
    app.player.shutdown();
}

#[test]
fn deferred_restart_waits_for_latest_settings_and_database_without_stalling_input() {
    let mut app = fixture();
    let release_save = app.paths.hold_config_saves();
    let (entered, started) = std::sync::mpsc::channel();
    let (release_db, waiting) = std::sync::mpsc::channel();
    app.lib.db_post(move |_, _| { entered.send(()).unwrap(); let _ = waiting.recv_timeout(Duration::from_secs(5)); });
    started.recv_timeout(Duration::from_secs(2)).unwrap();
    app.begin_restart();
    app.screen_on = false;
    let volume = app.config.playback.volume;
    app.input(Raw::WheelTicks(2), Instant::now(), &mut crate::platform::Headless { last: None });
    assert_eq!(app.config.playback.volume, volume + 2);
    assert!(!app.pending.contains(&Request::RestartReady));
    release_save.send(()).unwrap();
    settle_settings(&mut app);
    assert_eq!(Config::load(&crate::rt::Blocking::dedicated_thread(), &app.paths.config()).playback.volume, volume + 2);
    assert!(!app.pending.contains(&Request::RestartReady));
    release_db.send(()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !app.pending.contains(&Request::RestartReady) {
        let message = crate::ui::recv_within(&mut app.inbox, deadline.saturating_duration_since(Instant::now())).expect("restart barrier completion");
        app.handle(message, &mut crate::platform::Headless { last: None });
    }
    app.player.shutdown();
}

#[test]
fn tailscale_publication_refreshes_visible_and_back_stack_rows_without_a_timer() {
    let mut app = fixture();
    app.config.tailscale.enabled = true;
    page(&mut app, Page::Connections);
    select_label(&mut app, "Tailscale"); step(&mut app, Event::Center);
    assert_eq!(title(&app), "Tailscale");
    let last_input = app.last_input;
    app.screen_on = false;
    app.tailscale.publish_generated(crate::tailscale::State { status: "Connected".into(), address: "100.64.0.1".into(), ..Default::default() });
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let message = recv_within(&mut app.inbox, deadline.saturating_duration_since(Instant::now())).expect("owner publication reaches UI");
        let changed = matches!(message, UiMsg::Tailscale);
        app.handle(message, &mut crate::platform::Headless {last: None});
        if changed { break; }
    }
    assert!(!app.screen_on, "backend status must not wake the display");
    assert_eq!(app.last_input, last_input, "backend status is not user activity");
    let surface = app.stack.last().unwrap().1.audit();
    assert_eq!(surface.rows().iter().find(|r| r.title == "Status").unwrap().detail.as_deref(), Some("Connected"));
    let id = app.stack.last().unwrap().0;
    assert_eq!(app.with_screen(id, |screen, ctx| screen.tick(ctx)), Some(false));
    app.screen_on = true;
    step(&mut app, Event::Back);
    let surface = app.stack.last().unwrap().1.audit();
    assert_eq!(surface.rows()[surface.selected()].title, "Tailscale");
    assert_eq!(surface.rows().iter().find(|r| r.title == "Tailscale").unwrap().value.as_deref(), Some("Connected"));
    app.player.shutdown();
}

#[test]
fn sleep_timer_is_cancelable_session_only_and_expires_across_navigation() {
    fn open_manual(app: &mut App) {
        step(app, Event::CenterLong); select_label(app, "Sleep Timer"); step(app, Event::Center);
        assert_eq!(title(app), "Sleep Timer");
    }
    let mut app = fixture();
    app.background = false;
    generated_spoken_playback(&mut app, false, 101);
    open_manual(&mut app);
    step(&mut app, Event::Back);
    assert!(app.sleep_timer.is_none(), "opening and cancelling must not arm playback changes");
    open_manual(&mut app);
    step(&mut app, Event::Scroll(1)); step(&mut app, Event::Center);
    assert!(app.stack.last().unwrap().1.is_now_playing());
    let deadline = app.sleep_timer.expect("five-minute timer armed");
    assert!(deadline.saturating_duration_since(Instant::now()) <= Duration::from_secs(300));
    assert!(app.config_save.is_none(), "session timer must not persist as an automatically rearmed setting");
    open_manual(&mut app);
    // UXI-023: the running timer's period is checked and selected; Off is one row up.
    let surface = app.stack.last().unwrap().1.audit();
    assert_eq!(surface.rows()[surface.selected()].title, "5 Minutes");
    step(&mut app, Event::Scroll(-1)); step(&mut app, Event::Center);
    assert!(app.sleep_timer.is_none());
    open_manual(&mut app);
    step(&mut app, Event::Scroll(1)); step(&mut app, Event::Center);
    let deadline = app.sleep_timer.unwrap();
    step(&mut app, Event::Back);
    app.screen_on = false;
    app.sleep_timer = Some(Instant::now() + Duration::from_millis(20));
    assert!(app.next_wake() <= Duration::from_millis(20), "screen-off wait honors timer deadline");
    app.sleep_timer = Some(deadline);
    app.tick_at(&mut crate::platform::Headless { last: None }, deadline - Duration::from_millis(1));
    assert!(app.sleep_timer.is_some());
    app.tick_at(&mut crate::platform::Headless { last: None }, deadline);
    assert!(app.sleep_timer.is_none());
    assert_eq!(app.toast.as_ref().map(|(text, _)| text.as_str()), Some("Sleep timer ended"));
    app.toast = None;
    app.tick_at(&mut crate::platform::Headless { last: None }, deadline + Duration::from_secs(1));
    assert!(app.toast.is_none(), "expiry is one-shot");
    app.player.shutdown();
}

#[test]
fn auto_sleep_settings_are_grouped_and_preserve_back_and_choices() {
    let mut app=fixture(); app.background=false;
    page(&mut app,Page::Playback);
    let surface=app.stack.last().unwrap().1.audit();
    assert!(!surface.rows().iter().any(|row|row.title=="Sleep Timer"));
    assert!(!surface.rows().iter().any(|row|row.title=="Auto Timer Starts"));
    let entry=surface.rows().iter().find(|row|row.title=="Auto Sleep Timer").unwrap();
    assert_eq!(entry.value.as_deref(),Some("Off"));
    select_label(&mut app,"Auto Sleep Timer"); step(&mut app,Event::Center);
    assert_eq!(title(&app),"Auto Sleep Timer");
    let surface=app.stack.last().unwrap().1.audit();
    assert_eq!(surface.rows().iter().map(|r|r.title.as_str()).collect::<Vec<_>>(),vec!["Duration","Auto Timer Starts","Auto Timer Ends"]);
    assert_eq!(surface.rows()[0].detail.as_deref(),Some("Audiobooks and podcasts only"));
    select_label(&mut app,"Duration"); step(&mut app,Event::Center);
    assert_eq!(title(&app),"Duration");
    step(&mut app,Event::Back); assert_eq!(app.config.playback.auto_sleep_minutes,0);
    step(&mut app,Event::Center); select_label(&mut app,"30 Minutes"); step(&mut app,Event::Center);
    assert_eq!(title(&app),"Auto Sleep Timer"); assert_eq!(app.config.playback.auto_sleep_minutes,30);
    select_label(&mut app,"Auto Timer Starts"); step(&mut app,Event::Center);
    select_label(&mut app,"21:00"); step(&mut app,Event::Center);
    assert_eq!(app.config.playback.auto_sleep_start,21*60);
    select_label(&mut app,"Auto Timer Ends"); step(&mut app,Event::Center);
    select_label(&mut app,"06:00"); step(&mut app,Event::Center);
    assert_eq!(app.config.playback.auto_sleep_end,6*60);
    step(&mut app,Event::Back); assert_eq!(title(&app),"Playback");
    let surface=app.stack.last().unwrap().1.audit();
    assert_eq!(surface.rows()[surface.selected()].title,"Auto Sleep Timer");
    assert_eq!(surface.rows()[surface.selected()].value.as_deref(),Some("30 Minutes"));
    app.player.shutdown();
}

#[test]
fn manual_sleep_timer_is_available_for_music_and_radio() {
    let mut app=fixture(); app.background=false;
    for radio in [false,true] {
        app.stack.truncate(1);
        app.status.queue=Arc::new(vec![subsonic::Song {id:"generated".into(),title:"Generated".into(),
            radio_url:radio.then(|| "https://example.invalid/radio".into()), ..Default::default()}]);
        app.status.state=PlayState::Playing;
        let queue=app.status.queue.clone(); app.apply(Action::NowPlaying);
        step(&mut app,Event::CenterLong); select_label(&mut app,"Sleep Timer"); step(&mut app,Event::Center);
        assert_eq!(title(&app),"Sleep Timer"); step(&mut app,Event::Back);
        assert!(app.stack.last().unwrap().1.is_now_playing()); assert!(app.sleep_timer.is_none());
        step(&mut app,Event::CenterLong); select_label(&mut app,"Sleep Timer"); step(&mut app,Event::Center);
        select_label(&mut app,"5 Minutes"); step(&mut app,Event::Center);
        assert!(app.sleep_timer.is_some()); assert!(Arc::ptr_eq(&app.status.queue,&queue));
        app.sleep_timer=None; app.sleep_timer_period=None;
    }
    app.player.shutdown();
}

#[test]
fn spoken_now_playing_sleep_timer_preserves_menu_and_back() {
    let mut app = fixture();
    generated_spoken_playback(&mut app, false, 101);
    step(&mut app, Event::CenterLong);
    assert_eq!(title(&app), "Audiobook");
    let rows = app.stack.last().unwrap().1.audit();
    assert!(rows.rows().iter().any(|row| row.title == "Playback Speed"));
    assert!(rows.rows().iter().any(|row| row.title == "Sleep Timer"));
    select_label(&mut app, "Sleep Timer");
    step(&mut app, Event::Center);
    assert_eq!(title(&app), "Sleep Timer");
    step(&mut app, Event::Back);
    assert!(app.stack.last().unwrap().1.is_now_playing());
    assert!(app.sleep_timer.is_none());
    app.player.shutdown();
}

#[test]
fn now_playing_sleep_status_renders_without_moving_existing_controls() {
    fn save_frame(name: &str, pixels: &[u32]) {
        let Some(directory) = std::env::var_os("Q2_TIMER_RENDER_DIR") else { return };
        let directory = std::path::Path::new(&directory);
        std::fs::create_dir_all(directory).unwrap();
        let path = directory.join(format!("{name}.png"));
        assert!(!path.exists(), "preserve earlier generated screenshots");
        let rgb: Vec<u8> = pixels.iter().flat_map(|p| [(*p >> 16) as u8, (*p >> 8) as u8, *p as u8]).collect();
        image::save_buffer(path, &rgb, WIDTH as u32, HEIGHT as u32, image::ColorType::Rgb8).unwrap();
    }
    let mut app = fixture();
    for label in ["Sleep in 1:00:00", "Fading out 0:10"] {
        assert!(app.fonts.measure(label, 14, Weight::Bold) <= WIDTH - 40);
    }
    assert!(app.fonts.line_height(14) <= HEIGHT - (STATUS_H + 270));
    for (episode, name) in [(false, "book"), (true, "podcast")] {
        generated_spoken_playback(&mut app, episode, 100 + u64::from(episode));
        for error in [false, true] {
            app.status.error = error.then(|| "Generated playback error".into());
            app.status.state = if error { PlayState::Paused } else { PlayState::Playing };
            app.sleep_timer = None;
            let without = app.render().pixels.clone();
            app.sleep_timer = Some(Instant::now() + Duration::from_secs(62));
            app.sleep_fading = false;
            let active = app.render().pixels.clone();
            assert_eq!(&without[..(WIDTH * (STATUS_H + 270)) as usize], &active[..(WIDTH * (STATUS_H + 270)) as usize], "timer must not cover art, text, errors or controls");
            assert_ne!(without, active, "armed timer is visible");
            app.sleep_fading = true;
            app.sleep_timer = Some(Instant::now() + Duration::from_secs(9));
            let fading = app.render().pixels.clone();
            assert_ne!(active, fading, "fade has a distinct visible label");
            assert_eq!(&without[..(WIDTH * (STATUS_H + 270)) as usize], &fading[..(WIDTH * (STATUS_H + 270)) as usize]);
            if !error { save_frame(&format!("{name}-active"), &active); save_frame(&format!("{name}-fading"), &fading); }
        }
    }
    app.status.error = None;
    app.status.queue = Arc::new(vec![subsonic::Song { id:"generated-music".into(), title:"Generated Music".into(), ..Default::default() }]);
    app.status.generation += 1;
    app.sleep_timer = None;
    let without = app.render().pixels.clone();
    app.sleep_timer = Some(Instant::now() + Duration::from_secs(60)); app.sleep_fading = false;
    let music = app.render().pixels.clone();
    assert_ne!(without, music, "manual timers are also visible for music");
    save_frame("music-active", &music);
    app.status.queue = Arc::new(vec![subsonic::Song { id:"generated-radio".into(), title:"Generated Radio".into(), radio_url:Some("http://127.0.0.1:1/generated".into()), ..Default::default() }]);
    app.status.generation += 1;
    app.sleep_timer = None;
    let without_radio = app.render().pixels.clone();
    app.sleep_timer = Some(Instant::now() + Duration::from_secs(60));
    let radio = app.render().pixels.clone();
    assert_ne!(without_radio, radio, "manual timers are also visible for radio");
    assert_eq!(&without_radio[..(WIDTH * (STATUS_H + 270)) as usize], &radio[..(WIDTH * (STATUS_H + 270)) as usize]);
    save_frame("radio-active", &radio);
    app.player.shutdown();
}

#[test]
fn now_playing_sleep_redraw_tracks_timer_and_fade_even_when_paused() {
    let mut app = fixture();
    generated_spoken_playback(&mut app, false, 101);
    app.status.state = PlayState::Paused;
    let screen = app.top_id();
    let now = Instant::now();
    app.sleep_timer = Some(now + Duration::from_secs(62));
    let tick = |app: &mut App, time| app.with_screen(screen, |screen, ctx| { ctx.now = time; screen.tick(ctx) }).unwrap();
    assert!(tick(&mut app, now)); // Initial generation.
    assert!(tick(&mut app, now)); // Initial timer label.
    assert!(!tick(&mut app, now + Duration::from_millis(100)), "do not redraw within the same displayed second");
    assert!(tick(&mut app, now + Duration::from_secs(1)), "paused countdown continues");
    app.screen_on = false;
    assert!(!tick(&mut app, now + Duration::from_secs(2)), "screen-off countdown must not redraw");
    app.screen_on = true;
    assert!(tick(&mut app, now + Duration::from_secs(2)), "wake redraws the current countdown");
    app.sleep_fading = true;
    assert!(tick(&mut app, now + Duration::from_secs(2)), "fade transition redraws even within a second");
    app.sleep_fading = false;
    app.sleep_timer = Some(now + Duration::from_secs(120));
    assert!(tick(&mut app, now + Duration::from_secs(2)), "restart updates the displayed timer");
    app.sleep_timer = None;
    assert!(tick(&mut app, now + Duration::from_secs(2)), "cancel removes the label");
    assert!(!tick(&mut app, now + Duration::from_secs(2)));
    app.player.shutdown();
}

#[test]
fn play_during_fade_restarts_original_duration() {
    let mut app = fixture();
    generated_spoken_playback(&mut app, false, 101);
    app.player.shutdown();
    let (player, commands) = crate::audio::player::PlayerHandle::fixture(app.status.clone());
    app.player = player;
    app.sleep_timer = Some(Instant::now() + Duration::from_secs(5));
    app.sleep_timer_period = Some(Duration::from_secs(1800));
    app.sleep_fading = true;
    step(&mut app, Event::PlayPause);
    assert!(!app.sleep_fading);
    assert!(matches!(commands.try_recv(), Ok(Cmd::SetSleepTimer { deadline: Some(_) })));
    let remaining = app.sleep_timer.unwrap().saturating_duration_since(Instant::now());
    assert!(remaining > Duration::from_secs(1799) && remaining <= Duration::from_secs(1800));
    assert_eq!(app.toast.as_ref().map(|(text, _)| text.as_str()), Some("Sleep timer restarted"));
    app.player.shutdown();
}

#[test]
fn auto_timer_ui_owner_limits_scope_and_detects_start_and_window_entry() {
    let mut app = fixture();
    app.player.shutdown();
    let (player, commands) = player::PlayerHandle::fixture(app.status.clone());
    app.player = player;
    app.config.playback.auto_sleep_minutes = 30;
    app.config.playback.auto_sleep_start = 22 * 60;
    app.config.playback.auto_sleep_end = 7 * 60;
    let now = Instant::now();
    app.status.state = PlayState::Playing;
    app.status.queue = Arc::new(vec![subsonic::Song { id:"music".into(), ..Default::default() }]);
    app.update_auto_sleep(now, 23 * 60);
    assert!(app.sleep_timer.is_none());
    assert!(commands.try_recv().is_err());
    generated_spoken_playback(&mut app, false, 3);
    app.update_auto_sleep(now, 21 * 60);
    assert!(app.sleep_timer.is_none());
    app.update_auto_sleep(now, 22 * 60);
    let deadline = now + Duration::from_secs(1800);
    assert_eq!(app.sleep_timer, Some(deadline));
    assert!(matches!(commands.try_recv(), Ok(Cmd::SetSleepTimer {deadline:Some(at)}) if at==deadline));
    app.sleep_timer = None; // Deliberate user cancellation; no repeated rearm.
    app.update_auto_sleep(now, 23 * 60);
    assert!(app.sleep_timer.is_none());
    app.status.state = PlayState::Paused;
    app.update_auto_sleep(now, 23 * 60);
    app.status.state = PlayState::Playing;
    app.update_auto_sleep(now, 23 * 60);
    assert_eq!(app.sleep_timer, Some(deadline));
    assert!(matches!(commands.try_recv(), Ok(Cmd::SetSleepTimer {deadline:Some(at)}) if at==deadline));
    app.sleep_timer = None;
    app.auto_sleep = auto_sleep::AutoSleep::default();
    generated_spoken_playback(&mut app, true, 4);
    app.update_auto_sleep(now, 23 * 60);
    assert_eq!(app.sleep_timer, Some(deadline), "a podcast started inside the window arms too");
    assert!(matches!(commands.try_recv(), Ok(Cmd::SetSleepTimer {deadline:Some(at)}) if at==deadline));
    app.player.shutdown();
}

#[test]
fn screen_off_deadlines_issue_fade_then_single_expiry_with_rewind() {
    let mut app = fixture();
    app.player.shutdown();
    let (player, commands) = player::PlayerHandle::fixture(app.status.clone());
    app.player = player;
    app.background = false;
    app.screen_on = false;
    app.status.generation = 37;
    let now = Instant::now();
    let deadline = now + Duration::from_secs(10);
    app.sleep_timer = Some(deadline);
    app.sleep_timer_period = Some(Duration::from_secs(300));
    let mut platform = crate::platform::Headless { last:None };
    app.tick_at(&mut platform, now);
    assert!(matches!(commands.try_recv(), Ok(Cmd::StartSleepFade {deadline:at}) if at==deadline));
    assert!(app.sleep_fading);
    app.tick_at(&mut platform, deadline);
    assert!(matches!(commands.try_recv(), Ok(Cmd::FinishSleepTimer {deadline:at,generation:37,rewind_ms:15_000}) if at==deadline));
    assert!(app.sleep_timer.is_none());
    app.tick_at(&mut platform, deadline + Duration::from_secs(1));
    assert!(commands.try_recv().is_err());
    app.player.shutdown();
}

/// UX-006 / UXI-001: playback stopped by a network failure resumes by itself
/// when the link is usable; a user's own pause is never resumed.
#[test]
fn network_stopped_playback_resumes_when_the_link_returns() {
    let mut app = fixture();
    app.player.shutdown();
    let song = subsonic::Song { id: "generated-episode".into(), duration: 600, ..Default::default() };
    let playing = Status { state: PlayState::Playing, generation: 7, queue: Arc::new(vec![song]), ..Default::default() };
    let (player, commands) = PlayerHandle::fixture(playing.clone()); app.player = player;
    app.handle(UiMsg::Player, &mut crate::platform::Headless { last: None });
    let stopped = Status { state: PlayState::Paused, error: Some("Stream interrupted".into()),
        error_code: Some(crate::ux::Code::StreamInterrupted as u16), ..playing.clone() };
    app.player.fixture_status(stopped.clone());
    app.handle(UiMsg::Player, &mut crate::platform::Headless { last: None });
    let mut platform = crate::platform::Headless { last: None };
    let now = Instant::now();
    app.net.simulate_address(None);
    app.tick_at(&mut platform, now + Duration::from_secs(3));
    assert!(!commands.try_iter().any(|c| matches!(c, Cmd::Resume { .. })), "no link: no attempt");
    app.net.simulate_address(Some("192.0.2.10"));
    app.tick_at(&mut platform, now + Duration::from_secs(4));
    assert!(commands.try_iter().any(|c| matches!(c, Cmd::Resume { generation: 7 })));
    // A pause without a failure is the listener's: never resumed.
    app.player.fixture_status(playing.clone());
    app.handle(UiMsg::Player, &mut platform);
    app.player.fixture_status(Status { state: PlayState::Paused, ..playing });
    app.handle(UiMsg::Player, &mut platform);
    app.tick_at(&mut platform, now + Duration::from_secs(30));
    assert!(!commands.try_iter().any(|c| matches!(c, Cmd::Resume { .. })));
}

/// Reuse the actual UI control paths against the generated ABS server fixture.
/// The server owner checks the requests observed at each boundary.
pub(crate) fn exercise_retained_spoken_selection(
    lib: Arc<Library>, songs: Vec<subsonic::Song>,
    same: screens::spoken::Selection, other: screens::spoken::Selection,
    requests: impl Fn() -> Vec<String>, expected_new_play: &str,
) {
    use crate::audio::player::{Cmd, PlayerHandle, Status};
    let mut app=fixture(); app.player.shutdown(); app.lib=lib;
    let status=Status {state:PlayState::Paused,queue:Arc::new(songs),index:0,
        generation:17,seek_revision:2,position_ms:12345,..Default::default()};
    // The listener paused this instance just now (PLAYBACK-007: resume locally).
    let (player,commands)=PlayerHandle::fixture(Status {state:PlayState::Playing,..status.clone()}); app.player=player;
    app.handle(UiMsg::Player,&mut crate::platform::Headless {last:None});
    app.player.fixture_status(status.clone());
    app.handle(UiMsg::Player,&mut crate::platform::Headless {last:None});
    let before=requests(); let parent=app.top_id();
    step(&mut app,Event::PlayPause);
    assert!(matches!(commands.try_recv().unwrap(),Cmd::PlayPause));
    assert_eq!(app.top_id(),parent,"hardware resume leaves the current view in place");
    assert_eq!(requests(),before,"hardware resume must not request a new session, metadata or audio");
    let action=app.with_screen(parent,|_,ctx|screens::spoken::start_selection(same,"same".into(),ctx)).unwrap();
    assert!(matches!(action,Action::Push(_)),"explicit reopening retains remote-progress reconciliation");
    app.background=true; app.apply(action);
    let deadline=Instant::now()+Duration::from_secs(5);
    while app.stack.last().is_some_and(|(_,screen)|screen.opening_spoken()) {
        let message=recv_within(&mut app.inbox,deadline.saturating_duration_since(Instant::now()))
            .expect("explicit reopen completes its progress check");
        app.handle(message,&mut crate::platform::Headless {last:None});
        assert!(Instant::now()<deadline,"explicit reopen did not settle");
    }
    assert!(matches!(commands.try_recv().unwrap(),Cmd::ResumeSpoken {generation:17,seek_revision:2}));
    assert!(commands.try_recv().is_err());
    let after_reopen=requests();
    assert!(after_reopen.len()>before.len());
    assert!(after_reopen[before.len()..].iter().any(|request|request.starts_with("GET /api/me/progress/")));
    assert!(!after_reopen[before.len()..].iter().any(|request|request.starts_with("POST /api/items/")),"explicit reopen must not create another play session");
    let parent=app.top_id();
    let action=app.with_screen(parent,|_,ctx|screens::spoken::start_selection(other,"other".into(),ctx)).unwrap();
    assert!(matches!(action,Action::Push(_)),"different selection must use the normal open path");
    app.background=true; app.apply(action);
    let deadline=Instant::now()+Duration::from_secs(5);
    while !requests().iter().any(|request|request==expected_new_play) {
        assert!(Instant::now()<deadline,"different selection did not issue its actual ABS play request");
        std::thread::sleep(Duration::from_millis(1));
    }
    while app.stack.last().is_some_and(|(_,screen)|screen.opening_spoken()) {
        let message=recv_within(&mut app.inbox,deadline.saturating_duration_since(Instant::now()))
            .expect("different selection completes its owned opening task");
        app.handle(message,&mut crate::platform::Headless {last:None});
        assert!(Instant::now()<deadline,"different selection did not settle");
    }
    assert!(matches!(commands.try_recv().unwrap(),Cmd::Play { .. }));
    app.player.shutdown();
}

/// PLAYBACK-007: a spoken item paused for hours refreshes its remote position
/// before resuming; a recent pause resumes in place (above).
#[test]
fn long_paused_spoken_item_refreshes_before_resuming() {
    use crate::audio::player::{Cmd, PlayerHandle, Status};
    let mut app = fixture(); app.player.shutdown();
    let song = subsonic::Song { id: "generated-part".into(), duration: 600, spoken: Some(Box::new(subsonic::Spoken {
        id: "abs:generated:book:".into(), duration_ms: 600_000,
        source: subsonic::SpokenSource::Audiobookshelf { namespace: "generated".into(), item: "book".into(), episode: None, content_url: "file".into() },
        ..Default::default() })), ..Default::default() };
    let status = Status { state: PlayState::Paused, queue: Arc::new(vec![song]), generation: 3, ..Default::default() };
    let (player, commands) = PlayerHandle::fixture(Status { state: PlayState::Playing, ..status.clone() }); app.player = player;
    app.handle(UiMsg::Player, &mut crate::platform::Headless { last: None });
    app.player.fixture_status(status);
    app.handle(UiMsg::Player, &mut crate::platform::Headless { last: None });
    app.paused_since = Some(Instant::now() - Duration::from_secs(3 * 60 * 60));
    step(&mut app, Event::PlayPause);
    assert!(app.stack.last().is_some_and(|(_, screen)| screen.opening_spoken()), "refresh opens with a progress check");
    assert!(!commands.try_iter().any(|c| matches!(c, Cmd::PlayPause)));
}

/// UX-006: a page that never answers says so after its deadline and retries
/// on Center; a later answer still replaces the message.
#[test]
fn stalled_page_offers_a_retry_after_its_deadline() {
    use crate::ui::screens::menu::{Menu, RowsTask, Source};
    struct Stalled(Arc<std::sync::atomic::AtomicUsize>);
    impl Source for Stalled {
        fn title(&self) -> String { "Generated Stalled".into() }
        fn load(self: Arc<Self>, _lib: Arc<Library>, _policy: crate::library::Policy) -> RowsTask {
            self.0.fetch_add(1, Ordering::SeqCst);
            Box::pin(std::future::pending())
        }
    }
    let mut app = fixture();
    let loads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    app.push(Menu::boxed(Stalled(loads.clone())));
    let state = |app: &App| app.stack.last().unwrap().1.audit().state().to_owned();
    let mut platform = crate::platform::Headless { last: None };
    app.tick_at(&mut platform, Instant::now());
    assert_eq!(state(&app), "loading");
    std::thread::sleep(crate::ui::screens::menu::LOAD_DEADLINE + Duration::from_millis(50));
    app.tick_at(&mut platform, Instant::now());
    assert!(state(&app).contains(crate::ux::Code::Stuck.message()), "{}", state(&app));
    let before = loads.load(Ordering::SeqCst);
    step(&mut app, Event::Center);
    assert_eq!(state(&app), "loading");
    let deadline = Instant::now() + Duration::from_secs(3);
    while loads.load(Ordering::SeqCst) == before { assert!(Instant::now() < deadline, "no retry"); std::thread::sleep(Duration::from_millis(5)); }
}

/// UXI-013: the Song Cache slice lists the cached songs by title and size,
/// largest first, instead of the cache folder's file names.
#[test]
fn song_cache_drilldown_lists_cached_songs_by_title() {
    let app = fixture();
    for id in ["generated-cached-small", "generated-cached-large"] { cache_generated_song(&app, id); }
    let blocking = crate::rt::Blocking::dedicated_thread();
    let large = app.lib.paths.root.join("cache-generated").join("generated-cached-large.mp3");
    app.lib.db().put_cached_song(&blocking, "generated-cached-large", "raw", &large, 64 * 1024).unwrap();
    let runtime = crate::rt::Ui::new().unwrap();
    let songs = runtime.block_on(app.lib.cached_music_async()).unwrap();
    assert_eq!(songs.iter().map(|(song, _)| song.id.as_str()).collect::<Vec<_>>(), ["generated-cached-large", "generated-cached-small"]);
    assert_eq!((songs[0].1, songs[1].1), (64 * 1024, 15));
    app.lib.db().forget_cached_song(&blocking, "generated-cached-large", "raw");
    assert_eq!(runtime.block_on(app.lib.cached_music_async()).unwrap().len(), 1, "an evicted copy is not listed");
}

fn row_titles(app: &App) -> Vec<String> { app.stack.last().unwrap().1.audit().rows().iter().map(|row| row.title.clone()).collect() }

#[test]
fn bluetooth_is_a_settings_item_and_about_is_under_system() {
    let mut app = fixture();
    page(&mut app, Page::Root);
    assert_eq!(row_titles(&app), ["Sound", "Playback", "Display", "Connections", "Bluetooth", "Storage", "System"]);
    assert_eq!(app.stack.last().unwrap().1.audit().rows()[4].value.as_deref(), Some("Off"));
    select_label(&mut app, "Bluetooth");
    step(&mut app, Event::Center);
    assert_eq!((title(&app), row_titles(&app)), ("Bluetooth".into(), vec!["Bluetooth".to_string()]));
    // Long Center: the details page for diagnosis; Back returns to the Bluetooth page, then to Settings at its row.
    step(&mut app, Event::CenterLong);
    assert_eq!(title(&app), "Bluetooth Details");
    step(&mut app, Event::Back);
    assert_eq!(title(&app), "Bluetooth");
    step(&mut app, Event::Back);
    let surface = app.stack.last().unwrap().1.audit();
    assert_eq!((title(&app), surface.rows()[surface.selected()].title.as_str()), ("Settings".into(), "Bluetooth"));
    select_label(&mut app, "System");
    step(&mut app, Event::Center);
    assert_eq!(row_titles(&app).last().map(String::as_str), Some("About"));
    assert!(app.stack.last().unwrap().1.audit().rows().last().unwrap().separator);
    select_label(&mut app, "About");
    step(&mut app, Event::Center);
    assert_eq!(title(&app), "About");
    step(&mut app, Event::Back);
    assert_eq!(title(&app), "System");
    page(&mut app, Page::Connections);
    assert!(!row_titles(&app).iter().any(|row| row == "Bluetooth"), "Bluetooth left Connections");
    app.player.shutdown();
}

/// Delivers the Bluetooth thread's state to the screens until `done` holds for the top one.
fn bluetooth_until(app: &mut App, what: &str, done: impl Fn(&App) -> bool) {
    let limit = Instant::now() + Duration::from_secs(20);
    loop {
        app.handle(UiMsg::Bluetooth, &mut crate::platform::Headless { last: None });
        if done(app) { return }
        assert!(Instant::now() < limit, "{what}: {} {:?}", title(app), row_titles(app));
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn pair_new_device_searches_pairs_and_closes_then_the_device_page_forgets() {
    let mut app = fixture();
    let notify = Arc::new(|| {});
    app.bluetooth = crate::bluetooth::Bluetooth::new(crate::bluetooth::Options::simulated(&app.paths.root.join("bluetooth-ui")), notify);
    page(&mut app, Page::Root);
    select_label(&mut app, "Bluetooth");
    step(&mut app, Event::Center);
    step(&mut app, Event::Center);
    assert!(app.config.bluetooth.enabled && app.bluetooth.state().phase == crate::bluetooth::Phase::Starting);
    // Pair New Device opened while the controller starts: it says so, then searches when it is ready.
    select_label(&mut app, "Pair New Device");
    step(&mut app, Event::Center);
    assert_eq!(title(&app), "Pair New Device");
    assert_eq!(row_titles(&app)[0], "Starting Bluetooth…");
    bluetooth_until(&mut app, "found", |app| row_titles(app).iter().any(|row| row == "Fixture Headphones"));
    assert!(app.bluetooth.state().discovering);
    select_label(&mut app, "Fixture Headphones");
    step(&mut app, Event::Center);
    bluetooth_until(&mut app, "paired", |app| title(app) == "Bluetooth");
    assert_eq!(app.toast.as_ref().map(|(text, _)| text.as_str()), Some("Connected to Fixture Headphones"));
    bluetooth_until(&mut app, "discovery stopped", |app| { let state = app.bluetooth.state(); !state.discovering && !state.scanning });
    let rows = app.stack.last().unwrap().1.audit().rows().to_vec();
    let paired = rows.iter().find(|row| row.title == "Fixture Headphones").expect("the new device heads the list");
    assert!(paired.value.as_deref().is_some_and(|value| value == "Connected" || value == "Playing"));
    // Its page: Disconnect, then Forget through the confirmation.
    select_label(&mut app, "Fixture Headphones");
    step(&mut app, Event::Center);
    assert_eq!((title(&app), row_titles(&app)[0].as_str()), ("Fixture Headphones".into(), "Disconnect"));
    step(&mut app, Event::Center);
    bluetooth_until(&mut app, "disconnected", |app| row_titles(app)[0] == "Connect");
    select_label(&mut app, "Forget This Device…");
    step(&mut app, Event::Center);
    assert_eq!(title(&app), "Forget Device?");
    step(&mut app, Event::Scroll(1));
    step(&mut app, Event::Center);
    assert_eq!(title(&app), "Bluetooth");
    bluetooth_until(&mut app, "forgotten", |app| !row_titles(app).iter().any(|row| row == "Fixture Headphones"));
    app.bluetooth.shutdown();
    app.player.shutdown();
}

fn wait_for_generated_player(
    app: &mut App,
    description: &str,
    condition: impl Fn(&Status) -> bool,
) -> Status {
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        app.handle(UiMsg::Player, &mut crate::platform::Headless { last: None });
        let current = app.player.status();
        if condition(&current) {
            return current;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {description}");
        std::thread::sleep(Duration::from_millis(15));
    }
}

#[test]
#[ignore = "run inside its generated venue: tests/portal/run.py --only app-wheel --only app-phone"]
fn generated_delta_wheel_app_journey() {
    use super::portal::{Portal, State};
    let mut app = fixture();
    app.background = false;
    let case = std::fs::read_to_string("/fixture/lifecycle/case").unwrap_or_default();
    if case == "saved" {
        // A generated account in config.toml's [portal.delta]; Delta accepts either name field.
        app.config.portal = toml::from_str("[delta]\nskymiles_number = \"ab\"\npassword = \"ab\"\n").unwrap();
    }
    let notify = app.tx.clone();
    app.net = Net::new(
        "/fixture/generated-wifi".into(),
        false,
        Arc::new(move || { let _ = notify.send(UiMsg::Net); }),
    );
    app.portal = Portal::new(app.tx.clone(), false);
    app.push(Box::new(screens::wifi::Wifi::new()));
    let has_row = |app: &App, label: &str| {
        app.stack.last().unwrap().1.audit().rows().iter().any(|row| row.title == label)
    };
    let wait = |app: &mut Fixture, description: &str, condition: &dyn Fn(&App) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(20);
        while !condition(app) {
            let message = super::recv_within(
                &mut app.inbox,
                deadline.saturating_duration_since(Instant::now()),
            ).unwrap_or_else(|| panic!("timed out waiting for {description}"));
            app.handle(message, &mut crate::platform::Headless { last: None });
            assert!(Instant::now() < deadline, "timed out waiting for {description}");
        }
    };
    let choose = |app: &mut Fixture, label: &str| {
        select_label(app, label);
        step(app, Event::Center);
    };

    assert_eq!(title(&app), "Wi-Fi");
    assert!(!has_row(&app, "Wi-Fi Sign-In"));
    app.net.set_visible(true);
    app.net.set_enabled(true, &[]);
    wait(&mut app, "Wi-Fi Sign-In after the generated lease", &|app| {
        has_row(app, "Wi-Fi Sign-In")
    });
    assert_eq!(app.net.state().ip.as_deref(), Some("192.0.2.10"));
    choose(&mut app, "Wi-Fi Sign-In");
    wait(&mut app, "Delta Wi-Fi choices", &|app| app.portal.state == State::Delta);
    assert_eq!(title(&app), "Delta Wi-Fi");
    let lifecycle = std::path::Path::new("/fixture/lifecycle");
    let queued_song = if lifecycle.join("streaming").is_file() {
        app.config.server.url = "http://198.51.100.7".into();
        app.config.server.user = "fixture-media-user".into();
        app.config.server.password = "fixture-media-password".into();
        app.config.playback.stream_quality = Quality::Original;
        app.config.playback.sync_queue = false;
        app.config.playback.scrobble = false;
        app.lib.configure(&app.config);
        let queue = Arc::new(vec![subsonic::Song {
            id: "fixture-uncached".into(),
            title: "Generated uncached PCM".into(),
            suffix: Some("wav".into()),
            ..Default::default()
        }]);
        app.player.send(Cmd::Restore { songs: queue.clone(), index: 0, position_ms: 0 });
        wait_for_generated_player(&mut app, "the generated paused queue", |status| {
            status.state == PlayState::Paused && Arc::ptr_eq(&status.queue, &queue)
        });
        step(&mut app, Event::PlayPause);
        let denied = wait_for_generated_player(&mut app, "playback failure before admission", |status| {
            status.error.is_some()
        });
        assert_eq!(denied.position_ms, 0);
        assert!(Arc::ptr_eq(&denied.queue, &queue));
        Some(queue)
    } else {
        None
    };
    let phone = case == "phone";
    if phone {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let qr_state = |app: &App, expected: &str| {
            app.stack.last().unwrap().1.audit().state() == expected
        };
        choose(&mut app, "Enter Details on Phone");
        assert_eq!(title(&app), "Prepare Delta Sign-In");
        choose(&mut app, "Start Phone Setup");
        wait(&mut app, "Wi-Fi QR", &|app| qr_state(app, "Wi-Fi QR"));
        step(&mut app, Event::Center);
        assert!(qr_state(&app, "Setup QR"));
        let State::Pairing { page, .. } = &app.portal.state else {
            panic!("QR screen must reflect the live helper");
        };
        let qr_page = lifecycle.join("qr-page");
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&qr_page)
            .unwrap();
        output.write_all(page.as_bytes()).unwrap();
        drop(output);
        std::fs::write(lifecycle.join("scan-ready"), b"ready\n").unwrap();
        wait(&mut app, "Account Ready after scanning both codes", &|app| {
            has_row(app, "Account Ready")
        });
        std::fs::remove_file(&qr_page).unwrap();
        let deadline = Instant::now() + Duration::from_secs(8);
        while !lifecycle.join("phone-retired").is_file() {
            assert!(Instant::now() < deadline, "phone fixture and temporary AP must retire");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(app.portal.has_prepared());
        choose(&mut app, "Done");
        wait(&mut app, "Use Prepared Account", &|app| has_row(app, "Use Prepared Account"));
        assert_eq!(title(&app), "Delta Wi-Fi");
        choose(&mut app, "Use Prepared Account");
        assert!(app.portal.has_prepared(), "preparation survives until the helper requests credentials");
        if lifecycle.join("retry-prepared").is_file() {
            wait(&mut app, "the transient failure before the password prompt", &|app| {
                matches!(app.portal.state, State::Failed(_))
            });
            assert_eq!(app.stack.last().unwrap().1.audit().state(), "Retry");
            assert!(app.portal.has_prepared(), "the temporary failure must preserve the account");
            step(&mut app, Event::Center);
            wait(&mut app, "Use Prepared Account after Retry", &|app| {
                has_row(app, "Use Prepared Account")
            });
            assert_eq!(title(&app), "Delta Wi-Fi");
            choose(&mut app, "Use Prepared Account");
            assert!(app.portal.has_prepared(), "starting the retry still does not consume the account");
        }
    } else if case == "saved" {
        assert!(has_row(&app, "Use Saved Account"));
        choose(&mut app, "Use Saved Account");
        assert!(app.portal.state == State::Working);
    } else {
        assert!(!has_row(&app, "Use Saved Account"));
        choose(&mut app, "Enter SkyMiles Details");
        assert_eq!(title(&app), "Wi-Fi Sign-In");
        assert!(app.portal.state == State::Working);
        wait(&mut app, "the helper's identity prompt", &|app| title(app) == "SkyMiles Account");
        assert!(app.portal.state == State::Credentials);

        for _ in 0..2 {
            // The wheel initially selects the first lower-case letter. One
            // detent makes the generated credential "ab" without an injection hook.
            step(&mut app, Event::Center);
            step(&mut app, Event::Scroll(1));
            step(&mut app, Event::Center);
            step(&mut app, Event::CenterLong);
            if app.portal.state == State::Credentials {
                assert_eq!(title(&app), "SkyMiles Password");
            }
        }
    }
    assert!(app.portal.state == State::Working);
    wait(&mut app, "Internet Available", &|app| has_row(app, "Internet Available"));
    assert!(app.portal.state == State::Online);
    if phone {
        assert!(!app.portal.has_prepared(), "the submitted prepared account is one-use");
    }
    if case == "saved" {
        assert!(app.config.portal.delta.as_ref().is_some_and(|d| d.sign_in().is_some()), "the settings keep the account");
    }
    choose(&mut app, "Done");
    assert_eq!(title(&app), "Wi-Fi");

    if let Some(queue) = queued_song {
        let before = app.player.status();
        assert_eq!(before.position_ms, 0);
        assert!(Arc::ptr_eq(&before.queue, &queue));
        step(&mut app, Event::PlayPause);
        let playing = wait_for_generated_player(&mut app, "decoded audio after admission", |status| {
            status.error.is_none() && status.position_ms >= 100 && status.sample_rate.is_some()
        });
        assert!(Arc::ptr_eq(&playing.queue, &queue));
        assert_eq!(playing.index, 0);
        println!("generated same-queue stream advanced {} ms", playing.position_ms);
    }
}

#[test]
fn hardware_touchscreen_switch_warns_saves_and_only_accepts_lit_fresh_taps() {
    #[derive(Default)]
    struct Panel(Vec<bool>);
    impl Platform for Panel {
        fn set_touchscreen(&mut self, enabled: bool) -> crate::platform::ControlFuture<'_, Result<(), String>> {
            self.0.push(enabled);
            Box::pin(async { Ok(()) })
        }
        fn present<'a>(&'a mut self, _: &'a Canvas) -> crate::platform::PresentFuture<'a> { Box::pin(async { Ok(()) }) }
        fn set_screen(&mut self, _: bool) {}
        fn set_brightness(&mut self, _: u8) {}
        fn battery(&mut self) -> Battery { Battery::default() }
        fn headphones(&mut self) -> Option<bool> { None }
        fn audio_hw(&mut self) -> Box<dyn crate::audio::alsa::AudioHw> { Box::new(crate::audio::alsa::SoftHw("null".into())) }
        fn suspend(&mut self) -> crate::platform::SuspendFuture<'_> { Box::pin(async { Ok(()) }) }
        fn power_off(&mut self) -> crate::platform::SuspendFuture<'_> { Box::pin(async { Ok(()) }) }
        fn is_device(&self) -> bool { false }
    }
    let mut app = fixture();
    let mut panel = Panel::default();
    let saved: Config = toml::from_str("[display]\nbrightness = 43").unwrap();
    assert!(!saved.display.touchscreen);
    app.hardware.touchscreen = true;
    page(&mut app, Page::System);
    select_label(&mut app, "Touchscreen");
    let before_enable = Instant::now();
    step(&mut app, Event::Center);
    assert!(app.config.display.touchscreen);
    let warning = "Touchscreen is a work in progress";
    assert_eq!(app.toast.as_ref().unwrap().0, warning);
    app.fonts.capture(true);
    app.render();
    assert!(app.fonts.capture(false).iter().any(|line| line == warning), "full warning fits the toast");
    app.presentation_completed();
    app.start_settings_save();
    app.settle_settings_fixture();
    assert!(app.paths.load_config(&crate::rt::Blocking::dedicated_thread()).display.touchscreen);
    immediate_control(app.sync_touchscreen(&mut panel));
    let touchscreen_row = |at| Raw::Touch { start_x: 130, start_y: 84, x: 130, y: 84, at };
    app.input(touchscreen_row(before_enable), Instant::now(), &mut panel);
    assert!(app.config.display.touchscreen, "ignore a tap queued before enabling");
    app.screen_on = false;
    immediate_control(app.sync_touchscreen(&mut panel));
    let last_input = app.last_input;
    let dark = Instant::now();
    app.input(touchscreen_row(dark), dark, &mut panel);
    assert!(!app.screen_on);
    assert_eq!(app.last_input, last_input);
    app.wake(&mut panel);
    immediate_control(app.sync_touchscreen(&mut panel));
    app.render();
    app.presentation_completed();
    app.input(touchscreen_row(dark), Instant::now(), &mut panel);
    assert!(app.config.display.touchscreen);
    app.toast = None;
    app.input(touchscreen_row(Instant::now()), Instant::now(), &mut panel);
    immediate_control(app.sync_touchscreen(&mut panel));
    assert!(!app.config.display.touchscreen, "tap the visible switch to turn it off");
    assert!(app.toast.is_none(), "warn only when enabling");
    assert_eq!(panel.0, [true, false, true, false]);
}
