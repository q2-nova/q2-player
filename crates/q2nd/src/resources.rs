//! Packaged resources, with explicit image/card roots for development and tests.
#![forbid(unsafe_code)]

use std::io::{self, Read};
use std::path::{Path, PathBuf};

pub fn directory(kind: &str) -> PathBuf {
    if let Some(root) = std::env::var_os("Q2ND_RESOURCE_ROOT") {
        return PathBuf::from(root).join(kind);
    }
    if let Some(root) = std::env::var_os("Q2ND_FONT_PREFIX") {
        return PathBuf::from(root).join("usr/share/q2nd").join(kind);
    }
    #[cfg(target_arch = "mips")]
    { PathBuf::from("/usr/share/q2nd").join(kind) }
    #[cfg(not(target_arch = "mips"))]
    {
        // Native development uses the same on-disk files as image packaging.
        let source = Path::new(env!("CARGO_MANIFEST_DIR"));
        if kind == "fonts" { source.join("../../assets/fonts") }
        else { source.join("assets").join(kind) }
    }
}

/// Reject non-files and overlong data before parsing. A card-provided FIFO must
/// not park a resource worker in open/read; regular files retain ordinary I/O.
pub fn read(_blocking: &crate::rt::Blocking, path: &Path, limit: usize) -> io::Result<Vec<u8>> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Resource is not a regular file"));
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Resource exceeds its size limit"));
    }
    Ok(bytes)
}
