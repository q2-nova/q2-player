#![forbid(unsafe_code)]
//! What Settings → About → System Information shows: the software and
//! hardware this player runs on, read from the kernel and the root filesystem.
//! Every field is optional; the page leaves out what a system doesn't expose.

use std::io::Read;
use crate::rt::Blocking;
use std::path::Path;
use std::sync::OnceLock;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SystemInfo {
    /// `PRETTY_NAME` from os-release, e.g. "Buildroot 2020.02.1".
    pub os: Option<String>,
    /// `uname -r`, e.g. "5.10.21+".
    pub kernel: Option<String>,
    /// `uname -v`: build number, options and date.
    pub kernel_build: Option<String>,
    /// `uname -m`, e.g. "mips".
    pub arch: Option<String>,
    /// The device tree's model.
    pub model: Option<String>,
    /// The SoC, from cpuinfo's `machine`, e.g. "Ingenic X2000".
    pub soc: Option<String>,
    /// The CPU core's model, e.g. "Ingenic XBurst II V0.0 FPU V0.0".
    pub cpu: Option<String>,
    pub cores: usize,
    pub memory_bytes: Option<u64>,
    pub card_bytes: Option<u64>,
}

/// Public installed OS identity. App releases (`CARGO_PKG_VERSION`), kernel
/// builds are deliberately independent of this tag.
static VERSION: OnceLock<Option<String>> = OnceLock::new();

/// Memory-only during interaction; normal startup initializes the public tag.
pub fn installed_os_version() -> Option<&'static str> {
    VERSION.get().and_then(Option::as_deref)
}

/// Ordinary startup boundary, before entering the UI runtime.
pub(crate) fn load_installed_os_version() {
    assert!(crate::rt::may_wait_for_tasks(), "load installed OS identity on ordinary startup");
    let blocking = Blocking::dedicated_thread();
    VERSION.get_or_init(|| read_installed_os_version(&blocking));
}

fn read_installed_os_version(blocking: &Blocking) -> Option<String> {
    os_version(blocking, std::fs::File::open("/etc/q2-os-release.json").ok()?)
}

fn os_version(_blocking: &Blocking, reader: impl Read) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Release { release_tag: String }
    // This is small public metadata, never application configuration. Bound
    // both I/O and the displayed value; unknown fields permit schema evolution.
    const MAX_BYTES: u64 = 4096;
    let mut bytes = Vec::new();
    reader.take(MAX_BYTES + 1).read_to_end(&mut bytes).ok()?;
    if bytes.len() as u64 > MAX_BYTES { return None; }
    let release: Release = serde_json::from_slice(&bytes).ok()?;
    let tag = release.release_tag;
    (!tag.is_empty() && tag.len() <= 64
        && tag.bytes().all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b)))
        .then_some(tag)
}

impl SystemInfo {
    /// Reads everything once per process; later calls return the same copy.
    /// Call it off the UI thread.
    pub fn get(blocking: &Blocking, card: &Path) -> SystemInfo {
        static INFO: OnceLock<SystemInfo> = OnceLock::new();
        INFO.get_or_init(|| SystemInfo::read(blocking, card)).clone()
    }

    fn read(blocking: &Blocking, card: &Path) -> SystemInfo {
        let text = |p: &str| std::fs::read_to_string(p).ok();
        let cpuinfo = text("/proc/cpuinfo").unwrap_or_default();
        let (kernel, kernel_build, arch) = uname();
        SystemInfo {
            os: text("/etc/os-release").as_deref().and_then(os_name),
            kernel,
            kernel_build,
            arch,
            // A device tree without a model: MIPS builds are only for the Q2.
            model: std::fs::read("/proc/device-tree/model").ok().and_then(|b| clean(&String::from_utf8_lossy(&b)))
                .or_else(|| cfg!(target_arch = "mips").then(|| "Shanling Q2".to_string())),
            soc: cpu_field(&cpuinfo, "machine").map(|m| soc_name(&m)),
            cpu: cpu_field(&cpuinfo, "cpu model").or_else(|| cpu_field(&cpuinfo, "model name")),
            cores: cpuinfo.lines().filter(|l| l.split(':').next().is_some_and(|k| k.trim() == "processor")).count(),
            memory_bytes: text("/proc/meminfo").as_deref().and_then(mem_total),
            card_bytes: card_size(blocking, card),
        }
    }
}

/// Trims, collapses runs of whitespace and drops NULs (device-tree strings end in one).
fn clean(s: &str) -> Option<String> {
    let s = s.split(|c: char| c.is_whitespace() || c == '\0').filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ");
    (!s.is_empty()).then_some(s)
}

fn os_name(release: &str) -> Option<String> {
    let field = |key: &str| release.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
        .map(|v| v.trim().trim_matches('"').to_string()).and_then(|v| clean(&v));
    field("PRETTY_NAME").or_else(|| Some(format!("{} {}", field("NAME")?, field("VERSION_ID").unwrap_or_default())).and_then(|v| clean(&v)))
}

fn cpu_field(cpuinfo: &str, key: &str) -> Option<String> {
    cpuinfo.lines().find_map(|l| {
        let (k, v) = l.split_once(':')?;
        if k.trim() == key { clean(v) } else { None }
    })
}

/// "ingenic,x2000" → "Ingenic X2000".
fn soc_name(machine: &str) -> String {
    let Some((vendor, chip)) = machine.split_once(',') else { return machine.to_string() };
    let mut v = vendor.chars();
    let vendor = v.next().map(|c| c.to_uppercase().chain(v).collect::<String>()).unwrap_or_default();
    format!("{vendor} {}", chip.to_uppercase())
}

fn mem_total(meminfo: &str) -> Option<u64> {
    let line = meminfo.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb * 1024)
}

fn card_size(blocking: &Blocking, path: &Path) -> Option<u64> {
    let st=crate::platform::system::filesystem(blocking,path).ok()?;
    Some(st.f_blocks as u64 * st.f_frsize as u64)
}

fn uname() -> (Option<String>, Option<String>, Option<String>) {
    let Ok(u)=crate::platform::system::uname() else {return (None,None,None)};
    let field = |f: &[libc::c_char]| {
        let bytes: Vec<u8> = f.iter().take_while(|c| **c != 0).map(|c| *c as u8).collect();
        clean(&String::from_utf8_lossy(&bytes))
    };
    (field(&u.release), field(&u.version), field(&u.machine))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installed_release_tag_is_typed_bounded_and_independent() {
        let blocking = Blocking::dedicated_thread();
        let parse = |s: &str| os_version(&blocking, s.as_bytes());
        assert_eq!(parse(r#"{"release_tag":"V0.16","profile":"split-installed-candidate","app_sha256":"generated"}"#).as_deref(), Some("V0.16"));
        for invalid in ["", "not json", "{}", r#"{"release_tag":16}"#,
            r#"{"release_tag":null}"#, r#"{"release_tag":""}"#,
            r#"{"release_tag":"V0.16\nforged"}"#, r#"{"release_tag":"V0.16 with text"}"#,
            r#"{"kernel":"6.12-fixture","app_version":"0.1.0"}"#] {
            assert_eq!(parse(invalid), None, "{invalid}");
        }
        assert_eq!(parse(&format!(r#"{{"release_tag":"{}"}}"#, "x".repeat(65))), None);
        assert_eq!(parse(&format!(r#"{{"release_tag":"V0.16","padding":"{}"}}"#, "x".repeat(4096))), None);
        struct Broken;
        impl Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::NotFound.into())
            }
        }
        assert_eq!(os_version(&blocking, Broken), None);
    }

    #[test]
    fn os_release_names() {
        // The stock Q2 rootfs's file.
        let stock = "NAME=Buildroot\nVERSION=2020.02.1-svn1\nID=buildroot\nVERSION_ID=2020.02.1\nPRETTY_NAME=\"Buildroot 2020.02.1\"\n";
        assert_eq!(os_name(stock).as_deref(), Some("Buildroot 2020.02.1"));
        assert_eq!(os_name("NAME=\"Q2 OS\"\nVERSION_ID=3\n").as_deref(), Some("Q2 OS 3"));
        assert_eq!(os_name("ID=x\n"), None);
    }

    #[test]
    fn x2000_cpuinfo() {
        // From the Q2 (build/hardware-support/device-inventory.txt).
        let cpuinfo = "system type\t\t: xburst2-based\nmachine\t\t\t: ingenic,x2000\nprocessor\t\t: 0\n\
            cpu model\t\t: Ingenic XBurst II V0.0  FPU V0.0\nisa\t\t\t: mips1 mips2 mips32r2 mips32r5\n\n\
            processor\t\t: 1\ncpu model\t\t: Ingenic XBurst II V0.0  FPU V0.0\n";
        assert_eq!(cpu_field(cpuinfo, "machine").map(|m| soc_name(&m)).as_deref(), Some("Ingenic X2000"));
        assert_eq!(cpu_field(cpuinfo, "cpu model").as_deref(), Some("Ingenic XBurst II V0.0 FPU V0.0"));
        assert_eq!(cpuinfo.lines().filter(|l| l.split(':').next().is_some_and(|k| k.trim() == "processor")).count(), 2);
    }

    #[test]
    fn memory_total() {
        assert_eq!(mem_total("MemTotal:         120368 kB\nMemFree:  1 kB\n"), Some(120368 * 1024));
        assert_eq!(mem_total("MemFree:  1 kB\n"), None);
    }

    #[test]
    fn device_tree_strings() {
        assert_eq!(clean("Shanling Q2\0").as_deref(), Some("Shanling Q2"));
        assert_eq!(clean(" \0"), None);
    }

    #[test]
    fn reads_this_host() {
        let info = SystemInfo::read(&Blocking::dedicated_thread(), Path::new("."));
        assert!(info.kernel.is_some() && info.cores > 0 && info.memory_bytes.is_some_and(|m| m > 0));
    }
}
