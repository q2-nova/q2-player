//! Generated optimization controls; never owner settings, media or services.
use super::*;
use crate::audio::{alsa::SoftHw, player};

struct BatteryProbe {
    reads: usize, plugged: bool, value: Option<Battery>,
    screens: Vec<(bool, usize)>, headphones_reads: usize,
    pending_battery: Option<tokio::sync::oneshot::Receiver<()>>,
    fresh_result: Option<tokio::sync::oneshot::Receiver<Battery>>,
    battery_wait_started: Option<Arc<std::sync::atomic::AtomicBool>>,
    pending_screen: Option<tokio::sync::oneshot::Receiver<()>>,
    screen_attempts: usize,
    headphone_events: bool,
    headphone_value: Option<bool>,
    presented: Option<tokio::sync::oneshot::Sender<()>>,
}
impl BatteryProbe {
    fn new(plugged: bool) -> Self {
        Self { reads: 0, plugged, value: None, screens: Vec::new(), headphones_reads: 0, pending_battery: None, fresh_result: None, battery_wait_started: None, pending_screen: None, screen_attempts: 0, headphone_events: false, headphone_value: Some(true), presented: None }
    }
}
impl Platform for BatteryProbe {
    fn present<'a>(&'a mut self, _: &'a Canvas) -> crate::platform::PresentFuture<'a> {
        Box::pin(async move { if let Some(done) = self.presented.take() { let _ = done.send(()); } Ok(()) })
    }
    fn set_screen(&mut self, on: bool) { self.screens.push((on, self.reads)); }
    fn set_screen_async(&mut self, on: bool) -> crate::platform::ControlFuture<'_, ()> {
        Box::pin(async move {
            self.screen_attempts += 1;
            if let Some(wait) = self.pending_screen.as_mut() { let _ = wait.await; self.pending_screen = None; }
            self.set_screen(on);
        })
    }
    fn set_brightness(&mut self, _: u8) {}
    fn battery(&mut self) -> Battery {
        self.reads += 1;
        self.value.unwrap_or(Battery { level: Some(76), charging: self.plugged, plugged: self.plugged })
    }
    fn battery_async(&mut self) -> crate::platform::ControlFuture<'_, Battery> {
        Box::pin(async move {
            if let Some(wait) = self.pending_battery.as_mut() {
                if let Some(started) = &self.battery_wait_started {
                    started.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                let _ = wait.await;
                self.pending_battery = None;
            }
            self.battery()
        })
    }
    fn start_battery_refresh(&mut self) -> Option<crate::platform::ControlFuture<'static, Battery>> {
        let value = self.battery();
        let wait = self.pending_battery.take();
        let fresh_result = self.fresh_result.take();
        let started = self.battery_wait_started.clone();
        Some(Box::pin(async move {
            if let Some(wait) = wait {
                if let Some(started) = started { started.store(true, std::sync::atomic::Ordering::SeqCst); }
                let _ = wait.await;
            }
            match fresh_result { Some(result) => result.await.unwrap(), None => value }
        }))
    }
    fn headphones(&mut self) -> Option<bool> { self.headphones_reads += 1; self.headphone_value }
    fn headphone_events(&self) -> bool { self.headphone_events }
    fn audio_hw(&mut self) -> Box<dyn crate::audio::alsa::AudioHw> { Box::new(SoftHw("null".into())) }
    fn suspend(&mut self) -> crate::platform::SuspendFuture<'_> { Box::pin(async { Ok(()) }) }
    fn power_off(&mut self) -> crate::platform::SuspendFuture<'_> { Box::pin(async { Ok(()) }) }
    fn is_device(&self) -> bool { false }
}

fn fixture() -> (App, PlayerHandle) {
    let root = crate::library::generated_test_directory("battery-ui");
    std::fs::create_dir_all(&root).unwrap();
    let paths = Paths::new(root.clone());
    let mut config = Config::default();
    config.power.sleep_minutes = 1;
    let lib = Library::open(paths.clone(), &config, Arc::new(|_| {}));
    let player = player::spawn(Arc::new(crate::library::Tracks(lib.clone())), Box::new(SoftHw("null".into())),
        player::Settings { volume: config.playback.volume, headphone_power: crate::config::HeadphonePower::Standard, filter: 0,
            replay_gain: config.playback.replay_gain, gapless: true, eq: config.playback.eq.clone() }, Arc::new(|| {}));
    let net = Net::new(root.join("unused-wifi.sh"), true, Arc::new(|| {}));
    let tailscale = crate::tailscale::Tailscale::new(root.clone(), root.clone(), true, false);
    let (tx, _receive) = channel();
    let app = App::new(lib, player.clone(), config, paths, net, tailscale, Fonts::bundled(), tx);
    (app, player)
}

#[test]
fn idle_sleep_retains_periodic_battery_poll_without_a_charger_gate() {
    let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let (mut app, player) = fixture();
    app.screen_on = false;
    app.last_input = Instant::now() - Duration::from_secs(10);
    let mut probe = BatteryProbe::new(false);
    app.battery_polled = Some(Instant::now());
    for _ in 0..100 {
        app.polled = Instant::now();
        app.tick(&mut probe);
    }
    assert_eq!(probe.reads, 0, "no redundant charger reads during sleep grace period");
    assert!(app.pending.is_empty());
    app.battery_polled = Some(Instant::now() - IDLE_TICK - Duration::from_secs(1));
    app.polled = Instant::now() - Duration::from_secs(1);
    app.tick(&mut probe);
    assert_eq!(probe.reads, 1, "normal periodic battery polling is preserved");
    assert_eq!(app.battery.level, Some(76));
    app.polled = Instant::now();
    app.last_input = Instant::now() - Duration::from_secs(61);
    probe.plugged = true;
    app.tick(&mut probe);
    assert_eq!(probe.reads, 1, "USB power no longer gates idle suspend or adds a read");
    assert!(matches!(app.pending.as_slice(), [Request::Sleep]));
    app.pending.clear();
    app.last_input = Instant::now() - Duration::from_secs(61);
    probe.plugged = false;
    app.tick(&mut probe);
    assert_eq!(probe.reads, 1);
    assert!(matches!(app.pending.as_slice(), [Request::Sleep]));
    player.send(Cmd::Quit);
}

#[test]
fn battery_cadence_seeded_transitions_preserve_wake_and_missing_data_recovery() {
    let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let (mut app, player) = fixture();
    app.config.power.sleep_minutes = 0;
    app.config.display.backlight_seconds = 0;
    let mut probe = BatteryProbe::new(false);
    let mut now = Instant::now() + Duration::from_secs(1);
    let mut last = None;
    let mut cached = Battery::default();
    let mut reads = 0;
    let mut seed = 0x51325057u32;
    for _ in 0..4096 {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        now += Duration::from_millis(u64::from(seed % 1101));
        // Includes unavailable/malformed platform results and subsequent recovery.
        let value = if seed % 13 == 0 { Battery::default() } else {
            Battery { level: Some((seed % 101) as u8), plugged: seed & 1 != 0, charging: seed & 3 == 3 }
        };
        probe.value = Some(value);
        if seed & 31 == 0 {
            if app.screen_on { app.screen_on = false; } else {
                app.wake_at(&mut probe, now);
                reads += 1; last = Some(now); cached = value;
                assert_eq!(probe.screens.last(), Some(&(true, reads)), "fresh read precedes visible wake");
            }
        }
        let interval = if app.screen_on { Duration::from_millis(500) } else { IDLE_TICK };
        if last.is_none_or(|time| now.duration_since(time) > interval) {
            reads += 1; last = Some(now); cached = value;
        }
        app.tick_at(&mut probe, now);
        assert_eq!(probe.reads, reads, "poll cadence survives messages/health ticks");
        assert_eq!(app.battery, cached, "UI changes only from the platform value actually read");
        assert!(app.pending.is_empty());
    }
    player.send(Cmd::Quit);
}

#[test]
fn dark_battery_deadline_and_visible_boundary_are_strict_and_wake_is_immediate() {
    let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let (mut app, player) = fixture();
    app.config.power.sleep_minutes = 0;
    app.config.display.backlight_seconds = 0;
    let mut probe = BatteryProbe::new(false);
    let start = Instant::now() + Duration::from_secs(1);
    app.screen_on = false;
    app.tick_at(&mut probe, start);
    app.tick_at(&mut probe, start + IDLE_TICK);
    assert_eq!(probe.reads, 1, "thirty-second boundary has existing strict timer semantics");
    let due = start + IDLE_TICK + Duration::from_nanos(1);
    app.tick_at(&mut probe, due);
    assert_eq!(probe.reads, 2);
    probe.plugged = true;
    app.wake_at(&mut probe, due + Duration::from_millis(1));
    assert!(app.battery.plugged);
    assert_eq!(probe.screens.last(), Some(&(true, 3)), "even a brief dark interval refreshes first");
    app.tick_at(&mut probe, due + Duration::from_millis(501));
    assert_eq!(probe.reads, 3);
    app.tick_at(&mut probe, due + Duration::from_millis(501) + Duration::from_nanos(1));
    assert_eq!(probe.reads, 4);
    player.send(Cmd::Quit);
}

#[test]
fn due_sleep_ignores_charger_and_active_headphone_checks_keep_their_cadence() {
    let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let (mut app, player) = fixture();
    let mut probe = BatteryProbe::new(true);
    let now = Instant::now() + Duration::from_secs(1);
    app.screen_on = false;
    app.battery_polled = Some(now);
    app.battery.plugged = false; // Cached value disagrees with current hardware.
    app.last_input = now - Duration::from_secs(61);
    app.tick_at(&mut probe, now);
    assert_eq!(probe.reads, 0, "due sleep does not add a charger read");
    assert!(matches!(app.pending.as_slice(), [Request::Sleep]), "USB power permits due sleep");
    app.pending.clear();
    app.battery.plugged = true;
    probe.plugged = false;
    app.last_input = now - Duration::from_secs(61);
    app.tick_at(&mut probe, now + Duration::from_secs(1));
    assert_eq!(probe.reads, 0);
    assert!(matches!(app.pending.as_slice(), [Request::Sleep]), "battery power also permits due sleep");
    app.pending.clear();
    app.config.power.sleep_minutes = 0;
    app.status.state = PlayState::Playing;
    app.config.playback.pause_on_unplug = true;
    app.battery_polled = Some(now);
    let before = probe.reads;
    for seconds in 1..=40 { app.tick_at(&mut probe, now + Duration::from_secs(seconds)); }
    assert_eq!(probe.reads - before, 1, "active jack checks do not force battery reads");
    assert!(probe.headphones_reads >= 39, "headphone freshness is independent");
    player.send(Cmd::Quit);
}

#[test]
fn cancelled_wake_never_publishes_a_stale_battery_or_visible_frame() {
    let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let (mut app, player) = fixture();
    app.screen_on = false;
    app.battery_polled = None;
    let mut probe = BatteryProbe::new(false);
    probe.value = Some(Battery { level: Some(25), ..Default::default() });
    let (release, wait) = tokio::sync::oneshot::channel();
    probe.pending_battery = Some(wait);
    let (fresh, result) = tokio::sync::oneshot::channel();
    probe.fresh_result = Some(result);
    let ui = crate::rt::Ui::new().unwrap();
    let now = Instant::now();
    ui.block_on(async {
        let wake = app.wake_at_async(&mut probe, now);
        tokio::pin!(wake);
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(wake.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        }).await;
        tokio::task::yield_now().await;
    });
    assert!(!app.screen_on);
    assert!(app.battery_polled.is_none());
    assert!(probe.screens.is_empty());
    probe.value = Some(Battery { level: Some(88), plugged: true, charging: false });
    fresh.send(probe.value.unwrap()).unwrap();
    release.send(()).unwrap();
    ui.block_on(app.wake_at_async(&mut probe, now));
    assert_eq!(app.battery, probe.value.unwrap());
    assert_eq!(probe.screens, [(true, 1)], "fresh result precedes first visible wake");
    player.send(Cmd::Quit);
}


#[test]
fn cancelled_screen_enable_resumes_wake_without_an_unrelated_redraw() {
    let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let (mut app, player) = fixture();
    let mut probe = BatteryProbe::new(false);
    let ui = crate::rt::Ui::new().unwrap();
    let now = Instant::now();
    // Make all ordinary wake observations unchanged: none can accidentally
    // provide the redraw that the interrupted transition itself must arrange.
    ui.block_on(app.refresh_battery_async(&mut probe, now));
    app.refresh_clock(clock);
    app.update_player(player.status());
    app.screen_on = false;
    app.dirty = false;
    let (release, wait) = tokio::sync::oneshot::channel();
    probe.pending_screen = Some(wait);
    ui.block_on(async {
        let wake = app.wake_at_async(&mut probe, now);
        tokio::pin!(wake);
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(wake.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        }).await;
    });
    assert_eq!(probe.screen_attempts, 1, "cancel after battery, inside real async enable");
    assert_eq!(probe.reads, 2);
    assert!(probe.screens.is_empty());
    assert!(!app.screen_on, "pending enable is not a completed visible wake");
    assert!(app.screen_restore.as_ref().unwrap().battery_ready);
    app.dirty = false;
    release.send(()).unwrap();
    ui.block_on(app.wake_at_async(&mut probe, now));
    assert_eq!(probe.screen_attempts, 2, "re-entry must join enable even without a redraw");
    assert_eq!(probe.reads, 2, "accepted fresh observation is retained through enable");
    assert_eq!(probe.screens, [(true, 2)]);
    assert!(app.screen_on && app.dirty && app.screen_restore.is_none());
    player.send(Cmd::Quit);
}

#[test]
fn cancelled_power_restore_keeps_original_outcome_and_finishes_on_next_tick() {
    let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let (mut app, player) = fixture();
    app.screen_on = false;
    app.config.power.sleep_minutes = 0;
    app.config.display.backlight_seconds = 0;
    let mut probe = BatteryProbe::new(false);
    let (release, wait) = tokio::sync::oneshot::channel();
    probe.pending_screen = Some(wait);
    let ui = crate::rt::Ui::new().unwrap();
    ui.block_on(async {
        let restore = app.finish_power_async(&mut probe, Err("Generated helper refusal".into()), None);
        tokio::pin!(restore);
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(restore.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        }).await;
    });
    assert_eq!(probe.screen_attempts, 1);
    assert_eq!(probe.reads, 1);
    assert!(!app.screen_on && probe.screens.is_empty());
    let cutoff = app.resumed_at;
    // Even an observer retry with a different result cannot overwrite the
    // original helper outcome or repeat the already accepted restoration.
    ui.block_on(async {
        let restore = app.finish_power_async(&mut probe, Ok(()), Some(Instant::now()));
        tokio::pin!(restore);
        std::future::poll_fn(|cx| {
            assert!(std::future::Future::poll(restore.as_mut(), cx).is_pending());
            std::task::Poll::Ready(())
        }).await;
    });
    assert_eq!(app.resumed_at, cutoff);
    assert!(app.resume_measurement.is_none());
    assert_eq!(probe.reads, 1);
    app.dirty = false;
    release.send(()).unwrap();
    let now = app.battery_polled.unwrap();
    ui.block_on(app.tick_at_async(&mut probe, now));
    assert!(app.screen_on && app.dirty && app.screen_restore.is_none());
    assert_eq!(probe.screens, [(true, 1)]);
    assert_eq!(app.toast.as_ref().map(|(text, _)| text.as_str()), Some("Generated helper refusal"));
    player.send(Cmd::Quit);
}

#[test]
fn jack_events_replace_known_state_polling_without_screen_or_activity_changes() {
    let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let (mut app, player) = fixture();
    player.shutdown();
    let (observed, commands) = PlayerHandle::fixture(Status {state: PlayState::Playing, ..Default::default()});
    app.player = observed;
    let mut probe = BatteryProbe::new(false);
    probe.headphone_events = true;
    app.status.state = PlayState::Playing;
    app.config.playback.pause_on_unplug = true;
    app.config.power.sleep_minutes = 0;
    app.screen_on = false;
    let now = Instant::now() + Duration::from_secs(1);
    app.last_input = now;
    for seconds in 0..40 { app.tick_at(&mut probe, now + Duration::from_secs(seconds)); }
    assert_eq!(probe.headphones_reads, 1, "one baseline, no repeated reads while event-backed state is known");
    let activity = app.last_input;
    let screens = probe.screens.len();
    probe.headphone_value = None;
    app.handle(UiMsg::Headphones, &mut probe);
    assert_eq!(app.headphones, None, "unavailable must not become unplugged");
    app.tick_at(&mut probe, now + Duration::from_secs(41));
    assert_eq!(probe.headphones_reads, 3, "unknown state retains bounded retry");
    probe.headphone_value = Some(true);
    app.handle(UiMsg::Headphones, &mut probe);
    assert_eq!(app.headphones, Some(true));
    assert!(commands.try_recv().is_err(), "baseline, unknown and reattachment never pause");
    probe.headphone_value = Some(false);
    app.handle(UiMsg::Input(Raw::HeadphonesChanged), &mut probe);
    assert_eq!(app.headphones, Some(false));
    assert!(matches!(commands.try_recv(), Ok(Cmd::Pause)), "actual UI dispatch must pause on verified unplug");
    assert!(commands.try_recv().is_err(), "one transition sends one pause");
    assert_eq!(app.last_input, activity); assert!(!app.screen_on); assert_eq!(probe.screens.len(), screens);
    let reads = probe.headphones_reads;
    app.config.playback.pause_on_unplug = false;
    app.handle(UiMsg::Headphones, &mut probe);
    assert_eq!(probe.headphones_reads, reads); assert_eq!(app.headphones, None);
    app.config.playback.pause_on_unplug = true;
    app.tick_at(&mut probe, now + Duration::from_secs(42));
    assert_eq!(probe.headphones_reads, reads + 1, "enabling establishes a new baseline");
    assert!(commands.try_recv().is_err(), "disabled notifications and a new baseline do not pause");
}


/// Actual main-loop regression: an unresolved periodic observation must not
/// prevent queued input from reaching the player. No wall-time performance claim.
#[test]
fn pending_periodic_battery_keeps_actual_run_loop_input_responsive() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::Poll;
    use std::future::{Future, poll_fn};

    let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    for held in [false, true] {
        let (mut app, player) = fixture();
        player.shutdown();
        let (observed, commands) = PlayerHandle::fixture(Status::default());
        app.player = observed;
        app.config.power.sleep_minutes = 0;
        app.config.power.radio_doze = false;
        app.config.display.backlight_seconds = 0;
        app.stack.truncate(1);
        let mut probe = BatteryProbe::new(false);
        let ui = crate::rt::Ui::new().unwrap();
        let now = Instant::now();
        app.screen_on = false;
        ui.block_on(app.wake_at_async(&mut probe, now));
        assert!(app.screen_on);
        assert!(app.screen_restore.is_none());
        assert_eq!(probe.screens, [(true, 1)], "fresh battery still precedes wake enable");
        // This is the ordinary later refresh, not the wake's fresh-result gate.
        app.battery_polled = Some(now - Duration::from_secs(1));
        app.polled = now;
        let entered = Arc::new(AtomicBool::new(false));
        let (release, waiting) = tokio::sync::oneshot::channel();
        if held {
            probe.pending_battery = Some(waiting);
            probe.battery_wait_started = Some(entered.clone());
        }
        let (tx, mut inbox) = channel();
        tx.send(UiMsg::Headphones).unwrap(); // Start one actual main-loop turn.
        let mut trace = None;
        ui.block_on(async {
            let running = crate::run(&mut app, &mut probe, &mut inbox, &mut trace);
            tokio::pin!(running);
            poll_fn(|cx| {
                assert!(running.as_mut().poll(cx).is_pending());
                Poll::Ready(())
            }).await;
            assert_eq!(entered.load(Ordering::SeqCst), held);
            tx.send(UiMsg::Input(Raw::Media(crate::input::Media::Next))).unwrap();
            tx.send(UiMsg::Quit).unwrap();
            let heartbeat = tokio::spawn(async { tokio::task::yield_now().await; 17u8 });
            assert_eq!(tokio::time::timeout(Duration::from_secs(1), heartbeat).await.unwrap().unwrap(), 17);
            let polled = poll_fn(|cx| Poll::Ready(running.as_mut().poll(cx))).await;
            assert_eq!(polled, Poll::Ready(0), "both ready and held battery permit queued input and Quit");
            if held { release.send(()).unwrap(); }
        });
        assert!(matches!(commands.try_recv(), Ok(Cmd::Next)), "exactly the original input is delivered without releasing the battery");
        assert!(commands.try_recv().is_err());
        assert_eq!(probe.reads, 2, "one wake read plus one ordinary periodic read");
    }
}

#[test]
fn pending_wake_helpers_keep_actual_run_loop_input_responsive() {
    use std::{future::{Future, poll_fn}, task::Poll};
    let exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut responsive = Vec::new();
    for held_screen in [false, true] {
        let (mut app, player) = fixture();
        player.shutdown();
        let (observed, commands) = PlayerHandle::fixture(Status::default());
        app.player = observed;
        app.config.power.sleep_minutes = 0;
        app.config.power.radio_doze = false;
        app.config.display.backlight_seconds = 0;
        app.screen_on = false;
        app.battery_polled = Some(Instant::now());
        let mut probe = BatteryProbe::new(false);
        probe.value = Some(Battery { level: Some(88), plugged: true, charging: false });
        let (release, waiting) = tokio::sync::oneshot::channel();
        if held_screen { probe.pending_screen = Some(waiting); }
        else { probe.pending_battery = Some(waiting); }
        let (presented, mut frame) = tokio::sync::oneshot::channel();
        probe.presented = Some(presented);
        let (tx, mut inbox) = channel();
        tx.send(UiMsg::Input(Raw::Key { key: crate::input::Key::Center, down: true })).unwrap();
        let mut trace = None;
        crate::rt::Ui::new().unwrap().block_on(async {
            let running = crate::run(&mut app, &mut probe, &mut inbox, &mut trace);
            tokio::pin!(running);
            poll_fn(|cx| { assert!(running.as_mut().poll(cx).is_pending()); Poll::Ready(()) }).await;
            tx.send(UiMsg::Input(Raw::Media(crate::input::Media::Next))).unwrap();
            poll_fn(|cx| { assert!(running.as_mut().poll(cx).is_pending()); Poll::Ready(()) }).await;
            responsive.push(matches!(commands.try_recv(), Ok(Cmd::Next)));
            assert!(matches!(frame.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)),
                "pending battery/display must not publish a visible frame");
            release.send(()).unwrap();
            let exit = tokio::spawn(async move { frame.await.unwrap(); tx.send(UiMsg::Quit).unwrap(); });
            assert_eq!(tokio::time::timeout(Duration::from_secs(2), running).await.unwrap(), 0);
            exit.await.unwrap();
        });
        assert!(app.screen_on && app.screen_restore.is_none());
        assert_eq!(app.battery, probe.value.unwrap());
        assert_eq!(probe.screens, [(true, 1)], "exactly one fresh read and completed enable");
    }
    // Preserve other UI controls when running this against the defective source.
    drop(exclusive);
    assert_eq!(responsive, vec![true, true], "input must reach the player while either wake helper is held");
}

#[test]
fn pending_idle_blank_keeps_wake_and_media_input_responsive() {
    use std::{future::{Future, poll_fn}, task::Poll};
    let exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let (mut app, player) = fixture();
    player.shutdown();
    let (observed, commands) = PlayerHandle::fixture(Status::default());
    app.player = observed;
    app.config.power.sleep_minutes = 0;
    app.config.power.radio_doze = false;
    app.config.display.backlight_seconds = 1;
    app.last_input = Instant::now() - Duration::from_secs(2);
    let mut probe = BatteryProbe::new(false);
    let (release, waiting) = tokio::sync::oneshot::channel();
    probe.pending_screen = Some(waiting);
    let (presented, frame) = tokio::sync::oneshot::channel();
    probe.presented = Some(presented);
    let (tx, mut inbox) = channel();
    tx.send(UiMsg::Headphones).unwrap();
    let mut trace = None;
    let responsive = crate::rt::Ui::new().unwrap().block_on(async {
        let running = crate::run(&mut app, &mut probe, &mut inbox, &mut trace);
        tokio::pin!(running);
        poll_fn(|cx| { assert!(running.as_mut().poll(cx).is_pending()); Poll::Ready(()) }).await;
        tx.send(UiMsg::Input(Raw::Key { key: crate::input::Key::Center, down: true })).unwrap();
        tx.send(UiMsg::Input(Raw::Media(crate::input::Media::Next))).unwrap();
        poll_fn(|cx| { assert!(running.as_mut().poll(cx).is_pending()); Poll::Ready(()) }).await;
        let responsive = matches!(commands.try_recv(), Ok(Cmd::Next));
        release.send(()).unwrap();
        let exit = tokio::spawn(async move { frame.await.unwrap(); tx.send(UiMsg::Quit).unwrap(); });
        assert_eq!(tokio::time::timeout(Duration::from_secs(2), running).await.unwrap(), 0);
        exit.await.unwrap();
        responsive
    });
    assert!(app.screen_on && app.screen_restore.is_none());
    assert_eq!(probe.screens.iter().map(|(on, _)| *on).collect::<Vec<_>>(), [false, true],
        "accepted blank must finish before the later wake enables display");
    drop(exclusive);
    assert!(responsive, "media input waited behind accepted screen blanking");
}

#[test]
fn pending_idle_blank_defers_sleep_and_explicit_power_until_completion() {
    let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    for explicit in [false, true] {
        let (mut app, player) = fixture();
        app.config.power.radio_doze = false;
        app.config.display.backlight_seconds = 1;
        let now = Instant::now();
        app.last_input = now - Duration::from_secs(61);
        let mut probe = BatteryProbe::new(false);
        let (release, waiting) = tokio::sync::oneshot::channel();
        probe.pending_screen = Some(waiting);
        let ui = crate::rt::Ui::new().unwrap();
        ui.block_on(app.tick_at_async(&mut probe, now));
        assert!(!app.screen_on && app.screen_blanking);
        assert!(app.pending.is_empty(), "automatic sleep cannot pass an unfinished blank");
        if explicit { app.power = Power::Ready { off: false }; }
        for _ in 0..8 { ui.block_on(app.tick_at_async(&mut probe, now)); }
        assert!(app.pending.is_empty());
        assert!(probe.screens.is_empty());
        release.send(()).unwrap();
        ui.block_on(app.tick_at_async(&mut probe, now));
        assert!(!app.screen_blanking && !app.screen_on);
        assert_eq!(probe.screens, [(false, 0)], "the accepted blank completes once");
        assert_eq!(app.pending.len(), 1);
        assert!(if explicit { matches!(app.pending[0], Request::EnterPower { off: false }) }
            else { matches!(app.pending[0], Request::Sleep) });
        player.shutdown();
    }
}

#[test]
fn quit_interrupts_pending_idle_blank_without_losing_accepted_work() {
    use std::{future::{Future, poll_fn}, task::Poll};
    let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let (mut app, player) = fixture();
    player.shutdown();
    app.config.power.sleep_minutes = 0;
    app.config.power.radio_doze = false;
    app.config.display.backlight_seconds = 1;
    app.last_input = Instant::now() - Duration::from_secs(2);
    let mut probe = BatteryProbe::new(false);
    let (release, waiting) = tokio::sync::oneshot::channel();
    probe.pending_screen = Some(waiting);
    let (tx, mut inbox) = channel();
    tx.send(UiMsg::Headphones).unwrap();
    let mut trace = None;
    let ui = crate::rt::Ui::new().unwrap();
    ui.block_on(async {
        let running = crate::run(&mut app, &mut probe, &mut inbox, &mut trace);
        tokio::pin!(running);
        poll_fn(|cx| { assert!(running.as_mut().poll(cx).is_pending()); Poll::Ready(()) }).await;
        tx.send(UiMsg::Quit).unwrap();
        assert_eq!(poll_fn(|cx| Poll::Ready(running.as_mut().poll(cx))).await, Poll::Ready(0));
    });
    assert!(!app.screen_on && app.screen_blanking && app.screen_restore.is_none());
    assert!(probe.screens.is_empty());
    assert_eq!(probe.reads, 0);
    release.send(()).unwrap();
    ui.block_on(app.restore_screen_async(&mut probe, Instant::now()));
    assert_eq!(probe.screens, [(false, 0)]);
    assert!(!app.screen_transitioning());
}

#[test]
fn pending_wake_preserves_one_read_boundary_and_swallows_wake_keys() {
    let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let (mut app, player) = fixture();
    let pages: Vec<_> = app.stack.iter().map(|(id, _)| *id).collect();
    app.screen_on = false;
    app.config.power.radio_doze = false;
    let now = Instant::now();
    let mut probe = BatteryProbe::new(false);
    let (release, wait) = tokio::sync::oneshot::channel();
    probe.pending_battery = Some(wait);
    crate::rt::Ui::new().unwrap().block_on(async {
        for (offset, down) in [(0, true), (1, false), (2, true), (3, false)] {
            let input = app.input_async(Raw::Key { key: crate::input::Key::Center, down }, now + Duration::from_millis(offset), &mut probe);
            tokio::time::timeout(Duration::from_secs(1), input).await.unwrap();
            assert_eq!(app.wake_measurement, Some(now), "repeated keys preserve the first wake boundary");
        }
        for i in 0..32 {
            app.tick_at_async(&mut probe, now + Duration::from_secs(70 + i)).await;
        }
        assert_eq!(probe.reads, 1, "periodic ticks cannot add reads or consume restoration's result");
        assert!(probe.screens.is_empty() && !app.needs_draw());
        assert!(app.pending.is_empty(), "pending restoration cannot enter automatic sleep");
        assert_eq!(app.stack.iter().map(|(id, _)| *id).collect::<Vec<_>>(), pages, "wake keys do not activate a menu");
        release.send(()).unwrap();
        app.restore_screen_async(&mut probe, now).await;
    });
    assert_eq!(probe.screens, [(true, 1)]);
    assert!(app.screen_on && app.screen_restore.is_none());
    player.send(Cmd::Quit);
}

#[test]
fn quit_interrupts_pending_wake_without_presenting_or_restarting_helpers() {
    use std::{future::{Future, poll_fn}, task::Poll};
    let exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut completed = Vec::new();
    for held_screen in [false, true] {
        let (mut app, player) = fixture();
        player.shutdown();
        app.config.power.sleep_minutes = 0;
        app.config.power.radio_doze = false;
        app.screen_on = false;
        app.battery_polled = Some(Instant::now());
        let mut probe = BatteryProbe::new(false);
        let (release, waiting) = tokio::sync::oneshot::channel();
        if held_screen { probe.pending_screen = Some(waiting); }
        else { probe.pending_battery = Some(waiting); }
        let (presented, mut frame) = tokio::sync::oneshot::channel();
        probe.presented = Some(presented);
        let (tx, mut inbox) = channel();
        tx.send(UiMsg::Input(Raw::Key { key: crate::input::Key::Center, down: true })).unwrap();
        let mut trace = None;
        crate::rt::Ui::new().unwrap().block_on(async {
            let running = crate::run(&mut app, &mut probe, &mut inbox, &mut trace);
            tokio::pin!(running);
            poll_fn(|cx| { assert!(running.as_mut().poll(cx).is_pending()); Poll::Ready(()) }).await;
            tx.send(UiMsg::Quit).unwrap();
            completed.push(poll_fn(|cx| Poll::Ready(running.as_mut().poll(cx))).await == Poll::Ready(0));
            release.send(()).unwrap();
        });
        assert!(matches!(frame.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Empty)));
        assert!(!app.screen_on && app.screen_restore.is_some());
        assert_eq!(probe.reads, 1);
    }
    drop(exclusive);
    assert_eq!(completed, vec![true, true]);
}

#[test]
fn periodic_battery_coalesces_pending_ticks_and_wake_discards_older_result() {
    use std::future::{Future, poll_fn};
    use std::task::Poll;
    let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let (mut app, player) = fixture();
    app.config.power.sleep_minutes = 0;
    app.config.power.radio_doze = false;
    app.config.display.backlight_seconds = 0;
    let mut probe = BatteryProbe::new(false);
    probe.value = Some(Battery { level: Some(25), ..Default::default() });
    let (release, wait) = tokio::sync::oneshot::channel();
    probe.pending_battery = Some(wait);
    let ui = crate::rt::Ui::new().unwrap();
    let now = Instant::now();
    for i in 0..100 {
        ui.block_on(app.tick_at_async(&mut probe, now + Duration::from_millis(i)));
    }
    assert_eq!(probe.reads, 1, "one accepted periodic request, not one job per tick");
    assert!(app.battery_refresh.is_some());
    assert!(app.battery_polled.is_none(), "incomplete data is not published");
    app.screen_on = false;
    ui.block_on(async {
        let wake = app.wake_at_async(&mut probe, now);
        tokio::pin!(wake);
        poll_fn(|cx| {
            assert!(wake.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        }).await;
    });
    assert!(app.battery_refresh.is_some(), "cancelled wake retains accepted older work");
    assert!(probe.screens.is_empty());
    probe.value = Some(Battery { level: Some(88), plugged: true, charging: false });
    release.send(()).unwrap();
    ui.block_on(app.wake_at_async(&mut probe, now));
    assert_eq!(app.battery, probe.value.unwrap(), "older periodic value cannot replace fresh wake state");
    assert_eq!(probe.screens, [(true, 2)]);
    assert!(app.battery_refresh.is_none());
    ui.block_on(app.tick_at_async(&mut probe, now));
    assert_eq!(probe.reads, 2, "no immediate duplicate observation after wake");
    player.send(Cmd::Quit);
}
