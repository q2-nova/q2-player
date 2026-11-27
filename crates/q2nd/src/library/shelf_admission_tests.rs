//! Real DB-owner ordering with generated identities; no network/device dependency.
use super::*;
use std::sync::mpsc;
use std::time::Duration;

fn fixture() -> (Arc<Library>, crate::config::Config) {
    let mut config = crate::config::Config::default();
    config.audiobookshelf.url = "http://127.0.0.1:1/generated".into();
    config.audiobookshelf.token = "generated-a".into();
    let paths =
        crate::config::Paths::new(super::super::generated_test_directory("shelf-db-admission"));
    let (lib, _) = Library::create(paths, &config, Arc::new(|_| {}));
    (lib, config)
}
fn wait<T: Send + 'static>(
    future: impl std::future::Future<Output = Result<T, LibError>> + Send + 'static,
) -> Result<T, LibError> {
    super::super::shelf_browse::wait(future)
}
struct Gate(Option<mpsc::SyncSender<()>>);
impl Drop for Gate {
    fn drop(&mut self) {
        if let Some(release) = self.0.take() {
            let _ = release.send(());
        }
    }
}
fn hold_database(lib: &Library) -> Gate {
    let (entered, seen) = mpsc::sync_channel(1);
    let (release, resume) = mpsc::sync_channel(1);
    lib.db_post(move |_, _| {
        entered.send(()).unwrap();
        resume
            .recv_timeout(Duration::from_secs(5))
            .expect("generated owner gate released");
    });
    seen.recv_timeout(Duration::from_secs(5))
        .expect("DB owner entered gate");
    Gate(Some(release))
}
fn client(lib: &Library) -> Arc<Client> {
    lib.shelf.read().unwrap().client.clone().unwrap()
}
fn verified(lib: &Arc<Library>, namespace: &str) {
    let publication = lib
        .prepare_shelf_probe(client(lib), Ok(namespace.into()))
        .unwrap();
    wait(publication.saved).unwrap();
}

#[test]
fn accepted_shelf_job_precedes_rebind_but_old_result_and_late_job_are_refused() {
    let (lib, _) = fixture();
    verified(&lib, "generated-a-namespace");
    let old = lib.shelf_generation();
    let gate = hold_database(&lib);
    let accepted = lib.shelf_db_run(old.clone(), |_blocking, db| {
        db.put_spoken_response(&crate::rt::Blocking::dedicated_thread(), "generated-order", "accepted-before-rebind")
            .map_err(fail)
    });
    // This is the real identity-change path. Binding and marker are posted
    // while the DB is occupied, without waiting for that owner or its mutex.
    let invalidated = lib
        .prepare_shelf_probe(client(&lib), Err(subsonic::audiobookshelf::Error::Changed))
        .unwrap();
    let late = lib.shelf_db_run(old.clone(), |_blocking, db| {
        db.put_spoken_response(&crate::rt::Blocking::dedicated_thread(), "generated-order", "late-obsolete-response")
            .map_err(fail)
    });
    let live = lib.shelf_generation();
    assert!(!Arc::ptr_eq(&old, &live));
    drop(gate);
    wait(accepted).unwrap();
    wait(invalidated.saved).unwrap();
    assert!(wait(late).is_err());
    let result_lib = lib.clone();
    assert!(wait(async move { result_lib.shelf_result(old, ()).await }).is_err());
    assert_eq!(
        wait(lib.shelf_db_run(live, |_blocking, db| {
            db.spoken_response(&crate::rt::Blocking::dedicated_thread(), "generated-order").map_err(fail)
        }))
        .unwrap()
        .as_deref(),
        Some("accepted-before-rebind")
    );
}

#[test]
fn cancelled_probe_and_a_b_a_route_rebind_cannot_resurrect_a_namespace_marker() {
    let (lib, mut config) = fixture();
    verified(&lib, "generated-a-namespace");
    let old = lib.shelf_generation();
    let key = client(&lib).account_cache_key();
    let gate = hold_database(&lib);
    let invalidated = lib
        .prepare_shelf_probe(client(&lib), Err(subsonic::audiobookshelf::Error::Changed))
        .unwrap();
    drop(invalidated); // Observer cancellation must not cancel its admitted marker.
    // Both configuration changes complete with the DB still held. They cannot
    // read an older on-card marker ahead of the queued invalidation write.
    config.audiobookshelf.token = "generated-b".into();
    lib.set_shelf(&config);
    let middle = lib.shelf_generation();
    config.audiobookshelf.token = "generated-a".into();
    config.tailscale.enabled = true;
    lib.set_shelf(&config);
    let current = lib.shelf_generation();
    assert!(!Arc::ptr_eq(&old, &current));
    assert!(!Arc::ptr_eq(&middle, &current));
    assert!(lib.shelf.read().unwrap().namespace.is_empty());
    let old_response = lib.shelf_db_run(old, |_, _| Ok(()));
    let middle_response = lib.shelf_db_run(middle, |_, _| Ok(()));
    drop(gate);
    assert!(wait(old_response).is_err());
    assert!(wait(middle_response).is_err());
    assert_eq!(
        wait(lib.shelf_db_run(current, move |_blocking, db| db.spoken_response(&crate::rt::Blocking::dedicated_thread(), &key).map_err(fail)))
            .unwrap()
            .as_deref(),
        Some("")
    );
}

#[test]
fn legacy_guard_and_reset_preserve_owner_binding_and_route_cache() {
    let (lib, mut config) = fixture();
    verified(&lib, "generated-a-namespace");
    let old_client = client(&lib);
    let old = lib.shelf_generation();
    config.tailscale.enabled = true;
    lib.set_shelf(&config);
    let live = lib.shelf_generation();
    assert!(!Arc::ptr_eq(&old, &live));
    assert_eq!(lib.shelf.read().unwrap().namespace, "generated-a-namespace");
    // Remaining file/download transactions use this direct guard: it must
    // reject the previous client, and never replace the ephemeral DB binding.
    assert!(
        lib.publish_shelf::<()>(&old_client, || panic!("obsolete publication ran"))
            .is_err()
    );
    let active = client(&lib);
    lib.publish_shelf(&active, || {
        lib.db().reset(&crate::rt::Blocking::dedicated_thread());
        lib.db()
            .put_spoken_response(&crate::rt::Blocking::dedicated_thread(), "generated-legacy", "current")
            .map_err(fail)
    })
    .unwrap();
    assert!(wait(lib.shelf_db_run(old, |_, _| Ok(()))).is_err());
    assert_eq!(
        wait(lib.shelf_db_run(live, |_blocking, db| {
            db.spoken_response(&crate::rt::Blocking::dedicated_thread(), "generated-legacy").map_err(fail)
        }))
        .unwrap()
        .as_deref(),
        Some("current")
    );
}
