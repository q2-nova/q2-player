//! Generated fixtures for the `--bench` library kinds (perf/library.rs): a
//! bulk insert of downloaded music that would otherwise cost one durable
//! commit per song, and a digest of the rows a sync changes.
use super::*;
use md5::{Digest, Md5};

impl Db {
    /// The rows `enqueue_download` and `complete_music_download` leave for a
    /// finished album download, in one transaction and without artwork.
    pub(crate) fn generated_downloaded_album(&self, _blocking: &crate::rt::Blocking, set: &OfflineSet,
        songs: &[(Song, PathBuf, u64)]) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("INSERT INTO offline_sets VALUES(?1,?2,?3,?4,?5,?6,0)",
            params![set.kind, set.id, set.name, set.artist, set.cover_art,
                serde_json::to_string(&set.song_ids).map_err(|_| rusqlite::Error::InvalidQuery)?])?;
        {
            let mut insert = tx.prepare("INSERT INTO offline_songs VALUES(?1,?2,?3,?4)")?;
            for (song, path, size) in songs {
                insert.execute(params![song.id, path.to_string_lossy(), i64::try_from(*size).map_err(|_| rusqlite::Error::InvalidQuery)?,
                    serde_json::to_string(song).map_err(|_| rusqlite::Error::InvalidQuery)?])?;
            }
        }
        tx.commit()
    }

    /// MD5 of what a sync can change: the published catalog, the catalog
    /// versions kept, downloaded music, offline sets, the download queue and
    /// its upgrades. Timestamps are left out, and `root` in stored paths is
    /// replaced so that the digest does not depend on where the scratch is.
    pub(crate) fn generated_sync_digest(&self, _blocking: &crate::rt::Blocking, root: &Path) -> rusqlite::Result<String> {
        let mut md5 = Md5::new();
        let root = root.to_string_lossy();
        for sql in [
            "SELECT e.position || ':' || e.id || ':' || e.summary FROM album_catalog_entries AS e
             JOIN album_catalog_head AS h ON h.version=e.version ORDER BY e.position",
            "SELECT g.ordinal || ':' || g.position || ':' || g.label FROM album_catalog_groups AS g
             JOIN album_catalog_head AS h ON h.version=g.version ORDER BY g.ordinal",
            "SELECT count(*) || ':' || sum(complete) || ':' || sum(total) FROM album_catalog_versions",
            "SELECT COALESCE((SELECT marker FROM album_catalog_markers JOIN album_catalog_head USING(version)), '')",
            "SELECT song_id || ':' || path || ':' || size || ':' || song FROM offline_songs ORDER BY song_id",
            "SELECT kind || ':' || id || ':' || songs FROM offline_sets ORDER BY kind, id",
            "SELECT song_id || ':' || queued || ':' || song || ':' || COALESCE(error, '') FROM download_queue ORDER BY song_id",
            "SELECT CAST(ticket AS TEXT) FROM download_upgrades ORDER BY ticket",
            "SELECT key || ':' || value FROM kv WHERE key IN ('server', 'music-scan-marker', 'download-sequence') ORDER BY key",
        ] {
            let mut query = self.conn.prepare(sql)?;
            let mut rows = query.query([])?;
            while let Some(row) = rows.next()? {
                let value: Option<String> = row.get(0)?;
                md5.update(value.unwrap_or_default().replace(&*root, "<root>").as_bytes());
                md5.update(b"\n");
            }
            md5.update(b"--\n");
        }
        Ok(format!("{:x}", md5.finalize()))
    }
}
