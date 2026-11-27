//! Keeps the player's own code resident (UX-008).
//!
//! The root filesystem is xz squashfs in 128 KiB blocks, so an evicted code
//! page costs a whole block's decompression when it is next run. Under memory
//! pressure an unpinned player refaulted about 95 code pages per Shuffle Songs
//! on the Q2, and every thread stalled for 260–370 ms at random points (formatting, MD5,
//! FLAC probing, list drawing). Pinning the executable and its libraries
//! (about 18 MiB, 12 MiB of it resident anyway) removes those refaults.
//! Code that only the `--bench` and `--script` modes run is linked into its
//! own section (`link/q2tools.ld`) and stays unlocked.
//!
//! The heap is tuned for the same 118 MiB: one malloc arena, so freed memory
//! of every thread is reusable by every other; a fixed 128 KiB mmap threshold,
//! so large transient buffers (cover decodes, response bodies) return to the
//! kernel when freed instead of fragmenting the heap; and a trim of the free
//! heap pages when the screen goes off.

/// Sections of the executable left out of the locked set: the code that only
/// `--bench` (`perf`) and `--script` run, gathered by `link/q2tools.ld`.
const TOOLS_SECTIONS: &[&str] = &[".q2tools"];
/// The unwind tables. std's panic backtrace reads them when `RUST_BACKTRACE`
/// is set. Release builds link no unwinder; the emulator diagnostic build's
/// stall sampler (feature `stall-sampler`, vendor/diagnostics/stall.c) also
/// reads them, in the stalled thread's signal handler. Left unlocked, a
/// backtrace or stall sample may first fault their pages back in from the
/// squashfs root.
const UNWIND_SECTIONS: &[&str] = &[".eh_frame_hdr", ".eh_frame"];
/// Whether the unwind tables stay locked: yes, unless the build sets
/// `--cfg q2_unlock_unwind_tables`, which tools/locked_size.py also models.
const LOCK_UNWIND_TABLES: bool = !cfg!(q2_unlock_unwind_tables);
/// Alignment of the ranges left unlocked. mlock rounds outward to the kernel's
/// pages, so a kernel with larger pages only locks more at each edge.
const PAGE: usize = 4096;

/// What `lock_code` did: bytes locked, bytes of the executable's mappings
/// left unlocked, and the ranges whose mlock failed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Pinned {
    pub locked: usize,
    pub unlocked: usize,
    pub failed: usize,
}

/// Pins every executable file mapping of this process except the sections
/// named above, which are page-aligned inward and skipped.
pub(crate) fn lock_code(blocking: &crate::rt::Blocking) -> Pinned {
    let Ok(maps) = std::fs::read_to_string("/proc/self/maps") else { return Pinned::default() };
    let exe = std::fs::read_link("/proc/self/exe").ok()
        .map(|p| p.to_string_lossy().trim_end_matches(" (deleted)").to_owned());
    let inode = std::fs::metadata("/proc/self/exe").map(|m| std::os::unix::fs::MetadataExt::ino(&m)).ok();
    let names: Vec<&str> = TOOLS_SECTIONS.iter().chain(if LOCK_UNWIND_TABLES { &[][..] } else { UNWIND_SECTIONS }).copied().collect();
    let sections = std::fs::File::open("/proc/self/exe").ok().and_then(|f| file_sections(blocking, &f, &names)).unwrap_or_default();
    let mut pinned = Pinned::default();
    let mode = lock_mode();
    if mode == LockMode::None { return pinned; }
    for mapping in maps.lines().filter_map(code_mapping) {
        let own = exe.as_deref() == Some(mapping.path) && inode == Some(mapping.inode);
        let skip = if own { mapping.skipped(&sections) } else { Vec::new() };
        pinned.unlocked += skip.iter().map(|(a, b)| b - a).sum::<usize>();
        for (start, end) in remainder(mapping.start, mapping.end, &skip) {
            // SAFETY: mlock and mlock2 only pin the pages of an existing
            // mapping of this process; they neither read nor write through the
            // pointer.
            let locked = unsafe {
                match mode {
                    LockMode::OnFault => libc::mlock2(start as *const libc::c_void, end - start, libc::MLOCK_ONFAULT),
                    _ => libc::mlock(start as *const libc::c_void, end - start),
                }
            };
            if locked == 0 { pinned.locked += end - start; } else { pinned.failed += 1; }
        }
    }
    pinned
}

/// How `lock_code` pins the code, from `Q2ND_LOCK` (read once, at the
/// pin after the first frame), so one binary can be compared on the Q2:
/// - `onfault` (default): each page is locked when first run
///   (mlock2 MLOCK_ONFAULT), so code never run takes no memory, and code once
///   run is never evicted. On the Q2 it locked 12.9 MB against 16.3 MB for
///   `all`, with the same UI latency warm and after drop_caches;
/// - `all`: every page is read in and locked now;
/// - `none`: nothing is locked; the kernel may evict any code page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LockMode { All, OnFault, None }

pub(crate) fn lock_mode() -> LockMode {
    parse_lock_mode(std::env::var("Q2ND_LOCK").ok().as_deref())
}

fn parse_lock_mode(value: Option<&str>) -> LockMode {
    match value {
        Some("all") => LockMode::All,
        Some("none") => LockMode::None,
        _ => LockMode::OnFault,
    }
}

#[cfg(test)]
mod lock_mode_tests {
    use super::{parse_lock_mode, LockMode};

    #[test]
    fn onfault_is_the_default_and_the_others_are_explicit() {
        assert_eq!(parse_lock_mode(None), LockMode::OnFault);
        assert_eq!(parse_lock_mode(Some("onfault")), LockMode::OnFault);
        assert_eq!(parse_lock_mode(Some("bogus")), LockMode::OnFault);
        assert_eq!(parse_lock_mode(Some("all")), LockMode::All);
        assert_eq!(parse_lock_mode(Some("none")), LockMode::None);
    }
}

/// Before any thread starts. The Q2 has one core, so one arena costs no
/// parallelism; glibc's default on 32 bits is two per core.
pub(crate) fn tune_heap() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: mallopt only sets glibc allocator parameters; no memory is touched.
    unsafe {
        libc::mallopt(libc::M_ARENA_MAX, 1);
        libc::mallopt(libc::M_MMAP_THRESHOLD, 128 * 1024);
    }
}

/// Memory before a trim and what the trim returned, in KiB.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Trim {
    pub resident_kib: u64,
    pub available_kib: u64,
    pub heap_free_kib: u64,
    pub released_kib: u64,
}

/// Returns the heap's free pages to the kernel (`malloc_trim`).
pub(crate) fn trim(_blocking: &crate::rt::Blocking) -> Trim {
    let resident = || status_kib(&std::fs::read_to_string("/proc/self/status").unwrap_or_default(), "VmRSS:");
    let before = resident();
    let available_kib = status_kib(&std::fs::read_to_string("/proc/meminfo").unwrap_or_default(), "MemAvailable:");
    let heap_free_kib = heap_bytes(Some(_blocking)).2 / 1024;
    Trim { resident_kib: before, available_kib, heap_free_kib, released_kib: before.saturating_sub(resident()) }
}

/// glibc's main arena in bytes: its size, the chunks in use, the free chunks
/// it holds and separately mapped blocks (mallinfo2). With a blocking
/// witness, the free pages are then returned to the kernel (`malloc_trim`).
pub(crate) fn heap_bytes(trim: Option<&crate::rt::Blocking>) -> (u64, u64, u64, u64) {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: mallinfo2 and malloc_trim only inspect and release glibc's own free chunks.
    unsafe {
        let info = libc::mallinfo2();
        if trim.is_some() { libc::malloc_trim(0); }
        (info.arena as u64, info.uordblks as u64, info.fordblks as u64, info.hblkhd as u64)
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    { let _ = trim; (0, 0, 0, 0) }
}

/// At most every 10 minutes, on the blocking pool; reports to telemetry.
pub(crate) fn trim_after_screen_off() {
    use std::sync::atomic::{AtomicU32, Ordering};
    // Boot seconds: the Q2 has no 64-bit atomics.
    static LAST_S: AtomicU32 = AtomicU32::new(0);
    let now = (crate::playtrace::boottime_us() / 1_000_000) as u32;
    let last = LAST_S.load(Ordering::Relaxed);
    if last != 0 && now.wrapping_sub(last) < 600 { return; }
    LAST_S.store(now.max(1), Ordering::Relaxed);
    drop(crate::rt::spawn_blocking(|blocking| {
        // Before the trim: what the session left behind.
        let sample = super::memory::sample(blocking);
        let trim = trim(blocking);
        log::info!("heap trim: {trim:?}; memory: {}", serde_json::to_string(&sample).unwrap_or_default());
        crate::diagnostics::health::memory(&trim);
        crate::diagnostics::admit_memory(sample, crate::playtrace::boottime_us());
    }));
}

/// After a screen that filled the heap (Cover Flow) closed and dropped its
/// images: return the free heap pages, on the blocking pool. On the host a
/// trim of a 12 MiB arena takes about 0.25 ms (tests/perf/coverflow_memory.py).
pub(crate) fn trim_after_browsing() {
    if cfg!(test) { return; }
    drop(crate::rt::spawn_blocking(|blocking| {
        let started = std::time::Instant::now();
        let trim = trim(blocking);
        log::info!("heap trim after browsing: {trim:?} in {} us", started.elapsed().as_micros());
    }));
}

/// Maps a read-only file for the rest of the process (the fallback fonts):
/// its pages are file cache, reclaimable and not part of the heap, rather
/// than a heap copy. Refuses non-files, empty files and files over `limit`.
pub(crate) fn map_file(_blocking: &crate::rt::Blocking, path: &std::path::Path, limit: usize) -> std::io::Result<&'static [u8]> {
    use std::os::unix::{fs::OpenOptionsExt, io::AsRawFd};
    let file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(path)?;
    let meta = file.metadata()?;
    let len = usize::try_from(meta.len()).unwrap_or(usize::MAX);
    if !meta.is_file() || len == 0 || len > limit {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "Resource is not a regular file within its size limit"));
    }
    // SAFETY: a private read-only mapping of `len` bytes of an open regular
    // file; it is never unmapped, so the slice lives as long as the process.
    // A file truncated while mapped would fault on access: these are image
    // files on the read-only root filesystem.
    unsafe {
        let at = libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_PRIVATE, file.as_raw_fd(), 0);
        if at == libc::MAP_FAILED { return Err(std::io::Error::last_os_error()); }
        Ok(std::slice::from_raw_parts(at.cast::<u8>(), len))
    }
}

/// Documents at least this large are followed by a trim.
const TRIM_DOCUMENT_BYTES: u64 = 256 * 1024;

/// After a large server document was parsed or a sync batch was stored:
/// return the free heap pages, on the blocking pool. At most one such trim
/// is queued at a time. malloc_trim takes about 1 ms per 2 MB of heap on
/// the Q2.
pub(crate) fn trim_after_document(bytes: u64) {
    if bytes < TRIM_DOCUMENT_BYTES { return; }
    trim_after_sync();
}

/// After a sync batch (album catalog page, Audiobookshelf round).
pub(crate) fn trim_after_sync() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static QUEUED: AtomicBool = AtomicBool::new(false);
    if cfg!(test) || QUEUED.swap(true, Ordering::AcqRel) { return; }
    drop(crate::rt::spawn_blocking(|blocking| {
        QUEUED.store(false, Ordering::Release);
        let started = std::time::Instant::now();
        let trim = trim(blocking);
        log::debug!("heap trim after document: {trim:?} in {} us", started.elapsed().as_micros());
    }));
}

/// The KiB value of `key` in a `/proc/self/status` or `/proc/meminfo` text.
fn status_kib(text: &str, key: &str) -> u64 {
    text.lines().find_map(|l| l.strip_prefix(key)).and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok()).unwrap_or(0)
}

/// A file-backed `r-x` line of `/proc/self/maps`.
#[derive(Debug, PartialEq, Eq)]
struct Mapping<'a> {
    start: usize,
    end: usize,
    /// File offset of `start`.
    offset: u64,
    inode: u64,
    path: &'a str,
}

fn code_mapping(line: &str) -> Option<Mapping<'_>> {
    let mut field = line.split_whitespace();
    let (range, perms, offset, _device, inode, path) = (field.next()?, field.next()?, field.next()?, field.next()?, field.next()?, field.next()?);
    if !perms.starts_with("r-x") || inode == "0" || !path.starts_with('/') { return None; }
    let (start, end) = range.split_once('-')?;
    let (start, end) = (usize::from_str_radix(start, 16).ok()?, usize::from_str_radix(end, 16).ok()?);
    let (offset, inode) = (u64::from_str_radix(offset, 16).ok()?, inode.parse().ok()?);
    (end > start).then_some(Mapping { start, end, offset, inode, path })
}

impl Mapping<'_> {
    /// The address ranges of `sections` (file offset, length) inside this
    /// mapping, merged where adjacent and shrunk to whole pages.
    fn skipped(&self, sections: &[(u64, u64)]) -> Vec<(usize, usize)> {
        let file_end = self.offset + (self.end - self.start) as u64;
        let mut ranges: Vec<(usize, usize)> = sections.iter().filter_map(|&(offset, len)| {
            let (a, b) = (offset.max(self.offset), offset.saturating_add(len).min(file_end));
            (a < b).then(|| (self.start + (a - self.offset) as usize, self.start + (b - self.offset) as usize))
        }).collect();
        ranges.sort_unstable();
        let mut merged: Vec<(usize, usize)> = Vec::new();
        for (a, b) in ranges {
            // Sections separated only by alignment padding are one range.
            match merged.last_mut() { Some(last) if a <= last.1 + 16 => last.1 = last.1.max(b), _ => merged.push((a, b)) }
        }
        merged.into_iter().filter_map(|(a, b)| {
            let (a, b) = (a.next_multiple_of(PAGE), b / PAGE * PAGE);
            (a < b).then_some((a, b))
        }).collect()
    }
}

/// `start..end` without the sorted, disjoint `skip` ranges inside it.
fn remainder(start: usize, end: usize, skip: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut pieces = Vec::new();
    let mut at = start;
    for &(a, b) in skip {
        if a > at { pieces.push((at, a)); }
        at = at.max(b);
    }
    if end > at { pieces.push((at, end)); }
    pieces
}

/// File offset and length of each named section with file contents, from
/// the ELF section headers (little-endian ELF32 or ELF64). None if the file
/// is not such an ELF file or its headers are out of bounds.
fn file_sections(_blocking: &crate::rt::Blocking, file: &std::fs::File, names: &[&str]) -> Option<Vec<(u64, u64)>> {
    use std::os::unix::fs::FileExt;
    let read = |offset: u64, len: usize| -> Option<Vec<u8>> {
        if len > 1 << 20 { return None; }
        let mut bytes = vec![0; len];
        file.read_exact_at(&mut bytes, offset).ok()?;
        Some(bytes)
    };
    let u16_at = |b: &[u8], at: usize| Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?));
    let u32_at = |b: &[u8], at: usize| Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?));
    let u64_at = |b: &[u8], at: usize| Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?));
    let header = read(0, 64)?;
    if header[..4] != *b"\x7fELF" || header[5] != 1 { return None; }
    let wide = match header[4] { 1 => false, 2 => true, _ => return None };
    let (shoff, entsize, count, strndx) = if wide {
        (u64_at(&header, 0x28)?, u16_at(&header, 0x3a)?, u16_at(&header, 0x3c)?, u16_at(&header, 0x3e)?)
    } else {
        (u64::from(u32_at(&header, 0x20)?), u16_at(&header, 0x2e)?, u16_at(&header, 0x30)?, u16_at(&header, 0x32)?)
    };
    let (entsize, count, strndx) = (usize::from(entsize), usize::from(count), usize::from(strndx));
    if entsize < if wide { 64 } else { 40 } || strndx >= count { return None; }
    let table = read(shoff, entsize * count)?;
    // (name offset, type, file offset, size) of section `i`.
    let entry = |i: usize| -> Option<(u32, u32, u64, u64)> {
        let e = table.get(i * entsize..(i + 1) * entsize)?;
        Some(if wide { (u32_at(e, 0)?, u32_at(e, 4)?, u64_at(e, 24)?, u64_at(e, 32)?) }
             else { (u32_at(e, 0)?, u32_at(e, 4)?, u64::from(u32_at(e, 16)?), u64::from(u32_at(e, 20)?)) })
    };
    let (_, _, str_offset, str_len) = entry(strndx)?;
    let strings = read(str_offset, usize::try_from(str_len).ok()?)?;
    const NOBITS: u32 = 8;
    let mut found = Vec::new();
    for i in 1..count {
        let (name, kind, offset, len) = entry(i)?;
        let name = strings.get(name as usize..)?;
        let name = &name[..name.iter().position(|&b| b == 0)?];
        if kind != NOBITS && len > 0 && names.iter().any(|n| n.as_bytes() == name) { found.push((offset, len)); }
    }
    Some(found)
}

#[cfg(test)]
mod tests {
    use super::{code_mapping, file_sections, remainder, status_kib};

    #[test]
    fn memory_fields_read_in_kib() {
        assert_eq!(status_kib("VmHWM:\t   38768 kB\nVmRSS:\t   37788 kB\n", "VmRSS:"), 37788);
        assert_eq!(status_kib("MemTotal:         118628 kB\nMemAvailable:      33736 kB\n", "MemAvailable:"), 33736);
        assert_eq!(status_kib("", "VmRSS:"), 0);
    }

    #[test]
    fn trim_reports_plausible_memory() {
        let held: Vec<Vec<u8>> = (0..64).map(|n| vec![n as u8; 64 * 1024]).collect();
        drop(held);
        let trim = super::trim(&crate::rt::Blocking::dedicated_thread());
        assert!(trim.resident_kib > 0 && trim.released_kib <= trim.resident_kib);
    }

    #[test]
    fn only_file_backed_code_is_pinned() {
        let m = code_mapping("00400000-0125a000 r-xp 00000000 fe:00 696        /usr/bin/q2nd").unwrap();
        assert_eq!((m.start, m.end, m.offset, m.inode, m.path), (0x400000, 0x125a000, 0, 696, "/usr/bin/q2nd"));
        let m = code_mapping("77af0000-77cb7000 r-xp 00010000 fe:00 400        /lib/libc.so.6").unwrap();
        assert_eq!((m.start, m.end, m.offset), (0x77af0000, 0x77cb7000, 0x10000));
        assert_eq!(code_mapping("0126f000-01290000 rw-p 00e5f000 fe:00 696        /usr/bin/q2nd"), None);
        assert_eq!(code_mapping("7ff4e000-7ff4f000 r-xp 00000000 00:00 0          [vdso]"), None);
        assert_eq!(code_mapping("7fefa000-7fefb000 r-xp 00000000 00:00 0 "), None);
    }

    #[test]
    fn skipped_sections_are_whole_pages_inside_the_mapping() {
        let m = code_mapping("00400000-00800000 r-xp 00000000 fe:00 696 /usr/bin/q2nd").unwrap();
        // A section from file offset 0x1100 for 0x3000 bytes: only the pages
        // 0x402000..0x404000 lie wholly inside it.
        assert_eq!(m.skipped(&[(0x1100, 0x3000)]), vec![(0x402000, 0x404000)]);
        // Adjacent sections (4 bytes of padding) merge before rounding:
        // neither holds a whole page alone.
        assert_eq!(m.skipped(&[(0x10800, 0x17fc), (0x12000, 0x800)]), vec![(0x411000, 0x412000)]);
        assert_eq!(m.skipped(&[(0x10800, 0x17fc)]), vec![]);
        // Outside the mapping's file range, or smaller than a page: nothing.
        assert_eq!(m.skipped(&[(0x500000, 0x10000), (0x2010, 0x100)]), vec![]);
        // A mapping that starts at a file offset translates through it.
        let m = code_mapping("77af0000-77b00000 r-xp 00010000 fe:00 400 /lib/libc.so.6").unwrap();
        assert_eq!(m.skipped(&[(0x12000, 0x2000)]), vec![(0x77af2000, 0x77af4000)]);
    }

    #[test]
    fn locking_skips_the_ranges() {
        assert_eq!(remainder(0x1000, 0x9000, &[]), vec![(0x1000, 0x9000)]);
        assert_eq!(remainder(0x1000, 0x9000, &[(0x2000, 0x3000), (0x5000, 0x9000)]), vec![(0x1000, 0x2000), (0x3000, 0x5000)]);
        assert_eq!(remainder(0x1000, 0x9000, &[(0x1000, 0x9000)]), vec![]);
    }

    #[test]
    fn section_headers_are_read_from_this_executable() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let file = std::fs::File::open("/proc/self/exe").unwrap();
        let text = file_sections(&blocking, &file, &[".text"]).unwrap();
        assert_eq!(text.len(), 1);
        assert!(text[0].1 > 4096);
        let unwind = file_sections(&blocking, &file, &[".eh_frame_hdr", ".eh_frame"]).unwrap();
        assert_eq!(unwind.len(), 2);
        assert_eq!(file_sections(&blocking, &file, &[".q2tools"]), Some(vec![]));
        let not_elf = std::fs::File::open("/proc/self/maps").unwrap();
        assert_eq!(file_sections(&blocking, &not_elf, &[".text"]), None);
    }

    #[test]
    fn section_headers_are_read_from_a_32_bit_executable() {
        // The Q2's layout: ELF32 little-endian. Sections: null, .q2tools,
        // a NOBITS .eh_frame (no file contents, so never skipped), .shstrtab.
        let names = b"\0.q2tools\0.shstrtab\0.eh_frame\0";
        let mut elf = vec![0u8; 96 + 4 * 40];
        elf[..6].copy_from_slice(b"\x7fELF\x01\x01");
        elf[0x20..0x24].copy_from_slice(&96u32.to_le_bytes());
        elf[0x2e..0x30].copy_from_slice(&40u16.to_le_bytes());
        elf[0x30..0x32].copy_from_slice(&4u16.to_le_bytes());
        elf[0x32..0x34].copy_from_slice(&3u16.to_le_bytes());
        elf[64..64 + names.len()].copy_from_slice(names);
        for (i, (name, kind, offset, size)) in [(1u32, 1u32, 0x1000u32, 0x2000u32), (20, 8, 0x3000, 0x100), (10, 3, 64, names.len() as u32)].into_iter().enumerate() {
            let e = 96 + (i + 1) * 40;
            for (at, value) in [(0, name), (4, kind), (16, offset), (20, size)] { elf[e + at..e + at + 4].copy_from_slice(&value.to_le_bytes()); }
        }
        let path = std::env::temp_dir().join(format!("q2nd-elf32-{}", std::process::id()));
        std::fs::write(&path, &elf).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let found = file_sections(&crate::rt::Blocking::dedicated_thread(), &file, &[".q2tools", ".eh_frame"]);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(found, Some(vec![(0x1000, 0x2000)]));
    }

    #[test]
    fn mapped_files_read_as_written_and_bad_files_are_refused() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let dir = std::env::temp_dir().join(format!("q2nd-map-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // Not a whole number of pages, so the last page is partly the file.
        let bytes: Vec<u8> = (0..10_000u32).map(|n| (n * 7 + 3) as u8).collect();
        let font = dir.join("font.ttf");
        std::fs::write(&font, &bytes).unwrap();
        let mapped = super::map_file(&blocking, &font, bytes.len()).unwrap();
        // The mapping outlives the file's removal and its name.
        std::fs::remove_file(&font).unwrap();
        assert_eq!(mapped, &bytes[..]);
        assert_eq!(mapped.as_ptr() as usize % 4096, 0);
        let refused = |path: &std::path::Path, limit| super::map_file(&blocking, path, limit).unwrap_err().kind();
        std::fs::write(&font, &bytes).unwrap();
        assert_eq!(refused(&font, bytes.len() - 1), std::io::ErrorKind::InvalidData);
        let empty = dir.join("empty");
        std::fs::write(&empty, b"").unwrap();
        assert_eq!(refused(&empty, 1 << 20), std::io::ErrorKind::InvalidData);
        assert_eq!(refused(&dir, 1 << 20), std::io::ErrorKind::InvalidData);
        assert_eq!(refused(&dir.join("missing"), 1 << 20), std::io::ErrorKind::NotFound);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
