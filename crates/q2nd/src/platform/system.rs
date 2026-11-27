//! Small POSIX boundaries shared by otherwise safe application modules.
//! These wrappers own all pointer lifetimes; filesystem calls remain subject
//! to the caller's existing blocking-worker/startup admission.
use std::{ffi::CString,io,path::Path,os::unix::ffi::OsStrExt};

pub(crate) fn filesystem(_blocking: &crate::rt::Blocking, path:&Path)->io::Result<libc::statvfs> {
    let path=CString::new(path.as_os_str().as_bytes()).map_err(|_|io::Error::new(io::ErrorKind::InvalidInput,"invalid filesystem path"))?;
    // SAFETY: statvfs is a C output structure whose fields admit zero. Both
    // pointers remain valid for this synchronous call and are not retained.
    unsafe {
        let mut stat:libc::statvfs=std::mem::zeroed();
        if libc::statvfs(path.as_ptr(),&mut stat)!=0 {return Err(io::Error::last_os_error());}
        Ok(stat)
    }
}

/// Renames an entry without replacing any present destination.
pub(crate) fn rename_noreplace(_blocking: &crate::rt::Blocking, from: &Path, to: &Path) -> io::Result<()> {
    let c_path = |path: &Path| {
        CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid filesystem path"))
    };
    let from = c_path(from)?;
    let to = c_path(to)?;
    // SAFETY: both C strings are NUL-terminated and live throughout the
    // synchronous call; AT_FDCWD needs no borrowed directory handles.
    let result = unsafe {
        libc::syscall(libc::SYS_renameat2, libc::AT_FDCWD, from.as_ptr(),
                      libc::AT_FDCWD, to.as_ptr(), libc::RENAME_NOREPLACE)
    };
    if result == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

fn clock_time(clock:libc::clockid_t)->io::Result<libc::timespec> {
    let mut time=libc::timespec {tv_sec:0,tv_nsec:0};
    // SAFETY: an initialized writable timespec lives throughout the call.
    if unsafe {libc::clock_gettime(clock,&mut time)}!=0 {return Err(io::Error::last_os_error());}
    Ok(time)
}
pub(crate) fn monotonic_time() -> Option<std::time::Duration> {
    let time = clock_time(libc::CLOCK_MONOTONIC).ok()?;
    Some(std::time::Duration::new(time.tv_sec as u64, time.tv_nsec as u32))
}
#[cfg(test)]
pub(crate) fn clock_nanos(clock:libc::clockid_t)->io::Result<u64> {
    let time=clock_time(clock)?;
    let seconds=u64::try_from(time.tv_sec).map_err(|_|io::Error::other("invalid clock"))?;
    let nanos=u64::try_from(time.tv_nsec).ok().filter(|n|*n<1_000_000_000).ok_or_else(||io::Error::other("invalid clock"))?;
    seconds.checked_mul(1_000_000_000).and_then(|n|n.checked_add(nanos)).ok_or_else(||io::Error::other("clock overflow"))
}
pub(crate) fn boottime_us()->Option<u64> {
    boottime_us_checked().ok()
}
pub(crate) fn boottime_us_checked()->io::Result<u64> {
    // Keep the existing hot-path arithmetic: do not construct full nanoseconds
    // only to divide a wider integer back to microseconds on 32-bit MIPS.
    clock_time(libc::CLOCK_BOOTTIME).map(|t|t.tv_sec as u64*1_000_000+t.tv_nsec as u64/1000)
}

pub(crate) fn effective_uid()->libc::uid_t {
    // SAFETY: geteuid has no pointers or caller preconditions.
    unsafe { libc::geteuid() }
}

/// Sets the calling thread's nice value. Linux keeps nice per thread, and
/// PRIO_PROCESS with `who` 0 names the calling thread only (setpriority(2),
/// BUGS); the process's other threads keep theirs. Threads it creates later
/// inherit it. A value below the current one needs CAP_SYS_NICE (q2nd runs as
/// root on the Q2) or an RLIMIT_NICE that allows it; without either the call
/// fails with EACCES and nothing changes.
pub(crate) fn set_thread_nice(nice:i32)->io::Result<()> {
    // SAFETY: setpriority takes no pointers and changes only the calling
    // thread's scheduling weight.
    if unsafe {libc::setpriority(libc::PRIO_PROCESS,0,nice)}!=0 {return Err(io::Error::last_os_error());}
    Ok(())
}

/// Open relative to a retained directory, transferring the new descriptor's
/// ownership exactly once. Callers select their existing no-follow/create flags.
pub(crate) fn open_at(_blocking: &crate::rt::Blocking, directory:&impl std::os::fd::AsFd,name:&std::ffi::CStr,flags:i32,mode:libc::mode_t)->io::Result<std::fs::File> {
    use std::os::fd::{AsRawFd,FromRawFd};
    // SAFETY: the borrowed directory and NUL-terminated name stay live; mode
    // is supplied even when creation flags are absent. Only a new valid fd is owned.
    let fd=unsafe {libc::openat(directory.as_fd().as_raw_fd(),name.as_ptr(),flags,mode)};
    if fd<0 {return Err(io::Error::last_os_error());}
    Ok(unsafe {std::fs::File::from_raw_fd(fd)})
}

pub(crate) fn link_at(_blocking: &crate::rt::Blocking, directory:&impl std::os::fd::AsFd,from:&std::ffi::CStr,to:&std::ffi::CStr)->io::Result<()> {
    use std::os::fd::AsRawFd;
    let fd=directory.as_fd().as_raw_fd();
    // SAFETY: names and the borrowed fd remain live through this synchronous call.
    if unsafe {libc::linkat(fd,from.as_ptr(),fd,to.as_ptr(),0)}!=0 {return Err(io::Error::last_os_error());}
    Ok(())
}

pub(crate) fn unlink_at(_blocking: &crate::rt::Blocking, directory:&impl std::os::fd::AsFd,name:&std::ffi::CStr)->io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: the borrowed fd and NUL-terminated name remain live.
    if unsafe {libc::unlinkat(directory.as_fd().as_raw_fd(),name.as_ptr(),0)}!=0 {return Err(io::Error::last_os_error());}
    Ok(())
}

pub(crate) fn local_time()->libc::tm {
    // SAFETY: null requests time's value without an output pointer; localtime_r
    // writes into the live initialized tm and does not retain either pointer.
    // Keep the existing zero fallback if the platform cannot convert the time.
    unsafe {
        let time=libc::time(std::ptr::null_mut());
        let mut local:libc::tm=std::mem::zeroed();
        libc::localtime_r(&time,&mut local);
        local
    }
}
pub(crate) fn uname()->io::Result<libc::utsname> {
    // SAFETY: utsname contains only fixed C character arrays. The output
    // pointer is live and writable for the synchronous syscall.
    unsafe {
        let mut name:libc::utsname=std::mem::zeroed();
        if libc::uname(&mut name)!=0 {return Err(io::Error::last_os_error());}
        Ok(name)
    }
}
pub(crate) fn process_usage()->libc::rusage {
    // SAFETY: rusage's C scalar/time fields admit zero and the live output
    // pointer is not retained. Preserve the diagnostic's old zero fallback.
    unsafe {
        let mut usage:libc::rusage=std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF,&mut usage);
        usage
    }
}
pub(crate) fn create_fifo(path:&Path,mode:libc::mode_t)->io::Result<()> {
    let path=CString::new(path.as_os_str().as_bytes()).map_err(|_|io::Error::new(io::ErrorKind::InvalidInput,"invalid FIFO path"))?;
    // SAFETY: the NUL-terminated name lives through mkfifo; no pointer escapes.
    if unsafe {libc::mkfifo(path.as_ptr(),mode)}!=0 {return Err(io::Error::last_os_error());}
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::FileTypeExt;
    #[test]
    fn system_readbacks_preserve_clock_and_filesystem_domains() {
        let first=boottime_us().unwrap();assert!(boottime_us().unwrap()>=first);
        assert!(clock_nanos(i32::MAX).is_err());
        let time=local_time();assert!((0..24).contains(&time.tm_hour));assert!((0..60).contains(&time.tm_min));assert!((0..=60).contains(&time.tm_sec));
        assert_ne!(uname().unwrap().release[0],0);
        let fs=filesystem(&crate::rt::Blocking::dedicated_thread(), Path::new("/")).unwrap();assert!(fs.f_frsize>0 && fs.f_bfree<=fs.f_blocks && fs.f_bavail<=fs.f_bfree);
        assert_eq!(filesystem(&crate::rt::Blocking::dedicated_thread(), Path::new("invalid\0path")).err().unwrap().kind(),io::ErrorKind::InvalidInput);
        let usage=process_usage();assert!(usage.ru_utime.tv_sec>=0 && usage.ru_stime.tv_sec>=0);
    }
    /// The calling thread's nice value, field 19 of its stat line.
    fn thread_nice()->i32 {
        let stat=std::fs::read_to_string("/proc/thread-self/stat").unwrap();
        let fields:Vec<&str>=stat[stat.rfind(')').unwrap()+2..].split(' ').collect();
        fields[19-3].parse().unwrap()
    }
    #[test]
    fn thread_nice_changes_only_the_calling_thread_and_a_refused_raise_changes_nothing() {
        let before=thread_nice();
        let (lowered,raised)=std::thread::spawn(move || {
            // Lowering the priority needs no privilege.
            set_thread_nice(before+5).unwrap();
            let lowered=thread_nice();
            // Raising it needs CAP_SYS_NICE: it takes, or fails as not permitted and leaves the value.
            let raised=set_thread_nice(-10).map(|()|thread_nice()).map_err(|error|(error.kind(),thread_nice()));
            (lowered,raised)
        }).join().unwrap();
        assert_eq!(lowered,(before+5).min(19));
        match raised {
            Ok(nice)=>assert_eq!(nice,-10),
            Err((kind,nice))=>{assert_eq!(kind,io::ErrorKind::PermissionDenied);assert_eq!(nice,lowered);}
        }
        assert_eq!(thread_nice(),before,"the other threads keep their nice value");
    }
    #[test]
    fn fifo_creation_refuses_existing_paths_without_following_or_replacing() {
        let base=std::env::var_os("Q2_GENERATED_TEST_ROOT").map(std::path::PathBuf::from)
            .unwrap_or_else(||Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build/perf/tests"));
        let root=base.join(format!("system-fifo-{}-{}",std::process::id(),fastrand::u64(..)));
        std::fs::create_dir_all(&root).unwrap();let fifo=root.join("control");
        create_fifo(&fifo,0o600).unwrap();assert!(std::fs::symlink_metadata(&fifo).unwrap().file_type().is_fifo());
        assert_eq!(create_fifo(&fifo,0o600).unwrap_err().kind(),io::ErrorKind::AlreadyExists);
        let target=root.join("target");std::fs::write(&target,"sentinel").unwrap();
        let link=root.join("link");std::os::unix::fs::symlink(&target,&link).unwrap();
        assert!(create_fifo(&link,0o600).is_err());assert_eq!(std::fs::read_to_string(target).unwrap(),"sentinel");
        assert_eq!(create_fifo(Path::new("bad\0path"),0o600).unwrap_err().kind(),io::ErrorKind::InvalidInput);
        std::fs::remove_dir_all(root).unwrap();
    }
}


/// Generated controls inspect the borrowed descriptor through fcntl rather
/// than procfs: qemu-user translates F_GETFL to the guest's O_NONBLOCK ABI.
#[cfg(test)]
pub(crate) fn descriptor_is_nonblocking(descriptor: &impl std::os::fd::AsFd) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;
    // SAFETY: the borrowed descriptor remains live; F_GETFL has no pointer
    // arguments and does not change descriptor state or ownership.
    let flags = unsafe { libc::fcntl(descriptor.as_fd().as_raw_fd(), libc::F_GETFL) };
    if flags < 0 { return Err(std::io::Error::last_os_error()); }
    Ok(flags & libc::O_NONBLOCK != 0)
}
