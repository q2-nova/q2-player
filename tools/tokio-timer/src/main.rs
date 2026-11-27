//! Thread wakeups per tokio timer expiry on a current-thread runtime.
//!
//!     timer-wake-* MODE [ARGS]
//!
//! One JSON line: the timers that expired, and the voluntary context switches
//! of the runtime thread over the run (`/proc/thread-self/status`). A thread
//! that sleeps in `epoll_wait` and is woken makes one voluntary switch, so
//! `voluntary / expiries` is wakeups per expiry.
//!
//! Modes: `sleep MS SECONDS` (one task, `sleep` in a loop), `interval MS
//! SECONDS` (`tokio::time::interval`), `many SECONDS` (eight tasks, 1 s
//! sleeps, 125 ms apart), `cancel SECONDS` (a 100 ms sleep raced against a
//! 300 ms sleep that is dropped), `timeout-ready N` (N timeouts of 5 s armed
//! and cancelled without waiting, then one 200 ms sleep), `timeout-wake
//! SECONDS` (a 5 s timeout armed around each message from another thread,
//! one message per 100 ms).
use std::time::{Duration, Instant};

fn switches() -> (u64, u64) {
    let text = std::fs::read_to_string("/proc/thread-self/status").unwrap_or_default();
    let field = |name: &str| text.lines().find_map(|l| l.strip_prefix(name)).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
    (field("voluntary_ctxt_switches:"), field("nonvoluntary_ctxt_switches:"))
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str).unwrap_or("");
    let number = |i: usize, default: u64| args.get(i).and_then(|v| v.parse().ok()).unwrap_or(default);
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    // Start the driver and settle before counting.
    rt.block_on(async { tokio::time::sleep(Duration::from_millis(20)).await });
    let before = switches();
    let started = Instant::now();
    let (expiries, note): (u64, &str) = rt.block_on(async {
        match mode {
            "sleep" => {
                let (period, end) = (Duration::from_millis(number(1, 250)), started + Duration::from_secs(number(2, 10)));
                let mut n = 0;
                while Instant::now() < end { tokio::time::sleep(period).await; n += 1; }
                (n, "expiries are sleeps completed")
            }
            "interval" => {
                let (period, end) = (Duration::from_millis(number(1, 250)), started + Duration::from_secs(number(2, 10)));
                let mut ticks = tokio::time::interval(period);
                ticks.tick().await; // the first tick is immediate
                let mut n = 0;
                while Instant::now() < end { ticks.tick().await; n += 1; }
                (n, "expiries are interval ticks after the first")
            }
            "many" => {
                let end = started + Duration::from_secs(number(1, 10));
                let mut tasks = Vec::new();
                for i in 0..8u64 {
                    tasks.push(tokio::spawn(async move {
                        tokio::time::sleep(Duration::from_millis(125 * i + 1)).await;
                        let mut n = 1u64;
                        while Instant::now() < end { tokio::time::sleep(Duration::from_secs(1)).await; n += 1; }
                        n
                    }));
                }
                let mut n = 0;
                for t in tasks { n += t.await.unwrap(); }
                (n, "eight tasks; no two expiries coincide")
            }
            "cancel" => {
                let end = started + Duration::from_secs(number(1, 10));
                let mut n = 0;
                while Instant::now() < end {
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_millis(100)) => n += 1,
                        _ = tokio::time::sleep(Duration::from_millis(300)) => unreachable!(),
                    }
                }
                (n, "expiries are the 100 ms sleeps; each drops an armed 300 ms sleep")
            }
            "timeout-ready" => {
                let n = number(1, 10_000);
                for _ in 0..n {
                    let _ = tokio::time::timeout(Duration::from_secs(5), tokio::task::yield_now()).await;
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
                (1, "one sleep expires; the timeouts are armed and dropped")
            }
            "timeout-wake" => {
                let seconds = number(1, 10);
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
                std::thread::spawn(move || {
                    for _ in 0..seconds * 10 {
                        std::thread::sleep(Duration::from_millis(100));
                        if tx.send(()).is_err() { return; }
                    }
                });
                let mut n = 0;
                while let Ok(Some(())) = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await { n += 1; }
                (n, "expiries are messages received; no timer expires")
            }
            _ => {
                eprintln!("usage: MODE = sleep MS S | interval MS S | many S | cancel S | timeout-ready N | timeout-wake S");
                std::process::exit(2);
            }
        }
    });
    let after = switches();
    let voluntary = after.0 - before.0;
    println!(
        "{{\"mode\":\"{}\",\"args\":\"{}\",\"expiries\":{},\"voluntary\":{},\"nonvoluntary\":{},\"voluntary_per_expiry\":{:.3},\"elapsed_ms\":{},\"note\":\"{}\"}}",
        mode, args[1..].join(" "), expiries, voluntary, after.1 - before.1,
        voluntary as f64 / expiries.max(1) as f64, started.elapsed().as_millis(), note
    );
}
