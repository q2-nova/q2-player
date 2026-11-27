use super::*;

fn rows(start: u32, count: u32) -> Vec<(Album, String)> {
    (start..start+count).map(|n| (Album { id:format!("id-{n}"), name:format!("Generated {n}"), artist:"Generated".into(),
        ..Default::default() }, format!("Group {}", n / 400))).collect()
}
fn small(db: &mut Db, blocking: &Blocking, start: u32) -> Catalog {
    let build = db.catalog_begin(blocking).unwrap();
    db.catalog_append(blocking, &build, 0, &rows(start, 3)).unwrap();
    db.catalog_publish(blocking, &build).unwrap()
}

#[test]
fn large_catalog_is_paged_in_both_directions_without_the_twenty_thousand_limit() {
    let b = Blocking::dedicated_thread(); let mut db = Db::memory(&b);
    let build = db.catalog_begin(&b).unwrap();
    for start in (0..20_500).step_by(500) {
        assert_eq!(db.catalog_append(&b, &build, start, &rows(start, 500)).unwrap(), start + 500);
        assert!(db.catalog_current(&b).unwrap().is_none(), "staging never becomes visible");
        assert!(db.catalog_publish(&b, &build).is_err(), "full pages do not prove exhaustion");
    }
    db.catalog_append(&b, &build, 20_500, &rows(20_500, 1)).unwrap();
    let catalog = db.catalog_publish(&b, &build).unwrap();
    assert_eq!(catalog.len, 20_501);
    for start in (0..20_501).step_by(100).chain((0..20_501).step_by(100).rev()) {
        let page = db.catalog_page(&b, &catalog, start, 100).unwrap();
        assert_eq!(page.len(), (20_501-start).min(100) as usize);
        assert!(page.iter().enumerate().all(|(i,a)| a.id == format!("id-{}", start + i as u32)));
    }
    assert!(db.catalog_page(&b, &catalog, 20_501, 100).unwrap().is_empty());
    assert!(db.catalog_page(&b, &catalog, 0, 501).is_err());
    assert_eq!(db.catalog_jump(&b, &catalog, 500, 1).unwrap(), Some(Group { position:800, label:"Group 2".into() }));
    assert_eq!(db.catalog_jump(&b, &catalog, 500, i32::MIN).unwrap().unwrap().position, 0);
    assert_eq!(db.catalog_jump(&b, &catalog, 0, i32::MAX).unwrap().unwrap().position, 20_400);
}

#[test]
fn catalog_overlap_groups_and_repeated_pages_preserve_server_order() {
    let b = Blocking::dedicated_thread(); let mut db = Db::memory(&b);
    let build = db.catalog_begin(&b).unwrap();
    db.catalog_append(&b, &build, 0, &rows(0, 500)).unwrap();
    assert!(db.catalog_append(&b, &build, 499, &rows(500, 500)).is_err(), "wrong offset cannot append");
    assert!(db.catalog_append(&b, &build, 500, &rows(0, 500)).is_err(), "repeated page cannot claim progress");
    assert!(db.catalog_publish(&b, &build).is_err());
    assert_eq!(db.catalog_append(&b, &build, 500, &rows(490, 500)).unwrap(), 1000);
    // Repeated label after a different group stays a distinct position; no
    // assumption that local normalization has the server's lexical order.
    let mut end = rows(990, 2); end[0].1 = "Group 0".into(); end[1].1 = "Group 2".into();
    db.catalog_append(&b, &build, 1000, &end).unwrap();
    let catalog = db.catalog_publish(&b, &build).unwrap();
    assert_eq!(catalog.len, 992); assert_eq!(catalog.groups, 5);
    assert_eq!(db.catalog_page(&b, &catalog, 985, 20).unwrap().iter().map(|a| a.id.clone()).collect::<Vec<_>>(),
        (985..992).map(|n| format!("id-{n}")).collect::<Vec<_>>());
    assert_eq!(db.catalog_jump(&b, &catalog, 900, 1).unwrap(), Some(Group { position:990, label:"Group 0".into() }));
    assert!(db.catalog_append(&b, &build, 1002, &[]).is_err(), "published writer cannot append");
}

#[test]
fn refresh_keeps_pinned_readers_and_reclaims_only_unowned_versions() {
    let b = Blocking::dedicated_thread(); let mut db = Db::memory(&b);
    let old = small(&mut db, &b, 0); let clone = old.clone();
    let failed = db.catalog_begin(&b).unwrap();
    db.catalog_append(&b, &failed, 0, &rows(10, 500)).unwrap();
    assert_eq!(db.catalog_current(&b).unwrap().unwrap().version, old.version);
    let new = small(&mut db, &b, 100);
    assert!(db.catalog_append(&b, &failed, 500, &[]).is_err(), "superseded writer is rejected");
    assert_eq!(db.catalog_page(&b, &old, 0, 1).unwrap()[0].id, "id-0");
    drop(old);
    let abandoned = db.catalog_begin(&b).unwrap();
    assert_eq!(db.catalog_page(&b, &clone, 0, 1).unwrap()[0].id, "id-0");
    let old_id = clone.version; drop(clone); drop(abandoned);
    db.catalog_begin(&b).unwrap();
    assert!(!db.conn.query_row("SELECT EXISTS(SELECT 1 FROM album_catalog_versions WHERE id=?1)", [old_id], |r| r.get::<_, bool>(0)).unwrap());
    assert_eq!(db.catalog_page(&b, &new, 0, 1).unwrap()[0].id, "id-100");
    assert_eq!(db.conn.query_row("SELECT count(*) FROM album_catalog_versions", [], |r| r.get::<_, u32>(0)).unwrap(), 2,
        "one current catalog and at most one staging version after collection");
}

#[test]
fn catalog_write_and_publication_failures_leave_the_previous_head_and_batch_intact() {
    let b = Blocking::dedicated_thread(); let mut db = Db::memory(&b);
    let old = small(&mut db, &b, 0);
    let build = db.catalog_begin(&b).unwrap();
    db.conn.execute_batch("CREATE TRIGGER reject_catalog BEFORE INSERT ON album_catalog_entries WHEN NEW.id='id-20' BEGIN SELECT RAISE(ABORT,'generated write failure'); END").unwrap();
    assert!(db.catalog_append(&b, &build, 0, &rows(10, 50)).is_err());
    assert_eq!(db.conn.query_row("SELECT count(*) FROM album_catalog_entries WHERE version=?1", [build.version], |r| r.get::<_, u32>(0)).unwrap(), 0);
    db.conn.execute_batch("DROP TRIGGER reject_catalog").unwrap();
    db.catalog_append(&b, &build, 0, &rows(10, 50)).unwrap();
    db.conn.execute_batch("CREATE TRIGGER reject_head BEFORE UPDATE ON album_catalog_head BEGIN SELECT RAISE(ABORT,'generated publish failure'); END").unwrap();
    assert!(db.catalog_publish(&b, &build).is_err());
    assert_eq!(db.catalog_current(&b).unwrap().unwrap().version, old.version);
    assert_eq!(db.catalog_page(&b, &old, 0, 1).unwrap()[0].id, "id-0");
    db.conn.execute_batch("DROP TRIGGER reject_head").unwrap();
    assert_eq!(db.catalog_publish(&b, &build).unwrap().len, 50);
}

#[test]
fn catalog_handles_cannot_cross_databases_or_music_account_resets() {
    let b = Blocking::dedicated_thread(); let mut db = Db::memory(&b); let mut other = Db::memory(&b);
    db.bind_music_server(&b, "generated-first").unwrap();
    let catalog = small(&mut db, &b, 0); let build = db.catalog_begin(&b).unwrap();
    small(&mut other, &b, 0); other.catalog_begin(&b).unwrap();
    assert!(other.catalog_page(&b, &catalog, 0, 1).is_err());
    assert!(other.catalog_append(&b, &build, 0, &rows(0, 1)).is_err());
    db.bind_music_server(&b, "generated-second").unwrap();
    assert!(db.catalog_current(&b).unwrap().is_none());
    assert!(db.catalog_page(&b, &catalog, 0, 1).is_err());
    assert!(db.catalog_append(&b, &build, 0, &rows(0, 1)).is_err());
    let new = small(&mut db, &b, 100);
    assert_ne!(new.version, catalog.version, "account reset does not recycle identities");
    assert_eq!(db.catalog_page(&b, &new, 0, 1).unwrap()[0].id, "id-100");
}

#[test]
fn card_free_sqlite_full_and_reopen_preserve_completed_catalogs() {
    let b = Blocking::dedicated_thread(); let mut db = Db::temporary(&b);
    let old = small(&mut db, &b, 0); let build = db.catalog_begin(&b).unwrap();
    let mut huge = rows(100, 500);
    for (album, _) in &mut huge { album.name = "generated".repeat(1024); }
    let error = db.catalog_append(&b, &build, 0, &huge).unwrap_err();
    assert!(matches!(error, Error::SqliteFailure(e, _) if e.code == rusqlite::ErrorCode::DiskFull));
    assert_eq!(db.catalog_page(&b, &old, 0, 1).unwrap()[0].id, "id-0");
    assert_eq!(db.catalog_current(&b).unwrap().unwrap().version, old.version);
    assert!(db.catalog_publish(&b, &build).is_err());

    let root = crate::library::generated_test_directory("catalog-reopen");
    std::fs::create_dir_all(&root).unwrap(); let path = root.join("catalog.sqlite");
    let mut db = Db::open(&b, &path).unwrap(); let before = small(&mut db, &b, 30);
    db.catalog_begin(&b).unwrap(); drop(db);
    let mut reopened = Db::open(&b, &path).unwrap();
    assert!(reopened.catalog_page(&b, &before, 0, 1).is_err(), "handles do not survive connection replacement");
    let after = reopened.catalog_current(&b).unwrap().unwrap();
    assert_eq!(reopened.catalog_page(&b, &after, 0, 1).unwrap()[0].id, "id-30");
    reopened.catalog_begin(&b).unwrap();
    assert_eq!(reopened.conn.query_row("SELECT count(*) FROM album_catalog_versions", [], |r| r.get::<_, u32>(0)).unwrap(), 2);
}
