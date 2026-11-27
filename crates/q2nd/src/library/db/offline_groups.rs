use super::{Db, OfflineSet, now, queue_tracks};
use crate::rt::Blocking;
use rusqlite::{Connection, OptionalExtension, params};
use subsonic::Song;

pub(super) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS followed_playlists (id TEXT PRIMARY KEY);
CREATE TABLE IF NOT EXISTS followed_collections (
    namespace TEXT NOT NULL, library TEXT NOT NULL, id TEXT NOT NULL,
    PRIMARY KEY(namespace,library,id));
CREATE TABLE IF NOT EXISTS collection_members (
    namespace TEXT NOT NULL, library TEXT NOT NULL, collection TEXT NOT NULL,
    logical TEXT NOT NULL, item TEXT NOT NULL, title TEXT NOT NULL, admitted INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY(namespace,library,collection,logical));
CREATE INDEX IF NOT EXISTS collection_waiting ON collection_members(namespace,admitted);
";

impl Db {
    pub(crate) fn any_offline_group_follows(&self) -> rusqlite::Result<bool> {
        self.conn.query_row("SELECT EXISTS(SELECT 1 FROM followed_playlists UNION ALL SELECT 1 FROM followed_collections)", [], |row| row.get(0))
    }

    pub(crate) fn playlist_followed(&self, id: &str) -> rusqlite::Result<bool> {
        self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM followed_playlists WHERE id=?1)",
            [id],
            |row| row.get(0),
        )
    }

    pub(crate) fn followed_playlists(&self) -> rusqlite::Result<Vec<String>> {
        let mut query = self
            .conn
            .prepare("SELECT id FROM followed_playlists ORDER BY id")?;
        query.query_map([], |row| row.get(0))?.collect()
    }

    pub(crate) fn stop_following_playlist(
        &self,
        blocking: &Blocking,
        id: &str,
    ) -> rusqlite::Result<()> {
        self.full_sync(blocking, |conn| {
            conn.execute("DELETE FROM followed_playlists WHERE id=?1", [id])
        })?;
        Ok(())
    }

    /// Only a user command can activate a follow. Refreshes recheck the durable
    /// flag, and retain old members so their downloads remain owned.
    pub(crate) fn follow_playlist(
        &self,
        blocking: &Blocking,
        set: &OfflineSet,
        songs: &[Song],
        activate: bool,
    ) -> rusqlite::Result<()> {
        self.full_sync(blocking, |conn| {
            let tx = conn.unchecked_transaction()?;
            if activate {
                tx.execute("INSERT OR IGNORE INTO followed_playlists VALUES(?1)", [&set.id])?;
            } else if !self.playlist_followed(&set.id)? {
                return Ok(());
            }
            let previous = tx.query_row("SELECT songs FROM offline_sets WHERE kind='playlist' AND id=?1", [&set.id], |row| row.get::<_, String>(0)).optional()?;
            let prior: Vec<String> = match previous {
                Some(data) => serde_json::from_str(&data).map_err(|_| rusqlite::Error::InvalidQuery)?,
                None => Vec::new(),
            };
            let seen: std::collections::HashSet<_> = prior.iter().collect();
            let mut ids = set.song_ids.clone();
            let incoming: std::collections::HashSet<_> = ids.iter().cloned().collect();
            ids.extend(prior.iter().filter(|id| !incoming.contains(*id)).cloned());
            let mut added = Vec::new();
            for song in songs {
                if (activate || !seen.contains(&song.id)) && missing_music(&tx, &song.id)? {
                    added.push(song.clone());
                }
            }
            let json = serde_json::to_string(&ids).map_err(|_| rusqlite::Error::InvalidQuery)?;
            tx.execute("INSERT INTO offline_sets VALUES('playlist',?1,?2,?3,?4,?5,?6)
                ON CONFLICT(kind,id) DO UPDATE SET name=excluded.name, artist=excluded.artist,
                cover_art=excluded.cover_art, songs=excluded.songs WHERE
                offline_sets.songs IS NOT excluded.songs OR offline_sets.name IS NOT excluded.name OR
                offline_sets.artist IS NOT excluded.artist OR offline_sets.cover_art IS NOT excluded.cover_art",
                params![set.id, set.name, set.artist, set.cover_art, json, now()])?;
            super::auto_downloads::promote_music(&tx, &set.song_ids)?;
            if !added.is_empty() { queue_tracks(blocking, &tx, &added)?; }
            tx.commit()
        })
    }

    pub(crate) fn followed_collections(
        &self,
        namespace: &str,
    ) -> rusqlite::Result<Vec<(String, String)>> {
        let mut query = self.conn.prepare(
            "SELECT library,id FROM followed_collections WHERE namespace=?1 ORDER BY library,id",
        )?;
        query
            .query_map([namespace], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect()
    }

    pub(crate) fn toggle_collection_follow(
        &self,
        blocking: &Blocking,
        namespace: &str,
        library: &str,
        id: &str,
    ) -> rusqlite::Result<bool> {
        self.full_sync(blocking, |conn| {
            let tx = conn.unchecked_transaction()?;
            let removed = tx.execute(
                "DELETE FROM followed_collections WHERE namespace=?1 AND library=?2 AND id=?3",
                params![namespace, library, id],
            )?;
            if removed == 0 {
                tx.execute(
                    "INSERT INTO followed_collections VALUES(?1,?2,?3)",
                    params![namespace, library, id],
                )?;
            }
            tx.commit()?;
            Ok(removed == 0)
        })
    }

    pub(crate) fn remember_collection_page(
        &self,
        blocking: &Blocking,
        namespace: &str,
        library: &str,
        id: &str,
        books: &[(String, String)],
        refresh: bool,
    ) -> rusqlite::Result<()> {
        self.full_sync(blocking, |conn| {
            let tx = conn.unchecked_transaction()?;
            if refresh && !tx.query_row("SELECT EXISTS(SELECT 1 FROM followed_collections WHERE namespace=?1 AND library=?2 AND id=?3)",
                params![namespace, library, id], |row| row.get::<_, bool>(0))? { return Ok(()); }
            for (item, title) in books {
                let logical = format!("abs:{namespace}:{item}:");
                tx.execute("INSERT INTO collection_members(namespace,library,collection,logical,item,title) VALUES(?1,?2,?3,?4,?5,?6)
                    ON CONFLICT(namespace,library,collection,logical) DO UPDATE SET admitted=0 WHERE ?7=0",
                    params![namespace, library, id, logical, item, title, refresh])?;
            }
            tx.commit()
        })
    }

    /// Drain durable collection requests under the same gate as individual
    /// spoken requests. The queue stays capped even for very large collections.
    pub(in crate::library) fn admit_collection_members(
        &self,
        blocking: &Blocking,
        namespace: &str,
    ) -> rusqlite::Result<()> {
        let exists = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM collection_members WHERE namespace=?1 AND admitted=0)",
            [namespace],
            |row| row.get::<_, bool>(0),
        )?;
        if !exists {
            return Ok(());
        }
        self.full_sync(blocking, |conn| {
            let tx = conn.unchecked_transaction()?;
            let count: i64 = tx.query_row("SELECT count(*) FROM spoken_download_queue", [], |row| row.get(0))?;
            let remaining = 32 - count;
            if remaining <= 0 { return Ok(()); }
            let waiting = {
                let mut query = tx.prepare("SELECT DISTINCT logical,item,title FROM collection_members WHERE namespace=?1 AND admitted=0 ORDER BY rowid LIMIT ?2")?;
                query.query_map(params![namespace, remaining], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            for (logical, item, title) in waiting {
                tx.execute("DELETE FROM auto_downloads WHERE kind='spoken' AND id=?1", [&logical])?;
                tx.execute("INSERT OR IGNORE INTO spoken_download_queue(namespace,logical,item,title)
                    SELECT ?1,?2,?3,?4 WHERE NOT EXISTS (SELECT 1 FROM spoken_downloads WHERE logical=?2)",
                    params![namespace, logical, item, title])?;
                tx.execute("UPDATE collection_members SET admitted=1 WHERE namespace=?1 AND logical=?2", params![namespace, logical])?;
            }
            tx.commit()
        })
    }
}

pub(super) fn missing_music(conn: &Connection, id: &str) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT NOT EXISTS(SELECT 1 FROM offline_songs WHERE song_id=?1)
        AND NOT EXISTS(SELECT 1 FROM download_queue WHERE song_id=?1)",
        [id],
        |row| row.get(0),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playlist_follow_adds_only_new_members_and_retains_removed_downloads() {
        let blocking = &Blocking::dedicated_thread();
        let db = Db::memory(blocking);
        let mut set = OfflineSet {
            kind: "playlist".into(),
            id: "generated-list".into(),
            name: "Generated".into(),
            artist: String::new(),
            cover_art: None,
            song_ids: vec!["song-a".into()],
        };
        let a = Song {
            id: "song-a".into(),
            ..Song::default()
        };
        let b = Song {
            id: "song-b".into(),
            ..Song::default()
        };
        db.follow_playlist(blocking, &set, &[a], true).unwrap();
        let first = db.download_ticket(blocking, "song-a").unwrap();
        set.song_ids = vec!["song-b".into()];
        db.follow_playlist(blocking, &set, std::slice::from_ref(&b), false)
            .unwrap();
        let next = db.download_ticket(blocking, "song-b").unwrap();
        assert!(next > first);
        assert_eq!(
            db.offline_set(blocking, "playlist", &set.id)
                .unwrap()
                .song_ids,
            ["song-b", "song-a"]
        );
        db.follow_playlist(blocking, &set, &[b], false).unwrap();
        assert_eq!(db.download_ticket(blocking, "song-a").unwrap(), first);
        assert_eq!(db.download_ticket(blocking, "song-b").unwrap(), next);
        db.remove_offline_set_checked(blocking, "playlist", &set.id)
            .unwrap();
        assert!(!db.playlist_followed(&set.id).unwrap());
    }

    #[test]
    fn removing_a_book_discards_its_unadmitted_collection_request() {
        let blocking = &Blocking::dedicated_thread();
        let db = Db::memory(blocking);
        let books = [("book".into(), "Generated book".into())];
        db.remember_collection_page(blocking, "account", "library", "collection", &books, false)
            .unwrap();
        db.remove_spoken_queue(blocking, "abs:account:book:")
            .unwrap();
        db.admit_collection_members(blocking, "account").unwrap();
        assert!(db.next_spoken(blocking, "account").unwrap().is_none());

        assert!(
            db.toggle_collection_follow(blocking, "account", "library", "collection")
                .unwrap()
        );
        db.remember_collection_page(blocking, "account", "library", "collection", &books, true)
            .unwrap();
        db.admit_collection_members(blocking, "account").unwrap();
        assert!(db.next_spoken(blocking, "account").unwrap().is_none());
    }

    #[test]
    fn collection_survives_full_queue_and_only_admits_new_follow_members() {
        let blocking = &Blocking::dedicated_thread();
        let db = Db::memory(blocking);
        let books: Vec<_> = (0..40)
            .map(|i| (format!("book-{i}"), format!("Generated {i}")))
            .collect();
        db.remember_collection_page(blocking, "account", "library", "collection", &books, false)
            .unwrap();
        db.admit_collection_members(blocking, "account").unwrap();
        assert_eq!(db.spoken_queue_counts(blocking, "account").unwrap().0, 32);
        assert!(
            db.toggle_collection_follow(blocking, "account", "library", "collection")
                .unwrap()
        );
        let entries = [
            books[0].clone(),
            ("book-new".into(), "Generated new book".into()),
        ];
        db.remember_collection_page(blocking, "account", "library", "collection", &entries, true)
            .unwrap();
        let mut ids = std::collections::HashSet::new();
        while let Some((ticket, _, item, _, _)) = db.next_spoken(blocking, "account").unwrap() {
            assert!(ids.insert(item));
            db.finish_spoken_queue(blocking, ticket, None).unwrap();
            db.admit_collection_members(blocking, "account").unwrap();
        }
        assert_eq!(ids.len(), 41);
        assert!(ids.contains("book-new"));
    }
}
