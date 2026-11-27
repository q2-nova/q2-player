//! Bounded optional crash evidence with immutable pre-crash classification.
//!
//! Ordinary owners prepare and sync a safe header before publishing a slot.
//! Published descriptors and slots live until process exit (maximum 64). Fatal
//! handlers only select an immutable slot and pwrite a 16-byte numeric marker.
//! No text, address, allocation, lock, unwinding or network operation enters that
//! recording path. Marker writes are not a power-loss durability guarantee.
use super::{Context, ProcessIdentity, Stream};
use crate::rt::Blocking;
use serde::{Deserialize, Serialize};
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::IntoRawFd,
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Path, PathBuf},
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, AtomicPtr, AtomicU32, Ordering},
    },
};

const HEADER: usize = 4096;
const MARKER: usize = 16;
const FILE_BYTES: usize = HEADER + 2 * MARKER;
const MAX_CONTEXTS: u32 = 64;
const MAX_FILES: usize = 256;
static ACTIVE: AtomicPtr<Slot> = AtomicPtr::new(std::ptr::null_mut());
static SLOTS: AtomicU32 = AtomicU32::new(0);
static INSTALL: Mutex<()> = Mutex::new(());
static COVERAGE: OnceLock<Coverage> = OnceLock::new();
const SIGNALS: [i32; 5] = [
    libc::SIGABRT,
    libc::SIGILL,
    libc::SIGFPE,
    libc::SIGSEGV,
    libc::SIGBUS,
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub(super) enum Event {
    AppPanic {},
    FatalSignal {
        signal: u32,
        code: i32,
    },
    ProcessExit {
        wait_status: u32,
        reason: super::supervisor::Reason,
    },
    Health { metric: super::health::Metric, value: u64 },
    /// Memory pressure at a screen-off heap trim (platform/memory.rs).
    Memory { sample: crate::platform::memory::Sample },
    Distribution { metric: super::health::Metric, sketch: super::ddsketch::Sketch, last_us: u64 },
    /// UX error/recovery/journey/stop/stuck/problem report (crate::ux).
    Ux {
        what: crate::ux::What,
        code: u16,
        journey: u8,
        outcome: u8,
        latency_us: u64,
        detail: u32,
        state: crate::ux::Snapshot,
        recent: Vec<[u32; 3]>,
        /// While Bluetooth is on; absent from earlier reports.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bluetooth: Option<crate::ux::bluetooth::Bluetooth>,
    },
    KernelArchiveObserved {
        records: u32,
        bytes: u64,
        collecting_boot: String,
        source_boot: Option<String>,
        source_context: Option<Context>,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Header {
    pub build: String,
    pub boot: String,
    pub process: ProcessIdentity,
    pub context: Context,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Coverage {
    pub panic: bool,
    pub installed_signals: Vec<i32>,
    pub refused_owned_signals: Vec<i32>,
}
pub(super) struct PendingFile {
    pub header: Header,
    pub events: Vec<(String, Event)>,
    path: PathBuf,
    digest: String,
}
pub(super) struct Collection {
    pub files: Vec<PendingFile>,
    /// Invalid/partial files stay local; they never become inferred events.
    pub rejected_files: usize,
}
struct Slot {
    fd: i32,
    panic: AtomicBool,
    fatal: AtomicBool,
}
fn invalid() -> io::Error {
    io::Error::other("invalid bounded crash evidence")
}
fn hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
fn uuid(value: &str) -> bool {
    crate::config::telemetry::valid_uuid(value)
}
fn valid_header(header: &Header) -> bool {
    hex(&header.build)
        && uuid(&header.boot)
        && header.process.pid > 0
        && header.process.pid <= i32::MAX as u32
        && header.process.start_ticks > 0
        && header.process.start_ticks <= i64::MAX as u64
        && header.context.clock == "boottime"
        && header.context.kernel.as_ref().is_none_or(|v| hex(v))
        && header.context.platform.as_ref().is_none_or(|v| hex(v))
        && header.context.run_id.as_ref().is_none_or(|v| uuid(v))
        && match header.context.stream {
            Stream::Production => header.context.labels.is_empty(),
            Stream::AutomatedTest => header.context.labels == ["automated-test"],
        }
}
fn filename(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".crash"))
        .is_some_and(|id| uuid(id) && id.ends_with('0'))
}
fn paths(_blocking: &Blocking, directory: &Path) -> io::Result<Vec<PathBuf>> {
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let metadata = std::fs::symlink_metadata(directory)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(invalid());
    }
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let entry = entry?;
        if paths.len() >= MAX_FILES || !entry.file_type()?.is_file() || !filename(&entry.path()) {
            return Err(invalid());
        }
        paths.push(entry.path());
    }
    paths.sort();
    Ok(paths)
}

/// Called before the sender's new context is acknowledged. A failed enable
/// never replaces the current published slot; callers disable first at an
/// explicit transition boundary. Exhaustion rejects enable, never disable.
pub(super) fn enable(_blocking: &Blocking, directory: &Path, header: Header) -> io::Result<()> {
    if !valid_header(&header) {
        return Err(invalid());
    }
    let bytes = serde_json::to_vec(&header).map_err(|_| invalid())?;
    if bytes.len() > HEADER - 12 {
        return Err(invalid());
    }
    SLOTS
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
            (n < MAX_CONTEXTS).then_some(n + 1)
        })
        .map_err(|_| io::Error::other("crash context capacity reached"))?;
    let result = (|| {
        std::fs::create_dir_all(directory)?;
        if paths(_blocking,directory)?.len() >= MAX_FILES {
            return Err(io::Error::other("crash journal capacity reached"));
        }
        let mut id = super::uuid();
        id.replace_range(35..36, "0");
        let path = directory.join(format!("{id}.crash"));
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(&path)?;
        let mut prepared = [0u8; FILE_BYTES];
        prepared[..8].copy_from_slice(b"Q2CRSH01");
        prepared[8..12].copy_from_slice(&(bytes.len() as u32).to_le_bytes());
        prepared[12..12 + bytes.len()].copy_from_slice(&bytes);
        file.write_all(&prepared)?;
        file.sync_all()?;
        File::open(directory)?.sync_all()?;
        let slot = Box::leak(Box::new(Slot {
            fd: file.into_raw_fd(),
            panic: AtomicBool::new(false),
            fatal: AtomicBool::new(false),
        }));
        ACTIVE.store(slot, Ordering::Release);
        Ok(())
    })();
    if result.is_err() {
        SLOTS.fetch_sub(1, Ordering::AcqRel);
    }
    result
}
/// No disk access or lock: optional capture always turns off, including at cap.
pub(super) fn disable() {
    ACTIVE.store(std::ptr::null_mut(), Ordering::Release);
}

/// Opt-out discards optional crash backlog too. Unlinking a published slot is
/// safe: a concurrent fatal writer retains its never-reused fd to that unlinked
/// inode, and cannot recreate or reclassify it. Mandatory pstore is elsewhere.
pub(super) fn discard(_blocking: &Blocking, directory: &Path) -> io::Result<()> {
    disable();
    for path in paths(_blocking,directory)? { std::fs::remove_file(path)?; }
    match File::open(directory) {
        Ok(file) => file.sync_all(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn record(kind: u8, signal: u8, code: i32) {
    let pointer = ACTIVE.load(Ordering::Acquire);
    if pointer.is_null() {
        return;
    }
    // SAFETY: published slots/descriptors are immutable and never freed/closed
    // during the process. Only the independent one-shot marker atomics mutate.
    let slot = unsafe { &*pointer };
    let claimed = if kind == 1 { &slot.panic } else { &slot.fatal };
    if claimed.swap(true, Ordering::AcqRel) {
        return;
    }
    let mut marker = [0u8; MARKER];
    marker[..4].copy_from_slice(b"Q2EV");
    marker[4] = kind;
    marker[5] = signal;
    marker[8..12].copy_from_slice(&code.to_le_bytes());
    marker[12..].copy_from_slice(b"DONE");
    let offset = HEADER + usize::from(kind - 1) * MARKER;
    // One bounded write; partial/error is retained as incomplete and rejected by
    // collection. pwrite is async-signal-safe; fsync is deliberately not done.
    unsafe {
        libc::pwrite(
            slot.fd,
            marker.as_ptr().cast(),
            marker.len(),
            offset as libc::off_t,
        );
    }
}
extern "C" fn fatal(signal: i32, info: *mut libc::siginfo_t, _: *mut libc::c_void) {
    // SAFETY: the kernel supplies siginfo for SA_SIGINFO, or null is tolerated.
    let code = if info.is_null() {
        0
    } else {
        unsafe { (*info).si_code }
    };
    record(2, signal as u8, code);
    // SA_RESETHAND already restored default disposition. raise queues the same
    // signal for this thread, delivered on handler return with its mask restored.
    // Preserve signal termination instead of turning it into a numeric exit.
    unsafe {
        if libc::raise(signal) != 0 {
            libc::_exit(128 + signal);
        }
    }
}

/// Preserve other signal owners (notably Rust's stack-overflow handlers).
/// Existing panic-hook local stderr behavior is unchanged; this recorder never
/// inspects its payload. Only record() is the allocation/lock-free capture path.
pub(super) fn install_handlers(_blocking: &Blocking) -> io::Result<Coverage> {
    let _guard = INSTALL.lock().map_err(|_| invalid())?;
    if let Some(coverage) = COVERAGE.get() {
        return Ok(coverage.clone());
    }
    let mut coverage = Coverage {
        panic: true,
        installed_signals: Vec::new(),
        refused_owned_signals: Vec::new(),
    };
    for signal in SIGNALS {
        // SAFETY: zeroed sigaction is initialized through libc before use.
        let mut previous: libc::sigaction = unsafe { std::mem::zeroed() };
        if unsafe { libc::sigaction(signal, std::ptr::null(), &mut previous) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if previous.sa_sigaction != libc::SIG_DFL {
            coverage.refused_owned_signals.push(signal);
            continue;
        }
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = fatal as *const () as libc::sighandler_t;
        action.sa_flags = libc::SA_SIGINFO | libc::SA_RESETHAND;
        unsafe {
            libc::sigemptyset(&mut action.sa_mask);
        }
        if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        coverage.installed_signals.push(signal);
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |information| {
        record(1, 0, 0);
        previous(information);
    }));
    let _ = COVERAGE.set(coverage.clone());
    Ok(coverage)
}
fn read(_blocking: &Blocking, path: &Path) -> io::Result<Vec<u8>> {
    if !filename(path) {
        return Err(invalid());
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() != FILE_BYTES as u64 || metadata.nlink() != 1 {
        return Err(invalid());
    }
    let mut bytes = Vec::new();
    file.take(FILE_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() != FILE_BYTES {
        return Err(invalid());
    }
    Ok(bytes)
}
fn digest(bytes: &[u8]) -> String {
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect()
}
fn decode(bytes: &[u8], id: &str) -> io::Result<(Header, Vec<(String, Event)>)> {
    if bytes.len() != FILE_BYTES || &bytes[..8] != b"Q2CRSH01" {
        return Err(invalid());
    }
    let size = u32::from_le_bytes(bytes[8..12].try_into().map_err(|_| invalid())?) as usize;
    if size == 0 || size > HEADER - 12 || bytes[12 + size..HEADER].iter().any(|b| *b != 0) {
        return Err(invalid());
    }
    let header: Header = serde_json::from_slice(&bytes[12..12 + size]).map_err(|_| invalid())?;
    if !valid_header(&header) {
        return Err(invalid());
    }
    let mut events = Vec::new();
    for index in 0..2 {
        let marker = &bytes[HEADER + index * MARKER..HEADER + (index + 1) * MARKER];
        if marker.iter().all(|b| *b == 0) {
            continue;
        }
        if &marker[..4] != b"Q2EV"
            || &marker[12..] != b"DONE"
            || marker[4] != index as u8 + 1
            || marker[6..8] != [0, 0]
        {
            return Err(invalid());
        }
        let code = i32::from_le_bytes(marker[8..12].try_into().map_err(|_| invalid())?);
        let event = if index == 0 {
            if marker[5] != 0 || code != 0 {
                return Err(invalid());
            }
            Event::AppPanic {}
        } else {
            if !SIGNALS.contains(&i32::from(marker[5])) {
                return Err(invalid());
            }
            Event::FatalSignal {
                signal: u32::from(marker[5]),
                code,
            }
        };
        let mut event_id = id.to_owned();
        event_id.replace_range(35..36, if index == 0 { "0" } else { "1" });
        events.push((event_id, event));
    }
    Ok((header, events))
}

/// Collect only this bounded generated journal, never pstore, stderr or logs.
/// Current/live process files stay untouched. Cross-boot/process context comes
/// from the immutable header, not the collector's current settings.
pub(super) fn collect(
    _blocking: &Blocking,
    directory: &Path,
    current_boot: &str,
    current_process: ProcessIdentity,
) -> io::Result<Collection> {
    if !uuid(current_boot)
        || current_process.pid == 0
        || current_process.pid > i32::MAX as u32
        || current_process.start_ticks == 0
        || current_process.start_ticks > i64::MAX as u64
    {
        return Err(invalid());
    }
    let mut result = Collection {
        files: Vec::new(),
        rejected_files: 0,
    };
    for path in paths(_blocking,directory)? {
        let decoded = (|| {
            let bytes = read(_blocking,&path)?;
            let id = path
                .file_stem()
                .and_then(|n| n.to_str())
                .ok_or_else(invalid)?;
            let (header, events) = decode(&bytes, id)?;
            Ok::<_, io::Error>((bytes, header, events))
        })();
        let Ok((bytes, header, events)) = decoded else {
            result.rejected_files += 1;
            continue;
        };
        if header.boot == current_boot {
            if header.process == current_process {
                continue;
            }
            // Conservatively retain a reused live PID too; a stale numeric PID
            // must not authorize consuming a journal with an active writer.
            if unsafe { libc::kill(header.process.pid as i32, 0) } == 0
                || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
            {
                continue;
            }
        }
        result.files.push(PendingFile {
            header,
            events,
            digest: digest(&bytes),
            path,
        });
    }
    Ok(result)
}
/// Call only after all returned events from this file are durably queued.
pub(super) fn acknowledge(_blocking: &Blocking, pending: &PendingFile) -> io::Result<()> {
    if digest(&read(_blocking,&pending.path)?) != pending.digest {
        return Err(invalid());
    }
    std::fs::remove_file(&pending.path)?;
    File::open(pending.path.parent().ok_or_else(invalid)?)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    const BOOT: &str = "12345678-1234-1234-1234-123456789abc";
    const RUN: &str = "12345678-1234-1234-1234-123456789def";
    fn header() -> Header {
        Header {
            build: "a".repeat(64),
            boot: BOOT.into(),
            process: ProcessIdentity {
                pid: std::process::id(),
                start_ticks: 1,
            },
            context: Context {
                stream: Stream::AutomatedTest,
                labels: vec!["automated-test".into()],
                run_id: Some(RUN.into()),
                kernel: None,
                platform: None,
                clock: "boottime".into(),
            },
        }
    }
    fn fixture() -> PathBuf {
        let root = std::env::var_os("Q2_GENERATED_TEST_ROOT")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("../../build/perf"));
        let path = root.join(format!("crash-{:08x}", fastrand::u32(..)));
        std::fs::create_dir_all(&path).unwrap();
        path
    }
    fn child(mode: &str, directory: &Path) -> std::process::ExitStatus {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "diagnostics::crash::tests::child_fixture",
                "--nocapture",
            ])
            .env("Q2_CRASH_CHILD", mode)
            .env("Q2_CRASH_DIRECTORY", directory.canonicalize().unwrap())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return status;
            }
            if std::time::Instant::now() >= until {
                let _ = child.kill();
                let _ = child.wait();
                panic!("bounded generated crash child timed out");
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    #[test]
    fn child_fixture() {
        let Some(mode) = std::env::var_os("Q2_CRASH_CHILD") else {
            return;
        };
        let directory = PathBuf::from(std::env::var_os("Q2_CRASH_DIRECTORY").unwrap());
        let blocking = Blocking::dedicated_thread();
        // A generated fatal child must never leave unrestricted memory/core data.
        let limits = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_CORE, &limits) }, 0);
        if mode == "off" {
            disable();
            record(1, 0, 0);
            return;
        }
        if mode == "cap" {
            for _ in 0..MAX_CONTEXTS {
                enable(&blocking, &directory, header()).unwrap();
            }
            disable();
            assert!(enable(&blocking, &directory, header()).is_err());
            disable();
            record(1, 0, 0);
            return;
        }
        if mode == "owner" {
            unsafe {
                libc::signal(libc::SIGABRT, libc::SIG_IGN);
            }
            let coverage = install_handlers(&blocking).unwrap();
            assert!(coverage.refused_owned_signals.contains(&libc::SIGABRT));
            assert!(!coverage.installed_signals.contains(&libc::SIGABRT));
            return;
        }
        let coverage = install_handlers(&blocking).unwrap();
        enable(&blocking, &directory, header()).unwrap();
        if mode == "panic" {
            panic!("SYNTHETIC_PRIVATE_PANIC_CANARY");
        }
        if mode == "signal" {
            assert!(coverage.installed_signals.contains(&libc::SIGABRT));
            unsafe {
                libc::raise(libc::SIGABRT);
            }
            panic!("fatal signal returned");
        }
        if mode == "transition" {
            record(1, 0, 0);
            disable();
            let mut production = header();
            production.context.stream = Stream::Production;
            production.context.labels.clear();
            production.context.run_id = None;
            enable(&blocking, &directory, production).unwrap();
            record(2, libc::SIGABRT as u8, -6);
            disable();
        }
    }

    #[test]
    fn actual_panic_keeps_original_context_without_payload_and_ack_follows_durable_import() {
        let directory = fixture();
        let blocking = Blocking::dedicated_thread();
        let result = child("panic", &directory);
        assert!(!result.success());
        let collection = collect(&blocking, &directory, BOOT, header().process).unwrap();
        assert_eq!(collection.rejected_files, 0);
        assert_eq!(collection.files.len(), 1);
        let pending = &collection.files[0];
        assert_eq!(pending.events.len(), 1);
        assert_eq!(pending.events[0].1, Event::AppPanic {});
        assert_eq!(pending.header.context.stream, Stream::AutomatedTest);
        assert_eq!(pending.header.context.run_id.as_deref(), Some(RUN));
        assert_eq!(pending.header.boot, BOOT);
        let bytes = read(&blocking,&pending.path).unwrap();
        assert!(
            !bytes
                .windows(b"SYNTHETIC_PRIVATE_PANIC_CANARY".len())
                .any(|v| v == b"SYNTHETIC_PRIVATE_PANIC_CANARY")
        );
        // Collection itself never deletes evidence; caller first persists events.
        assert!(pending.path.exists());
        acknowledge(&blocking, pending).unwrap();
        assert!(paths(&blocking,&directory).unwrap().is_empty());
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn actual_fatal_signal_preserves_signal_exit_and_numeric_marker() {
        let directory = fixture();
        let blocking = Blocking::dedicated_thread();
        let result = child("signal", &directory);
        assert_eq!(result.signal(), Some(libc::SIGABRT));
        let rows = collect(&blocking, &directory, BOOT, header().process).unwrap();
        assert_eq!(rows.rejected_files, 0);
        assert_eq!(rows.files.len(), 1);
        assert!(
            matches!(rows.files[0].events[0].1, Event::FatalSignal { signal, .. } if signal == libc::SIGABRT as u32)
        );
        acknowledge(&blocking, &rows.files[0]).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn later_context_does_not_relabel_prior_panic_and_off_never_writes() {
        let directory = fixture();
        let blocking = Blocking::dedicated_thread();
        assert!(child("off", &directory).success());
        assert!(paths(&blocking,&directory).unwrap().is_empty());
        assert!(child("transition", &directory).success());
        let rows = collect(&blocking, &directory, BOOT, header().process).unwrap();
        assert_eq!(rows.files.len(), 2);
        for row in &rows.files {
            match row.events[0].1 {
                Event::AppPanic {} => assert_eq!(row.header.context.run_id.as_deref(), Some(RUN)),
                Event::FatalSignal { .. } => {
                    assert_eq!(row.header.context.stream, Stream::Production);
                    assert!(row.header.context.run_id.is_none());
                }
                _ => panic!("nonfatal event is not a fatal-handler marker"),
            }
            acknowledge(&blocking, row).unwrap();
        }
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn capacity_refuses_more_contexts_but_disable_always_works() {
        let directory = fixture();
        let blocking = Blocking::dedicated_thread();
        assert!(child("cap", &directory).success());
        let rows = collect(&blocking, &directory, BOOT, header().process).unwrap();
        assert_eq!(rows.files.len(), MAX_CONTEXTS as usize);
        for row in &rows.files {
            assert!(row.events.is_empty());
            acknowledge(&blocking, row).unwrap();
        }
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn existing_signal_owner_is_not_replaced() {
        let directory = fixture();
        assert!(child("owner", &directory).success());
        std::fs::remove_dir(directory).unwrap();
    }

    #[test]
    fn corrupt_partial_records_are_retained_and_event_schema_rejects_text() {
        let directory = fixture();
        let blocking = Blocking::dedicated_thread();
        let path = directory.join("12345678-1234-1234-1234-123456789ab0.crash");
        std::fs::write(&path, b"partial crash record").unwrap();
        let rows = collect(&blocking, &directory, BOOT, header().process).unwrap();
        assert_eq!(rows.rejected_files, 1);
        assert!(rows.files.is_empty());
        assert!(path.exists());
        for value in [
            serde_json::json!({"type":"app-panic","text":"secret-canary"}),
            serde_json::json!({"type":"fatal-signal","signal":6,"code":-6,"address":123}),
        ] {
            assert!(serde_json::from_value::<Event>(value).is_err());
        }
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}

#[cfg(test)]
mod ux_wire_tests {
    /// Must match `validate_ux` in tools/diagnostic_receiver.py.
    #[test]
    fn ux_event_wire_format_matches_receiver() {
        let event = super::Event::Ux {
            what: crate::ux::What::Error, code: 13, journey: 4, outcome: 3, latency_us: 1_500_000, detail: 123456,
            state: crate::ux::Snapshot { wifi: 2, navidrome: 1, abs: 2, doze: false, offline: false, screen_on: true, play: 2, pages: 1, idle_s: 4, route: 3 },
            recent: vec![[1000, 1, 257]], bluetooth: None,
        };
        let json: serde_json::Value = serde_json::to_value(&event).unwrap();
        assert_eq!(json, serde_json::json!({"type":"ux","what":"error","code":13,"journey":4,"outcome":3,"latency_us":1500000,"detail":123456,
            "state":{"wifi":2,"navidrome":1,"abs":2,"doze":false,"offline":false,"screen_on":true,"play":2,"pages":1,"idle_s":4,"route":3},
            "recent":[[1000,1,257]]}));
        assert_eq!(serde_json::from_value::<super::Event>(json).unwrap(), event);
        // A report from before the route and Bluetooth fields still reads.
        let old = serde_json::json!({"type":"ux","what":"error","code":13,"journey":4,"outcome":3,"latency_us":1500000,"detail":123456,
            "state":{"wifi":2,"navidrome":1,"abs":2,"doze":false,"offline":false,"screen_on":true,"play":2,"pages":1,"idle_s":4},
            "recent":[[1000,1,257]]});
        let super::Event::Ux { state, bluetooth, .. } = serde_json::from_value::<super::Event>(old).unwrap() else { panic!("ux") };
        assert_eq!((state.route, bluetooth), (0, None));
        // With Bluetooth on, its numbers ride along under "bluetooth".
        let bt = crate::ux::bluetooth::Bluetooth { state: 6, codec: 5, rate: 44_100, kbps: 606, rssi: Some(-4), ..Default::default() };
        let super::Event::Ux { what, code, journey, outcome, latency_us, detail, state, recent, .. } = event else { panic!("ux") };
        let with = super::Event::Ux { what, code, journey, outcome, latency_us, detail, state, recent, bluetooth: Some(bt) };
        let json = serde_json::to_value(&with).unwrap();
        assert_eq!((json["bluetooth"]["codec"].as_u64(), json["bluetooth"]["rssi"].as_i64()), (Some(5), Some(-4)));
        assert_eq!(serde_json::from_value::<super::Event>(json).unwrap(), with);
    }
}
