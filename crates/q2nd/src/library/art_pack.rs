//! Disposable and explicitly synced encoded artwork share this card-local DB.
//! Callers own the namespace digest, media admission and loose-file fallback.
use crate::rt::Blocking;
use rusqlite::config::DbConfig;
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const MAX_PNG: usize = 5 * 1024 * 1024;
const MAX_BATCH_BYTES: usize = 16 * 1024 * 1024;
const MAX_BATCH_ROWS: usize = 64;
const PNG_HEADER: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR";
const APPLICATION_ID: u32 = 0x51324152; // Q2AR
const LEGACY_VERSION: u32 = 2;
const VERSION: u32 = 3;
const COALESCE_ROWS: usize = 16;
const COALESCE_BYTES: usize = 512 * 1024;
const WAL_CHECKPOINT_PAGES: u32 = 64;
const WAL_RETAIN_BYTES: u32 = 512 * 1024;
const SCHEMA: &str = "CREATE TABLE artwork (
    digest TEXT NOT NULL PRIMARY KEY CHECK(length(digest)=32 AND digest NOT GLOB '*[^0-9a-f]*'),
    png BLOB NOT NULL CHECK(length(png) BETWEEN 45 AND 5242880 AND substr(png,1,16)=x'89504e470d0a1a0a0000000d49484452'),
    durable INTEGER NOT NULL CHECK(durable IN (0, 1))
) STRICT";
const PAYLOAD_SCHEMA: &str = "CREATE TABLE artwork_payloads (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    sha256 BLOB NOT NULL CHECK(length(sha256)=32),
    png BLOB NOT NULL CHECK(length(png) BETWEEN 45 AND 5242880 AND substr(png,1,16)=x'89504e470d0a1a0a0000000d49484452')
) STRICT";
const KEY_SCHEMA: &str = "CREATE TABLE artwork_keys (
    digest TEXT NOT NULL PRIMARY KEY CHECK(length(digest)=32 AND digest NOT GLOB '*[^0-9a-f]*'),
    payload_id INTEGER NOT NULL REFERENCES artwork_payloads(id),
    durable INTEGER NOT NULL CHECK(durable IN (0, 1))
) STRICT, WITHOUT ROWID";
const HASH_INDEX: &str = "CREATE INDEX artwork_payload_hash ON artwork_payloads(sha256)";
const KEY_INDEX: &str = "CREATE INDEX artwork_key_payload ON artwork_keys(payload_id)";

static NEXT_GENERATION: AtomicUsize = AtomicUsize::new(1);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ContentId {
    generation: usize,
    row_id: i64,
}

impl ContentId {
    pub(super) fn decode_slot(self, slots: usize) -> usize {
        self.row_id as usize % slots
    }
}

type SharedArtwork = (Option<ContentId>, Option<Vec<u8>>);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CoalesceProgress {
    More,
    Complete,
    LowSpace,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum WriteMode {
    Durable,
    Import,
    Presentation,
}

struct OpenPack {
    database: Connection,
    version: u32,
    generation: usize,
    writable: bool,
    immutable: bool,
    directories_synced: bool,
}

#[derive(Default)]
struct PackState {
    identity: Option<(u64, u64)>,
    connection: Option<OpenPack>,
}

pub(crate) struct ArtPack {
    path: PathBuf,
    state: Mutex<PackState>,
}

impl ArtPack {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self {
            path: root.join("artwork.sqlite3"),
            state: Mutex::default(),
        }
    }

    pub(crate) fn get(&self, blocking: &Blocking, key: &str) -> Result<Option<Vec<u8>>, String> {
        self.get_shared(blocking, key, |_| false)
            .map(|row| row.and_then(|(_, png)| png))
    }

    /// Read the key and payload in one snapshot. A decoded hit avoids fetching
    /// the PNG pages, and an unmigrated key still works through the legacy table.
    pub(crate) fn get_shared(
        &self,
        blocking: &Blocking,
        key: &str,
        mut cached: impl FnMut(ContentId) -> bool,
    ) -> Result<Option<SharedArtwork>, String> {
        validate_key(key)?;
        self.access(blocking, false, |open| {
            let snapshot = open.database.unchecked_transaction().map_err(sql_error)?;
            refresh_version(blocking, &snapshot, &mut open.version)?;
            if open.version == VERSION {
                let row_id = snapshot
                    .prepare_cached("SELECT payload_id FROM artwork_keys WHERE digest=?1")
                    .and_then(|mut statement| {
                        statement.query_row([key], |row| row.get(0)).optional()
                    })
                    .map_err(sql_error)?;
                if let Some(row_id) = row_id {
                    let id = ContentId {
                        generation: open.generation,
                        row_id,
                    };
                    if cached(id) {
                        return Ok(Some((Some(id), None)));
                    }
                    return read_png(
                        blocking,
                        &snapshot,
                        "SELECT png FROM artwork_payloads WHERE id=?1",
                        row_id,
                    )
                    .map(|png| png.map(|png| (Some(id), Some(png))));
                }
            }
            read_png(
                blocking,
                &snapshot,
                "SELECT png FROM artwork WHERE digest=?1",
                key,
            )
            .map(|png| png.map(|png| (None, Some(png))))
        })
        .map(Option::flatten)
    }

    pub(crate) fn contains(&self, blocking: &Blocking, key: &str) -> Result<bool, String> {
        validate_key(key)?;
        self.access(blocking, false, |open| {
            let snapshot = open.database.unchecked_transaction().map_err(sql_error)?;
            refresh_version(blocking, &snapshot, &mut open.version)?;
            let source = if open.version == VERSION {
                "SELECT png FROM artwork_keys JOIN artwork_payloads
                 ON artwork_payloads.id=artwork_keys.payload_id
                 WHERE digest=?1 AND durable=1
                 UNION ALL SELECT png FROM artwork WHERE digest=?1 AND durable=1"
            } else {
                "SELECT png FROM artwork WHERE digest=?1 AND durable=1"
            };
            let sql = format!(
                "SELECT EXISTS(SELECT 1 FROM ({source})
                 WHERE typeof(png)='blob' AND length(png) BETWEEN 45 AND ?2
                    AND substr(png,1,16)=?3)"
            );
            snapshot
                .prepare_cached(&sql)
                .and_then(|mut statement| {
                    statement.query_row(params![key, MAX_PNG as i64, PNG_HEADER], |row| row.get(0))
                })
                .map_err(sql_error)
        })
        .map(|found| found.unwrap_or(false))
    }

    pub(crate) fn put(&self, blocking: &Blocking, rows: &[(&str, &[u8])]) -> Result<(), String> {
        self.write(blocking, rows, WriteMode::Durable).map(|_| ())
    }

    pub(crate) fn import(&self, blocking: &Blocking, rows: &[(&str, &[u8])]) -> Result<(), String> {
        self.write(blocking, rows, WriteMode::Import).map(|_| ())
    }

    pub(crate) fn put_presentation(
        &self,
        blocking: &Blocking,
        digest: &str,
        png: &[u8],
    ) -> Result<Option<ContentId>, String> {
        self.write(blocking, &[(digest, png)], WriteMode::Presentation)
    }

    fn write(
        &self,
        blocking: &Blocking,
        rows: &[(&str, &[u8])],
        mode: WriteMode,
    ) -> Result<Option<ContentId>, String> {
        if rows.is_empty() {
            return Ok(None);
        }
        if rows.len() > MAX_BATCH_ROWS {
            return Err("Artwork database batch has too many rows".into());
        }
        let mut total = 0;
        for &(key, png) in rows {
            validate_key(key)?;
            if !valid_png(png) {
                return Err("Invalid packed artwork PNG".into());
            }
            total += png.len();
            if total > MAX_BATCH_BYTES {
                return Err("Artwork database batch is too large".into());
            }
        }
        self.access(blocking, true, |open| {
            let durable = mode != WriteMode::Presentation;
            let generation = open.generation;
            self.transact(blocking, open, durable, |transaction, version| {
                if version == LEGACY_VERSION {
                    create_shared_schema(blocking, transaction).map_err(sql_error)?;
                }
                let mut row_id = None;
                for &(key, png) in rows {
                    row_id = write_key(blocking, transaction, key, png, mode)?;
                }
                Ok(row_id.map(|row_id| ContentId { generation, row_id }))
            })
        })
        .map(Option::flatten)
    }

    /// Convert a small acknowledged batch. Each commit moves complete keys
    /// (including the durability bit) and releases their original pages.
    pub(crate) fn coalesce_batch(
        &self,
        blocking: &Blocking,
        headroom_bytes: u64,
    ) -> Result<CoalesceProgress, String> {
        use CoalesceProgress::{Complete, LowSpace};

        if read_only_card(blocking, &self.path) {
            return Ok(Complete);
        }
        self.access(blocking, false, |open| {
            if !open.writable {
                return Ok(Complete);
            }
            let has_legacy: bool = open
                .database
                .query_row("SELECT EXISTS(SELECT 1 FROM artwork)", [], |row| row.get(0))
                .map_err(sql_error)?;
            if !has_legacy {
                return Ok(Complete);
            }
            if headroom_bytes < COALESCE_BYTES as u64 {
                return Ok(LowSpace);
            }
            self.transact(blocking, open, true, |transaction, version| {
                coalesce_rows(blocking, transaction, version, headroom_bytes)
            })
        })
        .map(|progress| progress.unwrap_or(Complete))
    }

    fn transact<T>(
        &self,
        blocking: &Blocking,
        open: &mut OpenPack,
        durable: bool,
        operation: impl FnOnce(&Connection, u32) -> Result<T, String>,
    ) -> Result<T, String> {
        let connection = &mut open.database;
        if durable {
            connection
                .pragma_update(None, "synchronous", "FULL")
                .map_err(sql_error)?;
        }
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(sql_error)?;
        if durable && !open.directories_synced {
            // BEGIN IMMEDIATE has opened the WAL. Sync its directory entry
            // before writing a marker another reader could acknowledge.
            sync_pack_directories(blocking, &self.path)?;
        }
        let mut version = open.version;
        refresh_version(blocking, &transaction, &mut version)?;
        let result = operation(&transaction, version)?;
        if version == LEGACY_VERSION {
            version = transaction
                .pragma_query_value(None, "user_version", |row| row.get(0))
                .map_err(sql_error)?;
        }
        transaction.commit().map_err(sql_error)?;
        open.version = version;
        if durable {
            connection
                .pragma_update(None, "synchronous", "NORMAL")
                .map_err(sql_error)?;
            open.directories_synced = true;
        }
        Ok(result)
    }

    fn access<T>(
        &self,
        blocking: &Blocking,
        write: bool,
        operation: impl FnOnce(&mut OpenPack) -> Result<T, String>,
    ) -> Result<Option<T>, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "Artwork database lock failed")?;
        let result = (|| {
            let identity = match file_identity(blocking, &self.path) {
                Ok(identity) => identity,
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    state.close_moved(blocking)?;
                    if !write {
                        return Ok(None);
                    }
                    initialize(blocking, &self.path)?;
                    file_identity(blocking, &self.path).map_err(|error| error.to_string())?
                }
                Err(error) => return Err(error.to_string()),
            };
            if state.identity.is_some_and(|previous| previous != identity) {
                state.close_moved(blocking)?;
                if !no_wal_sidecars(blocking, &self.path) {
                    return Err(
                        "Artwork database was replaced with its old WAL sidecars present".into(),
                    );
                }
            }
            if state.connection.as_ref().is_none_or(|open| {
                write && !open.writable || open.immutable && !read_only_card(blocking, &self.path)
            }) {
                state.connection = Some(open_pack(blocking, &self.path, identity, write)?);
                state.identity = Some(identity);
            }
            operation(state.connection.as_mut().expect("opened artwork database")).map(Some)
        })();
        if result.is_err() {
            state.connection = None;
        }
        result
    }
}

impl PackState {
    fn close_moved(&mut self, _blocking: &Blocking) -> Result<(), String> {
        if let Some(open) = self.connection.as_ref() {
            open.database
                .set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)
                .map_err(|error| error.to_string())?;
        }
        self.connection = None;
        Ok(())
    }
}

fn validate_key(key: &str) -> Result<(), String> {
    if key.len() == 32
        && key
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Ok(())
    } else {
        Err("Invalid packed artwork digest".into())
    }
}

fn sql_error(error: rusqlite::Error) -> String {
    error.to_string()
}

fn valid_png(png: &[u8]) -> bool {
    (45..=MAX_PNG).contains(&png.len()) && png.starts_with(PNG_HEADER)
}

fn read_png(
    _blocking: &Blocking,
    database: &Connection,
    source: &str,
    key: impl rusqlite::ToSql,
) -> Result<Option<Vec<u8>>, String> {
    let sql = format!(
        "SELECT png FROM ({source}) WHERE typeof(png)='blob'
            AND length(png) BETWEEN 45 AND ?2 AND substr(png,1,16)=?3"
    );
    database
        .prepare_cached(&sql)
        .and_then(|mut statement| {
            statement
                .query_row(params![key, MAX_PNG as i64, PNG_HEADER], |row| row.get(0))
                .optional()
        })
        .map_err(sql_error)
}

/// A hash narrows the lookup. Comparing the bytes before reusing a row makes
/// aliases exact even if different inputs have the same hash.
fn intern_payload(
    _blocking: &Blocking,
    database: &Connection,
    png: &[u8],
    hash: &[u8],
) -> rusqlite::Result<i64> {
    let existing = database
        .prepare_cached("SELECT id FROM artwork_payloads WHERE sha256=?1 AND png=?2 LIMIT 1")?
        .query_row(params![hash, png], |row| row.get(0))
        .optional()?;
    if let Some(id) = existing {
        return Ok(id);
    }
    database
        .prepare_cached("INSERT INTO artwork_payloads(sha256,png) VALUES (?1,?2) RETURNING id")?
        .query_row(params![hash, png], |row| row.get(0))
}

fn upsert_key(
    _blocking: &Blocking,
    database: &Connection,
    key: &str,
    row_id: i64,
    durable: bool,
) -> rusqlite::Result<()> {
    database
        .prepare_cached(
            "INSERT INTO artwork_keys VALUES (?1,?2,?3)
             ON CONFLICT(digest) DO UPDATE SET payload_id=excluded.payload_id, durable=excluded.durable
             WHERE payload_id!=excluded.payload_id OR durable!=excluded.durable",
        )?
        .execute(params![key, row_id, durable])?;
    Ok(())
}

fn write_key(
    blocking: &Blocking,
    database: &Connection,
    key: &str,
    png: &[u8],
    mode: WriteMode,
) -> Result<Option<i64>, String> {
    let current: Option<(Option<i64>, bool, bool)> = database
        .prepare_cached(
            "SELECT payload_id, durable, EXISTS(
                 SELECT 1 FROM artwork_payloads WHERE id=payload_id AND png=?2)
             FROM artwork_keys WHERE digest=?1
             UNION ALL SELECT NULL, durable, png=?2 FROM artwork WHERE digest=?1 LIMIT 1",
        )
        .and_then(|mut statement| {
            statement
                .query_row(params![key, png], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .optional()
        })
        .map_err(sql_error)?;
    if let Some((id, durable, identical)) = current {
        match mode {
            WriteMode::Presentation if durable => return Ok(id.filter(|_| identical)),
            WriteMode::Import if durable || !identical => return Ok(None),
            _ => {}
        }
    }

    let row_id = match current {
        Some((Some(row_id), _, true)) => row_id,
        _ => {
            let hash = ring::digest::digest(&ring::digest::SHA256, png);
            intern_payload(blocking, database, png, hash.as_ref()).map_err(sql_error)?
        }
    };
    upsert_key(
        blocking,
        database,
        key,
        row_id,
        mode != WriteMode::Presentation,
    )
    .map_err(sql_error)?;
    match current {
        Some((None, _, _)) => {
            database
                .prepare_cached("DELETE FROM artwork WHERE digest=?1")
                .and_then(|mut statement| statement.execute([key]))
                .map_err(sql_error)?;
        }
        Some((Some(previous), _, _)) if previous != row_id => {
            database
                .prepare_cached(
                    "DELETE FROM artwork_payloads WHERE id=?1
                 AND NOT EXISTS(SELECT 1 FROM artwork_keys WHERE payload_id=?1)",
                )
                .and_then(|mut statement| statement.execute([previous]))
                .map_err(sql_error)?;
        }
        _ => {}
    }
    Ok(Some(row_id))
}

fn coalesce_rows(
    blocking: &Blocking,
    database: &Connection,
    version: u32,
    headroom_bytes: u64,
) -> Result<CoalesceProgress, String> {
    use CoalesceProgress::{Complete, LowSpace, More};

    // Reserve space for SQLite metadata, WAL frames and their eventual main
    // file writes. Existing free pages may reduce the actual growth.
    let budget = (headroom_bytes - COALESCE_BYTES as u64) / 4;
    let budget = usize::try_from(budget).unwrap_or(usize::MAX);
    let mut statement = database
        .prepare_cached("SELECT digest, length(png), durable FROM artwork ORDER BY digest LIMIT ?1")
        .map_err(sql_error)?;
    let mut rows = statement.query([COALESCE_ROWS as i64 + 1]).map_err(sql_error)?;
    let mut batch = Vec::with_capacity(COALESCE_ROWS);
    let mut bytes = 0;
    let mut more = false;
    while let Some(row) = rows.next().map_err(sql_error)? {
        let length = row.get::<_, u32>(1).map_err(sql_error)? as usize;
        if length > MAX_PNG {
            return Err("Oversized PNG in legacy artwork database".into());
        }
        if length > budget.saturating_sub(bytes) {
            if batch.is_empty() {
                return Ok(LowSpace);
            }
            more = true;
            break;
        }
        if !batch.is_empty()
            && (batch.len() == COALESCE_ROWS || length > COALESCE_BYTES.saturating_sub(bytes))
        {
            more = true;
            break;
        }
        batch.push((
            row.get::<_, String>(0).map_err(sql_error)?,
            row.get::<_, bool>(2).map_err(sql_error)?,
        ));
        bytes += length;
    }
    drop(rows);
    if batch.is_empty() {
        return Ok(Complete);
    }
    if version == LEGACY_VERSION {
        create_shared_schema(blocking, database).map_err(sql_error)?;
    }
    for (key, durable) in batch {
        let png = read_png(
            blocking,
            database,
            "SELECT png FROM artwork WHERE digest=?1",
            &key,
        )?
        .ok_or_else(|| "Invalid PNG in legacy artwork database".to_owned())?;
        let hash = ring::digest::digest(&ring::digest::SHA256, &png);
        let row_id = intern_payload(blocking, database, &png, hash.as_ref()).map_err(sql_error)?;
        upsert_key(blocking, database, &key, row_id, durable).map_err(sql_error)?;
        database
            .prepare_cached("DELETE FROM artwork WHERE digest=?1")
            .and_then(|mut statement| statement.execute([key]))
            .map_err(sql_error)?;
    }
    Ok(if more { More } else { Complete })
}

fn create_shared_schema(_blocking: &Blocking, database: &Connection) -> rusqlite::Result<()> {
    for sql in [PAYLOAD_SCHEMA, KEY_SCHEMA, HASH_INDEX, KEY_INDEX] {
        database.execute_batch(sql)?;
    }
    database.pragma_update(None, "user_version", VERSION)
}

fn refresh_version(
    blocking: &Blocking,
    database: &Connection,
    version: &mut u32,
) -> Result<(), String> {
    if *version == LEGACY_VERSION {
        let actual: u32 = database
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .map_err(sql_error)?;
        if actual != *version {
            *version = check_schema(blocking, database)?;
        }
    }
    Ok(())
}

fn file_identity(_blocking: &Blocking, path: &Path) -> std::io::Result<(u64, u64)> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() {
        return Err(std::io::Error::other(
            "Artwork database path is not a regular file",
        ));
    }
    Ok((metadata.dev(), metadata.ino()))
}

fn connection(_blocking: &Blocking, path: &Path, flags: OpenFlags) -> rusqlite::Result<Connection> {
    Connection::open_with_flags(path, flags | OpenFlags::SQLITE_OPEN_NOFOLLOW)
}

fn configure_connection(_blocking: &Blocking, database: &Connection) -> rusqlite::Result<()> {
    database.busy_timeout(Duration::from_millis(250))?;
    database.pragma_update(None, "trusted_schema", false)?;
    database.pragma_update(None, "cache_size", -512)?;
    database.set_prepared_statement_cache_capacity(12);
    Ok(())
}

fn check_schema(_blocking: &Blocking, database: &Connection) -> Result<u32, String> {
    let check = || -> rusqlite::Result<Option<u32>> {
        let application_id =
            database.pragma_query_value(None, "application_id", |row| row.get::<_, u32>(0))?;
        let version =
            database.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))?;
        if application_id != APPLICATION_ID || ![LEGACY_VERSION, VERSION].contains(&version) {
            return Ok(None);
        }
        let mut expected = vec![
            ("artwork", "table", "artwork", Some(SCHEMA)),
            ("sqlite_autoindex_artwork_1", "index", "artwork", None),
        ];
        if version == VERSION {
            expected.extend([
                (
                    "artwork_key_payload",
                    "index",
                    "artwork_keys",
                    Some(KEY_INDEX),
                ),
                ("artwork_keys", "table", "artwork_keys", Some(KEY_SCHEMA)),
                (
                    "artwork_payload_hash",
                    "index",
                    "artwork_payloads",
                    Some(HASH_INDEX),
                ),
                (
                    "artwork_payloads",
                    "table",
                    "artwork_payloads",
                    Some(PAYLOAD_SCHEMA),
                ),
                (
                    "sqlite_sequence",
                    "table",
                    "sqlite_sequence",
                    Some("CREATE TABLE sqlite_sequence(name,seq)"),
                ),
            ]);
        }
        expected.sort_unstable();
        let mut statement =
            database.prepare("SELECT name,type,tbl_name,sql FROM sqlite_schema ORDER BY name")?;
        let mut rows = statement.query([])?;
        for (name, kind, table, sql) in expected {
            let Some(row) = rows.next()? else {
                return Ok(None);
            };
            if row.get_ref(0)?.as_str()? != name
                || row.get_ref(1)?.as_str()? != kind
                || row.get_ref(2)?.as_str()? != table
                || row.get::<_, Option<String>>(3)?.as_deref() != sql
            {
                return Ok(None);
            }
        }
        Ok(rows.next()?.is_none().then_some(version))
    };
    check()
        .map_err(sql_error)?
        .ok_or_else(|| "Unsupported artwork database format".into())
}

fn open_pack(
    blocking: &Blocking,
    path: &Path,
    identity: (u64, u64),
    write: bool,
) -> Result<OpenPack, String> {
    let read_only = !write && read_only_card(blocking, path);
    let immutable = read_only && no_wal_sidecars(blocking, path);
    // SQLite can checkpoint stale WAL pages on close even after reporting
    // NOTADB. Admit the original main file independently through an immutable
    // connection, which neither opens sidecars nor holds a writable handle.
    let main = immutable_connection(blocking, path).map_err(|error| error.to_string())?;
    let main_version = check_schema(blocking, &main)?;
    let (database, version) = if immutable {
        (main, main_version)
    } else {
        drop(main);
        let database = if read_only {
            connection(blocking, path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        } else {
            connection(blocking, path, OpenFlags::SQLITE_OPEN_READ_WRITE).or_else(|error| {
                if write {
                    Err(error)
                } else {
                    connection(blocking, path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                }
            })
        }
        .map_err(|error| error.to_string())?;
        database
            .set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, true)
            .map_err(|error| error.to_string())?;
        let version = {
            let snapshot = database.unchecked_transaction().map_err(sql_error)?;
            check_schema(blocking, &snapshot)?
        };
        if version < main_version {
            return Err("Artwork WAL has an older schema than the main file".into());
        }
        let journal_mode: String = database
            .pragma_query_value(None, "journal_mode", |row| row.get(0))
            .map_err(|error| error.to_string())?;
        if journal_mode != "wal" {
            return Err("Unsupported artwork database journal mode".into());
        }
        (database, version)
    };
    if file_identity(blocking, path).map_err(|error| error.to_string())? != identity {
        return Err("Artwork database changed during open".into());
    }
    configure_connection(blocking, &database).map_err(|error| error.to_string())?;
    let writable = !database
        .is_readonly(rusqlite::MAIN_DB)
        .map_err(|error| error.to_string())?;
    if write && !writable {
        return Err("Artwork database is read-only".into());
    }
    if writable {
        configure_writer(blocking, &database).map_err(|error| error.to_string())?;
        database
            .set_db_config(DbConfig::SQLITE_DBCONFIG_NO_CKPT_ON_CLOSE, false)
            .map_err(|error| error.to_string())?;
    }
    Ok(OpenPack {
        database,
        version,
        generation: NEXT_GENERATION.fetch_add(1, Ordering::Relaxed),
        writable,
        immutable,
        directories_synced: false,
    })
}

fn configure_writer(_blocking: &Blocking, database: &Connection) -> rusqlite::Result<()> {
    database.pragma_update(None, "foreign_keys", true)?;
    database.pragma_update(None, "synchronous", "NORMAL")?;
    database.pragma_update(None, "wal_autocheckpoint", WAL_CHECKPOINT_PAGES)?;
    database.pragma_update(None, "journal_size_limit", WAL_RETAIN_BYTES)
}

fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}

fn read_only_card(blocking: &Blocking, path: &Path) -> bool {
    crate::platform::system::filesystem(blocking, path)
        .is_ok_and(|filesystem| filesystem.f_flag & libc::ST_RDONLY != 0)
}

fn no_wal_sidecars(_blocking: &Blocking, path: &Path) -> bool {
    ["-wal", "-shm"].iter().all(|suffix| {
        fs::symlink_metadata(sidecar(path, suffix))
            .is_err_and(|error| error.kind() == ErrorKind::NotFound)
    })
}

fn immutable_connection(blocking: &Blocking, path: &Path) -> rusqlite::Result<Connection> {
    use std::fmt::Write;
    use std::os::unix::ffi::OsStrExt;

    // Percent-encode even URI metacharacters present in a real card path.
    let mut uri = String::from("file:");
    for byte in path.as_os_str().as_bytes() {
        if *byte == b'/' || byte.is_ascii_alphanumeric() {
            uri.push(*byte as char);
        } else {
            write!(uri, "%{byte:02x}").expect("writing a String");
        }
    }
    uri.push_str("?immutable=1");
    connection(
        blocking,
        Path::new(&uri),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
}

fn sync_pack_directories(blocking: &Blocking, path: &Path) -> Result<(), String> {
    let root = path.parent().expect("artwork database parent");
    super::offline_art::sync_parents(blocking, root).map_err(|error| error.to_string())
}

fn initialize(blocking: &Blocking, path: &Path) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;

    if !no_wal_sidecars(blocking, path) {
        return Err("Artwork database is missing with WAL sidecars present".into());
    }

    let root = path.parent().expect("artwork database parent");
    fs::create_dir_all(root).map_err(|error| error.to_string())?;
    // A process interrupted during schema creation leaves no partial database
    // at the real path. An existing pack always wins concurrent initialization.
    let stage = root.join(format!(".artwork-{:016x}.sqlite3", fastrand::u64(..)));
    fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&stage)
        .map_err(|error| error.to_string())?;
    let result = (|| {
        let mut database = connection(blocking, &stage, OpenFlags::SQLITE_OPEN_READ_WRITE)
            .map_err(|error| error.to_string())?;
        configure_connection(blocking, &database).map_err(|error| error.to_string())?;
        database
            .pragma_update(None, "synchronous", "EXTRA")
            .map_err(|error| error.to_string())?;
        let transaction = database.transaction().map_err(|error| error.to_string())?;
        transaction
            .execute_batch(SCHEMA)
            .map_err(|error| error.to_string())?;
        transaction
            .pragma_update(None, "application_id", APPLICATION_ID)
            .map_err(|error| error.to_string())?;
        create_shared_schema(blocking, &transaction).map_err(sql_error)?;
        transaction.commit().map_err(|error| error.to_string())?;
        let mode: String = database
            .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
            .map_err(|error| error.to_string())?;
        if mode != "wal" {
            return Err("Could not initialize the artwork WAL".into());
        }
        database.close().map_err(|(_, error)| error.to_string())?;

        // An existing pack always wins, including an unrelated or damaged file.
        match crate::platform::system::rename_noreplace(blocking, &stage, path) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error.to_string()),
        }
        sync_pack_directories(blocking, path)
    })();
    let _ = fs::remove_file(&stage);
    for suffix in ["-journal", "-wal", "-shm"] {
        let _ = fs::remove_file(sidecar(&stage, suffix));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const FIRST: &str = "11111111111111111111111111111111";
    const SECOND: &str = "22222222222222222222222222222222";

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            Self(super::super::generated_test_directory("art-pack"))
        }

        fn pack(&self) -> ArtPack {
            ArtPack::new(self.0.join("cache/art"))
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn png(tone: u8) -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        image::RgbImage::from_pixel(3, 3, image::Rgb([tone, 2, 3]))
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }

    fn legacy_fixture(pack: &ArtPack, rows: &[(&str, &[u8], bool)]) {
        fs::create_dir_all(pack.path.parent().unwrap()).unwrap();
        let database = Connection::open(&pack.path).unwrap();
        database.execute_batch(SCHEMA).unwrap();
        database
            .pragma_update(None, "application_id", APPLICATION_ID)
            .unwrap();
        database
            .pragma_update(None, "user_version", LEGACY_VERSION)
            .unwrap();
        for &(key, png, durable) in rows {
            database
                .execute(
                    "INSERT INTO artwork VALUES (?1, ?2, ?3)",
                    params![key, png, durable],
                )
                .unwrap();
        }
        let mode: String = database
            .pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }

    #[test]
    fn payload_aliases_keep_separate_durability_and_skip_png_reads() {
        let blocking = Blocking::dedicated_thread();
        let fixture = Fixture::new();
        let pack = fixture.pack();
        let blank = png(1);
        let replacement = png(2);
        let id = pack
            .put_presentation(&blocking, FIRST, &blank)
            .unwrap()
            .unwrap();
        pack.import(&blocking, &[(SECOND, &blank)]).unwrap();
        assert!(!pack.contains(&blocking, FIRST).unwrap());
        assert!(pack.contains(&blocking, SECOND).unwrap());
        assert_eq!(
            pack.get_shared(&blocking, SECOND, |candidate| candidate == id)
                .unwrap(),
            Some((Some(id), None)),
        );
        assert_ne!(
            pack.put_presentation(&blocking, FIRST, &replacement)
                .unwrap(),
            Some(id)
        );
        assert_eq!(
            pack.put_presentation(&blocking, SECOND, &replacement)
                .unwrap(),
            None
        );
        assert_eq!(pack.get(&blocking, SECOND).unwrap(), Some(blank));
        pack.put(&blocking, &[(SECOND, &replacement)]).unwrap();
        let database = Connection::open(&pack.path).unwrap();
        let counts: (i64, i64) = database
            .query_row(
                "SELECT (SELECT count(*) FROM artwork_keys),
                    (SELECT count(*) FROM artwork_payloads)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(counts, (2, 1));
    }

    #[test]
    fn hash_collision_retains_both_exact_payloads() {
        let blocking = Blocking::dedicated_thread();
        let database = Connection::open_in_memory().unwrap();
        create_shared_schema(&blocking, &database).unwrap();
        let first = png(1);
        let second = png(2);
        let hash = [0; 32];
        let first_id = intern_payload(&blocking, &database, &first, &hash).unwrap();
        let second_id = intern_payload(&blocking, &database, &second, &hash).unwrap();
        assert_ne!(first_id, second_id);
        assert_eq!(
            intern_payload(&blocking, &database, &first, &hash).unwrap(),
            first_id
        );
        assert_eq!(
            intern_payload(&blocking, &database, &second, &hash).unwrap(),
            second_id
        );
    }

    #[test]
    fn legacy_reads_resume_during_bounded_coalescing_and_wal_recovery() {
        use CoalesceProgress::{Complete, LowSpace, More};

        let blocking = Blocking::dedicated_thread();
        let fixture = Fixture::new();
        let pack = fixture.pack();
        let common = png(1);
        let keys: Vec<_> = (0..COALESCE_ROWS + 1)
            .map(|i| format!("{i:032x}"))
            .collect();
        let rows: Vec<_> = keys
            .iter()
            .enumerate()
            .map(|(i, key)| (key.as_str(), common.as_slice(), i != 0))
            .collect();
        legacy_fixture(&pack, &rows);

        let immutable = immutable_connection(&blocking, &pack.path).unwrap();
        assert_eq!(check_schema(&blocking, &immutable).unwrap(), LEGACY_VERSION);
        assert_eq!(
            read_png(
                &blocking,
                &immutable,
                "SELECT png FROM artwork WHERE digest=?1",
                &keys[0]
            )
            .unwrap(),
            Some(common.clone())
        );
        assert!(no_wal_sidecars(&blocking, &pack.path));
        drop(immutable);

        let reader = fixture.pack();
        assert_eq!(
            reader
                .get_shared(&blocking, &keys[0], |_| panic!(
                    "Legacy key has no shared ID"
                ))
                .unwrap(),
            Some((None, Some(common.clone())))
        );
        assert_eq!(
            pack.coalesce_batch(&blocking, COALESCE_BYTES as u64)
                .unwrap(),
            LowSpace
        );
        assert_eq!(pack.coalesce_batch(&blocking, u64::MAX).unwrap(), More);
        assert!(
            reader
                .get_shared(&blocking, &keys[0], |_| true)
                .unwrap()
                .unwrap()
                .0
                .is_some()
        );
        assert!(!reader.contains(&blocking, &keys[0]).unwrap());
        assert!(reader.contains(&blocking, &keys[COALESCE_ROWS]).unwrap());

        let crash = Fixture::new();
        let reopened = crash.pack();
        fs::create_dir_all(reopened.path.parent().unwrap()).unwrap();
        fs::copy(&pack.path, &reopened.path).unwrap();
        let bytes = fs::copy(sidecar(&pack.path, "-wal"), sidecar(&reopened.path, "-wal")).unwrap();
        assert!(bytes > 32);
        assert_eq!(
            check_schema(
                &blocking,
                &immutable_connection(&blocking, &reopened.path).unwrap()
            )
            .unwrap(),
            LEGACY_VERSION
        );
        assert_eq!(
            reopened.get(&blocking, &keys[0]).unwrap(),
            Some(common.clone())
        );
        assert_eq!(
            reopened.coalesce_batch(&blocking, u64::MAX).unwrap(),
            Complete
        );
        let id = reopened
            .get_shared(&blocking, &keys[0], |_| false)
            .unwrap()
            .unwrap()
            .0
            .unwrap();
        assert_eq!(
            reopened
                .get_shared(&blocking, &keys[COALESCE_ROWS], |candidate| candidate == id)
                .unwrap(),
            Some((Some(id), None)),
        );
        assert!(reopened.contains(&blocking, &keys[COALESCE_ROWS]).unwrap());
    }

    #[test]
    fn reads_do_not_create_and_import_does_not_replace() {
        let blocking = Blocking::dedicated_thread();
        let fixture = Fixture::new();
        let pack = fixture.pack();
        let current = png(1);
        let older = png(2);
        assert_eq!(pack.get(&blocking, FIRST).unwrap(), None);
        assert!(!pack.contains(&blocking, FIRST).unwrap());
        pack.import(&blocking, &[]).unwrap();
        assert!(!pack.path.exists());

        pack.put(&blocking, &[(FIRST, &current)]).unwrap();
        pack.import(&blocking, &[(FIRST, &older), (SECOND, &older)])
            .unwrap();
        let reopened = fixture.pack();
        assert!(reopened.contains(&blocking, FIRST).unwrap());
        assert_eq!(reopened.get(&blocking, FIRST).unwrap(), Some(current));
        assert_eq!(
            reopened.get(&blocking, SECOND).unwrap(),
            Some(older.clone())
        );
        pack.put(&blocking, &[(FIRST, &older)]).unwrap();
        assert_eq!(reopened.get(&blocking, FIRST).unwrap(), Some(older));
    }

    #[test]
    fn corrupt_rows_are_misses_and_invalid_batches_leave_rows_intact() {
        let blocking = Blocking::dedicated_thread();
        let fixture = Fixture::new();
        let pack = fixture.pack();
        let current = png(1);
        let replacement = png(2);

        assert!(pack.put(&blocking, &[("not-a-digest", &current)]).is_err());
        assert!(!pack.path.exists());
        pack.put(&blocking, &[(FIRST, &current)]).unwrap();
        assert!(
            pack.put(&blocking, &[(FIRST, &replacement), (SECOND, b"bad")])
                .is_err()
        );
        assert_eq!(pack.get(&blocking, FIRST).unwrap().as_ref(), Some(&current));
        assert!(!pack.contains(&blocking, SECOND).unwrap());

        let database = Connection::open(&pack.path).unwrap();
        database
            .pragma_update(None, "ignore_check_constraints", true)
            .unwrap();
        database
            .execute(
                "INSERT INTO artwork VALUES (?1, zeroblob(?2), 1)",
                params![SECOND, MAX_PNG as i64 + 1],
            )
            .unwrap();
        assert_eq!(pack.get(&blocking, SECOND).unwrap(), None);
        assert!(!pack.contains(&blocking, SECOND).unwrap());
        database
            .execute(
                "UPDATE artwork SET png = x'6162' WHERE digest = ?1",
                [SECOND],
            )
            .unwrap();
        assert_eq!(pack.get(&blocking, SECOND).unwrap(), None);
        pack.put(&blocking, &[(SECOND, &replacement)]).unwrap();
        assert_eq!(pack.get(&blocking, SECOND).unwrap(), Some(replacement));
    }

    #[test]
    fn missing_and_replaced_databases_drop_the_stale_handle() {
        let blocking = Blocking::dedicated_thread();
        let fixture = Fixture::new();
        let pack = fixture.pack();
        let current = png(1);
        pack.put(&blocking, &[(FIRST, &current)]).unwrap();
        fs::rename(&pack.path, pack.path.with_extension("old")).unwrap();
        assert_eq!(pack.get(&blocking, FIRST).unwrap(), None);
        assert!(!pack.path.exists());

        let wal = sidecar(&pack.path, "-wal");
        let wal_bytes = fs::read(&wal).unwrap();
        assert!(wal_bytes.len() > 32);
        assert!(pack.put(&blocking, &[(SECOND, &current)]).is_err());
        assert!(!pack.path.exists());

        let damaged = b"generated invalid database";
        fs::write(&pack.path, damaged).unwrap();
        let reopened = fixture.pack();
        for reader in [&pack, &reopened] {
            assert!(reader.get(&blocking, FIRST).is_err());
            assert!(reader.put(&blocking, &[(FIRST, &current)]).is_err());
            assert_eq!(fs::read(&pack.path).unwrap(), damaged);
            assert_eq!(fs::read(&wal).unwrap(), wal_bytes);
        }
        fs::rename(&pack.path, pack.path.with_extension("damaged")).unwrap();
        for suffix in ["-wal", "-shm"] {
            fs::rename(
                sidecar(&pack.path, suffix),
                sidecar(&pack.path.with_extension("old"), suffix),
            )
            .unwrap();
        }
        pack.put(&blocking, &[(SECOND, &current)]).unwrap();
        assert_eq!(pack.get(&blocking, FIRST).unwrap(), None);
        assert_eq!(pack.get(&blocking, SECOND).unwrap(), Some(current));
    }

    #[test]
    fn write_reopens_a_read_only_connection() {
        let blocking = Blocking::dedicated_thread();
        let fixture = Fixture::new();
        let pack = fixture.pack();
        let current = png(1);
        pack.put(&blocking, &[(FIRST, &current)]).unwrap();
        let database = connection(&blocking, &pack.path, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        pack.state.lock().unwrap().connection = Some(OpenPack {
            database,
            version: VERSION,
            generation: NEXT_GENERATION.fetch_add(1, Ordering::Relaxed),
            writable: false,
            immutable: false,
            directories_synced: false,
        });
        assert!(pack.contains(&blocking, FIRST).unwrap());
        pack.put(&blocking, &[(SECOND, &current)]).unwrap();
        assert_eq!(pack.get(&blocking, SECOND).unwrap(), Some(current));
    }

    #[test]
    fn presentation_rows_become_durable_only_with_identical_import_or_explicit_sync() {
        let blocking = Blocking::dedicated_thread();
        let fixture = Fixture::new();
        let pack = fixture.pack();
        let first = png(1);
        let second = png(2);

        pack.put_presentation(&blocking, FIRST, &first).unwrap();
        assert_eq!(pack.get(&blocking, FIRST).unwrap().as_ref(), Some(&first));
        assert!(!pack.contains(&blocking, FIRST).unwrap());
        pack.import(&blocking, &[(FIRST, &second)]).unwrap();
        assert_eq!(pack.get(&blocking, FIRST).unwrap().as_ref(), Some(&first));
        assert!(!pack.contains(&blocking, FIRST).unwrap());
        pack.import(&blocking, &[(FIRST, &first)]).unwrap();
        assert!(pack.contains(&blocking, FIRST).unwrap());

        pack.put_presentation(&blocking, FIRST, &second).unwrap();
        assert_eq!(pack.get(&blocking, FIRST).unwrap().as_ref(), Some(&first));
        pack.put_presentation(&blocking, SECOND, &first).unwrap();
        pack.put(&blocking, &[(FIRST, &second), (SECOND, &second)])
            .unwrap();
        assert!(pack.contains(&blocking, FIRST).unwrap());
        assert!(pack.contains(&blocking, SECOND).unwrap());
        assert_eq!(pack.get(&blocking, FIRST).unwrap(), Some(second));

        let state = pack.state.lock().unwrap();
        let database = &state.connection.as_ref().unwrap().database;
        let synchronous: u32 = database
            .pragma_query_value(None, "synchronous", |row| row.get(0))
            .unwrap();
        let checkpoint: u32 = database
            .pragma_query_value(None, "wal_autocheckpoint", |row| row.get(0))
            .unwrap();
        assert_eq!(synchronous, 1); // NORMAL
        assert_eq!(checkpoint, WAL_CHECKPOINT_PAGES);
    }

    #[test]
    fn reopen_recovers_committed_wal_frames_without_a_shared_memory_file() {
        let blocking = Blocking::dedicated_thread();
        let fixture = Fixture::new();
        let pack = fixture.pack();
        let current = png(1);
        pack.put(&blocking, &[(FIRST, &current), (SECOND, &current)])
            .unwrap();

        // Copy the database and WAL while the writer is still open. SQLite
        // must rebuild the transient SHM and replay the acknowledged batch.
        let crash = Fixture::new();
        let reopened = crash.pack();
        fs::create_dir_all(reopened.path.parent().unwrap()).unwrap();
        fs::copy(&pack.path, &reopened.path).unwrap();
        let bytes = fs::copy(sidecar(&pack.path, "-wal"), sidecar(&reopened.path, "-wal")).unwrap();
        assert!(bytes > 32);
        assert!(reopened.contains(&blocking, FIRST).unwrap());
        assert!(reopened.contains(&blocking, SECOND).unwrap());
        assert_eq!(reopened.get(&blocking, FIRST).unwrap(), Some(current));
    }

    #[test]
    fn clean_wal_database_supports_immutable_reads_without_sidecars() {
        let blocking = Blocking::dedicated_thread();
        let fixture = Fixture::new();
        let root = fixture.0.join("cache/art #? %");
        let pack = ArtPack::new(root.clone());
        let current = png(1);
        pack.put(&blocking, &[(FIRST, &current)]).unwrap();
        drop(pack);

        let path = ArtPack::new(root).path;
        assert!(no_wal_sidecars(&blocking, &path));
        let database = immutable_connection(&blocking, &path).unwrap();
        let durable: u32 = database
            .query_row(
                "SELECT durable FROM artwork_keys WHERE digest=?1",
                [FIRST],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(durable, 1);
        assert!(database.is_readonly(rusqlite::MAIN_DB).unwrap());
        assert!(no_wal_sidecars(&blocking, &path));
    }
}
