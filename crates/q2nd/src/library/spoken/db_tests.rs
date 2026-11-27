use super::*;
use crate::config::{Config, Paths};
use crate::library::{Tracks, generated_test_directory};
use crate::audio::player::TrackSource;
use std::{future::Future, sync::mpsc, time::Duration};

fn fixture() -> Arc<Library> {
    Library::create(Paths::new(generated_test_directory("spoken-db-owner")), &Config::default(), Arc::new(|_| {})).0
}
fn book(part: usize, podcast: bool) -> Song {
    Song {
        id: format!("generated-{}-{part}", if podcast { "podcast" } else { "book" }),
        spoken: Some(Box::new(Spoken {
            id: if podcast { "generated-podcast" } else { "generated-book" }.into(),
            offset_ms: part as u64 * 1000, duration_ms: 20_000,
            source: SpokenSource::Audiobookshelf { namespace: "generated".into(), item: "item".into(), episode: podcast.then(|| "episode".into()), content_url: "unused".into() },
            ..Default::default()
        })), ..Default::default()
    }
}
fn wait<T>(future: impl Future<Output = T>) -> T {
    crate::rt::Ui::new().unwrap().block_on(async {
        tokio::time::timeout(crate::library::TEST_LIVENESS, future).await.expect("spoken DB work did not complete")
    })
}
fn block_db(lib: &Library) -> mpsc::Sender<()> {
    let (entered, started) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    lib.db_post(move |_, _| { entered.send(()).unwrap(); wait.recv_timeout(crate::library::TEST_LIVENESS).unwrap(); });
    started.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
    release
}

#[test]
fn spoken_inventory_and_opening_await_one_owner_without_blocking_runtime() {
    let lib = fixture();
    let songs = [book(1, false), book(0, false), book(0, true)];
    for song in &songs {
        let path = lib.paths.offline().join(format!("{}.m4b", song.id));
        std::fs::write(&path, b"generated downloaded bytes").unwrap();
        let db = lib.db();
        db.put_spoken(&crate::rt::Blocking::dedicated_thread(), song).unwrap();
        db.put_spoken_downloads(&crate::rt::Blocking::dedicated_thread(), &song.spoken.as_ref().unwrap().id, &[(song.id.clone(), path, 26)]).unwrap();
    }
    let release = block_db(&lib);
    wait(async {
        let work = async {
            tokio::join!(lib.downloaded_spoken_async(), lib.spoken_availability_async(), lib.spoken_queue_async("generated-book".into()))
        };
        tokio::pin!(work);
        tokio::select! {
            _ = &mut work => panic!("inventory escaped held DB owner"),
            _ = tokio::time::sleep(Duration::from_millis(5)) => {}
        }
        release.send(()).unwrap();
        let (downloaded, availability, queue) = work.await;
        let downloaded = downloaded.unwrap();
        assert_eq!(downloaded.len(), 2, "one row per downloaded logical book/episode");
        assert_eq!(availability, Availability { books: Some(true), podcasts: Some(true) });
        assert_eq!(queue.unwrap().iter().map(|song| song.spoken.as_ref().unwrap().offset_ms).collect::<Vec<_>>(), [0, 1000]);
    });
    std::fs::remove_file(lib.paths.offline().join(format!("{}.m4b", songs[2].id))).unwrap();
    assert_eq!(wait(lib.downloaded_spoken_async()).unwrap().len(), 1);
    assert_eq!(wait(lib.spoken_availability_async()).podcasts, Some(false));
}

#[test]
fn production_checkpoint_awaits_database_and_dropped_observer_keeps_durability() {
    let lib = fixture();
    let song = book(1, false);
    let release = block_db(&lib);
    let source = Arc::new(Tracks(lib.clone()));
    let checkpoint = source.checkpoint_async(song.clone(), 4500, None);
    wait(async {
        tokio::pin!(checkpoint);
        tokio::select! {
            _ = &mut checkpoint => panic!("checkpoint acknowledged before its DB write"),
            _ = tokio::time::sleep(Duration::from_millis(5)) => {}
        }
        release.send(()).unwrap();
        checkpoint.await.unwrap();
    });
    let position = lib.spoken_position("generated-book").unwrap().unwrap();
    assert_eq!(position.position_ms, 5500);
    let release = block_db(&lib);
    drop(lib.save_spoken_async(&song, 7000));
    release.send(()).unwrap();
    lib.db_worker.drain();
    let db = crate::library::db::Db::open(&crate::rt::Blocking::dedicated_thread(), &lib.paths.database()).unwrap();
    let saved = db.spoken_position(&crate::rt::Blocking::dedicated_thread(), "generated-book").unwrap().unwrap();
    assert_eq!(saved.position_ms, 8000);
    assert!(saved.revision > position.revision && saved.dirty);
}

#[test]
fn checkpoint_owner_reports_storage_error_without_acknowledging_or_replacing_position() {
    let lib = fixture();
    let song = book(0, false);
    wait(lib.save_spoken_async(&song, 1234)).unwrap();
    let before = lib.spoken_position("generated-book").unwrap().unwrap();
    let conn = rusqlite::Connection::open(lib.paths.database()).unwrap();
    conn.execute_batch("CREATE TRIGGER reject_checkpoint BEFORE UPDATE ON spoken_positions BEGIN SELECT RAISE(ABORT,'generated failure'); END;").unwrap();
    let error = wait(Arc::new(Tracks(lib.clone())).checkpoint_async(song, 5678, None)).unwrap_err();
    assert_eq!(error, "Unable to save listening position on the card");
    assert_eq!(lib.spoken_position("generated-book").unwrap().unwrap(), before);
}

#[test]
fn local_scan_keeps_bad_file_rows_and_publishes_metadata_through_owner() {
    let lib = fixture();
    let directory = lib.paths.media_root().join("audiobooks");
    let atom = |kind: &[u8], body: &[u8]| [(body.len() as u32 + 8).to_be_bytes().as_slice(), kind, body].concat();
    let mut mvhd = vec![0u8; 20];
    mvhd[12..16].copy_from_slice(&1000u32.to_be_bytes());
    mvhd[16..20].copy_from_slice(&60_000u32.to_be_bytes());
    std::fs::write(directory.join("Generated.m4b"), atom(b"moov", &atom(b"mvhd", &mvhd))).unwrap();
    std::fs::write(directory.join("Malformed.m4b"), b"generated invalid file").unwrap();
    let release = block_db(&lib);
    let (books, issues) = wait(async {
        let scan = lib.local_audiobooks_async();
        tokio::pin!(scan);
        tokio::select! {
            _ = &mut scan => panic!("scan completed before catalog publication"),
            _ = tokio::time::sleep(Duration::from_millis(5)) => {}
        }
        release.send(()).unwrap();
        scan.await.unwrap()
    });
    assert_eq!(books.len(), 1);
    assert_eq!(books[0].title, "Generated");
    assert_eq!(books[0].duration, 60);
    assert_eq!(issues, ["Cannot read audiobook: Malformed.m4b"]);
    let queue = wait(lib.spoken_queue_async("local-m4b:Generated.m4b".into())).unwrap();
    assert_eq!(queue, books);
    let info = wait(lib.book_info_async(crate::library::book_info::BookInfoTarget::Local { relative_path: "Generated.m4b".into() }, crate::library::Policy::CacheOnly)).unwrap();
    assert_eq!(info.title, "Generated");
    assert_eq!(info.duration_ms, 60_000);
}
