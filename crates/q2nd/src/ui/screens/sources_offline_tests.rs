//! Music browsing without Navidrome: Artists, an artist, Albums, an album
//! and Search list what is downloaded, from the card alone (MUSIC-003).
use super::*;
use crate::config::{Config, Paths};
use crate::rt::Blocking;

/// Generated songs: Alpha's two albums downloaded whole, one song of Beta's
/// album downloaded on its own (no album download), and a song only in the
/// stream cache's metadata, which is not a download.
fn generated_card(label: &str, offline_mode: bool) -> Arc<Library> {
    let root = crate::library::generated_test_directory(label);
    let mut config = Config::default();
    // Port 9 refuses: nothing here may depend on an answer.
    config.server.url = "http://127.0.0.1:9".into();
    config.server.user = "generated".into();
    config.server.password = "synthetic-only".into();
    let lib = Library::open(Paths::new(root.clone()), &config, Arc::new(|_| {}));
    lib.set_offline_mode(offline_mode);
    let song = |id: &str, title: &str, track: u32, album: &str, album_id: &str, artist: &str, year: i32| Song {
        id: id.into(), title: title.into(), track: Some(track), album: album.into(), album_id: Some(album_id.into()),
        artist: artist.into(), artist_id: Some(format!("artist-{}", artist.to_lowercase())), year: Some(year),
        cover_art: Some(format!("cover-{album_id}")), duration: 8, suffix: Some("flac".into()), ..Default::default()
    };
    let blocking = Blocking::dedicated_thread();
    let db = lib.db();
    for (album_id, album, year) in [("alpha-one", "Alpha One", 2001), ("alpha-two", "Alpha Two", 2002)] {
        // Stored in reverse: the album's order comes from its download.
        let songs: Vec<Song> = (1..=2).rev().map(|t| song(&format!("{album_id}-{t}"), &format!("{album} Song {t}"), t, album, album_id, "Alpha", year)).collect();
        for s in &songs { db.put_offline_song(&blocking, s, &root.join(format!("{}.flac", s.id)), 1); }
        let mut ids: Vec<String> = songs.iter().map(|s| s.id.clone()).collect();
        ids.reverse();
        db.put_offline_set(&blocking, &OfflineSet { kind: "album".into(), id: album_id.into(), name: album.into(),
            artist: "Alpha".into(), cover_art: Some(format!("cover-{album_id}")), song_ids: ids });
    }
    let single = song("beta-one-2", "Beta One Song 2", 2, "Beta One", "beta-one", "Beta", 2003);
    db.put_offline_song(&blocking, &single, &root.join("beta-one-2.flac"), 1);
    db.put_offline_set(&blocking, &OfflineSet { kind: "song".into(), id: single.id.clone(), name: single.title.clone(),
        artist: "Beta".into(), cover_art: single.cover_art.clone(), song_ids: vec![single.id.clone()] });
    drop(db);
    lib
}

fn titles(rows: &[Row]) -> Vec<&str> { rows.iter().map(|row| row.title.as_str()).collect() }

fn opens(row: &Row) -> Dest {
    match &row.action { RowAction::Open(dest) => dest.clone(), _ => panic!("{} does not open a page", row.title) }
}

fn browse(lib: Arc<Library>, policy: Policy) {
    let rt = crate::rt::Ui::new().unwrap();
    let artists = rt.block_on(Arc::new(Artists).load(lib.clone(), policy)).expect("Artists lists the downloaded artists");
    assert_eq!(titles(&artists), ["Alpha", "Beta"]);
    assert_eq!(artists[0].detail.as_deref(), Some("2 albums"));

    let Dest::Artist { id, name } = opens(&artists[0]) else { panic!("an artist row opens the artist") };
    let artist = rt.block_on(Arc::new(Artist { id, name }).load(lib.clone(), policy)).expect("an artist opens from the card");
    assert_eq!(titles(&artist), ["Alpha Two", "Alpha One"], "downloaded albums, newest first; no server-only Top Songs");

    let Dest::Album { id, name } = opens(&artist[1]) else { panic!("an album row opens the album") };
    let album = rt.block_on(Arc::new(AlbumPage::new(id, name)).load(lib.clone(), policy)).expect("a downloaded album opens");
    assert_eq!(titles(&album)[..3], ["Shuffle", "1. Alpha One Song 1", "2. Alpha One Song 2"]);
    assert!(album.iter().all(|row| !row.dim), "downloaded songs play");
    assert!(matches!(album[1].action, RowAction::Play { index: 0, .. }));
    assert!(titles(&album).contains(&"More by Alpha"));

    // A song downloaded on its own still lists its artist and album.
    let Dest::Artist { id, name } = opens(&artists[1]) else { panic!() };
    let beta = rt.block_on(Arc::new(Artist { id, name }).load(lib.clone(), policy)).unwrap();
    assert_eq!(titles(&beta), ["Beta One"]);
    let Dest::Album { id, name } = opens(&beta[0]) else { panic!() };
    let single = rt.block_on(Arc::new(AlbumPage::new(id, name)).load(lib.clone(), policy)).unwrap();
    assert_eq!(titles(&single)[..2], ["Shuffle", "2. Beta One Song 2"]);

    let albums = Arc::new(Albums::new(AlbumListType::AlphabeticalByName, "Albums".into()));
    let page = rt.block_on(albums.window_page(lib.clone(), 0, policy)).expect("Albums lists the downloaded albums");
    assert_eq!(titles(&page.rows), ["Shuffle Albums", "Alpha One", "Alpha Two", "Beta One"]);
    assert!(!page.more);

    let found = rt.block_on(Arc::new(SearchResults { query: "alpha t".into() }).load(lib.clone(), policy)).expect("Search finds downloads");
    assert_eq!(titles(&found), ["Alpha Two", "Alpha Two Song 1", "Alpha Two Song 2"]);
    let found = rt.block_on(Arc::new(SearchResults { query: "BETA".into() }).load(lib, policy)).unwrap();
    assert_eq!(titles(&found), ["Beta", "Beta One", "Beta One Song 2"]);
}

#[test]
fn offline_mode_browses_downloaded_artists_and_albums_from_the_card() {
    let lib = generated_card("offline-browse-mode", true);
    browse(lib.clone(), Policy::CacheOnly);
    browse(lib, Policy::NetworkFirst);
}

#[test]
fn unreachable_server_browses_downloaded_artists_and_albums_from_the_card() {
    // A cold start: Navidrome is not reached and nothing was ever cached.
    let lib = generated_card("offline-browse-unreachable", false);
    assert!(!lib.is_online());
    browse(lib.clone(), Policy::CacheOnly);
    browse(lib, Policy::NetworkFirst);
}

#[test]
fn online_a_cache_miss_waits_for_the_server_and_a_failed_request_uses_the_card() {
    let lib = generated_card("offline-browse-online", false);
    lib.mark_online_for_test(true);
    let rt = crate::rt::Ui::new().unwrap();
    // The menu's preview: no stored copy, so the network request decides.
    assert!(matches!(rt.block_on(Arc::new(Artists).load(lib.clone(), Policy::CacheOnly)), Err(LibError::Offline)));
    // The server refuses before it is marked unavailable: the card answers.
    let rows = rt.block_on(Arc::new(Artists).load(lib, Policy::NetworkFirst)).expect("the card after a failed request");
    assert_eq!(titles(&rows), ["Alpha", "Beta"]);
}
