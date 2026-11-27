//! The async runtimes (docs/async-conversion.md, "Target threads").
//!
//! Two current-thread tokio runtimes: [`io`] on its own thread for sockets,
//! pipes, child processes and timers, and [`Ui`] for the main thread (the UI
//! loop runs on it). Each drives its own epoll set. Operations
//! with no readiness interface run on the I/O runtime's blocking pool through
//! [`spawn_blocking`], which hands the closure the [`Blocking`] witness.
//!
//! Timers are `tokio::time`: the workspace's tokio is patched to wake the
//! runtime thread once per expiry (crates/vendor/tokio/README.q2.md), which
//! the tests below count.
//!
//! Rules for async modules (checked by `tools/async_gates.py`): spawn tasks
//! with [`Runtime::spawn`] so the stall detector sees them, never hold a
//! `std` lock across an `.await`, and reach blocking work only through a
//! `&Blocking`.
#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

pub mod blocking;
mod fd;
mod stall;
#[cfg(test)]
pub(crate) mod test_support;

pub use blocking::Blocking;
#[allow(unused_imports)]
pub use blocking::on_executor;
pub use blocking::may_wait_for_tasks;
#[allow(unused_imports)] // Also used by generated synchronous-boundary controls.
pub use blocking::on_pool;
#[allow(unused_imports)]
pub(crate) use fd::{CloseFile, Fd, ReactorFile};
pub use stall::Stall;

use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::task::JoinHandle;

/// Blocking pool: started on demand, at most this many threads.
pub(crate) const BLOCKING_THREADS: usize = 3;
/// An idle pool thread exits after this long.
const BLOCKING_KEEP_ALIVE: Duration = Duration::from_secs(10);

fn builder() -> tokio::runtime::Builder {
    let mut builder = tokio::runtime::Builder::new_current_thread();
    builder.enable_all().max_blocking_threads(BLOCKING_THREADS).thread_keep_alive(BLOCKING_KEEP_ALIVE).thread_name("blocking");
    builder
}

/// A current-thread runtime running on a thread of its own.
#[derive(Clone)]
pub struct Runtime {
    handle: tokio::runtime::Handle,
    stall: Arc<Stall>,
}

impl Runtime {
    /// Starts a thread called `name` that polls a new runtime until the
    /// process exits.
    pub fn start(name: &'static str) -> std::io::Result<Runtime> {
        let runtime = builder().build()?;
        let handle = runtime.handle().clone();
        // The one thread this module creates; tasks go through `spawn`.
        #[allow(clippy::disallowed_methods)]
        std::thread::Builder::new().name(name.into()).spawn(move || {
            if name == "io" { crate::diagnostics::register(2); }
            let _executor = blocking::enter_executor();
            runtime.block_on(std::future::pending::<()>());
        })?;
        Ok(Runtime { handle, stall: Stall::new(name) })
    }

    /// Runs `future` as a task of this runtime. Callable from any thread.
    #[track_caller]
    pub fn spawn<F>(&self, future: F) -> JoinHandle<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        // Boxed: tokio instantiates its task code once per spawned type, so
        // every task with the same output shares one copy (about 3 kB of MIPS
        // code per type otherwise).
        let task = crate::diagnostics::task_tag(std::any::type_name::<F>());
        let future: std::pin::Pin<Box<dyn Future<Output = F::Output> + Send>> = Box::pin(future);
        self.handle.spawn(stall::Timed::new(self.stall.clone(), future, task))
    }

    /// Runs `work` on this runtime's blocking pool with the witness.
    pub fn spawn_blocking<T: Send + 'static>(&self, work: impl FnOnce(&Blocking) -> T + Send + 'static) -> JoinHandle<T> {
        spawn_boxed_blocking(&self.handle, Box::new(work))
    }

    /// The longest time one task kept this runtime from the others.
    pub fn stall(&self) -> &Stall {
        &self.stall
    }
}

/// Declare the Tokio adapter outside the caller-generic methods. Boxing the
/// callback inside those methods still leaves their anonymous caller type in
/// the adapter's identity; this boundary shares task code for each output T.
/// Both owners keep their existing pool, one callback allocation and RAII scope.
#[track_caller]
fn spawn_boxed_blocking<T: Send + 'static>(handle: &tokio::runtime::Handle,
    work: Box<dyn FnOnce(&Blocking) -> T + Send>) -> JoinHandle<T> {
    handle.spawn_blocking(move || {
        let _pool = blocking::enter_pool();
        work(&Blocking::new())
    })
}

/// The I/O runtime, on the thread `io`. Started at the first call; a failure
/// to start it ends the process, as a failed `player` thread does.
pub fn io() -> &'static Runtime {
    static IO: OnceLock<Runtime> = OnceLock::new();
    IO.get_or_init(|| Runtime::start("io").expect("io runtime"))
}

/// Runs `work` on the I/O runtime's blocking pool. `work` receives the
/// witness that blocking functions ask for.
pub fn spawn_blocking<T: Send + 'static>(work: impl FnOnce(&Blocking) -> T + Send + 'static) -> JoinHandle<T> {
    io().spawn_blocking(work)
}

/// Finite local work isolated from the I/O runtime's blocking capacity.
/// UI filesystem/database jobs must not delay transfer writes or DNS. Network
/// work belongs in async tasks; every job forbids synchronous task waits.
/// Started on demand, at most [`JOB_THREADS`], each ending after
/// [`BLOCKING_KEEP_ALIVE`] idle: none while nothing runs.
pub struct Jobs {
    // Never driven: only its blocking pool is used.
    runtime: tokio::runtime::Runtime,
}

/// At most this many job threads, as many as the page threads were.
const JOB_THREADS: usize = 3;

impl Jobs {
    fn pool(name: &'static str, capacity: usize) -> std::io::Result<Self> {
        Ok(Self { runtime: tokio::runtime::Builder::new_current_thread()
            .max_blocking_threads(capacity)
            .thread_keep_alive(BLOCKING_KEEP_ALIVE)
            .thread_name(name)
            .build()? })
    }

    /// A separate capacity-one lane with no executor thread and no worker
    /// before first use. Its worker retires on the ordinary pool idle cadence.
    /// Keep this owner on a blocking thread until every submitted job is joined.
    pub fn serial(name: &'static str) -> std::io::Result<Self> { Self::pool(name, 1) }

    /// Finite blocking work on an isolated lane. This callback cannot
    /// synchronously wait for I/O-runtime tasks.
    pub fn run_isolated<T: Send + 'static>(&self, work: impl FnOnce(&Blocking) -> T + Send + 'static) -> JoinHandle<T> {
        spawn_boxed_blocking(self.runtime.handle(), Box::new(work))
    }

    /// Retire an isolated lane without blocking the caller's runtime. Owners
    /// normally join every finite job first; any remaining job keeps its own
    /// resources until it returns. This never cancels a running file syscall.
    pub fn shutdown_background(self) { self.runtime.shutdown_background(); }

}

/// The job pool, made at the first use.
pub fn jobs() -> &'static Jobs {
    static JOBS: OnceLock<Jobs> = OnceLock::new();
    JOBS.get_or_init(|| Jobs::pool("job", JOB_THREADS).expect("job pool"))
}

/// Retire a blocking resource without closing it on an executor during
/// cancellation or an abandoned reply. Ordinary owners already run outside
/// executors and close inline; executor drops use the existing finite job pool.
/// This schedules cleanup, not an acknowledgement that cleanup has completed.
pub(crate) fn drop_off_executor<T: Send + 'static>(resource: T) {
    retire_blocking(move |_| drop(resource));
}

/// Give final resource cleanup the witness of the thread actually executing it.
/// A transferable owner must not carry a witness across its async lifetime.
/// Like `drop_off_executor`, this schedules cleanup without acknowledging it.
pub(crate) fn retire_blocking(work: impl FnOnce(&Blocking) + Send + 'static) {
    if on_executor() {
        jobs().run_isolated(work);
    } else {
        // This synchronous branch cannot move the witness or poll a runtime.
        // Ordinary owners and existing pool jobs preserve their inline close.
        work(&Blocking::new());
    }
}

/// The UI runtime: polled by the thread that calls [`Ui::block_on`]. Built
/// by `main` from stage 2 so its cost is part of every measurement; the UI
/// loop runs on it from stage 4.
pub struct Ui {
    runtime: tokio::runtime::Runtime,
    stall: Arc<Stall>,
}

impl Ui {
    pub fn new() -> std::io::Result<Ui> {
        // The UI thread never blocks; its work goes to `io` or the pool.
        Ok(Ui { runtime: builder().max_blocking_threads(1).build()?, stall: Stall::new("ui") })
    }

    /// Polls `future` and the runtime's tasks on the calling thread.
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        let _executor = blocking::enter_executor();
        self.runtime.block_on(stall::Timed::new(self.stall.clone(), future, crate::diagnostics::task_tag(std::any::type_name::<F>())))
    }

    pub fn stall(&self) -> &Stall {
        &self.stall
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn abandoned_resources_close_off_executor_and_ordinary_owners_close_inline() {
        struct Resource(std::sync::mpsc::Sender<(bool, bool, std::thread::ThreadId)>);
        impl Drop for Resource {
            fn drop(&mut self) {
                self.0.send((on_executor(), on_pool(), std::thread::current().id())).unwrap();
            }
        }
        let (sent, received) = std::sync::mpsc::channel();
        let ordinary = std::thread::current().id();
        drop_off_executor(Resource(sent.clone()));
        assert_eq!(received.try_recv().unwrap(), (false, false, ordinary));

        let ui = Ui::new().unwrap();
        ui.block_on(async { drop_off_executor(Resource(sent.clone())); });
        let closed = received.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!((closed.0, closed.1), (false, true));
        assert_ne!(closed.2, ordinary);

        // A pool result whose observer has been cancelled must retire on the
        // pool or executor, never on whichever runtime drops the reply.
        let (started, ready) = std::sync::mpsc::channel();
        let (release, released) = std::sync::mpsc::channel();
        struct Reply(Option<Resource>);
        impl Drop for Reply {
            fn drop(&mut self) { drop_off_executor(self.0.take()); }
        }
        let pending = jobs().run_isolated(move |_| {
            started.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(5)).unwrap();
            Reply(Some(Resource(sent)))
        });
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        ui.block_on(async { drop(pending); });
        release.send(()).unwrap();
        let closed = received.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(!closed.0);
        assert_ne!(closed.2, ordinary);
    }

    /// Separate capacity does not permit a local job to wait for async I/O.
    #[test]
    fn isolated_jobs_forbid_waiting_for_tasks() {
        let (tx, rx) = std::sync::mpsc::channel();
        jobs().run_isolated(move |_| tx.send((may_wait_for_tasks(), std::thread::current().name().map(str::to_owned))).unwrap());
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)).unwrap(), (false, Some("job".into())));
    }

    /// Voluntary context switches of the calling thread: one per sleep in
    /// the kernel that ended in a wakeup.
    fn voluntary_switches() -> u64 {
        std::fs::read_to_string("/proc/thread-self/status").unwrap().lines()
            .find_map(|l| l.strip_prefix("voluntary_ctxt_switches:")).unwrap().trim().parse().unwrap()
    }

    /// Runs `work` on a fresh UI-style runtime on this thread and returns
    /// (timers that expired, wakeups of this thread).
    fn wakeups<F: Future<Output = u64>>(work: impl FnOnce() -> F) -> (u64, u64) {
        let ui = Ui::new().unwrap();
        // Start the drivers before counting.
        ui.block_on(async { tokio::time::sleep(Duration::from_millis(5)).await });
        let before = voluntary_switches();
        let expiries = ui.block_on(work());
        (expiries, voluntary_switches() - before)
    }

    /// Timer and stall measurements, printed one per line. Run by
    /// `timers_wake_the_thread_once_and_async_work_never_stalls` in a
    /// process of its own: in the shared test process hundreds of threads
    /// contend for kernel locks (each wait is a voluntary switch) and for
    /// CPUs (a preempted poll looks like a stall).
    #[test]
    #[ignore = "run by timers_wake_the_thread_once_and_async_work_never_stalls"]
    fn quiet_probe() {
        let report = |name: &str, (expiries, wakeups): (u64, u64)| println!("WAKE {name} {expiries} {wakeups}");
        report("sleep-250ms", wakeups(|| async {
            for _ in 0..6 { tokio::time::sleep(Duration::from_millis(250)).await; }
            6
        }));
        report("sleep-1s", wakeups(|| async {
            for _ in 0..2 { tokio::time::sleep(Duration::from_secs(1)).await; }
            2
        }));
        report("interval-250ms", wakeups(|| async {
            let mut ticks = tokio::time::interval(Duration::from_millis(250));
            ticks.tick().await;
            for _ in 0..6 { ticks.tick().await; }
            6
        }));
        report("eight-timers", wakeups(|| async {
            let mut tasks = Vec::new();
            for i in 0..8u64 {
                tasks.push(tokio::task::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(60 * i + 1)).await;
                    for _ in 0..3 { tokio::time::sleep(Duration::from_millis(500)).await; }
                    4u64
                }));
            }
            let mut expiries = 0;
            for task in tasks { expiries += task.await.unwrap(); }
            expiries
        }));
        // Each round arms a 300 ms timer and drops it when the 100 ms one
        // fires; then 1,000 timeouts are armed and dropped without waiting.
        report("cancelled", wakeups(|| async {
            for _ in 0..10 {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(100)) => {}
                    _ = tokio::time::sleep(Duration::from_millis(300)) => unreachable!(),
                }
            }
            for _ in 0..1000 {
                let _ = tokio::time::timeout(Duration::from_secs(5), tokio::task::yield_now()).await;
            }
            10
        }));

        // An idle runtime thread: parked for 300 ms, woken by one message.
        let runtime = Runtime::start("rt-test-idle").unwrap();
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let (tx, rx) = std::sync::mpsc::channel();
        runtime.spawn(async move {
            let before = voluntary_switches();
            tx.send(0).unwrap();
            stopped.await.unwrap();
            tx.send(voluntary_switches() - before).unwrap();
        });
        rx.recv_timeout(Duration::from_secs(10)).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        stop.send(()).unwrap();
        println!("IDLE {}", rx.recv_timeout(Duration::from_secs(10)).unwrap());

        // The stall detector over a generated async workload.
        let runtime = Runtime::start("rt-test-stall").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        for i in 0..16u64 {
            let tx = tx.clone();
            runtime.spawn(async move {
                for round in 0..20 {
                    tokio::time::sleep(Duration::from_millis(1 + (i + round) % 5)).await;
                    tokio::task::yield_now().await;
                }
                // Blocking work goes to the pool and does not count.
                io().spawn_blocking(|_| std::thread::sleep(Duration::from_millis(30))).await.unwrap();
                tx.send(()).unwrap();
            });
        }
        for _ in 0..16 { rx.recv_timeout(Duration::from_secs(20)).unwrap(); }
        println!("STALL {} {}", runtime.stall().polls(), runtime.stall().longest().as_micros());
    }

    /// One wakeup fires a timer; two more are allowed for a run as a whole
    /// (the first park, a wake racing the end). Unpatched tokio 1.54.1 makes
    /// about two wakeups per expiry and fails every timer case
    /// (tools/tokio-timer shows both side by side). An idle runtime does not
    /// wake, and no poll of the async workload holds the executor for 20 ms.
    #[test]
    fn timers_wake_the_thread_once_and_async_work_never_stalls() {
        let probe = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "rt::tests::quiet_probe", "--ignored", "--nocapture", "--test-threads=1"])
            .output().unwrap();
        let text = String::from_utf8_lossy(&probe.stdout).into_owned();
        assert!(probe.status.success(), "{text}\n{}", String::from_utf8_lossy(&probe.stderr));
        let mut seen = Vec::new();
        for line in text.lines() {
            // libtest prints the test's name before its first line of output.
            let fields: Vec<&str> = line.split_whitespace().skip_while(|f| !["WAKE", "IDLE", "STALL"].contains(f)).collect();
            match fields.as_slice() {
                ["WAKE", name, expiries, wakeups] => {
                    let (expiries, wakeups): (u64, u64) = (expiries.parse().unwrap(), wakeups.parse().unwrap());
                    assert!(expiries > 0, "{name}: no timer expired");
                    assert!(wakeups <= expiries + 2, "{name}: {wakeups} wakeups for {expiries} expiries");
                    seen.push(name.to_string());
                }
                ["IDLE", switches] => {
                    assert!(switches.parse::<u64>().unwrap() <= 2, "an idle runtime woke {switches} times in 300 ms");
                    seen.push("idle".into());
                }
                ["STALL", polls, longest_us] => {
                    assert!(polls.parse::<u64>().unwrap() > 16 * 20);
                    assert!(longest_us.parse::<u64>().unwrap() < 20_000, "async workload held the executor for {longest_us} us");
                    seen.push("stall".into());
                }
                _ => {}
            }
        }
        assert_eq!(seen, ["sleep-250ms", "sleep-1s", "interval-250ms", "eight-timers", "cancelled", "idle", "stall"], "{text}");
    }

    #[test]
    fn spawn_blocking_hands_out_the_witness_off_the_executor() {
        fn card_work(_: &Blocking) -> (bool, Option<String>) {
            (on_executor(), std::thread::current().name().map(str::to_owned))
        }
        let runtime = Runtime::start("rt-test-pool").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let pool = runtime.clone();
        runtime.spawn(async move {
            let on_runtime = on_executor();
            let (on_pool, name) = pool.spawn_blocking(card_work).await.unwrap();
            tx.send((on_runtime, on_pool, name)).unwrap();
        });
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), (true, false, Some("blocking".to_string())));
        assert!(!on_executor(), "a test thread is not an executor");
        // The global entry point reaches the I/O runtime's pool.
        let (tx, rx) = std::sync::mpsc::channel();
        spawn_blocking(move |blocking| tx.send(card_work(blocking)).unwrap());
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), (false, Some("blocking".to_string())));
    }

    /// Must fail: the witness refuses to exist on a runtime thread.
    /// Panic-abort targets use the host's optimized actual-source control;
    /// they cannot catch this deliberate rejection inside the test process.
    #[cfg(panic = "unwind")]
    #[test]
    fn creating_the_witness_on_an_executor_panics() {
        let ui = Ui::new().unwrap();
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| ui.block_on(async { let _ = Blocking::new(); })));
        assert!(caught.is_err());
        // Outside a runtime the same call is fine.
        let _ = Blocking::dedicated_thread();
    }

    /// The stall detector's negative control: a 100 ms block on the executor
    /// must read at least 100 ms. (Its limit for async work is checked in
    /// `quiet_probe`.)
    #[test]
    fn stall_detector_catches_an_inline_block() {
        let runtime = Runtime::start("rt-test-block").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let started = Instant::now();
        runtime.spawn(async move {
            // The mistake the detector exists for.
            #[allow(clippy::disallowed_methods)]
            std::thread::sleep(Duration::from_millis(100));
            tx.send(()).unwrap();
        });
        rx.recv_timeout(Duration::from_secs(20)).unwrap();
        // The poll is recorded after the task's send; wait for it.
        while runtime.stall().take_longest() < Duration::from_millis(100) {
            assert!(started.elapsed() < Duration::from_secs(10), "the inline block was not detected");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn fd_wrapper_reads_and_writes_through_the_reactor() {
        let runtime = Runtime::start("rt-test-fd").unwrap();
        let (ours, theirs) = std::os::unix::net::UnixStream::pair().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        runtime.spawn(async move {
            let fd = Fd::new(ours.into()).unwrap();
            let mut buffer = [0u8; 16];
            // Nothing to read yet: the task waits without blocking the thread.
            let n = fd.read(&mut buffer).await.unwrap();
            fd.write(&buffer[..n]).await.unwrap();
            let end = fd.read(&mut buffer).await.unwrap();
            tx.send((n, end)).unwrap();
        });
        use std::io::{Read, Write};
        let mut theirs = theirs;
        std::thread::sleep(Duration::from_millis(20));
        theirs.write_all(b"ready").unwrap();
        let mut echo = [0u8; 5];
        theirs.read_exact(&mut echo).unwrap();
        assert_eq!(&echo, b"ready");
        drop(theirs);
        assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), (5, 0));
    }
}
