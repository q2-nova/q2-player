//! Owned nonblocking kernel-event sockets. Pointer and descriptor lifetimes
//! stay here; the USB monitor retains event policy and bounded draining.
use std::io;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};

pub(crate) fn kobject_events() -> io::Result<OwnedFd> {
    // SAFETY: socket takes scalar arguments. A successful descriptor is
    // transferred to exactly one owner before any later fallible operation.
    let fd = unsafe { libc::socket(libc::AF_NETLINK,
        libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
        libc::NETLINK_KOBJECT_UEVENT) };
    if fd < 0 { return Err(io::Error::last_os_error()); }
    // SAFETY: the new descriptor is valid and has no other owner.
    let socket = unsafe { OwnedFd::from_raw_fd(fd) };
    // SAFETY: sockaddr_nl is a C scalar structure admitting an all-zero value,
    // including its private padding. The initialized address stays live in bind.
    let mut address: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    address.nl_family = libc::AF_NETLINK as _;
    address.nl_groups = 1;
    // SAFETY: socket owns the live descriptor, and the pointer's exact extent
    // is provided. bind does not retain the address.
    if unsafe { libc::bind(socket.as_raw_fd(), (&address as *const libc::sockaddr_nl).cast(),
        std::mem::size_of_val(&address) as _) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(socket)
}

/// Preserve kernel-sender authentication before the caller interprets bytes.
pub(crate) fn receive_kernel(socket: &impl AsFd, buffer: &mut [u8]) -> io::Result<(usize, bool)> {
    // SAFETY: sockaddr_nl admits zero; recvfrom may write only this exact
    // structure and the exclusive buffer extent supplied below.
    let mut sender: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of_val(&sender) as libc::socklen_t;
    // SAFETY: the borrowed descriptor remains open, both output pointers stay
    // live, and buffer.len() bounds the writable slice. No pointer escapes.
    let count = unsafe { libc::recvfrom(socket.as_fd().as_raw_fd(), buffer.as_mut_ptr().cast(),
        buffer.len(), 0, (&mut sender as *mut libc::sockaddr_nl).cast(), &mut length) };
    if count < 0 { return Err(io::Error::last_os_error()); }
    Ok((count as usize, length as usize == std::mem::size_of_val(&sender) && sender.nl_pid == 0))
}

/// Inspect current error/hangup flags without waiting or changing readiness.
pub(crate) fn read_events(socket: &impl AsFd) -> io::Result<i16> {
    let mut poll = libc::pollfd { fd: socket.as_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 };
    // SAFETY: one initialized pollfd is writable throughout the call; the
    // borrowed descriptor is live and timeout zero never waits.
    if unsafe { libc::poll(&mut poll, 1, 0) } < 0 { return Err(io::Error::last_os_error()); }
    Ok(poll.revents)
}
