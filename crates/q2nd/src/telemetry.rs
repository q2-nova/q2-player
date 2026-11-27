#![forbid(unsafe_code)]
//! Bounded persistent numeric logs. Callers enqueue without filesystem I/O;
//! one drain per log uses the existing blocking pool, with no resident owner.
use crate::rt::Blocking;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub(crate) const MAX_BYTES: u64 = 128 * 1024;
const MAX_PENDING: usize = 128;
const MAX_LINE: usize = 512;

#[derive(Default, Debug)]
struct Pending {
    sequence: u64,
    scheduled: bool,
    lines: VecDeque<String>,
    #[cfg(test)]
    gate: Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
}

#[derive(Debug)]
pub(crate) struct Log {
    path: PathBuf,
    pending: Mutex<Pending>,
    idle: tokio::sync::Notify,
}

impl Log {
    pub fn new(path: PathBuf) -> Arc<Self> {
        Arc::new(Self { path, pending: Mutex::default(), idle: tokio::sync::Notify::new() })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, Pending> { self.pending.lock().unwrap() }

    /// Join the currently queued writes. False means a bounded retry remains
    /// after an I/O failure. Callers may cancel this wait without cancelling
    /// the writer or losing ownership; shutdown can impose its own deadline.
    pub async fn settle(&self) -> bool {
        loop {
            let idle = self.idle.notified();
            tokio::pin!(idle);
            idle.as_mut().enable();
            let finished = { let state = self.state(); (!state.scheduled).then_some(state.lines.is_empty()) };
            if let Some(ok) = finished { return ok; }
            idle.await;
        }
    }

    /// A monotonic identity even when the record cannot be retained. Gaps or
    /// absent endpoints must invalidate analysis, never turn into successes.
    pub fn record(self: &Arc<Self>, format: impl FnOnce(u64) -> String) -> u64 {
        let mut pending = self.state();
        let Some(sequence) = pending.sequence.checked_add(1) else { return 0 };
        pending.sequence = sequence;
        let line = format(sequence);
        if line.len() > MAX_LINE || line.contains('\n') { return sequence; }
        if pending.lines.len() == MAX_PENDING { pending.lines.pop_front(); }
        pending.lines.push_back(line);
        if !pending.scheduled {
            pending.scheduled = true;
            let log = self.clone();
            crate::rt::spawn_blocking(move |blocking| log.drain(blocking));
        }
        sequence
    }

    fn drain(&self, blocking: &Blocking) {
        let mut failed: Option<VecDeque<String>> = None;
        loop {
            #[cfg(test)]
            let gate;
            let batch = {
                let mut pending = self.state();
                if let Some(mut retry) = failed.take() {
                    // Retry on a later event, without timers or an idle worker.
                    // Partial appends may duplicate records; consumers refuse them.
                    retry.append(&mut pending.lines);
                    while retry.len() > MAX_PENDING { retry.pop_front(); }
                    pending.lines = retry;
                    pending.scheduled = false;
                    self.idle.notify_waiters();
                    return;
                }
                if pending.lines.is_empty() { pending.scheduled = false; self.idle.notify_waiters(); return; }
                #[cfg(test)]
                { gate = pending.gate.take(); }
                std::mem::take(&mut pending.lines)
            };
            #[cfg(test)]
            if let Some((entered, release)) = gate {
                entered.send(()).unwrap();
                release.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
            }
            if append_bounded(blocking, &self.path, &batch).is_err() { failed = Some(batch); }
        }
    }

    #[cfg(test)]
    pub fn wait_idle(&self) -> bool {
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let pending = self.pending.lock().unwrap();
            if !pending.scheduled { return pending.lines.is_empty(); }
            drop(pending);
            assert!(std::time::Instant::now() < until, "telemetry writer did not retire");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }
}

/// Preserve the newer half on rotation, as the original doze-cycle log did.
pub(crate) fn append_bounded(_blocking: &Blocking, path: &Path, lines: &VecDeque<String>) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if lines.is_empty() { return Ok(()); }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_file() => Some(metadata),
        Ok(_) => return Err(std::io::Error::other("telemetry path is not a regular file")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e),
    };
    if metadata.as_ref().is_some_and(|m| m.len() > MAX_BYTES + (MAX_PENDING * (MAX_LINE + 1)) as u64) {
        return Err(std::io::Error::other("oversized telemetry file"));
    }
    if metadata.is_some_and(|m| m.len() > MAX_BYTES) {
        // Bytes, not text: one damaged byte on the card must not refuse
        // every later rotation. (The size was bounded above.)
        let text = std::fs::read(path)?;
        let half = text.len() / 2;
        let keep = text[half..].iter().position(|b| *b == b'\n').map_or(&[][..], |i| &text[half + i + 1..]);
        let tmp = path.with_extension(format!("{:016x}.new", fastrand::u64(..)));
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
        let result = file.write_all(keep).and_then(|_| std::fs::rename(&tmp, path));
        if result.is_err() { let _ = std::fs::remove_file(&tmp); }
        result?;
    }
    let mut file = std::fs::OpenOptions::new().create(true).append(true)
        .mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
    let mut text = lines.iter().map(String::as_str).collect::<Vec<_>>().join("\n");
    text.push('\n');
    file.write_all(text.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn root() -> PathBuf {
        let root = std::env::var_os("Q2_GENERATED_TEST_ROOT").map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build/perf/tests"))
            .join(format!("numeric-log-{}", fastrand::u64(..)));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    #[test]
    fn stalled_writer_does_not_block_executor_and_overflow_leaves_sequence_gaps() {
        let root = root();
        let log = Log::new(root.join("events.log"));
        let (entered, arrival) = std::sync::mpsc::channel();
        let (release, gate) = std::sync::mpsc::channel();
        log.pending.lock().unwrap().gate = Some((entered, gate));
        log.record(|n| n.to_string());
        arrival.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        crate::rt::Ui::new().unwrap().block_on(async {
            for _ in 0..1000 { log.record(|n| n.to_string()); }
            tokio::task::yield_now().await;
            assert!(log.pending.lock().unwrap().lines.len() <= MAX_PENDING);
            assert!(tokio::time::timeout(std::time::Duration::from_millis(5), log.settle()).await.is_err(), "a cancelled wait must not report a stalled writer complete");
        });
        release.send(()).unwrap();
        assert!(crate::rt::Ui::new().unwrap().block_on(log.settle()));
        let rows: Vec<u64> = std::fs::read_to_string(&log.path).unwrap().lines().map(|n| n.parse().unwrap()).collect();
        assert_eq!(rows[0], 1);
        assert_eq!(*rows.last().unwrap(), 1001);
        assert!(rows.windows(2).any(|p| p[1] > p[0] + 1));
        assert!(rows.len() <= MAX_PENDING + 1);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn rotation_survives_bytes_that_are_not_text() {
        // The log is on the card (FAT): a damaged cluster must not stop
        // rotation, and so every later write, for good.
        let root = root();
        let path = root.join("events.log");
        let mut damaged = vec![0xFFu8, 0xFE, b'\n'];
        while damaged.len() <= MAX_BYTES as usize { damaged.extend_from_slice(b"an earlier line\n"); }
        damaged.extend_from_slice(b"the newest line\n");
        std::fs::write(&path, &damaged).unwrap();
        let log = Log::new(path.clone());
        log.record(|n| n.to_string());
        assert!(log.wait_idle(), "the line was not written");
        let kept = std::fs::read(&path).unwrap();
        assert!(kept.len() as u64 <= MAX_BYTES / 2 + 64 && kept.ends_with(b"the newest line\n1\n"), "{} bytes", kept.len());
        assert!(kept.starts_with(b"an earlier line\n"), "rotation keeps whole lines");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failed_writes_retry_on_next_event_without_following_symlinks() {
        let root = root();
        let path = root.join("events.log");
        let sentinel = root.join("sentinel");
        std::fs::write(&sentinel, b"unchanged").unwrap();
        std::os::unix::fs::symlink(&sentinel, &path).unwrap();
        let log = Log::new(path.clone());
        log.record(|n| n.to_string());
        assert!(!log.wait_idle());
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"unchanged");
        std::fs::remove_file(&path).unwrap();
        log.record(|n| n.to_string());
        assert!(log.wait_idle());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "1\n2\n");
        log.record(|_| "x".repeat(MAX_LINE + 1));
        log.record(|n| n.to_string());
        assert!(log.wait_idle());
        assert!(std::fs::read_to_string(path).unwrap().ends_with("2\n4\n"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
