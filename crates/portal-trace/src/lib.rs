//! Best-effort, per-attempt diagnostics on the removable card.
use ring::rand::{SecureRandom, SystemRandom};
use serde_json::{Value, json};
use std::ffi::{CStr, CString};
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const CARD: &str = "/mnt/mmc";
const MAX_EVENT_BYTES: usize = 64 * 1024;
const MAX_TRACE_BYTES: usize = 2 * 1024 * 1024;
const MAX_EVENTS: u32 = 256;
const QUEUE_SLOTS: usize = 32;
const QUEUE_BYTES: usize = 384 * 1024;
const TERMINAL_SPACE: usize = 8 * 1024;
const FOOTER_SPACE: usize = 512;
const FREE_FLOOR: u64 = 4 * 1024 * 1024;
const CLOSE_WAIT: Duration = Duration::from_millis(900);
const SYNC_INTERVAL: Duration = Duration::from_millis(40);

pub struct Trace {
    id: Option<String>,
    queue: Option<SyncSender<Event>>,
    done: Option<Receiver<()>>,
    stats: Arc<QueueStats>,
    started: Instant,
}

#[derive(Default)]
struct QueueStats {
    running: AtomicBool,
    queue_full: AtomicU32,
    bytes: AtomicUsize,
    slots: AtomicUsize,
}

impl QueueStats {
    fn reserve(&self, bytes: usize, terminal: bool) -> bool {
        let slots = QUEUE_SLOTS + usize::from(terminal);
        let limit = if terminal {
            QUEUE_BYTES
        } else {
            QUEUE_BYTES - TERMINAL_SPACE
        };
        if self
            .slots
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                (used < slots).then_some(used + 1)
            })
            .is_err()
        {
            return false;
        }
        if self
            .bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                (bytes <= limit.saturating_sub(used)).then_some(used + bytes)
            })
            .is_err()
        {
            self.slots.fetch_sub(1, Ordering::AcqRel);
            return false;
        }
        true
    }

    fn retire(&self, bytes: usize) {
        self.bytes.fetch_sub(bytes, Ordering::AcqRel);
        self.slots.fetch_sub(1, Ordering::AcqRel);
    }
}

struct Event {
    kind: String,
    detail: Vec<u8>,
    milliseconds: u64,
    queue_bytes: usize,
}

impl Trace {
    pub fn open(op: &str) -> Self {
        let id = std::env::var("Q2_PORTAL_ATTEMPT")
            .ok()
            .filter(|id| valid_uuid(id))
            .or_else(new_id);
        let stats = Arc::new(QueueStats::default());
        let mut trace = Self {
            id,
            queue: None,
            done: None,
            stats,
            started: Instant::now(),
        };
        if let Some(id) = trace.id.clone() {
            let (queue, events) = mpsc::sync_channel(QUEUE_SLOTS + 1);
            let (finished, done) = mpsc::sync_channel(1);
            let stats = trace.stats.clone();
            stats.running.store(true, Ordering::Release);
            let operation: String = op.chars().take(32).collect();
            if std::thread::Builder::new()
                .name("portal-trace".into())
                .spawn(move || {
                    let _ = record_card(&id, &operation, events, &stats);
                    stats.running.store(false, Ordering::Release);
                    let _ = finished.send(());
                })
                .is_ok()
            {
                trace.queue = Some(queue);
                trace.done = Some(done);
            }
        }
        trace
    }

    pub fn enabled(&self) -> bool {
        self.queue.is_some() && self.stats.running.load(Ordering::Acquire)
    }

    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    /// Record only the bounded, caller-sanitized context for this event.
    pub fn event(&self, kind: &str, detail: Value) {
        let Some(queue) = self.queue.as_ref().filter(|_| self.enabled()) else {
            return;
        };
        let terminal = kind == "terminal";
        let payload = event_payload(
            detail,
            if terminal {
                TERMINAL_SPACE / 2
            } else {
                MAX_EVENT_BYTES
            },
        );
        let queue_bytes = payload.capacity() + 256;
        if self.stats.reserve(queue_bytes, terminal) {
            let event = Event {
                kind: kind.chars().take(48).collect(),
                detail: payload,
                milliseconds: self.started.elapsed().as_millis() as u64,
                queue_bytes,
            };
            if queue.try_send(event).is_ok() {
                return;
            }
            self.stats.retire(queue_bytes);
        }
        self.stats.queue_full.fetch_add(1, Ordering::Relaxed);
    }
}

impl Drop for Trace {
    fn drop(&mut self) {
        drop(self.queue.take());
        if let Some(done) = self.done.take() {
            let _ = done.recv_timeout(CLOSE_WAIT);
        }
    }
}

struct BoundedBytes(Vec<u8>, usize);

impl Write for BoundedBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.1.saturating_sub(self.0.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "trace event too large",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn event_payload(mut detail: Value, limit: usize) -> Vec<u8> {
    let bytes = |value: &Value| {
        let mut buffer = BoundedBytes(Vec::new(), limit);
        serde_json::to_writer(&mut buffer, value)
            .ok()
            .map(|_| buffer.0)
    };
    if let Some(payload) = bytes(&detail) {
        return payload;
    }
    // Keep the envelope's compact fields if a caller's body or other
    // projection exceeds the storage limit. Never save a partial JSON value.
    if let Some(fields) = detail.as_object_mut() {
        fields.retain(|_, value| bytes(value).is_some_and(|encoded| encoded.len() <= 1024));
        fields.insert("storage_truncated".into(), Value::Bool(true));
        if let Some(payload) = bytes(&detail) {
            return payload;
        }
    }
    br#"{"storage_truncated":true}"#.to_vec()
}

fn time_metadata() -> Value {
    let wall = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|time| time.as_millis() as u64);
    let mut time = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    let boot_ms = (unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut time) } == 0)
        .then(|| time.tv_sec as u64 * 1000 + time.tv_nsec as u64 / 1_000_000);
    let boot_id = fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|id| id.trim().to_owned())
        .filter(|id| valid_uuid(id));
    json!({"unix_ms":wall,"boot_ms":boot_ms,"boot_id":boot_id})
}

fn build_metadata() -> Value {
    let mut bytes = Vec::new();
    if File::open("/etc/q2-os-release.json")
        .and_then(|f| f.take(8192).read_to_end(&mut bytes))
        .is_err()
    {
        return Value::Null;
    }
    let Ok(data) = serde_json::from_slice::<Value>(&bytes) else {
        return Value::Null;
    };
    let mut result = serde_json::Map::new();
    for key in [
        "release_tag",
        "package_tag",
        "app_source_commit",
        "source_commit",
    ] {
        if let Some(value) = data[key].as_str().filter(|value| {
            value.len() <= 64
                && value
                    .bytes()
                    .all(|ch| ch.is_ascii_alphanumeric() || b"._-".contains(&ch))
        }) {
            result.insert(key.into(), Value::String(value.to_owned()));
        }
    }
    Value::Object(result)
}

fn valid_uuid(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, byte)| match i {
            8 | 13 | 18 | 23 => byte == b'-',
            14 => byte == b'4',
            19 => matches!(byte, b'8' | b'9' | b'a' | b'b'),
            _ => byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'),
        })
}

pub fn new_id() -> Option<String> {
    let mut bytes = [0; 16];
    SystemRandom::new().fill(&mut bytes).ok()?;
    bytes[6] = bytes[6] & 0x0f | 0x40;
    bytes[8] = bytes[8] & 0x3f | 0x80;
    let mut id = String::with_capacity(36);
    for (index, byte) in bytes.into_iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            id.push('-');
        }
        use std::fmt::Write as _;
        write!(id, "{byte:02x}").ok()?;
    }
    Some(id)
}

fn open_at(dir: &File, name: &CStr, flags: libc::c_int) -> io::Result<File> {
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn child_dir(dir: &File, name: &CStr) -> io::Result<File> {
    open_at(dir, name, libc::O_RDONLY | libc::O_DIRECTORY)
}

fn mkdir_at(dir: &File, name: &CStr, allow_existing: bool) -> io::Result<()> {
    if unsafe { libc::mkdirat(dir.as_raw_fd(), name.as_ptr(), 0o700) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if allow_existing && error.kind() == io::ErrorKind::AlreadyExists {
        Ok(())
    } else {
        Err(error)
    }
}

fn refused() -> io::Error {
    io::Error::new(io::ErrorKind::NotFound, "no admitted trace card")
}

fn mount_id(file: &File) -> io::Result<u64> {
    fs::read_to_string(format!("/proc/self/fdinfo/{}", file.as_raw_fd()))?
        .lines()
        .find_map(|line| line.strip_prefix("mnt_id:")?.trim().parse().ok())
        .ok_or_else(refused)
}

fn listed_mount(mounts: &str, id: u64, destination: &str) -> bool {
    mounts.lines().any(|line| {
        let mut fields = line.split_ascii_whitespace();
        fields.next().and_then(|raw| raw.parse().ok()) == Some(id)
            && fields.nth(3) == Some(destination)
    })
}

fn mounted_card() -> io::Result<File> {
    let root = File::open("/")?;
    let mnt = child_dir(&root, c"mnt")?;
    let card = child_dir(&mnt, c"mmc")?;
    let id = mount_id(&card)?;
    let mounts = fs::read_to_string("/proc/self/mountinfo")?;
    if id == mount_id(&mnt)? || !listed_mount(&mounts, id, CARD) {
        return Err(refused());
    }
    Ok(card)
}

fn own_mount(file: &File, expected: u64) -> io::Result<()> {
    if mount_id(file)? == expected {
        Ok(())
    } else {
        Err(refused())
    }
}

fn check_space(card: &File) -> io::Result<()> {
    let mut status = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    if unsafe { libc::fstatvfs(card.as_raw_fd(), status.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let status = unsafe { status.assume_init() };
    if (status.f_bavail as u64).saturating_mul(status.f_frsize as u64) >= FREE_FLOOR {
        Ok(())
    } else {
        Err(io::Error::other("insufficient card space for trace"))
    }
}

fn record_card(id: &str, op: &str, events: Receiver<Event>, stats: &QueueStats) -> io::Result<()> {
    let card = mounted_card()?;
    check_space(&card)?;
    record_attempt(&card, id, op, events, stats)
}

fn record_attempt(
    card: &File,
    id: &str,
    op: &str,
    events: Receiver<Event>,
    stats: &QueueStats,
) -> io::Result<()> {
    if !valid_uuid(id) {
        return Err(refused());
    }
    let card_mount = mount_id(card)?;
    mkdir_at(card, c"portal-debug", true)?;
    let base = child_dir(card, c"portal-debug")?;
    own_mount(&base, card_mount)?;
    let name = CString::new(id).map_err(|_| refused())?;
    mkdir_at(&base, &name, false)?;
    let attempt = child_dir(&base, &name)?;
    own_mount(&attempt, card_mount)?;
    let mut file = open_at(
        &attempt,
        c"events.jsonl",
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NONBLOCK,
    )?;
    own_mount(&file, card_mount)?;
    file.write_all(
        format!(
            "{}\n",
            json!({"v":1,"seq":0,"kind":"start","attempt":id,"op":op,"time":time_metadata(),"build":build_metadata()})
        )
        .as_bytes(),
    )?;
    file.sync_data()?;
    attempt.sync_all()?;
    base.sync_all()?;
    card.sync_all()?;

    let mut remaining = MAX_TRACE_BYTES - FOOTER_SPACE - TERMINAL_SPACE;
    let mut seq = 1;
    let mut omitted = 0_u32;
    let mut terminal_space = TERMINAL_SPACE;
    let mut last_sync = Instant::now();
    for event in events {
        if event.queue_bytes != 0 {
            stats.retire(event.queue_bytes);
        }
        let prefix = format!(
            "{{\"v\":1,\"seq\":{seq},\"ms\":{},\"kind\":{},\"detail\":",
            event.milliseconds,
            json!(event.kind)
        );
        let size = prefix.len() + event.detail.len() + 2;
        let terminal = event.kind == "terminal";
        let budget = if terminal {
            &mut terminal_space
        } else {
            &mut remaining
        };
        if seq > MAX_EVENTS - u32::from(!terminal) || size > *budget {
            omitted = omitted.saturating_add(1);
            continue;
        }
        file.write_all(prefix.as_bytes())?;
        file.write_all(&event.detail)?;
        file.write_all(b"}\n")?;
        // Coalesce adjacent HTTP records. Prompt/output boundaries force
        // durable progress, and the final close flushes every queued record.
        if matches!(event.kind.as_str(), "state" | "terminal")
            || last_sync.elapsed() >= SYNC_INTERVAL
        {
            file.sync_data()?;
            last_sync = Instant::now();
        }
        *budget -= size;
        seq += 1;
    }

    // A valid final line identifies a clean close. An incomplete final line
    // or no close record is distinguishable after power loss or I/O failure.
    let queue_full = stats.queue_full.load(Ordering::Relaxed);
    let dropped = queue_full.saturating_add(omitted);
    file.write_all(
        format!(
            "{}\n",
            json!({"v":1,"seq":seq,"kind":"close","dropped":dropped,"queue_full":queue_full,"attempt_limit":omitted})
        )
        .as_bytes(),
    )?;
    file.sync_data()
}

/// Persist the player-observed end independently of the helper's lifetime.
/// Call only from a dedicated worker since removable-media I/O can stall.
pub fn player_outcome(id: &str, reason: &str) {
    if valid_uuid(id) && valid_outcome(reason) {
        let _ = mounted_card().and_then(|card| store_outcome(&card, id, reason));
    }
}

fn valid_outcome(reason: &str) -> bool {
    matches!(
        reason,
        "back"
            | "roam"
            | "timeout"
            | "helper_crash"
            | "cancelled"
            | "superseded"
            | "completed"
            | "wifi_off"
            | "shutdown"
            | "helper_error"
    )
}

fn store_outcome(card: &File, id: &str, reason: &str) -> io::Result<()> {
    if !valid_uuid(id) || !valid_outcome(reason) {
        return Err(refused());
    }
    let name = CString::new(id).map_err(|_| refused())?;
    let card_mount = mount_id(card)?;
    let open_attempt = || -> io::Result<File> {
        let base = child_dir(card, c"portal-debug")?;
        own_mount(&base, card_mount)?;
        let attempt = child_dir(&base, &name)?;
        own_mount(&attempt, card_mount)?;
        Ok(attempt)
    };
    let mut retries = 0;
    let attempt = loop {
        match open_attempt() {
            Ok(attempt) => break attempt,
            Err(e) if e.kind() == io::ErrorKind::NotFound && retries < 3 => {
                retries += 1;
                std::thread::sleep(Duration::from_millis(35));
            }
            Err(e) => return Err(e),
        }
    };

    let temporary = c".player-outcome.tmp";
    let mut file = open_at(
        &attempt,
        temporary,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NONBLOCK,
    )?;
    let write = (|| {
        own_mount(&file, card_mount)?;
        writeln!(
            file,
            "{}",
            json!({"v":1,"attempt":id,"reason":reason,"time":time_metadata()})
        )?;
        file.sync_data()?;
        let fd = attempt.as_raw_fd();
        let renamed = unsafe {
            libc::syscall(
                libc::SYS_renameat2,
                fd,
                temporary.as_ptr(),
                fd,
                c"player-outcome.json".as_ptr(),
                libc::RENAME_NOREPLACE,
            )
        };
        if renamed != 0 {
            return Err(io::Error::last_os_error());
        }
        attempt.sync_all()
    })();
    if write.is_err() {
        unsafe { libc::unlinkat(attempt.as_raw_fd(), temporary.as_ptr(), 0) };
    }
    write
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_and_mount_identity_are_strict() {
        for _ in 0..4 {
            assert!(valid_uuid(&new_id().unwrap()));
        }
        assert!(!valid_uuid("b8f38ded-1ceb-7b13-b0e3-9448edcaf61c"));
        assert!(!valid_uuid("../../../mnt/mmc/portal-debug"));
        let mounts = "57 2 8:1 / /mnt/mmc rw - vfat /dev/test rw\n58 2 8:2 / /mnt/mmc/other rw - vfat /dev/test2 rw";
        assert!(listed_mount(mounts, 57, CARD));
        assert!(!listed_mount(mounts, 58, CARD));
    }

    #[test]
    fn oversized_projection_keeps_http_metadata() {
        let payload = event_payload(
            json!({
                "status": 511,
                "url": "/welcome",
                "body": "x".repeat(MAX_EVENT_BYTES),
            }),
            MAX_EVENT_BYTES,
        );
        let detail: Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(detail["status"], 511);
        assert_eq!(detail["url"], "/welcome");
        assert_eq!(detail["storage_truncated"], true);
        assert!(detail.get("body").is_none());
    }

    #[test]
    fn queue_preserves_a_terminal_slot_under_pressure() {
        let stats = QueueStats::default();
        for _ in 0..QUEUE_SLOTS {
            assert!(stats.reserve(1024, false));
        }
        assert!(!stats.reserve(1024, false));
        assert!(stats.reserve(1024, true));
        assert!(!stats.reserve(1024, true));
        for _ in 0..=QUEUE_SLOTS {
            stats.retire(1024);
        }
        assert_eq!(stats.bytes.load(Ordering::Relaxed), 0);
        assert_eq!(stats.slots.load(Ordering::Relaxed), 0);

        assert!(stats.reserve(QUEUE_BYTES - TERMINAL_SPACE, false));
        assert!(!stats.reserve(1, false));
        assert!(stats.reserve(TERMINAL_SPACE, true));
        assert!(!stats.reserve(1, true));
    }

    #[test]
    fn short_attempt_closes_durably_and_player_outcome_is_first_wins() {
        let root = std::env::temp_dir().join(format!("q2-trace-{}", new_id().unwrap()));
        fs::create_dir(&root).unwrap();
        let card = File::open(&root).unwrap();
        let id = new_id().unwrap();
        assert!(store_outcome(&card, &id, "back").is_err());
        assert!(!root.join("portal-debug").exists());
        let (tx, rx) = mpsc::channel();
        drop(tx);
        record_attempt(&card, &id, "check", rx, &QueueStats::default()).unwrap();
        let attempt = root.join("portal-debug").join(&id);
        let lines = fs::read_to_string(attempt.join("events.jsonl")).unwrap();
        let rows: Vec<Value> = lines
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["kind"], "start");
        assert_eq!(rows[1]["kind"], "close");
        assert!(rows[0]["time"]["boot_ms"].is_number());

        assert!(store_outcome(&card, &id, "../../credentials").is_err());
        store_outcome(&card, &id, "back").unwrap();
        assert!(store_outcome(&card, &id, "roam").is_err());
        let outcome: Value =
            serde_json::from_slice(&fs::read(attempt.join("player-outcome.json")).unwrap())
                .unwrap();
        assert_eq!(outcome["reason"], "back");
        assert_eq!(outcome["attempt"], id);
        assert!(!attempt.join(".player-outcome.tmp").exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn records_are_ordered_and_existing_attempts_and_symlinks_fail() {
        let root = std::env::temp_dir().join(format!("q2-trace-{}", new_id().unwrap()));
        fs::create_dir(&root).unwrap();
        let card = File::open(&root).unwrap();
        let id = new_id().unwrap();
        std::os::unix::fs::symlink(".", root.join("portal-debug")).unwrap();
        let (tx, rx) = mpsc::channel();
        drop(tx);
        assert!(record_attempt(&card, &id, "guest", rx, &QueueStats::default()).is_err());
        fs::remove_file(root.join("portal-debug")).unwrap();

        let (tx, rx) = mpsc::channel();
        tx.send(Event {
            kind: "observed".into(),
            detail: br#"{"status":511}"#.to_vec(),
            milliseconds: 20,
            queue_bytes: 0,
        })
        .unwrap();
        drop(tx);
        record_attempt(
            &card,
            &id,
            "guest",
            rx,
            &QueueStats {
                queue_full: AtomicU32::new(2),
                ..Default::default()
            },
        )
        .unwrap();
        let path = root.join("portal-debug").join(&id).join("events.jsonl");
        let trace = fs::read_to_string(&path).unwrap();
        let rows: Vec<Value> = trace
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["seq"], 0);
        assert_eq!(rows[1]["seq"], 1);
        assert_eq!(rows[1]["detail"]["status"], 511);
        assert_eq!(rows[2]["dropped"], 2);

        let (tx, rx) = mpsc::channel();
        drop(tx);
        assert!(record_attempt(&card, &id, "guest", rx, &QueueStats::default()).is_err());
        assert_eq!(fs::read_to_string(path).unwrap(), trace);
        fs::remove_dir_all(root).unwrap();
    }
}
