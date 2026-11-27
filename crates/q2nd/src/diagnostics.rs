//! Opt-in numeric incident reports. The independent sampler can persist a
//! blocked executor's stack; delivery uses the existing I/O runtime only while
//! the radio is already online. No ordinary logs or media data enter reports.
use crate::rt::Blocking;
use crate::config::telemetry::{Telemetry, Stream};
mod crash;
mod supervisor;
mod kernel;
pub(crate) mod health;
mod dispatch;
mod ddsketch;
mod identity;
#[cfg(test)]
mod import_tests;
use serde::{Deserialize, Serialize};
use std::os::unix::net::UnixDatagram;
use std::{collections::HashMap, io::{self, Read, Write}, path::{Path, PathBuf},
    os::{fd::IntoRawFd, unix::fs::OpenOptionsExt},
    sync::{Arc, OnceLock, Mutex, atomic::{AtomicBool, AtomicU32, Ordering}}, time::{Duration, Instant}};

const LIMIT: usize = 256;
const REPORT_BYTES: usize = 12 * 1024;
static STATE: OnceLock<Arc<State>> = OnceLock::new();
static ACTIVE: AtomicBool = AtomicBool::new(false);
static ONLINE: AtomicBool = AtomicBool::new(false);
static WAKE: tokio::sync::Notify = tokio::sync::Notify::const_new();
static FAILURES: AtomicU32 = AtomicU32::new(0);

#[repr(C)]
#[derive(Clone, Copy)]
struct Sample {
    version: u32, role: u32, tid: u32, task: u32, sequence: u32,
    elapsed_ms: u32, count: u32, status: i32, generation: u32, reserved: u32, monotonic_us: u64, pc: [u64; 48],
}
unsafe extern "C" {
    fn q2_stall_collecting(collecting: i32) -> u32;
    fn q2_stall_generation() -> u32;
    fn q2_stall_start(writer: i32, reader: i32, callback: extern "C" fn(*const Sample)) -> i32;
    fn q2_stall_register(role: u32) -> i32;
    fn q2_stall_enter(task: u32) -> u32;
    fn q2_stall_leave(ticket: u32);
    fn q2_stall_note(role: u32, task: u32, elapsed_ms: u32, generation: u32);
    fn q2_stall_wall_enter(task: u32) -> u32;
    fn q2_stall_wall_leave(ticket: u32);
    fn q2_stall_dropped() -> u32;
}

#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
struct Transport { endpoint: String, token: String }
struct State {
    directory: PathBuf, transport: Mutex<Transport>, boot: String, build: String,
    capture: Mutex<CaptureState>, delivery_gate: tokio::sync::Mutex<()>,
    modules: Mutex<HashMap<PathBuf, String>>,
    process: ProcessIdentity,
    _spool_lock: Option<std::fs::File>,
    health: Mutex<dispatch::Pending>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Context {
    stream: Stream, labels: Vec<String>, run_id: Option<String>,
    kernel: Option<String>, platform: Option<String>, clock: String,
}
impl Default for Context {
    fn default() -> Self { Self { stream:Stream::Production, labels:vec![], run_id:None, kernel:None, platform:None, clock:"boottime".into() } }
}
struct CaptureState { enabled: bool, context: Context, since_us: u64, generation: u32 }
impl Default for CaptureState {
    fn default() -> Self { Self { enabled: true, context: Context::default(), since_us: 0, generation: 0 } }
}
fn boot_us() -> io::Result<u64> {
    let mut time = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut time) } != 0 { return Err(io::Error::last_os_error()); }
    Ok(time.tv_sec as u64 * 1_000_000 + time.tv_nsec as u64 / 1000)
}
/// Process incarnation within `boot`; captured once, never from the sampler.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "ProcessIdentityFields")]
struct ProcessIdentity { pid: u32, start_ticks: u64 }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessIdentityFields { pid: u32, start_ticks: u64 }
impl TryFrom<ProcessIdentityFields> for ProcessIdentity {
    type Error = &'static str;
    fn try_from(value: ProcessIdentityFields) -> Result<Self, Self::Error> {
        if value.pid == 0 || value.start_ticks == 0 || value.start_ticks > i64::MAX as u64 {
            return Err("invalid diagnostic process identity");
        }
        Ok(Self { pid: value.pid, start_ticks: value.start_ticks })
    }
}
fn process_identity(bytes: &[u8], expected_pid: u32) -> io::Result<ProcessIdentity> {
    let invalid = || io::Error::other("invalid diagnostic process stat");
    if bytes.len() > 4096 { return Err(invalid()); }
    let stat = std::str::from_utf8(bytes).map_err(|_| invalid())?;
    let (pid, rest) = stat.split_once(' ').ok_or_else(invalid)?;
    if !rest.starts_with('(') || pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) { return Err(invalid()); }
    // comm may contain spaces and ')'; all fields after its final ')' are scalar.
    let (_, fields) = rest.rsplit_once(')').ok_or_else(invalid)?;
    if !fields.starts_with(' ') { return Err(invalid()); }
    let mut fields = fields.split_ascii_whitespace();
    let state = fields.next().ok_or_else(invalid)?;
    if state.len() != 1 || !state.as_bytes()[0].is_ascii_alphabetic() { return Err(invalid()); }
    let ticks = fields.nth(18).ok_or_else(invalid)?; // field 22, after state (field 3)
    if ticks.is_empty() || !ticks.bytes().all(|b| b.is_ascii_digit()) { return Err(invalid()); }
    let identity = ProcessIdentity::try_from(ProcessIdentityFields {
        pid: pid.parse().map_err(|_| invalid())?, start_ticks: ticks.parse().map_err(|_| invalid())?,
    }).map_err(|_| invalid())?;
    if identity.pid != expected_pid { return Err(invalid()); }
    Ok(identity)
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct Frame { module: String, build: String, offset: u64 }
#[derive(Serialize, Deserialize, Debug)]
#[serde(deny_unknown_fields)]
struct Report {
    id: String, build: String, boot: String, runtime: String, kind: String,
    thread: u32, task: u32, poll: u32, monotonic_us: u64, elapsed_us: u64,
    dropped_reports: u32,
    frames: Vec<Frame>, stack_status: String,
    context: Context,
    process: ProcessIdentity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    event: Option<crash::Event>,
}

fn bounded(_blocking: &Blocking, path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    let mut file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
    if !file.metadata()?.is_file() { return Err(io::Error::other("nonregular diagnostic input")); }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file).take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit { return Err(io::Error::other("diagnostic input bound")); }
    Ok(bytes)
}
fn hash(blocking: &Blocking, path: &Path) -> io::Result<String> {
    // Actual deployed bytes, not a version string. Chunked so hashing a module
    // never allocates its whole text image. Call only on ordinary owners.
    let _ = blocking;
    hash_file(std::fs::File::open(path)?)
}
fn hash_file(mut file: std::fs::File) -> io::Result<String> {
    if !file.metadata()?.is_file() || file.metadata()?.len() > 128 * 1024 * 1024 { return Err(io::Error::other("module bound")); }
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buffer = [0u8; 16384];
    loop { let n = file.read(&mut buffer)?; if n == 0 { break; } hash.update(&buffer[..n]); }
    Ok(hash.finish().as_ref().iter().map(|b| format!("{b:02x}")).collect())
}
fn uuid() -> String {
    let mut bytes = [0u8; 16]; fastrand::fill(&mut bytes);
    bytes[6] = (bytes[6] & 15) | 64; bytes[8] = (bytes[8] & 63) | 128;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..])
}
fn report_name(name: &str) -> bool {
    let Some(id) = name.strip_suffix(".json") else { return false; };
    id.len() == 36 && id.bytes().enumerate().all(|(i, b)| if [8,13,18,23].contains(&i) { b == b'-' } else { b.is_ascii_hexdigit() && !b.is_ascii_uppercase() })
}
fn pending(_blocking: &Blocking, directory: &Path) -> io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_name().to_str().is_some_and(report_name) {
            if !entry.file_type()?.is_file() { return Err(io::Error::other("nonregular outbox entry")); }
            paths.push(entry.path());
            if paths.len() > LIMIT { return Err(io::Error::other("outbox capacity")); }
        }
    }
    paths.sort(); Ok(paths)
}
fn retain(blocking: &Blocking, directory: &Path, report: &Report) -> io::Result<()> {
    let mut occupied = 0;
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if entry.file_name().to_str().is_some_and(|name| report_name(name) ||
            name.strip_suffix(".pending").is_some_and(|id| report_name(&format!("{id}.json")))) {
            occupied += 1;
            let capacity = if matches!(report.event, Some(crash::Event::AppPanic {} | crash::Event::FatalSignal {..} | crash::Event::ProcessExit {..})) { LIMIT } else { LIMIT - 16 };
            if occupied >= capacity { return Err(io::Error::other("outbox full")); }
        }
    }
    let _ = blocking;
    let body = serde_json::to_vec(report)?;
    if body.len() > REPORT_BYTES { return Err(io::Error::other("report bound")); }
    let temporary = directory.join(format!("{}.pending", report.id));
    let final_path = directory.join(format!("{}.json", report.id));
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary)?;
    let result = (|| {
        file.write_all(&body)?; file.sync_all()?;
        // A hard link publishes atomically without replacing a colliding ID.
        std::fs::hard_link(&temporary, &final_path)?;
        std::fs::File::open(directory)?.sync_all()
    })();
    let cleanup = std::fs::remove_file(&temporary);
    result.and(cleanup)
}

fn retain_once(blocking: &Blocking, directory: &Path, report: &Report) -> io::Result<()> {
    let path=directory.join(format!("{}.json",report.id));
    match bounded(blocking,&path,REPORT_BYTES) {
        Ok(bytes) if bytes==serde_json::to_vec(report)? => Ok(()),
        Err(error) if error.kind()==io::ErrorKind::NotFound => {
            // Only this deterministic imported identity is owned by this
            // serialized importer. An interrupted unpublished write is retryable.
            let temporary=directory.join(format!("{}.pending",report.id));
            match std::fs::symlink_metadata(&temporary) {
                Ok(metadata) if metadata.is_file()=>std::fs::remove_file(temporary)?,
                Ok(_)=>return Err(io::Error::other("nonregular imported report")),
                Err(error) if error.kind()==io::ErrorKind::NotFound=>{},
                Err(error)=>return Err(error),
            }
            retain(blocking,directory,report)
        },
        _ => Err(io::Error::other("telemetry import identity conflict")),
    }
}

/// Caller serializes capture/reconfiguration. Full optional storage retains
/// crash journals for retry; it must never disable delivery of that backlog.
fn import_crashes(blocking: &Blocking, state: &State) -> io::Result<()> {
    let directory=state.directory.parent().ok_or_else(||io::Error::other("outbox parent missing"))?.join("crashes");
    let collected=crash::collect(blocking,&directory,&state.boot,state.process)?;
    FAILURES.fetch_add(collected.rejected_files as u32,Ordering::Relaxed);
    for pending in collected.files {
        for (id,event) in &pending.events {
            let report=Report {id:id.clone(),build:pending.header.build.clone(),boot:pending.header.boot.clone(),
                process:pending.header.process,context:pending.header.context.clone(),event:Some(event.clone()),
                runtime:"process".into(),kind:"crash".into(),thread:0,task:0,poll:0,monotonic_us:0,
                elapsed_us:0,dropped_reports:0,frames:vec![],stack_status:"not_requested".into()};
            retain_once(blocking,&state.directory,&report)?;
        }
        crash::acknowledge(blocking,&pending)?;
    }
    Ok(())
}

/// Launcher-owned termination evidence: never infer a signal from shell status,
/// initialize a sender, or load user settings in this short-lived helper.
pub fn record_process_exit(blocking: &Blocking, directory: &Path, pid: u32, start: u64,
    status: u32, reason: &str) -> io::Result<bool> {
    let boot=String::from_utf8(bounded(blocking,Path::new("/proc/sys/kernel/random/boot_id"),64)?)
        .map_err(|_|io::Error::other("invalid boot identity"))?;
    let prepared=supervisor::prepare_exit(blocking,directory,boot.trim(),pid,start,status,reason.parse()?)?;
    let supervisor::Preparation::Ready(exit)=prepared else { return Ok(false); };
    let report=Report {id:exit.id,build:exit.header.build,boot:exit.header.boot,process:exit.header.process,
        context:exit.header.context,event:Some(exit.event),runtime:"process".into(),kind:"process-exit".into(),
        thread:0,task:0,poll:0,monotonic_us:0,elapsed_us:0,dropped_reports:0,frames:vec![],stack_status:"not_requested".into()};
    let outbox=directory.join("outbox-v2");
    if !std::fs::symlink_metadata(&outbox)?.file_type().is_dir() {return Err(io::Error::other("invalid telemetry outbox"));}
    retain_once(blocking,&outbox,&report)?;
    Ok(true)
}

fn transport_for(blocking: &Blocking, directory: &Path, settings: &Telemetry) -> io::Result<Transport> {
    let provisioned = if settings.endpoint.is_none() || settings.token.is_none() {
        Some(serde_json::from_slice::<Transport>(&bounded(blocking, &directory.join("transport.json"), 4096)?)?)
    } else { None };
    let endpoint = settings.endpoint.clone().or_else(|| provisioned.as_ref().map(|p|
        p.endpoint.trim_end_matches('/').to_owned())).unwrap();
    let token = settings.token.clone().or_else(|| provisioned.map(|p|p.token)).unwrap();
    Telemetry { endpoint: Some(endpoint.clone()), token: Some(token.clone()), ..Telemetry::default() }.validate()?;
    Ok(Transport { endpoint, token })
}

fn discard_optional(blocking: &Blocking, directory: &Path) -> io::Result<()> {
    let metadata = match std::fs::symlink_metadata(directory) {
        Ok(value) => value, Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()), Err(error) => return Err(error),
    };
    if !metadata.is_dir() || metadata.file_type().is_symlink() { return Err(io::Error::other("invalid telemetry outbox")); }
    let _ = blocking;
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if report_name(&name) || name.strip_suffix(".pending").is_some_and(|id|report_name(&format!("{id}.json"))) {
            if !entry.file_type()?.is_file() { return Err(io::Error::other("nonregular optional report")); }
            std::fs::remove_file(entry.path())?;
        }
    }
    std::fs::File::open(directory)?.sync_all()
}

/// Apply an effective configuration at startup or an acknowledged test boundary.
/// The application retains its original main Config; overlays never reach saves.
pub fn reconfigure(blocking: &Blocking, directory: &Path, settings: &Telemetry) -> io::Result<()> {
    settings.validate()?;
    let outbox = directory.join("outbox-v2");
    if !settings.enabled && STATE.get().is_none() {
        crash::disable();
        discard_optional(blocking, &outbox)?;
        crash::discard(blocking, &directory.join("crashes"))?;
        // A stale enabled marker must not attribute a later off-session crash
        // to its predecessor. Mandatory recovery archives are separate.
        match std::fs::remove_file(directory.join("capture-context.json")) {
            Ok(()) => std::fs::File::open(directory)?.sync_all(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()), Err(error) => Err(error),
        }?;
        return Ok(());
    }
    let transport = settings.enabled.then(|| transport_for(blocking, directory, settings)).transpose()?;
    let context = Context { stream: settings.stream, labels: settings.capture_labels(), run_id: settings.run_id.clone(),
        kernel: None, platform: None, clock: "boottime".into() };
    if STATE.get().is_none() {
        initialize(blocking, outbox, transport.clone().unwrap(), context.clone())?;
    }
    let state = STATE.get().ok_or_else(||io::Error::other("telemetry unavailable"))?;
    // Acknowledgement waits for any earlier upload/capture to finish. No report
    // captured before this boundary may acquire the new context afterward.
    let _delivery = state.delivery_gate.blocking_lock();
    let mut capture = state.capture.lock().map_err(|_|io::Error::other("telemetry context unavailable"))?;
    if capture.enabled == settings.enabled && capture.context == context &&
        transport.as_ref().is_none_or(|value|state.transport.lock().is_ok_and(|old|*old == *value)) {
        return Ok(());
    }
    ACTIVE.store(false, Ordering::Release);
    unsafe { q2_stall_collecting(0); }
    crash::disable();
    capture.enabled = false;
    // Only this whitelist persists for future crash attribution; no routing,
    // token, media or owner config is copied into crash context.
    crate::config::write_atomic(blocking, &directory.join("capture-context.json"),
        &serde_json::to_vec(&serde_json::json!({"schema":2,"enabled":settings.enabled,
            "boot":state.boot,"build":state.build,"process":state.process,"context":context}))?)?;
    if !settings.enabled {
        state.health.lock().map_err(|_|io::Error::other("telemetry queue unavailable"))?.clear();
        discard_optional(blocking, &state.directory)?;
        crash::discard(blocking, &directory.join("crashes"))?;
    }
    if let Some(transport) = transport {
        *state.transport.lock().map_err(|_|io::Error::other("telemetry transport unavailable"))? = transport;
    }
    if settings.enabled {
        let crash_directory = directory.join("crashes");
        if import_crashes(blocking,state).is_err() {FAILURES.fetch_add(1,Ordering::Relaxed);}
        let crash_ready=(|| -> io::Result<()> {
            let coverage=crash::install_handlers(blocking)?;
            crate::config::write_atomic(blocking,&directory.join("crash-coverage.json"),&serde_json::to_vec(&coverage)?)?;
            crash::enable(blocking,&crash_directory,crash::Header {build:state.build.clone(),boot:state.boot.clone(),
                process:state.process,context:context.clone()})
        })();
        if crash_ready.is_err() {FAILURES.fetch_add(1,Ordering::Relaxed);}
    }
    capture.context = context;
    capture.since_us = boot_us()?;
    capture.enabled = settings.enabled;
    capture.generation = unsafe { q2_stall_collecting(i32::from(settings.enabled)) };
    if capture.generation == 0 {
        capture.enabled = false;
        return Err(io::Error::other("telemetry capture generation exhausted"));
    }
    ACTIVE.store(settings.enabled, Ordering::Release);
    WAKE.notify_one();
    Ok(())
}

fn initialize(blocking: &Blocking, directory: PathBuf, transport: Transport, context: Context) -> io::Result<()> {
    let boot = String::from_utf8(bounded(blocking, Path::new("/proc/sys/kernel/random/boot_id"), 64)?)
        .map_err(io::Error::other)?.trim().to_owned();
    if !report_name(&format!("{boot}.json")) { return Err(io::Error::other("invalid boot identity")); }
    let build = identity::executable(blocking)?;
    let process = process_identity(&bounded(blocking, Path::new("/proc/self/stat"), 4096)?, std::process::id())?;
    std::fs::create_dir_all(&directory)?;
    if std::fs::symlink_metadata(&directory)?.file_type().is_symlink() { return Err(io::Error::other("outbox symlink")); }
    let spool_lock=std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600)
        .custom_flags(libc::O_NOFOLLOW|libc::O_NONBLOCK).open(directory.parent().unwrap().join("telemetry.lock"))?;
    use std::os::fd::AsRawFd;
    if !spool_lock.metadata()?.is_file() || unsafe {libc::flock(spool_lock.as_raw_fd(),libc::LOCK_EX|libc::LOCK_NB)}!=0 {
        return Err(io::Error::other("telemetry spool already owned"));
    }
    let (writer, reader) = UnixDatagram::pair()?;
    writer.set_nonblocking(true)?; reader.set_nonblocking(true)?;
    let state = Arc::new(State { directory, transport: Mutex::new(transport), boot, build, process, modules: Mutex::default(),
        capture: Mutex::new(CaptureState { enabled: false, context, since_us: boot_us()?, generation: 0 }), delivery_gate: tokio::sync::Mutex::new(()),_spool_lock:Some(spool_lock),health:Mutex::default() });
    // Process-lifetime descriptors: the sampler has no route to ordinary app
    // data, owns its signal, and exits with the process. Never reused after close.
    let writer = writer.into_raw_fd(); let reader = reader.into_raw_fd();
    if unsafe { q2_stall_start(writer, reader, captured) } != 0 {
        // Startup failure creates no observer and restores the signal handler.
        unsafe { libc::close(writer); libc::close(reader); }
        return Err(io::Error::other("diagnostic sampler refused startup"));
    }
    STATE.set(state).map_err(|_|io::Error::other("diagnostics already initialized"))?;
    Ok(())
}

pub fn register(role: u32) {
    unsafe { q2_stall_register(role); }
}

/// Admissions waiting for a busy capture boundary on a blocking worker.
static DEFERRED: AtomicU32 = AtomicU32::new(0);
const DEFERRED_LIMIT: u32 = 16;

/// Admission without a filesystem or contended wait on the caller. `make`
/// builds the report under the capture boundary, or declines it. When a
/// delivery write holds that boundary, a blocking worker finishes the
/// admission instead of dropping the report: the V0.107 device trace lost its
/// slow-journey reports to exactly that overlap.
fn admit_with(make: impl FnOnce(&Arc<State>, &CaptureState) -> Option<Report> + Send + 'static) -> bool {
    if !ACTIVE.load(Ordering::Acquire) { return false; }
    let Some(state)=STATE.get() else {return false;};
    match state.capture.try_lock() {
        Ok(capture) => match make(state,&capture) { Some(report)=>dispatch::submit(state,report), None=>false },
        Err(std::sync::TryLockError::WouldBlock) if DEFERRED.fetch_add(1,Ordering::AcqRel)<DEFERRED_LIMIT => {
            drop(crate::rt::spawn_blocking(move |blocking| admit_deferred(blocking, make)));
            true
        }
        Err(std::sync::TryLockError::WouldBlock) => {
            DEFERRED.fetch_sub(1,Ordering::AcqRel);
            FAILURES.fetch_add(1,Ordering::Relaxed);false
        }
        Err(_) => {FAILURES.fetch_add(1,Ordering::Relaxed);false}
    }
}

fn admit_deferred(_blocking: &Blocking, make: impl FnOnce(&Arc<State>, &CaptureState) -> Option<Report>) {
    struct Release;
    impl Drop for Release { fn drop(&mut self) { DEFERRED.fetch_sub(1,Ordering::AcqRel); } }
    let _release=Release;
    let Some(state)=STATE.get() else {return;};
    let Ok(capture)=state.capture.lock() else {FAILURES.fetch_add(1,Ordering::Relaxed);return;};
    if let Some(report)=make(state,&capture) { let _=dispatch::submit(state,report); }
}

/// Optional numeric observation admission. Context is fixed at admission,
/// before deferred persistence can run.
pub(super) fn admit_health(metric: health::Metric, value: u64, time: u64) -> bool {
    admit_with(move |state,capture| {
        if !capture.enabled {return None;}
        if time<capture.since_us || !metric.valid(value) {FAILURES.fetch_add(1,Ordering::Relaxed);return None;}
        let event = if metric.distribution() {
            crash::Event::Distribution { metric, sketch: ddsketch::Sketch::sample(value).expect("validated duration"), last_us: time }
        } else { crash::Event::Health { metric, value } };
        Some(Report {id:uuid(),build:state.build.clone(),boot:state.boot.clone(),process:state.process,
            context:capture.context.clone(),event:Some(event),runtime:"process".into(),
            kind:"health".into(),thread:0,task:0,poll:0,monotonic_us:time,elapsed_us:0,
            dropped_reports:FAILURES.load(Ordering::Relaxed),frames:vec![],stack_status:"not_requested".into()})
    })
}

/// A memory sample (platform/memory.rs) through the same opt-in, bounded
/// admission as health: one report per screen-off heap trim.
pub(crate) fn admit_memory(sample: crate::platform::memory::Sample, time: u64) -> bool {
    admit_with(move |state,capture| {
        if !capture.enabled {return None;}
        if time<capture.since_us {FAILURES.fetch_add(1,Ordering::Relaxed);return None;}
        Some(Report {id:uuid(),build:state.build.clone(),boot:state.boot.clone(),process:state.process,
            context:capture.context.clone(),event:Some(crash::Event::Memory {sample}),runtime:"process".into(),
            kind:"memory".into(),thread:0,task:0,poll:0,monotonic_us:time,elapsed_us:0,
            dropped_reports:FAILURES.load(Ordering::Relaxed),frames:vec![],stack_status:"not_requested".into()})
    })
}

/// A UX report through the same opt-in, bounded admission as health.
pub(crate) fn admit_ux(report: crate::ux::Report, time: u64) -> bool {
    admit_with(move |state,capture| {
        if !capture.enabled {return None;}
        if time<capture.since_us || report.recent.len()>64 || report.latency_us>86_400_000_000 {FAILURES.fetch_add(1,Ordering::Relaxed);return None;}
        let event=crash::Event::Ux {what:report.what,code:report.code,journey:report.journey,outcome:report.outcome,
            latency_us:report.latency_us,detail:report.detail,state:report.state,recent:report.recent,bluetooth:report.bluetooth};
        Some(Report {id:uuid(),build:state.build.clone(),boot:state.boot.clone(),process:state.process,
            context:capture.context.clone(),event:Some(event),runtime:"process".into(),
            kind:"ux".into(),thread:0,task:0,poll:0,monotonic_us:time,elapsed_us:0,
            dropped_reports:FAILURES.load(Ordering::Relaxed),frames:vec![],stack_status:"not_requested".into()})
    })
}

/// A current collection-health observation, never attribution of an earlier
/// crash to this session. Raw recovery archives remain local and untouched.
pub fn observe_kernel_archive(blocking: &Blocking) {
    let Some(state)=STATE.get() else {return;};
    let Ok(capture)=state.capture.lock() else {return;};
    if !capture.enabled {return;}
    let observation=kernel::collect(blocking,Path::new("/state/crash"),&state.boot);
    let observation=match observation {
        Ok(Some(value))=>value, Ok(None)=>return,
        Err(_)=>{FAILURES.fetch_add(1,Ordering::Relaxed);return;},
    };
    // Archive identity does not identify the current observation or its context.
    let _archive_identity=observation.id;
    let report=Report {id:uuid(),build:state.build.clone(),boot:state.boot.clone(),process:state.process,
        context:capture.context.clone(),event:Some(crash::Event::KernelArchiveObserved {
            records:observation.records,bytes:observation.bytes,collecting_boot:observation.collecting_boot,
            source_boot:None,source_context:None}),runtime:"kernel".into(),kind:"kernel-archive-observed".into(),
        thread:0,task:0,poll:0,monotonic_us:boot_us().unwrap_or(0),elapsed_us:0,
        dropped_reports:FAILURES.load(Ordering::Relaxed),frames:vec![],stack_status:"not_requested".into()};
    if retain(blocking,&state.directory,&report).is_err() {FAILURES.fetch_add(1,Ordering::Relaxed);}
    else {WAKE.notify_one();}
}
pub struct PollScope(u32);
/// Preserve a numeric future-type fingerprint before runtime boxing erases it.
/// This is an attribution hint within an exact build, never a unique identity.
pub fn task_tag(name: &str) -> u32 {
    name.bytes().fold(2166136261u32, |hash, byte| (hash ^ u32::from(byte)).wrapping_mul(16777619)) | 0x80000000
}
impl PollScope {
    pub fn enter(task: u32) -> Self {
        Self(if ACTIVE.load(Ordering::Acquire) { unsafe { q2_stall_enter(task) } } else { 0 })
    }
}
impl Drop for PollScope { fn drop(&mut self) { if self.0 != 0 { unsafe { q2_stall_leave(self.0); } } } }
pub struct WallScope { started: Option<Instant>, role: u32, task: u32, ticket: u32, generation: u32 }
impl WallScope {
    pub fn new(role: u32, task: u32) -> Self {
        let started = ACTIVE.load(Ordering::Acquire).then(Instant::now);
        Self { started, role, task, generation: unsafe { q2_stall_generation() }, ticket: if started.is_some() { unsafe { q2_stall_wall_enter(task) } } else { 0 } }
    }
}
impl Drop for WallScope {
    fn drop(&mut self) {
        if self.ticket != 0 { unsafe { q2_stall_wall_leave(self.ticket); } }
        if let Some(start) = self.started {
            let ms = start.elapsed().as_millis().min(u128::from(u32::MAX)) as u32;
            if ms >= 250 { unsafe { q2_stall_note(self.role, self.task, ms, self.generation); } }
        }
    }
}

extern "C" fn captured(sample: *const Sample) {
    // Called by the independent observer, never from the signal handler or a
    // runtime. C owns a full aligned Sample until this callback returns.
    let sample = unsafe { *sample };
    let result = std::panic::catch_unwind(|| capture(&Blocking::dedicated_thread(), sample));
    if !matches!(result, Ok(Ok(()))) { FAILURES.fetch_add(1, Ordering::Relaxed); }
}
fn capture(blocking: &Blocking, sample: Sample) -> io::Result<()> {
    let state = STATE.get().ok_or_else(|| io::Error::other("diagnostics unavailable"))?;
    if sample.version != 2 || !(1..=3).contains(&sample.role) || sample.count > 48 { return Err(io::Error::other("invalid sample")); }
    let context = state.capture.lock().map_err(|_| io::Error::other("telemetry context unavailable"))?;
    if !context.enabled || sample.generation != context.generation || sample.monotonic_us < context.since_us { return Ok(()); }
    let frames = frames(blocking, state, &sample.pc[..sample.count as usize]).unwrap_or_default();
    let stack_status = if matches!(sample.status, -2000 | -2001) { "not_requested" }
        else if frames.is_empty() { "unavailable" }
        else if sample.status == 0 && frames.len() == sample.count as usize { "complete" } else { "truncated" };
    let report = Report { id: uuid(), build: state.build.clone(), boot: state.boot.clone(),
        runtime: ["ui", "io", "player"][sample.role as usize - 1].into(),
        kind: match sample.status { -2000 => "completed", -2001 => "waiting", _ => "stall" }.into(),
        thread: sample.tid, task: sample.task, poll: sample.sequence,
        monotonic_us: sample.monotonic_us, elapsed_us: u64::from(sample.elapsed_ms)*1000,
        dropped_reports: FAILURES.load(Ordering::Relaxed).saturating_add(unsafe {q2_stall_dropped()}),
        frames, stack_status: stack_status.into(), process: state.process, context: context.context.clone(), event:None };
    retain(blocking, &state.directory, &report)?; WAKE.notify_one(); Ok(())
}

fn frames(blocking: &Blocking, state: &State, pcs: &[u64]) -> io::Result<Vec<Frame>> {
    if pcs.is_empty() { return Ok(Vec::new()); }
    use std::os::unix::fs::{FileExt, MetadataExt};
    let mut executable = std::fs::File::open("/proc/self/exe")?;
    let metadata = executable.metadata()?;
    let executable_device = (libc::major(metadata.dev()), libc::minor(metadata.dev()));
    let maps = String::from_utf8(bounded(blocking, Path::new("/proc/self/maps"), 256*1024)?).map_err(io::Error::other)?;
    let mut cache = state.modules.lock().map_err(|_| io::Error::other("module cache unavailable"))?;
    let mut frames = Vec::new();
    for pc in pcs {
        for line in maps.lines() {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len()!=6 || !fields[1].contains('x') { continue; }
            let Some((start,end))=fields[0].split_once('-') else {continue;};
            let (Ok(start),Ok(end),Ok(file_offset))=(u64::from_str_radix(start,16),u64::from_str_radix(end,16),u64::from_str_radix(fields[2],16)) else {continue;};
            if *pc < start || *pc >= end {continue;}
            let device = fields[3].split_once(':').and_then(|(major, minor)|
                Some((u32::from_str_radix(major,16).ok()?, u32::from_str_radix(minor,16).ok()?)));
            let main = device == Some(executable_device) && fields[4].parse::<u64>().ok() == Some(metadata.ino());
            let path=Path::new(fields[5]);
            let Some(name)=path.file_name().and_then(|n|n.to_str()) else {continue;};
            if !main && !(name.starts_with("lib") && name.contains(".so") && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"_.+-".contains(&b))) {continue;}
            let mut header=[0u8;64];
            let mut module;
            let elf=if main {&mut executable} else {module=std::fs::File::open(path)?; &mut module};
            elf.read_exact_at(&mut header, 0)?;
            if &header[..4]!=b"\x7fELF" || header[5]!=1 {continue;}
            // ET_EXEC uses absolute virtual addresses. ET_DYN uses the load
            // segment's virtual address; identify it from program headers.
            let offset=if u16::from_le_bytes([header[16],header[17]])==2 {*pc}
                else { match elf_address(blocking,elf,&header,start,file_offset,*pc) {Some(address)=>address,None=>continue} };
            let build=if main {state.build.clone()} else {
                if !cache.contains_key(path) {
                    if cache.len()>=64 {continue;}
                    cache.insert(path.to_path_buf(),hash(blocking,path)?);
                }
                cache[path].clone()
            };
            frames.push(Frame{module:if main {"q2nd"}else{name}.into(),build,offset}); break;
        }
    }
    Ok(frames)
}
fn elf_address(_blocking: &Blocking, file: &mut std::fs::File, header: &[u8;64], start: u64, offset: u64, pc: u64) -> Option<u64> {
    use std::io::{Seek,SeekFrom};
    let u16at=|p|u16::from_le_bytes(header[p..p+2].try_into().unwrap());
    let u32at=|p|u32::from_le_bytes(header[p..p+4].try_into().unwrap()) as u64;
    let u64at=|p|u64::from_le_bytes(header[p..p+8].try_into().unwrap());
    let (at,size,count)=match header[4] {1=>(u32at(28),u16at(42),u16at(44)),2=>(u64at(32),u16at(54),u16at(56)),_=>return None};
    if count>64 || size>64 || size<(if header[4]==2 {56}else{32}) || at>1024*1024 {return None;}
    for i in 0..count {
        file.seek(SeekFrom::Start(at+u64::from(i)*u64::from(size))).ok()?;
        let mut entry=[0u8;64];file.read_exact(&mut entry[..usize::from(size)]).ok()?;
        if u32::from_le_bytes(entry[..4].try_into().ok()?)!=1 {continue;}
        let u32at=|p|u32::from_le_bytes(entry[p..p+4].try_into().unwrap()) as u64;
        let u64at=|p|u64::from_le_bytes(entry[p..p+8].try_into().unwrap());
        let (fileoff,virtual_address,length)=if header[4]==1 {(u32at(4),u32at(8),u32at(16))}else{(u64at(8),u64at(16),u64at(32))};
        if offset>=fileoff/4096*4096 && offset<fileoff.saturating_add(length).saturating_add(4095)/4096*4096 {
            let address=i128::from(pc)-i128::from(start)+i128::from(offset)+i128::from(virtual_address)-i128::from(fileoff);
            return u64::try_from(address).ok();
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (PathBuf, Blocking) {
        let path=crate::library::generated_test_directory("incident-outbox");
        std::fs::create_dir_all(&path).unwrap();
        (path,Blocking::dedicated_thread())
    }
    fn report() -> Report {
        Report {id:uuid(),build:"a".repeat(64),boot:"11111111-1111-4111-8111-111111111111".into(),
            runtime:"ui".into(),kind:"waiting".into(),thread:1,task:102,poll:1,
            monotonic_us:900000,elapsed_us:500000,dropped_reports:0,frames:Vec::new(),stack_status:"not_requested".into(),process:ProcessIdentity{pid:1,start_ticks:1},context:Context::default(),event:None}
    }
    #[test]
    fn process_stat_uses_start_ticks_after_parenthesized_comm_and_refuses_malformed_identity() {
        let stat = |pid: &str, ticks: &str| format!("{pid} (generated ) (name)) S {}{ticks} 0 0\n", "0 ".repeat(18));
        assert_eq!(process_identity(stat("42", "123456").as_bytes(),42).unwrap(),ProcessIdentity{pid:42,start_ticks:123456});
        for bytes in [stat("0","123"),stat("43","123"),stat("+42","123"),stat("42","0"),
            stat("42","-1"),stat("42","+1"),stat("42","9223372036854775808"),
            stat("42","18446744073709551616"),"42 (x) S 0".into(),"x".repeat(4097)] {
            assert!(process_identity(bytes.as_bytes(),42).is_err());
        }
        assert!(process_identity(&[255],42).is_err());
    }
    #[test]
    fn report_schema_requires_process_and_context_and_refuses_invalid_identity() {
        let mut value=serde_json::to_value(report()).unwrap();
        for field in ["process","context"] {
            let mut missing=value.clone();missing.as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<Report>(missing).is_err());
        }
        for process in [serde_json::Value::Null,serde_json::json!({}),serde_json::json!({"pid":1}),
            serde_json::json!({"pid":1,"start_ticks":0}),serde_json::json!({"pid":0,"start_ticks":1}),
            serde_json::json!({"pid":true,"start_ticks":1}),serde_json::json!({"pid":4294967296u64,"start_ticks":1}),
            serde_json::json!({"pid":1,"start_ticks":9223372036854775808u64}),
            serde_json::json!({"pid":1,"start_ticks":1,"extra":0})] {
            value["process"]=process;
            assert!(serde_json::from_value::<Report>(value.clone()).is_err());
        }
        value["process"]=serde_json::json!({"pid":42,"start_ticks":123});
        let extended=serde_json::from_value::<Report>(value.clone()).unwrap();
        assert_eq!(serde_json::to_value(extended).unwrap(),value);
        let duplicate=serde_json::to_string(&value).unwrap().replace("\"pid\":42", "\"pid\":42,\"pid\":43");
        assert!(serde_json::from_str::<Report>(&duplicate).is_err());
    }
    #[test]
    #[cfg(target_os = "linux")]
    fn current_process_identity_is_stable_without_sampler_or_transport() {
        let blocking=Blocking::dedicated_thread();
        let read=|| process_identity(&bounded(&blocking,Path::new("/proc/self/stat"),4096).unwrap(),std::process::id()).unwrap();
        let first=read();assert!(first.start_ticks>0);assert_eq!(first.pid,std::process::id());assert_eq!(first,read());
    }
    #[test]
    fn atomic_outbox_survives_reopen_refuses_collision_and_never_sends_incomplete_records() {
        let (path,blocking)=fixture();
        let report=report();
        retain(&blocking,&path,&report).unwrap();
        let files=pending(&blocking,&path).unwrap();assert_eq!(files.len(),1);
        let bytes=bounded(&blocking,&files[0],REPORT_BYTES).unwrap();
        assert_eq!(serde_json::from_slice::<Report>(&bytes).unwrap().id,report.id);
        assert!(retain(&blocking,&path,&report).is_err());
        assert_eq!(bounded(&blocking,&files[0],REPORT_BYTES).unwrap(),bytes);
        let incomplete=path.join(format!("{}.pending",uuid()));
        std::fs::write(incomplete,b"partial crash write").unwrap();
        assert_eq!(pending(&blocking,&path).unwrap(),files);
        assert_eq!(std::fs::read_dir(&path).unwrap().count(),2,"failed publication cleans only its owned temporary");
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn capacity_includes_interrupted_writes_and_never_evicts_unacknowledged_incidents() {
        let (path,blocking)=fixture();
        for _ in 0..LIMIT {std::fs::write(path.join(format!("{}.pending",uuid())),b"partial").unwrap();}
        assert!(retain(&blocking,&path,&report()).is_err());
        assert_eq!(std::fs::read_dir(&path).unwrap().count(),LIMIT);
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn bounded_reads_refuse_symlinks_and_oversized_records() {
        let (path,blocking)=fixture();let regular=path.join("regular");
        std::fs::write(&regular,[0u8;65]).unwrap();
        assert!(bounded(&blocking,&regular,64).is_err());
        std::os::unix::fs::symlink(&regular,path.join("alias")).unwrap();
        assert!(bounded(&blocking,&path.join("alias"),128).is_err());
        assert_eq!(hash(&blocking,&regular).unwrap().len(),64);
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn delivery_yields_while_capture_persistence_holds_its_lock() {
        use std::{future::{Future, poll_fn}, task::Poll, sync::mpsc};
        let (path,_blocking)=fixture();
        let state=Arc::new(State{directory:path.clone(),transport:Mutex::new(Transport{endpoint:"http://127.0.0.1:1".into(),token:"a".repeat(64)}),capture:Mutex::new(CaptureState::default()),delivery_gate:tokio::sync::Mutex::new(()),boot:String::new(),build:String::new(),process:ProcessIdentity{pid:123,start_ticks:456},modules:Mutex::default(),_spool_lock:None,health:Mutex::default()});
        let (ready_tx,ready_rx)=mpsc::channel();
        let (yielded_tx,yielded_rx)=mpsc::channel();
        let persisting=state.clone();
        let holder=std::thread::spawn(move || {
            let _capture=persisting.capture.lock().unwrap();
            ready_tx.send(()).unwrap();
            // A bounded failure releases the old synchronous implementation;
            // success requires its actual poll to return before lock release.
            yielded_rx.recv_timeout(Duration::from_secs(2)).is_ok()
        });
        ready_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let agent=subsonic::http::AgentBuilder::new().build();
        let online=AtomicBool::new(true);
        let yielded=crate::rt::Ui::new().unwrap().block_on(async {
            let mut delivery=std::pin::pin!(deliver(&agent,state,&online));
            let pending=poll_fn(|cx| Poll::Ready(delivery.as_mut().poll(cx).is_pending())).await;
            let _=yielded_tx.send(());
            if pending {delivery.await.unwrap();}
            pending
        });
        let released_after_yield=holder.join().unwrap();
        std::fs::remove_dir_all(path).unwrap();
        assert!(yielded && released_after_yield,"delivery blocked its executor on capture persistence");
    }

    #[test]
    fn delivery_keeps_offline_and_unacknowledged_reports_then_retries_exact_ids() {
        let (path,blocking)=fixture();
        retain(&blocking,&path,&report()).unwrap();
        let mut extended=report();extended.process=ProcessIdentity{pid:123,start_ticks:456};
        retain(&blocking,&path,&extended).unwrap();
        let listener=std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint=format!("http://{}",listener.local_addr().unwrap());
        let server=std::thread::spawn(move || {
            let mut first=None;
            for attempt in 0..2 {
                let deadline=Instant::now()+Duration::from_secs(5);
                let mut socket=loop {
                    match listener.accept() {
                        Ok((socket,_))=>break socket,
                        Err(e) if e.kind()==io::ErrorKind::WouldBlock=>{
                            assert!(Instant::now()<deadline,"missing generated batch");
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        Err(e)=>panic!("{e}"),
                    }
                };
                socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut header=Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    let mut byte=[0];socket.read_exact(&mut byte).unwrap();header.push(byte[0]);assert!(header.len()<8192);
                }
                let header=String::from_utf8(header).unwrap().to_ascii_lowercase();
                assert!(header.starts_with("post /v2/production/reports "));
                assert!(header.contains(&format!("authorization: bearer {}", "a".repeat(64))));
                let length=header.lines().find_map(|line|line.strip_prefix("content-length: ")).unwrap().parse::<usize>().unwrap();
                assert!(length<=64*1024);
                let mut body=vec![0;length];socket.read_exact(&mut body).unwrap();
                if let Some(first)=&first {assert_eq!(&body,first,"retry must preserve the exact reports");} else {first=Some(body.clone());}
                let batch:serde_json::Value=serde_json::from_slice(&body).unwrap();
                let ids:Vec<_>=batch["reports"].as_array().unwrap().iter().map(|r|r["id"].clone()).collect();
                assert_eq!(ids.len(),2);
                let reports=batch["reports"].as_array().unwrap();
                assert!(reports.iter().all(|r|r.get("process").is_some()));
                assert_eq!(reports.iter().filter(|r|r["process"]==serde_json::json!({"pid":123,"start_ticks":456})).count(),1);
                let accepted=if attempt==0 {ids[..1].to_vec()} else {ids};
                let reply=serde_json::to_vec(&serde_json::json!({"schema":2,"stream":"production","accepted":accepted})).unwrap();
                write!(socket,"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",reply.len()).unwrap();
                socket.write_all(&reply).unwrap();
            }
        });
        let state=Arc::new(State{directory:path.clone(),transport:Mutex::new(Transport{endpoint,token:"a".repeat(64)}),capture:Mutex::new(CaptureState::default()),delivery_gate:tokio::sync::Mutex::new(()),boot:String::new(),build:String::new(),process:ProcessIdentity{pid:123,start_ticks:456},modules:Mutex::default(),_spool_lock:None,health:Mutex::default()});
        let agent=subsonic::http::AgentBuilder::new().try_proxy_from_env(false).timeout(Duration::from_secs(2)).build();
        let online=AtomicBool::new(false);
        crate::rt::Ui::new().unwrap().block_on(async {
            deliver(&agent,state.clone(),&online).await.unwrap();
            assert_eq!(pending(&blocking,&path).unwrap().len(),2);
            online.store(true,Ordering::Relaxed);
            assert!(deliver(&agent,state.clone(),&online).await.is_err());
            assert_eq!(pending(&blocking,&path).unwrap().len(),2,"partial acknowledgement cannot delete evidence");
            deliver(&agent,state.clone(),&online).await.unwrap();
            assert!(pending(&blocking,&path).unwrap().is_empty());
        });
        server.join().unwrap();std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn delayed_v2_reports_keep_capture_stream_through_current_production_delivery() {
        let (path, blocking) = fixture();
        let mut test = report();
        test.id = "00000000-0000-4000-8000-000000000001".into();
        test.context = Context { stream:Stream::AutomatedTest, labels:vec!["automated-test".into()],
            run_id:Some("00000000-0000-4000-8000-000000000003".into()),kernel:None,platform:None,clock:"boottime".into() };
        let mut production = report();
        production.id = "00000000-0000-4000-8000-000000000002".into();
        production.context = Context { stream:Stream::Production,labels:vec![],run_id:None,kernel:None,platform:None,clock:"boottime".into() };
        retain(&blocking, &path, &test).unwrap();
        retain(&blocking, &path, &production).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}",listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            for stream in [Stream::AutomatedTest, Stream::Production] {
                let deadline=Instant::now()+Duration::from_secs(5);
                let mut socket=loop { match listener.accept() {
                    Ok((socket,_))=>break socket,
                    Err(error) if error.kind()==io::ErrorKind::WouldBlock=>{
                        assert!(Instant::now()<deadline,"missing generated v2 batch");
                        std::thread::sleep(Duration::from_millis(1));
                    }, Err(error)=>panic!("{error}"),
                }};
                socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                let mut header=Vec::new();
                while !header.ends_with(b"\r\n\r\n") {
                    let mut byte=[0];socket.read_exact(&mut byte).unwrap();header.push(byte[0]);assert!(header.len()<8192);
                }
                let header=String::from_utf8(header).unwrap().to_ascii_lowercase();
                assert!(header.starts_with(&format!("post /v2/{}/reports ",stream.as_str())));
                let length=header.lines().find_map(|line|line.strip_prefix("content-length: ")).unwrap().parse::<usize>().unwrap();
                let mut body=vec![0;length];socket.read_exact(&mut body).unwrap();
                let batch:serde_json::Value=serde_json::from_slice(&body).unwrap();
                assert_eq!(batch["schema"],2);assert_eq!(batch["stream"],stream.as_str());
                let rows=batch["reports"].as_array().unwrap();assert_eq!(rows.len(),1);
                assert_eq!(rows[0]["context"]["stream"],stream.as_str());
                if stream==Stream::AutomatedTest {assert_eq!(rows[0]["context"]["run_id"],"00000000-0000-4000-8000-000000000003");}
                let reply=serde_json::to_vec(&serde_json::json!({"schema":2,"stream":stream,"accepted":[rows[0]["id"]]})).unwrap();
                write!(socket,"HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",reply.len()).unwrap();
                socket.write_all(&reply).unwrap();
            }
        });
        let state=Arc::new(State {directory:path.clone(),transport:Mutex::new(Transport{endpoint,token:"a".repeat(64)}),
            boot:String::new(),build:String::new(),process:ProcessIdentity{pid:1,start_ticks:1},modules:Mutex::default(),
            capture:Mutex::new(CaptureState {context:production.context,..CaptureState::default()}),delivery_gate:tokio::sync::Mutex::new(()),_spool_lock:None,health:Mutex::default()});
        let agent=subsonic::http::AgentBuilder::new().try_proxy_from_env(false).timeout(Duration::from_secs(2)).build();
        crate::rt::Ui::new().unwrap().block_on(async {
            deliver(&agent,state.clone(),&AtomicBool::new(true)).await.unwrap();
            assert_eq!(pending(&blocking,&path).unwrap().len(),1);
            deliver(&agent,state,&AtomicBool::new(true)).await.unwrap();
            assert!(pending(&blocking,&path).unwrap().is_empty());
        });
        server.join().unwrap();std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn opt_out_discards_optional_backlog_without_reading_transport() {
        let (path,blocking)=fixture();
        let optional=path.join("outbox-v2");
        std::fs::create_dir_all(&optional).unwrap();
        retain(&blocking,&optional,&report()).unwrap();
        std::fs::write(optional.join(format!("{}.pending",uuid())),b"partial").unwrap();
        std::fs::write(path.join("transport.json"),b"unread-invalid-provisioning").unwrap();
        discard_optional(&blocking,&optional).unwrap();
        assert_eq!(std::fs::read_dir(&optional).unwrap().count(),0);
        let transport=transport_for(&blocking,&path,&Telemetry {enabled:true,endpoint:Some("http://127.0.0.1:8766".into()),token:Some("a".repeat(64)),..Telemetry::default()}).unwrap();
        assert_eq!(transport.endpoint,"http://127.0.0.1:8766");
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn actual_capture_boundary_child() {
        if std::env::var_os("Q2_GENERATED_CONTEXT_CHILD").is_none() { return; }
        let (path,blocking)=fixture();
        register(1); // Register an owner while optional telemetry is still off.
        reconfigure(&blocking,&path,&Telemetry::default()).unwrap();
        assert!(STATE.get().is_none());
        let mut settings=Telemetry {enabled:true,stream:Stream::AutomatedTest,
            run_id:Some("00000000-0000-4000-8000-000000000003".into()),
            endpoint:Some("http://127.0.0.1:8766".into()),token:Some("a".repeat(64)),..Telemetry::default()};
        reconfigure(&blocking,&path,&settings).unwrap();
        let old_generation=unsafe { q2_stall_generation() };
        let sample=|generation|Sample {version:2,role:1,tid:1,task:1,sequence:1,elapsed_ms:500,count:0,status:-2000,
            generation,reserved:0,monotonic_us:boot_us().unwrap()+1_000_000,pc:[0;48]};
        capture(&blocking,sample(old_generation)).unwrap();
        settings.stream=Stream::Production;settings.run_id=None;
        reconfigure(&blocking,&path,&settings).unwrap();
        // A timestamp newer than the boundary does not rescue an old sample.
        capture(&blocking,sample(old_generation)).unwrap();
        capture(&blocking,sample(unsafe { q2_stall_generation() })).unwrap();
        let outbox=path.join("outbox-v2");
        let rows=pending(&blocking,&outbox).unwrap();assert_eq!(rows.len(),2);
        let mut streams:Vec<_>=rows.iter().map(|row|serde_json::from_slice::<Report>(&bounded(&blocking,row,REPORT_BYTES).unwrap()).unwrap().context.stream.as_str()).collect();
        streams.sort();assert_eq!(streams,["automated-test","production"]);
        // Hold the three existing job workers so the real metrics hook must
        // preserve admission context across a later switch, without a RAM sink.
        let (started,ready)=std::sync::mpsc::channel();
        let mut release=Vec::new();
        let mut workers=Vec::new();
        for _ in 0..3 {
            let (send,receive)=std::sync::mpsc::channel::<()>(); release.push(send);
            let started=started.clone();
            workers.push(crate::rt::jobs().run_isolated(move |_| {started.send(()).unwrap();let _=receive.recv();}));
        }
        for _ in 0..3 {ready.recv_timeout(Duration::from_secs(2)).unwrap();}
        crate::metrics::record_at(crate::metrics::Event::ScreenWake,Duration::from_micros(123),boot_us().unwrap());
        health::audio_error(libc::EBADFD);
        health::audio_recovery(libc::EBADFD, true);
        health::audio_recovery(libc::EBADF, false);
        settings.stream=Stream::AutomatedTest;settings.run_id=Some("00000000-0000-4000-8000-000000000004".into());
        reconfigure(&blocking,&path,&settings).unwrap();
        drop(release);
        crate::rt::Ui::new().unwrap().block_on(async {for worker in workers {worker.await.unwrap();}});
        let deadline=Instant::now()+Duration::from_secs(2);
        loop {
            let rows=pending(&blocking,&outbox).unwrap();
            let reports:Vec<_>=rows.iter().map(|row|serde_json::from_slice::<Report>(&bounded(&blocking,row,REPORT_BYTES).unwrap()).unwrap())
                .filter(|row|row.kind=="health").collect();
            if reports.len()==4 {
                assert!(reports.iter().all(|report|report.context.stream==Stream::Production));
                assert!(reports.iter().any(|report|matches!(report.event,Some(crash::Event::Distribution {metric:health::Metric::InputToPresent,ref sketch,..}) if sketch.valid() && sketch.count==1 && sketch.sum_us==123)));
                for (metric,value) in [(health::Metric::AudioError,libc::EBADFD),
                    (health::Metric::AudioRecoverySucceeded,libc::EBADFD),
                    (health::Metric::AudioRecoveryExhausted,libc::EBADF)] {
                    assert!(reports.iter().any(|report|matches!(report.event,
                        Some(crash::Event::Health {metric:actual,value:observed}) if actual==metric && observed==value as u64)));
                }
                break;
            }
            assert!(Instant::now()<deadline,"numeric hook did not persist without RAM recorder");
            std::thread::sleep(Duration::from_millis(5));
        }
        // A busy capture boundary defers admission to a worker; nothing is lost.
        let before=FAILURES.load(Ordering::Relaxed);
        let held=STATE.get().unwrap().capture.lock().unwrap();
        assert!(admit_health(health::Metric::FirstFrame,0,boot_us().unwrap()));
        drop(held);
        let deadline=Instant::now()+Duration::from_secs(2);
        while !pending(&blocking,&outbox).unwrap().iter().any(|row| matches!(serde_json::from_slice::<Report>(&bounded(&blocking,row,REPORT_BYTES).unwrap()).unwrap().event,
                Some(crash::Event::Health {metric:health::Metric::FirstFrame,..}))) {
            assert!(Instant::now()<deadline,"deferred admission did not persist");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(FAILURES.load(Ordering::Relaxed),before);
        reconfigure(&blocking,&path,&Telemetry::default()).unwrap();
        assert_eq!(unsafe { q2_stall_generation() },0);
        assert!(!ACTIVE.load(Ordering::Acquire));
        assert!(pending(&blocking,&outbox).unwrap().is_empty());
        let persisted:serde_json::Value=serde_json::from_slice(&std::fs::read(path.join("capture-context.json")).unwrap()).unwrap();
        assert_eq!(persisted["enabled"],false);
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn actual_capture_boundary_preserves_old_context_then_opt_out_discards_backlog() {
        let (path,_blocking)=fixture();
        let child=path.join("context-child");
        // The host's debug test executable exceeds the production module bound.
        // Exercise the real code as a stripped executable, like deployed Q2
        // bytes, without weakening the bound or hashing a substitute fixture.
        let stripped=std::process::Command::new("strip").arg("--strip-debug").arg("-o").arg(&child)
            .arg(std::env::current_exe().unwrap()).output().unwrap();
        assert!(stripped.status.success(),"could not prepare stripped context child");
        let result=std::process::Command::new(&child)
            .args(["--exact","diagnostics::tests::actual_capture_boundary_child","--nocapture"])
            .env("Q2_GENERATED_CONTEXT_CHILD","1").output().unwrap();
        std::fs::remove_dir_all(path).unwrap();
        assert!(result.status.success(),"generated context child failed: {}",String::from_utf8_lossy(&result.stderr));
    }
    #[test]
    fn dynamic_elf_addresses_account_for_nonzero_segment_virtual_addresses() {
        let (path,_blocking)=fixture();
        for class in [1u8,2] {
            let mut elf=vec![0u8;128];elf[..4].copy_from_slice(b"\x7fELF");elf[4]=class;elf[5]=1;
            if class==1 {
                elf[28..32].copy_from_slice(&64u32.to_le_bytes());
                elf[42..44].copy_from_slice(&32u16.to_le_bytes());elf[44..46].copy_from_slice(&1u16.to_le_bytes());
                elf[68..72].copy_from_slice(&0x2000u32.to_le_bytes());elf[72..76].copy_from_slice(&0x6000u32.to_le_bytes());
                elf[80..84].copy_from_slice(&0x3000u32.to_le_bytes());
            } else {
                elf[32..40].copy_from_slice(&64u64.to_le_bytes());
                elf[54..56].copy_from_slice(&56u16.to_le_bytes());elf[56..58].copy_from_slice(&1u16.to_le_bytes());
                elf[72..80].copy_from_slice(&0x2000u64.to_le_bytes());elf[80..88].copy_from_slice(&0x6000u64.to_le_bytes());
                elf[96..104].copy_from_slice(&0x3000u64.to_le_bytes());
            }
            elf[64..68].copy_from_slice(&1u32.to_le_bytes());
            let name=path.join(format!("elf{class}"));std::fs::write(&name,&elf).unwrap();
            let header=elf[..64].try_into().unwrap();let mut file=std::fs::File::open(name).unwrap();
            assert_eq!(elf_address(&_blocking,&mut file,&header,0x70000000,0x3000,0x70000123),Some(0x7123));
            assert_eq!(elf_address(&_blocking,&mut file,&header,0x70000000,0x9000,0x70000123),None);
        }
        std::fs::remove_dir_all(path).unwrap();
    }
}

pub fn online(online: bool) {
    ONLINE.store(online, Ordering::Relaxed);
    if online { WAKE.notify_one(); }
}
pub fn start_delivery() {
    crate::rt::io().spawn(async {
        let agent=subsonic::http::AgentBuilder::new().redirects(0).timeout(Duration::from_secs(5)).build();
        loop {
            if !ACTIVE.load(Ordering::Acquire) { WAKE.notified().await; continue; }
            if ONLINE.load(Ordering::Relaxed) {
                if let Some(state) = STATE.get() { let _=deliver(&agent,state.clone(),&ONLINE).await; }
            }
            tokio::select! { _=WAKE.notified()=>{}, _=tokio::time::sleep(Duration::from_secs(60))=>{} }
            if ACTIVE.load(Ordering::Acquire) { tokio::time::sleep(Duration::from_secs(5)).await; }
        }
    });
}
async fn deliver(agent: &subsonic::http::Agent, state: Arc<State>, online: &AtomicBool) -> io::Result<()> {
    let _gate = state.delivery_gate.lock().await;
    if !online.load(Ordering::Relaxed) {return Ok(());}
    let reading=state.clone();
    let batch=crate::rt::spawn_blocking(move |blocking| -> io::Result<Option<(Transport,Vec<(PathBuf,Report)>)>> {
        // Capture persistence holds this lock through filesystem writes. Wait
        // on the existing finite worker, never the delivery executor. The
        // async delivery gate still excludes context/transport reconfiguration.
        if !reading.capture.lock().map_err(|_|io::Error::other("telemetry context unavailable"))?.enabled {return Ok(None);}
        let transport=reading.transport.lock().map_err(|_|io::Error::other("telemetry transport unavailable"))?.clone();
        let mut rows=Vec::new();let mut bytes=0;
        for path in pending(blocking,&reading.directory)?.into_iter().take(16) {
            let body=bounded(blocking,&path,REPORT_BYTES)?;
            if bytes+body.len()>60*1024 {break;}
            let report:Report=serde_json::from_slice(&body)?;
            if path.file_name().and_then(|p|p.to_str())!=Some(&format!("{}.json",report.id)) {return Err(io::Error::other("report identity mismatch"));}
            // A delayed event retains its original stream. Send homogeneous
            // batches to the matching physical receiver store, never relabel.
            if let Some((_, first)) = rows.first() {
                let first: &Report = first;
                if first.context.stream != report.context.stream { continue; }
            }
            bytes+=body.len();rows.push((path,report));
        }
        Ok(Some((transport,rows)))
    }).await.map_err(|_|io::Error::other("diagnostic batch worker stopped"))??;
    let Some((transport,rows))=batch else {return Ok(());};
    if rows.is_empty() || !online.load(Ordering::Relaxed) {return Ok(());}
    let stream = rows[0].1.context.stream;
    let body=serde_json::to_vec(&serde_json::json!({"schema":2,"stream":stream,
        "reports":rows.iter().map(|(_,report)|report).collect::<Vec<_>>()}))?;
    let endpoint=format!("{}/v2/{}/reports",transport.endpoint.trim_end_matches('/'),stream.as_str());
    let reply=agent.post(&endpoint).set("Authorization",&format!("Bearer {}",transport.token))
        .set("Content-Type","application/json").send(body.into()).await.map_err(|_|io::Error::other("diagnostic delivery unavailable"))?;
    if reply.status()!=200 {return Err(io::Error::other("diagnostic delivery refused"));}
    #[derive(Deserialize)] #[serde(deny_unknown_fields)] struct Ack {schema:u32,accepted:Vec<String>,stream:Stream}
    let ack:Ack=serde_json::from_slice(&reply.bytes(4096).await?)?;
    let ids:Vec<_>=rows.iter().map(|(_,r)|r.id.clone()).collect();
    if ack.schema!=2 || ack.stream!=stream || ack.accepted!=ids {return Err(io::Error::other("diagnostic acknowledgement mismatch"));}
    let acknowledged = state.clone();
    crate::rt::spawn_blocking(move |blocking| -> io::Result<()> {
        for (path,_) in rows {std::fs::remove_file(path)?;}
        std::fs::File::open(&acknowledged.directory)?.sync_all()?;
        let capture=acknowledged.capture.lock().map_err(|_|io::Error::other("telemetry context unavailable"))?;
        if capture.enabled && import_crashes(blocking,&acknowledged).is_err() {FAILURES.fetch_add(1,Ordering::Relaxed);}
        Ok(())
    }).await.map_err(|_|io::Error::other("diagnostic acknowledgement worker stopped"))?
}

/// Explicit generated process only, before ordinary player initialization.
/// Exercises the real poll wrapper, observer, durable outbox and HTTP sender
/// without loading a library/configuration or touching display/audio hardware.
pub fn fixture(blocking: &Blocking, directory: PathBuf, mode: &str) -> io::Result<()> {
    if !matches!(mode,"capture"|"deliver") {return Err(io::Error::other("unknown fixture phase"));}
    let settings=crate::config::telemetry::load_overlay(blocking,&Telemetry::default(),&directory.join("telemetry-overlay.toml"))?;
    if !settings.enabled || settings.stream != Stream::AutomatedTest {return Err(io::Error::other("generated telemetry fixture context required"));}
    reconfigure(blocking,&directory,&settings)?;
    register(1);
    let state=STATE.get().ok_or_else(||io::Error::other("fixture state absent"))?.clone();
    let runtime=crate::rt::Ui::new()?;
    if mode=="capture" {
        if !pending(blocking,&state.directory)?.is_empty() {return Err(io::Error::other("fresh fixture required"));}
        let work=crate::rt::io().spawn(async {generated_blocked_poll();});
        runtime.block_on(async {
            generated_blocked_poll();
            work.await.map_err(|_|io::Error::other("fixture task stopped"))
        })?;
        let deadline=Instant::now()+Duration::from_secs(3);
        let reports=loop {
            let rows=pending(blocking,&state.directory)?;
            if rows.len()==2 {break rows;}
            if rows.len()>2 || Instant::now()>=deadline {return Err(io::Error::other("fixture capture incomplete"));}
            std::thread::sleep(Duration::from_millis(10));
        };
        let mut roles=Vec::new();let mut frames=0;
        for path in reports {
            let report:Report=serde_json::from_slice(&bounded(blocking,&path,REPORT_BYTES)?)?;
            if report.kind!="stall" || !matches!(report.stack_status.as_str(),"complete"|"truncated")
                || !report.frames.iter().any(|frame|frame.module=="q2nd" && frame.build==state.build) {
                return Err(io::Error::other("fixture execution stack unavailable"));
            }
            frames+=report.frames.len();roles.push(report.runtime);
        }
        roles.sort();if roles!=["io","ui"] {return Err(io::Error::other("fixture owner attribution mismatch"));}
        println!("{{\"captured\":2,\"resolved_frames\":{frames},\"offline_retained\":true}}");
    } else {
        let before=pending(blocking,&state.directory)?.len();
        if before!=2 {return Err(io::Error::other("fixture retained capture missing"));}
        let agent=subsonic::http::AgentBuilder::new().try_proxy_from_env(false).redirects(0).timeout(Duration::from_secs(5)).build();
        runtime.block_on(deliver(&agent,state.clone(),&AtomicBool::new(false)))?;
        if pending(blocking,&state.directory)?.len()!=before {return Err(io::Error::other("offline evidence changed"));}
        runtime.block_on(deliver(&agent,state.clone(),&AtomicBool::new(true)))?;
        if !pending(blocking,&state.directory)?.is_empty() {return Err(io::Error::other("fixture acknowledgement incomplete"));}
        println!("{{\"acknowledged\":{before},\"outbox_empty\":true}}");
    }
    Ok(())
}

/// Deliberate fault reached only by the explicit generated fixture above.
/// Keep a real Rust frame so local symbols can verify interrupted attribution.
#[inline(never)]
fn generated_blocked_poll() { std::thread::sleep(Duration::from_millis(600)); }
