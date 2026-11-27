//! A pathname belongs to this socket only after bind succeeds. Cancellation
//! retains cleanup ownership; executor Drop schedules an existing finite job.
use crate::rt::{self, Blocking};
use std::path::{Path, PathBuf};
use tokio::net::UnixDatagram;

pub(super) struct SocketPath(Option<PathBuf>);

pub(super) fn bind(_blocking: &Blocking, path: &Path) -> std::io::Result<(UnixDatagram, SocketPath)> {
    let socket = UnixDatagram::bind(path)?;
    Ok((socket, SocketPath(Some(path.to_owned()))))
}

fn unlink(_blocking: &Blocking, path: PathBuf) {
    let _ = std::fs::remove_file(path);
}

impl SocketPath {
    /// Normal completion waits for removal; cancellation still schedules it.
    pub(super) async fn close(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = rt::spawn_blocking(move |blocking| unlink(blocking, path)).await;
        }
    }
}
impl Drop for SocketPath {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            rt::retire_blocking(move |blocking| unlink(blocking, path));
        }
    }
}
