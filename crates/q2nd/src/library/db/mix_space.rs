use super::{Db, replace_art_owner};
use crate::rt::Blocking;
use rusqlite::{OptionalExtension, params};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS mix_retired_audio (
    path TEXT PRIMARY KEY, device TEXT NOT NULL, inode TEXT NOT NULL);
";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::library) struct MixAudio {
    pub kind: String,
    pub id: String,
    pub album: Option<String>,
    pub paths: Vec<PathBuf>,
    pub art: Option<PathBuf>,
}

#[derive(Clone, Debug)]
pub(in crate::library) struct RetiredAudio {
    pub path: PathBuf,
    pub device: String,
    pub inode: String,
}

const MIX_ROWS: &str = "SELECT kind,id,album,
            CASE WHEN kind='music' THEN
                EXISTS(SELECT 1 FROM offline_songs WHERE song_id=a.id) AND
                NOT EXISTS(SELECT 1 FROM download_queue WHERE song_id=a.id) AND
                NOT EXISTS(SELECT 1 FROM offline_sets s,json_each(s.songs)
                    WHERE (s.kind<>'auto' OR s.id<>'drive') AND json_each.value=a.id)
            ELSE
                EXISTS(SELECT 1 FROM spoken_downloads WHERE logical=a.id) AND
                NOT EXISTS(SELECT 1 FROM spoken_download_queue WHERE logical=a.id) AND
                NOT EXISTS(SELECT 1 FROM collection_members WHERE logical=a.id) AND
                NOT EXISTS(SELECT 1 FROM spoken_positions WHERE id=a.id AND position_ms>0) AND
                NOT EXISTS(SELECT 1 FROM spoken_played WHERE logical=a.id) AND
                NOT EXISTS(SELECT 1 FROM spoken_observed WHERE logical=a.id AND percent>0)
            END
            FROM auto_downloads a";

impl Db {
    pub(in crate::library) fn manual_download_waiting(
        &self,
        namespace: &str,
    ) -> rusqlite::Result<bool> {
        self.conn.query_row(
            "SELECT EXISTS(
            SELECT 1 FROM download_queue AS q WHERE error IS NULL
                AND NOT EXISTS(SELECT 1 FROM auto_downloads WHERE kind='music' AND id=q.song_id)
            UNION ALL SELECT 1 FROM spoken_download_queue AS q WHERE namespace=?1 AND error IS NULL
                AND NOT EXISTS(SELECT 1 FROM auto_downloads WHERE kind='spoken' AND id=q.logical)
            UNION ALL SELECT 1 FROM collection_members WHERE namespace=?1 AND admitted=0)",
            [namespace],
            |row| row.get(0),
        )
    }

    /// List complete and exclusively Mix-owned units across all namespaces.
    /// An album is eligible only when every remaining Mix track is eligible.
    pub(in crate::library) fn mix_audio(
        &self,
        protected: &HashSet<&str>,
    ) -> rusqlite::Result<Vec<Vec<MixAudio>>> {
        let mut query = self
            .conn
            .prepare(&format!("{MIX_ROWS} ORDER BY added,kind,id"))?;
        let rows = query.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, bool>(3)?,
            ))
        })?;
        let mut groups = Vec::<(bool, Vec<MixAudio>)>::new();
        let mut albums = HashMap::new();
        for row in rows {
            let (kind, id, album, eligible) = row?;
            let index = match album.as_ref().filter(|_| kind == "music") {
                Some(album) => *albums.entry(album.clone()).or_insert_with(|| {
                    groups.push((true, Vec::new()));
                    groups.len() - 1
                }),
                None => {
                    groups.push((true, Vec::new()));
                    groups.len() - 1
                }
            };
            let group = &mut groups[index];
            group.0 &= eligible && !protected.contains(id.as_str());
            if !group.0 {
                continue;
            }
            group.1.push(self.mix_member(kind, id, album)?);
        }
        Ok(groups
            .into_iter()
            .filter_map(|(eligible, members)| eligible.then_some(members))
            .collect())
    }

    fn mix_member(
        &self,
        kind: String,
        id: String,
        album: Option<String>,
    ) -> rusqlite::Result<MixAudio> {
        let sql = if kind == "music" {
            "SELECT path FROM offline_songs WHERE song_id=?1"
        } else {
            "SELECT path FROM spoken_downloads WHERE logical=?1"
        };
        let mut paths = self.conn.prepare_cached(sql)?;
        let paths = paths
            .query_map([&id], |row| row.get::<_, String>(0).map(PathBuf::from))?
            .collect::<rusqlite::Result<_>>()?;
        let art = self.conn.query_row(
                "SELECT path FROM offline_art_refs AS r JOIN offline_art AS art ON art.key=r.art WHERE r.kind=?1 AND r.owner=?2",
                params![kind, id], |row| row.get::<_, String>(0).map(PathBuf::from),
            ).optional()?;
        Ok(MixAudio {
            kind,
            id,
            album,
            paths,
            art,
        })
    }

    pub(in crate::library) fn media_reference_paths(&self) -> rusqlite::Result<Vec<PathBuf>> {
        let mut query = self.conn.prepare(
            "SELECT path FROM offline_songs UNION ALL SELECT path FROM spoken_downloads
            UNION ALL SELECT path FROM song_cache UNION ALL SELECT path FROM offline_art",
        )?;
        query
            .query_map([], |row| row.get::<_, String>(0).map(PathBuf::from))?
            .collect()
    }

    pub(in crate::library) fn retired_mix_audio(&self) -> rusqlite::Result<Vec<RetiredAudio>> {
        let mut query = self
            .conn
            .prepare("SELECT path,device,inode FROM mix_retired_audio")?;
        query
            .query_map([], |row| {
                Ok(RetiredAudio {
                    path: PathBuf::from(row.get::<_, String>(0)?),
                    device: row.get(1)?,
                    inode: row.get(2)?,
                })
            })?
            .collect()
    }

    /// Called under the publication gate following filesystem planning. Repeat
    /// eligibility checks in the transaction, including the current queue.
    pub(in crate::library) fn retire_mix_audio(
        &self,
        blocking: &Blocking,
        members: &[MixAudio],
        files: &[RetiredAudio],
        art: &HashMap<PathBuf, RetiredAudio>,
        protected: &HashSet<&str>,
    ) -> rusqlite::Result<Option<Vec<RetiredAudio>>> {
        if let Some(album) = members.first().and_then(|member| member.album.as_ref()) {
            let count = self.conn.query_row(
                "SELECT count(*) FROM auto_downloads WHERE kind='music' AND album=?1",
                [album],
                |row| row.get::<_, i64>(0),
            )?;
            if count as usize != members.len() {
                return Ok(None);
            }
        }
        let mut check = self
            .conn
            .prepare_cached(&format!("{MIX_ROWS} WHERE kind=?1 AND id=?2"))?;
        for member in members {
            if protected.contains(member.id.as_str()) {
                return Ok(None);
            }
            let current = check
                .query_row(params![member.kind, member.id], |row| {
                    Ok((row.get::<_, Option<String>>(2)?, row.get::<_, bool>(3)?))
                })
                .optional()?;
            let Some((album, true)) = current else {
                return Ok(None);
            };
            if self.mix_member(member.kind.clone(), member.id.clone(), album)? != *member {
                return Ok(None);
            }
        }
        self.full_sync(blocking, |conn| {
            let tx = conn.unchecked_transaction()?;
            let mut music = false;
            let mut retired_art = Vec::new();
            for member in members {
                tx.execute(
                    "DELETE FROM auto_downloads WHERE kind=?1 AND id=?2",
                    params![member.kind, member.id],
                )?;
                if member.kind == "music" {
                    music = true;
                    tx.execute("DELETE FROM offline_songs WHERE song_id=?1", [&member.id])?;
                } else {
                    tx.execute(
                        "DELETE FROM spoken_downloads WHERE logical=?1",
                        [&member.id],
                    )?;
                }
                for path in replace_art_owner(blocking, &tx, &member.kind, &member.id, None)? {
                    if let Some(file) = art.get(&path) {
                        retired_art.push(file);
                    }
                }
            }
            if music {
                super::auto_downloads::sync_auto_set(&tx)?;
            }
            let retired: Vec<_> = files.iter().chain(retired_art).cloned().collect();
            for file in &retired {
                tx.execute(
                    "INSERT OR IGNORE INTO mix_retired_audio VALUES(?1,?2,?3)",
                    params![file.path.to_string_lossy(), file.device, file.inode],
                )?;
            }
            tx.commit()?;
            Ok(Some(retired))
        })
    }

    pub(in crate::library) fn clear_retired_mix_audio(
        &self,
        blocking: &Blocking,
        files: &[RetiredAudio],
    ) -> rusqlite::Result<()> {
        self.full_sync(blocking, |conn| {
            let tx = conn.unchecked_transaction()?;
            for file in files {
                tx.execute(
                    "DELETE FROM mix_retired_audio WHERE path=?1 AND device=?2 AND inode=?3",
                    params![file.path.to_string_lossy(), file.device, file.inode],
                )?;
            }
            tx.commit()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use subsonic::Song;

    fn music(db: &Db, blocking: &Blocking, id: &str, album: Option<&str>) -> Song {
        let song = Song {
            id: id.into(),
            ..Default::default()
        };
        assert_eq!(
            db.auto_music(blocking, album, &[(song.clone(), 20)], 20)
                .unwrap(),
            1
        );
        db.put_offline_song(blocking, &song, &PathBuf::from(id), 20);
        db.finish_download(blocking, id, None);
        song
    }

    #[test]
    fn pressure_sees_former_namespace_after_protected_front_and_protects_group_ownership() {
        let blocking = &Blocking::dedicated_thread();
        let db = Db::memory(blocking);
        let ids: Vec<_> = (0..40).map(|index| format!("playing-{index:02}")).collect();
        for id in &ids {
            music(&db, blocking, id, None);
        }
        music(&db, blocking, "owned-album-first", Some("group"));
        music(&db, blocking, "owned-album-second", Some("group"));
        db.conn.execute("INSERT INTO offline_sets VALUES('playlist','followed','Generated','',NULL,'[\"owned-album-first\"]',0)", []).unwrap();
        db.conn
            .execute("INSERT INTO followed_playlists VALUES('followed')", [])
            .unwrap();
        assert!(
            db.auto_spoken(blocking, "former", "book", None, "Generated", 20)
                .unwrap()
        );
        let logical = "abs:former:book:";
        let (ticket, _, _, _, _) = db.next_spoken(blocking, "former").unwrap().unwrap();
        db.put_spoken_downloads(
            blocking,
            logical,
            &[(logical.into(), PathBuf::from("book"), 20)],
        )
        .unwrap();
        db.finish_spoken_queue(blocking, ticket, None).unwrap();
        db.conn
            .execute("UPDATE auto_downloads SET added=0 WHERE kind='music'", [])
            .unwrap();

        let protected = ids.iter().map(String::as_str).collect();
        let candidates = db.mix_audio(&protected).unwrap();
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0][0].id, logical);
        // A manual request which was admitted after planning cannot be evicted.
        db.enqueue_spoken(blocking, "former", logical, "book", None, "Generated")
            .unwrap();
        assert!(
            db.retire_mix_audio(blocking, &candidates[0], &[], &HashMap::new(), &protected)
                .unwrap()
                .is_none()
        );
        assert!(db.spoken_download(blocking, logical).unwrap().is_some());
    }

    #[test]
    fn retirement_transaction_keeps_old_files_on_sql_failure_and_preserves_shared_art() {
        let blocking = &Blocking::dedicated_thread();
        let db = Db::memory(blocking);
        let song = music(&db, blocking, "generated-audio", None);
        let cover = super::super::OfflineArt {
            key: "generated-cover".into(),
            path: "generated-cover.png".into(),
            size: 25,
        };
        replace_art_owner(blocking, &db.conn, "music", &song.id, Some(&cover)).unwrap();
        replace_art_owner(
            blocking,
            &db.conn,
            "spoken",
            "generated-other",
            Some(&cover),
        )
        .unwrap();
        let unit = db.mix_audio(&HashSet::new()).unwrap().pop().unwrap();
        let record = |path: &str| RetiredAudio {
            path: path.into(),
            device: "4".into(),
            inode: "8".into(),
        };
        let audio = [record("generated-audio")];
        let art = [(cover.path.clone(), record("generated-cover.png"))]
            .into_iter()
            .collect();
        db.conn
            .execute_batch(
                "CREATE TRIGGER abort_retirement BEFORE DELETE ON offline_songs
            BEGIN SELECT RAISE(ABORT,'generated retirement failure'); END;",
            )
            .unwrap();
        assert!(
            db.retire_mix_audio(blocking, &unit, &audio, &art, &HashSet::new())
                .is_err()
        );
        assert!(db.offline_song(blocking, &song.id).is_some());
        assert!(db.retired_mix_audio().unwrap().is_empty());
        db.conn
            .execute_batch("DROP TRIGGER abort_retirement")
            .unwrap();
        let retired = db
            .retire_mix_audio(blocking, &unit, &audio, &art, &HashSet::new())
            .unwrap()
            .unwrap();
        assert_eq!(retired.len(), 1);
        assert!(db.media_reference_paths().unwrap().contains(&cover.path));
    }

    #[test]
    fn only_unshared_art_is_recorded_for_owned_retirement() {
        let blocking = &Blocking::dedicated_thread();
        let db = Db::memory(blocking);
        let song = music(&db, blocking, "generated-solo", None);
        let cover = super::super::OfflineArt {
            key: "generated-cover".into(),
            path: "generated-cover.png".into(),
            size: 25,
        };
        replace_art_owner(blocking, &db.conn, "music", &song.id, Some(&cover)).unwrap();
        let unit = db.mix_audio(&HashSet::new()).unwrap().pop().unwrap();
        let retired_cover = RetiredAudio {
            path: cover.path.clone(),
            device: "4".into(),
            inode: "9".into(),
        };
        let art = [(cover.path.clone(), retired_cover)].into_iter().collect();
        let retired = db
            .retire_mix_audio(blocking, &unit, &[], &art, &HashSet::new())
            .unwrap()
            .unwrap();
        assert_eq!(retired.len(), 1);
        assert_eq!(retired[0].path, cover.path);
        assert_eq!(db.retired_mix_audio().unwrap().len(), 1);
    }
}
