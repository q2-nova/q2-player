//! The controller's UART: a raw, non-blocking tty at 8 data bits and even
//! parity (Realtek's H5 setting), and `poll` over it and the wake socket.
//! The `unsafe` here is the termios and poll FFI on descriptors this module
//! owns or borrows for the duration of the call.
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, BorrowedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use crate::rt::Blocking;

pub struct Uart { file: File, /// N of `/dev/ttySN`, for `/proc/tty/driver/serial`.
    index: Option<u32> }

fn speed(baud: u32) -> io::Result<libc::speed_t> {
    Ok(match baud {
        115_200 => libc::B115200, 230_400 => libc::B230400, 921_600 => libc::B921600, 1_000_000 => libc::B1000000,
        1_500_000 => libc::B1500000, 2_000_000 => libc::B2000000, 3_000_000 => libc::B3000000, 4_000_000 => libc::B4000000,
        _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, "unsupported UART rate")),
    })
}

fn check(result: libc::c_int) -> io::Result<()> { if result < 0 { Err(io::Error::last_os_error()) } else { Ok(()) } }

/// Waits until one of `fds` is readable or `timeout_ms` passes; returns which are readable.
/// A hung-up or failed descriptor counts as readable so its reader sees the error.
pub fn wait_readable<const N: usize>(_blocking: &Blocking, fds: [BorrowedFd<'_>; N], timeout_ms: i32) -> io::Result<[bool; N]> {
    let mut polled = fds.map(|fd| libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 });
    // SAFETY: `polled` is a live array of N initialized pollfd; the descriptors are borrowed for this call.
    let result = unsafe { libc::poll(polled.as_mut_ptr(), N as libc::nfds_t, timeout_ms) };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted { return Err(error) }
        return Ok([false; N]);
    }
    Ok(polled.map(|p| p.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0))
}

fn wait_writable(_blocking: &Blocking, fd: BorrowedFd<'_>, timeout_ms: i32) -> io::Result<bool> {
    let mut polled = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLOUT, revents: 0 };
    // SAFETY: one live, initialized pollfd; the descriptor is borrowed for this call.
    let result = unsafe { libc::poll(&mut polled, 1, timeout_ms) };
    if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted { return Err(io::Error::last_os_error()) }
    Ok(result > 0 && polled.revents & libc::POLLOUT != 0)
}

/// Writes all of `bytes` to a non-blocking descriptor, waiting up to two seconds for room.
pub fn write_all(blocking: &Blocking, mut file: &File, mut bytes: &[u8]) -> io::Result<()> {
    let mut stalled = 0;
    while !bytes.is_empty() {
        match file.write(bytes) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(n) => { bytes = &bytes[n..]; stalled = 0; }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                stalled += 1;
                if stalled > 20 { return Err(io::Error::new(io::ErrorKind::TimedOut, "UART does not take data")) }
                use std::os::fd::AsFd;
                wait_writable(blocking, file.as_fd(), 100)?;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Raw mode, 8 data bits, even parity, one stop bit, modem lines ignored, non-blocking reads.
fn raw_8e1(termios: &mut libc::termios, speed: libc::speed_t, flow: bool) -> io::Result<()> {
    // SAFETY: cfmakeraw only modifies the termios it is given.
    unsafe { libc::cfmakeraw(termios); }
    termios.c_cflag |= libc::CLOCAL | libc::CREAD | libc::CS8 | libc::PARENB;
    termios.c_cflag &= !(libc::PARODD | libc::CSTOPB | libc::CRTSCTS);
    if flow { termios.c_cflag |= libc::CRTSCTS; }
    termios.c_cc[libc::VMIN] = 0;
    termios.c_cc[libc::VTIME] = 0;
    // SAFETY: cfsetspeed only modifies the termios it is given.
    check(unsafe { libc::cfsetspeed(termios, speed) })
}

/// Makes a descriptor non-blocking.
fn nonblocking(fd: BorrowedFd<'_>) -> io::Result<()> {
    // SAFETY: fcntl with F_GETFL/F_SETFL on a borrowed open descriptor; no memory arguments.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
    check(flags)?;
    // SAFETY: as above.
    check(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) })
}

/// A pipe that wakes the thread out of `wait_readable`; neither end blocks.
pub fn wake_pipe() -> io::Result<(io::PipeReader, io::PipeWriter)> {
    use std::os::fd::AsFd;
    let (reader, writer) = io::pipe()?;
    nonblocking(reader.as_fd())?;
    nonblocking(writer.as_fd())?;
    Ok((reader, writer))
}

/// A pipe whose reader does not block and whose writer does (the fake controller's byte paths).
pub fn data_pipe() -> io::Result<(io::PipeReader, io::PipeWriter)> {
    use std::os::fd::AsFd;
    let (reader, writer) = io::pipe()?;
    nonblocking(reader.as_fd())?;
    Ok((reader, writer))
}

/// Wakes the reader's thread. The pipe is non-blocking: when it is full the thread is already due to wake.
pub fn signal(writer: &io::PipeWriter) { let _ = (&*writer).write(&[1]); }

impl Uart {
    /// Opens the tty without making it the controlling terminal, at 115200 8E1.
    pub fn open(_blocking: &Blocking, path: &Path) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new().read(true).write(true).custom_flags(libc::O_NOCTTY | libc::O_NONBLOCK).open(path)?;
        let index = path.file_name().and_then(|n| n.to_str()).and_then(|n| n.strip_prefix("ttyS")).and_then(|n| n.parse().ok());
        let uart = Self { file, index };
        uart.configure(q2_bt::host::INITIAL_BAUD, false)?;
        // SAFETY: tcflush on our open descriptor; it has no memory arguments.
        check(unsafe { libc::tcflush(uart.file.as_raw_fd(), libc::TCIOFLUSH) })?;
        Ok(uart)
    }
    pub fn file(&self) -> &File { &self.file }

    /// Sends what is queued at the old rate, then sets raw 8E1 at `baud`.
    /// Returns the rate the kernel applied.
    pub fn configure(&self, baud: u32, flow: bool) -> io::Result<libc::speed_t> {
        let fd = self.file.as_raw_fd();
        let speed = speed(baud)?;
        // SAFETY: termios is plain data; tcgetattr initializes it before it is read.
        let mut termios: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: `fd` is our open descriptor and `termios` a live, writable termios.
        check(unsafe { libc::tcgetattr(fd, &mut termios) })?;
        raw_8e1(&mut termios, speed, flow)?;
        // SAFETY: `fd` is our open descriptor; TCSADRAIN applies the settings after queued output is sent.
        // `termios` stays live and writable: the kernel's settings are read back into it (a rate the
        // driver cannot reach comes back as the rate it kept), and cfgetospeed reads it.
        let applied = unsafe {
            if libc::tcsetattr(fd, libc::TCSADRAIN, &termios) < 0 || libc::tcgetattr(fd, &mut termios) < 0 { None } else { Some(libc::cfgetospeed(&termios)) }
        };
        applied.ok_or_else(io::Error::last_os_error)
    }

    /// Whether the kernel takes `baud` (a mainline 8250 without the UMR patch
    /// clamps anything above 1.5 Mbaud). Leaves the port at 115200, as opened.
    pub fn accepts(&self, baud: u32) -> bool {
        let Ok(wanted) = speed(baud) else { return false };
        let took = self.configure(baud, false).is_ok_and(|applied| applied == wanted);
        let _ = self.configure(q2_bt::host::INITIAL_BAUD, false);
        took
    }

    /// The rate code and whether input is raw (no canonical mode, echo or signals).
    #[cfg(test)]
    fn settings(&self) -> io::Result<(libc::speed_t, bool)> {
        // SAFETY: as in `configure`.
        let mut termios: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: as in `configure`.
        check(unsafe { libc::tcgetattr(self.file.as_raw_fd(), &mut termios) })?;
        // SAFETY: cfgetospeed reads the termios it is given.
        let speed = unsafe { libc::cfgetospeed(&termios) };
        Ok((speed, termios.c_lflag & (libc::ICANON | libc::ECHO | libc::ISIG) == 0))
    }

    /// Reads what has arrived; zero when nothing has.
    pub fn read(&self, blocking: &Blocking, buffer: &mut [u8]) -> io::Result<usize> { read_ready(blocking, &self.file, buffer) }

    /// The kernel's receive error counts and modem lines for the port, from
    /// `/proc/tty/driver/serial` (root only; a pseudo-terminal has no line there).
    pub fn line_state(&self, _blocking: &Blocking) -> io::Result<LineState> {
        let index = self.index.ok_or_else(|| io::Error::new(io::ErrorKind::Unsupported, "not a ttyS port"))?;
        let text = std::fs::read_to_string("/proc/tty/driver/serial")?;
        parse_serial_proc(&text, index).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "port not listed"))
    }
}

/// The port's line of `/proc/tty/driver/serial` (`uart_line_info`): counters are
/// printed only when not zero; the modem lines follow as `RTS|CTS|...` (for root).
pub fn parse_serial_proc(text: &str, index: u32) -> Option<LineState> {
    let line = text.lines().find(|l| l.split(':').next().and_then(|n| n.trim().parse::<u32>().ok()) == Some(index))?;
    let count = |key: &str| line.split_whitespace().find_map(|w| w.strip_prefix(key)).and_then(|v| v.parse().ok()).unwrap_or(0);
    // `RTS|CTS|DTR`: the last word, the only one without a colon.
    let lines: Vec<&str> = line.split_whitespace().last().filter(|w| !w.contains(':')).map(|w| w.split('|').collect()).unwrap_or_default();
    Some(LineState { errors: Errors { frame: count("fe:"), overrun: count("oe:"), parity: count("pe:"), buffer: count("bo:") },
        cts: lines.contains(&"CTS"), rts: lines.contains(&"RTS") })
}

/// What the kernel reports for the port.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LineState { pub errors: Errors, pub cts: bool, pub rts: bool }

/// Receive errors the kernel counted on the port.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Errors {
    /// Framing errors (a bad stop bit: wrong rate or noise).
    pub frame: u32,
    /// Receive FIFO overruns: bytes lost because the interrupt came too late.
    pub overrun: u32,
    /// Parity errors (the line is 8E1).
    pub parity: u32,
    /// Bytes dropped because the tty's buffer was full.
    pub buffer: u32,
}
impl Errors {
    /// The counts since `base`.
    pub fn since(self, base: Errors) -> Errors {
        Errors { frame: self.frame.wrapping_sub(base.frame), overrun: self.overrun.wrapping_sub(base.overrun),
            parity: self.parity.wrapping_sub(base.parity), buffer: self.buffer.wrapping_sub(base.buffer) }
    }
    pub fn total(self) -> u32 { self.frame.wrapping_add(self.overrun).wrapping_add(self.parity).wrapping_add(self.buffer) }
}



/// Reads what a non-blocking descriptor has; zero when it has nothing.
pub fn read_ready(_blocking: &Blocking, mut source: impl Read, buffer: &mut [u8]) -> io::Result<usize> {
    {
        match source.read(buffer) {
            Err(error) if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted) => Ok(0),
            other => other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::{AsFd, FromRawFd, OwnedFd};

    /// A pseudo-terminal: its controller side and the path of the tty side.
    fn pty() -> (File, std::path::PathBuf) {
        // SAFETY: posix_openpt returns a new descriptor or -1; ownership passes to OwnedFd once checked.
        let fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
        assert!(fd >= 0);
        // SAFETY: `fd` is a fresh, valid descriptor nothing else owns.
        let owner = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut name = [0 as libc::c_char; 128];
        // SAFETY: grantpt and unlockpt take our descriptor; ptsname_r writes at most `name.len()` bytes with a terminator.
        unsafe {
            assert_eq!(libc::grantpt(fd), 0);
            assert_eq!(libc::unlockpt(fd), 0);
            assert_eq!(libc::ptsname_r(fd, name.as_mut_ptr(), name.len()), 0);
        }
        // SAFETY: ptsname_r wrote a NUL-terminated string into `name`.
        let path = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }.to_string_lossy().into_owned();
        (File::from(owner), path.into())
    }

    #[test]
    fn line_settings_are_raw_8e1_with_optional_flow_control() {
        // A pseudo-terminal clears PARENB, so the flags are checked on the structure itself.
        for flow in [false, true] {
            // SAFETY: termios is plain data and all-zero is a valid value of it.
            let mut termios: libc::termios = unsafe { std::mem::zeroed() };
            termios.c_cflag = libc::PARODD | libc::CSTOPB | libc::CRTSCTS | libc::CS5;
            termios.c_lflag = libc::ICANON | libc::ECHO | libc::ISIG;
            termios.c_iflag = libc::IXON | libc::ICRNL | libc::INPCK | libc::ISTRIP;
            termios.c_oflag = libc::OPOST;
            raw_8e1(&mut termios, libc::B1500000, flow).unwrap();
            assert_eq!(termios.c_cflag & (libc::CSIZE | libc::PARENB | libc::PARODD | libc::CSTOPB | libc::CLOCAL | libc::CREAD),
                libc::CS8 | libc::PARENB | libc::CLOCAL | libc::CREAD);
            assert_eq!(termios.c_cflag & libc::CRTSCTS != 0, flow);
            assert_eq!((termios.c_lflag & (libc::ICANON | libc::ECHO | libc::ISIG), termios.c_oflag & libc::OPOST), (0, 0));
            assert_eq!(termios.c_iflag & (libc::IXON | libc::ICRNL | libc::ISTRIP | libc::INLCR | libc::IGNCR), 0);
            assert_eq!((termios.c_cc[libc::VMIN], termios.c_cc[libc::VTIME]), (0, 0));
            // SAFETY: cfgetispeed and cfgetospeed read the termios they are given.
            assert_eq!(unsafe { (libc::cfgetispeed(&termios), libc::cfgetospeed(&termios)) }, (libc::B1500000, libc::B1500000));
        }
    }

    #[test]
    fn opens_raw_at_the_initial_rate_and_retunes() {
        let (_controller, path) = pty();
        let blocking = Blocking::dedicated_thread();
        let uart = Uart::open(&blocking, &path).unwrap();
        assert_eq!(uart.settings().unwrap(), (libc::B115200, true));
        uart.configure(1_500_000, true).unwrap();
        assert_eq!(uart.settings().unwrap(), (libc::B1500000, true));
        assert_eq!(uart.configure(123_456, false).unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn error_counts_and_modem_lines_fail_cleanly_where_the_tty_has_none() {
        let (_controller, path) = pty();
        let blocking = Blocking::dedicated_thread();
        let uart = Uart::open(&blocking, &path).unwrap();
        // A pseudo-terminal is not a ttyS port: an error, never a panic or garbage.
        assert!(uart.line_state(&blocking).is_err());
        assert!(uart.accepts(1_500_000) && !uart.accepts(123_456));
        // uart_line_info's format: counters only when not zero, then the modem lines.
        let proc = "serinfo:1.0 driver revision:\n0: uart:16550A mmio:0x10030000 irq:21 tx:5120 rx:880 fe:2 pe:3 oe:7 RTS|CTS|DTR\n\
            3: uart:16550A mmio:0x10033000 irq:24 tx:99 rx:0 RTS|DTR\n";
        assert_eq!(parse_serial_proc(proc, 0), Some(LineState { errors: Errors { frame: 2, overrun: 7, parity: 3, buffer: 0 }, cts: true, rts: true }));
        assert_eq!(parse_serial_proc(proc, 3), Some(LineState { errors: Errors::default(), cts: false, rts: true }));
        assert_eq!(parse_serial_proc(proc, 1), None);
        let (a, b) = (Errors { frame: 1, overrun: 5, parity: 2, buffer: 0 }, Errors { frame: 0, overrun: 3, parity: 2, buffer: 0 });
        assert_eq!(a.since(b), Errors { frame: 1, overrun: 2, parity: 0, buffer: 0 });
        assert_eq!(a.since(b).total(), 3);
    }

    #[test]
    fn bytes_pass_unchanged_in_both_directions_and_poll_reports_them() {
        let (mut controller, path) = pty();
        let blocking = Blocking::dedicated_thread();
        let uart = Uart::open(&blocking, &path).unwrap();
        // Every byte value, including the ones a cooked terminal would translate or act on.
        let all: Vec<u8> = (0..=255).collect();
        write_all(&blocking, uart.file(), &all).unwrap();
        let mut got = vec![0u8; 256];
        controller.read_exact(&mut got).unwrap();
        assert_eq!(got, all);
        let mut buffer = [0u8; 512];
        assert_eq!(uart.read(&blocking, &mut buffer).unwrap(), 0);
        assert_eq!(wait_readable(&blocking, [uart.file().as_fd()], 0).unwrap(), [false]);
        controller.write_all(&all).unwrap();
        assert_eq!(wait_readable(&blocking, [uart.file().as_fd()], 1000).unwrap(), [true]);
        let mut received = Vec::new();
        while received.len() < 256 { let n = uart.read(&blocking, &mut buffer).unwrap(); received.extend_from_slice(&buffer[..n]); }
        assert_eq!(received, all);
    }

    #[test]
    fn a_closed_peer_is_reported_as_readable_and_a_missing_tty_as_an_error() {
        let blocking = Blocking::dedicated_thread();
        let (reader, writer) = wake_pipe().unwrap();
        assert_eq!(wait_readable(&blocking, [reader.as_fd()], 0).unwrap(), [false]);
        // A full wake pipe does not block the waker.
        for _ in 0..100_000 { signal(&writer); }
        assert_eq!(wait_readable(&blocking, [reader.as_fd()], 0).unwrap(), [true]);
        let mut buffer = [0u8; 4096];
        while read_ready(&blocking, &reader, &mut buffer).unwrap() > 0 {}
        assert_eq!(wait_readable(&blocking, [reader.as_fd()], 0).unwrap(), [false]);
        drop(writer);
        assert_eq!(wait_readable(&blocking, [reader.as_fd()], 0).unwrap(), [true]);
        assert!(Uart::open(&blocking, Path::new("/dev/q2-no-such-tty")).is_err());
    }
}
