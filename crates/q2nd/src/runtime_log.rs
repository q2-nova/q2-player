#![forbid(unsafe_code)]
//! Direct startup logging, then bounded nonblocking admission and ordered
//! finite sink jobs. No resident logging thread and no logging from the drain.
use std::{
    cell::Cell, collections::VecDeque, fmt, io::{self, Write}, path::Path,
    sync::{Arc, Mutex, OnceLock, atomic::{AtomicBool, AtomicU32, Ordering}},
    time::Duration,
};
use tokio::sync::{mpsc, watch, Notify, OwnedSemaphorePermit, Semaphore};
use crate::rt::Blocking;

const CAPACITY: usize = 64;
const NORMAL_CAPACITY: usize = 48; // Reserve space for warnings/errors.
const BATCH: usize = 8;
const MAX_LINE: usize = 2048;
const TRUNCATED: &str = " [log truncated]\n";
static LOGGER: OnceLock<&'static Logger> = OnceLock::new();
thread_local! { static IN_LOG: Cell<bool> = const { Cell::new(false) }; }
struct RecursionGuard;
impl RecursionGuard {
    fn enter() -> Option<Self> { IN_LOG.with(|flag| (!flag.replace(true)).then_some(Self)) }
}
impl Drop for RecursionGuard { fn drop(&mut self) { IN_LOG.with(|flag| flag.set(false)); } }

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Report {
    dropped_normal: u32, dropped_critical: u32, truncated: u32,
    reentrant: u32, sink_failures: u32, after_close: u32, after_close_stall: u32,
}
impl Report {
    fn clean(self) -> bool { self == Self::default() }
    fn line(self) -> String {
        format!("q2nd logger: dropped_normal={} dropped_critical={} truncated={} reentrant={} sink_failures={} after_close={} after_close_stall={}\n",
            self.dropped_normal, self.dropped_critical, self.truncated, self.reentrant, self.sink_failures, self.after_close, self.after_close_stall)
    }
}
#[derive(Default)]
struct Counters {
    dropped_normal: AtomicU32, dropped_critical: AtomicU32, truncated: AtomicU32,
    reentrant: AtomicU32, sink_failures: AtomicU32, after_close: AtomicU32, after_close_stall: AtomicU32,
}
fn increment(value: &AtomicU32) {
    let _ = value.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| Some(old.saturating_add(1)));
}
impl Counters {
    fn report(&self) -> Report {
        Report { dropped_normal: self.dropped_normal.load(Ordering::Relaxed),
            dropped_critical: self.dropped_critical.load(Ordering::Relaxed),
            truncated: self.truncated.load(Ordering::Relaxed), reentrant: self.reentrant.load(Ordering::Relaxed),
            sink_failures: self.sink_failures.load(Ordering::Relaxed), after_close: self.after_close.load(Ordering::Relaxed),
            after_close_stall: self.after_close_stall.load(Ordering::Relaxed) }
    }
}
struct Line {
    text: String,
    profile_first: bool,
    // Retain the normal quota through the actual write, not just dequeue.
    _normal: Option<OwnedSemaphorePermit>,
}
struct Sinks { stderr: Box<dyn Write + Send>, file: Option<Box<dyn Write + Send>> }
impl Sinks {
    fn write(&mut self, _blocking: &Blocking, bytes: &[u8], counters: &Counters) -> bool {
        let mut ok = true;
        // Attempt both sinks independently. A failed stderr must not suppress
        // the retained file, or vice versa. Never call log! or eprint! here.
        if self.stderr.write_all(bytes).is_err() { increment(&counters.sink_failures); ok = false; }
        if self.file.as_mut().is_some_and(|file| file.write_all(bytes).is_err()) {
            increment(&counters.sink_failures); ok = false;
        }
        ok
    }
    fn health(&mut self, blocking: &Blocking, counters: &Counters, reported: &mut Report) {
        let current = counters.report();
        if current != *reported && self.write(blocking, current.line().as_bytes(), counters) { *reported = current; }
    }
    fn flush(&mut self, _blocking: &Blocking, counters: &Counters) {
        if self.stderr.flush().is_err() { increment(&counters.sink_failures); }
        if self.file.as_mut().is_some_and(|file| file.flush().is_err()) { increment(&counters.sink_failures); }
    }
}
struct RuntimeLog {
    sender: mpsc::Sender<Line>, normal: Arc<Semaphore>, counters: Counters,
    closing: AtomicBool, stop: Notify, completed: watch::Sender<Option<Report>>,
    final_health_after_close: AtomicU32,
}
impl RuntimeLog {
    fn reject(&self, critical: bool) {
        increment(if critical { &self.counters.dropped_critical } else { &self.counters.dropped_normal });
    }
    async fn finish(&self, bound: Duration) -> Option<Report> {
        self.closing.store(true, Ordering::Release);
        self.stop.notify_one();
        let mut completed = self.completed.subscribe();
        tokio::time::timeout(bound, async {
            loop {
                if let Some(report) = *completed.borrow_and_update() { return report; }
                completed.changed().await.expect("logger retains completion sender");
            }
        }).await.ok()
    }
}
struct Logger {
    startup: Mutex<Option<(Sinks, mpsc::Receiver<Line>)>>,
    active: AtomicBool, profile_first: AtomicBool, runtime: Arc<RuntimeLog>,
}
impl Logger {
    fn new(sinks: Sinks, profile_first: bool) -> Self {
        let (sender, receiver) = mpsc::channel(CAPACITY);
        let (completed, _) = watch::channel(None);
        Self { startup: Mutex::new(Some((sinks, receiver))), active: AtomicBool::new(false),
            profile_first: AtomicBool::new(profile_first), runtime: Arc::new(RuntimeLog {
                sender, normal: Arc::new(Semaphore::new(NORMAL_CAPACITY)), counters: Counters::default(),
                closing: AtomicBool::new(false), stop: Notify::new(), completed, final_health_after_close: AtomicU32::new(0),
            }) }
    }
    fn start(&self) {
        assert!(crate::rt::may_wait_for_tasks(), "activate logging on ordinary startup");
        let mut startup = self.startup.lock().unwrap();
        let Some((sinks, receiver)) = startup.take() else { return; };
        self.active.store(true, Ordering::Release);
        drop(startup);
        crate::rt::io().spawn(drain(self.runtime.clone(), sinks, receiver));
    }
    fn log_ordinary(&self, record: &log::Record<'_>) -> bool {
        // This is the original synchronous bootstrap boundary, never an
        // executor/pool producer. No witness can escape this calling thread.
        assert!(crate::rt::may_wait_for_tasks(), "direct logging belongs to ordinary startup");
        let blocking = Blocking::dedicated_thread();
        self.log_startup(&blocking, record)
    }
    fn log_startup(&self, blocking: &Blocking, record: &log::Record<'_>) -> bool {
        let mut startup = self.startup.lock().unwrap();
        let Some((sinks, _)) = startup.as_mut() else { return false; };
        let (text, first) = self.format(record, false);
        sinks.write(blocking, text.as_bytes(), &self.runtime.counters);
        if first { crate::record_boot_stage("profile-first-log-output-returned"); }
        true
    }
    fn format(&self, record: &log::Record<'_>, bounded: bool) -> (String, bool) {
        let first = self.profile_first.swap(false, Ordering::Relaxed);
        if first { crate::record_boot_stage("profile-first-log-clock-begin"); }
        let text = if bounded {
            let mut line = BoundedLine { text: String::with_capacity(MAX_LINE), truncated: false };
            if fmt::write(&mut line, format_args!("{} {}: {}\n", crate::ui::clock(true), record.level(), record.args())).is_err() {
                line.truncated = true;
            }
            if line.truncated {
                increment(&self.runtime.counters.truncated);
                line.text.push_str(TRUNCATED);
            }
            line.text
        } else {
            format!("{} {}: {}\n", crate::ui::clock(true), record.level(), record.args())
        };
        if first { crate::record_boot_stage("profile-first-log-clock-returned"); }
        (text, first)
    }
}
impl log::Log for Logger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        enabled_for(metadata.level(), metadata.target())
    }
    fn log(&self, record: &log::Record<'_>) {
        if !enabled_for(record.level(), record.target()) { return; }
        let Some(_recursion) = RecursionGuard::enter() else { increment(&self.runtime.counters.reentrant); return; };
        // Only ordinary startup may use direct sinks. Transition takes this
        // lock before publishing active; runtime producers never acquire it.
        if !self.active.load(Ordering::Acquire) && crate::rt::may_wait_for_tasks() && self.log_ordinary(record) {
            return;
        }
        if self.runtime.closing.load(Ordering::Acquire) {
            increment(&self.runtime.counters.after_close);
            if record.target() == "q2nd::rt::stall" { increment(&self.runtime.counters.after_close_stall); }
            return;
        }
        let critical = record.level() <= log::Level::Warn;
        let normal = if critical { None } else {
            match self.runtime.normal.clone().try_acquire_owned() {
                Ok(permit) => Some(permit),
                Err(_) => { self.runtime.reject(false); return; }
            }
        };
        let (text, profile_first) = self.format(record, true);
        if self.runtime.sender.try_send(Line { text, profile_first, _normal: normal }).is_err() {
            self.runtime.reject(critical);
        }
    }
    // Synchronous flush must never wait for a task that may need this caller's
    // runtime/worker. Process shutdown explicitly awaits finish instead.
    fn flush(&self) {}
}
fn enabled_for(level: log::Level, target: &str) -> bool {
    level <= log::Level::Info || target.starts_with("q2nd")
}
struct BoundedLine { text: String, truncated: bool }
impl fmt::Write for BoundedLine {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        if self.truncated { return Ok(()); }
        let remaining = (MAX_LINE - TRUNCATED.len()).saturating_sub(self.text.len());
        let mut count = remaining.min(text.len());
        while !text.is_char_boundary(count) { count -= 1; }
        self.text.push_str(&text[..count]);
        self.truncated |= count != text.len();
        Ok(())
    }
}
async fn drain(runtime: Arc<RuntimeLog>, mut sinks: Sinks, mut receiver: mpsc::Receiver<Line>) {
    let mut reported = Report::default();
    loop {
        if runtime.closing.load(Ordering::Acquire) { receiver.close(); }
        let first = tokio::select! {
            biased;
            _ = runtime.stop.notified(), if !receiver.is_closed() => { receiver.close(); continue; }
            line = receiver.recv() => line,
        };
        let Some(first) = first else { break; };
        let mut batch = VecDeque::with_capacity(BATCH);
        batch.push_back(first);
        while batch.len() < BATCH {
            match receiver.try_recv() { Ok(line) => batch.push_back(line), Err(_) => break }
        }
        let owner = runtime.clone();
        let written = crate::rt::jobs().run_isolated(move |blocking| {
            write_batch(blocking, sinks, batch, &owner.counters, reported)
        }).await;
        match written {
            Ok(result) => { (sinks, reported) = result; }
            Err(_) => {
                increment(&runtime.counters.sink_failures);
                runtime.closing.store(true, Ordering::Release);
                receiver.close();
                runtime.completed.send_replace(Some(runtime.counters.report()));
                return;
            }
        }
    }
    let owner = runtime.clone();
    let finished = crate::rt::jobs().run_isolated(move |blocking| {
        let _recursion = RecursionGuard::enter();
        sinks.flush(blocking, &owner.counters);
        sinks.health(blocking, &owner.counters, &mut reported);
        owner.final_health_after_close.store(reported.after_close, Ordering::Release);
        drop(sinks);
    }).await;
    if finished.is_err() { increment(&runtime.counters.sink_failures); }
    runtime.completed.send_replace(Some(runtime.counters.report()));
}

fn write_batch(blocking: &Blocking, mut sinks: Sinks, batch: VecDeque<Line>, counters: &Counters, mut reported: Report) -> (Sinks, Report) {
    let _recursion = RecursionGuard::enter();
    for line in batch {
        sinks.write(blocking, line.text.as_bytes(), counters);
        if line.profile_first { crate::record_boot_stage("profile-first-log-output-returned"); }
    }
    sinks.health(blocking, counters, &mut reported);
    (sinks, reported)
}

pub(crate) fn init(path: &Path) {
    assert!(crate::rt::may_wait_for_tasks(), "initialize logging on ordinary startup");
    let blocking = Blocking::dedicated_thread();
    init_startup(&blocking, path);
}
fn init_startup(_blocking: &Blocking, path: &Path) {
    // Startup behavior retained: one older log and optional append-file sink.
    if path.metadata().is_ok_and(|metadata| metadata.len() > 512 * 1024) {
        let _ = std::fs::rename(path, path.with_extension("log.old"));
    }
    let file = std::fs::OpenOptions::new().create(true).append(true).open(path);
    let failed = file.is_err();
    let logger = Box::leak(Box::new(Logger::new(Sinks {
        stderr: Box::new(io::stderr()), file: file.ok().map(|file| Box::new(file) as Box<dyn Write + Send>),
    }, crate::startup_profile_enabled())));
    if failed { increment(&logger.runtime.counters.sink_failures); }
    if log::set_logger(logger).is_ok() { let _ = LOGGER.set(logger); }
    log::set_max_level(log::LevelFilter::Debug);
}
pub(crate) fn initialized() -> bool { LOGGER.get().is_some() }

pub(crate) fn start() { if let Some(logger) = LOGGER.get() { logger.start(); } }
/// Completion and loss are different outcomes. Sink health precedes the
/// executor's final counter snapshot, so it cannot be the only loss evidence.
pub(crate) struct Shutdown { completed: bool }
pub(crate) async fn shutdown(bound: Duration) -> Shutdown {
    let completed = match LOGGER.get() {
        Some(logger) => logger.runtime.finish(bound).await.is_some(),
        None => true,
    };
    Shutdown { completed }
}
impl Shutdown {
    /// Called after Ui::block_on returns, also capturing any warning emitted by
    /// its Timed wrapper. Never use the closed logger to report its own loss.
    pub(crate) fn record(self) -> bool {
        // No logger requested is neutral, not evidence of a completed logger.
        let Some(logger) = LOGGER.get() else { return true; };
        let report = logger.runtime.counters.report();
        let health = logger.runtime.final_health_after_close.load(Ordering::Acquire);
        let rows = shutdown_rows(self.completed, report, health);
        if let Some(time) = crate::platform::system::boottime_us() {
            for (field, count) in rows {
                crate::metrics::logger_shutdown_at(field, count, time);
            }
        } else { crate::metrics::clock_failure(); }
        self.completed && report.clean()
    }
}
fn shutdown_rows(completed: bool, report: Report, health: u32) -> [(crate::metrics::LoggerField, u32); 9] {
    use crate::metrics::LoggerField::*;
    [
        (Outcome, if !completed { 2 } else if report.clean() { 0 } else { 1 }),
        (DroppedNormal, report.dropped_normal), (DroppedCritical, report.dropped_critical),
        (Truncated, report.truncated), (Reentrant, report.reentrant), (SinkFailures, report.sink_failures),
        (AfterClose, report.after_close), (AfterCloseStall, report.after_close_stall), (FinalHealthAfterClose, health),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use log::Log;
    use std::future::Future;
    use std::sync::mpsc as channel;

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<String>>>);
    impl Capture { fn lines(&self) -> Vec<String> { self.0.lock().unwrap().clone() } }
    struct Sink { capture: Capture, before: Option<Box<dyn FnMut() + Send>>, fail: bool }
    impl Write for Sink {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if let Some(before) = &mut self.before { before(); }
            if self.fail { return Err(io::Error::new(io::ErrorKind::BrokenPipe, "generated sink failure")); }
            self.capture.0.lock().unwrap().push(String::from_utf8(bytes.to_vec()).unwrap());
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> { Ok(()) }
    }
    fn sink(capture: Capture) -> Box<dyn Write + Send> { Box::new(Sink { capture, before: None, fail: false }) }
    fn emit(logger: &Logger, level: log::Level, text: &str) {
        logger.log(&log::Record::builder().args(format_args!("{text}")).level(level).target("q2nd::logger-test").build());
    }
    struct Release(Option<channel::Sender<()>>);
    impl Release { fn now(&mut self) { if let Some(send) = self.0.take() { let _ = send.send(()); } } }
    impl Drop for Release { fn drop(&mut self) { self.now(); } }
    fn held() -> (Arc<Logger>, Capture, channel::Receiver<()>, Release) {
        let capture = Capture::default();
        let (entered, observed) = channel::channel();
        let (release, gate) = channel::channel();
        let mut gate = Some(gate);
        let logger = Arc::new(Logger::new(Sinks {
            stderr: Box::new(Sink { capture: capture.clone(), fail: false, before: Some(Box::new(move || {
                if let Some(gate) = gate.take() {
                    entered.send(()).unwrap();
                    gate.recv_timeout(Duration::from_secs(5)).unwrap();
                }
            })) }), file: None,
        }, false));
        logger.start();
        emit(&logger, log::Level::Info, "held-first");
        observed.recv_timeout(Duration::from_secs(3)).unwrap();
        (logger, capture, observed, Release(Some(release)))
    }
    fn finish(logger: &Logger) -> Report {
        crate::rt::Ui::new().unwrap().block_on(logger.runtime.finish(Duration::from_secs(3))).expect("logger drain")
    }

    #[test]
    fn slow_sink_keeps_runtime_producers_live_and_bounds_overflow_and_shutdown() {
        let (logger, capture, _, mut release) = held();
        let (done, received) = channel::channel();
        let producer = logger.clone();
        crate::rt::io().spawn(async move {
            for number in 0..NORMAL_CAPACITY { emit(&producer, log::Level::Info, &format!("normal-{number}")); }
            for number in 0..CAPACITY { emit(&producer, log::Level::Error, &format!("critical-{number}")); }
            // This task must return while the actual output syscall is held.
            done.send(()).unwrap();
        });
        received.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(capture.lines().is_empty());
        let counters = logger.runtime.counters.report();
        assert_eq!(counters.dropped_normal, 1);
        assert_eq!(counters.dropped_critical, (NORMAL_CAPACITY - 1) as u32);
        let timed = crate::rt::Ui::new().unwrap().block_on(logger.runtime.finish(Duration::from_millis(30)));
        assert!(timed.is_none(), "a blocked sink cannot falsely acknowledge drain");
        emit(&logger, log::Level::Error, "after-close");
        release.now();
        let report = finish(&logger);
        assert_eq!(report.after_close, 1);
        assert!(!report.clean());
        let lines = capture.lines();
        let data: Vec<_> = lines.iter().filter(|line| !line.starts_with("q2nd logger:")).collect();
        assert_eq!(data.len(), CAPACITY + 1);
        assert!(data[0].ends_with(": held-first\n"));
        for number in 0..NORMAL_CAPACITY - 1 { assert!(data[number + 1].ends_with(&format!(": normal-{number}\n"))); }
        for number in 0..CAPACITY - NORMAL_CAPACITY + 1 {
            assert!(data[number + NORMAL_CAPACITY].ends_with(&format!(": critical-{number}\n")));
        }
        assert!(lines.iter().any(|line| line.contains("dropped_normal=1 dropped_critical=47")));
    }

    #[test]
    fn post_health_close_rejection_remains_dirty_and_has_numeric_attribution() {
        struct HeldClose { capture: Capture, entered: channel::Sender<()>, release: channel::Receiver<()> }
        impl Write for HeldClose {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.capture.0.lock().unwrap().push(String::from_utf8(bytes.to_vec()).unwrap());
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> { Ok(()) }
        }
        impl Drop for HeldClose {
            fn drop(&mut self) {
                let _ = self.entered.send(());
                let _ = self.release.recv_timeout(Duration::from_secs(5));
            }
        }
        let capture = Capture::default();
        let (entered, observed) = channel::channel();
        let (release, wait) = channel::channel();
        let mut release = Release(Some(release));
        let logger = Arc::new(Logger::new(Sinks {
            stderr: Box::new(HeldClose { capture: capture.clone(), entered, release: wait }), file: None,
        }, false));
        logger.start();
        emit(&logger, log::Level::Info, "accepted-before-close");
        let owner = logger.clone();
        let finishing = crate::rt::io().spawn(async move { owner.runtime.finish(Duration::from_secs(3)).await });
        let reached = observed.recv_timeout(Duration::from_secs(3));
        // The final health snapshot is already written; sink destruction is
        // still owned by the finite job. Reproduce an instrumented late poll.
        if reached.is_ok() {
            logger.log(&log::Record::builder().args(format_args!("generated late stall"))
                .level(log::Level::Warn).target("q2nd::rt::stall").build());
        }
        release.now();
        let report = crate::rt::Ui::new().unwrap().block_on(finishing).unwrap().unwrap();
        reached.unwrap();
        assert_eq!(report.after_close, 1);
        assert_eq!(report.after_close_stall, 1);
        assert!(!report.clean(), "late loss must not be masked as a clean drain");
        assert!(capture.lines().iter().any(|line| line.ends_with(": accepted-before-close\n")));
        assert!(!capture.lines().iter().any(|line| line.starts_with("q2nd logger:")),
            "this interval cannot be attributed through already-closed text sinks");
        use crate::metrics::LoggerField;
        let rows = shutdown_rows(true, report, logger.runtime.final_health_after_close.load(Ordering::Acquire));
        assert!(rows.contains(&(LoggerField::Outcome, 1)));
        assert!(rows.contains(&(LoggerField::AfterClose, 1)));
        assert!(rows.contains(&(LoggerField::AfterCloseStall, 1)));
        assert!(rows.contains(&(LoggerField::FinalHealthAfterClose, 0)));
        assert!(shutdown_rows(false, Report::default(), 0).contains(&(LoggerField::Outcome, 2)),
            "timeout differs from completed loss even if no counter has changed");
    }

    #[test]
    fn cancelled_shutdown_observer_does_not_cancel_accepted_writes() {
        let (logger, capture, _, mut release) = held();
        emit(&logger, log::Level::Warn, "accepted-second");
        let owner = logger.clone();
        let (entered, observed) = channel::channel();
        let waiting = crate::rt::io().spawn(async move {
            let finish = owner.runtime.finish(Duration::from_secs(3));
            tokio::pin!(finish);
            // Poll the real shutdown path once to pin admission closure, then
            // tell the ordinary fixture thread it can cancel this observer.
            std::future::poll_fn(|cx| {
                assert!(finish.as_mut().poll(cx).is_pending());
                std::task::Poll::Ready(())
            }).await;
            entered.send(()).unwrap();
            finish.await
        });
        observed.recv_timeout(Duration::from_secs(3)).unwrap();
        waiting.abort();
        assert!(crate::rt::Ui::new().unwrap().block_on(waiting).unwrap_err().is_cancelled());
        release.now();
        assert!(finish(&logger).clean());
        assert!(capture.lines()[1].ends_with(": accepted-second\n"));
    }

    #[test]
    fn startup_is_direct_and_runtime_failures_truncation_and_reentry_are_visible() {
        let file = Capture::default();
        let stderr = Capture::default();
        let logger = Arc::new(Logger::new(Sinks { stderr: sink(stderr.clone()), file: Some(sink(file.clone())) }, false));
        emit(&logger, log::Level::Info, "ordinary-startup");
        assert_eq!(file.lines(), stderr.lines());
        assert_eq!(file.lines().len(), 1, "startup write completes synchronously");
        // Fail stderr and attempt reentrant logging from the output callback.
        // The file must still receive data and health, without an output loop.
        let weak = Arc::downgrade(&logger);
        let mut once = true;
        logger.startup.lock().unwrap().as_mut().unwrap().0.stderr = Box::new(Sink {
            capture: stderr.clone(), fail: true, before: Some(Box::new(move || {
                if once {
                    once = false;
                    emit(&weak.upgrade().unwrap(), log::Level::Warn, "recursive-sink");
                }
            })),
        });
        logger.start();
        let long = "🎵".repeat(MAX_LINE);
        emit(&logger, log::Level::Warn, &long);
        let report = finish(&logger);
        assert_eq!(report.truncated, 1);
        assert_eq!(report.reentrant, 1);
        assert!(report.sink_failures > 0);
        let lines = file.lines();
        assert!(lines[1].ends_with(TRUNCATED));
        assert!(lines[1].len() <= MAX_LINE);
        assert!(lines.iter().any(|line| line.contains("truncated=1 reentrant=1 sink_failures=")));
        assert!(!lines.iter().any(|line| line.contains("recursive-sink")));
    }
}
