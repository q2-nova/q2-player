use super::*;
use crate::library::music_reconcile::MissingSong;
use crate::rt::Blocking;

impl Db {
    /// Called under the same publication permit as cache rename and indexing.
    pub(super) fn forget_cached_versions(
        tx: &rusqlite::Transaction<'_>,
        id: &str,
    ) -> rusqlite::Result<Vec<PathBuf>> {
        let mut deleted = tx.prepare("DELETE FROM song_cache WHERE song_id=?1 RETURNING path")?;
        let files = deleted
            .query_map([id], |r| Ok(PathBuf::from(r.get::<_, String>(0)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        tx.execute("DELETE FROM cached_song_meta WHERE song_id=?1", [id])?;
        Ok(files)
    }

    pub(crate) fn upgrade_ticket(
        &self,
        _blocking: &Blocking,
        ticket: i64,
    ) -> rusqlite::Result<bool> {
        self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM download_upgrades WHERE ticket=?1)",
            [ticket],
            |r| r.get(0),
        )
    }

    pub(crate) fn download_page(
        &self,
        _blocking: &Blocking,
        after: &str,
    ) -> rusqlite::Result<Vec<MissingSong>> {
        let mut query = self.conn.prepare("SELECT song,path,song_id FROM offline_songs WHERE song_id>?1 ORDER BY song_id LIMIT 64")?;
        query
            .query_map([after], |row| {
                let data: String = row.get(0)?;
                let song: Song =
                    serde_json::from_str(&data).map_err(|_| rusqlite::Error::InvalidQuery)?;
                if song.id != row.get::<_, String>(2)? {
                    return Err(rusqlite::Error::InvalidQuery);
                }
                Ok(MissingSong {
                    song,
                    path: PathBuf::from(row.get::<_, String>(1)?),
                })
            })?
            .collect()
    }

    /// Check only downloaded songs appearing in a freshly received album.
    /// A cached response can be older than what is already on the card.
    pub(crate) fn observed_album(
        &self,
        blocking: &Blocking,
        songs: &[Song],
    ) -> rusqlite::Result<bool> {
        let mut queued = false;
        for chunk in songs.chunks(128) {
            let ids = serde_json::to_string(&chunk.iter().map(|s| &s.id).collect::<Vec<_>>())
                .map_err(|_| rusqlite::Error::InvalidQuery)?;
            let mut query = self.conn.prepare_cached("SELECT song,path FROM offline_songs WHERE song_id IN (SELECT value FROM json_each(?1))")?;
            let rows = query
                .query_map([ids], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(query);
            for (json, path) in rows {
                let old: Song =
                    serde_json::from_str(&json).map_err(|_| rusqlite::Error::InvalidQuery)?;
                if let Some(new) = chunk.iter().find(|s| {
                    s.id == old.id && crate::library::music_reconcile::media_changed(&old, s)
                }) {
                    queued |= self.queue_upgrade(
                        blocking,
                        &MissingSong {
                            song: old,
                            path: path.into(),
                        },
                        new,
                    )?;
                }
            }
        }
        Ok(queued)
    }

    pub(crate) fn queue_upgrade(
        &self,
        blocking: &Blocking,
        local: &MissingSong,
        new: &Song,
    ) -> rusqlite::Result<bool> {
        self.full_sync(blocking, |conn| {
            let tx = conn.unchecked_transaction()?;
            let path = local.path.to_string_lossy();
            let current: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM offline_songs WHERE song_id=?1 AND path=?2)", params![new.id,path], |r|r.get(0))?;
            if !current { return Ok(false); }
            let queued: Option<String> = tx.query_row("SELECT song FROM download_queue WHERE song_id=?1 AND error IS NULL AND queued IN (SELECT ticket FROM download_upgrades)", [&new.id], |r| r.get(0)).optional()?;
            let encoded = serde_json::to_string(new).map_err(|_| rusqlite::Error::InvalidQuery)?;
            if queued.as_deref() == Some(&encoded) { return Ok(false); }
            queue_tracks(blocking, &tx, std::slice::from_ref(new))?;
            tx.execute("INSERT INTO download_upgrades SELECT queued FROM download_queue WHERE song_id=?1", [&new.id])?;
            tx.commit()?;
            Ok(true)
        })
    }

    pub(crate) fn remove_confirmed_music(
        &self,
        blocking: &Blocking,
        songs: &[MissingSong],
    ) -> rusqlite::Result<(usize, Vec<PathBuf>)> {
        self.full_sync(blocking, |conn| {
            let tx = conn.unchecked_transaction()?;
            let mut files = Vec::new();
            let mut count = 0;
            for saved in songs {
                let id = &saved.song.id;
                let removed = tx.execute("DELETE FROM offline_songs WHERE song_id=?1 AND path=?2 AND NOT EXISTS(SELECT 1 FROM download_queue WHERE song_id=?1)", params![id,saved.path.to_string_lossy()])?;
                if removed == 0 { continue; }
                count += removed;
                // Change only sets containing this confirmed track. Keep the
                // playlist subscription even if its last local member is gone.
                tx.execute("DELETE FROM offline_sets WHERE json_array_length(songs)=1 AND json_extract(songs,'$[0]')=?1",[id])?;
                tx.execute("UPDATE offline_sets SET songs=(SELECT json_group_array(value) FROM json_each(offline_sets.songs) WHERE value<>?1) WHERE EXISTS(SELECT 1 FROM json_each(offline_sets.songs) WHERE value=?1)",[id])?;
                tx.execute("DELETE FROM auto_downloads WHERE kind='music' AND id=?1",[id])?;
                files.extend(Self::forget_cached_versions(&tx, id)?);
                files.push(saved.path.clone());
                files.extend(replace_art_owner(blocking, &tx, "music", id, None)?);
            }
            tx.commit()?;
            Ok((count, files))
        })
    }
}
