use std::{fs::File, io::{self, Read}};

#[cfg(target_os = "linux")]
use std::{fs::OpenOptions, os::unix::fs::OpenOptionsExt, path::Path};

use crate::rt::Blocking;
use serde::Deserialize;

pub(super) fn executable(blocking: &Blocking) -> io::Result<String> {
    let file = File::open("/proc/self/exe")?;
    #[cfg(target_os = "linux")]
    if let Some(build) = bundled(blocking, &file, Path::new("/")).or_else(|| selected(blocking, &file, Path::new("/"))) {
        return Ok(build);
    }
    // Direct development launches and older launchers have no attestation.
    // Keep their existing exact crash-report identity.
    super::hash_file(file)
}

#[derive(Deserialize)]
struct ReleaseIdentity {
    profile: String,
    app_sha256: String,
}

fn read_bounded(_blocking: &Blocking, file: impl Read, limit: u64) -> Option<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes).ok()?;
    (bytes.len() as u64 <= limit).then_some(bytes)
}

fn valid_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn release_digest(blocking: &Blocking, file: impl Read) -> Option<String> {
    let release: ReleaseIdentity = serde_json::from_slice(&read_bounded(blocking, file, 4096)?).ok()?;
    (release.profile == "split-installed-candidate"
        && valid_digest(&release.app_sha256))
        .then_some(release.app_sha256)
}

#[cfg(target_os = "linux")]
fn open_path(_blocking: &Blocking, path: &Path) -> Option<File> {
    OpenOptions::new().read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path).ok()
}

#[cfg(target_os = "linux")]
fn squashfs(file: &File) -> bool {
    use std::{mem::MaybeUninit, os::fd::AsRawFd};
    let mut fs = MaybeUninit::<libc::statfs>::uninit();
    const SQUASHFS_MAGIC: libc::c_long = 0x7371_7368;
    unsafe {
        libc::fstatfs(file.as_raw_fd(), fs.as_mut_ptr()) == 0
            && fs.assume_init().f_type == SQUASHFS_MAGIC
    }
}

#[cfg(target_os = "linux")]
fn executable_mount(blocking: &Blocking, running: &File, canonical: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    // A read-only mount of a mutable filesystem cannot attest these bytes.
    if !squashfs(running) {
        return None;
    }
    let canonical = open_path(blocking, canonical)?;
    let actual = running.metadata().ok()?;
    let selected = canonical.metadata().ok()?;
    let mount = mount_id(blocking, running)?;
    (actual.is_file() && selected.is_file() && actual.dev() == selected.dev()
        && actual.ino() == selected.ino() && mount_id(blocking, &canonical) == Some(mount))
        .then_some(mount)
}

#[cfg(target_os = "linux")]
fn bundled(blocking: &Blocking, running: &File, root: &Path) -> Option<String> {
    let mount = executable_mount(blocking, running, &root.join("usr/bin/q2nd"))?;
    let root_directory = File::open(root).ok()?;
    let release = open_path(blocking, &root.join("etc/q2-os-release.json"))?;
    if mount_id(blocking, &root_directory) != Some(mount) || mount_id(blocking, &release) != Some(mount)
        || !release.metadata().ok()?.is_file()
    {
        return None;
    }
    // The image producer records the digest of the packaged canonical app.
    // All descriptors above belong to that same immutable root mount.
    release_digest(blocking, release)
}

#[cfg(target_os = "linux")]
fn selected(blocking: &Blocking, running: &File, root: &Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let mount = executable_mount(blocking, running, &root.join("run/q2-app/q2nd"))?;
    let runtime = root.join("run/q2nd");
    let directory = open_path(blocking, &runtime)?.metadata().ok()?;
    if !directory.is_dir() || directory.uid() != 0 || directory.mode() & 0o022 != 0 {
        return None;
    }
    let receipt = open_path(blocking, &runtime.join("app-build"))?;
    let source = receipt.metadata().ok()?;
    if !source.is_file() || source.uid() != 0 || source.mode() & 0o022 != 0 {
        return None;
    }
    selection_digest(blocking, receipt, mount)
}

#[cfg(target_os = "linux")]
fn selection_digest(blocking: &Blocking, file: impl Read, mount: u64) -> Option<String> {
    let record = read_bounded(blocking, file, 128)?;
    let record = std::str::from_utf8(&record).ok()?;
    let (record_mount, digest) = record.strip_prefix("Q2APPBUILD1 ")?
        .strip_suffix('\n')?.split_once(' ')?;
    (record_mount == mount.to_string() && valid_digest(digest)).then(|| digest.to_owned())
}

#[cfg(target_os = "linux")]
fn mount_id(_blocking: &Blocking, file: &File) -> Option<u64> {
    use std::os::fd::AsRawFd;
    let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", file.as_raw_fd())).ok()?;
    info.lines().find_map(|line| line.strip_prefix("mnt_id:")?.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_digest_requires_packager_profile_and_lowercase_sha256() {
        let blocking = Blocking::dedicated_thread();
        let digest = "ab".repeat(32);
        let parse = |profile: &str, hash: &str| {
            let input = serde_json::json!({"profile": profile, "app_sha256": hash});
            release_digest(&blocking, input.to_string().as_bytes())
        };
        assert_eq!(parse("split-installed-candidate", &digest), Some(digest.clone()));
        assert_eq!(parse("unrecognized", &digest), None);
        assert_eq!(parse("split-installed-candidate", &digest.to_ascii_uppercase()), None);
        assert_eq!(parse("split-installed-candidate", &digest[..63]), None);
        assert_eq!(release_digest(&blocking, vec![b' '; 4097].as_slice()), None);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn mutable_root_does_not_supply_executable_identity() {
        let blocking = Blocking::dedicated_thread();
        let root = crate::library::generated_test_directory("diagnostic-identity");
        std::fs::create_dir_all(root.join("usr/bin")).unwrap();
        std::fs::create_dir_all(root.join("etc")).unwrap();
        let app = root.join("usr/bin/q2nd");
        std::fs::write(&app, b"generated executable").unwrap();
        std::fs::write(root.join("etc/q2-os-release.json"), serde_json::json!({
            "profile": "split-installed-candidate", "app_sha256": "a".repeat(64)
        }).to_string()).unwrap();
        let file = File::open(app).unwrap();
        assert!(mount_id(&blocking, &file).is_some());
        assert_eq!(bundled(&blocking, &file, &root), None);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn update_receipt_requires_exact_mount_and_digest() {
        let blocking = Blocking::dedicated_thread();
        let digest = "ab".repeat(32);
        let record = format!("Q2APPBUILD1 17 {digest}\n");
        assert_eq!(selection_digest(&blocking, record.as_bytes(), 17), Some(digest.clone()));
        assert_eq!(selection_digest(&blocking, record.as_bytes(), 18), None);
        for malformed in [
            format!("Q2APPBUILD1 017 {digest}\n"),
            format!("Q2APPBUILD1 17 {digest} \n"),
            format!("Q2APPBUILD1 17 {digest}"),
            format!("Q2APPBUILD1 17 {}\n", digest.to_ascii_uppercase()),
            format!("{record}{record}"),
        ] {
            assert_eq!(selection_digest(&blocking, malformed.as_bytes(), 17), None);
        }
    }
}
