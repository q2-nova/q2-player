//! Bounded catalog storage. Build privately in batches, then publish atomically.
//! Reader leases preserve older versions across refresh; no UI rows live here.
use super::{Db, OptionalExtension, params};
use crate::rt::Blocking;
use rusqlite::{Error, Result};
use std::{collections::HashMap, sync::{Arc, Weak}};
use subsonic::Album;

pub(super) type Leases = HashMap<i64, Weak<()>>;
pub(super) type BuildOwner = (i64, Weak<()>);
pub(crate) const BATCH_LIMIT: usize = 500;

#[derive(Clone)]
pub(crate) struct Build { version: i64, owner: Arc<()> }

/// Immutable catalog identity. Clones pin the same completed version; a handle
/// from a different database instance cannot authorize a read or publication.
#[derive(Clone)]
pub(crate) struct Catalog { version: i64, pub len: u32, pub groups: u32, owner: Arc<()> }

#[derive(Debug, PartialEq)]
pub(crate) struct Group { pub position: u32, pub label: String }

impl Db {
    /// Reclaim abandoned builds and unpinned old versions before starting one
    /// replacement. The current completed version is never removed here.
    pub(crate) fn catalog_begin(&mut self, _blocking: &Blocking) -> Result<Build> {
        let tx = self.conn.unchecked_transaction()?;
        let current: Option<i64> = tx.query_row("SELECT version FROM album_catalog_head WHERE slot=0", [], |r| r.get(0)).optional()?;
        let versions = {
            let mut query = tx.prepare("SELECT id FROM album_catalog_versions")?;
            query.query_map([], |r| r.get::<_, i64>(0))?.collect::<Result<Vec<_>>>()?
        };
        for version in versions {
            if current == Some(version) || self.catalog_leases.get(&version).and_then(Weak::upgrade).is_some() { continue; }
            tx.execute("DELETE FROM album_catalog_entries WHERE version=?1", [version])?;
            tx.execute("DELETE FROM album_catalog_groups WHERE version=?1", [version])?;
            tx.execute("DELETE FROM album_catalog_versions WHERE id=?1", [version])?;
            tx.execute("DELETE FROM album_catalog_markers WHERE version=?1", [version])?;
        }
        tx.execute("INSERT INTO album_catalog_versions DEFAULT VALUES", [])?;
        let version = tx.last_insert_rowid();
        tx.commit()?;
        self.catalog_leases.retain(|_, lease| lease.strong_count() != 0);
        let owner = Arc::new(());
        self.catalog_build = Some((version, Arc::downgrade(&owner)));
        Ok(Build { version, owner })
    }

    fn catalog_writer_current(&self, build: &Build) -> Result<()> {
        if self.catalog_build.as_ref().is_some_and(|(version, owner)| *version == build.version
            && owner.upgrade().is_some_and(|owner| Arc::ptr_eq(&owner, &build.owner))) { Ok(()) }
        else { Err(Error::InvalidQuery) }
    }

    /// `offset` counts server rows, including overlap. Stored positions count
    /// distinct IDs. Labels come from the caller's actual display normalization,
    /// in server order, and may recur in nonadjacent groups.
    pub(crate) fn catalog_append(&mut self, _blocking: &Blocking, build: &Build,
        offset: u32, albums: &[(Album, String)]) -> Result<u32> {
        self.catalog_writer_current(build)?;
        if albums.len() > BATCH_LIMIT { return Err(Error::InvalidQuery); }
        let received = offset.checked_add(albums.len() as u32).ok_or(Error::InvalidQuery)?;
        let tx = self.conn.unchecked_transaction()?;
        let (mut total, expected, mut groups): (u32, u32, u32) = tx.query_row(
            "SELECT total,received,groups FROM album_catalog_versions WHERE id=?1 AND complete=0 AND exhausted=0",
            [build.version], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        if offset != expected { return Err(Error::InvalidQuery); }
        let before = total;
        let mut last: Option<String> = tx.query_row(
            "SELECT label FROM album_catalog_groups WHERE version=?1 ORDER BY ordinal DESC LIMIT 1",
            [build.version], |r| r.get(0)).optional()?;
        {
            let mut insert = tx.prepare("INSERT OR IGNORE INTO album_catalog_entries VALUES(?1,?2,?3,?4)")?;
            let mut group = tx.prepare("INSERT INTO album_catalog_groups VALUES(?1,?2,?3,?4)")?;
            for (album, label) in albums {
                if album.id.is_empty() { return Err(Error::InvalidQuery); }
                let summary = serde_json::to_string(album).map_err(|_| Error::InvalidQuery)?;
                if insert.execute(params![build.version, total, album.id, summary])? == 0 { continue; }
                if last.as_ref() != Some(label) {
                    group.execute(params![build.version, groups, total, label])?;
                    groups = groups.checked_add(1).ok_or(Error::InvalidQuery)?;
                    last = Some(label.clone());
                }
                total = total.checked_add(1).ok_or(Error::InvalidQuery)?;
            }
        }
        // An endlessly repeated nonempty page is a failure, not exhaustion.
        if !albums.is_empty() && before == total { return Err(Error::InvalidQuery); }
        tx.execute("UPDATE album_catalog_versions SET total=?2,received=?3,groups=?4,exhausted=?5 WHERE id=?1",
            params![build.version, total, received, groups, albums.len() < BATCH_LIMIT])?;
        tx.commit()?;
        Ok(received)
    }

    /// Publication is explicit: a partial or failed sequence never changes the
    /// head. A final short batch (possibly empty) must establish exhaustion.
    pub(crate) fn catalog_publish(&mut self, blocking: &Blocking, build: &Build) -> Result<Catalog> {
        self.catalog_publish_marked(blocking, build, None)
    }

    /// [`Db::catalog_publish`], recording the server's scan marker that held
    /// from before the first page to after the last (`None`: unknown, so the
    /// next browse builds again).
    pub(crate) fn catalog_publish_marked(&mut self, _blocking: &Blocking, build: &Build, marker: Option<&str>) -> Result<Catalog> {
        self.catalog_writer_current(build)?;
        let tx = self.conn.unchecked_transaction()?;
        let (len, groups) = tx.query_row("SELECT total,groups FROM album_catalog_versions WHERE id=?1 AND complete=0 AND exhausted=1",
            [build.version], |r| Ok((r.get(0)?, r.get(1)?)))?;
        tx.execute("UPDATE album_catalog_versions SET complete=1 WHERE id=?1", [build.version])?;
        tx.execute("INSERT INTO album_catalog_head VALUES(0,?1) ON CONFLICT(slot) DO UPDATE SET version=excluded.version", [build.version])?;
        if let Some(marker) = marker {
            tx.execute("INSERT INTO album_catalog_markers VALUES(?1,?2) ON CONFLICT(version) DO UPDATE SET marker=excluded.marker", params![build.version, marker])?;
        }
        tx.commit()?;
        let owner = Arc::new(());
        self.catalog_leases.insert(build.version, Arc::downgrade(&owner));
        self.catalog_build = None;
        Ok(Catalog { version: build.version, len, groups, owner })
    }

    pub(crate) fn catalog_current(&mut self, _blocking: &Blocking) -> Result<Option<Catalog>> {
        let current: Option<(i64,u32,u32)> = self.conn.query_row(
            "SELECT id,total,groups FROM album_catalog_versions JOIN album_catalog_head ON version=id WHERE slot=0 AND complete=1",
            [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).optional()?;
        Ok(current.map(|(version, len, groups)| {
            let owner = self.catalog_leases.get(&version).and_then(Weak::upgrade).unwrap_or_default();
            self.catalog_leases.insert(version, Arc::downgrade(&owner));
            Catalog { version, len, groups, owner }
        }))
    }

    /// The current completed catalog if it was built under `marker`: the
    /// server has not scanned since, so its albums are the server's.
    pub(crate) fn catalog_current_marked(&mut self, blocking: &Blocking, marker: &str) -> Result<Option<Catalog>> {
        let same: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM album_catalog_head JOIN album_catalog_markers USING(version) WHERE slot=0 AND marker=?1)",
            [marker], |r| r.get(0))?;
        if same { self.catalog_current(blocking) } else { Ok(None) }
    }

    /// Records the server's scan marker. When it differs from the last one
    /// seen (or none was), stored responses fetched before now stop counting
    /// as fresh: a scan may have renamed or removed what they hold. Returns
    /// whether it changed. Written only on a change.
    pub(crate) fn observe_scan_marker(&self, _blocking: &Blocking, marker: &str) -> Result<bool> {
        let previous: Option<String> = self.conn.query_row("SELECT value FROM kv WHERE key='music-scan-marker'", [], |r| r.get(0)).optional()?;
        if previous.as_deref() == Some(marker) { return Ok(false); }
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("INSERT INTO kv VALUES('music-scan-marker',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [marker])?;
        tx.execute("INSERT INTO kv VALUES('music-stale-before',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [super::now().to_string()])?;
        tx.commit()?;
        Ok(true)
    }

    /// Stored responses fetched before now stop counting as fresh: the server
    /// has acknowledged a star or rating they show the old value of.
    pub(crate) fn mark_responses_stale(&self, _blocking: &Blocking) -> Result<()> {
        self.conn.execute("INSERT INTO kv VALUES('music-stale-before',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [super::now().to_string()])?;
        Ok(())
    }

    fn catalog_reader_current(&self, _blocking: &Blocking, catalog: &Catalog) -> Result<()> {
        let owned = self.catalog_leases.get(&catalog.version).and_then(Weak::upgrade)
            .is_some_and(|owner| Arc::ptr_eq(&owner, &catalog.owner));
        if owned && self.conn.query_row("SELECT EXISTS(SELECT 1 FROM album_catalog_versions WHERE id=?1 AND complete=1)",
            [catalog.version], |r| r.get::<_, bool>(0))? { Ok(()) } else { Err(Error::InvalidQuery) }
    }

    pub(crate) fn catalog_page(&self, _blocking: &Blocking, catalog: &Catalog, start: u32, count: u32) -> Result<Vec<Album>> {
        self.catalog_reader_current(_blocking, catalog)?;
        if count as usize > BATCH_LIMIT { return Err(Error::InvalidQuery); }
        let mut query = self.conn.prepare("SELECT summary FROM album_catalog_entries WHERE version=?1 AND position>=?2 ORDER BY position LIMIT ?3")?;
        query.query_map(params![catalog.version, start, count], |r| {
            let summary: String = r.get(0)?;
            serde_json::from_str(&summary).map_err(|_| Error::InvalidQuery)
        })?.collect()
    }

    pub(crate) fn catalog_positions(&self, blocking: &Blocking, catalog: &Catalog, positions: &[u32]) -> Result<Vec<Album>> {
        self.catalog_reader_current(blocking, catalog)?;
        if positions.len() > BATCH_LIMIT || positions.iter().any(|&position| position >= catalog.len) { return Err(Error::InvalidQuery); }
        let mut query = self.conn.prepare("SELECT summary FROM album_catalog_entries WHERE version=?1 AND position=?2")?;
        positions.iter().map(|&position| {
            let summary: String = query.query_row(params![catalog.version, position], |r| r.get(0))?;
            serde_json::from_str(&summary).map_err(|_| Error::InvalidQuery)
        }).collect()
    }

    /// Move by actual contiguous display groups, without materializing an index
    /// proportional to the catalog size or assuming a particular collation.
    pub(crate) fn catalog_jump(&self, _blocking: &Blocking, catalog: &Catalog, position: u32, delta: i32) -> Result<Option<Group>> {
        self.catalog_reader_current(_blocking, catalog)?;
        if catalog.groups == 0 { return Ok(None); }
        let current: i64 = self.conn.query_row(
            "SELECT ordinal FROM album_catalog_groups WHERE version=?1 AND position<=?2 ORDER BY position DESC LIMIT 1",
            params![catalog.version, position], |r| r.get(0))?;
        let next = (current + i64::from(delta)).clamp(0, i64::from(catalog.groups) - 1);
        self.conn.query_row("SELECT position,label FROM album_catalog_groups WHERE version=?1 AND ordinal=?2",
            params![catalog.version, next], |r| Ok(Group { position:r.get(0)?, label:r.get(1)? })).optional()
    }
}

#[cfg(test)]
mod tests;
