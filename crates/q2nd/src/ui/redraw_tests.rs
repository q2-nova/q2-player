//! Deterministic, generated actual-App redraw observations. No owner inputs.
use super::*;
use crate::audio::{alsa::SoftHw, player};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};

fn generated_root() -> std::path::PathBuf {
    std::env::var_os("Q2_GENERATED_TEST_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build/perf"))
        .join("ui-redraw")
}

fn fixture() -> App {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    let root = generated_root().join(format!(
        "generated-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    std::fs::create_dir_all(&root).unwrap();
    let paths = Paths::new(root.clone());
    let mut config = Config::default();
    config.display.clock_in_title = false;
    config.display.night_warmth = false;
    config.display.backlight_seconds = 0;
    config.display.clicker = false;
    config.power.sleep_minutes = 0;
    let lib = Library::open(paths.clone(), &config, Arc::new(|_| {}));
    let player = player::spawn(Arc::new(crate::library::Tracks(lib.clone())), Box::new(SoftHw("null".into())),
        player::Settings { volume: config.playback.volume, headphone_power: crate::config::HeadphonePower::Standard, filter: 0,
            replay_gain: config.playback.replay_gain, gapless: true, eq: config.playback.eq.clone() }, Arc::new(|| {}));
    let net = Net::new(root.join("unused-generated-wifi.sh"), true, Arc::new(|| {}));
    let tailscale = crate::tailscale::Tailscale::new(root.clone(), root.clone(), true, false);
    let (tx, _) = channel();
    let mut app = App::new(lib, player, config, paths, net, tailscale, Fonts::bundled(), tx);
    app.stack.truncate(1);
    app
}
fn status(duration: u64, radio: bool) -> Status {
    Status { generation: 11, state: PlayState::Playing, duration_ms: duration,
        format: Some("Generated FLAC 16/44.1".into()),
        queue: Arc::new(vec![subsonic::Song { id: "generated-redraw".into(), title: "Generated Redraw".into(),
            artist: "Synthetic Author".into(), radio_url: radio.then(|| "http://127.0.0.1:1/generated".into()),
            ..Default::default() }]), ..Default::default() }
}
fn setup(app: &mut App, status: &Status, scene: &str) {
    app.status = status.clone();
    if scene == "static" { app.push(Box::new(screens::settings::Settings::new(screens::settings::Page::Playback))); }
    else { app.apply(Action::NowPlaying); }
    if scene == "popup" { app.push(Box::new(screens::popup::Popup::new("Generated Popup").add("Generated choice", |_| Action::Pop))); }
    if scene == "dark" { app.screen_on = false; }
}
fn outside_progress_same(a: &[u32], b: &[u32]) -> bool {
    a.iter().zip(b).enumerate().all(|(i,(a,b))| {
        let x=i as i32 % WIDTH; let y=i as i32 / WIDTH;
        (30..WIDTH-30).contains(&x) && (STATUS_H+236..STATUS_H+246).contains(&y) || a==b
    })
}
fn player_scene(name: &str, duration: u64, speed: u16, scene: &str) -> Value {
    let (mut app, mut oracle) = (fixture(), fixture());
    let mut next = status(duration, scene == "radio");
    next.speed_percent=speed;
    setup(&mut app,&next,scene); setup(&mut oracle,&next,scene);
    let mut presented=app.render().pixels.clone();
    let mut previous=oracle.render().pixels.clone();
    let (mut draws,mut unchanged,mut visual_changes,mut held_progress)=(0,0,0,0);
    for step in 1..=50 {
        next.position_ms=u64::from(step as u32)*200*u64::from(speed)/100;
        app.update_player(next.clone()); oracle.update_player(next.clone());
        let needed=app.needs_draw();
        let expected=oracle.render().pixels.clone();
        visual_changes+=usize::from(expected!=previous);
        if needed {
            draws+=1;
            unchanged+=usize::from(expected==presented);
            presented=app.render().pixels.clone();
            assert!(presented==expected,"{name}: presented frame matches full renderer");
        } else if !matches!(scene,"popup"|"dark") {
            assert!(outside_progress_same(&presented,&expected),"{name}: only ordinary subsecond progress may wait");
            held_progress+=usize::from(presented!=expected);
        }
        previous=expected;
    }
    app.player.shutdown(); oracle.player.shutdown();
    json!({"scene":name,"updates":50,"draws":draws,"pixel_changes_at_200ms":visual_changes,
        "unchanged_draws":unchanged,"held_progress_frames":held_progress,"media_speed_percent":speed})
}

#[test]
fn actual_app_redraw_census_uses_generated_media_time_and_full_frame_oracle() {
    let _exclusive=crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let rows=vec![
        player_scene("static-settings",7_200_000,100,"static"),
        player_scene("long-now-playing",7_200_000,100,"normal"),
        player_scene("fractional-duration",7_200_513,100,"normal"),
        player_scene("short-now-playing",12_000,100,"normal"),
        player_scene("half-speed",7_200_000,50,"normal"),
        player_scene("triple-speed",7_200_000,300,"normal"),
        player_scene("radio-stable-metadata",0,100,"radio"),
        player_scene("popup-over-playback",7_200_000,100,"popup"),
        player_scene("dark-presentation",7_200_000,100,"dark"),
    ];
    let mut app=fixture(); let mut oracle=fixture(); let next=status(7_200_000,false);
    setup(&mut app,&next,"normal"); setup(&mut oracle,&next,"normal");
    app.render(); oracle.render();
    let start=Instant::now();
    app.night.tick_at(false,0,start); oracle.night.tick_at(false,0,start);
    let mut warmth_draws=0;
    for step in 1..=240 {
        let now=start+Duration::from_millis(step*250);
        if app.night.tick_at(true,22*3600,now) { app.dirty=true; }
        oracle.night.tick_at(true,22*3600,now);
        if app.needs_draw() {
            warmth_draws+=1;
            assert!(app.render().pixels==oracle.render().pixels,"warmth stays immediate and pixel-exact");
        }
    }
    app.player.shutdown(); oracle.player.shutdown();
    let root=generated_root();
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("actual-app-observations.json"),serde_json::to_vec_pretty(&json!({
        "scope":"deterministic generated actual-App status/render trace; not wall-time notification frequency, physical CPU or energy",
        "rows":rows,"warmth_250ms_steps":240,"warmth_draws":warmth_draws})).unwrap()).unwrap();
}

#[test]
fn ordinary_progress_uses_both_media_second_labels_and_all_mandatory_updates_remain_immediate() {
    let _exclusive=crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut app=fixture(); let mut oracle=fixture();
    let mut next=status(1_500,false);
    setup(&mut app,&next,"normal"); setup(&mut oracle,&next,"normal");
    app.render(); oracle.render();
    // Remaining crosses at501ms although elapsed stays0; this must not use
    // wall-clock throttling or only the elapsed label.
    for (position,required) in [(499,false),(500,false),(501,true),(999,false),(1000,true),(1001,false)] {
        next.position_ms=position;
        app.update_player(next.clone()); oracle.update_player(next.clone());
        assert_eq!(app.needs_draw(),required,"fractional-duration label boundary at{position}ms");
        if required { assert!(app.render().pixels==oracle.render().pixels); }
    }
    let mut seed=0x51324652u32;
    for _ in 0..512 {
        seed=seed.wrapping_mul(1664525).wrapping_add(1013904223);
        next.position_ms=u64::from(seed%1499);
        let old=app.status.position_ms;
        let needed=old/1000!=next.position_ms/1000 || (1500-old)/1000!=(1500-next.position_ms)/1000;
        app.update_player(next.clone()); oracle.update_player(next.clone());
        assert_eq!(app.needs_draw(),needed,"seeded media position cadence");
        if needed { assert!(app.render().pixels==oracle.render().pixels); }
    }
    for field in 0..24 {
        match field {
            0=>next.seek_revision+=1, 1=>next.duration_ms+=17,
            2=>next.format=Some("Generated MP3".into()),
            3=>next.origin=Some(crate::audio::player::Origin::Streaming),
            4=>next.radio_title=Some("Generated radio metadata".into()),
            5=>next.shuffle=true, 6=>next.repeat=crate::audio::player::Repeat::All,
            7=>next.speed_request+=1, 8=>next.speed_percent=125,
            9=>next.speed_override=Some(125), 10=>next.error=Some("Generated fault".into()),
            11=>next.generation+=1, 12=>next.state=PlayState::Buffering,
            13=>next.sample_rate=Some(48000), 14=>next.queue=Arc::new(next.queue.as_ref().clone()),
            15=>next.output_revision+=1, 16=>next.output_usb_card=Some(2),
            17=>next.output_usb_device=Some(1), 18=>next.output_owned=true,
            19=>next.pause_admission_failed=true, 20=>next.output_pause_kind=3,
            21=>next.output_pause_frames=4096, 22=>next.volume+=1, 23=>next.index=1,
            _=>unreachable!(),
        }
        app.update_player(next.clone()); oracle.update_player(next.clone());
        assert!(app.needs_draw(),"mandatory player field{field} stays immediate");
        assert!(app.render().pixels==oracle.render().pixels,"mandatory update is pixel-equivalent to full render");
    }
    app.player.shutdown(); oracle.player.shutdown();
}

#[test]
fn controls_buffering_and_return_from_hidden_views_present_latest_full_frame() {
    let _exclusive=crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let (mut app,mut oracle)=(fixture(),fixture());
    let mut next=status(7200_513,false);
    setup(&mut app,&next,"normal"); setup(&mut oracle,&next,"normal");
    app.render(); oracle.render();
    next.state=PlayState::Buffering; next.position_ms=0;
    app.update_player(next.clone()); oracle.update_player(next.clone());
    app.render(); oracle.render();
    next.position_ms=1;
    app.update_player(next.clone()); oracle.update_player(next.clone());
    assert!(app.needs_draw(),"Buffering label appears immediately even when both second labels stay unchanged");
    assert!(app.render().pixels==oracle.render().pixels);
    for event in [Event::Center,Event::Scroll(1),Event::Scroll(-1),Event::CenterLong,Event::Back] {
        app.event(event,&mut crate::platform::Headless {last:None});
        oracle.event(event,&mut crate::platform::Headless {last:None});
        assert!(app.needs_draw(),"controls keep immediate redraw");
        assert!(app.render().pixels==oracle.render().pixels,"control frame matches complete renderer");
    }
    app.push(Box::new(screens::popup::Popup::new("Generated Overlay").add("Generated choice", |_|Action::Pop)));
    oracle.push(Box::new(screens::popup::Popup::new("Generated Overlay").add("Generated choice", |_|Action::Pop)));
    app.render(); oracle.render();
    next.position_ms=12345;
    app.update_player(next.clone()); oracle.update_player(next.clone());
    for current in [&mut app,&mut oracle] { current.event(Event::Back,&mut crate::platform::Headless {last:None}); }
    assert!(app.needs_draw());
    assert!(app.render().pixels==oracle.render().pixels,"return from overlay uses latest state");
    app.screen_on=false; oracle.screen_on=false;
    next.position_ms=54321;
    app.update_player(next.clone()); oracle.update_player(next);
    assert!(!app.needs_draw());
    app.wake(&mut crate::platform::Headless {last:None});
    oracle.wake(&mut crate::platform::Headless {last:None});
    assert!(app.needs_draw());
    assert!(app.render().pixels==oracle.render().pixels,"first wake frame renders latest published player state");
    app.player.shutdown(); oracle.player.shutdown();
}


#[test]
fn presentation_clock_is_deferred_while_hidden_and_refreshes_on_visible_changes() {
    let _exclusive=crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut app=fixture(); let mut reads=0; let mut seed=0x5132434cu32;
    for step in 0..1024 {
        seed=seed.wrapping_mul(1664525).wrapping_add(1013904223);
        app.screen_on=seed&1!=0;
        app.config.display.clock_in_title=seed&2!=0;
        app.dirty=false;
        let old=app.shown_clock.clone();
        let next=format!("Generated minute{}",step/8);
        let expected=app.screen_on&&app.config.display.clock_in_title;
        let before=reads;
        app.refresh_clock(|_| { reads+=1; next.clone() });
        assert_eq!(reads-before,usize::from(expected),"hidden clock never formats local time");
        assert_eq!(app.shown_clock,if expected {next.clone()} else {old.clone()});
        assert_eq!(app.needs_draw(),expected&&next!=old);
    }
    app.screen_on=false; app.config.display.clock_in_title=true;
    app.shown_clock="Generated stale time".into();
    app.wake_at(&mut crate::platform::Headless {last:None},Instant::now());
    assert_ne!(app.shown_clock,"Generated stale time","wake refresh precedes first frame");
    assert!(!app.shown_clock.is_empty());
    assert!(app.needs_draw());
    app.player.shutdown();
}


#[test]
fn wake_reads_latest_real_player_snapshot_before_rendering() {
    let _exclusive=crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut app=fixture();
    app.status=Status::default(); app.screen_on=false;
    let generated=subsonic::Song {id:"generated-wake".into(),title:"Generated wake".into(),duration:180,..Default::default()};
    app.player.send(Cmd::Restore {songs:std::sync::Arc::new(vec![generated]),index:0,position_ms:54321});
    let deadline=Instant::now()+Duration::from_secs(3);
    while app.player.status().current().is_none() && Instant::now()<deadline {
        std::thread::sleep(Duration::from_millis(1));
    }
    let latest=app.player.status();
    assert_eq!(latest.current().unwrap().id,"generated-wake");
    assert_eq!(latest.position_ms,54321);
    assert!(app.status.current().is_none(),"hidden UI has not consumed any player messages");
    app.wake_at(&mut crate::platform::Headless {last:None},Instant::now());
    assert_eq!(app.status.current().unwrap().id,"generated-wake");
    assert_eq!(app.status.position_ms,54321,"first visible frame uses the latest published snapshot");
    assert!(app.needs_draw());
    app.player.shutdown();
}

#[test]
fn hidden_now_playing_progress_skips_identical_paints_and_controls_still_draw() {
    let _exclusive = crate::ui::TEST_FIXTURE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let (mut app, mut oracle) = (fixture(), fixture());
    let mut next = status(120_513, false);
    setup(&mut app, &next, "normal");
    setup(&mut oracle, &next, "normal");

    for current in [&mut app, &mut oracle] {
        current.volume_shown = Some(Instant::now());
    }
    let volume_pixels = app.render().pixels.clone();
    next.position_ms = 1_100;
    app.update_player(next.clone());
    oracle.update_player(next.clone());
    assert!(volume_pixels == oracle.render().pixels, "cursor changes leave the volume frame identical");
    assert!(!app.needs_draw(), "hidden elapsed and remaining seconds do not repaint the volume bar");

    let expiry = Instant::now() + VOLUME_TIME + Duration::from_millis(1);
    for current in [&mut app, &mut oracle] {
        current.tick_at(&mut crate::platform::Headless { last: None }, expiry);
    }
    assert!(app.needs_draw(), "volume expiry reveals current playback progress");
    assert!(app.render().pixels == oracle.render().pixels);

    for current in [&mut app, &mut oracle] {
        current.event(Event::Center, &mut crate::platform::Headless { last: None });
    }
    next.position_ms = 1_200;
    app.update_player(next.clone());
    oracle.update_player(next.clone());
    assert!(app.needs_draw(), "the scrubber keeps immediate cursor updates");
    assert!(app.render().pixels == oracle.render().pixels);
    for current in [&mut app, &mut oracle] {
        current.event(Event::Center, &mut crate::platform::Headless { last: None });
    }
    assert!(app.needs_draw(), "Center reveals rating immediately");
    let rating_pixels = app.render().pixels.clone();
    next.position_ms = 2_100;
    app.update_player(next.clone());
    oracle.update_player(next);
    assert!(rating_pixels == oracle.render().pixels, "cursor changes leave the rating frame identical");
    assert!(!app.needs_draw(), "hidden rating progress does not repaint");

    for current in [&mut app, &mut oracle] {
        current.event(Event::Scroll(1), &mut crate::platform::Headless { last: None });
    }
    assert!(app.needs_draw(), "the wheel updates the chosen rating immediately");
    assert!(app.render().pixels == oracle.render().pixels);
    for current in [&mut app, &mut oracle] {
        current.event(Event::Back, &mut crate::platform::Headless { last: None });
    }
    assert!(app.needs_draw(), "Back brings the current progress back immediately");
    assert!(app.render().pixels == oracle.render().pixels);
    app.player.shutdown();
    oracle.player.shutdown();
}
