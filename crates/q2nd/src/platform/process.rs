//! Linux child-process FFI boundaries. Callers retain owning Command/Child
//! handles; supervisor state and spawn placement stay outside this module.
use std::io;
use std::os::unix::process::CommandExt;
use std::process::Command;

/// Spawn on an I/O-runtime-associated finite worker; the fork/exec handshake
/// is synchronous even though Child::wait is async. Its reactor stays with io.
/// Do not use for parent-death-bound Tailscale children: their spawning thread
/// must remain alive. These sleep/USB helpers deliberately have no PDEATHSIG.
pub(crate) fn spawn_reactor_child(_blocking: &crate::rt::Blocking, mut command: tokio::process::Command)
    -> io::Result<tokio::process::Child> {
    command.spawn()
}

/// Arrange SIGTERM when the Linux thread that spawns this command disappears.
/// Configure anywhere, but spawn on a persistent parent owner: Linux binds the
/// parent-death signal to the spawning thread, not to the configuring thread.
/// Also set the child's timer slack (`PR_SET_TIMERSLACK`) before exec, which
/// its threads inherit; 0 keeps the kernel default (50 µs). Timed sleeps of
/// the child may then end up to `slack_ns` late, and the kernel coalesces
/// their expiries with other timer interrupts.
///
/// `data_limit` caps the child's data segment (`RLIMIT_DATA`, soft and
/// hard): brk and private writable mappings, which for a Go program are its
/// heap arenas, stacks, .data and .bss. A mapping past the cap fails with
/// ENOMEM, so the child exits instead of growing. Address space reserved
/// with PROT_NONE is not counted, so Go's arena reservations (VmSize) are
/// unaffected.
pub(crate) fn supervised_child(command: &mut Command, slack_ns: u64, data_limit: Option<u64>) {
    let parent = std::process::id();
    let slack = slack_ns as libc::c_ulong;
    let data = data_limit.map(|bytes| libc::rlimit { rlim_cur: bytes as libc::rlim_t, rlim_max: bytes as libc::rlim_t });
    // SAFETY: the child callback only invokes Linux syscalls and constructs
    // errno-based errors; it takes no locks or references to parent resources.
    unsafe {
        command.pre_exec(move || {
            if let Some(limit) = &data {
                if libc::setrlimit(libc::RLIMIT_DATA, limit) != 0 {
                    return Err(io::Error::last_os_error());
                }
            }
            if slack != 0 && libc::prctl(libc::PR_SET_TIMERSLACK, slack, 0 as libc::c_ulong, 0 as libc::c_ulong, 0 as libc::c_ulong) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() as u32 != parent {
                return Err(io::Error::from_raw_os_error(libc::ECHILD));
            }
            Ok(())
        });
    }
}

/// Detach the USB helper's session and mark inherited non-stdio FDs CLOEXEC.
/// Do not install a parent-death signal: this helper must survive app shutdown.
pub(crate) fn detached_handoff(command: &mut Command) {
    // SAFETY: only async-signal-safe syscalls/errno conversion after fork.
    // CLOEXEC preserves Rust's exec-error pipe until exec succeeds; closing
    // arbitrary FDs in pre_exec instead would hide helper launch failures.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, 4u32) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Admit and signal only the caller's still-unreaped owned child. No public
/// arbitrary-PID signaling interface: a reaped/exited child is never targeted.
/// Signal errors remain best-effort; the supervisor still waits then kills.
pub(crate) fn terminate_owned_child(child: &mut tokio::process::Child) -> bool {
    if !matches!(child.try_wait(), Ok(None)) {
        return false;
    }
    if let Some(pid) = child.id() {
        // SAFETY: positive PID obtained from this exclusively borrowed,
        // unreaped child; no other caller can wait through this handle here.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }
    true
}

#[cfg(test)]
pub(crate) mod generated {
    use std::io;
    use std::os::fd::{AsFd, AsRawFd};

    /// Bound a generated child's pre-exec handshake to a test-owned socket.
    /// Only async-signal-safe syscalls run after fork; the five-second limit
    /// also releases a failed test without leaving a pre-exec child behind.
    pub(crate) fn exec_barrier(command: &mut std::process::Command,
        barrier: std::os::unix::net::UnixStream) {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(move || {
                let fd = barrier.as_raw_fd();
                let ready = libc::getpid().to_ne_bytes();
                if libc::write(fd, ready.as_ptr().cast(), ready.len()) != ready.len() as isize {
                    return Err(io::Error::last_os_error());
                }
                let mut event = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
                if libc::poll(&mut event, 1, 5000) != 1 {
                    return Err(io::Error::from_raw_os_error(libc::ETIMEDOUT));
                }
                let mut release = [0u8];
                if libc::read(fd, release.as_mut_ptr().cast(), 1) != 1 || release != [1] {
                    return Err(io::Error::from_raw_os_error(libc::ECANCELED));
                }
                Ok(())
            });
        }
    }

    /// Only generated child fixtures call this after their owner has joined.
    pub(crate) fn wait_nonblocking(pid: i32) -> io::Result<i32> {
        if pid <= 0 {
            return Err(io::Error::from_raw_os_error(libc::EINVAL));
        }
        // SAFETY: no status pointer is dereferenced; positive PID excludes
        // process-group / any-child wait modes.
        let result = unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result)
        }
    }

    pub(crate) fn exists(pid: i32) -> bool {
        pid > 0 && unsafe { libc::kill(pid, 0) } == 0
    }

    pub(crate) fn current_thread_id() -> u32 {
        // SAFETY: gettid has no pointer arguments or external resource owner.
        unsafe { libc::syscall(libc::SYS_gettid) as u32 }
    }

    /// Deliberately expose a generated file to prove handoff exec closes it.
    pub(crate) fn make_inheritable(file: &impl AsFd) -> io::Result<()> {
        // SAFETY: the borrowed descriptor remains open throughout fcntl.
        let result = unsafe { libc::fcntl(file.as_fd().as_raw_fd(), libc::F_SETFD, 0) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn supervised_child_slack_reaches_the_child_and_zero_keeps_the_default() {
        let slack = |ns| {
            let mut command = std::process::Command::new("/bin/sh");
            command.args(["-c", "read s < /proc/$$/timerslack_ns; echo $s"]);
            super::supervised_child(&mut command, ns, None);
            String::from_utf8(command.output().unwrap().stdout).unwrap().trim().parse::<u64>().unwrap()
        };
        assert_eq!(slack(1_000_000), 1_000_000);
        assert_ne!(slack(0), 1_000_000);
    }

    #[test]
    #[allow(clippy::disallowed_types, clippy::disallowed_methods)]
    fn data_limit_reaches_the_child() {
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "ulimit -d; ulimit -H -d"]);
        super::supervised_child(&mut command, 0, Some(64 << 20));
        assert_eq!(String::from_utf8(command.output().unwrap().stdout).unwrap(), "65536\n65536\n");
    }
}
