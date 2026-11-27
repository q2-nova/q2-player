use super::*;
use crate::{
    config::Config,
    library::{generated_test_directory, Paths},
};
use std::{sync::mpsc, time::Duration};

fn fixture() -> (Arc<Library>, Config) {
    let mut config = Config::default();
    config.server.url = "http://127.0.0.1:9".into();
    config.server.user = "generated".into();
    let (lib, _) = Library::create(
        Paths::new(generated_test_directory("music-actions")),
        &config,
        Arc::new(|_| {}),
    );
    (lib, config)
}
fn song() -> Song {
    Song {
        id: "generated-song".into(),
        ..Default::default()
    }
}
fn blocked(lib: &Library) -> mpsc::Sender<()> {
    let (entered, started) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    lib.db_post(move |_, _| {
        entered.send(()).unwrap();
        wait.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
    });
    started.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
    release
}
fn wait<T>(future: impl Future<Output = T>) -> T {
    crate::rt::Ui::new().unwrap().block_on(async {
        tokio::time::timeout(Duration::from_secs(3), future)
            .await
            .expect("music action did not complete")
    })
}

#[test]
fn rapid_actions_post_in_input_order_and_survive_cancelled_waiters() {
    let (lib, _) = fixture();
    let song = song();
    let release = blocked(&lib);
    let first = lib.star(ItemKind::Song, &song.id, true);
    assert!(lib.loved(&song));
    let second = lib.rate(&song.id, 4);
    assert_eq!(lib.rating(&song), 4);
    let third = lib.star(ItemKind::Song, &song.id, false);
    assert!(!lib.loved(&song));
    // No polling has been needed to post or display any of these intents.
    drop(first);
    release.send(()).unwrap();
    wait(third).unwrap();
    wait(second).unwrap();
    assert!(!lib.loved(&song));
    assert_eq!(lib.rating(&song), 4);
    let expected = vec![
        PendingCall::Star {
            kind: "song".into(),
            id: song.id.clone(),
            on: true,
        },
        PendingCall::Rating {
            id: song.id.clone(),
            rating: 4,
        },
        PendingCall::Star {
            kind: "song".into(),
            id: song.id.clone(),
            on: false,
        },
    ];
    let actual = lib
        .db()
        .pending(&crate::rt::Blocking::dedicated_thread(), 10)
        .into_iter()
        .map(|(_, call)| call)
        .collect::<Vec<_>>();
    assert_eq!(actual, expected);
    lib.prepare_sleep().unwrap();
    // Reopen the generated file, not merely the connection's in-memory view.
    let reopened = super::super::db::Db::open(&crate::rt::Blocking::dedicated_thread(), &lib.paths.database()).unwrap();
    assert_eq!(
        reopened
            .pending(&crate::rt::Blocking::dedicated_thread(), 10)
            .into_iter()
            .map(|(_, call)| call)
            .collect::<Vec<_>>(),
        expected
    );
}

#[test]
fn failed_actions_roll_back_only_latest_feedback_and_refuse_transition() {
    let (lib, _) = fixture();
    let song = song();
    wait(lib.rate(&song.id, 3)).unwrap();
    let release = blocked(&lib);
    lib.db_post(|_blocking, db| db.reject_music_actions(&crate::rt::Blocking::dedicated_thread(), true));
    let failed_heart = lib.star(ItemKind::Song, &song.id, true);
    let failed_rating = lib.rate(&song.id, 5);
    lib.db_post(|_blocking, db| db.reject_music_actions(&crate::rt::Blocking::dedicated_thread(), false));
    let newest_heart = lib.star(ItemKind::Song, &song.id, false);
    assert!(!lib.loved(&song));
    assert_eq!(lib.rating(&song), 5);
    release.send(()).unwrap();
    wait(newest_heart).unwrap();
    assert!(wait(failed_rating).unwrap_err().contains("Could not save"));
    assert!(wait(failed_heart).is_err());
    assert!(
        !lib.loved(&song),
        "an older failure must not undo the newer heart"
    );
    assert_eq!(
        lib.rating(&song),
        3,
        "latest failed rating returns to acknowledged value"
    );
    assert_eq!(lib.db().pending(&crate::rt::Blocking::dedicated_thread(), 10).len(), 2);
    assert!(lib
        .prepare_sleep()
        .unwrap_err()
        .contains("transition cancelled"));
    lib.prepare_sleep().unwrap(); // explicit failed transition acknowledged the error
}

#[test]
fn account_round_trip_rejects_queued_actions_but_transport_changes_preserve_them() {
    let (lib, mut config) = fixture();
    let song = song();
    wait(lib.star(ItemKind::Song, &song.id, true)).unwrap();
    let epoch = lib.music_epoch.load(Ordering::SeqCst);
    config.server.password = "new-generated-password".into();
    lib.configure(&config);
    assert_eq!(lib.music_epoch.load(Ordering::SeqCst), epoch);
    assert!(lib.loved(&song));
    // Exercise the real owner admission while the DB is held. The old action
    // posts first, but a later A→B→A intent invalidates it before execution.
    let release = blocked(&lib);
    let stale = lib.star(ItemKind::Song, &song.id, false);
    let original = config.server.user.clone();
    config.server.user = "other-generated".into();
    let middle = lib.configure_async(&config);
    config.server.user = original;
    let current = lib.configure_async(&config);
    release.send(()).unwrap();
    wait(middle).unwrap(); wait(current).unwrap();
    assert!(wait(stale).unwrap_err().contains("account changed"));
    // B was superseded before its binding could execute. A's already durable
    // state survives, while its queued pre-switch action is still rejected.
    assert_eq!(lib.db().pending(&crate::rt::Blocking::dedicated_thread(), 10).len(), 1);
    // Optimistic display feedback is scoped to the old epoch; the synthetic
    // Song has no server-provided favorite state to restore after switching.
    assert!(!lib.loved(&song));
    wait(lib.star(ItemKind::Song, &song.id, true)).unwrap();
    assert!(lib.loved(&song));
    assert_eq!(lib.db().pending(&crate::rt::Blocking::dedicated_thread(), 10).len(), 2);
}

#[test]
fn played_scrobble_cannot_cross_an_account_round_trip_while_database_waits() {
    use crate::audio::player::TrackSource;
    let (lib, mut config) = fixture();
    let release = blocked(&lib);
    crate::library::Tracks(lib.clone()).played(&song(), 1234);
    let original = config.server.user.clone();
    config.server.user = "other-generated".into();
    let middle = lib.configure_async(&config);
    config.server.user = original;
    let current = lib.configure_async(&config);
    release.send(()).unwrap();
    wait(middle).unwrap();
    wait(current).unwrap();
    let pending = wait(lib.db_run(|blocking, db| db.pending(blocking, 10)));
    assert!(pending.is_empty(), "a scrobble admitted before account replacement must be rejected");
}

#[test]
fn failed_scrobble_storage_is_reported_by_the_durability_barrier() {
    use crate::audio::player::TrackSource;
    let (lib, _) = fixture();
    lib.db_post(|blocking, db| db.reject_music_actions(blocking, true));
    crate::library::Tracks(lib.clone()).played(&song(), 1234);
    assert!(lib.prepare_sleep().is_err(), "failed scrobble persistence must not report a successful durability barrier");
    assert!(wait(lib.db_run(|blocking, db| db.pending(blocking, 10))).is_empty());
    lib.prepare_sleep().unwrap();
}

#[test]
fn played_scrobble_preserves_deferred_sqlite_sync() {
    use crate::audio::player::TrackSource;
    let (lib, _) = fixture();
    let connection = rusqlite::Connection::open(lib.paths.database()).unwrap();
    connection.execute_batch("CREATE TABLE generated_sync_modes (mode INTEGER); CREATE TRIGGER generated_record_sync AFTER INSERT ON pending BEGIN INSERT INTO generated_sync_modes SELECT synchronous FROM pragma_synchronous; END;").unwrap();
    crate::library::Tracks(lib.clone()).played(&song(), 1234);
    wait(lib.rate(&song().id, 4)).unwrap();
    let modes = connection.prepare("SELECT mode FROM generated_sync_modes ORDER BY rowid").unwrap()
        .query_map([], |row| row.get::<_, i32>(0)).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(modes, vec![1, 2], "scrobble inserts at NORMAL; acknowledged UI action inserts at FULL");
}

#[test]
fn played_scrobbles_preserve_transport_rebind_and_do_not_wake_sync() {
    use crate::audio::player::TrackSource;
    let (lib, mut config) = fixture();
    let tracks = crate::library::Tracks(lib.clone());
    let release = blocked(&lib);
    tracks.played(&song(), 1234);
    config.server.password = "new-generated-password".into();
    let rebound = lib.configure_async(&config);
    release.send(()).unwrap();
    wait(rebound).unwrap();
    assert_eq!(wait(lib.db_run(|blocking, db| db.pending(blocking, 10))).len(), 1);

    // Consume configuration's retained permit before observing the played path.
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(Box::pin(lib.sync_wake.notified()).as_mut().poll(&mut context).is_ready());
    tracks.played(&song(), 5678);
    let pending = wait(lib.db_run(|blocking, db| db.pending(blocking, 10)));
    assert_eq!(pending.into_iter().map(|(_, call)| call).collect::<Vec<_>>(), vec![
        PendingCall::Scrobble { id: song().id, time_ms: 1234 },
        PendingCall::Scrobble { id: song().id, time_ms: 5678 },
    ]);
    assert!(Box::pin(lib.sync_wake.notified()).as_mut().poll(&mut context).is_pending(),
        "completed-song persistence must retain the existing network batching policy");
}

#[test]
fn unpolled_now_playing_cannot_follow_an_account_round_trip() {
    use std::{io::{BufRead, BufReader, Write}, net::TcpListener, time::Instant};
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let (lib, mut config) = fixture();
    config.server.url = format!("http://{}", listener.local_addr().unwrap());
    lib.configure(&config);
    lib.set_online(true);
    let request = lib.scrobble_now_playing(&song()).unwrap();
    // No poll occurs until both account intents have been admitted/completed.
    let original = config.server.user.clone();
    config.server.user = "other-generated".into();
    let middle = lib.configure_async(&config);
    config.server.user = original;
    let current = lib.configure_async(&config);
    wait(middle).unwrap();
    wait(current).unwrap();
    lib.set_online(true);
    wait(request);
    assert!(matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock));

    // A track selected while the radio wakes must not issue a best-effort
    // now-playing request until its restored link is ready.
    let previously_admitted = lib.scrobble_now_playing(&song()).unwrap();
    lib.set_radio_doze(true);
    lib.set_radio_doze(false);
    assert!(lib.scrobble_now_playing(&song()).is_none());
    wait(previously_admitted);
    assert!(matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock));
    lib.link_available();

    // Positive control: a newly admitted request still uses the real wire path.
    let server = std::thread::spawn(move || {
        let deadline = Instant::now() + crate::library::TEST_LIVENESS;
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "new now-playing request was not sent");
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("generated listener failed: {error}"),
            }
        };
        stream.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        stream.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut first = String::new();
        reader.read_line(&mut first).unwrap();
        assert!(first.contains("scrobble") && first.contains("submission=false"));
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            if line == "\r\n" { break; }
        }
        let body = r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#;
        write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
    });
    wait(lib.scrobble_now_playing(&song()).unwrap());
    server.join().unwrap();
}

#[test]
fn sleep_barrier_waits_for_accepted_action_and_reports_abandoned_failure() {
    let (lib, _) = fixture();
    let release = blocked(&lib);
    lib.db_post(|_blocking, db| db.reject_music_actions(&crate::rt::Blocking::dedicated_thread(), true));
    drop(lib.rate(&song().id, 4));
    let (done, result) = mpsc::channel();
    let (entering, entered) = mpsc::channel();
    let saving = lib.clone();
    let barrier = std::thread::spawn(move || {
        entering.send(()).unwrap();
        done.send(saving.prepare_sleep()).unwrap();
    });
    entered.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
    assert!(matches!(
        result.recv_timeout(Duration::from_millis(10)),
        Err(mpsc::RecvTimeoutError::Timeout)
    ));
    release.send(()).unwrap();
    assert!(result
        .recv_timeout(Duration::from_secs(3))
        .unwrap()
        .unwrap_err()
        .contains("transition cancelled"));
    barrier.join().unwrap();
    assert!(lib.db().pending(&crate::rt::Blocking::dedicated_thread(), 10).is_empty());
}
