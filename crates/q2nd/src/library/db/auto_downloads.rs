use super::{Db, now, offline_groups::missing_music, queue_tracks, replace_art_owner};
use crate::rt::Blocking;
use rusqlite::{Connection, OptionalExtension, params};
use std::path::PathBuf;
use subsonic::Song;

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS auto_downloads (
    kind TEXT NOT NULL, id TEXT NOT NULL, namespace TEXT NOT NULL DEFAULT '',
    estimate INTEGER NOT NULL, added INTEGER NOT NULL, album TEXT, PRIMARY KEY(kind,id));
UPDATE offline_sets SET name='Offline Mix' WHERE kind='auto' AND id='drive' AND name='Fill My Drive';
";

impl Db {
    /// A legacy or explicitly kept song is already manual. An automatic
    /// copy remains eligible for the ordinary Download action to claim.
    pub(in crate::library) fn manually_downloaded_song(
        &self,
        _blocking: &Blocking,
        id: &str,
    ) -> rusqlite::Result<bool> {
        self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM offline_songs WHERE song_id=?1)
            AND NOT EXISTS(SELECT 1 FROM auto_downloads WHERE kind='music' AND id=?1)",
            [id],
            |row| row.get(0),
        )
    }

    pub(crate) fn auto_bytes(&self, namespace: &str) -> rusqlite::Result<(u64, u64)> {
        let used: i64 = self.conn.query_row("SELECT
            coalesce((SELECT sum(size) FROM offline_songs JOIN auto_downloads ON kind='music' AND id=song_id),0) +
            coalesce((SELECT sum(size) FROM spoken_downloads JOIN auto_downloads AS a ON a.kind='spoken' AND a.id=logical),0)",
            [], |row| row.get(0))?;
        let pending: i64 = self.conn.query_row("SELECT coalesce(sum(estimate),0) FROM auto_downloads AS a WHERE
            (kind='music' AND EXISTS(SELECT 1 FROM download_queue WHERE song_id=a.id AND error IS NULL))
            OR (kind='spoken' AND namespace=?1 AND EXISTS(SELECT 1 FROM spoken_download_queue WHERE logical=a.id AND error IS NULL))",
            [namespace], |row| row.get(0))?;
        Ok((used.max(0) as u64, pending.max(0) as u64))
    }

    pub(crate) fn auto_music(
        &self,
        blocking: &Blocking,
        album: Option<&str>,
        songs: &[(Song, u64)],
        budget: u64,
    ) -> rusqlite::Result<usize> {
        self.full_sync(blocking, |conn| {
            let tx = conn.unchecked_transaction()?;
            let mut added = Vec::new();
            let mut available = budget;
            for (song, size) in songs {
                if !missing_music(&tx, &song.id)? {
                    continue;
                }
                let Some(left) = available.checked_sub(*size) else {
                    // A random album is useful offline as a complete set.
                    if album.is_some() {
                        return Ok(0);
                    }
                    continue;
                };
                if tx.execute(
                    "INSERT OR IGNORE INTO auto_downloads VALUES('music',?1,'',?2,?3,?4)",
                    params![
                        song.id,
                        i64::try_from(*size).map_err(|_| rusqlite::Error::InvalidQuery)?,
                        now(),
                        album
                    ],
                )? == 0
                {
                    continue;
                }
                available = left;
                added.push(song.clone());
            }
            if !added.is_empty() {
                sync_auto_set(&tx)?;
                queue_tracks(blocking, &tx, &added)?;
            }
            tx.commit()?;
            Ok(added.len())
        })
    }

    pub(crate) fn auto_spoken(
        &self,
        blocking: &Blocking,
        namespace: &str,
        item: &str,
        episode: Option<&str>,
        title: &str,
        size: u64,
    ) -> rusqlite::Result<bool> {
        self.full_sync(blocking, |conn| {
            let tx = conn.unchecked_transaction()?;
            let logical = format!("abs:{namespace}:{item}:{}", episode.unwrap_or_default());
            let inserted = tx.execute("INSERT OR IGNORE INTO auto_downloads
                SELECT 'spoken',?2,?1,?5,?6,NULL WHERE
                NOT EXISTS (SELECT 1 FROM spoken_downloads WHERE logical=?2) AND
                NOT EXISTS (SELECT 1 FROM collection_members WHERE logical=?2) AND
                NOT EXISTS (SELECT 1 FROM spoken_download_queue WHERE logical=?2) AND
                (SELECT count(*) FROM spoken_download_queue) < 32",
                params![namespace, logical, item, episode, i64::try_from(size).map_err(|_| rusqlite::Error::InvalidQuery)?, now()])?;
            if inserted != 0 {
                tx.execute("INSERT INTO spoken_download_queue(namespace,logical,item,episode,title) VALUES(?1,?2,?3,?4,?5)",
                    params![namespace, logical, item, episode, title])?;
            }
            tx.commit()?;
            Ok(inserted != 0)
        })
    }

    /// Rotate at most one entry per pass. Constrain reaping to the selected
    /// automatic owner so orphaned files from old installations are untouched.
    pub(crate) fn rotate_automatic(
        &self,
        blocking: &Blocking,
        namespace: &str,
        before: Option<i64>,
        protected: &std::collections::HashSet<&str>,
        music: bool,
    ) -> rusqlite::Result<Option<Vec<PathBuf>>> {
        self.full_sync(blocking, |conn| {
            let tx = conn.unchecked_transaction()?;
            let now = now();
            if before.is_some() {
                let last = tx.query_row("SELECT CAST(value AS INTEGER) FROM kv WHERE key='auto-rotation'", [], |row| row.get::<_, i64>(0)).optional()?;
                if last.is_some_and(|last| now.saturating_sub(last) < 24 * 60 * 60) { return Ok(None); }
            }
            let candidates = {
                let mut query = tx.prepare("SELECT kind,id,album FROM auto_downloads AS a WHERE added<?1 AND
                    (kind='music' AND ?3 AND NOT EXISTS(SELECT 1 FROM download_queue WHERE song_id=a.id AND error IS NULL)
                    OR kind='spoken' AND namespace=?2 AND NOT EXISTS(SELECT 1 FROM spoken_download_queue WHERE logical=a.id AND error IS NULL)
                    AND NOT EXISTS(SELECT 1 FROM spoken_positions AS p WHERE p.id=a.id AND p.position_ms>0)
                    AND NOT EXISTS(SELECT 1 FROM spoken_played AS p WHERE p.logical=a.id)
                    AND NOT EXISTS(SELECT 1 FROM spoken_observed AS p WHERE p.logical=a.id AND p.percent>0))
                    ORDER BY added LIMIT 32")?;
                query.query_map(params![before.unwrap_or(i64::MAX), namespace, music], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<String>>(2)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            let mut eligible = None;
            for (kind, id, album) in candidates {
                let members = if let Some(album) = album {
                    let mut query = tx.prepare("SELECT id FROM auto_downloads WHERE kind='music' AND album=?1")?;
                    query.query_map([album], |row| row.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?
                } else {
                    vec![id.clone()]
                };
                if members.iter().any(|id| protected.contains(id.as_str())) { continue; }
                if kind == "music" {
                    let mut pending = false;
                    for member in &members {
                        pending |= tx.query_row("SELECT EXISTS(SELECT 1 FROM download_queue WHERE song_id=?1 AND error IS NULL)",
                            [member], |row| row.get::<_, bool>(0))?;
                    }
                    if pending { continue; }
                }
                eligible = Some((kind, id, members));
                break;
            }
            let Some((kind, id, members)) = eligible else { return Ok(None); };
            let mut files = Vec::new();
            if kind == "music" {
                for member in &members {
                    tx.execute("DELETE FROM auto_downloads WHERE kind='music' AND id=?1", [member])?;
                }
                sync_auto_set(&tx)?;
                for member in members {
                    let shared: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM offline_sets,json_each(offline_sets.songs) WHERE json_each.value=?1)", [&member], |row| row.get(0))?;
                    if !shared {
                        tx.execute("DELETE FROM download_queue WHERE song_id=?1", [&member])?;
                        let path = tx.query_row("DELETE FROM offline_songs WHERE song_id=?1 RETURNING path", [&member], |row| row.get::<_, String>(0)).optional()?;
                        files.extend(path.map(PathBuf::from));
                        files.extend(replace_art_owner(blocking, &tx, "music", &member, None)?);
                    }
                }
            } else {
                tx.execute("DELETE FROM auto_downloads WHERE kind='spoken' AND id=?1", [&id])?;
                tx.execute("DELETE FROM spoken_download_queue WHERE logical=?1", [&id])?;
                let mut query = tx.prepare("DELETE FROM spoken_downloads WHERE logical=?1 RETURNING path")?;
                files.extend(query.query_map([&id], |row| row.get::<_, String>(0).map(PathBuf::from))?.collect::<rusqlite::Result<Vec<_>>>()?);
                files.extend(replace_art_owner(blocking, &tx, "spoken", &id, None)?);
            }
            let mut unused = Vec::new();
            {
                let mut indexed = tx.prepare_cached("SELECT EXISTS(
                    SELECT 1 FROM offline_songs WHERE path=?1 UNION ALL
                    SELECT 1 FROM spoken_downloads WHERE path=?1 UNION ALL
                    SELECT 1 FROM song_cache WHERE path=?1 UNION ALL
                    SELECT 1 FROM offline_art WHERE path=?1)")?;
                for path in files {
                    if !indexed.query_row([path.to_string_lossy()], |row| row.get::<_, bool>(0))? {
                        unused.push(path);
                    }
                }
            }
            if before.is_some() {
                tx.execute("INSERT INTO kv VALUES('auto-rotation',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [now.to_string()])?;
            }
            tx.commit()?;
            Ok(Some(unused))
        })
    }
}

pub(super) fn promote_music(conn: &Connection, ids: &[String]) -> rusqlite::Result<()> {
    let mut remove_mark =
        conn.prepare_cached("DELETE FROM auto_downloads WHERE kind='music' AND id=?1")?;
    let mut changed = false;
    for id in ids {
        changed |= remove_mark.execute([id])? != 0;
    }
    if changed {
        sync_auto_set(conn)?;
    }
    Ok(())
}

pub(super) fn sync_auto_set(conn: &Connection) -> rusqlite::Result<()> {
    let ids: String = conn.query_row(
        "SELECT json_group_array(id) FROM
        (SELECT id FROM auto_downloads WHERE kind='music' ORDER BY added,id)",
        [],
        |row| row.get(0),
    )?;
    if ids == "[]" {
        conn.execute(
            "DELETE FROM offline_sets WHERE kind='auto' AND id='drive'",
            [],
        )?;
    } else {
        conn.execute(
            "INSERT INTO offline_sets VALUES('auto','drive','Offline Mix','',NULL,?1,?2)
            ON CONFLICT(kind,id) DO UPDATE SET songs=excluded.songs",
            params![ids, now()],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rename_keeps_existing_mix_members_and_same_named_manual_playlist() {
        let blocking = &Blocking::dedicated_thread();
        let db = Db::memory(blocking);
        for (kind, id) in [("auto", "drive"), ("playlist", "manual")] {
            db.conn.execute(r#"INSERT INTO offline_sets VALUES(?1,?2,'Fill My Drive','',NULL,'["song"]',9)"#,
                [kind, id]).unwrap();
        }
        db.conn.execute_batch(SCHEMA).unwrap();
        let mix = db.offline_set(blocking, "auto", "drive").unwrap();
        assert_eq!(mix.name, "Offline Mix");
        assert_eq!(mix.song_ids, ["song"]);
        assert_eq!(db.offline_set(blocking, "playlist", "manual").unwrap().name, "Fill My Drive");
    }

    #[test]
    fn another_shelf_accounts_completed_audio_counts_toward_card_share() {
        let blocking = &Blocking::dedicated_thread();
        let db = Db::memory(blocking);
        let id = "abs:former:published:";
        assert!(
            db.auto_spoken(blocking, "former", "published", None, "Published", 40)
                .unwrap()
        );
        let (ticket, _, _, _, _) = db.next_spoken(blocking, "former").unwrap().unwrap();
        db.put_spoken_downloads(blocking, id, &[(id.into(), PathBuf::from("generated"), 40)])
            .unwrap();
        db.finish_spoken_queue(blocking, ticket, None).unwrap();
        assert!(
            db.auto_spoken(blocking, "former", "waiting", None, "Waiting", 20)
                .unwrap()
        );
        assert!(
            db.auto_spoken(blocking, "current", "waiting", None, "Waiting", 10)
                .unwrap()
        );
        assert_eq!(db.auto_bytes("current").unwrap(), (40, 10));
        assert_eq!(db.auto_bytes("former").unwrap(), (40, 20));
        assert!(
            db.rotate_automatic(blocking, "current", None, &Default::default(), false)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            db.spoken_download(blocking, id).unwrap(),
            Some(PathBuf::from("generated"))
        );
    }

    #[test]
    fn repeated_mixed_rotation_after_restart_keeps_manual_tracks_books_and_episodes() {
        let blocking = &Blocking::dedicated_thread();
        let root = crate::library::generated_test_directory("automatic-mix");
        std::fs::create_dir_all(&root).unwrap();
        let database = root.join("library.db");
        let namespace = "generated-abs";
        let mut manual = Vec::new();
        for cycle in 0..2 {
            let db = Db::open(blocking, &database).unwrap();
            let song = |suffix| Song {
                id: format!("music-{cycle}-{suffix}"),
                ..Song::default()
            };
            let album = song("random-album");
            let favorite = song("favorite");
            let keep_music = song("keep");
            for (owned_album, media) in [
                (Some("random-album"), &album),
                (None, &favorite),
                (None, &keep_music),
            ] {
                assert_eq!(
                    db.auto_music(blocking, owned_album, &[(media.clone(), 4)], 4)
                        .unwrap(),
                    1
                );
                let path = root.join(&media.id);
                std::fs::write(&path, b"song").unwrap();
                db.put_offline_song(blocking, media, &path, 4);
                db.finish_download(blocking, &media.id, None);
            }
            let keep_set = super::super::OfflineSet {
                kind: "song".into(),
                id: keep_music.id.clone(),
                name: keep_music.id.clone(),
                artist: String::new(),
                cover_art: None,
                song_ids: vec![keep_music.id.clone()],
            };
            assert!(
                !db.manually_downloaded_song(blocking, &keep_music.id)
                    .unwrap()
            );
            db.enqueue_download(blocking, &keep_set, std::slice::from_ref(&keep_music))
                .unwrap();
            assert!(
                db.manually_downloaded_song(blocking, &keep_music.id)
                    .unwrap()
            );
            db.finish_download(blocking, &keep_music.id, None);
            manual.push(root.join(&keep_music.id));
            for (suffix, episode, preserve) in [
                ("book-rotate", None, false),
                ("episode-rotate", Some("episode"), false),
                ("book-keep", None, true),
                ("episode-keep", Some("episode"), true),
            ] {
                let item = format!("{cycle}-{suffix}");
                let logical = format!("abs:{namespace}:{item}:{}", episode.unwrap_or_default());
                assert!(
                    db.auto_spoken(blocking, namespace, &item, episode, &item, 4)
                        .unwrap()
                );
                let path = root.join(&item);
                std::fs::write(&path, b"book").unwrap();
                let (ticket, _, _, _, _) = db.next_spoken(blocking, namespace).unwrap().unwrap();
                db.put_spoken_downloads(blocking, &logical, &[(logical.clone(), path.clone(), 4)])
                    .unwrap();
                db.finish_spoken_queue(blocking, ticket, None).unwrap();
                if preserve {
                    db.enqueue_spoken(blocking, namespace, &logical, &item, episode, &item)
                        .unwrap();
                    if let Some((ticket, _, _, _, _)) = db.next_spoken(blocking, namespace).unwrap()
                    {
                        db.finish_spoken_queue(blocking, ticket, None).unwrap();
                    }
                    manual.push(path);
                }
            }
            assert_eq!(db.auto_bytes(namespace).unwrap(), (16, 0));
            drop(db);

            let db = Db::open(blocking, &database).unwrap();
            db.conn
                .execute("UPDATE auto_downloads SET added=0", [])
                .unwrap();
            let mut paths = std::collections::HashSet::new();
            for _ in 0..4 {
                // Advance the maintenance window without waiting a day.
                db.conn
                    .execute("UPDATE kv SET value='0' WHERE key='auto-rotation'", [])
                    .unwrap();
                let removed = db
                    .rotate_automatic(
                        blocking,
                        namespace,
                        Some(now() + 1),
                        &Default::default(),
                        true,
                    )
                    .unwrap()
                    .unwrap();
                assert_eq!(removed.len(), 1);
                let path = removed.into_iter().next().unwrap();
                assert!(!manual.contains(&path));
                std::fs::remove_file(&path).unwrap();
                paths.insert(path.file_name().unwrap().to_string_lossy().into_owned());
            }
            assert_eq!(
                paths,
                [
                    album.id,
                    favorite.id,
                    format!("{cycle}-book-rotate"),
                    format!("{cycle}-episode-rotate")
                ]
                .into_iter()
                .collect()
            );
            assert_eq!(db.auto_bytes(namespace).unwrap(), (0, 0));
            assert!(manual.iter().all(|path| path.is_file()));
        }
    }

    #[test]
    fn automatic_album_fits_as_a_whole_and_rotates_as_a_whole() {
        let blocking = &Blocking::dedicated_thread();
        let db = Db::memory(blocking);
        let songs = ["album-first", "album-second"].map(|id| Song {
            id: id.into(),
            ..Song::default()
        });
        let selections = songs.clone().map(|song| (song, 100));
        assert_eq!(
            db.auto_music(blocking, Some("album"), &selections, 199)
                .unwrap(),
            0
        );
        assert_eq!(db.auto_bytes("").unwrap(), (0, 0));
        assert_eq!(
            db.auto_music(blocking, Some("album"), &selections, 200)
                .unwrap(),
            2
        );
        for song in &songs {
            db.put_offline_song(blocking, song, std::path::Path::new(&song.id), 100);
            db.finish_download(blocking, &song.id, None);
        }
        let protected = ["album-first"].into_iter().collect();
        assert!(
            db.rotate_automatic(blocking, "", None, &protected, true)
                .unwrap()
                .is_none()
        );
        let removed = db
            .rotate_automatic(blocking, "", None, &Default::default(), true)
            .unwrap()
            .unwrap();
        assert_eq!(removed.len(), 2);
        assert_eq!(db.auto_bytes("").unwrap(), (0, 0));
    }

    #[test]
    fn auto_rotation_and_off_switch_preserve_manual_and_protected_ownership() {
        let blocking = &Blocking::dedicated_thread();
        let db = Db::memory(blocking);
        let songs = ["generated-manual", "generated-playing", "generated-rotate"].map(|id| Song {
            id: id.into(),
            ..Song::default()
        });
        let selections = songs.clone().map(|song| (song, 64));
        assert_eq!(
            db.auto_music(blocking, None, &selections, u64::MAX)
                .unwrap(),
            3
        );
        assert_eq!(db.auto_bytes("").unwrap(), (0, 192));
        assert!(
            db.next_eligible_download(blocking, false)
                .unwrap()
                .is_none()
        );
        let first = songs[0].clone();
        let manual = super::super::OfflineSet {
            kind: "song".into(),
            id: first.id.clone(),
            name: first.id.clone(),
            artist: String::new(),
            cover_art: None,
            song_ids: vec![first.id.clone()],
        };
        db.enqueue_download(blocking, &manual, std::slice::from_ref(&first))
            .unwrap();
        assert_eq!(db.auto_bytes("").unwrap(), (0, 128));
        let selected = db.next_eligible_download(blocking, false).unwrap().unwrap();
        assert_eq!(selected.song.id, first.id);
        assert!(!selected.automatic);
        for song in &songs {
            db.put_offline_song(blocking, song, std::path::Path::new(&song.id), 64);
            db.finish_download(blocking, &song.id, None);
        }
        let protected = [songs[1].id.as_str()].into_iter().collect();
        let removed = db
            .rotate_automatic(blocking, "", Some(now() + 1), &protected, true)
            .unwrap()
            .unwrap();
        assert_eq!(removed, [PathBuf::from(&songs[2].id)]);
        assert!(db.offline_song(blocking, &first.id).is_some());
        assert!(db.offline_song(blocking, &songs[1].id).is_some());
        assert!(db.offline_song(blocking, &songs[2].id).is_none());
        assert_eq!(db.auto_bytes("").unwrap(), (64, 0));
    }
}
