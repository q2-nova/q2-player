//! Memory pressure, sampled with the screen-off heap trim (resident.rs, at
//! most every 10 minutes; no wakeup of its own) for the opt-in `memory`
//! telemetry report (docs/telemetry.md).
//!
//! System gauges from `/proc/meminfo`; reclaim and refault counters from
//! `/proc/vmstat` (cumulative since boot, with the change since the previous
//! sample of this process); per process for the player and the Tailscale
//! daemon it supervises: `status` VmRSS/VmHWM, `stat` major faults and
//! `smaps_rollup`; the player's heap (mallinfo2) and decoded artwork.
//! Numbers only. The kernel has no PSI (`/proc/pressure`); nothing here uses it.
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU32, Ordering};

/// `/proc/meminfo`, KiB. A field the kernel does not have is null.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct MemInfo {
    pub mem_free_kib: Option<u64>,
    pub mem_available_kib: Option<u64>,
    pub cached_kib: Option<u64>,
    pub active_file_kib: Option<u64>,
    pub inactive_file_kib: Option<u64>,
    pub anon_pages_kib: Option<u64>,
    pub mlocked_kib: Option<u64>,
    pub shmem_kib: Option<u64>,
    pub slab_kib: Option<u64>,
    pub sreclaimable_kib: Option<u64>,
    pub sunreclaim_kib: Option<u64>,
}

/// `/proc/vmstat` counters, pages or events. `allocstall` sums every
/// `allocstall_*` zone; a kernel before 5.9 has one `workingset_refault`,
/// read as the file count.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VmStat {
    pub workingset_refault_file: Option<u64>,
    pub workingset_refault_anon: Option<u64>,
    pub pgmajfault: Option<u64>,
    pub pgsteal_kswapd: Option<u64>,
    pub pgsteal_direct: Option<u64>,
    pub pgscan_kswapd: Option<u64>,
    pub pgscan_direct: Option<u64>,
    pub allocstall: Option<u64>,
}

/// One process, KiB except the fault counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Process {
    pub vm_rss_kib: Option<u64>,
    pub vm_hwm_kib: Option<u64>,
    /// Major faults of the whole process (`stat` field 12), and the change
    /// since the previous sample of the same process.
    pub majflt: Option<u64>,
    pub majflt_delta: Option<u64>,
    pub rss_kib: Option<u64>,
    pub pss_kib: Option<u64>,
    pub private_clean_kib: Option<u64>,
    pub private_dirty_kib: Option<u64>,
    pub shared_clean_kib: Option<u64>,
    pub anonymous_kib: Option<u64>,
}

/// The player's own memory: glibc's heap (mallinfo2) and decoded artwork.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Player {
    /// The main arena's size (`arena`), the chunks in use (`uordblks`), the
    /// free chunks it holds (`fordblks`) and separately mapped blocks (`hblkhd`).
    pub heap_arena_kib: u64,
    pub heap_used_kib: u64,
    pub heap_free_kib: u64,
    pub heap_mmap_kib: u64,
    /// The artwork memory cache (library/art.rs): decoded pixels and images.
    pub art_decoded_kib: u64,
    pub art_images: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Sample {
    /// Seconds since the previous sample of this process; 0 for the first.
    pub interval_s: u64,
    /// What reading every file took, microseconds.
    pub read_us: u64,
    pub meminfo: MemInfo,
    pub vmstat: VmStat,
    /// `vmstat` change since the previous sample; null for the first.
    pub vmstat_delta: Option<VmStat>,
    pub player: Process,
    /// Null when the player does not run Tailscale.
    pub tailscaled: Option<Process>,
    pub heap: Player,
}

/// The previous sample's counters, lock-free in 32-bit atomics (the Q2 has
/// no 64-bit ones): [0] boot seconds + 1 (0: none), [1] which counters were
/// present (bits 0-7 vmstat, 8 the player's majflt, 9 tailscaled's),
/// [2..10] vmstat, [10] the player's majflt, [11] tailscaled's pid, [12] its
/// majflt. Counters keep their low 32 bits: an interval's change is far
/// below 2^32, so a wrapping difference is exact. One sampler runs at a time
/// (the screen-off trim, at most every 10 minutes).
static PREVIOUS: [AtomicU32; 13] = [const { AtomicU32::new(0) }; 13];

impl VmStat {
    fn fields(&self) -> [Option<u64>; 8] {
        [self.workingset_refault_file, self.workingset_refault_anon, self.pgmajfault, self.pgsteal_kswapd,
         self.pgsteal_direct, self.pgscan_kswapd, self.pgscan_direct, self.allocstall]
    }
    fn from_fields(f: [Option<u64>; 8]) -> VmStat {
        VmStat { workingset_refault_file: f[0], workingset_refault_anon: f[1], pgmajfault: f[2], pgsteal_kswapd: f[3],
                 pgsteal_direct: f[4], pgscan_kswapd: f[5], pgscan_direct: f[6], allocstall: f[7] }
    }
}

/// A counter's change since `before` (low 32 bits), when both were read.
fn delta(now: Option<u64>, before: Option<u32>) -> Option<u64> {
    Some(u64::from((now? as u32).wrapping_sub(before?)))
}

fn previous(index: usize, present: u32, bit: u32) -> Option<u32> {
    (present >> bit & 1 == 1).then(|| PREVIOUS[index].load(Ordering::Relaxed))
}

/// Reads every source; blocking file reads, on the blocking pool.
pub(crate) fn sample(_blocking: &crate::rt::Blocking) -> Sample {
    let started = std::time::Instant::now();
    let read = |path: &str| std::fs::read_to_string(path).unwrap_or_default();
    let meminfo = meminfo(&read("/proc/meminfo"));
    let vmstat = vmstat(&read("/proc/vmstat"));
    let mut player = process(&read("/proc/self/status"), &read("/proc/self/stat"), &read("/proc/self/smaps_rollup"));
    let daemon = crate::tailscale::daemon_pid().filter(|pid| {
        matches!(read(&format!("/proc/{pid}/comm")).trim(), "tailscaled" | "tsrsd")
    });
    let mut tailscaled = daemon.map(|pid| process(&read(&format!("/proc/{pid}/status")), &read(&format!("/proc/{pid}/stat")),
        &read(&format!("/proc/{pid}/smaps_rollup"))));
    let read_us = started.elapsed().as_micros() as u64;
    let now_s = crate::playtrace::boottime_us() / 1_000_000;
    let (interval_s, vmstat_delta) = match PREVIOUS[0].load(Ordering::Relaxed) {
        0 => (0, None),
        at => {
            let present = PREVIOUS[1].load(Ordering::Relaxed);
            player.majflt_delta = delta(player.majflt, previous(10, present, 8));
            if let (Some(pid), Some(t)) = (daemon, tailscaled.as_mut()) {
                if PREVIOUS[11].load(Ordering::Relaxed) == pid { t.majflt_delta = delta(t.majflt, previous(12, present, 9)); }
            }
            let now = vmstat.fields();
            let since = std::array::from_fn(|i| delta(now[i], previous(2 + i, present, i as u32)));
            (now_s.saturating_sub(u64::from(at - 1)), Some(VmStat::from_fields(since)))
        }
    };
    let mut present = 0;
    for (i, value) in vmstat.fields().into_iter().enumerate() {
        if let Some(value) = value { PREVIOUS[2 + i].store(value as u32, Ordering::Relaxed); present |= 1 << i; }
    }
    if let Some(n) = player.majflt { PREVIOUS[10].store(n as u32, Ordering::Relaxed); present |= 1 << 8; }
    PREVIOUS[11].store(daemon.unwrap_or(0), Ordering::Relaxed);
    if let Some(n) = tailscaled.and_then(|t| t.majflt) { PREVIOUS[12].store(n as u32, Ordering::Relaxed); present |= 1 << 9; }
    PREVIOUS[1].store(present, Ordering::Relaxed);
    PREVIOUS[0].store((now_s as u32).saturating_add(1), Ordering::Relaxed);
    Sample { interval_s, read_us, meminfo, vmstat, vmstat_delta, player, tailscaled, heap: heap() }
}


/// `Key:   123 kB` lines (meminfo, status, smaps_rollup).
fn kib(text: &str, key: &str) -> Option<u64> {
    text.lines().find_map(|line| line.strip_prefix(key)?.strip_prefix(':'))
        .and_then(|value| value.trim().trim_end_matches("kB").trim().parse().ok())
}

fn meminfo(text: &str) -> MemInfo {
    MemInfo {
        mem_free_kib: kib(text, "MemFree"), mem_available_kib: kib(text, "MemAvailable"), cached_kib: kib(text, "Cached"),
        active_file_kib: kib(text, "Active(file)"), inactive_file_kib: kib(text, "Inactive(file)"),
        anon_pages_kib: kib(text, "AnonPages"), mlocked_kib: kib(text, "Mlocked"), shmem_kib: kib(text, "Shmem"),
        slab_kib: kib(text, "Slab"), sreclaimable_kib: kib(text, "SReclaimable"), sunreclaim_kib: kib(text, "SUnreclaim"),
    }
}

fn vmstat(text: &str) -> VmStat {
    let value = |key: &str| text.lines().find_map(|line| line.strip_prefix(key)?.strip_prefix(' ')?.trim().parse::<u64>().ok());
    let stalls: Vec<u64> = text.lines().filter(|line| line.starts_with("allocstall"))
        .filter_map(|line| line.split_ascii_whitespace().nth(1)?.parse().ok()).collect();
    VmStat {
        workingset_refault_file: value("workingset_refault_file").or_else(|| value("workingset_refault")),
        workingset_refault_anon: value("workingset_refault_anon"),
        pgmajfault: value("pgmajfault"), pgsteal_kswapd: value("pgsteal_kswapd"), pgsteal_direct: value("pgsteal_direct"),
        pgscan_kswapd: value("pgscan_kswapd"), pgscan_direct: value("pgscan_direct"),
        allocstall: (!stalls.is_empty()).then(|| stalls.iter().sum()),
    }
}

fn process(status: &str, stat: &str, rollup: &str) -> Process {
    // Fields after the parenthesized command name: state is field 3, majflt 12.
    let majflt = stat.rsplit_once(") ").and_then(|(_, tail)| tail.split_ascii_whitespace().nth(9)?.parse().ok());
    Process {
        vm_rss_kib: kib(status, "VmRSS"), vm_hwm_kib: kib(status, "VmHWM"), majflt, majflt_delta: None,
        rss_kib: kib(rollup, "Rss"), pss_kib: kib(rollup, "Pss"), private_clean_kib: kib(rollup, "Private_Clean"),
        private_dirty_kib: kib(rollup, "Private_Dirty"), shared_clean_kib: kib(rollup, "Shared_Clean"),
        anonymous_kib: kib(rollup, "Anonymous"),
    }
}

/// glibc's main-arena statistics and the artwork cache, without trimming.
pub(crate) fn heap() -> Player {
    let (arena, used, free, mapped) = super::resident::heap_bytes(None);
    Player {
        heap_arena_kib: arena / 1024, heap_used_kib: used / 1024, heap_free_kib: free / 1024, heap_mmap_kib: mapped / 1024,
        art_decoded_kib: u64::from(crate::library::art::DECODED_KIB.load(Ordering::Relaxed)),
        art_images: u64::from(crate::library::art::DECODED_IMAGES.load(Ordering::Relaxed)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_texts_parse_to_numbers_and_missing_fields_stay_null() {
        let m = meminfo("MemTotal:         118628 kB\nMemFree:            5120 kB\nMemAvailable:      33736 kB\nCached:  20000 kB\nSwapCached: 0 kB\nActive(file):   9000 kB\nInactive(file): 11000 kB\nAnonPages: 40000 kB\nMlocked: 18000 kB\nShmem: 100 kB\nSlab: 6000 kB\nSReclaimable: 2000 kB\nSUnreclaim: 4000 kB\n");
        assert_eq!((m.mem_free_kib, m.cached_kib, m.active_file_kib, m.sunreclaim_kib), (Some(5120), Some(20000), Some(9000), Some(4000)));
        let v = vmstat("pgmajfault 77\nworkingset_refault_anon 3\nworkingset_refault_file 9000\nallocstall_normal 4\nallocstall_movable 1\npgsteal_kswapd 10\npgscan_kswapd 20\n");
        assert_eq!((v.workingset_refault_file, v.workingset_refault_anon, v.pgmajfault, v.allocstall), (Some(9000), Some(3), Some(77), Some(5)));
        assert_eq!((v.pgsteal_direct, v.pgscan_direct), (None, None));
        assert_eq!(vmstat("workingset_refault 12\n").workingset_refault_file, Some(12), "pre-5.9 kernels");
        let p = process("VmHWM:\t   45000 kB\nVmRSS:\t   43800 kB\n",
            "812 (q2 nd) S 1 812 812 0 -1 4194560 9000 0 321 0 50 20 0 0 20 0 9 0 1234 0 0\n",
            "00400000-7fff0000 ---p 00000000 00:00 0 [rollup]\nRss:  43800 kB\nPss:  40000 kB\nPss_Anon: 25300 kB\nShared_Clean: 3000 kB\nPrivate_Clean: 15000 kB\nPrivate_Dirty: 25800 kB\nAnonymous: 25300 kB\n");
        assert_eq!((p.vm_rss_kib, p.vm_hwm_kib, p.majflt), (Some(43800), Some(45000), Some(321)));
        assert_eq!((p.pss_kib, p.private_clean_kib, p.private_dirty_kib, p.shared_clean_kib, p.anonymous_kib), (Some(40000), Some(15000), Some(25800), Some(3000), Some(25300)));
        assert_eq!(process("", "", ""), Process::default());
    }

    #[test]
    fn counters_report_their_change_since_the_previous_sample() {
        assert_eq!(delta(Some(77), Some(70)), Some(7));
        assert_eq!(delta(Some(77), None), None, "nothing to compare with");
        assert_eq!(delta(None, Some(70)), None);
        // Past 2^32: the low 32 bits still give the interval's change.
        assert_eq!(delta(Some((1 << 32) + 5), Some(u32::MAX - 4)), Some(10));
        let v = vmstat("pgmajfault 70\nworkingset_refault_file 100\n");
        assert_eq!(VmStat::from_fields(v.fields()), v);
    }

    /// Must match `validate_memory` in tools/diagnostic_receiver.py and
    /// MEMORY_SAMPLE in tests/perf/test_diagnostic_receiver.py.
    #[test]
    fn memory_event_wire_format_matches_receiver() {
        let json = serde_json::to_value(Sample::default()).unwrap();
        let keys = |value: &serde_json::Value| { let mut k: Vec<String> = value.as_object().unwrap().keys().cloned().collect(); k.sort(); k.join(",") };
        assert_eq!(keys(&json), "heap,interval_s,meminfo,player,read_us,tailscaled,vmstat,vmstat_delta");
        assert_eq!(keys(&json["meminfo"]), "active_file_kib,anon_pages_kib,cached_kib,inactive_file_kib,mem_available_kib,mem_free_kib,mlocked_kib,shmem_kib,slab_kib,sreclaimable_kib,sunreclaim_kib");
        assert_eq!(keys(&json["vmstat"]), "allocstall,pgmajfault,pgscan_direct,pgscan_kswapd,pgsteal_direct,pgsteal_kswapd,workingset_refault_anon,workingset_refault_file");
        assert_eq!(keys(&json["player"]), "anonymous_kib,majflt,majflt_delta,private_clean_kib,private_dirty_kib,pss_kib,rss_kib,shared_clean_kib,vm_hwm_kib,vm_rss_kib");
        assert_eq!(keys(&json["heap"]), "art_decoded_kib,art_images,heap_arena_kib,heap_free_kib,heap_mmap_kib,heap_used_kib");
        assert!(json["vmstat_delta"].is_null() && json["tailscaled"].is_null());
    }

    #[test]
    fn a_live_sample_reads_this_process() {
        let s = sample(&crate::rt::Blocking::dedicated_thread());
        assert!(s.player.vm_rss_kib.unwrap() > 0 && s.player.majflt.is_some());
        assert!(s.meminfo.mem_available_kib.is_some() && s.vmstat.pgmajfault.is_some());
        let json = serde_json::to_string(&s).unwrap();
        assert!(!json.contains('/'), "numbers only: {json}");
    }
}
