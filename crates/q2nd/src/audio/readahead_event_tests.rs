//! Generated controls for ready-path locking and the existing health policy.
//! The ABBA diagnostic compares wait algorithms, not installed-app wakeups,
//! energy or end-to-end audio throughput. Thread resource endpoints are
//! diagnostic observations, not a count of kernel/hardware wakeups.
use super::*;
use crate::audio::interrupt::{Event, Waiter};
use std::os::unix::net::UnixDatagram;
use std::path::PathBuf;
use std::sync::{atomic::AtomicBool, mpsc, Condvar};

#[derive(Clone, Copy, Debug, PartialEq)]
struct WaitCost {
    pid: u32, tid: u32, start_ticks: u64,
    voluntary: u64, involuntary: u64, cpu_ns: u64,
}
impl WaitCost {
    fn parse(status: &str, stat: &str, cpu_ns: u64) -> Option<Self> {
        fn field(text: &str, key: &str) -> Option<u64> {
            let mut found = text.lines().filter_map(|line| line.strip_prefix(key));
            let number = found.next()?.trim().parse().ok()?;
            if found.next().is_some() { return None; }
            Some(number)
        }
        let (tid, rest) = stat.split_once(' ')?;
        let tid: u32 = tid.parse().ok()?;
        let (_, fields) = rest.rsplit_once(") ")?;
        let start_ticks: u64 = fields.split_whitespace().nth(19)?.parse().ok()?;
        let pid = u32::try_from(field(status, "Tgid:")?).ok()?;
        if pid == 0 || tid == 0 || start_ticks == 0 || field(status, "Pid:")? != u64::from(tid) { return None; }
        Some(Self { pid, tid, start_ticks, cpu_ns,
            voluntary: field(status, "voluntary_ctxt_switches:")?,
            involuntary: field(status, "nonvoluntary_ctxt_switches:")? })
    }

    #[cfg(target_os = "linux")]
    fn capture() -> Option<Self> {
        // Generated worker only. Never retain raw proc text or names. Sampling
        // uses no extra thread/timer; its small endpoint cost is included.
        let status = std::fs::read_to_string("/proc/thread-self/status").ok()?;
        let stat = std::fs::read_to_string("/proc/thread-self/stat").ok()?;
        let cpu = crate::platform::system::clock_nanos(libc::CLOCK_THREAD_CPUTIME_ID).ok()?;
        Self::parse(&status, &stat, cpu)
    }
    #[cfg(not(target_os = "linux"))]
    fn capture() -> Option<Self> { None }

    fn delta(self, after: Self) -> Option<serde_json::Value> {
        if (self.pid, self.tid, self.start_ticks) != (after.pid, after.tid, after.start_ticks) { return None; }
        Some(serde_json::json!({ "available": true, "pid": self.pid, "tid": self.tid,
            "start_ticks": self.start_ticks, "cpu_ns": after.cpu_ns.checked_sub(self.cpu_ns)?,
            "voluntary_context_switches": after.voluntary.checked_sub(self.voluntary)?,
            "involuntary_context_switches": after.involuntary.checked_sub(self.involuntary)?,
            "scope": "worker_thread_endpoints_include_sampling_and_ready_handshake_not_hardware_wakeups" }))
    }
}

#[test]
fn wait_cost_numeric_endpoints_refuse_malformed_identity_and_counters() {
    let status = "Tgid:\t42\nPid:\t43\nvoluntary_ctxt_switches:\t12\nnonvoluntary_ctxt_switches:\t3\n";
    let stat = format!("43 (generated ) worker) S {} 123 0", vec!["0"; 18].join(" "));
    let value = WaitCost::parse(status, &stat, 100).unwrap();
    assert_eq!((value.pid, value.tid, value.start_ticks), (42, 43, 123));
    for invalid in [status.replace("Pid:\t43", "Pid:\t44"),
        status.replace("Tgid:\t42", "Tgid:\t0"),
        status.replace("12", "-1"), status.replace("12", "18446744073709551616"),
        status.replace("nonvoluntary_ctxt_switches:\t3\n", ""),
        format!("{status}voluntary_ctxt_switches:\t12\n")] {
        assert!(WaitCost::parse(&invalid, &stat, 100).is_none());
    }
    assert!(WaitCost::parse(status, "43 (truncated) S 0", 100).is_none());
    assert!(WaitCost::parse(status, &stat.replace("123", "0"), 100).is_none());
}

#[test]
fn wait_cost_delta_refuses_reused_thread_and_counter_rollback() {
    let before = WaitCost { pid: 42, tid: 43, start_ticks: 123, voluntary: 12, involuntary: 3, cpu_ns: 100 };
    let after = WaitCost { voluntary: 20, involuntary: 5, cpu_ns: 130, ..before };
    let delta = before.delta(after).unwrap();
    assert_eq!(delta["cpu_ns"], 30);
    assert_eq!(delta["voluntary_context_switches"], 8);
    assert_eq!(delta["involuntary_context_switches"], 2);
    for invalid in [WaitCost { tid: 44, ..after }, WaitCost { pid: 44, ..after },
        WaitCost { start_ticks: 124, ..after }, WaitCost { cpu_ns: 99, ..after },
        WaitCost { voluntary: 11, ..after }, WaitCost { involuntary: 2, ..after }] {
        assert!(before.delta(invalid).is_none());
    }
}

fn reader(data: Vec<u8>, eof: bool) -> ReadAhead {
    let len = data.len().max(1) as u64;
    ReadAhead {
        shared: Arc::new(Shared {
            buffer: Mutex::new(Buf {
                data: data.into(), start: 0, eof, error: None, seek: None, closed: false,
            }),
            ready: Arc::new(Event::default()),
            changed: tokio::sync::Notify::new(),
            card_hook: CardHook::default(),
        }),
        pos: 0, len: Length::Fixed(len),
        interrupt: Arc::new(Interrupt::new(false)), stop_io: None,
    }
}

#[test]
fn ready_reader_bytes_do_not_acquire_interrupt_or_event_locks() {
    let expected: Vec<u8> = (0..CAP).map(|i| (i * 7 + i / 251) as u8).collect();
    let mut reader = reader(expected.clone(), true);
    let interrupt = reader.interrupt.clone();
    let event = reader.shared.ready.clone();
    let (done, complete) = mpsc::channel();
    // Keep reader Drop outside the held locks: retirement legitimately notifies.
    let (worker, early) = interrupt.with_generated_locks(&event, || {
        let worker = std::thread::spawn(move || {
            let blocking = crate::rt::Blocking::dedicated_thread();
            let result = crate::audio::callback::with(&blocking, || {
                let started = Instant::now();
                let mut bytes = Vec::with_capacity(CAP);
                let mut chunk = [0; 3000];
                let mut calls = 0_u64;
                loop {
                    let n = reader.read(&mut chunk)?;
                    calls += 1;
                    if n == 0 { break; }
                    bytes.extend_from_slice(&chunk[..n]);
                }
                Ok::<_, io::Error>((bytes, calls, started.elapsed()))
            });
            assert!(done.send((reader, result)).is_ok());
        });
        let early = complete.recv_timeout(Duration::from_secs(3));
        (worker, early)
    });
    let returned_while_locked = early.is_ok();
    // A regression releases the locks before bounded cleanup and failure.
    let (reader, result) = early.unwrap_or_else(|_| {
        complete.recv_timeout(Duration::from_secs(3)).expect("reader did not retire after lock release")
    });
    worker.join().unwrap();
    drop(reader);
    assert!(returned_while_locked, "ready bytes must not acquire notification locks");
    let (bytes, calls, elapsed) = result.unwrap();
    assert_eq!(bytes, expected);
    let checksum: u64 = bytes.iter().map(|&b| u64::from(b)).sum();
    println!("Q2_MEDIA_READY {}", serde_json::json!({
        "bytes": bytes.len(), "calls": calls, "checksum": checksum,
        "wall_ns": elapsed.as_nanos(), "returned_while_locks_held": true,
        "scope": "actual_ready_reader_candidate_only"
    }));
}

struct SocketRoot(PathBuf);
impl SocketRoot {
    fn new() -> Self {
        // Unix sockaddr paths are short: retain the established relative host
        // fallback, while the target owns its explicit generated root.
        let root = std::env::var_os("Q2_GENERATED_TEST_ROOT").map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("../../build/perf"))
            .join(format!("media-health-{:08x}", fastrand::u32(..)));
        std::fs::create_dir_all(&root).unwrap();
        Self(root)
    }
}
impl Drop for SocketRoot {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); }
}

#[test]
fn stalled_reader_preserves_health_cutoff_and_cancels_after_it() {
    let root = SocketRoot::new();
    let path = root.0.join("h.sock");
    let receiver = UnixDatagram::bind(&path).unwrap();
    receiver.set_nonblocking(true).unwrap();
    let mut reader = reader(Vec::new(), false);
    let interrupt = reader.interrupt.clone();
    let (started, starting) = mpsc::channel();
    let (done, complete) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let result = crate::health::with_generated_sender(&path, b'A', || {
            let blocking = crate::rt::Blocking::dedicated_thread();
            crate::audio::callback::with(&blocking, || {
                let before = Instant::now();
                started.send(before).unwrap();
                let result = reader.read(&mut [0; 1]);
                (before, Instant::now(), result)
            })
        });
        drop(reader);
        done.send(result).unwrap();
    });
    let before = starting.recv_timeout(Duration::from_secs(3)).unwrap();
    // Cross the real five-second watchdog cutoff, not a shortened test policy.
    std::thread::sleep(STALL + Duration::from_millis(1200));
    let cancelled = Instant::now();
    interrupt.store(true, Ordering::Relaxed);
    let ((recorded_start, reader_returned, result), last_pulse) = complete
        .recv_timeout(Duration::from_secs(3)).unwrap().unwrap();
    worker.join().unwrap();
    assert_eq!(before, recorded_start);
    assert!(super::super::source::is_cut(&result.unwrap_err()));
    let mut pulses = 0;
    let mut bytes = [0; 8];
    loop {
        match receiver.recv(&mut bytes) {
            Ok(n) => { assert_eq!(&bytes[..n], b"A"); pulses += 1; }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
            Err(e) => panic!("generated health receiver: {e}"),
        }
    }
    assert!(pulses >= 2, "actual reader must maintain health while initially waiting");
    assert!(last_pulse > before + Duration::from_secs(1));
    assert!(last_pulse < cancelled - Duration::from_millis(500),
        "a stalled reader must stop pulsing, not feed health indefinitely");
    println!("Q2_MEDIA_HEALTH {}", serde_json::json!({
        "pulses": pulses, "last_pulse_ns": last_pulse.duration_since(before).as_nanos(),
        "cancel_after_ns": cancelled.duration_since(before).as_nanos(),
        "reader_return_ns": reader_returned.duration_since(before).as_nanos(),
        "cancel_to_reader_return_ns": reader_returned.checked_duration_since(cancelled).unwrap().as_nanos(),
        "cutoff_ns": STALL.as_nanos(), "cut": true,
        "scope": "actual_reader_health_protocol_not_supervisor_restart"
    }));
    drop(receiver);
}

fn wait_arm(legacy: bool) -> serde_json::Value {
    let origin = Instant::now();
    let interrupt = Arc::new(Interrupt::new(false));
    let old_flag = Arc::new(AtomicBool::new(false));
    let event = Arc::new(Event::default());
    let (ready, started) = mpsc::channel();
    let (done, complete) = mpsc::channel();
    let worker_interrupt = interrupt.clone();
    let worker_flag = old_flag.clone();
    let worker = std::thread::spawn(move || {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let counts = crate::audio::callback::with(&blocking, || {
            let mut entries = 0_u64;
            let mut returns = 0_u64;
            let mut timeouts = 0_u64;
            let before = WaitCost::capture();
            #[cfg(target_os = "linux")]
            assert!(before.is_some(), "Linux worker resource endpoints must be available");
            if legacy {
                // The previous algorithm's cancellation-only held predicate:
                // atomic flag plus a 50 ms timed Condvar, with no cancel notify.
                let data = Mutex::new(());
                let cv = Condvar::new();
                ready.send(()).unwrap();
                while !worker_flag.load(Ordering::Relaxed) {
                    entries += 1;
                    let (guard, result) = cv.wait_timeout(data.lock().unwrap(), Duration::from_millis(50)).unwrap();
                    drop(guard);
                    returns += 1;
                    timeouts += u64::from(result.timed_out());
                }
            } else {
                let mut wait = Waiter::new(&worker_interrupt, &event);
                wait.wait(None).unwrap(); // Registration/recheck, not a park.
                ready.send(()).unwrap();
                while !worker_interrupt.load(Ordering::Relaxed) {
                    entries += 1;
                    wait.wait(None).unwrap();
                    returns += 1;
                }
            }
            // Mark predicate return before proc sampling, channel delivery and
            // join; cancel latency must not accidentally include host receive.
            let returned = Instant::now();
            let cost = match (before, WaitCost::capture()) {
                (Some(before), Some(after)) => before.delta(after).expect("worker identity/counters changed"),
                (None, None) => serde_json::json!({"available": false, "reason": "non_linux_thread_resources"}),
                _ => panic!("incomplete worker resource endpoints"),
            };
            (entries, returns, timeouts, returned, cost)
        });
        done.send(counts).unwrap();
    });
    started.recv_timeout(Duration::from_secs(3)).unwrap();
    let held = Instant::now();
    std::thread::sleep(Duration::from_millis(400));
    let cancelled = Instant::now();
    if legacy { old_flag.store(true, Ordering::Relaxed); }
    else { interrupt.store(true, Ordering::Relaxed); }
    let (entries, returns, timeouts, returned, cost) = complete.recv_timeout(Duration::from_secs(3)).unwrap();
    let retired = Instant::now();
    worker.join().unwrap();
    assert_eq!(entries, returns);
    serde_json::json!({
        "legacy": legacy, "held_ns": cancelled.duration_since(held).as_nanos(),
        "cancel_to_return_ns": retired.duration_since(cancelled).as_nanos(),
        "cancel_to_return_domain": "parent_result_receive_historical_field",
        "cancel_request_ns": cancelled.duration_since(origin).as_nanos(),
        "worker_return_ns": returned.duration_since(origin).as_nanos(),
        "cancel_to_worker_return_ns": returned.checked_duration_since(cancelled).unwrap().as_nanos(),
        "thread_cost": cost,
        "wait_entries": entries, "wait_returns": returns, "timeout_returns": timeouts,
        "registration_returns": if legacy { 0 } else { 1 }, "retired": true
    })
}

#[test]
fn generated_wait_algorithm_abba_reports_logical_returns() {
    let arms: Vec<_> = [true, false, false, true].into_iter().map(wait_arm).collect();
    // Scheduling and spurious wakes are observations, never savings assertions.
    println!("Q2_MEDIA_WAIT_ABBA {}", serde_json::json!({
        "arms": arms, "scope": "wait_algorithm_logical_returns_not_kernel_wakeups",
        "schema": 2, "hold_ms": 400, "legacy_timeout_ms": 50,
        "energy_claim": false, "app_wakeup_claim": false
    }));
}
