//! Real-server generated-fixture gate. Exercises Library's actual worker and
//! normal artwork reader; never loads saved configuration or opens PCM.
use super::{Spec, Value, json};
use crate::config::{Config, Paths};
use crate::library::{Library, Policy, db::OfflineSet};
use md5::{Digest, Md5};
use serde::Deserialize;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Deserialize)]
struct Fixture {
    synthetic_credentials: bool,
    server: String,
    user: String,
    password: String,
    album_id: String,
    owned_scratch: PathBuf,
    expected_songs: Vec<Expected>,
}
#[derive(Deserialize)]
struct Expected {
    id: String,
    bytes: u64,
    md5: String,
}
fn failure(message: &str) -> String {
    message.into()
}
fn below_build(_blocking: &crate::rt::Blocking, path: &Path) -> Result<PathBuf, String> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .map_err(|_| failure("Cannot locate fixture workspace"))?;
    let path = path
        .canonicalize()
        .map_err(|_| failure("Explicit generated fixture path does not exist"))?;
    if !path.starts_with(root.join("build")) {
        return Err(failure(
            "Generated fixture paths must be within this worktree build/",
        ));
    }
    Ok(path)
}
fn fixture(blocking: &crate::rt::Blocking, spec: &Spec) -> Result<Fixture, String> {
    let path = below_build(blocking, &spec.fixture)?;
    if std::fs::metadata(&path)
        .map_err(|_| failure("Cannot inspect generated fixture"))?
        .len()
        > 64 * 1024
    {
        return Err(failure("Generated credential fixture is too large"));
    }
    let fixture: Fixture = serde_json::from_slice(
        &std::fs::read(path).map_err(|_| failure("Cannot read generated fixture"))?,
    )
    .map_err(|_| failure("Invalid generated credential fixture"))?;
    let port = fixture
        .server
        .strip_prefix("http://127.0.0.1:")
        .and_then(|port| port.parse::<u16>().ok());
    if !fixture.synthetic_credentials
        || port.is_none_or(|port| port == 0)
        || fixture.user.is_empty()
        || fixture.password.is_empty()
        || fixture.album_id.is_empty()
        || fixture.expected_songs.len() != 2
        || fixture.expected_songs.iter().any(|song| {
            song.id.is_empty()
                || song.bytes == 0
                || song.bytes > 32 * 1024 * 1024
                || song.md5.len() != 32
                || !song.md5.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    {
        return Err(failure(
            "Expected explicit synthetic loopback credentials and two bounded generated songs",
        ));
    }
    if below_build(blocking, &spec.scratch)? != below_build(blocking, &fixture.owned_scratch)? {
        return Err(failure(
            "Scratch does not match the generated fixture's owned path",
        ));
    }
    Ok(fixture)
}
fn digest(_blocking: &crate::rt::Blocking, path: &Path) -> Result<(u64, String), String> {
    let mut file =
        std::fs::File::open(path).map_err(|_| failure("Generated offline audio is missing"))?;
    let mut hash = Md5::new();
    let mut total = 0;
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|_| failure("Cannot read generated offline audio"))?;
        if count == 0 {
            break;
        }
        total += count as u64;
        if total > 32 * 1024 * 1024 {
            return Err(failure("Generated audio exceeded its bounded fixture size"));
        }
        hash.update(&buffer[..count]);
    }
    Ok((total, format!("{:x}", hash.finalize())))
}
fn set(song: &subsonic::Song) -> OfflineSet {
    OfflineSet {
        kind: "song".into(),
        id: song.id.clone(),
        name: song.title.clone(),
        artist: song.artist.clone(),
        cover_art: song.cover_art.clone(),
        song_ids: vec![song.id.clone()],
    }
}
fn index(blocking: &crate::rt::Blocking, lib: &Library) -> Result<Value, String> {
    let db = rusqlite::Connection::open_with_flags(
        lib.paths.database(),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|_| failure("Cannot inspect generated download index"))?;
    let count = |table: &str| {
        db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|_| failure("Cannot inspect generated download references"))
    };
    let mut query = db
        .prepare("SELECT path,size FROM offline_art ORDER BY key")
        .map_err(|_| failure("Cannot inspect generated artwork paths"))?;
    let rows = query
        .query_map([], |row| {
            Ok((
                PathBuf::from(row.get::<_, String>(0)?),
                u64::try_from(row.get::<_, i64>(1)?).map_err(|_| rusqlite::Error::InvalidQuery)?,
            ))
        })
        .map_err(|_| failure("Cannot inspect generated artwork paths"))?;
    let mut files = Vec::new();
    for row in rows {
        let (path, size) = row.map_err(|_| failure("Cannot inspect generated artwork row"))?;
        let found = lib.paths.locate_metadata(blocking, &path).ok().flatten()
            .ok_or_else(|| failure("Indexed generated cover is missing"))?;
        if !path.starts_with(lib.paths.offline_art()) || found.1.len() != size {
            return Err(failure("Generated artwork index/file mismatch"));
        }
        files.push(path);
    }
    // Both layouts (crate::shard).
    let physical_files = crate::shard::walk(blocking, &lib.paths.offline_art(), true, usize::MAX)
        .map_err(|_| failure("Cannot enumerate generated artwork files"))?
        .0.len() as u64;
    Ok(
        json!({"audio_rows":count("offline_songs")?,"art_rows":count("offline_art")?,"art_owners":count("offline_art_refs")?,"physical_art_files":physical_files,"art_paths":files}),
    )
}
fn expected_index(value: &Value, audio: u64, art: u64, owners: u64) -> Result<(), String> {
    if value["audio_rows"] != audio
        || value["art_rows"] != art
        || value["art_owners"] != owners
        || value["physical_art_files"] != art
    {
        return Err(failure("Unexpected generated audio/art ownership counts"));
    }
    Ok(())
}
fn read_audio(blocking: &crate::rt::Blocking, lib: &Library, fixture: &Fixture) -> Result<Value, String> {
    let mut audio = Vec::new();
    for expected in &fixture.expected_songs {
        let path = lib
            .offline_song_path(&expected.id)
            .ok_or_else(|| failure("Generated song was not downloaded"))?;
        if !path.starts_with(lib.paths.offline()) {
            return Err(failure(
                "Generated audio escaped its owned offline directory",
            ));
        }
        let (bytes, md5) = digest(blocking, &path)?;
        if bytes != expected.bytes || md5 != expected.md5 {
            return Err(failure(
                "Generated offline audio differs from its original fixture",
            ));
        }
        audio.push(json!({"id":expected.id,"bytes":bytes,"md5":md5}));
    }
    Ok(json!(audio))
}
fn read_art(_blocking: &crate::rt::Blocking, lib: &Library, cover: &str) -> Result<Value, String> {
    let mut images = Vec::new();
    for size in [50, 150, 320] {
        let deadline = Instant::now() + Duration::from_secs(10);
        let image = loop {
            if let Some(image) = lib.art.get(cover, size) {
                break image;
            }
            if Instant::now() >= deadline {
                return Err(failure(
                    "Normal ArtCache did not load the generated offline cover",
                ));
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        if image.width != size || image.height != size {
            return Err(failure("Unexpected generated artwork dimensions"));
        }
        let mut hash = Md5::new();
        for pixel in &image.pixels {
            hash.update(pixel.to_le_bytes());
        }
        images.push(json!({"size":size,"pixel_md5":format!("{:x}",hash.finalize())}));
    }
    Ok(json!(images))
}
pub(super) fn run(blocking: &crate::rt::Blocking, spec: &Spec) -> Result<Value, String> {
    if !["download", "offline", "cleanup"].contains(&spec.scene.as_str()) {
        return Err(failure(
            "download-owned scene must be download, offline or cleanup",
        ));
    }
    let fixture = fixture(blocking, spec)?;
    let mut config = Config::default();
    config.server.url = fixture.server.clone();
    config.server.user = fixture.user.clone();
    config.server.password = fixture.password.clone();
    config.storage.reserve_mb = 0;
    config.storage.prefetch_songs = 0;
    let lib = Library::open(Paths::new(spec.scratch.clone()), &config, Arc::new(|_| {}));
    let offline = spec.scene != "download";
    lib.set_offline_mode(offline);
    let album = lib
        .fetch(
            &subsonic::calls::album(&fixture.album_id),
            if offline {
                Policy::CacheOnly
            } else {
                Policy::NetworkOnly
            },
        )
        .map_err(|_| failure("Generated album fetch failed"))?
        .value;
    let cover = album
        .song
        .first()
        .and_then(|song| song.cover_art.as_deref())
        .filter(|cover| !cover.is_empty())
        .ok_or_else(|| failure("Real Navidrome did not supply generated song cover IDs"))?;
    if album.song.len() != 2
        || album
            .song
            .iter()
            .any(|song| song.cover_art.as_deref() != Some(cover))
        || fixture
            .expected_songs
            .iter()
            .any(|expected| !album.song.iter().any(|song| song.id == expected.id))
    {
        return Err(failure(
            "Expected two generated songs sharing one real cover ID",
        ));
    }
    if !offline {
        for song in &album.song {
            lib.download(set(song), std::slice::from_ref(song))?;
        }
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let status = lib.downloads.status(&lib);
            if status.failed != 0 {
                return Err(failure(
                    "Real generated music worker reported a failed download",
                ));
            }
            if status.queued == 0 && status.current.is_none() && lib.offline_ids().len() == 2 {
                break;
            }
            if Instant::now() >= deadline {
                return Err(failure(
                    "Real generated music download did not finish within 60 seconds",
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    let before = index(blocking, &lib)?;
    expected_index(&before, 2, 1, 2)?;
    let audio = read_audio(blocking, &lib, &fixture)?;
    let artwork = read_art(blocking, &lib, cover)?;
    let cleanup = if spec.scene == "cleanup" {
        let first = lib
            .offline_song_path(&album.song[0].id)
            .ok_or("Missing first generated song")?;
        let second = lib
            .offline_song_path(&album.song[1].id)
            .ok_or("Missing second generated song")?;
        lib.remove_download("song", &album.song[0].id)?;
        let shared = index(blocking, &lib)?;
        expected_index(&shared, 1, 1, 1)?;
        if first.exists() || !second.exists() {
            return Err(failure("First removal damaged generated audio ownership"));
        }
        let shared_art = read_art(blocking, &lib, cover)?;
        lib.remove_download("song", &album.song[1].id)?;
        let empty = index(blocking, &lib)?;
        expected_index(&empty, 0, 0, 0)?;
        if second.exists() {
            return Err(failure("Final generated audio file was not removed"));
        }
        Some(json!({"after_first":shared,"shared_art":shared_art,"after_last":empty}))
    } else {
        None
    };
    Ok(
        json!({"scope":"generated real Navidrome download/index/artwork gate; no PCM or Q2 hardware", "scene":spec.scene,"forced_offline":offline,"album_id":fixture.album_id,"cover_id":cover,"index":before,"audio":audio,"artwork":artwork,"cleanup":cleanup}),
    )
}
