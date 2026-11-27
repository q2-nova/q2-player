//! Current-context observations that a completed local archive is present.
//!
//! Policy: unproven source crash evidence stays local. This module may produce
//! only a `kernel-archive-observed` collection-health event under the current
//! opt-in context, never a crash event attributed to that context. The wire
//! event must contain source_boot:null and source_context:null, no reason or
//! prior labels, and collecting_boot equal to the outer observation's boot.
//! In particular an old automated-test archive is NOT a production crash.
//!
//! Raw archive bytes are bounded and hashed only, never parsed or forwarded.
//! A latest pmsg marker, archive dirname, timestamp or current context cannot
//! prove the source boot. Prior automated-test crash separation therefore
//! remains unimplemented; these observations are explicitly partial coverage.
use crate::{config::telemetry::valid_uuid, rt::Blocking};
use std::{
    collections::BTreeMap,
    fs::OpenOptions,
    io::{self, Read},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

const MAX_BYTES: u64 = 1_048_576;
const MAX_RECORDS: usize = 32;
pub(super) struct ArchiveObservation {
    pub id: String,
    pub records: u32,
    pub bytes: u64,
    pub collecting_boot: String,
}
fn invalid() -> io::Error {
    io::Error::other("invalid bounded kernel archive receipt")
}
fn regular_directory(_blocking: &Blocking, path: &Path) -> io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => Ok(true),
        Ok(_) => Err(invalid()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}
fn record_name(name: &str) -> bool {
    [
        "dmesg-ramoops-",
        "console-ramoops-",
        "pmsg-ramoops-",
        "ftrace-ramoops-",
    ]
    .iter()
    .any(|prefix| {
        name.strip_prefix(prefix).is_some_and(|suffix| {
            !suffix.is_empty() && suffix.len() <= 10 && suffix.bytes().all(|b| b.is_ascii_digit())
        })
    })
}
fn decimal(value: &str) -> io::Result<u64> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    value.parse().map_err(|_| invalid())
}
fn hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
/// This is a current collection observation, not a source crash report.
/// Only invoke under the currently enabled collection context. The archive
/// root is the maintained /state/crash; only this collecting boot is examined.
/// No boot is inferred from a pmsg record, filename, time, or collector context.
/// Missing/incomplete archive means no summary; invalid completion is an error.
pub(super) fn collect(
    blocking: &Blocking,
    archive_root: &Path,
    collecting_boot: &str,
) -> io::Result<Option<ArchiveObservation>> {
    if !valid_uuid(collecting_boot) {
        return Err(invalid());
    }
    if !regular_directory(blocking,archive_root)? {
        return Ok(None);
    }
    let archive = archive_root.join(collecting_boot);
    if !regular_directory(blocking,&archive)? {
        return Ok(None);
    }
    let complete = match super::bounded(blocking, &archive.join("complete"), 128) {
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let line = std::str::from_utf8(&complete).map_err(|_| invalid())?;
    let parts: Vec<&str> = line
        .strip_suffix('\n')
        .ok_or_else(invalid)?
        .split(' ')
        .collect();
    if parts.len() != 4 || parts[1] != "records" || parts[3] != "bytes" {
        return Err(invalid());
    }
    let count = decimal(parts[0])?;
    let expected_bytes = decimal(parts[2])?;
    if count == 0 || count > MAX_RECORDS as u64 || expected_bytes > MAX_BYTES {
        return Err(invalid());
    }
    let manifest = super::bounded(blocking, &archive.join("SHA256SUMS"), 4096)?;
    let manifest_text = std::str::from_utf8(&manifest).map_err(|_| invalid())?;
    if !manifest_text.ends_with('\n') {
        return Err(invalid());
    }
    let mut records = BTreeMap::new();
    for row in manifest_text
        .strip_suffix('\n')
        .ok_or_else(invalid)?
        .split('\n')
    {
        let (hash, name) = row.split_once("  ").ok_or_else(invalid)?;
        if !hex(hash)
            || !record_name(name)
            || records.insert(name, hash).is_some()
            || records.len() > MAX_RECORDS
        {
            return Err(invalid());
        }
    }
    if records.len() != count as usize {
        return Err(invalid());
    }
    let mut entries = 0;
    for entry in std::fs::read_dir(&archive)? {
        let entry = entry?;
        entries += 1;
        if entries > MAX_RECORDS + 2 || !entry.file_type()?.is_file() {
            return Err(invalid());
        }
        let name = entry.file_name();
        let name = name.to_str().ok_or_else(invalid)?;
        if name != "complete" && name != "SHA256SUMS" && !records.contains_key(name) {
            return Err(invalid());
        }
    }
    if entries != records.len() + 2 {
        return Err(invalid());
    }
    let mut bytes = 0;
    for (name, expected) in &records {
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(archive.join(name))?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.nlink() != 1 || metadata.len() > MAX_BYTES {
            return Err(invalid());
        }
        bytes += metadata.len();
        if bytes > MAX_BYTES {
            return Err(invalid());
        }
        let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
        let mut buffer = [0u8; 8192];
        let mut read = 0u64;
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            read += n as u64;
            if read > metadata.len() {
                return Err(invalid());
            }
            hash.update(&buffer[..n]);
        }
        if read != metadata.len() {
            return Err(invalid());
        }
        let actual: String = hash
            .finish()
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if actual != *expected {
            return Err(invalid());
        }
    }
    if bytes != expected_bytes {
        return Err(invalid());
    }
    // Stable identity binds exactly one completed collection archive. Repeated
    // collection under a different context must not silently relabel that ID.
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    hash.update(b"q2-kernel-summary-v2:");
    hash.update(collecting_boot.as_bytes());
    hash.update(&complete);
    hash.update(&manifest);
    let hash = hash.finish();
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(&hash.as_ref()[..16]);
    uuid[6] = (uuid[6] & 15) | 80;
    uuid[8] = (uuid[8] & 63) | 128;
    let hex: String = uuid.iter().map(|b| format!("{b:02x}")).collect();
    let id = format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    );
    Ok(Some(ArchiveObservation {
        id,
        records: count as u32,
        bytes,
        collecting_boot: collecting_boot.into(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    const BOOT: &str = "12345678-1234-1234-1234-123456789abc";
    fn fixture() -> std::path::PathBuf {
        let root = crate::library::generated_test_directory("kernel-summary");
        std::fs::create_dir_all(root.join(BOOT)).unwrap();
        root
    }
    fn archive(root: &Path, items: &[(&str, &[u8])]) {
        let path = root.join(BOOT);
        let mut manifest = String::new();
        let mut bytes = 0;
        for (name, content) in items {
            std::fs::write(path.join(name), content).unwrap();
            bytes += content.len();
            let hash: String = ring::digest::digest(&ring::digest::SHA256, content)
                .as_ref()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            manifest.push_str(&format!("{hash}  {name}\n"));
        }
        std::fs::write(path.join("SHA256SUMS"), manifest).unwrap();
        std::fs::write(
            path.join("complete"),
            format!("{} records {bytes} bytes\n", items.len()),
        )
        .unwrap();
    }
    fn summary(root: &Path) -> io::Result<Option<ArchiveObservation>> {
        collect(&Blocking::dedicated_thread(), root, BOOT)
    }
    #[test]
    fn completed_archive_yields_only_numbers_and_stale_pmsg_never_becomes_provenance() {
        let root = fixture();
        let private = b"PANIC secret-canary address=deadbeef token=private";
        let stale=b"Q2_CONTEXT {\"boot\":\"12345678-1234-1234-1234-123456789def\",\"stream\":\"automated-test\"}";
        archive(
            &root,
            &[("dmesg-ramoops-0", private), ("pmsg-ramoops-0", stale)],
        );
        let value = summary(&root).unwrap().unwrap();
        assert_eq!(value.records, 2);
        assert_eq!(value.bytes, (private.len() + stale.len()) as u64);
        assert_eq!(value.collecting_boot, BOOT);
        assert!(valid_uuid(&value.id));
        assert_eq!(value.id, summary(&root).unwrap().unwrap().id);
        assert_eq!(
            std::fs::read(root.join(BOOT).join("dmesg-ramoops-0")).unwrap(),
            private
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn actual_installed_collector_receipt_is_accepted_without_reading_payload_as_text() {
        let root = fixture();
        // The real collector owns archive creation; fixture() made only the
        // generated root and an empty placeholder collecting-boot directory.
        std::fs::remove_dir(root.join(BOOT)).unwrap();
        for name in ["pstore", "run", "proc/sys/kernel/random"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        std::fs::write(root.join("run/state-status"), b"ready\n").unwrap();
        std::fs::write(
            root.join("proc/sys/kernel/random/boot_id"),
            format!("{BOOT}\n"),
        )
        .unwrap();
        let payload = b"SYNTHETIC_PRIVATE_PSTORE_CANARY\x00\xff";
        std::fs::write(root.join("pstore/dmesg-ramoops-0"), payload).unwrap();
        let helper = std::env::var_os("Q2_PSTORE_COLLECTOR")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../tests/fixtures/q2-pstore"));
        let mut child = std::process::Command::new("sh")
            .args([
                "-c",
                r#"Q2_PSTORE_SOURCE_ONLY=1
. "$Q2_FIXTURE_HELPER"
PSTORE=$Q2_FIXTURE_ROOT/pstore
STATE=$Q2_FIXTURE_ROOT
PROC=$Q2_FIXTURE_ROOT/proc
RUN=$Q2_FIXTURE_ROOT/run
sync() { :; }
pstore_collect
"#,
            ])
            .env("Q2_FIXTURE_HELPER", helper)
            .env("Q2_FIXTURE_ROOT", root.canonicalize().unwrap())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                panic!("generated collector timed out");
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let value = collect(&Blocking::dedicated_thread(), &root.join("crash"), BOOT)
            .unwrap()
            .unwrap();
        assert_eq!(value.records, 1);
        assert_eq!(value.bytes, payload.len() as u64);
        assert!(!root.join("pstore/dmesg-ramoops-0").exists());
        assert_eq!(
            std::fs::read(root.join("crash").join(BOOT).join("dmesg-ramoops-0")).unwrap(),
            payload
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn absent_incomplete_and_unsafe_receipts_never_mean_success() {
        let root = fixture();
        assert!(summary(&root).unwrap().is_none());
        archive(&root, &[("dmesg-ramoops-0", b"generated")]);
        let path = root.join(BOOT).join("complete");
        for line in [
            "1 records 8 bytes",
            "0 records 0 bytes\n",
            "33 records 8 bytes\n",
            "1 records 1048577 bytes\n",
            "1 records 10 bytes\n",
            "1 records 8 bytes\nsecret",
        ] {
            std::fs::write(&path, line).unwrap();
            assert!(summary(&root).is_err());
        }
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn changed_raw_bytes_symlinks_and_manifest_paths_are_rejected() {
        let root = fixture();
        archive(&root, &[("dmesg-ramoops-0", b"generated")]);
        let path = root.join(BOOT);
        std::fs::write(path.join("dmesg-ramoops-0"), b"different").unwrap();
        assert!(summary(&root).is_err());
        archive(&root, &[("dmesg-ramoops-0", b"generated")]);
        std::fs::write(
            path.join("SHA256SUMS"),
            format!("{}  ../outside\n", "a".repeat(64)),
        )
        .unwrap();
        assert!(summary(&root).is_err());
        archive(&root, &[("dmesg-ramoops-0", b"generated")]);
        std::fs::remove_file(path.join("dmesg-ramoops-0")).unwrap();
        std::os::unix::fs::symlink("missing", path.join("dmesg-ramoops-0")).unwrap();
        assert!(summary(&root).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
