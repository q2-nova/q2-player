#![forbid(unsafe_code)]
//! Dedicated numeric, RAM-only release telemetry. Never includes user content.
use std::{io::Write, os::unix::fs::{MetadataExt, OpenOptionsExt}, path::{Path, PathBuf}, sync::{Arc, Mutex, OnceLock}, collections::VecDeque, time::Duration};
use crate::rt::Blocking;
const LIMIT: u64 = 256 * 1024;
#[derive(Clone, Copy)]
pub(crate) enum Event { NumericOwner, AppStart, FirstFrame, ScreenWakeRequested, ScreenWake, ResumePresent, WifiAssociated, TailscaleReady, BadgeSelected, BadgeSelectedLimit, TailscaleProbe(ProbePhase), LoggerShutdown(LoggerField) }
/// Fixed logger outcome/counters; the numeric value is a count, not elapsed
/// microseconds. Outcome is 0=completed clean, 1=completed dirty, 2=timeout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LoggerField {
    Outcome, DroppedNormal, DroppedCritical, Truncated, Reentrant, SinkFailures,
    AfterClose, AfterCloseStall, FinalHealthAfterClose,
}
impl LoggerField {
    fn name(self) -> &'static str { match self {
        Self::Outcome => "logger-shutdown-outcome",
        Self::DroppedNormal => "logger-dropped-normal", Self::DroppedCritical => "logger-dropped-critical",
        Self::Truncated => "logger-truncated", Self::Reentrant => "logger-reentrant",
        Self::SinkFailures => "logger-sink-failures", Self::AfterClose => "logger-after-close",
        Self::AfterCloseStall => "logger-after-close-stall", Self::FinalHealthAfterClose => "logger-final-health-after-close",
    }}
}
/// Fixed anonymous LocalAPI phases. The numeric value is an attempt index,
/// never a duration, response field, socket path, address or error string.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum ProbePhase {
    Begin, SocketPresent, SocketMissing, SocketOther, SocketCheckError,
    ConnectBegin, Connected, RequestWritten, FirstResponse, HeaderReceived,
    ResponseEnd, Success, Timeout, NotFound, InvalidStatus, Error, Limit,
}
impl ProbePhase {
    fn name(self) -> &'static str { match self {
        Self::Begin => "tailscale-probe-begin",
        Self::SocketPresent => "tailscale-probe-socket-present",
        Self::SocketMissing => "tailscale-probe-socket-missing",
        Self::SocketOther => "tailscale-probe-socket-other",
        Self::SocketCheckError => "tailscale-probe-socket-check-error",
        Self::ConnectBegin => "tailscale-probe-connect-begin",
        Self::Connected => "tailscale-probe-connected",
        Self::RequestWritten => "tailscale-probe-request-written",
        Self::FirstResponse => "tailscale-probe-first-response",
        Self::HeaderReceived => "tailscale-probe-header-received",
        Self::ResponseEnd => "tailscale-probe-response-end",
        Self::Success => "tailscale-probe-success",
        Self::Timeout => "tailscale-probe-timeout",
        Self::NotFound => "tailscale-probe-not-found",
        Self::InvalidStatus => "tailscale-probe-invalid-status",
        Self::Error => "tailscale-probe-error",
        Self::Limit => "tailscale-probe-limit",
    }}
}
impl Event {
    fn name(self) -> &'static str { match self {
        Self::NumericOwner => "numeric-owner-v1", Self::AppStart => "app-start", Self::FirstFrame => "first-frame",
        Self::ScreenWakeRequested => "screen-wake-requested",
        Self::ScreenWake => "input-to-present", Self::ResumePresent => "resume-return-to-present",
        Self::WifiAssociated => "wifi-associated-observed", Self::TailscaleReady => "tailscale-ready-observed",
        Self::BadgeSelected => "badge-selected", Self::BadgeSelectedLimit => "badge-selected-limit",
        Self::TailscaleProbe(phase) => phase.name(), Self::LoggerShutdown(field) => field.name(),
    }}
}
/// CLOCK_BOOTTIME as `S.mmm`, the base of /proc/uptime: log lines that a
/// hardware rig lines up with its own uptime-stamped samples carry it.
pub(crate) fn uptime() -> String {
    let Some(time)=crate::platform::system::boottime_us() else {return "?".into()};
    format!("{}.{:03}",time/1_000_000,time%1_000_000/1000)
}

const QUEUE_LIMIT: usize = 256;
const BATCH_LIMIT: usize = 32;
static OWNER: OnceLock<Option<Arc<Recorder>>> = OnceLock::new();

#[derive(Clone, Copy)]
enum Entry { Metric { event: Event, time: u64, value: u64 }, Boot { stage: &'static str, time: u64 }, Boundary { nonce: u64, accepted: u64 } }
#[derive(Default)]
struct Pending {
    queue: VecDeque<Entry>, accepted: u64, written: u64, dropped: u64, failures: u64,
    running: bool, asynchronous: bool, closed: bool, revision: u64,
}
struct Recorder { runtime: PathBuf, identity: (u32, u64, u64), pending: Mutex<Pending>, done: tokio::sync::Notify }

/// Called on ordinary startup before any runtime or network owner exists.
/// Executable identity belongs to this process, never to the later writer job.
pub(crate) fn initialize() {
    OWNER.get_or_init(initialize_ordinary);
}
fn initialize_ordinary() -> Option<Arc<Recorder>> {
    assert!(crate::rt::may_wait_for_tasks(), "initialize numeric recording on ordinary startup");
    let blocking = Blocking::dedicated_thread();
    initialize_startup(&blocking)
}
fn initialize_startup(blocking: &Blocking) -> Option<Arc<Recorder>> {
    let runtime = std::env::var_os("Q2ND_RUNTIME")?;
    let metadata = std::fs::metadata("/proc/self/exe");
    let (identity, failures) = match metadata { Ok(m) => ((std::process::id(), m.dev(), m.ino()), 0), Err(_) => ((std::process::id(), 0, 0), 1) };
    let owner = Arc::new(Recorder { runtime: runtime.into(),
        identity,
        pending: Mutex::new(Pending { failures, ..Default::default() }), done: tokio::sync::Notify::new() });
    if owner.status(blocking, false).is_err() { owner.pending.lock().unwrap().failures += 1; }
    if let Some(time) = crate::platform::system::boottime_us() {
        owner.submit(Some(Entry::Boot { stage: "numeric-owner-v1", time }));
        owner.submit(Some(Entry::Metric { event: Event::NumericOwner, time, value: 0 }));
    } else { owner.submit(None); }
    Some(owner)
}
/// Runtime callers only enqueue. The existing finite job pool provides the
/// writer; this adds no persistent thread or task waiting for more records.
pub(crate) fn activate() {
    initialize();
    if let Some(Some(owner)) = OWNER.get() {
        crate::rt::jobs();
        owner.pending.lock().unwrap().asynchronous = true;
    }
}
/// Whether this process has a numeric owner to seal, even without a logger.
pub(crate) fn initialized() -> bool { owner().is_some() }
pub(crate) fn enabled(runtime: &Path) -> bool { owner().is_some_and(|owner| owner.runtime == runtime) }
pub(crate) fn request_boundary(runtime: &Path, nonce: u64) {
    if nonce != 0 {
        if let Some(owner) = owner().filter(|owner| owner.runtime == runtime) { owner.submit(Some(Entry::Boundary { nonce, accepted: 0 })); }
    }
}
fn owner() -> Option<&'static Arc<Recorder>> { OWNER.get().and_then(Option::as_ref) }

/// Logger counts are numeric observations, never disguised durations. The
/// caller captures one timestamp for the complete fixed shutdown counter set.
pub(crate) fn logger_shutdown_at(field: LoggerField, count: u32, time: u64) {
    if let Some(owner) = owner() {
        owner.submit(Some(Entry::Metric { event: Event::LoggerShutdown(field), time, value: count as u64 }));
    }
}

/// Numeric enum/bitfield observation, not elapsed time. Selection precedes presentation.
pub(crate) fn record_code(event: Event, value: u32) {
    let Some(time) = crate::platform::system::boottime_us() else { clock_failure(); return; };
    crate::diagnostics::health::observe(event, u64::from(value), time);
    if let Some(owner) = owner() {
        owner.submit(Some(Entry::Metric { event, time, value: u64::from(value) }));
    }
}

pub(crate) fn record(event: Event, value: Duration) {
    let Some(time) = crate::platform::system::boottime_us() else { clock_failure(); return; };
    record_at(event, value, time);
}
/// Preserve the observation timestamp, even for a previously captured probe.
pub(crate) fn record_at(event: Event, value: Duration, time: u64) {
    crate::diagnostics::health::observe(event, value.as_micros().min(u64::MAX as u128) as u64, time);
    if let Some(owner) = owner() {
        owner.submit(Some(Entry::Metric { event, time, value: value.as_micros().min(u64::MAX as u128) as u64 }));
    }
}
/// Runtime-only admission to the already initialized optional numeric owner.
/// No requested owner is a no-op, as for record_at; never fall back to I/O.
pub(crate) fn boot_current_at(stage: &'static str, time: u64) {
    if let Some(owner) = owner() { owner.submit(Some(Entry::Boot { stage, time })); }
}
pub(crate) fn boot_at(runtime: &Path, stage: &'static str, time: u64) {
    if let Some(owner) = owner().filter(|owner| owner.runtime == runtime) {
        owner.submit(Some(Entry::Boot { stage, time }));
    } else {
        // Generated isolated fixtures and pre-initialization ordinary startup.
        // Production initializes this owner before recording main-entry.
        boot_ordinary(runtime, stage, time);
    }
}

fn boot_ordinary(runtime: &Path, stage: &'static str, time: u64) {
    assert!(crate::rt::may_wait_for_tasks(), "direct boot recording belongs to ordinary startup");
    let blocking = Blocking::dedicated_thread();
    let _ = write_boot(&blocking, runtime, stage, time);
}

pub(crate) fn clock_failure() { if let Some(owner) = owner() { owner.submit(None); } }

impl Recorder {
    /// Admission and sealing share this mutex. A rejected post-seal record
    /// belongs outside the final accepted prefix; it cannot change that
    /// prefix's counters, persisted status or files after publication.
    fn submit(self: &Arc<Self>, mut entry: Option<Entry>) -> bool {
        let asynchronous;
        let accepted;
        {
            let mut pending = self.pending.lock().unwrap();
            if pending.closed { return false; }
            asynchronous = pending.asynchronous;
            pending.revision += 1;
            accepted = entry.is_some() && pending.queue.len() < QUEUE_LIMIT;
            if !accepted {
                pending.dropped += 1;
            } else {
                if let Some(Entry::Boundary { nonce, .. }) = entry {
                    entry = Some(Entry::Boundary { nonce, accepted: pending.accepted });
                } else { pending.accepted += 1; }
                pending.queue.push_back(entry.unwrap());
            }
            if pending.running { return accepted; }
            pending.running = true;
        }
        if asynchronous { self.schedule(); } else { self.clone().drain_ordinary(); }
        accepted
    }
    fn schedule(self: &Arc<Self>) {
        let owner = self.clone();
        crate::rt::jobs().run_isolated(move |blocking| owner.drain(blocking));
    }
    fn drain_ordinary(self: Arc<Self>) {
        assert!(crate::rt::may_wait_for_tasks(), "direct numeric drain belongs to ordinary startup");
        let blocking = Blocking::dedicated_thread();
        self.drain(&blocking);
    }
    fn drain(self: Arc<Self>, blocking: &Blocking) {
        let batch: Vec<_> = {
            let mut pending = self.pending.lock().unwrap();
            let count = pending.queue.len().min(BATCH_LIMIT);
            pending.queue.drain(..count).collect()
        };
        for entry in batch {
            let boundary = matches!(entry, Entry::Boundary { .. });
            let result = match entry {
                Entry::Metric { event, time, value } => write(blocking, &self.runtime, self.identity, event, time, value),
                Entry::Boot { stage, time } => write_boot(blocking, &self.runtime, stage, time),
                Entry::Boundary { nonce, accepted } => self.boundary(blocking, nonce, accepted),
            };
            let mut pending = self.pending.lock().unwrap();
            if result.is_ok() { if !boundary { pending.written += 1; } } else { pending.failures += 1; }
        }
        // Publishing status is finite file work too. A failed publication is
        // sticky, and bounded shutdown must refuse a successful exit.
        let published = self.status(blocking, true);
        let again = {
            let mut pending = self.pending.lock().unwrap();
            let published = match published {
                Ok(revision) => revision,
                Err(_) => { pending.failures += 1; pending.revision }
            };
            if pending.queue.is_empty() && pending.revision == published { pending.running = false; false } else { true }
        };
        if again { self.schedule(); } else { self.done.notify_waiters(); }
    }
    fn boundary(&self, blocking: &Blocking, nonce: u64, accepted: u64) -> std::io::Result<()> {
        let (written, dropped, failures) = {
            let p = self.pending.lock().unwrap();
            (p.written, p.dropped, p.failures)
        };
        // This serial writer has completed every entry before the barrier and
        // cannot write later entries until this exact prefix proof is stored.
        let boot = std::fs::symlink_metadata(self.runtime.join("boot-times"))?;
        let metrics = std::fs::symlink_metadata(self.runtime.join("release-metrics"))?;
        if !boot.is_file() || !metrics.is_file() { return Err(std::io::Error::other("numeric boundary file type")); }
        let (pid, device, inode) = self.identity;
        let row = format!("1 {nonce} {pid} {device} {inode} {accepted} {written} {dropped} {failures} {} {}\n", boot.len(), metrics.len());
        self.replace(blocking, "numeric-owner-response", row.as_bytes())
    }
    fn replace(&self, _blocking: &Blocking, name: &str, row: &[u8]) -> std::io::Result<()> {
        let mut file = std::fs::OpenOptions::new().create(true).write(true).truncate(false).mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(self.runtime.join(name))?;
        if !file.metadata()?.is_file() { return Err(std::io::Error::other("numeric status is not regular")); }
        file.set_len(0)?;
        file.write_all(row)
    }
    fn status(&self, blocking: &Blocking, allow_settled: bool) -> std::io::Result<u64> {
        let (revision, row) = {
            let p = self.pending.lock().unwrap();
            let settled = allow_settled && p.closed && p.queue.is_empty();
            let (pid, device, inode) = self.identity;
            (p.revision, format!("1 {pid} {device} {inode} {} {} {} {} {}\n", p.accepted, p.written, p.dropped, p.failures, u8::from(settled)))
        };
        self.replace(blocking, "numeric-owner", row.as_bytes())?;
        Ok(revision)
    }
    async fn settle(self: &Arc<Self>) -> bool {
        {
            let mut p = self.pending.lock().unwrap();
            p.closed = true;
            p.revision += 1;
            if !p.running { p.running = true; self.schedule(); }
        }
        loop {
            let changed = self.done.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            {
                let p = self.pending.lock().unwrap();
                if !p.running { return p.queue.is_empty() && p.accepted == p.written && p.dropped == 0 && p.failures == 0; }
            }
            changed.await;
        }
    }
}
/// Seal the final accepted prefix after the logger and shutdown markers.
/// Background observations reaching admission after this point are explicitly
/// outside that prefix, even if their timestamp was captured earlier. A late
/// FIFO request receives no fresh response. An in-flight syscall remains
/// owned by its finite job if this bound expires.
pub(crate) async fn settle() -> bool {
    let Some(owner) = owner() else { return true; };
    tokio::time::timeout(Duration::from_secs(1), owner.settle()).await.unwrap_or(false)
}

fn append(_blocking: &Blocking, runtime: &Path, name: &str, record: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new().create(true).append(true).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(runtime.join(name))?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len().saturating_add(record.len() as u64) > LIMIT {
        return Err(std::io::Error::other("numeric telemetry capacity or file type"));
    }
    file.write_all(record)
}
fn write(blocking: &Blocking, runtime: &Path, identity: (u32, u64, u64), event: Event, time: u64, value: u64) -> std::io::Result<()> {
    let (pid, device, inode) = identity;
    append(blocking, runtime, "release-metrics", format!("2 {time} {pid} {device} {inode} {} {value}\n", event.name()).as_bytes())
}
fn write_boot(blocking: &Blocking, runtime: &Path, stage: &'static str, time: u64) -> std::io::Result<()> {
    append(blocking, runtime, "boot-times", format!("{}.{:03} {stage}\n", time/1_000_000, time%1_000_000/1000).as_bytes())
}
#[cfg(test)] mod tests {
    use super::*;
    fn generated(label: &str) -> PathBuf {
        std::env::var_os("Q2_GENERATED_TEST_ROOT").map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build/perf"))
            .join(format!("{label}-{:016x}", fastrand::u64(..)))
    }
    #[test] fn numeric_only_bounded_and_no_symlink_following() {
        let blocking = Blocking::dedicated_thread();
        let root = generated("release-metrics");
        std::fs::create_dir_all(&root).unwrap();
        write(&blocking,&root,(42,1,2),Event::ScreenWake,50000,1234).unwrap();
        let file=root.join("release-metrics");let s=std::fs::read_to_string(&file).unwrap();
        assert_eq!(s.split_whitespace().count(),7);assert!(s.contains("input-to-present 1234"));
        std::fs::OpenOptions::new().write(true).open(&file).unwrap().set_len(LIMIT).unwrap();
        assert!(write(&blocking,&root,(42,1,2),Event::WifiAssociated,60000,0).is_err());assert_eq!(std::fs::metadata(&file).unwrap().len(),LIMIT);
        std::fs::remove_file(&file).unwrap();let other=root.join("unchanged");std::fs::write(&other,b"sentinel").unwrap();
        std::os::unix::fs::symlink(&other,&file).unwrap();assert!(write(&blocking,&root,(42,1,2),Event::TailscaleReady,60000,0).is_err());
        assert_eq!(std::fs::read(&other).unwrap(),b"sentinel");std::fs::remove_dir_all(root).unwrap();
    }
    fn recorder(root: &Path) -> Arc<Recorder> {
        std::fs::create_dir_all(root).unwrap();
        Arc::new(Recorder { runtime: root.to_path_buf(), identity: (42, 7, 11),
            pending: Mutex::new(Pending::default()), done: tokio::sync::Notify::new() })
    }
    #[test]
    fn queued_numeric_writes_preserve_capture_identity_order_and_bound_loss() {
        let root = generated("numeric-owner-queue");
        let owner = recorder(&root);
        {
            let mut p = owner.pending.lock().unwrap();
            p.asynchronous = true;
            // A held writer has already accepted ownership. No filesystem
            // work can run until the test releases this actual queued batch.
            p.running = true;
        }
        let ui = crate::rt::Ui::new().unwrap();
        ui.block_on(async {
            owner.submit(Some(Entry::Metric { event: Event::ScreenWake, time: 9000, value: 17 }));
            owner.submit(Some(Entry::Metric { event: Event::ScreenWake, time: 8000, value: 18 }));
            for n in 2..QUEUE_LIMIT { owner.submit(Some(Entry::Boot { stage: "generated", time: n as u64 })); }
            owner.submit(Some(Entry::Boot { stage: "overflow", time: 0 }));
            tokio::time::sleep(Duration::from_millis(1)).await;
            assert!(!root.join("release-metrics").exists());
            let p = owner.pending.lock().unwrap();
            assert_eq!((p.accepted, p.queue.len(), p.dropped), (QUEUE_LIMIT as u64, QUEUE_LIMIT, 1));
        });
        let worker = owner.clone();
        // Dropping the observer cannot cancel an accepted finite batch.
        drop(crate::rt::jobs().run_isolated(move |blocking| worker.drain(blocking)));
        assert!(!ui.block_on(async { tokio::time::timeout(Duration::from_secs(3), owner.settle()).await.unwrap() }));
        let text = std::fs::read_to_string(root.join("release-metrics")).unwrap();
        assert_eq!(text, "2 9000 42 7 11 input-to-present 17\n2 8000 42 7 11 input-to-present 18\n");
        let fields: Vec<u64> = std::fs::read_to_string(root.join("numeric-owner")).unwrap().split_whitespace().map(|v| v.parse().unwrap()).collect();
        assert_eq!(&fields[4..], &[256, 256, 1, 0, 1]);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn numeric_boundary_binds_prior_prefix_and_sink_failure_cannot_settle() {
        let root = generated("numeric-owner-boundary");
        let owner = recorder(&root);
        owner.submit(Some(Entry::Boot { stage: "numeric-owner-v1", time: 1_000 }));
        owner.submit(Some(Entry::Metric { event: Event::FirstFrame, time: 2_000, value: 0 }));
        let expected_boot = std::fs::read(root.join("boot-times")).unwrap();
        let expected_events = std::fs::read(root.join("release-metrics")).unwrap();
        owner.submit(Some(Entry::Boundary { nonce: 123, accepted: 0 }));
        let response = std::fs::read_to_string(root.join("numeric-owner-response")).unwrap();
        let row: Vec<u64> = response.split_whitespace().map(|v| v.parse().unwrap()).collect();
        assert_eq!(&row[..9], &[1,123,42,7,11,2,2,0,0]);
        assert_eq!(&row[9..], &[expected_boot.len() as u64,expected_events.len() as u64]);
        owner.submit(Some(Entry::Metric { event: Event::ScreenWake, time: 3_000, value: 0 }));
        assert!(std::fs::read(root.join("release-metrics")).unwrap().starts_with(&expected_events));
        assert_eq!(std::fs::read_to_string(root.join("numeric-owner-response")).unwrap(),response,
            "an old response cannot be mistaken for a new nonce");
        std::fs::remove_file(root.join("release-metrics")).unwrap();
        let sentinel = root.join("sentinel"); std::fs::write(&sentinel,b"unchanged").unwrap();
        std::os::unix::fs::symlink(&sentinel,root.join("release-metrics")).unwrap();
        owner.submit(Some(Entry::Metric { event: Event::ScreenWake, time: 4_000, value: 0 }));
        assert!(!crate::rt::Ui::new().unwrap().block_on(owner.settle()));
        assert_eq!(std::fs::read(sentinel).unwrap(),b"unchanged");
        assert!(owner.pending.lock().unwrap().failures>0);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sealed_numeric_prefix_rejects_late_observations_without_mutating_its_proof() {
        let root = generated("numeric-owner-seal");
        let owner = recorder(&root);
        owner.submit(Some(Entry::Boot { stage: "numeric-owner-v1", time: 1_000 }));
        owner.submit(Some(Entry::Metric { event: Event::FirstFrame, time: 2_000, value: 0 }));
        owner.submit(Some(Entry::Boundary { nonce: 123, accepted: 0 }));
        assert!(crate::rt::Ui::new().unwrap().block_on(owner.settle()));
        let before: Vec<_> = ["boot-times", "release-metrics", "numeric-owner", "numeric-owner-response"]
            .into_iter().map(|name| (name, std::fs::read(root.join(name)).unwrap())).collect();
        // An already captured timestamp may reach admission only after close.
        assert!(!owner.submit(Some(Entry::Metric { event: Event::WifiAssociated, time: 1_500, value: 0 })));
        assert!(!owner.submit(None));
        assert!(!owner.submit(Some(Entry::Boundary { nonce: 124, accepted: 0 })));
        for (name, bytes) in before { assert_eq!(std::fs::read(root.join(name)).unwrap(), bytes, "{name}"); }
        let p = owner.pending.lock().unwrap();
        assert!(p.closed && !p.running && p.queue.is_empty());
        assert_eq!((p.accepted,p.written,p.dropped,p.failures),(2,2,0,0));
        drop(p);
        std::fs::remove_dir_all(root).unwrap();
    }

}
