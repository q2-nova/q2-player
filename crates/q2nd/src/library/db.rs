//! The card's database: cached server responses, the song cache's index,
//! downloads, and actions waiting for the network.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use subsonic::Song;

pub(crate) mod catalog;
mod music_reconcile;
mod offline_groups;
mod auto_downloads;
mod generated;
pub(super) mod mix_space;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS spoken_download_queue (ticket INTEGER PRIMARY KEY AUTOINCREMENT, namespace TEXT NOT NULL, logical TEXT NOT NULL UNIQUE, item TEXT NOT NULL, episode TEXT, title TEXT NOT NULL, error TEXT);
CREATE TABLE IF NOT EXISTS spoken_downloads (id TEXT PRIMARY KEY, logical TEXT NOT NULL, path TEXT NOT NULL, size INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS offline_art (key TEXT PRIMARY KEY, path TEXT NOT NULL, size INTEGER NOT NULL CHECK(size >= 0));
CREATE TABLE IF NOT EXISTS offline_art_refs (kind TEXT NOT NULL, owner TEXT NOT NULL, art TEXT NOT NULL, PRIMARY KEY(kind, owner));
CREATE INDEX IF NOT EXISTS offline_art_owners ON offline_art_refs(art);
CREATE TABLE IF NOT EXISTS spoken_responses (key TEXT PRIMARY KEY, body TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS spoken_catalog (id TEXT PRIMARY KEY, logical TEXT NOT NULL, song TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS spoken_logical ON spoken_catalog(logical);
CREATE TABLE IF NOT EXISTS spoken_speeds (target TEXT PRIMARY KEY, percent INTEGER NOT NULL CHECK(percent BETWEEN 50 AND 300 AND percent % 5 = 0));
CREATE TABLE IF NOT EXISTS spoken_positions (
    id TEXT PRIMARY KEY, position_ms INTEGER NOT NULL, updated_ms INTEGER NOT NULL,
    revision INTEGER NOT NULL, dirty INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS spoken_observed (
    logical TEXT PRIMARY KEY, percent INTEGER NOT NULL, updated INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS spoken_played (
    logical TEXT PRIMARY KEY, namespace TEXT, item TEXT, episode TEXT,
    percent INTEGER NOT NULL, revision INTEGER NOT NULL, pending INTEGER NOT NULL, marked_ms INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS spoken_played_pending ON spoken_played(namespace,pending);
CREATE TABLE IF NOT EXISTS spoken_sessions (
    id TEXT PRIMARY KEY, logical TEXT NOT NULL, body TEXT NOT NULL, dirty INTEGER NOT NULL);
CREATE INDEX IF NOT EXISTS spoken_sessions_pending ON spoken_sessions(dirty,logical);
CREATE TABLE IF NOT EXISTS responses (
    key TEXT PRIMARY KEY, body TEXT NOT NULL, fetched INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS song_cache (
    song_id TEXT NOT NULL, variant TEXT NOT NULL, path TEXT NOT NULL,
    size INTEGER NOT NULL, used INTEGER NOT NULL, PRIMARY KEY (song_id, variant));
CREATE TABLE IF NOT EXISTS offline_songs (
    song_id TEXT PRIMARY KEY, path TEXT NOT NULL, size INTEGER NOT NULL, song TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS cached_song_meta (song_id TEXT PRIMARY KEY, song TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS offline_sets (
    kind TEXT NOT NULL, id TEXT NOT NULL, name TEXT NOT NULL, artist TEXT NOT NULL,
    cover_art TEXT, songs TEXT NOT NULL, added INTEGER NOT NULL, PRIMARY KEY (kind, id));
CREATE TABLE IF NOT EXISTS download_queue (
    song_id TEXT PRIMARY KEY, song TEXT NOT NULL, queued INTEGER NOT NULL, error TEXT);
CREATE TABLE IF NOT EXISTS direct_listens (
    id INTEGER PRIMARY KEY, account BLOB NOT NULL, station TEXT, artist TEXT NOT NULL,
    title TEXT NOT NULL, started INTEGER NOT NULL, payload TEXT NOT NULL, finished INTEGER NOT NULL DEFAULT 0);
CREATE INDEX IF NOT EXISTS direct_listens_pending ON direct_listens(account,finished,id);
CREATE UNIQUE INDEX IF NOT EXISTS direct_listens_identity ON direct_listens(account,coalesce(station,''),artist,title,started);
CREATE TABLE IF NOT EXISTS download_upgrades (ticket INTEGER PRIMARY KEY);
CREATE TRIGGER IF NOT EXISTS retire_upgrade_after_removal AFTER DELETE ON download_queue
BEGIN DELETE FROM download_upgrades WHERE ticket=OLD.queued; END;
CREATE TRIGGER IF NOT EXISTS retire_upgrade_after_requeue AFTER UPDATE OF queued ON download_queue
WHEN OLD.queued<>NEW.queued BEGIN DELETE FROM download_upgrades WHERE ticket=OLD.queued; END;
CREATE TABLE IF NOT EXISTS pending (
    id INTEGER PRIMARY KEY AUTOINCREMENT, call TEXT NOT NULL, created INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS album_catalog_versions (
    id INTEGER PRIMARY KEY AUTOINCREMENT, complete INTEGER NOT NULL DEFAULT 0,
    total INTEGER NOT NULL DEFAULT 0, received INTEGER NOT NULL DEFAULT 0,
    groups INTEGER NOT NULL DEFAULT 0, exhausted INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS album_catalog_head (
    slot INTEGER PRIMARY KEY CHECK(slot=0), version INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS album_catalog_entries (
    version INTEGER NOT NULL, position INTEGER NOT NULL, id TEXT NOT NULL,
    summary TEXT NOT NULL, PRIMARY KEY(version,position), UNIQUE(version,id));
CREATE TABLE IF NOT EXISTS album_catalog_groups (
    version INTEGER NOT NULL, ordinal INTEGER NOT NULL, position INTEGER NOT NULL,
    label TEXT NOT NULL, PRIMARY KEY(version,ordinal));
CREATE INDEX IF NOT EXISTS album_catalog_group_positions ON album_catalog_groups(version,position);
CREATE TABLE IF NOT EXISTS album_catalog_markers (
    version INTEGER PRIMARY KEY, marker TEXT NOT NULL);
";

/// SQLite's page cache, KiB. Its default, 2,000 KiB, stays allocated once
/// filled (a catalog build or a large stored response fills it). Pages it
/// drops are still in the kernel's file cache, so a miss is a read() and a
/// copy, not a card access. `Q2ND_SQLITE_CACHE_KIB` overrides it for A/B
/// measurement on the Q2.
const CACHE_KIB: i64 = 512;

fn cache_kib() -> i64 {
    std::env::var("Q2ND_SQLITE_CACHE_KIB").ok().and_then(|v| v.parse::<i64>().ok()).filter(|k| (64..=65536).contains(k)).unwrap_or(CACHE_KIB)
}

/// An action taken while offline, sent when the server is back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum PendingCall {
    Scrobble { id: String, time_ms: u64 },
    Star { kind: String, id: String, on: bool },
    Rating { id: String, rating: u8 },
}

/// A downloaded album, playlist or song.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OfflineSet {
    /// Manual `album`, `playlist` and `song` sets, or the `auto` rotation.
    pub kind: String,
    pub id: String,
    pub name: String,
    pub artist: String,
    pub cover_art: Option<String>,
    pub song_ids: Vec<String>,
}

pub struct Db {
    conn: Connection,
    // Ephemeral owner admission; never persisted or supplied by server data.
    shelf_generation: Option<std::sync::Arc<()>>,
    catalog_leases: catalog::Leases,
    catalog_build: Option<catalog::BuildOwner>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OfflineArt {
    pub key: String,
    pub path: PathBuf,
    pub size: u64,
}
pub struct DownloadRequest {pub song:Song,pub ticket:i64,pub automatic:bool}

fn queue_tracks(_blocking: &crate::rt::Blocking, conn:&Connection,songs:&[Song])->rusqlite::Result<()> {
    let legacy=conn.query_row("SELECT COALESCE(MAX(queued),0) FROM download_queue",[],|row|row.get::<_,i64>(0))?;
    let saved=conn.query_row("SELECT value FROM kv WHERE key='download-sequence'",[],|row|row.get::<_,String>(0)).optional()?;
    let saved=saved.map(|value|value.parse::<i64>().map_err(|_|rusqlite::Error::InvalidQuery)).transpose()?.unwrap_or(0);
    if saved<0 {return Err(rusqlite::Error::InvalidQuery);}
    let mut ticket=legacy.max(saved).max(0);
    for song in songs {
        ticket=ticket.checked_add(1).ok_or(rusqlite::Error::InvalidQuery)?;
        conn.execute("INSERT INTO download_queue VALUES(?1,?2,?3,NULL) ON CONFLICT(song_id) DO UPDATE SET song=excluded.song,queued=excluded.queued,error=NULL",
            params![song.id,serde_json::to_string(song).map_err(|_|rusqlite::Error::InvalidQuery)?,ticket])?;
    }
    conn.execute("INSERT INTO kv VALUES('download-sequence',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[ticket.to_string()])?;
    Ok(())
}

/// A stored browse page body (a JSON object) led by the ETag it came with.
fn with_etag(body: &str, etag: Option<&str>) -> String {
    match (etag, body.strip_prefix('{')) {
        (Some(etag), Some(rest)) => {
            let tag = serde_json::to_string(etag).expect("a JSON string");
            if rest.trim_start().starts_with('}') { format!("{{\"etag2\":{tag}{rest}") } else { format!("{{\"etag2\":{tag},{rest}") }
        }
        _ => body.to_owned(),
    }
}
/// The ETag at the head of a stored page ([`with_etag`]). The field name
/// carries the version of the stored projection: a parser that keeps other
/// fields renames it, so a 304 never revives a copy parsed the old way.
pub(crate) fn page_etag(head: &str) -> Option<String> {
    let rest = head.strip_prefix("{\"etag2\":")?;
    serde_json::Deserializer::from_str(rest).into_iter::<String>().next()?.ok()
}

fn reset_music_data(_blocking: &crate::rt::Blocking, conn:&Connection)->rusqlite::Result<Vec<PathBuf>> {
    let mut obsolete={let mut query=conn.prepare("SELECT path FROM song_cache UNION ALL SELECT path FROM offline_songs")?;
        query.query_map([],|row|row.get::<_,String>(0).map(PathBuf::from))?.collect::<rusqlite::Result<Vec<_>>>()?};
    conn.execute_batch("DELETE FROM followed_playlists; DELETE FROM auto_downloads WHERE kind='music'; DELETE FROM responses; DELETE FROM song_cache; DELETE FROM cached_song_meta; DELETE FROM offline_songs; DELETE FROM offline_sets; DELETE FROM download_queue; DELETE FROM pending; DELETE FROM kv WHERE key IN ('queue','auto-rotation'); DELETE FROM kv WHERE key='music-scan-marker'; DELETE FROM kv WHERE key='music-stale-before'; DELETE FROM offline_art_refs WHERE kind='music'; DELETE FROM album_catalog_head; DELETE FROM album_catalog_entries; DELETE FROM album_catalog_groups; DELETE FROM album_catalog_versions; DELETE FROM album_catalog_markers;")?;
    {let mut query=conn.prepare("DELETE FROM offline_art WHERE NOT EXISTS (SELECT 1 FROM offline_art_refs WHERE art=offline_art.key) RETURNING path")?;
        obsolete.extend(query.query_map([],|row|row.get::<_,String>(0).map(PathBuf::from))?.collect::<rusqlite::Result<Vec<_>>>()?);}
    Ok(obsolete)
}

/// The caller holds the artwork publication/removal gate. File contents have
/// already been synchronized before this transaction makes the reference live.
fn replace_art_owner(_blocking: &crate::rt::Blocking, conn: &Connection, kind: &str, owner: &str, art: Option<&OfflineArt>) -> rusqlite::Result<Vec<PathBuf>> {
    let previous:Option<String>=conn.query_row("SELECT art FROM offline_art_refs WHERE kind=?1 AND owner=?2",params![kind,owner],|row|row.get(0)).optional()?;
    let mut replaced_path=None;
    if let Some(art)=art {
        replaced_path=conn.query_row("SELECT path FROM offline_art WHERE key=?1",[&art.key],|row|row.get::<_,String>(0)).optional()?.filter(|path|Path::new(path)!=art.path);
        conn.execute("INSERT INTO offline_art VALUES(?1,?2,?3) ON CONFLICT(key) DO UPDATE SET path=excluded.path,size=excluded.size",
            params![art.key,art.path.to_string_lossy(),i64::try_from(art.size).map_err(|_|rusqlite::Error::InvalidQuery)?])?;
        conn.execute("INSERT INTO offline_art_refs VALUES(?1,?2,?3) ON CONFLICT(kind,owner) DO UPDATE SET art=excluded.art",params![kind,owner,art.key])?;
    } else {
        conn.execute("DELETE FROM offline_art_refs WHERE kind=?1 AND owner=?2",params![kind,owner])?;
    }
    let mut orphaned=replaced_path.into_iter().map(PathBuf::from).collect::<Vec<_>>();
    if let Some(previous)=previous.filter(|key|art.is_none_or(|art|art.key!=*key)) {
        if let Some(path)=conn.query_row("DELETE FROM offline_art WHERE key=?1 AND NOT EXISTS (SELECT 1 FROM offline_art_refs WHERE art=?1) RETURNING path",[previous],|row|row.get::<_,String>(0)).optional()? {
            orphaned.push(PathBuf::from(path));
        }
    }
    Ok(orphaned)
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

impl Db {
    pub(super) fn bind_shelf_generation(&mut self, generation: std::sync::Arc<()>) {
        self.shelf_generation = Some(generation);
    }
    pub(super) fn shelf_generation_current(&self, generation: &std::sync::Arc<()>) -> bool {
        self.shelf_generation.as_ref().is_some_and(|current| std::sync::Arc::ptr_eq(current, generation))
    }

    pub fn offline_art(&self, _blocking: &crate::rt::Blocking,key:&str)->rusqlite::Result<Option<OfflineArt>> {
        self.conn.query_row("SELECT path,size FROM offline_art WHERE key=?1",[key],|row|Ok(OfflineArt {key:key.into(),path:PathBuf::from(row.get::<_,String>(0)?),size:u64::try_from(row.get::<_,i64>(1)?).map_err(|_|rusqlite::Error::InvalidQuery)?})).optional()
    }
    /// Audio, artwork ownership and queue completion are one durable commit.
    /// None means the user removed this request during the transfer.
    pub fn complete_music_download(&self, blocking: &crate::rt::Blocking,song:&Song,path:&Path,size:u64,art:Option<&OfflineArt>,ticket:i64)->rusqlite::Result<Option<Vec<PathBuf>>> {
        self.full_sync(blocking, |conn| {
            let tx=conn.unchecked_transaction()?;
            if !tx.query_row("SELECT EXISTS(SELECT 1 FROM download_queue WHERE song_id=?1 AND queued=?2 AND error IS NULL)",params![song.id,ticket],|row|row.get::<_,bool>(0))? {return Ok(None);}
            let previous=tx.query_row("SELECT path FROM offline_songs WHERE song_id=?1",[&song.id],|row|row.get::<_,String>(0)).optional()?;
            tx.execute("INSERT INTO offline_songs VALUES(?1,?2,?3,?4) ON CONFLICT(song_id) DO UPDATE SET path=excluded.path,size=excluded.size,song=excluded.song",
                params![song.id,path.to_string_lossy(),i64::try_from(size).map_err(|_|rusqlite::Error::InvalidQuery)?,serde_json::to_string(song).map_err(|_|rusqlite::Error::InvalidQuery)?])?;
            let mut obsolete=replace_art_owner(blocking, &tx,"music",&song.id,art)?;
            if tx.query_row("SELECT EXISTS(SELECT 1 FROM download_upgrades WHERE ticket=?1)", [ticket], |r| r.get::<_,bool>(0))? {
                obsolete.extend(Self::forget_cached_versions(&tx, &song.id)?.into_iter().filter(|p| p != path));
            }
            if let Some(previous)=previous.filter(|previous|Path::new(previous)!=path) {obsolete.push(PathBuf::from(previous));}
            tx.execute("DELETE FROM download_queue WHERE song_id=?1",[&song.id])?;
            tx.commit()?;Ok(Some(obsolete))
        })
    }
    pub fn music_download_path_indexed(&self, _blocking: &crate::rt::Blocking,path:&Path)->rusqlite::Result<bool> {
        self.conn.query_row("SELECT EXISTS(SELECT 1 FROM offline_songs WHERE path=?1)",[path.to_string_lossy()],|row|row.get(0))
    }
    pub fn download_ticket(&self, _blocking: &crate::rt::Blocking,id:&str)->rusqlite::Result<Option<i64>> {
        self.conn.query_row("SELECT queued FROM download_queue WHERE song_id=?1",[id],|row|row.get(0)).optional()
    }
    pub fn download_request_current(&self, _blocking: &crate::rt::Blocking,id:&str,ticket:i64)->rusqlite::Result<bool> {
        self.conn.query_row("SELECT EXISTS(SELECT 1 FROM download_queue WHERE song_id=?1 AND queued=?2 AND error IS NULL)",params![id,ticket],|row|row.get(0))
    }
    #[cfg(test)]
    pub fn next_download_request(&self, blocking: &crate::rt::Blocking) -> rusqlite::Result<Option<DownloadRequest>> {
        self.next_eligible_download(blocking, true)
    }
    pub(super) fn next_eligible_download(&self, _blocking: &crate::rt::Blocking, allow_auto: bool) -> rusqlite::Result<Option<DownloadRequest>> {
        let row = self.conn.query_row("SELECT song,queued,a.id IS NOT NULL FROM download_queue AS q
            LEFT JOIN auto_downloads AS a ON a.kind='music' AND a.id=q.song_id
            WHERE q.error IS NULL AND (?1 OR a.id IS NULL) ORDER BY a.id IS NOT NULL,queued LIMIT 1",
            [allow_auto], |row| Ok((row.get::<_,String>(0)?, row.get::<_,i64>(1)?, row.get::<_,bool>(2)?))).optional()?;
        row.map(|(song,ticket,automatic)| Ok(DownloadRequest {song:serde_json::from_str(&song).map_err(|_|rusqlite::Error::InvalidQuery)?,ticket,automatic})).transpose()
    }
    pub(super) fn automatic_pending_music(&self) -> usize {
        self.conn.query_row("SELECT count(*) FROM download_queue AS q JOIN auto_downloads AS a
            ON a.kind='music' AND a.id=q.song_id WHERE q.error IS NULL", [], |row| row.get::<_, i64>(0)).unwrap_or(0).max(0) as usize
    }
    pub fn finish_download_request(&self, blocking: &crate::rt::Blocking,id:&str,ticket:i64,error:Option<&str>)->rusqlite::Result<bool> {
        self.full_sync(blocking, |conn|Ok(if let Some(error)=error {
            conn.execute("UPDATE download_queue SET error=?3 WHERE song_id=?1 AND queued=?2",params![id,ticket,error])?
        } else {
            conn.execute("DELETE FROM download_queue WHERE song_id=?1 AND queued=?2",params![id,ticket])?
        }>0))
    }
    pub fn enqueue_download(&self, blocking: &crate::rt::Blocking,set:&OfflineSet,songs:&[Song])->rusqlite::Result<()> {
        self.full_sync(blocking, |conn| {
            let tx=conn.unchecked_transaction()?;
            tx.execute("INSERT INTO offline_sets VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(kind,id) DO UPDATE SET name=excluded.name,artist=excluded.artist,cover_art=excluded.cover_art,songs=excluded.songs",
                params![set.kind,set.id,set.name,set.artist,set.cover_art,serde_json::to_string(&set.song_ids).map_err(|_|rusqlite::Error::InvalidQuery)?,now()])?;
            if set.kind != "auto" { auto_downloads::promote_music(&tx, &set.song_ids)?; }
            queue_tracks(blocking, &tx,songs)?;tx.commit()
        })
    }
    pub fn reset_owned_music(&self, blocking: &crate::rt::Blocking)->rusqlite::Result<Vec<PathBuf>> {
        self.full_sync(blocking, |conn| {let tx=conn.unchecked_transaction()?;let obsolete=reset_music_data(blocking, &tx)?;tx.commit()?;Ok(obsolete)})
    }
    /// Install the configured server/user marker only with its successful reset.
    /// Unknown ownership is never silently adopted by a new configured account.
    /// Returns whether ownership changed and the files released by that change.
    pub fn bind_music_server(&self, blocking: &crate::rt::Blocking,id:&str)->rusqlite::Result<(bool,Vec<PathBuf>)> {
        self.full_sync(blocking, |conn| {
            let tx=conn.unchecked_transaction()?;
            let previous=tx.query_row("SELECT value FROM kv WHERE key='server'",[],|row|row.get::<_,String>(0)).optional()?;
            let changed=previous.as_deref()!=Some(id);
            let obsolete=if changed {reset_music_data(blocking, &tx)?} else {Vec::new()};
            tx.execute("INSERT INTO kv VALUES('server',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value",[id])?;
            tx.commit()?;Ok((changed,obsolete))
        })
    }
    /// Isolate real open/binding work on a fresh generated database. This does
    /// not open the normal library, configuration or network.
    pub fn startup_binding_benchmark_ordinary(root: &Path, iterations: usize, scene: &str) -> Result<serde_json::Value, String> {
        assert!(crate::rt::may_wait_for_tasks(), "binding benchmark belongs to an ordinary diagnostic thread");
        Self::startup_binding_benchmark(&crate::rt::Blocking::dedicated_thread(), root, iterations, scene)
    }
    pub fn startup_binding_benchmark(blocking: &crate::rt::Blocking, root:&Path,iterations:usize,scene:&str)->Result<serde_json::Value,String> {
        if !root.components().any(|c|c.as_os_str()=="build")
            || !(1..=1000).contains(&iterations) || !matches!(scene,"same"|"changed") {
            return Err("music-binding needs fresh build/ scratch, 1..1000 iterations and same/changed scene".into());
        }
        if let Some(parent)=root.parent() {std::fs::create_dir_all(parent).map_err(|e|e.to_string())?;}
        // Only this atomic creation grants ownership. An existence check before
        // create_dir_all would admit a concurrently created foreign database.
        std::fs::create_dir(root).map_err(|e|e.to_string())?;
        std::fs::write(root.join(".q2-generated-binding-benchmark"),b"generated database only\n").map_err(|e|e.to_string())?;
        let path=root.join("library.db");
        Db::open(blocking, &path).and_then(|db|db.bind_music_server(blocking, "generated-owner-a")).map_err(|e|e.to_string())?;
        let mut samples=Vec::with_capacity(iterations);
        for iteration in 0..iterations {
            let begin=std::time::Instant::now();
            let db=Db::open(blocking, &path).map_err(|e|e.to_string())?;
            let open_us=begin.elapsed().as_micros() as u64;
            let before=db.conn.total_changes();
            let owner=if scene=="changed" && iteration%2==0 {"generated-owner-b"} else {"generated-owner-a"};
            let begin=std::time::Instant::now();
            let (changed,removed)=db.bind_music_server(blocking, owner).map_err(|e|e.to_string())?;
            let binding_us=begin.elapsed().as_micros() as u64;
            if !removed.is_empty() || changed!=(scene=="changed") {return Err("generated ownership invariant failed".into());}
            samples.push(serde_json::json!({"open_us":open_us,"binding_us":binding_us,"changed":changed,"rows_changed":db.conn.total_changes()-before}));
        }
        Ok(serde_json::json!({"scene":scene,"iterations":iterations,"samples":samples,"scope":"fresh generated database; real Db open/bind; excludes UI/kernel/network"}))
    }
    pub fn open(_blocking: &crate::rt::Blocking, path: &Path) -> rusqlite::Result<Db> {
        let conn = Connection::open(path)?;
        // WAL keeps readers off the writer's back; NORMAL sync is safe with
        // WAL and spares the card a flush per write.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "cache_size", -cache_kib())?;
        conn.execute_batch(SCHEMA).and_then(|_| conn.execute_batch(offline_groups::SCHEMA)).and_then(|_| conn.execute_batch(auto_downloads::SCHEMA)).and_then(|_| conn.execute_batch(mix_space::SCHEMA))?;
        Ok(Db { conn, shelf_generation: None, catalog_leases: Default::default(), catalog_build: None })
    }

    pub fn memory(_blocking: &crate::rt::Blocking, ) -> Db {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).and_then(|_| conn.execute_batch(offline_groups::SCHEMA)).and_then(|_| conn.execute_batch(auto_downloads::SCHEMA)).and_then(|_| conn.execute_batch(mix_space::SCHEMA)).unwrap();
        Db { conn, shelf_generation: None, catalog_leases: Default::default(), catalog_build: None }
    }

    /// Tests: rows inserted, updated or deleted on this connection so far.
    #[cfg(test)]
    pub(crate) fn total_changes(&self) -> u64 { self.conn.total_changes() }

    /// Tests: bytes the write-ahead log holds (with checkpoints off, every
    /// page written since the test began).
    #[cfg(test)]
    pub(crate) fn wal_bytes(&self) -> u64 {
        self.conn.path().and_then(|path| std::fs::metadata(format!("{path}-wal")).ok()).map_or(0, |m| m.len())
    }

    /// Tests: no automatic checkpoint, so [`Db::wal_bytes`] counts pages written.
    #[cfg(test)]
    pub(crate) fn hold_checkpoints(&self) { self.conn.pragma_update(None, "wal_autocheckpoint", 0).unwrap(); }

    #[cfg(test)]
    pub(crate) fn audit_spoken_responses(&self, _blocking: &crate::rt::Blocking) -> rusqlite::Result<Vec<(String, String)>> {
        self.conn
            .prepare("SELECT key,body FROM spoken_responses ORDER BY key")?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect()
    }

    /// Card-free browsing must not grow a persistent NAND catalog. Network
    /// results can still be displayed when this small disposable cache is full.
    pub fn temporary(_blocking: &crate::rt::Blocking, ) -> Db {
        let conn = Connection::open_in_memory().expect("temporary SQLite database");
        conn.pragma_update(None, "page_size", 4096).expect("fixed SQLite page size");
        conn.pragma_update(None, "max_page_count", 256).expect("fixed SQLite page limit");
        conn.execute_batch(SCHEMA).and_then(|_| conn.execute_batch(offline_groups::SCHEMA)).and_then(|_| conn.execute_batch(auto_downloads::SCHEMA)).and_then(|_| conn.execute_batch(mix_space::SCHEMA)).expect("temporary SQLite schema");
        Db { conn, shelf_generation: None, catalog_leases: Default::default(), catalog_build: None }
    }

    pub(super) fn enqueue_spoken(&self, _blocking: &crate::rt::Blocking, namespace: &str, logical: &str, item: &str, episode: Option<&str>, title: &str) -> rusqlite::Result<()> {
        let tx=self.conn.unchecked_transaction()?;
        let count: i64 = tx.query_row("SELECT count(*) FROM spoken_download_queue", [], |r| r.get(0))?;
        let exists: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM spoken_download_queue WHERE logical=?1)", [logical], |r| r.get(0))?;
        if count >= 32 && !exists { return Err(rusqlite::Error::InvalidQuery); }
        tx.execute("DELETE FROM auto_downloads WHERE kind='spoken' AND id=?1", [logical])?;
        tx.execute("DELETE FROM spoken_download_queue WHERE logical=?1", [logical])?;
        tx.execute("INSERT INTO spoken_download_queue(namespace,logical,item,episode,title) VALUES(?1,?2,?3,?4,?5)", params![namespace,logical,item,episode,title])?;
        tx.commit()?;
        Ok(())
    }
    pub(super) fn spoken_download_ticket_current(&self,_blocking:&crate::rt::Blocking,logical:&str,ticket:i64)->rusqlite::Result<bool> {
        self.conn.query_row("SELECT EXISTS(SELECT 1 FROM spoken_download_queue WHERE logical=?1 AND ticket=?2 AND error IS NULL)",params![logical,ticket],|r|r.get(0))
    }
    #[cfg(test)]
    pub(super) fn next_spoken(&self, blocking: &crate::rt::Blocking, namespace: &str) -> rusqlite::Result<Option<(i64,String,String,Option<String>,String)>> {
        Ok(self.next_eligible_spoken(blocking, namespace, true)?.map(|(ticket,logical,item,episode,title,_)| (ticket,logical,item,episode,title)))
    }
    pub(super) fn next_eligible_spoken(&self, _blocking: &crate::rt::Blocking, namespace: &str, allow_auto: bool) -> rusqlite::Result<Option<(i64,String,String,Option<String>,String,bool)>> {
        self.conn.query_row("SELECT ticket,logical,item,episode,title,a.id IS NOT NULL FROM spoken_download_queue AS q
            LEFT JOIN auto_downloads AS a ON a.kind='spoken' AND a.id=q.logical
            WHERE q.namespace=?1 AND q.error IS NULL AND (?2 OR a.id IS NULL) ORDER BY a.id IS NOT NULL,ticket LIMIT 1",
            params![namespace,allow_auto], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()
    }
    pub(super) fn spoken_queue_rows(&self, _blocking: &crate::rt::Blocking, namespace: &str) -> rusqlite::Result<Vec<(String,String,Option<String>,String,bool)>> {
        let mut query=self.conn.prepare("SELECT logical,item,episode,title,error IS NOT NULL FROM spoken_download_queue WHERE namespace=?1 ORDER BY ticket")?;
        let rows=query.query_map([namespace], |r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?;
        rows.collect()
    }
    pub(super) fn failed_download_ids(&self, _blocking: &crate::rt::Blocking) -> rusqlite::Result<std::collections::HashSet<String>> {
        let mut query=self.conn.prepare("SELECT song_id FROM download_queue WHERE error IS NOT NULL")?;
        let rows=query.query_map([],|r|r.get(0))?;rows.collect()
    }
    pub(super) fn spoken_queue_counts(&self, _blocking: &crate::rt::Blocking, namespace: &str) -> rusqlite::Result<(usize,usize)> {
        self.conn.query_row("SELECT count(*),coalesce(sum(error IS NOT NULL),0) FROM spoken_download_queue WHERE namespace=?1", [namespace], |r| Ok((r.get::<_,i64>(0)? as usize,r.get::<_,i64>(1)? as usize)))
    }
    pub(super) fn finish_spoken_queue(&self, _blocking: &crate::rt::Blocking, ticket: i64, error: Option<&str>) -> rusqlite::Result<()> {
        if let Some(error)=error { self.conn.execute("UPDATE spoken_download_queue SET error=?2 WHERE ticket=?1",params![ticket,error])?; }
        else { self.conn.execute("DELETE FROM spoken_download_queue WHERE ticket=?1",[ticket])?; }
        Ok(())
    }
    pub(super) fn remove_spoken_queue(&self, _blocking: &crate::rt::Blocking, logical: &str) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM auto_downloads WHERE kind='spoken' AND id=?1", [logical])?;
        tx.execute("DELETE FROM spoken_download_queue WHERE logical=?1",[logical])?;
        tx.execute("UPDATE collection_members SET admitted=1 WHERE logical=?1 AND admitted=0", [logical])?;
        tx.commit()
    }
    pub(super) fn retry_spoken_queue(&self, _blocking: &crate::rt::Blocking, namespace: &str) -> rusqlite::Result<()> {
        self.conn.execute("UPDATE spoken_download_queue SET error=NULL WHERE namespace=?1",[namespace])?; Ok(())
    }
    pub fn spoken_download(&self, _blocking: &crate::rt::Blocking, id: &str) -> rusqlite::Result<Option<PathBuf>> {
        self.conn
            .query_row("SELECT path FROM spoken_downloads WHERE id=?1", [id], |r| {
                r.get::<_, String>(0).map(PathBuf::from)
            })
            .optional()
    }
    pub fn put_spoken_downloads(
        &self, blocking: &crate::rt::Blocking,
        logical: &str,
        files: &[(String, PathBuf, u64)],
    ) -> rusqlite::Result<Vec<PathBuf>> {
        self.put_spoken_downloads_with_art(blocking, logical,files,None)
    }
    pub fn put_spoken_downloads_with_art(&self, blocking: &crate::rt::Blocking,logical:&str,files:&[(String,PathBuf,u64)],art:Option<&OfflineArt>)->rusqlite::Result<Vec<PathBuf>> {
        self.full_sync(blocking, |conn| {
            let tx = conn.unchecked_transaction()?;
            let mut old = {
                let mut st = tx.prepare("SELECT path FROM spoken_downloads WHERE logical=?1")?;
                st.query_map([logical], |r| r.get::<_, String>(0).map(PathBuf::from))?
                    .collect::<rusqlite::Result<Vec<_>>>()?
            };
            tx.execute("DELETE FROM spoken_downloads WHERE logical=?1", [logical])?;
            for (id, path, size) in files {
                tx.execute(
                    "INSERT INTO spoken_downloads VALUES(?1,?2,?3,?4)",
                    params![
                        id,
                        logical,
                        path.to_string_lossy(),
                        (*size).min(i64::MAX as u64) as i64
                    ],
                )?;
            }
            old.extend(replace_art_owner(blocking, &tx,"spoken",logical,art)?);
            tx.commit()?;
            Ok(old)
        })
    }
    pub fn remove_spoken_downloads(&self, blocking: &crate::rt::Blocking, logical: &str) -> rusqlite::Result<Vec<PathBuf>> {
        self.put_spoken_downloads(blocking, logical, &[])
    }
    fn full_sync<T>(
        &self, _blocking: &crate::rt::Blocking,
        operation: impl FnOnce(&Connection) -> rusqlite::Result<T>,
    ) -> rusqlite::Result<T> {
        let previous: i64 = self
            .conn
            .pragma_query_value(None, "synchronous", |r| r.get(0))?;
        self.conn.pragma_update(None, "synchronous", "FULL")?;
        let saved = operation(&self.conn);
        let restored = self.conn.pragma_update(None, "synchronous", previous);
        let value = saved?;
        restored?;
        Ok(value)
    }
    /// Construction-only authenticated namespace index. Ordinary cache bodies
    /// and catalog rows are deliberately excluded from this small snapshot.
    pub(super) fn shelf_namespaces(&self, _blocking: &crate::rt::Blocking) -> rusqlite::Result<std::collections::HashMap<String, String>> {
        let mut query = self.conn.prepare("SELECT key,body FROM spoken_responses WHERE key >= 'shelf-account:' AND key < 'shelf-account;' ORDER BY key")?;
        query.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?.collect()
    }
    pub fn spoken_response(&self, _blocking: &crate::rt::Blocking, key: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT body FROM spoken_responses WHERE key=?1",
                [key],
                |r| r.get(0),
            )
            .optional()
    }
    pub fn put_spoken_response(&self, _blocking: &crate::rt::Blocking, key: &str, body: &str) -> rusqlite::Result<()> {
        self.conn.execute("INSERT INTO spoken_responses VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET body=excluded.body WHERE body IS NOT excluded.body",params![key,body])?;
        Ok(())
    }
    /// Explicit library sync acknowledges only a committed FULL-synchronous write.
    pub fn put_spoken_response_durable(&self, blocking: &crate::rt::Blocking, key: &str, body: &str) -> rusqlite::Result<()> {
        self.full_sync(blocking, |connection| {
            connection.execute("INSERT INTO spoken_responses VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET body=excluded.body WHERE body IS NOT excluded.body", params![key,body])?;
            Ok(())
        })
    }
    /// A browse page with the server's ETag for it as its first field
    /// (`etag2`, see [`page_etag`]): one row, so a page and its validator are
    /// written together and an unchanged page writes nothing. Returns whether
    /// the stored row changed.
    pub fn put_spoken_page(&self, _blocking: &crate::rt::Blocking, key: &str, body: &str, etag: Option<&str>) -> rusqlite::Result<bool> {
        let stored = with_etag(body, etag);
        Ok(self.conn.execute("INSERT INTO spoken_responses VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET body=excluded.body WHERE body IS NOT excluded.body", params![key, stored])? != 0)
    }
    /// The ETag a stored page was fetched with.
    pub fn spoken_page_etag(&self, _blocking: &crate::rt::Blocking, key: &str) -> rusqlite::Result<Option<String>> {
        let head: Option<String> = self.conn.query_row("SELECT substr(body,1,320) FROM spoken_responses WHERE key=?1", [key], |r| r.get(0)).optional()?;
        Ok(head.as_deref().and_then(page_etag))
    }
    /// Removes the pages a completed sync of `scope` stored before and did not
    /// reach this time: shows, series and pages the server no longer has.
    /// `kept` (sorted page digests, hex) replaces the scope's record
    /// (`shelf-sync-pages:`); page keys are `prefix` and a digest. An
    /// unchanged record writes nothing. Returns how many were removed.
    pub fn prune_synced_pages(&self, _blocking: &crate::rt::Blocking, scope: &str, prefix: &str, kept: &[String]) -> rusqlite::Result<usize> {
        let record = format!("shelf-sync-pages:{scope}");
        let tx = self.conn.unchecked_transaction()?;
        let previous: Option<String> = tx.query_row("SELECT body FROM spoken_responses WHERE key=?1", [&record], |r| r.get(0)).optional()?;
        let previous: Vec<String> = previous.and_then(|body| serde_json::from_str(&body).ok()).unwrap_or_default();
        let stale: Vec<&String> = previous.iter().filter(|digest| kept.binary_search(digest).is_err()).collect();
        for digest in &stale {
            tx.execute("DELETE FROM spoken_responses WHERE key=?1", [format!("{prefix}{digest}")])?;
        }
        let body = serde_json::to_string(kept).map_err(|_| rusqlite::Error::InvalidQuery)?;
        tx.execute("INSERT INTO spoken_responses VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET body=excluded.body WHERE body IS NOT excluded.body", params![record, body])?;
        tx.commit()?;
        Ok(stale.len())
    }
    pub fn put_spoken(&self, _blocking: &crate::rt::Blocking, song: &Song) -> rusqlite::Result<()> {
        let body = serde_json::to_string(song)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        let logical = &song
            .spoken
            .as_ref()
            .ok_or(rusqlite::Error::InvalidQuery)?
            .id;
        self.conn.execute("INSERT INTO spoken_catalog(id,logical,song) VALUES(?1,?2,?3) ON CONFLICT(id) DO UPDATE SET logical=excluded.logical,song=excluded.song",params![song.id,logical,body])?;
        Ok(())
    }
    pub fn replace_spoken_queue(&self, _blocking: &crate::rt::Blocking, logical: &str, songs: &[Song]) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM spoken_catalog WHERE logical=?1", [logical])?;
        for song in songs {
            let body = serde_json::to_string(song)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            tx.execute(
                "INSERT INTO spoken_catalog VALUES(?1,?2,?3)",
                params![song.id, logical, body],
            )?;
        }
        tx.commit()
    }
    pub fn pending_spoken(
        &self, _blocking: &crate::rt::Blocking,
        namespace: &str,
    ) -> rusqlite::Result<Vec<(Song, super::spoken::Position)>> {
        let mut st=self.conn.prepare("SELECT c.song,p.position_ms,p.updated_ms,p.revision,p.dirty
            FROM spoken_positions p JOIN spoken_catalog c ON c.id=(SELECT MIN(id) FROM spoken_catalog WHERE logical=p.id)
            WHERE p.dirty=1 AND p.id LIKE ?1 ORDER BY p.id LIMIT 100")?;
        let rows = st.query_map(
            [if namespace.is_empty() {
                "abs:%".into()
            } else {
                format!("abs:{namespace}:%")
            }],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    super::spoken::Position {
                        position_ms: r.get::<_, i64>(1)? as u64,
                        updated_ms: r.get::<_, i64>(2)? as u64,
                        revision: r.get::<_, i64>(3)? as u64,
                        dirty: r.get(4)?,
                    },
                ))
            },
        )?;
        let mut pending = Vec::new();
        for row in rows {
            let (body, position) = row?;
            let song = serde_json::from_str(&body).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(e),
                )
            })?;
            pending.push((song, position));
        }
        Ok(pending)
    }
    pub fn spoken_songs(&self, _blocking: &crate::rt::Blocking) -> rusqlite::Result<Vec<Song>> {
        let mut st = self
            .conn
            .prepare("SELECT song FROM spoken_catalog ORDER BY id")?;
        let bodies = st.query_map([], |r| r.get::<_, String>(0))?;
        let mut songs = Vec::new();
        for body in bodies {
            if let Ok(song) = serde_json::from_str(&body?) {
                songs.push(song);
            }
        }
        Ok(songs)
    }
    pub fn spoken_speed(&self, _blocking: &crate::rt::Blocking, target: &str) -> rusqlite::Result<Option<u16>> {
        self.conn.query_row("SELECT percent FROM spoken_speeds WHERE target=?1", [target], |row| row.get(0)).optional()
    }
    pub fn set_spoken_speed(&self, blocking: &crate::rt::Blocking, target: &str, percent: Option<u16>) -> rusqlite::Result<()> {
        self.full_sync(blocking, |conn| {
            if let Some(percent) = percent {
                conn.execute("INSERT INTO spoken_speeds VALUES(?1,?2) ON CONFLICT(target) DO UPDATE SET percent=excluded.percent", params![target,percent])?;
            } else {
                conn.execute("DELETE FROM spoken_speeds WHERE target=?1", [target])?;
            }
            Ok(())
        })
    }
    pub fn spoken_position(&self, _blocking: &crate::rt::Blocking, id: &str) -> rusqlite::Result<Option<super::spoken::Position>> {
        self.conn
            .query_row(
                "SELECT position_ms,updated_ms,revision,dirty FROM spoken_positions WHERE id=?1",
                [id],
                |r| {
                    Ok(super::spoken::Position {
                        position_ms: r.get::<_, i64>(0)? as u64,
                        updated_ms: r.get::<_, i64>(1)? as u64,
                        revision: r.get::<_, i64>(2)? as u64,
                        dirty: r.get(3)?,
                    })
                },
            )
            .optional()
    }
    pub(super) fn spoken_percent(&self, blocking: &crate::rt::Blocking, logical: &str, duration: u64) -> rusqlite::Result<u8> {
        if let Some(percent) = self.conn.query_row("SELECT percent FROM spoken_played WHERE logical=?1", [logical], |row| row.get(0)).optional()? {
            return Ok(percent);
        }
        let position = self.spoken_position(blocking, logical)?;
        if let Some(percent) = self.conn.query_row("SELECT percent FROM spoken_observed WHERE logical=?1 AND updated>=?2",
            params![logical, position.as_ref().map_or(0, |position| position.updated_ms.min(i64::MAX as u64) as i64)], |row| row.get(0)).optional()? {
            return Ok(percent);
        }
        let Some(position) = position.filter(|position| position.position_ms > 0) else { return Ok(0); };
        let duration = if duration > 0 { duration } else {
            // A local checkpoint is possible only after opening the item. Its
            // catalog entry supplies duration without expanding every UI row.
            self.conn.query_row("SELECT song FROM spoken_catalog WHERE logical=?1 LIMIT 1", [logical],
                |row| row.get::<_, String>(0)).optional()?
                .and_then(|body| serde_json::from_str::<Song>(&body).ok())
                .and_then(|song| song.spoken)
                .map_or(0, |spoken| spoken.duration_ms)
        };
        Ok(if duration > 0 { (position.position_ms.saturating_mul(100) / duration).min(100) as u8 } else { 0 })
    }

    pub(super) fn set_spoken_played(&self, blocking: &crate::rt::Blocking, target: &super::spoken::PlayedTarget, played: bool, now: u64) -> rusqlite::Result<()> {
        let (namespace, item, episode) = target.remote.as_ref()
            .map_or((None, None, None), |(namespace, item, episode)| (Some(namespace), Some(item), episode.as_ref()));
        self.full_sync(blocking, |conn| {
            let tx = conn.unchecked_transaction()?;
            tx.execute("INSERT INTO spoken_played VALUES(?1,?2,?3,?4,?5,1,?6,?7)
                ON CONFLICT(logical) DO UPDATE SET percent=excluded.percent,revision=spoken_played.revision+1,
                pending=excluded.pending,marked_ms=excluded.marked_ms",
                params![target.logical, namespace, item, episode, if played { 100 } else { 0 }, namespace.is_some(), now as i64])?;
            tx.execute("INSERT INTO spoken_positions VALUES(?1,0,?2,1,0)
                ON CONFLICT(id) DO UPDATE SET position_ms=0,updated_ms=MAX(spoken_positions.updated_ms+1,excluded.updated_ms),
                revision=spoken_positions.revision+1,dirty=0", params![target.logical, now as i64])?;
            tx.commit()
        })
    }

    pub(super) fn pending_spoken_played(&self, _blocking: &crate::rt::Blocking, namespace: &str) -> rusqlite::Result<Vec<super::spoken::PlayedUpload>> {
        let mut query = self.conn.prepare("SELECT logical,item,episode,percent,revision FROM spoken_played AS mark WHERE namespace=?1 AND pending=1
            AND NOT EXISTS(SELECT 1 FROM spoken_sessions WHERE logical=mark.logical AND dirty=1) ORDER BY rowid LIMIT 64")?;
        query.query_map([namespace], |row| Ok(super::spoken::PlayedUpload {
            logical: row.get(0)?, item: row.get(1)?, episode: row.get(2)?,
            played: row.get::<_, u8>(3)? == 100, revision: row.get(4)?,
        }))?.collect()
    }

    pub(super) fn acknowledge_spoken_played(&self, _blocking: &crate::rt::Blocking, mark: &super::spoken::PlayedUpload) -> rusqlite::Result<()> {
        self.conn.execute("UPDATE spoken_played SET pending=0 WHERE logical=?1 AND revision=?2", params![mark.logical, mark.revision])?;
        Ok(())
    }

    /// Position and pending sync are one atomic upsert. A failed write is an error.
    pub fn save_spoken_position(
        &self, blocking: &crate::rt::Blocking,
        id: &str,
        position: u64,
        updated: u64,
    ) -> rusqlite::Result<()> {
        self.save_spoken_checkpoint(blocking, id, position, updated, None)
    }
    pub fn save_spoken_checkpoint(
        &self, blocking: &crate::rt::Blocking, id: &str, position: u64, updated: u64,
        session: Option<(&Song, &crate::audio::listening::Session)>,
    ) -> rusqlite::Result<()> {
        self.full_sync(blocking, |conn| {
            let tx = conn.unchecked_transaction()?;
            let marked = tx.query_row("SELECT marked_ms FROM spoken_played WHERE logical=?1", [id], |row| row.get::<_, i64>(0)).optional()?;
            // A paused or queued older checkpoint can arrive after a manual mark.
            // Only newly audible listening supersedes the explicit action.
            let mark_still_applies = marked.is_some_and(|at| {
                !session.is_some_and(|(_, session)| session.last_listened_ms > at as u64)
            });
            if !mark_still_applies {
                tx.execute("INSERT INTO spoken_positions VALUES(?1,?2,?3,1,1)
                    ON CONFLICT(id) DO UPDATE SET position_ms=excluded.position_ms,
                    updated_ms=MAX(spoken_positions.updated_ms+1,excluded.updated_ms),revision=spoken_positions.revision+1,dirty=1
                    WHERE spoken_positions.position_ms<>excluded.position_ms",
                    params![id,position.min(i64::MAX as u64) as i64,updated.min(i64::MAX as u64) as i64])?;
                if marked.is_some() { tx.execute("DELETE FROM spoken_played WHERE logical=?1", [id])?; }
            }
            if let Some((song, session)) = session.filter(|(_, session)| session.listened_ms > 0) {
                let previous = tx.query_row("SELECT body FROM spoken_sessions WHERE id=?1", [&session.id], |r| r.get::<_, String>(0)).optional()?;
                let previous = previous.map(|body| serde_json::from_str::<super::spoken::SessionUpload>(&body)
                    .map_err(|_| rusqlite::Error::InvalidQuery)).transpose()?;
                if previous.as_ref().is_none_or(|p| p.session != *session || p.position_ms != position) {
                    let revision = tx.query_row("SELECT revision FROM spoken_positions WHERE id=?1", [id], |r| r.get::<_, i64>(0))? as u64;
                    let upload = super::spoken::SessionUpload { song: song.clone(), session: session.clone(), position_ms: position, updated_ms: updated, revision };
                    let body = serde_json::to_string(&upload).map_err(|_| rusqlite::Error::InvalidQuery)?;
                    tx.execute("DELETE FROM spoken_sessions WHERE logical=?1 AND id<>?2 AND dirty=0", params![id, session.id])?;
                    tx.execute("INSERT INTO spoken_sessions VALUES(?1,?2,?3,1) ON CONFLICT(id) DO UPDATE SET body=excluded.body,dirty=1",
                        params![session.id, id, body])?;
                    if mark_still_applies {
                        // Preserve the older listening receipt, then reapply the mark.
                        tx.execute("UPDATE spoken_played SET pending=1,revision=revision+1 WHERE logical=?1 AND namespace IS NOT NULL", [id])?;
                    }
                }
            }
            tx.commit()
        })
    }

    pub(super) fn has_pending_spoken_session(&self, _blocking: &crate::rt::Blocking, logical: &str) -> rusqlite::Result<bool> {
        self.conn.query_row("SELECT EXISTS(SELECT 1 FROM spoken_sessions WHERE logical=?1 AND dirty=1)", [logical], |row| row.get(0))
    }

    pub(super) fn pending_spoken_sessions(&self, _blocking: &crate::rt::Blocking, namespace: &str) -> rusqlite::Result<Vec<super::spoken::SessionUpload>> {
        // One bounded batch for the active account. Other accounts must not
        // consume the batch or have their JSON loaded into this sync pass.
        let prefix=format!("abs:{namespace}:");
        let mut st = self.conn.prepare("SELECT body FROM spoken_sessions WHERE dirty=1 AND substr(logical,1,length(?1))=?1 ORDER BY rowid LIMIT 64")?;
        let mut result = Vec::new();
        for body in st.query_map([prefix], |r| r.get::<_, String>(0))? {
            let upload: super::spoken::SessionUpload = serde_json::from_str(&body?).map_err(|_| rusqlite::Error::InvalidQuery)?;
            if upload.song.spoken.as_ref().is_some_and(|s| matches!(&s.source, subsonic::SpokenSource::Audiobookshelf { namespace: account, .. } if account == namespace)) {
                result.push(upload);
            }
        }
        Ok(result)
    }

    pub(super) fn acknowledge_spoken_session(&self, blocking: &crate::rt::Blocking, upload: &super::spoken::SessionUpload) -> rusqlite::Result<()> {
        let body = serde_json::to_string(upload).map_err(|_| rusqlite::Error::InvalidQuery)?;
        self.conn.execute("UPDATE spoken_sessions SET dirty=0 WHERE id=?1 AND body=?2", params![upload.session.id, body])?;
        if let Some(book) = &upload.song.spoken { self.acknowledge_spoken(blocking, &book.id, upload.revision)?; }
        Ok(())
    }
    pub fn accept_spoken_remote(
        &self, _blocking: &crate::rt::Blocking,
        id: &str,
        position: u64,
        updated: u64,
    ) -> rusqlite::Result<()> {
        self.reconcile_spoken_remote(_blocking, id, position, updated, false)
    }
    pub fn reconcile_spoken_remote(
        &self, _blocking: &crate::rt::Blocking, id: &str, position: u64, updated: u64, resolve_dirty: bool,
    ) -> rusqlite::Result<()> {
        self.conn.execute("INSERT INTO spoken_positions VALUES(?1,?2,?3,0,0)
            ON CONFLICT(id) DO UPDATE SET position_ms=excluded.position_ms,updated_ms=excluded.updated_ms,dirty=0
            WHERE (spoken_positions.dirty=0 OR ?4) AND excluded.updated_ms>spoken_positions.updated_ms
                AND NOT EXISTS(SELECT 1 FROM spoken_played WHERE logical=?1 AND pending=1)",
            params![id,position.min(i64::MAX as u64) as i64,updated.min(i64::MAX as u64) as i64,resolve_dirty])?;
        Ok(())
    }
    pub(super) fn observe_spoken_snapshot(&self, blocking: &crate::rt::Blocking, namespace: &str,
        records: &[subsonic::audiobookshelf::MediaProgress],
    ) -> rusqlite::Result<bool> {
        let tx = self.conn.unchecked_transaction()?;
        let mut changed = false;
        for record in records {
            if record.library_item_id.is_empty() { continue; }
            let logical = format!("abs:{namespace}:{}:{}", record.library_item_id, record.episode_id.as_deref().unwrap_or_default());
            let position = if record.is_finished { 0 } else { (record.current_time * 1000.0) as u64 };
            let percent = if record.is_finished { 100 } else { (record.progress.clamp(0.0, 1.0) * 100.0) as u8 };
            changed |= self.observe_spoken_remote(blocking, &logical, position, record.last_update, false, Some(percent))?;
        }
        tx.commit()?;
        Ok(changed)
    }

    pub(super) fn observe_spoken_remote(
        &self, blocking: &crate::rt::Blocking, logical: &str, position: u64, updated: u64,
        resolve_dirty: bool, percent: Option<u8>,
    ) -> rusqlite::Result<bool> {
        let previous = self.spoken_position(blocking, logical)?;
        self.reconcile_spoken_remote(blocking, logical, position, updated, resolve_dirty)?;
        let current = self.spoken_position(blocking, logical)?;
        if current == previous { return Ok(false); }
        if let Some(percent) = percent {
            self.conn.execute("INSERT INTO spoken_observed VALUES(?1,?2,?3)
                ON CONFLICT(logical) DO UPDATE SET percent=excluded.percent,updated=excluded.updated",
                params![logical, percent, updated.min(i64::MAX as u64) as i64])?;
        }
        self.conn.execute("DELETE FROM spoken_played WHERE logical=?1 AND pending=0", [logical])?;
        Ok(true)
    }

    pub fn acknowledge_spoken(&self, _blocking: &crate::rt::Blocking, id: &str, revision: u64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE spoken_positions SET dirty=0 WHERE id=?1 AND revision=?2",
            params![id, revision.min(i64::MAX as u64) as i64],
        )?;
        Ok(())
    }
    // Key-value settings and state.

    pub fn get(&self, _blocking: &crate::rt::Blocking, key: &str) -> Option<String> {
        self.conn.query_row("SELECT value FROM kv WHERE key = ?1", [key], |r| r.get(0)).optional().ok().flatten()
    }

    pub fn set(&self, blocking: &crate::rt::Blocking, key: &str, value: &str) {
        let _ = self.set_checked(blocking, key, value);
    }

    #[cfg(test)]
    pub(super) fn conn_for_sleep_fault(&self, _blocking: &crate::rt::Blocking) {
        self.conn.execute_batch("CREATE TRIGGER reject_sleep_queue BEFORE INSERT ON kv WHEN NEW.key='queue' BEGIN SELECT RAISE(ABORT,'synthetic queue fault'); END;").unwrap();
    }
    #[cfg(test)]
    pub(super) fn clear_sleep_fault(&self, _blocking: &crate::rt::Blocking) { self.conn.execute_batch("DROP TRIGGER reject_sleep_queue;").unwrap(); }

    pub fn checkpoint(&self, _blocking: &crate::rt::Blocking) -> rusqlite::Result<()> {
        let (busy, _, _): (i64, i64, i64) = self.conn.query_row(
            "PRAGMA wal_checkpoint(FULL)", [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        if busy != 0 { return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY), None)); }
        Ok(())
    }

    pub fn set_checked(&self, _blocking: &crate::rt::Blocking, key: &str, value: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO kv (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    // Response cache.

    /// Younger than `max_age` and fetched after the last observed server scan
    /// change (`music-stale-before`, local seconds like `fetched`).
    pub fn response_fresh(&self, _blocking: &crate::rt::Blocking, key: &str, max_age: i64) -> bool {
        self.conn.query_row(
            "SELECT fetched, COALESCE((SELECT CAST(value AS INTEGER) FROM kv WHERE key='music-stale-before'), 0) FROM responses WHERE key = ?1",
            [key], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
            .is_ok_and(|(fetched, scanned)| fetched <= now() && now() - fetched < max_age && fetched > scanned)
    }

    /// A stored response as text (tests and small entries; `response_with`
    /// streams). Bodies are stored as TEXT or, when streamed in, as BLOB.
    pub fn response(&self, _blocking: &crate::rt::Blocking, key: &str) -> Option<(String, i64)> {
        self.conn
            .query_row("SELECT body, fetched FROM responses WHERE key = ?1", [key], |r| {
                let body = match r.get_ref(0)? {
                    rusqlite::types::ValueRef::Text(t) | rusqlite::types::ValueRef::Blob(t) => String::from_utf8_lossy(t).into_owned(),
                    _ => String::new(),
                };
                Ok((body, r.get(1)?))
            })
            .optional()
            .ok()
            .flatten()
    }

    /// Hands a stored response to `feed` in 16 KiB pieces, without a copy of
    /// its whole text. None when there is none; false when `feed` refused a
    /// piece or the read failed.
    pub fn response_pieces(&self, _blocking: &crate::rt::Blocking, key: &str, feed: &mut dyn FnMut(&[u8]) -> bool) -> Option<bool> {
        let row: i64 = self.conn.query_row("SELECT rowid FROM responses WHERE key = ?1", [key], |r| r.get(0)).optional().ok()??;
        let mut blob = self.conn.blob_open(rusqlite::MAIN_DB, "responses", "body", row, true).ok()?;
        let mut piece = vec![0u8; 16 * 1024];
        loop {
            match std::io::Read::read(&mut blob, &mut piece) {
                Ok(0) => return Some(true),
                Ok(n) => if !feed(&piece[..n]) { return Some(false) },
                Err(_) => return Some(false),
            }
        }
    }

    /// Stores a response that `write` produces, streamed into the database:
    /// one pass counts its bytes, a second writes them into a reserved
    /// value. No complete text is built.
    pub fn put_response_with(&self, _blocking: &crate::rt::Blocking, key: &str, write: &dyn Fn(&mut dyn std::io::Write) -> std::io::Result<()>) {
        struct Count(u64);
        impl std::io::Write for Count {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> { self.0 += b.len() as u64; Ok(b.len()) }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }
        let mut count = Count(0);
        if write(&mut count).is_err() || count.0 > i32::MAX as u64 { return; }
        // The same document again (a list reopened, an album warmed a day
        // later): only its time changes. Rewriting it put every page of the
        // body through the write-ahead log onto the card again.
        if self.response_same(_blocking, key, count.0, write) {
            let _ = self.conn.execute("UPDATE responses SET fetched = ?2 WHERE key = ?1", params![key, now()]);
            return;
        }
        let stored = (|| -> rusqlite::Result<()> {
            let row: i64 = self.conn.query_row(
                "INSERT INTO responses (key, body, fetched) VALUES (?1, zeroblob(?2), ?3)
                 ON CONFLICT(key) DO UPDATE SET body = excluded.body, fetched = excluded.fetched RETURNING rowid",
                params![key, count.0 as i64, now()], |r| r.get(0))?;
            let blob = self.conn.blob_open(rusqlite::MAIN_DB, "responses", "body", row, false)?;
            let mut out = std::io::BufWriter::with_capacity(16 * 1024, blob);
            write(&mut out).map_err(|_| rusqlite::Error::InvalidQuery)?;
            std::io::Write::flush(&mut out).map_err(|_| rusqlite::Error::InvalidQuery)?;
            Ok(())
        })();
        // A partial value must not be read back as a stored response.
        if stored.is_err() { let _ = self.conn.execute("DELETE FROM responses WHERE key = ?1", [key]); }
    }

    /// Whether the stored response under `key` is the `length` bytes `write`
    /// produces: compared as they are produced, stopping at the first
    /// difference, without a copy of either.
    fn response_same(&self, blocking: &crate::rt::Blocking, key: &str, length: u64, write: &dyn Fn(&mut dyn std::io::Write) -> std::io::Result<()>) -> bool {
        /// The next `piece.len()` stored bytes (SQLite incremental blob I/O on the `db` thread).
        fn stored_piece(_blocking: &crate::rt::Blocking, blob: &mut rusqlite::blob::Blob<'_>, piece: &mut [u8]) -> std::io::Result<()> {
            std::io::Read::read_exact(blob, piece)
        }
        struct Compare<'a, 'b> { blocking: &'a crate::rt::Blocking, stored: rusqlite::blob::Blob<'b>, piece: Vec<u8> }
        impl std::io::Write for Compare<'_, '_> {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.piece.resize(b.len(), 0);
                stored_piece(self.blocking, &mut self.stored, &mut self.piece)?;
                if self.piece != b { return Err(std::io::Error::other("differs")); }
                Ok(b.len())
            }
            fn flush(&mut self) -> std::io::Result<()> { Ok(()) }
        }
        let Ok(Some(row)) = self.conn.query_row("SELECT rowid FROM responses WHERE key = ?1", [key], |r| r.get::<_, i64>(0)).optional() else { return false };
        let Ok(blob) = self.conn.blob_open(rusqlite::MAIN_DB, "responses", "body", row, true) else { return false };
        if blob.len() as u64 != length { return false; }
        let mut compare = std::io::BufWriter::with_capacity(16 * 1024, Compare { blocking, stored: blob, piece: Vec::new() });
        write(&mut compare).is_ok() && std::io::Write::flush(&mut compare).is_ok()
    }

    pub fn put_response(&self, _blocking: &crate::rt::Blocking, key: &str, body: &str) {
        let _ = self.conn.execute(
            "INSERT INTO responses (key, body, fetched) VALUES (?1, ?2, ?3)
             ON CONFLICT(key) DO UPDATE SET body = excluded.body, fetched = excluded.fetched",
            params![key, body, now()],
        );
    }

    // Song cache.

    /// A cached copy of a song, any of `variants` (first preferred).
    pub fn cached_song(&self, _blocking: &crate::rt::Blocking, song_id: &str, variants: &[&str]) -> Option<(String, PathBuf)> {
        for v in variants {
            let hit: Option<String> = self
                .conn
                .query_row("SELECT path FROM song_cache WHERE song_id = ?1 AND variant = ?2", params![song_id, v], |r| {
                    r.get(0)
                })
                .optional()
                .ok()
                .flatten();
            if let Some(p) = hit {
                let _ = self.conn.execute(
                    "UPDATE song_cache SET used = ?3 WHERE song_id = ?1 AND variant = ?2",
                    params![song_id, v, now()],
                );
                return Some((v.to_string(), PathBuf::from(p)));
            }
        }
        None
    }

    pub fn put_cached_song(&self, _blocking: &crate::rt::Blocking, song_id: &str, variant: &str, path: &Path, size: u64) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO song_cache (song_id, variant, path, size, used) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(song_id, variant) DO UPDATE SET path = excluded.path, size = excluded.size, used = excluded.used",
            params![song_id, variant, path.to_string_lossy(), i64::try_from(size).map_err(|_| rusqlite::Error::InvalidQuery)?, now()],
        )?;
        Ok(())
    }

    /// The song a complete cached copy belongs to, so cached audio can play
    /// (and shuffle) without the server (UX-009).
    pub fn put_cached_song_meta(&self, _blocking: &crate::rt::Blocking, song: &Song) -> rusqlite::Result<()> {
        let json = serde_json::to_string(song).map_err(|_| rusqlite::Error::InvalidQuery)?;
        self.conn.execute("INSERT INTO cached_song_meta (song_id, song) VALUES (?1, ?2)
            ON CONFLICT(song_id) DO UPDATE SET song = excluded.song", params![song.id, json])?;
        Ok(())
    }

    /// Songs with a cached copy and known metadata, with that copy's path and
    /// the bytes the cache registry records for the song's copies.
    pub fn cached_songs(&self, _blocking: &crate::rt::Blocking) -> rusqlite::Result<Vec<(Song, PathBuf, u64)>> {
        let mut query = self.conn.prepare("SELECT m.song, MIN(c.path), SUM(c.size) FROM cached_song_meta m JOIN song_cache c ON c.song_id = m.song_id GROUP BY m.song_id")?;
        let rows = query.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?)))?;
        let mut songs = Vec::new();
        for row in rows {
            let (json, path, size) = row?;
            if let Ok(song) = serde_json::from_str::<Song>(&json) { songs.push((song, PathBuf::from(path), u64::try_from(size).unwrap_or(0))); }
        }
        // Metadata of evicted copies is not kept.
        let _ = self.conn.execute("DELETE FROM cached_song_meta WHERE song_id NOT IN (SELECT song_id FROM song_cache)", []);
        Ok(songs)
    }

    #[cfg(test)]
    pub(super) fn reject_cache_publication(&self, _blocking: &crate::rt::Blocking) {
        self.conn.execute_batch("CREATE TRIGGER reject_cache BEFORE INSERT ON song_cache BEGIN SELECT RAISE(ABORT,'generated cache fault'); END;").unwrap();
    }

    pub fn forget_cached_song(&self, blocking: &crate::rt::Blocking, song_id: &str, variant: &str) {
        let _ = self.forget_cached_song_checked(blocking, song_id, variant);
    }
    pub(super) fn forget_cached_song_checked(&self, _blocking: &crate::rt::Blocking, song_id: &str, variant: &str) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM song_cache WHERE song_id = ?1 AND variant = ?2", params![song_id, variant]).map(|_| ())
    }

    pub(super) fn cached_song_paths(&self, _blocking: &crate::rt::Blocking, id: &str) -> rusqlite::Result<Vec<PathBuf>> {
        let mut query = self.conn.prepare("SELECT DISTINCT path FROM song_cache WHERE song_id = ?1")?;
        let paths = query.query_map([id], |row| row.get::<_, String>(0).map(PathBuf::from))?;
        paths.collect()
    }

    pub(super) fn delete_cached_song(&mut self, _blocking: &crate::rt::Blocking, id: &str) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute("DELETE FROM song_cache WHERE song_id = ?1", [id])?;
        tx.execute("DELETE FROM cached_song_meta WHERE song_id = ?1", [id])?;
        tx.commit()
    }

    pub fn song_cache_size(&self, blocking: &crate::rt::Blocking) -> u64 { self.song_cache_size_checked(blocking).unwrap_or(0) }
    pub(super) fn song_cache_size_checked(&self, _blocking: &crate::rt::Blocking) -> rusqlite::Result<u64> {
        self.conn.query_row("SELECT COALESCE(SUM(size), 0) FROM song_cache", [], |r| r.get::<_, i64>(0)).map(|size| size as u64)
    }

    /// Cached songs, least recently used first.
    pub fn song_cache_lru(&self, blocking: &crate::rt::Blocking, limit: usize) -> Vec<(String, String, PathBuf, u64)> {
        self.read_song_cache_lru(blocking, limit, false).unwrap_or_default()
    }
    pub(super) fn song_cache_lru_checked(&self, blocking: &crate::rt::Blocking, limit: usize) -> rusqlite::Result<Vec<(String, String, PathBuf, u64)>> {
        self.read_song_cache_lru(blocking, limit, true)
    }
    fn read_song_cache_lru(&self, _blocking: &crate::rt::Blocking, limit: usize, checked: bool) -> rusqlite::Result<Vec<(String, String, PathBuf, u64)>> {
        let mut st = self.conn.prepare("SELECT song_id, variant, path, size FROM song_cache ORDER BY used LIMIT ?1")?;
        let rows = st.query_map([limit as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, PathBuf::from(r.get::<_, String>(2)?), r.get::<_, i64>(3)? as u64))
        })?;
        if checked { rows.collect() } else { Ok(rows.filter_map(Result::ok).collect()) }
    }

    // Downloads.

    pub fn offline_song(&self, _blocking: &crate::rt::Blocking, song_id: &str) -> Option<PathBuf> {
        self.conn
            .query_row("SELECT path FROM offline_songs WHERE song_id = ?1", [song_id], |r| r.get::<_, String>(0))
            .optional()
            .ok()
            .flatten()
            .map(PathBuf::from)
    }

    pub fn put_offline_song(&self, _blocking: &crate::rt::Blocking, song: &Song, path: &Path, size: u64) {
        let _ = self.conn.execute(
            "INSERT INTO offline_songs (song_id, path, size, song) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(song_id) DO UPDATE SET path = excluded.path, size = excluded.size, song = excluded.song",
            params![song.id, path.to_string_lossy(), size as i64, serde_json::to_string(song).unwrap_or_default()],
        );
    }

    pub fn offline_songs(&self, _blocking: &crate::rt::Blocking, ids: &[String]) -> Vec<Song> {
        ids.iter()
            .filter_map(|id| {
                self.conn
                    .query_row("SELECT song FROM offline_songs WHERE song_id = ?1", [id], |r| r.get::<_, String>(0))
                    .optional()
                    .ok()
                    .flatten()
                    .and_then(|s| serde_json::from_str(&s).ok())
            })
            .collect()
    }

    /// One snapshot of completed downloads; no queued or streaming-cache rows.
    pub fn downloaded_songs(&self, _blocking: &crate::rt::Blocking) -> rusqlite::Result<Vec<(Song, PathBuf)>> {
        let mut query = self.conn.prepare("SELECT song_id, song, path FROM offline_songs ORDER BY song_id")?;
        let rows = query.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?;
        let mut songs = Vec::new();
        for row in rows {
            let (id, json, path) = row?;
            if let Ok(song) = serde_json::from_str::<Song>(&json) {
                if !id.is_empty() && song.id == id { songs.push((song, PathBuf::from(path))); }
            }
        }
        Ok(songs)
    }

    /// Completed spoken files with their chapter metadata. A book can name
    /// the same physical file through multiple chapters.
    pub(super) fn downloaded_spoken_files(&self, _blocking: &crate::rt::Blocking) -> rusqlite::Result<Vec<(Song, PathBuf)>> {
        let mut query = self.conn.prepare(
            "SELECT catalog.song, download.path FROM spoken_downloads AS download
             JOIN spoken_catalog AS catalog ON catalog.id = download.id AND catalog.logical = download.logical
             ORDER BY download.id",
        )?;
        let rows = query.query_map([], |row| Ok((row.get::<_, String>(0)?, PathBuf::from(row.get::<_, String>(1)?))))?;
        let mut songs = Vec::new();
        for row in rows {
            let (json, path) = row?;
            if let Ok(song) = serde_json::from_str(&json) {
                songs.push((song, path));
            }
        }
        Ok(songs)
    }

    /// Up to `limit` random downloaded and up to `limit` random cached songs,
    /// unverified: Shuffle Songs starts from one of them (UX-004). Each pick
    /// is one rowid probe, a few B-tree pages; `ORDER BY RANDOM()` read every
    /// row's metadata from the card and evicted the player's own pages (V0.108
    /// device: 9 MB refaulted per Shuffle).
    pub fn random_local_songs(&self, _blocking: &crate::rt::Blocking, limit: usize) -> rusqlite::Result<Vec<(Song, PathBuf)>> {
        let mut songs = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let sample = |table: &str| -> rusqlite::Result<Vec<i64>> {
            if limit == 0 { return Ok(Vec::new()); }
            // Probe every row in a small table, even after deletions leave
            // large rowid gaps. Read only keys until we know it is small.
            let count = i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX);
            // The scalar MAX seeks the B-tree endpoint in the same SQLite
            // snapshot. A combined MIN/MAX aggregate walked the whole index.
            let mut keys = self.conn.prepare(&format!(
                "SELECT rowid, (SELECT MAX(rowid) FROM {table}) FROM {table} ORDER BY rowid LIMIT ?1"
            ))?;
            let rows = keys.query_map([count], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if rows.len() <= limit { return Ok(rows.into_iter().map(|(id, _)| id).collect()); }
            let (low, high) = rows[0];
            Ok((0..limit).map(|_| fastrand::i64(low..=high)).collect())
        };
        let offline = sample("offline_songs")?;
        if !offline.is_empty() {
            let mut query = self.conn.prepare("SELECT rowid, song_id, song, path FROM offline_songs WHERE rowid >= ?1 ORDER BY rowid LIMIT 1")?;
            for probe in offline {
                let row = query.query_row([probe], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)));
                let Ok((rowid, id, json, path)) = row else { continue };
                if !seen.insert(("offline", rowid)) { continue; }
                if let Ok(song) = serde_json::from_str::<Song>(&json) {
                    if !id.is_empty() && song.id == id { songs.push((song, PathBuf::from(path))); }
                }
            }
        }
        let cached = sample("cached_song_meta")?;
        if !cached.is_empty() {
            let mut query = self.conn.prepare("SELECT m.rowid, m.song, (SELECT MIN(c.path) FROM song_cache c WHERE c.song_id = m.song_id) \
                FROM cached_song_meta m WHERE m.rowid >= ?1 ORDER BY m.rowid LIMIT 1")?;
            for probe in cached {
                let row = query.query_row([probe], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?)));
                let Ok((rowid, json, Some(path))) = row else { continue };
                if !seen.insert(("cached", rowid)) { continue; }
                if let Ok(song) = serde_json::from_str::<Song>(&json) { songs.push((song, PathBuf::from(path))); }
            }
        }
        Ok(songs)
    }

    fn offline_id_rows(&self, _blocking: &crate::rt::Blocking) -> rusqlite::Result<Vec<rusqlite::Result<String>>> {
        let mut st = self.conn.prepare("SELECT song_id FROM offline_songs")?;
        Ok(st.query_map([], |r| r.get(0))?.collect())
    }
    pub fn offline_ids(&self, blocking: &crate::rt::Blocking) -> std::collections::HashSet<String> {
        self.offline_id_rows(blocking).unwrap_or_default().into_iter().filter_map(Result::ok).collect()
    }
    pub(super) fn offline_ids_checked(&self, blocking: &crate::rt::Blocking) -> rusqlite::Result<std::collections::HashSet<String>> {
        self.offline_id_rows(blocking)?.into_iter().collect()
    }

    pub fn offline_size(&self, _blocking: &crate::rt::Blocking) -> u64 {
        self.conn.query_row("SELECT COALESCE(SUM(size), 0) FROM offline_songs", [], |r| r.get::<_, i64>(0)).unwrap_or(0)
            as u64
    }

    /// Downloads first, so a file also indexed as cache is counted once as a
    /// download. These are file identities, not a sum of possibly stale sizes.
    pub(crate) fn indexed_media_paths(&self, _blocking: &crate::rt::Blocking) -> rusqlite::Result<Vec<(bool, PathBuf)>> {
        let mut query = self.conn.prepare("SELECT 1,path FROM offline_songs UNION ALL SELECT 1,path FROM spoken_downloads UNION ALL SELECT 1,path FROM offline_art UNION ALL SELECT 0,path FROM song_cache")?;
        query.query_map([], |row| Ok((row.get::<_, bool>(0)?, PathBuf::from(row.get::<_, String>(1)?))))?.collect()
    }

    pub fn put_offline_set(&self, _blocking: &crate::rt::Blocking, set: &OfflineSet) {
        let _ = self.conn.execute(
            "INSERT INTO offline_sets (kind, id, name, artist, cover_art, songs, added) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(kind, id) DO UPDATE SET name = excluded.name, artist = excluded.artist,
               cover_art = excluded.cover_art, songs = excluded.songs",
            params![
                set.kind,
                set.id,
                set.name,
                set.artist,
                set.cover_art,
                serde_json::to_string(&set.song_ids).unwrap_or_default(),
                now()
            ],
        );
    }

    fn offline_set_rows(&self, _blocking: &crate::rt::Blocking) -> rusqlite::Result<Vec<rusqlite::Result<OfflineSet>>> {
        let mut st = self.conn.prepare("SELECT kind, id, name, artist, cover_art, songs FROM offline_sets ORDER BY kind, name COLLATE NOCASE")?;
        Ok(st.query_map([], |r| {
            Ok(OfflineSet {
                kind: r.get(0)?,
                id: r.get(1)?,
                name: r.get(2)?,
                artist: r.get(3)?,
                cover_art: r.get(4)?,
                song_ids: serde_json::from_str(&r.get::<_, String>(5)?).unwrap_or_default(),
            })
        })?.collect())
    }

    pub fn offline_sets(&self, blocking: &crate::rt::Blocking) -> Vec<OfflineSet> {
        self.offline_set_rows(blocking).unwrap_or_default().into_iter().filter_map(Result::ok).collect()
    }
    #[cfg(test)]
    pub(crate) fn reject_offline_set_reads(&self, _blocking: &crate::rt::Blocking, reject: bool) {
        self.conn.execute_batch(if reject {
            "ALTER TABLE offline_sets RENAME TO generated_unavailable_offline_sets;"
        } else { "ALTER TABLE generated_unavailable_offline_sets RENAME TO offline_sets;" }).unwrap();
    }
    pub(super) fn offline_sets_checked(&self, blocking: &crate::rt::Blocking) -> rusqlite::Result<Vec<OfflineSet>> {
        self.offline_set_rows(blocking)?.into_iter().collect()
    }

    pub fn offline_set(&self, blocking: &crate::rt::Blocking, kind: &str, id: &str) -> Option<OfflineSet> {
        self.offline_sets(blocking).into_iter().find(|s| s.kind == kind && s.id == id)
    }

    /// Legacy fixture-only removal. Production uses the checked FULL-sync
    /// transaction below, including artwork ownership and error propagation.
    #[cfg(test)]
    pub fn remove_offline_set(&self, _blocking: &crate::rt::Blocking, kind: &str, id: &str) -> Vec<PathBuf> {
        let _ = self.conn.execute("DELETE FROM offline_sets WHERE kind = ?1 AND id = ?2", params![kind, id]);
        let kept: std::collections::HashSet<String> =
            self.offline_sets(&crate::rt::Blocking::dedicated_thread()).into_iter().flat_map(|s| s.song_ids).collect();
        let all: Vec<(String, String)> = {
            let mut st = match self.conn.prepare("SELECT song_id, path FROM offline_songs") {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .map(|rows| rows.filter_map(Result::ok).collect())
                .unwrap_or_default()
        };
        let mut gone = Vec::new();
        for (sid, path) in all {
            if !kept.contains(&sid) {
                let _ = self.conn.execute("DELETE FROM offline_songs WHERE song_id = ?1", [&sid]);
                let _ = self.conn.execute("DELETE FROM download_queue WHERE song_id = ?1", [&sid]);
                gone.push(PathBuf::from(path));
            }
        }
        gone
    }

    pub fn remove_offline_set_checked(&self, blocking: &crate::rt::Blocking,kind:&str,id:&str)->rusqlite::Result<Vec<PathBuf>> {
        self.full_sync(blocking, |conn| {
            let tx=conn.unchecked_transaction()?;
            if kind == "playlist" {
                tx.execute("DELETE FROM followed_playlists WHERE id=?1", [id])?;
            }
            if kind == "auto" {
                tx.execute("DELETE FROM auto_downloads WHERE kind='music'", [])?;
            }
            tx.execute("DELETE FROM offline_sets WHERE kind=?1 AND id=?2",params![kind,id])?;
            tx.execute("DELETE FROM download_queue WHERE NOT EXISTS (SELECT 1 FROM offline_sets,json_each(offline_sets.songs) WHERE json_each.value=download_queue.song_id)",[])?;
            let removed={
                let mut query=tx.prepare("DELETE FROM offline_songs WHERE NOT EXISTS (SELECT 1 FROM offline_sets,json_each(offline_sets.songs) WHERE json_each.value=offline_songs.song_id) RETURNING song_id,path")?;
                query.query_map([],|row|Ok((row.get::<_,String>(0)?,row.get::<_,String>(1)?)))?.collect::<rusqlite::Result<Vec<_>>>()?
            };
            let mut obsolete=Vec::new();
            for (song,path) in removed {
                tx.execute("DELETE FROM download_queue WHERE song_id=?1",[&song])?;
                obsolete.push(PathBuf::from(path));obsolete.extend(replace_art_owner(blocking, &tx,"music",&song,None)?);
            }
            tx.commit()?;Ok(obsolete)
        })
    }

    pub fn queue_download(&self, blocking: &crate::rt::Blocking, song: &Song) {
        let _=self.full_sync(blocking, |conn| {let tx=conn.unchecked_transaction()?;queue_tracks(blocking, &tx,std::slice::from_ref(song))?;tx.commit()});
    }

    /// The next download not yet failed.
    pub fn next_download(&self, _blocking: &crate::rt::Blocking) -> Option<Song> {
        self.conn
            .query_row("SELECT song FROM download_queue WHERE error IS NULL ORDER BY queued LIMIT 1", [], |r| {
                r.get::<_, String>(0)
            })
            .optional()
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
    }

    pub fn finish_download(&self, _blocking: &crate::rt::Blocking, song_id: &str, error: Option<&str>) {
        match error {
            None => {
                let _ = self.conn.execute("DELETE FROM download_queue WHERE song_id = ?1", [song_id]);
            }
            Some(e) => {
                let _ =
                    self.conn.execute("UPDATE download_queue SET error = ?2 WHERE song_id = ?1", params![song_id, e]);
            }
        }
    }

    /// Queued and failed downloads.
    /// Songs waiting to download (failed ones excluded).
    pub(super) fn queued_download_ids_checked(&self, _blocking: &crate::rt::Blocking) -> rusqlite::Result<std::collections::HashSet<String>> {
        let mut query=self.conn.prepare("SELECT song_id FROM download_queue WHERE error IS NULL")?;
        let rows=query.query_map([],|r|r.get(0))?;rows.collect()
    }
    pub fn queued_download_ids(&self, _blocking: &crate::rt::Blocking) -> std::collections::HashSet<String> {
        let Ok(mut q) = self.conn.prepare("SELECT song_id FROM download_queue WHERE error IS NULL") else { return Default::default() };
        q.query_map([], |r| r.get::<_, String>(0)).map(|rows| rows.flatten().collect()).unwrap_or_default()
    }

    pub fn download_counts(&self, _blocking: &crate::rt::Blocking) -> (usize, usize) {
        let q = |sql| self.conn.query_row(sql, [], |r| r.get::<_, i64>(0)).unwrap_or(0) as usize;
        (
            q("SELECT COUNT(*) FROM download_queue WHERE error IS NULL"),
            q("SELECT COUNT(*) FROM download_queue WHERE error IS NOT NULL"),
        )
    }

    pub fn retry_failed_downloads(&self, _blocking: &crate::rt::Blocking) {
        let _ = self.conn.execute("UPDATE download_queue SET error = NULL", []);
    }

    // Actions waiting for the network.

    pub(super) fn queue_direct_listen(&self, blocking: &crate::rt::Blocking, account: &[u8], listen: super::direct_listens::Listen) -> rusqlite::Result<()> {
        let payload = serde_json::to_string(&listen).map_err(|_| rusqlite::Error::InvalidQuery)?;
        let started = i64::try_from(listen.started_at).map_err(|_| rusqlite::Error::InvalidQuery)?;
        self.full_sync(blocking, |conn| {
            conn.execute("DELETE FROM direct_listens WHERE account=?1 AND finished=1 AND started < ?2",
                params![account, started.saturating_sub(86400)])?;
            conn.execute("INSERT OR IGNORE INTO direct_listens (account,station,artist,title,started,payload)
                SELECT ?1,?2,?3,?4,?5,?6
                WHERE ?2 IS NULL OR NOT EXISTS (
                    SELECT 1 FROM direct_listens WHERE account=?1 AND station=?2 AND artist=?3 AND title=?4
                     AND started BETWEEN ?7 AND ?5
                )",
                params![account, listen.station, listen.artist, listen.title, started, payload,
                    started.saturating_sub(120)])?;
            Ok(())
        })
    }

    pub(super) fn next_direct_listen(&self, _blocking: &crate::rt::Blocking, account: &[u8])
        -> rusqlite::Result<Option<(i64, super::direct_listens::Listen)>> {
        let next: Option<(i64, String)> = self.conn.query_row(
            "SELECT id,payload FROM direct_listens WHERE account=?1 AND finished=0
                ORDER BY id LIMIT 1",
            [account], |row| Ok((row.get(0)?, row.get(1)?))).optional()?;
        next.map(|(id, payload)| {
            serde_json::from_str(&payload).map(|listen| (id, listen)).map_err(|_| rusqlite::Error::InvalidQuery)
        }).transpose()
    }

    pub(super) fn finish_direct_listen(&self, blocking: &crate::rt::Blocking, account: &[u8], id: i64) -> rusqlite::Result<()> {
        self.full_sync(blocking, |conn| {
            conn.execute("UPDATE direct_listens SET finished=1,payload='' WHERE account=?1 AND id=?2", params![account,id])?;
            Ok(())
        })
    }

    pub fn push_pending(&self, blocking: &crate::rt::Blocking, call: &PendingCall) {
        let _ = Self::insert_pending(blocking, &self.conn, call);
    }

    /// Background scrobbles retain NORMAL sync and the explicit checkpoint
    /// barrier. Report insertion failure without adding a per-song FULL sync.
    pub(super) fn push_pending_deferred(&self, blocking: &crate::rt::Blocking, call: &PendingCall) -> rusqlite::Result<()> {
        Self::insert_pending(blocking, &self.conn, call)
    }

    fn insert_pending(_blocking: &crate::rt::Blocking, conn: &Connection, call: &PendingCall) -> rusqlite::Result<()> {
        let text = serde_json::to_string(call).map_err(|_| rusqlite::Error::InvalidQuery)?;
        conn.execute(
            "INSERT INTO pending (call, created) VALUES (?1, ?2)",
            params![text, now()],
        )?;
        Ok(())
    }

    /// A user acknowledgement requires the queue row to reach durable storage.
    pub(super) fn push_pending_checked(&self, blocking: &crate::rt::Blocking, call: &PendingCall) -> rusqlite::Result<()> {
        self.full_sync(blocking, |conn| Self::insert_pending(blocking, conn, call))
    }

    #[cfg(test)]
    pub(crate) fn reject_music_actions(&self, _blocking: &crate::rt::Blocking, reject: bool) {
        self.conn.execute_batch(if reject {
            "CREATE TRIGGER reject_music_action BEFORE INSERT ON pending BEGIN SELECT RAISE(ABORT,'generated action fault'); END;"
        } else { "DROP TRIGGER reject_music_action;" }).unwrap();
    }

    pub fn pending(&self, _blocking: &crate::rt::Blocking, limit: usize) -> Vec<(i64, PendingCall)> {
        let mut st = match self.conn.prepare("SELECT id, call FROM pending ORDER BY id LIMIT ?1") {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        st.query_map([limit as i64], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
            .map(|rows| {
                rows.filter_map(Result::ok)
                    .filter_map(|(id, c)| serde_json::from_str(&c).ok().map(|c| (id, c)))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn drop_pending(&self, _blocking: &crate::rt::Blocking, id: i64) {
        let _ = self.conn.execute("DELETE FROM pending WHERE id = ?1", [id]);
    }

    /// Forgets everything tied to a server: for a change of server or user.
    pub fn reset(&self, blocking: &crate::rt::Blocking) -> Vec<PathBuf> {
        let mut files: Vec<PathBuf> = self.song_cache_lru(blocking, usize::MAX >> 1).into_iter().map(|(_, _, p, _)| p).collect();
        if let Ok(mut st) = self.conn.prepare("SELECT path FROM offline_songs") {
            if let Ok(rows) = st.query_map([], |r| r.get::<_, String>(0)) {
                files.extend(rows.filter_map(Result::ok).map(PathBuf::from));
            }
        }
        let _ = self.conn.execute_batch(
            "DELETE FROM followed_playlists; DELETE FROM auto_downloads WHERE kind='music'; DELETE FROM responses; DELETE FROM song_cache; DELETE FROM cached_song_meta; DELETE FROM offline_songs; DELETE FROM offline_sets;
             DELETE FROM download_queue; DELETE FROM pending; DELETE FROM kv WHERE key = 'queue';
             DELETE FROM kv WHERE key IN ('music-scan-marker', 'music-stale-before');
             DELETE FROM album_catalog_head; DELETE FROM album_catalog_entries; DELETE FROM album_catalog_groups;
             DELETE FROM album_catalog_versions; DELETE FROM album_catalog_markers;",
        );
        files
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shuffle_checks_all_small_library_rows_after_deletions_leave_large_gaps() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let db = Db::open(&blocking, Path::new(":memory:")).unwrap();
        // A long history of downloads and removals can leave two survivors
        // arbitrarily far apart. Probing random integers almost always maps
        // to the second row and can miss the only file still on the card.
        for (rowid, id) in [(1_i64, "first"), (1_i64 << 50, "last")] {
            let song = serde_json::to_string(&Song { id: id.into(), ..Default::default() }).unwrap();
            db.conn.execute("INSERT INTO offline_songs(rowid,song_id,path,size,song) VALUES (?1,?2,?2,1,?3)", params![rowid,id,song]).unwrap();
            db.conn.execute("INSERT INTO cached_song_meta(rowid,song_id,song) VALUES (?1,?2,?3)", params![rowid,id,song]).unwrap();
            db.put_cached_song(&blocking, id, "raw", Path::new(id), 1).unwrap();
        }
        let mut ids: Vec<_> = db.random_local_songs(&blocking, 2).unwrap().into_iter().map(|(s, _)| s.id).collect();
        ids.sort();
        assert_eq!(ids, ["first", "first", "last", "last"]);
        assert!(db.random_local_songs(&blocking, 0).unwrap().is_empty());
    }

    #[test]
    fn direct_listens_preserve_scope_order_and_acknowledgements_after_reopen() {
        let base = std::env::var_os("Q2_GENERATED_TEST_ROOT").map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build/perf/tests"));
        let root = base.join(format!("direct-listens-{}", fastrand::u64(..)));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("listens.sqlite");
        let blocking = crate::rt::Blocking::dedicated_thread();
        let a = [1; 32];
        let b = [2; 32];
        let track = |station: bool, time| super::super::direct_listens::Listen {
            artist: "Generated Artist".into(), title: "Generated Track".into(), album: None,
            duration_ms: None, station: station.then(|| "generated-radio".into()), started_at: time,
        };
        let db = Db::open(&blocking, &path).unwrap();
        db.queue_direct_listen(&blocking, &a, track(false, 1_900_000_000)).unwrap();
        db.queue_direct_listen(&blocking, &a, track(true, 1_900_000_020)).unwrap();
        drop(db);

        let db = Db::open(&blocking, &path).unwrap();
        assert!(db.next_direct_listen(&blocking, &b).unwrap().is_none());
        let (id, music) = db.next_direct_listen(&blocking, &a).unwrap().unwrap();
        assert!(music.station.is_none());
        db.finish_direct_listen(&blocking, &b, id).unwrap();
        assert_eq!(db.next_direct_listen(&blocking, &a).unwrap().unwrap().0, id);
        db.finish_direct_listen(&blocking, &a, id).unwrap();
        let (radio_id, radio) = db.next_direct_listen(&blocking, &a).unwrap().unwrap();
        assert!(radio.station.is_some());
        db.finish_direct_listen(&blocking, &a, radio_id).unwrap();
        drop(db);

        let db = Db::open(&blocking, &path).unwrap();
        db.queue_direct_listen(&blocking, &a, track(true, 1_900_000_070)).unwrap();
        db.queue_direct_listen(&blocking, &a, track(false, 1_900_000_000)).unwrap();
        assert!(db.next_direct_listen(&blocking, &a).unwrap().is_none());
        db.queue_direct_listen(&blocking, &b, track(true, 1_900_000_020)).unwrap();
        assert!(db.next_direct_listen(&blocking, &b).unwrap().is_some());
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sleep_checkpoint_reports_busy_reader_then_syncs_after_release() {
        let base = std::env::var_os("Q2_GENERATED_TEST_ROOT").map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build/perf/tests"));
        let root = base.join(format!("sleep-wal-{}", fastrand::u64(..)));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("generated.db");
        let db = Db::open(&crate::rt::Blocking::dedicated_thread(), &path).unwrap();
        db.conn.busy_timeout(std::time::Duration::ZERO).unwrap();
        db.set_checked(&crate::rt::Blocking::dedicated_thread(), "queue", "before").unwrap();
        let reader = Connection::open(&path).unwrap();
        reader.execute_batch("BEGIN").unwrap();
        let prior: String = reader.query_row("SELECT value FROM kv WHERE key='queue'", [], |r| r.get(0)).unwrap();
        assert_eq!(prior, "before");
        db.set_checked(&crate::rt::Blocking::dedicated_thread(), "queue", "after").unwrap();
        assert!(matches!(db.checkpoint(&crate::rt::Blocking::dedicated_thread()), Err(rusqlite::Error::SqliteFailure(error, _))
            if error.code == rusqlite::ErrorCode::DatabaseBusy));
        reader.execute_batch("ROLLBACK").unwrap();
        db.checkpoint(&crate::rt::Blocking::dedicated_thread()).unwrap();
        drop(db);
        assert_eq!(Db::open(&crate::rt::Blocking::dedicated_thread(), &path).unwrap().get(&crate::rt::Blocking::dedicated_thread(), "queue").as_deref(), Some("after"));
    }


    #[test]
    fn download_tickets_survive_legacy_rows_empty_queue_and_reset_without_stale_completion() {
        let db=Db::memory(&crate::rt::Blocking::dedicated_thread());let song=Song {id:"generated-ticket-song".into(),..Default::default()};
        db.conn.execute("INSERT INTO download_queue VALUES(?1,?2,100,NULL)",params![song.id,serde_json::to_string(&song).unwrap()]).unwrap();
        db.queue_download(&crate::rt::Blocking::dedicated_thread(), &song);let old=db.next_download_request(&crate::rt::Blocking::dedicated_thread()).unwrap().unwrap().ticket;assert_eq!(old,101);
        assert!(db.finish_download_request(&crate::rt::Blocking::dedicated_thread(), &song.id,old,None).unwrap());db.queue_download(&crate::rt::Blocking::dedicated_thread(), &song);
        let fresh=db.next_download_request(&crate::rt::Blocking::dedicated_thread()).unwrap().unwrap().ticket;assert!(fresh>old);
        assert!(!db.finish_download_request(&crate::rt::Blocking::dedicated_thread(), &song.id,old,Some("stale auth failure")).unwrap());
        assert!(db.complete_music_download(&crate::rt::Blocking::dedicated_thread(), &song,Path::new("stale.flac"),1,None,old).unwrap().is_none());
        assert!(db.download_request_current(&crate::rt::Blocking::dedicated_thread(), &song.id,fresh).unwrap());assert_eq!(db.download_counts(&crate::rt::Blocking::dedicated_thread()),(1,0));
        db.reset_owned_music(&crate::rt::Blocking::dedicated_thread()).unwrap();db.queue_download(&crate::rt::Blocking::dedicated_thread(), &song);
        let after=db.next_download_request(&crate::rt::Blocking::dedicated_thread()).unwrap().unwrap().ticket;assert!(after>fresh);
        assert!(!db.finish_download_request(&crate::rt::Blocking::dedicated_thread(), &song.id,fresh,None).unwrap());
        db.conn.execute("UPDATE kv SET value='invalid generated counter' WHERE key='download-sequence'",[]).unwrap();
        let set=OfflineSet {kind:"song".into(),id:"set".into(),name:String::new(),artist:String::new(),cover_art:None,song_ids:vec![song.id.clone()]};
        assert!(db.enqueue_download(&crate::rt::Blocking::dedicated_thread(), &set,std::slice::from_ref(&song)).is_err());assert_eq!(db.download_ticket(&crate::rt::Blocking::dedicated_thread(), &song.id).unwrap(),Some(after));
        db.conn.execute("UPDATE kv SET value=?1 WHERE key='download-sequence'",[i64::MAX.to_string()]).unwrap();
        assert!(db.enqueue_download(&crate::rt::Blocking::dedicated_thread(), &set,std::slice::from_ref(&song)).is_err());assert_eq!(db.download_ticket(&crate::rt::Blocking::dedicated_thread(), &song.id).unwrap(),Some(after));
        assert!(db.offline_set(&crate::rt::Blocking::dedicated_thread(), "song","set").is_none());
    }
    #[test]
    fn storage_index_counts_each_shared_cover_physical_row_as_download_before_cache() {
        let db=Db::memory(&crate::rt::Blocking::dedicated_thread());let art=generated_art(44);
        for owner in ["episode-one","episode-two"] {db.put_spoken_downloads_with_art(&crate::rt::Blocking::dedicated_thread(), owner,&[(owner.into(),PathBuf::from(format!("{owner}.mp3")),1)],Some(&art)).unwrap();}
        db.put_cached_song(&crate::rt::Blocking::dedicated_thread(), "same-file","raw",&art.path,art.size).unwrap();
        let indexed=db.indexed_media_paths(&crate::rt::Blocking::dedicated_thread()).unwrap();let entries=indexed.iter().filter(|(_,path)|path==&art.path).collect::<Vec<_>>();
        assert_eq!(entries.len(),2);assert!(entries[0].0);assert!(!entries[1].0);
        db.remove_spoken_downloads(&crate::rt::Blocking::dedicated_thread(), "episode-one").unwrap();assert_eq!(db.indexed_media_paths(&crate::rt::Blocking::dedicated_thread()).unwrap().iter().filter(|(download,path)|*download && path==&art.path).count(),1);
        db.remove_spoken_downloads(&crate::rt::Blocking::dedicated_thread(), "episode-two").unwrap();assert!(db.indexed_media_paths(&crate::rt::Blocking::dedicated_thread()).unwrap().iter().all(|(download,path)|!download || path!=&art.path));
    }
    #[test]
    fn checked_music_reset_preserves_spoken_art_and_rolls_back_on_index_failure() {
        let db=Db::memory(&crate::rt::Blocking::dedicated_thread());let shared=generated_art(0);let music_only=generated_art(1);
        for (id,art) in [("shared-song",&shared),("music-song",&music_only)] {
            let song=Song {id:id.into(),..Default::default()};db.queue_download(&crate::rt::Blocking::dedicated_thread(), &song);let ticket=db.download_ticket(&crate::rt::Blocking::dedicated_thread(), id).unwrap().unwrap();
            db.complete_music_download(&crate::rt::Blocking::dedicated_thread(), &song,Path::new(id),1,Some(art),ticket).unwrap().unwrap();
        }
        db.put_spoken_downloads_with_art(&crate::rt::Blocking::dedicated_thread(), "generated-book",&[("generated-part".into(),PathBuf::from("generated.m4b"),1)],Some(&shared)).unwrap();
        db.conn.execute_batch("CREATE TRIGGER fail_reset BEFORE DELETE ON offline_art_refs BEGIN SELECT RAISE(ABORT,'generated reset fault'); END;").unwrap();
        assert!(db.reset_owned_music(&crate::rt::Blocking::dedicated_thread()).is_err());assert!(db.offline_song(&crate::rt::Blocking::dedicated_thread(), "shared-song").is_some());assert!(db.offline_art(&crate::rt::Blocking::dedicated_thread(), &music_only.key).unwrap().is_some());
        db.conn.execute_batch("DROP TRIGGER fail_reset").unwrap();let removed=db.reset_owned_music(&crate::rt::Blocking::dedicated_thread()).unwrap();
        assert!(removed.contains(&music_only.path));assert!(!removed.contains(&shared.path));assert!(db.offline_art(&crate::rt::Blocking::dedicated_thread(), &shared.key).unwrap().is_some());assert!(db.offline_art(&crate::rt::Blocking::dedicated_thread(), &music_only.key).unwrap().is_none());
        assert!(db.spoken_download(&crate::rt::Blocking::dedicated_thread(), "generated-part").unwrap().is_some());assert!(db.offline_ids(&crate::rt::Blocking::dedicated_thread()).is_empty());
    }
    fn seed_music_binding(db:&Db) -> (OfflineArt,i64) {
        db.conn.execute("INSERT INTO kv VALUES('server','generated-old-account')",[]).unwrap();
        let art=generated_art(91);let song=Song {id:"binding-song".into(),..Default::default()};
        db.queue_download(&crate::rt::Blocking::dedicated_thread(), &song);let ticket=db.download_ticket(&crate::rt::Blocking::dedicated_thread(), &song.id).unwrap().unwrap();
        db.complete_music_download(&crate::rt::Blocking::dedicated_thread(), &song,Path::new("generated/binding-song.mp3"),12,Some(&art),ticket).unwrap().unwrap();
        db.put_cached_song(&crate::rt::Blocking::dedicated_thread(), "binding-cache","raw",Path::new("generated/binding-cache.mp3"),12).unwrap();
        db.put_spoken_downloads_with_art(&crate::rt::Blocking::dedicated_thread(), "binding-book",&[("binding-spoken-part".into(),PathBuf::from("generated/binding-spoken.m4b"),12)],Some(&art)).unwrap();
        let queued=Song {id:"binding-queued".into(),..Default::default()};db.queue_download(&crate::rt::Blocking::dedicated_thread(), &queued);
        (art,db.download_ticket(&crate::rt::Blocking::dedicated_thread(), &queued.id).unwrap().unwrap())
    }
    fn assert_music_binding_retained(db:&Db,art:&OfflineArt,ticket:i64) {
        assert_eq!(db.offline_song(&crate::rt::Blocking::dedicated_thread(), "binding-song"),Some(PathBuf::from("generated/binding-song.mp3")));
        assert!(db.cached_song(&crate::rt::Blocking::dedicated_thread(), "binding-cache",&["raw"]).is_some());
        assert_eq!(db.offline_art(&crate::rt::Blocking::dedicated_thread(), &art.key).unwrap(),Some(art.clone()));
        assert!(db.spoken_download(&crate::rt::Blocking::dedicated_thread(), "binding-spoken-part").unwrap().is_some());
        assert_eq!(db.download_ticket(&crate::rt::Blocking::dedicated_thread(), "binding-queued").unwrap(),Some(ticket));
        assert!(db.download_request_current(&crate::rt::Blocking::dedicated_thread(), "binding-queued",ticket).unwrap());
    }
    #[test]
    fn music_binding_marker_insert_or_update_failure_rolls_back_reset_and_preserves_spoken() {
        for existing_marker in [false,true] {
            let db=Db::memory(&crate::rt::Blocking::dedicated_thread());let (art,ticket)=seed_music_binding(&db);
            if !existing_marker {db.conn.execute("DELETE FROM kv WHERE key='server'",[]).unwrap();}
            let trigger=if existing_marker {"CREATE TRIGGER reject_marker BEFORE UPDATE ON kv WHEN NEW.key='server' BEGIN SELECT RAISE(ABORT,'generated marker update fault'); END;"}
                else {"CREATE TRIGGER reject_marker BEFORE INSERT ON kv WHEN NEW.key='server' BEGIN SELECT RAISE(ABORT,'generated marker insert fault'); END;"};
            db.conn.execute_batch(trigger).unwrap();
            assert!(db.bind_music_server(&crate::rt::Blocking::dedicated_thread(), "generated-new-account").is_err());
            assert_music_binding_retained(&db,&art,ticket);
            let marker=db.conn.query_row("SELECT value FROM kv WHERE key='server'",[],|row|row.get::<_,String>(0)).optional().unwrap();
            assert_eq!(marker,existing_marker.then(||"generated-old-account".to_string()));
            db.conn.execute_batch("DROP TRIGGER reject_marker").unwrap();
            let (changed,removed)=db.bind_music_server(&crate::rt::Blocking::dedicated_thread(), "generated-new-account").unwrap();assert!(changed);
            assert!(removed.contains(&PathBuf::from("generated/binding-song.mp3")));
            assert!(removed.contains(&PathBuf::from("generated/binding-cache.mp3")));
            assert!(!removed.contains(&art.path));assert!(db.offline_art(&crate::rt::Blocking::dedicated_thread(), &art.key).unwrap().is_some());
            assert!(db.spoken_download(&crate::rt::Blocking::dedicated_thread(), "binding-spoken-part").unwrap().is_some());assert!(db.offline_ids(&crate::rt::Blocking::dedicated_thread()).is_empty());
            assert!(db.next_download_request(&crate::rt::Blocking::dedicated_thread()).unwrap().is_none());
            assert_eq!(db.conn.query_row("SELECT value FROM kv WHERE key='server'",[],|row|row.get::<_,String>(0)).unwrap(),"generated-new-account");
            db.queue_download(&crate::rt::Blocking::dedicated_thread(), &Song {id:"after-binding".into(),..Default::default()});
            assert!(db.download_ticket(&crate::rt::Blocking::dedicated_thread(), "after-binding").unwrap().unwrap()>ticket);
        }
    }
    #[test]
    fn music_binding_rejects_unreadable_marker_and_same_account_preserves_music() {
        let db=Db::memory(&crate::rt::Blocking::dedicated_thread());let (art,ticket)=seed_music_binding(&db);
        let (changed,removed)=db.bind_music_server(&crate::rt::Blocking::dedicated_thread(), "generated-old-account").unwrap();assert!(!changed);assert!(removed.is_empty());
        assert_music_binding_retained(&db,&art,ticket);
        db.conn.execute("UPDATE kv SET value=zeroblob(1) WHERE key='server'",[]).unwrap();
        assert!(db.bind_music_server(&crate::rt::Blocking::dedicated_thread(), "generated-new-account").is_err());assert_music_binding_retained(&db,&art,ticket);
        assert_eq!(db.conn.query_row("SELECT typeof(value) FROM kv WHERE key='server'",[],|row|row.get::<_,String>(0)).unwrap(),"blob");
        db.conn.execute("UPDATE kv SET value='generated-old-account' WHERE key='server'",[]).unwrap();
        db.conn.execute_batch("ALTER TABLE kv RENAME TO generated_inaccessible_kv;").unwrap();
        assert!(db.bind_music_server(&crate::rt::Blocking::dedicated_thread(), "generated-new-account").is_err());
        assert!(db.offline_song(&crate::rt::Blocking::dedicated_thread(), "binding-song").is_some());assert!(db.offline_art(&crate::rt::Blocking::dedicated_thread(), &art.key).unwrap().is_some());
        assert_eq!(db.conn.query_row("SELECT value FROM generated_inaccessible_kv WHERE key='server'",[],|row|row.get::<_,String>(0)).unwrap(),"generated-old-account");
    }
    #[test]
    fn unchanged_music_binding_does_not_commit_database_changes() {
        let root=PathBuf::from(format!("build/perf/music-binding-{:x}",fastrand::u64(..)));
        std::fs::create_dir_all(&root).unwrap();
        let db=Db::open(&crate::rt::Blocking::dedicated_thread(), &root.join("generated.db")).unwrap();let (art,ticket)=seed_music_binding(&db);
        let observer=Connection::open_with_flags(root.join("generated.db"),rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let version=||observer.pragma_query_value::<i64,_>(None,"data_version",|row|row.get(0)).unwrap();
        let before=version();
        for _ in 0..10 {
            let (changed,removed)=db.bind_music_server(&crate::rt::Blocking::dedicated_thread(), "generated-old-account").unwrap();
            assert!(!changed);assert!(removed.is_empty());assert_eq!(version(),before);
        }
        assert_music_binding_retained(&db,&art,ticket);
        assert!(db.bind_music_server(&crate::rt::Blocking::dedicated_thread(), "generated-new-account").unwrap().0);
        assert_ne!(version(),before);
        assert!(db.offline_song(&crate::rt::Blocking::dedicated_thread(), "binding-song").is_none());
        drop(observer);drop(db);std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn generated_binding_benchmark_exclusively_claims_scratch_before_opening_database() {
        let root=PathBuf::from(format!("build/perf/binding-scratch-{:x}",fastrand::u64(..)));
        let barrier=std::sync::Arc::new(std::sync::Barrier::new(2));
        let workers=(0..2).map(|_| {
            let root=root.clone();let barrier=barrier.clone();
            std::thread::spawn(move || {barrier.wait();Db::startup_binding_benchmark(&crate::rt::Blocking::dedicated_thread(), &root,1,"same")})
        }).collect::<Vec<_>>();
        let results=workers.into_iter().map(|w|w.join().unwrap()).collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|r|r.is_ok()).count(),1);
        assert_eq!(results.iter().filter(|r|r.is_err()).count(),1);
        std::fs::remove_dir_all(&root).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("library.db"),b"generated foreign database sentinel").unwrap();
        assert!(Db::startup_binding_benchmark(&crate::rt::Blocking::dedicated_thread(), &root,1,"same").is_err());
        assert_eq!(std::fs::read(root.join("library.db")).unwrap(),b"generated foreign database sentinel");
        assert!(!root.join(".q2-generated-binding-benchmark").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
    fn generated_art(index:usize)->OfflineArt {OfflineArt {key:format!("generated-cover-{index}"),path:PathBuf::from(format!("generated/offline-art/{index}.png")),size:1024}}
    #[test]
    fn artwork_has_one_physical_row_and_survives_until_the_last_music_or_spoken_owner() {
        let db=Db::memory(&crate::rt::Blocking::dedicated_thread());let art=generated_art(0);let song=Song {id:"generated-song".into(),..Default::default()};
        db.queue_download(&crate::rt::Blocking::dedicated_thread(), &song);db.complete_music_download(&crate::rt::Blocking::dedicated_thread(), &song,Path::new("generated/offline/song.flac"),1234,Some(&art),db.download_ticket(&crate::rt::Blocking::dedicated_thread(), &song.id).unwrap().unwrap()).unwrap().unwrap();
        for owner in ["episode-a","episode-b"] {db.put_spoken_downloads_with_art(&crate::rt::Blocking::dedicated_thread(), owner,&[(owner.into(),PathBuf::from(format!("generated/{owner}.mp3")),100)],Some(&art)).unwrap();}
        assert_eq!(db.conn.query_row("SELECT COUNT(*) FROM offline_art",[],|row|row.get::<_,u32>(0)).unwrap(),1);
        assert!(!db.remove_spoken_downloads(&crate::rt::Blocking::dedicated_thread(), "episode-a").unwrap().contains(&art.path));assert!(db.offline_art(&crate::rt::Blocking::dedicated_thread(), &art.key).unwrap().is_some());
        db.put_offline_set(&crate::rt::Blocking::dedicated_thread(), &OfflineSet {kind:"song".into(),id:song.id.clone(),name:String::new(),artist:String::new(),cover_art:None,song_ids:vec![song.id.clone()]});
        assert!(!db.remove_offline_set_checked(&crate::rt::Blocking::dedicated_thread(), "song",&song.id).unwrap().contains(&art.path));
        assert!(db.remove_spoken_downloads(&crate::rt::Blocking::dedicated_thread(), "episode-b").unwrap().contains(&art.path));assert!(db.offline_art(&crate::rt::Blocking::dedicated_thread(), &art.key).unwrap().is_none());
    }
    #[test]
    fn cancelled_requests_cannot_publish_and_artwork_fault_rolls_back_the_whole_download() {
        let db=Db::memory(&crate::rt::Blocking::dedicated_thread());let song=Song {id:"generated-song".into(),..Default::default()};let original=generated_art(0);
        assert!(db.complete_music_download(&crate::rt::Blocking::dedicated_thread(), &song,Path::new("never.flac"),1,Some(&original),0).unwrap().is_none());assert!(db.offline_song(&crate::rt::Blocking::dedicated_thread(), &song.id).is_none());
        db.queue_download(&crate::rt::Blocking::dedicated_thread(), &song);db.complete_music_download(&crate::rt::Blocking::dedicated_thread(), &song,Path::new("old.flac"),1,Some(&original),db.download_ticket(&crate::rt::Blocking::dedicated_thread(), &song.id).unwrap().unwrap()).unwrap().unwrap();
        db.queue_download(&crate::rt::Blocking::dedicated_thread(), &song);db.conn.execute_batch("CREATE TRIGGER fail_art BEFORE INSERT ON offline_art_refs BEGIN SELECT RAISE(ABORT,'generated art fault'); END;").unwrap();
        let changed=generated_art(1);assert!(db.complete_music_download(&crate::rt::Blocking::dedicated_thread(), &song,Path::new("new.flac"),2,Some(&changed),db.download_ticket(&crate::rt::Blocking::dedicated_thread(), &song.id).unwrap().unwrap()).is_err());
        assert_eq!(db.offline_song(&crate::rt::Blocking::dedicated_thread(), &song.id),Some(PathBuf::from("old.flac")));assert_eq!(db.offline_art(&crate::rt::Blocking::dedicated_thread(), &original.key).unwrap(),Some(original));assert!(db.offline_art(&crate::rt::Blocking::dedicated_thread(), &changed.key).unwrap().is_none());assert_eq!(db.next_download(&crate::rt::Blocking::dedicated_thread()).unwrap().id,song.id);
        db.conn.execute_batch("DROP TRIGGER fail_art").unwrap();
        db.put_offline_set(&crate::rt::Blocking::dedicated_thread(), &OfflineSet {kind:"song".into(),id:song.id.clone(),name:String::new(),artist:String::new(),cover_art:None,song_ids:vec![song.id.clone()]});
        db.remove_offline_set_checked(&crate::rt::Blocking::dedicated_thread(), "song",&song.id).unwrap();assert!(db.next_download(&crate::rt::Blocking::dedicated_thread()).is_none());
        assert!(db.complete_music_download(&crate::rt::Blocking::dedicated_thread(), &song,Path::new("late.flac"),1,Some(&changed),0).unwrap().is_none());
    }
    #[test]
    fn generated_shared_art_reassignments_match_reference_ownership() {
        let db=Db::memory(&crate::rt::Blocking::dedicated_thread());let mut owners=std::collections::HashMap::<usize,usize>::new();let mut rng=fastrand::Rng::with_seed(0xC0_2026);
        for _ in 0..1024 {
            let owner=rng.usize(0..32);let key=rng.usize(0..16);let id=format!("logical-{owner}");
            if rng.bool() {db.put_spoken_downloads_with_art(&crate::rt::Blocking::dedicated_thread(), &id,&[(id.clone(),PathBuf::from(format!("generated/{owner}.m4b")),100)],Some(&generated_art(key))).unwrap();owners.insert(owner,key);}
            else {db.remove_spoken_downloads(&crate::rt::Blocking::dedicated_thread(), &id).unwrap();owners.remove(&owner);}
            for key in 0..16 {assert_eq!(db.offline_art(&crate::rt::Blocking::dedicated_thread(), &generated_art(key).key).unwrap().is_some(),owners.values().any(|value|*value==key));}
            assert_eq!(db.conn.query_row("SELECT COUNT(*) FROM offline_art_refs",[],|row|row.get::<_,u32>(0)).unwrap() as usize,owners.len());
        }
    }
    #[test]
    fn speed_overrides_survive_restart_clear_explicitly_and_report_storage_faults() {
        let root = PathBuf::from(format!("build/perf/spoken-speeds-{:x}",fastrand::u64(..)));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("generated.db");
        let db = Db::open(&crate::rt::Blocking::dedicated_thread(), &path).unwrap();
        assert_eq!(db.spoken_speed(&crate::rt::Blocking::dedicated_thread(), "book-a").unwrap(), None);
        db.set_spoken_speed(&crate::rt::Blocking::dedicated_thread(), "book-a", Some(175)).unwrap();
        db.set_spoken_speed(&crate::rt::Blocking::dedicated_thread(), "podcast-parent", Some(225)).unwrap();
        assert!(db.set_spoken_speed(&crate::rt::Blocking::dedicated_thread(), "book-a", Some(176)).is_err());
        db.conn.execute_batch("CREATE TRIGGER generated_speed_fault BEFORE UPDATE ON spoken_speeds BEGIN SELECT RAISE(ABORT,'generated card full'); END").unwrap();
        assert!(db.set_spoken_speed(&crate::rt::Blocking::dedicated_thread(), "book-a", Some(200)).is_err());
        assert_eq!(db.spoken_speed(&crate::rt::Blocking::dedicated_thread(), "book-a").unwrap(), Some(175));
        db.conn.execute_batch("DROP TRIGGER generated_speed_fault").unwrap();
        db.reset(&crate::rt::Blocking::dedicated_thread()); // Navidrome reset cannot erase independent spoken choices.
        drop(db);
        let db = Db::open(&crate::rt::Blocking::dedicated_thread(), &path).unwrap();
        assert_eq!(db.spoken_speed(&crate::rt::Blocking::dedicated_thread(), "book-a").unwrap(), Some(175));
        assert_eq!(db.spoken_speed(&crate::rt::Blocking::dedicated_thread(), "podcast-parent").unwrap(), Some(225));
        db.set_spoken_speed(&crate::rt::Blocking::dedicated_thread(), "book-a", None).unwrap();
        assert_eq!(db.spoken_speed(&crate::rt::Blocking::dedicated_thread(), "book-a").unwrap(), None);
        db.conn.execute_batch("ALTER TABLE spoken_speeds RENAME TO generated_unreadable_speeds").unwrap();
        assert!(db.spoken_speed(&crate::rt::Blocking::dedicated_thread(), "book-a").is_err());
        assert!(db.set_spoken_speed(&crate::rt::Blocking::dedicated_thread(), "book-a", None).is_err());
        drop(db);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unreadable_checkpoint_is_an_error_and_recovery_keeps_the_saved_value() {
        let db=Db::memory(&crate::rt::Blocking::dedicated_thread());
        db.save_spoken_position(&crate::rt::Blocking::dedicated_thread(), "generated-book",700,10).unwrap();
        db.conn.execute_batch("ALTER TABLE spoken_positions RENAME TO generated_unreadable_positions").unwrap();
        let root=PathBuf::from(format!("build/perf/bookmark-read-fault-{:x}",fastrand::u64(..)));
        let paths=crate::config::Paths::new(root.clone());paths.create_all(&crate::rt::Blocking::dedicated_thread()).unwrap();
        let lib=super::super::Library::open(paths,&crate::config::Config::default(),std::sync::Arc::new(|_|{}));
        *lib.db()=db;
        assert!(lib.spoken_position("generated-book").is_err());
        lib.db().conn.execute_batch("ALTER TABLE generated_unreadable_positions RENAME TO spoken_positions").unwrap();
        assert_eq!(lib.spoken_position("generated-book").unwrap().unwrap().position_ms,700);
        drop(lib);let _=std::fs::remove_dir_all(root);
    }

    #[test]
    fn spoken_checkpoints_report_failed_writes_and_survive_navidrome_reset() {
        let db=Db::memory(&crate::rt::Blocking::dedicated_thread());
        db.save_spoken_position(&crate::rt::Blocking::dedicated_thread(), "generated-book",100,10).unwrap();
        let before=db.spoken_position(&crate::rt::Blocking::dedicated_thread(), "generated-book").unwrap().unwrap();
        db.conn.execute_batch("CREATE TRIGGER reject_spoken BEFORE UPDATE ON spoken_positions BEGIN SELECT RAISE(ABORT,'synthetic card full'); END").unwrap();
        assert!(db.save_spoken_position(&crate::rt::Blocking::dedicated_thread(), "generated-book",700,11).is_err());
        assert_eq!(db.spoken_position(&crate::rt::Blocking::dedicated_thread(), "generated-book").unwrap(),Some(before.clone()));
        db.conn.execute_batch("DROP TRIGGER reject_spoken").unwrap();
        db.save_spoken_position(&crate::rt::Blocking::dedicated_thread(), "generated-book",700,11).unwrap();
        db.put_spoken_response(&crate::rt::Blocking::dedicated_thread(), "abs-generated","cached-library").unwrap();
        db.reset(&crate::rt::Blocking::dedicated_thread());
        assert_eq!(db.spoken_position(&crate::rt::Blocking::dedicated_thread(), "generated-book").unwrap().unwrap().position_ms,700);
        assert_eq!(db.spoken_response(&crate::rt::Blocking::dedicated_thread(), "abs-generated").unwrap().as_deref(),Some("cached-library"));
    }
    #[test]
    fn session_and_bookmark_commit_atomically_and_retry_preserves_identity() {
        let db = Db::memory(&crate::rt::Blocking::dedicated_thread());
        let blocking = crate::rt::Blocking::dedicated_thread();
        let song = Song::default();
        let session = crate::audio::listening::Session { id: "generated-session".into(), started_ms: 1,
            start_position_ms: 0, listened_ms: 1000, last_listened_ms: 1001 };
        db.save_spoken_position(&blocking, "generated", 10, 1).unwrap();
        let before = db.spoken_position(&blocking, "generated").unwrap();
        db.conn.execute_batch("CREATE TRIGGER reject_session BEFORE INSERT ON spoken_sessions BEGIN SELECT RAISE(ABORT,'synthetic card full'); END").unwrap();
        assert!(db.save_spoken_checkpoint(&blocking, "generated", 1000, 100, Some((&song, &session))).is_err());
        assert_eq!(db.spoken_position(&blocking, "generated").unwrap(), before);
        assert!(!db.has_pending_spoken_session(&blocking, "generated").unwrap());
        db.conn.execute_batch("DROP TRIGGER reject_session").unwrap();
        db.save_spoken_checkpoint(&blocking, "generated", 1000, 100, Some((&song, &session))).unwrap();
        let body: String = db.conn.query_row("SELECT body FROM spoken_sessions", [], |r| r.get(0)).unwrap();
        db.save_spoken_checkpoint(&blocking, "generated", 1000, 9999, Some((&song, &session))).unwrap();
        assert_eq!(db.conn.query_row("SELECT body FROM spoken_sessions", [], |r| r.get::<_, String>(0)).unwrap(), body);
        assert_eq!(db.spoken_position(&blocking, "generated").unwrap().unwrap().updated_ms, 100);
    }

    #[test]
    fn session_batches_are_bounded_and_other_accounts_do_not_starve_them() {
        let blocking=crate::rt::Blocking::dedicated_thread();let db=Db::memory(&blocking);
        // Unrelated account data must not even be decoded by this pass.
        db.conn.execute("INSERT INTO spoken_sessions VALUES('other','abs:other:item:','invalid-json',1)",[]).unwrap();
        let logical="abs:generated:item:";
        let song=Song {spoken:Some(Box::new(subsonic::Spoken {id:logical.into(),source:subsonic::SpokenSource::Audiobookshelf {
            namespace:"generated".into(),item:"item".into(),episode:None,content_url:String::new()},..Default::default()})),..Default::default()};
        for n in 0..70 {
            let session=crate::audio::listening::Session{id:format!("session-{n}"),started_ms:n,start_position_ms:0,listened_ms:1000,last_listened_ms:n+1000};
            db.save_spoken_checkpoint(&blocking,logical,n+1,n+1,Some((&song,&session))).unwrap();
        }
        let first=db.pending_spoken_sessions(&blocking,"generated").unwrap();assert_eq!(first.len(),64);
        for upload in first {db.acknowledge_spoken_session(&blocking,&upload).unwrap();}
        assert_eq!(db.pending_spoken_sessions(&blocking,"generated").unwrap().len(),6);
        assert!(db.pending_spoken_sessions(&blocking,"generate").unwrap().is_empty());
    }

    #[test]
    fn unchanged_checkpoint_preserves_remote_timestamp_and_pending_revision() {
        let db = Db::memory(&crate::rt::Blocking::dedicated_thread());
        let blocking = crate::rt::Blocking::dedicated_thread();
        db.accept_spoken_remote(&blocking, "generated", 12345, 100).unwrap();
        let before = db.spoken_position(&blocking, "generated").unwrap();
        db.save_spoken_position(&blocking, "generated", 12345, 99999).unwrap();
        assert_eq!(db.spoken_position(&blocking, "generated").unwrap(), before);
        db.save_spoken_position(&blocking, "generated", 14000, 200).unwrap();
        let dirty = db.spoken_position(&blocking, "generated").unwrap();
        db.save_spoken_position(&blocking, "generated", 14000, 99999).unwrap();
        assert_eq!(db.spoken_position(&blocking, "generated").unwrap(), dirty);
    }

    #[test]
    fn seeded_progress_sync_replays_delays_rewinds_and_clock_reversal() {
        let db=Db::memory(&crate::rt::Blocking::dedicated_thread());let mut model:Option<super::super::spoken::Position>=None;
        let mut rng=fastrand::Rng::with_seed(0xAB50FF);
        for _ in 0..10000 {
            let previous=model.clone();let p=rng.u64(0..100000);let time=rng.u64(0..100000);
            match rng.u8(0..4) {
                0|1=>{db.save_spoken_position(&crate::rt::Blocking::dedicated_thread(), "generated",p,time).unwrap();if previous.as_ref().is_none_or(|v|v.position_ms!=p) {model=Some(super::super::spoken::Position {position_ms:p,
                    updated_ms:previous.as_ref().map(|v|v.updated_ms+1).unwrap_or(0).max(time),revision:previous.as_ref().map(|v|v.revision+1).unwrap_or(1),dirty:true});}},
                2=>{let rev=rng.u64(0..10000);db.acknowledge_spoken(&crate::rt::Blocking::dedicated_thread(), "generated",rev).unwrap();if let Some(v)=&mut model {if v.revision==rev{v.dirty=false;}}},
                _=>{db.accept_spoken_remote(&crate::rt::Blocking::dedicated_thread(), "generated",p,time).unwrap();if model.as_ref().is_none_or(|v|!v.dirty && time>v.updated_ms) {
                    let revision=previous.as_ref().map(|v|v.revision).unwrap_or(0);model=Some(super::super::spoken::Position {position_ms:p,updated_ms:time,revision,dirty:false});
                }}
            }
            assert_eq!(db.spoken_position(&crate::rt::Blocking::dedicated_thread(), "generated").unwrap(),model);
        }
    }

    /// Browse reads at SQLite page caches of 2,000, 512 and 256 KiB: album
    /// catalog windows (100 albums at random positions of 20,000) and a
    /// stored 5,000-song Favorites copy parsed as it is read. Host only, with
    /// the file in the kernel's cache. Run with --ignored --nocapture.
    #[test]
    #[ignore]
    fn browse_reads_by_page_cache_size() {
        let b = crate::rt::Blocking::dedicated_thread();
        let dir = crate::library::generated_test_directory("sqlite-cache");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("library.db");
        {
            let mut db = Db::open(&b, &path).unwrap();
            let build = db.catalog_begin(&b).unwrap();
            for offset in (0..20_000u32).step_by(500) {
                let rows: Vec<_> = (offset..offset + 500).map(|i| (subsonic::Album { id: format!("album-{i:06}"), name: format!("Generated album {i}"),
                    artist: format!("Generated artist {}", i / 10), cover_art: Some(format!("al-{i:06}")), song_count: 12, ..Default::default() }, "G".to_string())).collect();
                db.catalog_append(&b, &build, offset, &rows).unwrap();
            }
            db.catalog_append(&b, &build, 20_000, &[]).unwrap();
            db.catalog_publish(&b, &build).unwrap();
            let starred = subsonic::Starred { song: (0..5000).map(|i| Song { id: format!("song-{i:06}"), title: format!("Generated title {i}"),
                album: format!("Generated album {}", i / 12), artist: format!("Generated artist {}", i / 120), cover_art: Some(format!("al-{:06}", i / 12)),
                suffix: Some("flac".into()), duration: 240, ..Default::default() }).collect(), ..Default::default() };
            let call = subsonic::calls::starred();
            db.put_response_with(&b, "getStarred2", &|out| call.write_stored(&starred, out).map_err(std::io::Error::other));
        }
        for kib in [2000i64, 512, 256] {
            let mut db = Db::open(&b, &path).unwrap();
            db.conn.pragma_update(None, "cache_size", -kib).unwrap();
            let catalog = db.catalog_current(&b).unwrap().unwrap();
            let mut pages = Vec::new();
            let mut rng = fastrand::Rng::with_seed(7);
            for _ in 0..400 {
                let start = rng.u32(0..19_900);
                let t = std::time::Instant::now();
                assert_eq!(db.catalog_page(&b, &catalog, start, 100).unwrap().len(), 100);
                pages.push(t.elapsed().as_micros());
            }
            let mut stored = Vec::new();
            for _ in 0..10 {
                let t = std::time::Instant::now();
                let mut parser = subsonic::calls::starred().stored_parser();
                assert_eq!(db.response_pieces(&b, "getStarred2", &mut |piece| parser.feed(piece).is_ok()), Some(true));
                let s = parser.finish().unwrap();
                assert_eq!(s.song.len(), 5000);
                stored.push(t.elapsed().as_micros());
            }
            pages.sort(); stored.sort();
            eprintln!("cache {kib} KiB: catalog window p50 {} us p95 {} us max {} us; stored Favorites p50 {} us max {} us",
                pages[200], pages[380], pages[399], stored[5], stored[9]);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn card_free_metadata_has_a_one_mib_page_budget() {
        let db = Db::temporary(&crate::rt::Blocking::dedicated_thread());
        for i in 0..256 { db.put_response(&crate::rt::Blocking::dedicated_thread(), &format!("generated-{i}"), &"x".repeat(16 * 1024)); }
        let page_size: i64 = db.conn.pragma_query_value(None, "page_size", |r| r.get(0)).unwrap();
        let page_count: i64 = db.conn.pragma_query_value(None, "page_count", |r| r.get(0)).unwrap();
        assert_eq!(page_size, 4096);
        assert!(page_count * page_size <= 1024 * 1024);
        assert!(db.response(&crate::rt::Blocking::dedicated_thread(), "generated-0").is_some());
        assert!(db.response(&crate::rt::Blocking::dedicated_thread(), "generated-255").is_none());
    }

    fn song(id: &str) -> Song {
        Song { id: id.into(), title: format!("Song {id}"), ..Default::default() }
    }

    /// A list opened again (NetworkFirst) or an album warmed again returns
    /// the same document: storing it writes one page for its time, not the
    /// body. A changed document is stored whole. Spoken pages the same.
    #[test]
    fn storing_an_unchanged_response_writes_no_body_pages() {
        let b = crate::rt::Blocking::dedicated_thread();
        let dir = crate::library::generated_test_directory("unchanged-response");
        std::fs::create_dir_all(&dir).unwrap();
        let db = Db::open(&b, &dir.join("library.db")).unwrap();
        db.hold_checkpoints();
        let starred = |title: &str| subsonic::Starred { song: (0..3000).map(|i| Song { id: format!("song-{i:06}"), title: format!("{title} {i}"),
            album: format!("Generated album {}", i / 12), ..Default::default() }).collect(), ..Default::default() };
        let call = subsonic::calls::starred();
        let store = |value: &subsonic::Starred| db.put_response_with(&b, "getStarred2", &|out| call.write_stored(value, out).map_err(std::io::Error::other));
        let first = starred("Generated");
        let wal = db.wal_bytes();
        store(&first);
        let body = db.wal_bytes() - wal;
        assert!(body > 200_000, "the first copy is written: {body}");
        db.conn.execute("UPDATE responses SET fetched = fetched - 100 WHERE key = 'getStarred2'", []).unwrap();
        let wal = db.wal_bytes();
        store(&first);
        let again = db.wal_bytes() - wal;
        assert!(again <= 3 * 4120, "the same document writes its time only: {again} bytes");
        assert!(db.response_fresh(&b, "getStarred2", 60), "its time is renewed");
        let mut parsed = call.stored_parser();
        assert_eq!(db.response_pieces(&b, "getStarred2", &mut |piece| parsed.feed(piece).is_ok()), Some(true));
        assert_eq!(parsed.finish().unwrap(), first);
        let changed = starred("Renamed");
        let wal = db.wal_bytes();
        store(&changed);
        assert!(db.wal_bytes() - wal > 200_000, "a changed document is stored");
        let mut parsed = call.stored_parser();
        db.response_pieces(&b, "getStarred2", &mut |piece| parsed.feed(piece).is_ok());
        assert_eq!(parsed.finish().unwrap(), changed);
        let page = "x".repeat(100_000);
        db.put_spoken_response_durable(&b, "shelf-browse:generated", &page).unwrap();
        let wal = db.wal_bytes();
        db.put_spoken_response_durable(&b, "shelf-browse:generated", &page).unwrap();
        db.put_spoken_response(&b, "shelf-browse:generated", &page).unwrap();
        assert_eq!(db.wal_bytes() - wal, 0, "an unchanged spoken page is not written");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A page and its ETag are one row: a first store is one row change, an
    /// unchanged one none, and the page still reads as a page. (Two rows, the
    /// first form, put 2.5 times the WAL bytes of the old sync through a full
    /// sync of the owner's library: 174 MB against 70.)
    #[test]
    fn a_page_and_its_etag_are_one_row() {
        let b = crate::rt::Blocking::dedicated_thread();
        let db = Db::memory(&b);
        let page = subsonic::audiobookshelf_browse::Page { total: Some(3), has_more: true, ..Default::default() };
        let body = serde_json::to_string(&page).unwrap();
        let before = db.total_changes();
        assert!(db.put_spoken_page(&b, "shelf-browse:n:p", &body, Some("W/\"1a-x\\y\"")).unwrap());
        assert_eq!(db.total_changes() - before, 1);
        assert!(!db.put_spoken_page(&b, "shelf-browse:n:p", &body, Some("W/\"1a-x\\y\"")).unwrap(), "unchanged");
        assert_eq!(db.total_changes() - before, 1);
        assert_eq!(db.spoken_page_etag(&b, "shelf-browse:n:p").unwrap().as_deref(), Some("W/\"1a-x\\y\""));
        let stored = db.spoken_response(&b, "shelf-browse:n:p").unwrap().unwrap();
        assert_eq!(serde_json::from_str::<subsonic::audiobookshelf_browse::Page>(&stored).unwrap(), page);
        db.put_spoken_page(&b, "shelf-browse:n:p", &body, None).unwrap();
        assert_eq!(db.spoken_page_etag(&b, "shelf-browse:n:p").unwrap(), None, "a page without a validator has none");
        assert_eq!(page_etag("{\"etag1\":\"x\",\"entries\":[]}"), None, "another projection's tag is not used");
        assert_eq!(with_etag("{}", Some("t")), "{\"etag2\":\"t\"}");
    }

    #[test]
    fn responses_and_kv() {
        let db = Db::memory(&crate::rt::Blocking::dedicated_thread());
        assert!(db.response(&crate::rt::Blocking::dedicated_thread(), "getArtists").is_none());
        db.put_response(&crate::rt::Blocking::dedicated_thread(), "getArtists", "{}");
        db.put_response(&crate::rt::Blocking::dedicated_thread(), "getArtists", "{\"a\":1}");
        assert_eq!(db.response(&crate::rt::Blocking::dedicated_thread(), "getArtists").unwrap().0, "{\"a\":1}");
        db.set(&crate::rt::Blocking::dedicated_thread(), "k", "v");
        assert_eq!(db.get(&crate::rt::Blocking::dedicated_thread(), "k").as_deref(), Some("v"));
    }

    #[test]
    fn song_cache_prefers_variants_in_order() {
        let db = Db::memory(&crate::rt::Blocking::dedicated_thread());
        db.put_cached_song(&crate::rt::Blocking::dedicated_thread(), "s1", "mp3-320", Path::new("/c/s1.mp3"), 10).unwrap();
        db.put_cached_song(&crate::rt::Blocking::dedicated_thread(), "s1", "raw", Path::new("/c/s1.flac"), 30).unwrap();
        assert_eq!(db.cached_song(&crate::rt::Blocking::dedicated_thread(), "s1", &["raw", "mp3-320"]).unwrap().0, "raw");
        assert_eq!(db.cached_song(&crate::rt::Blocking::dedicated_thread(), "s1", &["mp3-320"]).unwrap().1, PathBuf::from("/c/s1.mp3"));
        assert!(db.cached_song(&crate::rt::Blocking::dedicated_thread(), "s1", &["mp3-128"]).is_none());
        assert_eq!(db.song_cache_size(&crate::rt::Blocking::dedicated_thread()), 40);
        db.forget_cached_song(&crate::rt::Blocking::dedicated_thread(), "s1", "raw");
        assert_eq!(db.song_cache_size(&crate::rt::Blocking::dedicated_thread()), 10);
    }

    #[test]
    fn removing_a_set_keeps_shared_songs() {
        let db = Db::memory(&crate::rt::Blocking::dedicated_thread());
        for id in ["a", "b", "c"] {
            db.put_offline_song(&crate::rt::Blocking::dedicated_thread(), &song(id), Path::new(&format!("/o/{id}")), 1);
        }
        let set = |kind: &str, id: &str, songs: &[&str]| OfflineSet {
            kind: kind.into(),
            id: id.into(),
            name: id.into(),
            artist: String::new(),
            cover_art: None,
            song_ids: songs.iter().map(|s| s.to_string()).collect(),
        };
        db.put_offline_set(&crate::rt::Blocking::dedicated_thread(), &set("album", "al", &["a", "b"]));
        db.put_offline_set(&crate::rt::Blocking::dedicated_thread(), &set("playlist", "pl", &["b", "c"]));
        let gone = db.remove_offline_set(&crate::rt::Blocking::dedicated_thread(), "album", "al");
        assert_eq!(gone, vec![PathBuf::from("/o/a")]);
        assert!(db.offline_song(&crate::rt::Blocking::dedicated_thread(), "b").is_some());
        assert_eq!(db.offline_songs(&crate::rt::Blocking::dedicated_thread(), &["c".into()])[0].title, "Song c");
    }

    #[test]
    fn download_queue_and_pending() {
        let db = Db::memory(&crate::rt::Blocking::dedicated_thread());
        db.queue_download(&crate::rt::Blocking::dedicated_thread(), &song("x"));
        db.queue_download(&crate::rt::Blocking::dedicated_thread(), &song("y"));
        assert_eq!(db.next_download(&crate::rt::Blocking::dedicated_thread()).unwrap().id, "x");
        db.finish_download(&crate::rt::Blocking::dedicated_thread(), "x", Some("HTTP 404"));
        assert_eq!(db.next_download(&crate::rt::Blocking::dedicated_thread()).unwrap().id, "y");
        assert_eq!(db.download_counts(&crate::rt::Blocking::dedicated_thread()), (1, 1));
        db.retry_failed_downloads(&crate::rt::Blocking::dedicated_thread());
        assert_eq!(db.download_counts(&crate::rt::Blocking::dedicated_thread()), (2, 0));

        db.push_pending(&crate::rt::Blocking::dedicated_thread(), &PendingCall::Scrobble { id: "x".into(), time_ms: 5 });
        let p = db.pending(&crate::rt::Blocking::dedicated_thread(), 10);
        assert_eq!(p.len(), 1);
        db.drop_pending(&crate::rt::Blocking::dedicated_thread(), p[0].0);
        assert!(db.pending(&crate::rt::Blocking::dedicated_thread(), 10).is_empty());
    }
}

#[cfg(test)]
mod sqlite_build_tests {
    /// Host tests and the Q2 build one SQLite (crates/vendor/libsqlite3-sys):
    /// every option in q2/toolchain/libsqlite3-flags is in effect here.
    #[test]
    fn sqlite_is_built_with_the_device_flags() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        let mut stmt = conn.prepare("PRAGMA compile_options").unwrap();
        let options: Vec<String> = stmt.query_map([], |row| row.get(0)).unwrap().map(Result::unwrap).collect();
        let flags = include_str!("../../../../q2/toolchain/libsqlite3-flags");
        for flag in flags.split_whitespace() {
            let (kind, option) = flag.split_at(2);
            let option = option.trim_start_matches("SQLITE_");
            match kind {
                "-D" => assert!(options.iter().any(|o| o == option), "{flag} missing from {options:?}"),
                "-U" => assert!(!options.iter().any(|o| o.split('=').next() == Some(option)), "{flag} still in {options:?}"),
                _ => panic!("unexpected flag {flag}"),
            }
        }
        assert!(conn.execute_batch("CREATE VIRTUAL TABLE t USING fts5(x)").is_err());
    }
}
