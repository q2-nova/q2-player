//! The `db` thread exclusively owns the production SQLite connection. Requests
//! contain owned arguments and run in queue order, without Library locks.
//! Tests retain a shared adapter only for existing fixture injection and fault
//! controls; production code cannot borrow the connection or its mutex.
use super::db::Db;
use crate::rt::Blocking;
use std::cell::Cell;
use std::future::Future;
use std::sync::mpsc;
#[cfg(test)]
use std::sync::{Arc, Mutex};

#[cfg(not(test))]
type Database = Db;
#[cfg(test)]
type Database = Arc<Mutex<Db>>;

type Job = Box<dyn FnOnce(&crate::rt::Blocking, &mut Db) + Send>;

thread_local! {
    static IN_JOB: Cell<bool> = const { Cell::new(false) };
}

#[cfg(test)]
thread_local! {
    static HELD: Cell<u32> = const { Cell::new(0) };
}

/// `Library::db()`'s guard. Counts the guards a thread holds, so code that
/// waits for the `db` thread can assert it holds none (it would wait for the
/// mutex it holds).
#[cfg(test)]
pub(crate) struct DbGuard<'a>(std::sync::MutexGuard<'a, Db>);

#[cfg(test)]
impl<'a> DbGuard<'a> {
    pub(super) fn new(guard: std::sync::MutexGuard<'a, Db>) -> Self {
        HELD.with(|h| h.set(h.get() + 1));
        DbGuard(guard)
    }
}

#[cfg(test)]
impl Drop for DbGuard<'_> {
    fn drop(&mut self) {
        HELD.with(|h| h.set(h.get() - 1));
    }
}

#[cfg(test)]
impl std::ops::Deref for DbGuard<'_> {
    type Target = Db;
    fn deref(&self) -> &Db {
        &self.0
    }
}

#[cfg(test)]
impl std::ops::DerefMut for DbGuard<'_> {
    fn deref_mut(&mut self) -> &mut Db {
        &mut self.0
    }
}

/// Whether the calling thread holds a `Library::db()` guard.
pub(super) fn guard_held() -> bool {
    #[cfg(test)]
    { HELD.with(|h| h.get() > 0) }
    #[cfg(not(test))]
    { false }
}

/// Whether the calling thread is running a `db` closure now.
pub(super) fn in_job() -> bool {
    IN_JOB.with(Cell::get)
}

#[derive(Clone)]
pub(super) struct DbWorker {
    tx: mpsc::Sender<Job>,
}

impl DbWorker {
    /// Starts the thread. It ends when the worker is dropped, after the
    /// closures already sent.
    pub fn start(db: Database) -> DbWorker {
        let (tx, rx) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("db".into())
            .spawn(move || {
                let blocking = Blocking::dedicated_thread();
                serve(&blocking, db, rx);
            })
            .expect("db thread");
        DbWorker { tx }
    }

    /// Runs `job` on the `db` thread; the result arrives when it has run.
    pub fn run<R: Send + 'static>(&self, job: impl FnOnce(&crate::rt::Blocking, &mut Db) -> R + Send + 'static) -> impl Future<Output = R> + Send + 'static {
        let (reply, result) = tokio::sync::oneshot::channel();
        self.post(move |blocking, db| {
            let _ = reply.send(job(blocking, db));
        });
        async move { result.await.expect("the db thread ended with work queued") }
    }

    /// Explicit transaction for dedicated player/startup/diagnostic threads.
    /// Runtime callers await run(); finite pool callbacks must split their DB
    /// stage out instead of occupying a worker while this owner is busy.
    pub fn run_blocking<R: Send + 'static>(&self, job: impl FnOnce(&crate::rt::Blocking, &mut Db) -> R + Send + 'static) -> R {
        assert!(!crate::rt::on_executor() && !crate::rt::on_pool(), "await database work from runtime/pool callers");
        assert!(!in_job() && !guard_held(), "database owner cannot wait for itself");
        let (reply, result) = mpsc::sync_channel(1);
        self.post(move |blocking, db| { let _ = reply.send(job(blocking, db)); });
        result.recv().expect("the db thread ended with work queued")
    }

    /// Runs `job` on the `db` thread, in order with everything sent before
    /// it, without waiting.
    pub fn post(&self, job: impl FnOnce(&crate::rt::Blocking, &mut Db) + Send + 'static) {
        self.tx.send(Box::new(job)).expect("the db thread ended");
    }

    /// Fixture barrier; production callers await an ordered owner message.
    #[cfg(test)]
    pub fn drain(&self) {
        let (done, wait) = mpsc::sync_channel(1);
        self.post(move |_, _| {
            let _ = done.send(());
        });
        let _ = wait.recv();
    }
}

fn serve(blocking: &Blocking, db: Database, rx: mpsc::Receiver<Job>) {
    #[cfg(not(test))]
    let mut db = db;
    for job in rx {
        #[cfg(test)]
        let mut db = db.lock().unwrap();
        IN_JOB.with(|j| j.set(true));
        job(blocking, &mut db);
        IN_JOB.with(|j| j.set(false));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn worker() -> (Arc<Mutex<Db>>, DbWorker) {
        let db = Arc::new(Mutex::new(Db::temporary(&crate::rt::Blocking::dedicated_thread())));
        (db.clone(), DbWorker::start(db))
    }

    #[test]
    fn closures_run_in_the_order_sent_and_drain_waits_for_them() {
        let (db, worker) = worker();
        let seen = Arc::new(Mutex::new(Vec::new()));
        for i in 0..50 {
            let seen = seen.clone();
            worker.post(move |_blocking, db| {
                assert!(in_job());
                db.set(&crate::rt::Blocking::dedicated_thread(), "order", &i.to_string());
                seen.lock().unwrap().push(i);
            });
        }
        worker.drain();
        assert_eq!(*seen.lock().unwrap(), (0..50).collect::<Vec<_>>());
        assert_eq!(db.lock().unwrap().get(&crate::rt::Blocking::dedicated_thread(), "order").as_deref(), Some("49"));
        assert!(!in_job());
    }

    #[test]
    fn async_callers_get_results_and_threads_share_the_same_database() {
        let (db, worker) = worker();
        db.lock().unwrap().set(&crate::rt::Blocking::dedicated_thread(), "from-thread", "1");
        let runtime = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let got = runtime.block_on(worker.run(|_blocking, db| {
            db.set(&crate::rt::Blocking::dedicated_thread(), "from-task", "2");
            db.get(&crate::rt::Blocking::dedicated_thread(), "from-thread")
        }));
        assert_eq!(got.as_deref(), Some("1"));
        assert_eq!(db.lock().unwrap().get(&crate::rt::Blocking::dedicated_thread(), "from-task").as_deref(), Some("2"));
    }

    #[test]
    fn dropping_the_worker_runs_what_was_sent() {
        let (db, worker) = worker();
        let (done, wait) = mpsc::channel();
        for i in 0..20 {
            let done = done.clone();
            worker.post(move |_blocking, db| {
                db.set(&crate::rt::Blocking::dedicated_thread(), "last", &i.to_string());
                if i == 19 { let _ = done.send(()); }
            });
        }
        drop(worker);
        wait.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        assert_eq!(db.lock().unwrap().get(&crate::rt::Blocking::dedicated_thread(), "last").as_deref(), Some("19"));
    }
}

#[cfg(all(test, panic = "unwind"))]
mod boundary_tests {
    use super::*;
    #[test]
    fn blocking_transactions_refuse_runtime_pool_and_owner_reentry() {
        let db = Arc::new(Mutex::new(Db::temporary(&crate::rt::Blocking::dedicated_thread())));
        let worker = Arc::new(DbWorker::start(db));
        let ui = crate::rt::Ui::new().unwrap();
        let ordinary_benchmark = || Db::startup_binding_benchmark_ordinary(std::path::Path::new("not-generated"), 0, "invalid");
        assert!(ordinary_benchmark().is_err()); // Ordinary argument rejection, no filesystem access.
        ui.block_on(async {
            assert!(std::panic::catch_unwind(ordinary_benchmark).is_err());
            assert!(crate::rt::jobs().run_isolated(move |_| std::panic::catch_unwind(ordinary_benchmark).is_err()).await.unwrap());
            assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker.run_blocking(|_, _| ()))).is_err());
            let copy = worker.clone();
            assert!(crate::rt::jobs().run_isolated(move |_| std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| copy.run_blocking(|_, _| ()))).is_err()).await.unwrap());
            let copy = worker.clone();
            assert!(worker.run(move |_, _| std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| copy.run_blocking(|_, _| ()))).is_err()).await);
        });
        assert_eq!(worker.run_blocking(|blocking, db| { db.set(blocking, "generated", "usable"); db.get(blocking, "generated") }), Some("usable".into()));
    }
}
