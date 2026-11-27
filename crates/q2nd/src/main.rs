//! q2nd: a Navidrome player for the Shanling Q2 on the mainline image.
// A std lock held across an await blocks the runtime's other tasks
// (.github/workflows/lint.yml runs clippy).
#![deny(clippy::await_holding_lock, clippy::await_holding_refcell_ref)]
// MSA intrinsics (core::arch::mips) and #[target_feature(enable = "msa")]
// on the Q2's nightly only; every MSA routine keeps a portable fallback
// (docs/optimization-rig.md).
#![cfg_attr(target_arch = "mips", feature(stdarch_mips, mips_target_feature))]

mod audio;
mod bluetooth;
mod config;
mod gfx;
mod gfx_msa;
mod resize_msa;
mod health;
mod input;
mod library;
mod metrics;
mod telemetry;
#[cfg(test)]
mod test_alloc;
mod diagnostics;
mod telemetry_control;
mod playtrace;
mod process_profile;
mod numeric_probe;
mod net;
mod tailscale;
mod platform;
mod perf;
mod radio_import;
mod resources;
mod rt;
mod runtime_log;
mod sysinfo;
mod script;
mod shard;
mod ui;
#[path = "../../shared/timezones.rs"]
mod timezones;
mod usb;
mod ux;

use audio::player;
use config::Paths;
use platform::Platform;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use ui::{App, Request, UiMsg};

/// The data folder on the Q2 when neither `--data` nor `Q2ND_DATA` names one.
const DEVICE_ROOT: &str = "/state/Q2Navidrome";
static TERMINATE: AtomicBool = AtomicBool::new(false);
static TERMINATE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn terminate(_: libc::c_int) {
    TERMINATE.store(true, Ordering::Relaxed);
    // write(2) is async-signal-safe; sending through Rust's UI channel is not.
    // Preserve the interrupted thread's errno. The socket is nonblocking, and
    // an already queued byte is sufficient if repeated signals fill it.
    unsafe {
        let errno = libc::__errno_location();
        let saved = *errno;
        let fd = TERMINATE_FD.load(Ordering::Relaxed);
        if fd >= 0 { libc::write(fd, b"Q".as_ptr().cast(), 1); }
        *errno = saved;
    }
}

fn termination_wakeup(tx: ui::UiTx) -> std::io::Result<()> {
    use std::os::{fd::IntoRawFd, unix::net::UnixDatagram};
    let (reader, writer) = UnixDatagram::pair()?;
    writer.set_nonblocking(true)?;
    reader.set_nonblocking(true)?;
    // A task on `io` reads the handler's bytes (no thread of its own).
    rt::io().spawn(async move {
        let Ok(reader) = tokio::net::UnixDatagram::from_std(reader) else {
            log::error!("termination wakeup: not registered");
            return;
        };
        let mut byte = [0];
        while reader.recv(&mut byte).await.is_ok() {
            let _ = tx.send(UiMsg::Quit);
        }
    });
    // Keep this close-on-exec descriptor for the process lifetime: a signal
    // handler must never race descriptor closure/reuse. The reader waits with
    // no timer, so a screen-off idle player is not woken by it.
    TERMINATE_FD.store(writer.into_raw_fd(), Ordering::Relaxed);
    Ok(())
}

/// Keep first-boot diagnostics useful even if the kernel kills us before
/// the UI starts. Only memory counters are recorded, never settings.
fn log_startup(stage: &str) {
    match stage {
        "app start" => metrics::record(metrics::Event::AppStart, Duration::ZERO),
        "first frame" => metrics::record(metrics::Event::FirstFrame, Duration::ZERO),
        _ => {}
    }
    let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let memory = status.lines().filter(|l| l.starts_with("VmRSS:") || l.starts_with("VmHWM:"))
        .map(str::trim).collect::<Vec<_>>().join(", ");
    log::info!("startup {stage}: {memory}");
    let marker = match stage {
        "app start" => "app-start",
        "platform ready" => "platform-ready",
        "library ready" => "library-ready",
        "network ready" => "network-workers-ready",
        "fonts ready" => "fonts-ready",
        "UI ready" => "ui-ready",
        "first frame" => "first-frame",
        _ => return,
    };
    record_boot_stage(marker);
}

/// Fixed, non-secret stage names only. Network callers record observations,
/// never addresses, SSIDs, peer identities or an inferred usable connection.
fn record_boot_stage(stage: &'static str) {
    if let Some(runtime) = std::env::var_os("Q2ND_RUNTIME") {
        record_boot_time(&PathBuf::from(runtime), stage);
    }
}

fn startup_profile_enabled() -> bool {
    std::env::var_os("Q2ND_STARTUP_PROFILE").is_some_and(|value| value == "1")
}

/// Diagnostic-only fixed milestones. Normal installations leave this disabled;
/// the existing bounded RAM writer never exports settings or log contents.
fn record_profile_stage(stage: &'static str) {
    if startup_profile_enabled() { record_boot_stage(stage); }
}

/// Dedicated RAM-only metadata. Capture time before enqueue; the numeric
/// owner reports bounded queue and sink failures without exporting user data.
fn record_boot_time(runtime: &std::path::Path, stage: &'static str) {
    let Some(time)=platform::system::boottime_us() else {metrics::clock_failure();return};
    record_boot_time_at(runtime,stage,time);
}
fn record_boot_time_at(runtime:&std::path::Path,stage:&'static str,time:u64) {
    metrics::boot_at(runtime, stage, time);
}

#[cfg(test)]
mod boot_time_tests {
    use super::*;

    #[test]
    fn resume_without_input_exits_to_supervision_instead_of_healthy_unusable_ui() {
        struct ResumeFault { input: bool, helper: bool, screens: Vec<bool> }
        impl Platform for ResumeFault {
            fn present<'a>(&'a mut self, _: &'a gfx::Canvas) -> platform::PresentFuture<'a> { Box::pin(async { panic!("fatal resume cannot present a healthy UI") }) }
            fn set_screen(&mut self, on: bool) { self.screens.push(on); }
            fn set_brightness(&mut self, _: u8) {}
            fn battery(&mut self) -> platform::Battery { Default::default() }
            fn headphones(&mut self) -> Option<bool> { None }
            fn audio_hw(&mut self) -> Box<dyn audio::alsa::AudioHw> {
                Box::new(audio::alsa::SoftHw(audio::alsa::FAKE.into()))
            }
            fn suspend(&mut self) -> platform::SuspendFuture<'_> {
                Box::pin(async { if self.helper { Ok(()) } else { Err("Synthetic resume refusal".into()) } })
            }
            fn power_off(&mut self) -> platform::SuspendFuture<'_> { Box::pin(async { unreachable!() }) }
            fn controls_ready(&self) -> bool { self.input }
            fn is_device(&self) -> bool { true }
        }
        for input in [false, true] {
            for helper in [false, true] {
                let mut app = ui::settings_tests::fixture();
                app.player.prepare_sleep().unwrap();
                app.lib.prepare_sleep().unwrap();
                let mut platform = ResumeFault { input, helper, screens: Vec::new() };
                app.handle(UiMsg::PowerPrepared { off: false, result: Ok(()) }, &mut platform);
                let code = rt::Ui::new().unwrap().block_on(request(&mut app, &mut platform, Request::EnterPower { off: false }));
                assert_eq!(code, if input { None } else { Some(1) }, "input={input} helper={helper}");
                assert_eq!(app.screen_on, input);
                assert_eq!(platform.screens, if input { vec![false, true] } else { vec![false] });
                assert_eq!(app.player.prepare_sleep().is_ok(), input,
                    "only the usable restored UI releases its own barrier");
                if input { app.player.cancel_handoff(); }
                // The real audio owner must still finish its durable shutdown
                // while frozen on the fatal path; no emergency kill is needed.
                app.player.shutdown();
            }
        }
    }

    #[test]
    fn dedicated_checkpoint_file_contains_only_uptime_and_fixed_stages() {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../../build/perf/boot-times-{}", fastrand::u64(..)));
        std::fs::create_dir_all(&root).unwrap();
        record_boot_time(&root, "app-start");
        record_boot_time(&root, "first-frame");
        let text = std::fs::read_to_string(root.join("boot-times")).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        for (line, expected) in lines.iter().zip(["app-start", "first-frame"]) {
            let fields: Vec<_> = line.split_whitespace().collect();
            assert_eq!(fields.len(), 2);
            assert!(fields[0].parse::<f64>().unwrap() >= 0.0);
            assert_eq!(fields[1], expected);
        }
        record_boot_time(&root.join("missing"), "first-frame");
        assert!(!root.join("missing").exists());
    }
}

#[derive(PartialEq)]
enum Mode {
    Sim,
    Headless,
    DrmTest,
    Mainline,
}

fn mainline_startup_cancelled(mode: &Mode) -> bool {
    // A normal service stop can arrive before the UI loop exists. Once TERM
    // is remembered, avoid starting more card users merely to shut them down.
    if !matches!(mode, Mode::Mainline | Mode::DrmTest) || !TERMINATE.load(Ordering::Relaxed) {
        return false;
    }
    record_boot_stage("startup-cancelled");
    true
}

struct Args {
    mode: Mode,
    data: PathBuf,
    script: Option<PathBuf>,
}

fn args() -> Args {
    let device = cfg!(any(target_arch = "mips", target_arch = "mips64"));
    let mut a = Args {
        mode: if std::env::var("Q2ND_PLATFORM").as_deref() == Ok("headless") {
            Mode::Headless
        } else if std::env::var("Q2ND_PLATFORM").as_deref() == Ok("drm-test") {
            Mode::DrmTest
        } else if std::env::var("Q2ND_PLATFORM").as_deref() == Ok("mainline") {
            Mode::Mainline
        } else if device { Mode::Mainline } else { Mode::Sim },
        data: std::env::var_os("Q2ND_DATA").map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(if device { DEVICE_ROOT } else { "sim-data" })),
        script: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--data" => a.data = it.next().map(PathBuf::from).unwrap_or(a.data),
            "--sim" => a.mode = Mode::Sim,
            "--headless" => a.mode = Mode::Headless,
            "--mainline" => a.mode = Mode::Mainline,
            "--drm-test" => a.mode = Mode::DrmTest,
            "--script" => a.script = it.next().map(PathBuf::from),
            "--help" | "-h" => {
                println!(
                    "q2nd [--data DIR] [--sim | --mainline | --headless [--script FILE]]\n\
                     The emulator-only --drm-test mode uses KMS; Q2ND_DRM_INPUT=1 and Q2ND_DRM_ALSA=1 enable evdev/ALSA.\n\
                     Headless scripts: one step a line: `scroll N`, `center`, `back`, `play`,\n\
                     `hold center|back|play`, `wait MS`, `await MS [screen:|overlay:]TITLE`, `present MS`, `shot FILE.png`,\n\
                     `record-text`, `text NAME`, `await-play MS STATE [ADVANCE_MS]`, `fault-descriptor`\n\
                     (fault journeys: tests/ui/ux_faults.py)."
                );
                std::process::exit(0);
            }
            other => eprintln!("ignoring {other}"),
        }
    }
    a
}

fn main() {
    assert!(rt::may_wait_for_tasks(), "main startup belongs to an ordinary thread");
    platform::resident::tune_heap();
    let argv: Vec<_> = std::env::args_os().collect();
    if argv.len() == 2 && argv[1] == "--version" {
        println!("{}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if argv.get(1).is_some_and(|s| s == "--bt-probe") {
        let arguments: Vec<String> = argv[2..].iter().map(|a| a.to_string_lossy().into_owned()).collect();
        std::process::exit(bluetooth::probe::run(&arguments));
    }
    if argv.get(1).is_some_and(|s| s == "--telemetry-process-exit") {
        let result=(|| -> std::io::Result<bool> {
            let invalid=||std::io::Error::other("invalid process-exit arguments");
            if argv.len()!=6 {return Err(invalid());}
            let value=|index:usize|argv[index].to_str().ok_or_else(invalid);
            diagnostics::record_process_exit(&rt::Blocking::dedicated_thread(),std::path::Path::new("/state/diagnostics"),
                value(2)?.parse().map_err(|_|invalid())?,value(3)?.parse().map_err(|_|invalid())?,
                value(4)?.parse().map_err(|_|invalid())?,value(5)?)
        })();
        std::process::exit(match result {Ok(true)=>0,Ok(false)=>3,Err(_)=>1});
    }
    if argv.get(1).is_some_and(|s| s == "--telemetry-context-reload") {
        let result = argv.get(2).and_then(|v|v.to_str())
            .ok_or_else(||std::io::Error::other("telemetry control nonce required"))
            .and_then(|nonce|rt::Ui::new()?.block_on(telemetry_control::request_reload(nonce)));
        match result {
            Ok(receipt) => println!("{}", serde_json::to_string(&receipt).expect("numeric control receipt")),
            Err(_) => { eprintln!("telemetry context transition failed"); std::process::exit(1); }
        }
        return;
    }
    if argv.get(1).is_some_and(|s| s == "--runtime-diagnostics-fixture") {
        let result=if argv.len()==4 {
            diagnostics::fixture(&rt::Blocking::dedicated_thread(),PathBuf::from(&argv[2]),argv[3].to_str().unwrap_or(""))
        } else {Err(std::io::Error::other("usage: --runtime-diagnostics-fixture GENERATED_DIRECTORY capture|deliver"))};
        if result.is_err() {eprintln!("generated runtime diagnostic fixture failed");std::process::exit(1);}
        return;
    }
    metrics::initialize();
    record_boot_stage("main-entry");
    if argv.get(1).is_some_and(|s| s == "--radio-import") {
        let result=match (argv.get(2),argv.get(3)) {
            (Some(manifest),Some(config))=>radio_import::run(&rt::Blocking::dedicated_thread(), std::path::Path::new(manifest),std::path::Path::new(config),argv.get(4).is_some_and(|a| a=="--apply")),
            _=>Err("usage: --radio-import PUBLIC_MANIFEST SAVED_CONFIG [--apply]".into()),
        };
        if let Err(e)=result {eprintln!("radio import: {e}");std::process::exit(1);}
        return;
    }
    // Separate from --bench: hardware access requires this explicit fixture
    // entry and --pcm. Normal configuration and station routing are untouched.
    if argv.get(1).is_some_and(|s| s == "--radio-metadata-fixture") {
        let result = if argv.len() == 3 || (argv.len() == 4 && argv[3] == "--pcm") {
            perf::radio_metadata::run(&rt::Blocking::dedicated_thread(), std::path::Path::new(&argv[2]), argv.len() == 4)
        } else { Err("usage: --radio-metadata-fixture SPEC [--pcm]".into()) };
        let result = result.and(finish_fixture_logging());
        if let Err(e) = result { eprintln!("radio fixture: {e}"); std::process::exit(1); }
        return;
    }
    if argv.get(1).is_some_and(|s| s == "--bench") {
        let result = argv.get(2).ok_or_else(|| "--bench needs a JSON specification".to_string())
            .and_then(|p| perf::run(std::path::Path::new(p)));
        let result = result.and(finish_fixture_logging());
        if let Err(e) = result { eprintln!("benchmark: {e}"); std::process::exit(1); }
        return;
    }
    let args = args();
    record_profile_stage("profile-arguments-ready");
    // The launcher requests a graceful restart with TERM.
    // Before channel setup the atomic remembers a termination request; after
    // setup a socket wake also interrupts a long screen-off UI wait.
    unsafe { libc::signal(libc::SIGTERM, terminate as *const () as libc::sighandler_t); }
    if mainline_startup_cancelled(&args.mode) {
        record_boot_stage("shutdown-exit-requested");
        return;
    }
    let paths = if args.mode == Mode::Mainline { Paths::mainline(args.data.clone()) }
        else { Paths::new(args.data.clone()) };
    let config = {
        assert!(rt::may_wait_for_tasks(), "configuration startup belongs to the main thread");
        let startup_blocking = rt::Blocking::dedicated_thread();
        record_profile_stage("profile-paths-selected");
        if let Err(e) = paths.create_all(&startup_blocking) {
            eprintln!("{}: {e}", paths.root.display());
        }
        record_profile_stage("profile-paths-created");
        runtime_log::init(&paths.log());
        record_profile_stage("profile-logger-ready");
        log::info!("q2nd {} starting, data in {}", env!("CARGO_PKG_VERSION"), paths.root.display());
        log_startup("app start");

        paths.load_config(&startup_blocking)
    };
    // Before the library and the player take the trace.
    playtrace::configure(config.diagnostics.playtrace);
    record_boot_stage("config-ready");
    if mainline_startup_cancelled(&args.mode) {
        // No library/player exists, and the loaded configuration is unchanged.
        record_boot_stage("shutdown-exit-requested");
        return;
    }
    // The zone set here; the RTC holds UTC.
    let tz = (!config.display.timezone.is_empty()).then(|| config.display.timezone.clone());
    if let Some(tz) = tz {
        unsafe extern "C" { fn tzset(); }
        // The startup logger already called localtime_r. glibc's reentrant
        // conversion caches that initial zone until tzset explicitly reloads
        // TZ; changing the environment alone leaves the clock/tint in UTC.
        // SAFETY: no other threads yet.
        unsafe { std::env::set_var("TZ", tz); tzset(); }
    }
    // ALSA 1.1.6 can deadlock an ioplug (Bluetooth) behind plug when its
    // locks are on; the player uses its PCM from one thread only.
    unsafe { std::env::set_var("LIBASOUND_THREAD_SAFE", "0") };

    // About must never perform its first flash read on the UI executor.
    sysinfo::load_installed_os_version();
    let diagnostic_directory = if args.mode == Mode::Mainline { PathBuf::from("/state/diagnostics") }
        else { paths.root.join("diagnostics") };
    let effective_telemetry = {
        let blocking = rt::Blocking::dedicated_thread();
        let explicit = std::env::var_os("Q2ND_TELEMETRY_OVERLAY").map(PathBuf::from);
        let overlay = explicit.or_else(|| (args.mode == Mode::Mainline)
            .then(||PathBuf::from("/run/q2/telemetry-test.toml")));
        match overlay {
            Some(path) => match config::telemetry::load_overlay(&blocking, &config.telemetry, &path) {
                Ok(effective) => effective,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => config.telemetry.clone(),
                Err(_) => { log::warn!("Invalid telemetry overlay; optional reporting disabled"); config::Telemetry::default() }
            },
            None => config.telemetry.clone(),
        }
    };
    if diagnostics::reconfigure(&rt::Blocking::dedicated_thread(), &diagnostic_directory, &effective_telemetry).is_err() {
        log::warn!("Numeric diagnostics unavailable");
    }
    diagnostics::register(1);
    #[cfg(feature = "stall-sampler")]
    log::warn!("{}", diagnostics::DIAGNOSTIC_BUILD);

    // The runtimes exist from here on: `io` (its own thread) carries the
    // Wi-Fi monitor and HTTP; the UI runtime is built now so its cost is in every
    // measurement, and the UI loop moves onto it in stage 4.
    let ui_runtime = match rt::Ui::new() {
        Ok(runtime) => runtime,
        Err(e) => { log::error!("UI runtime: {e}"); std::process::exit(1); }
    };
    metrics::activate();
    runtime_log::start();
    rt::io();
    diagnostics::start_delivery();
    if args.mode == Mode::Mainline {
        rt::jobs().run_isolated(diagnostics::observe_kernel_archive);
        let baseline = config.telemetry.clone();
        let directory = diagnostic_directory.clone();
        if telemetry_control::start(&rt::Blocking::dedicated_thread(), move |blocking| {
            let (effective, present) = match config::telemetry::load_overlay(blocking, &baseline,
                std::path::Path::new(telemetry_control::OVERLAY)) {
                Ok(settings) => (settings, true),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (baseline.clone(), false),
                Err(error) => return Err(error),
            };
            diagnostics::reconfigure(blocking, &directory, &effective)?;
            Ok(telemetry_control::Applied { enabled:effective.enabled, stream:effective.stream.as_str().into(),
                run_id:effective.run_id, overlay_present:present })
        }).is_err() { log::warn!("Telemetry context control unavailable"); }
    }
    // HTTP (the subsonic crate's client) runs as tasks on `io`.
    subsonic::http::set_executor(|task| {
        rt::io().spawn(task);
    });
    let mut trace = perf::InputTrace::from_env();
    let (tx, rx) = ui::channel();
    if let Err(e) = termination_wakeup(tx.clone()) {
        log::error!("termination wakeup: {e}");
        std::process::exit(finish_observers(&ui_runtime, 1));
    }
    let fallbacks: Vec<PathBuf> = [
        "/usr/share/fonts/dejavu-sans-fonts/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/dejavu/DejaVuSans.ttf",
        // Emoji in titles and playlist names (monochrome Noto Emoji).
        "/usr/share/fonts/noto-emoji/NotoEmoji-Regular.ttf",
    ]
    .iter()
    // Emulator stages run the MIPS binary on the host: Q2ND_FONT_PREFIX
    // points these OS paths at a composed image's root. Unset on the Q2.
    .map(|p| match std::env::var_os("Q2ND_FONT_PREFIX") {
        Some(root) => PathBuf::from(root).join(p.trim_start_matches('/')),
        None => PathBuf::from(p),
    })
    .filter(|p| p.exists())
    .collect();
    let font_load = gfx::font::FontLoad::start(fallbacks);
    record_boot_stage("fonts-load-submitted");

    let input: platform::InputSink = {
        let tx = Mutex::new(tx.clone());
        Arc::new(move |raw| {
            let message = if matches!(raw, input::Raw::HeadphonesChanged) { UiMsg::Headphones }
                else { UiMsg::TimedInput { raw, received: std::time::Instant::now() } };
            let _ = tx.lock().unwrap().send(message);
        })
    };
    let wheel_step = Arc::new(AtomicI32::new(20));
    let mut platform: Box<dyn Platform> = match args.mode {
        #[cfg(feature = "sim")]
        Mode::Sim => match platform::sim::Sim::open(input) {
            Ok(s) => Box::new(s),
            Err(e) => {
                log::error!("no window ({e}); try --headless");
                drop(font_load);
                std::process::exit(finish_observers(&ui_runtime, 1));
            }
        },
        #[cfg(not(feature = "sim"))]
        Mode::Sim => {
            log::error!("built without the simulator");
            drop(font_load);
            std::process::exit(finish_observers(&ui_runtime, 1));
        }
        Mode::Headless => Box::new(platform::Headless { last: None }),
        Mode::DrmTest | Mode::Mainline => match platform::drm_test::DrmTest::open(config.display.brightness, input, wheel_step, args.mode == Mode::Mainline) {
            Ok(display) => Box::new(display),
            Err(e) => {
                log::error!("DRM test startup: {e}");
                drop(font_load);
                std::process::exit(finish_observers(&ui_runtime, 1));
            }
        },
    };
    log_startup("platform ready");
    // The existing display owner can show a font-independent image before
    // network/library initialization. Completing this flip is deliberately
    // separate from FirstFrame and the launcher's usable-UI readiness marker.
    let startup_cancelled = mainline_startup_cancelled(&args.mode);
    let startup_image = if startup_cancelled {
        Ok(())
    } else {
        let frame = ui::startup::frame(config.display.look, config.display.accent);
        let result = ui_runtime.block_on(platform.present(&frame));
        if result.is_ok() { record_boot_stage("startup-image"); }
        result
    };
    if startup_cancelled || startup_image.is_err() || mainline_startup_cancelled(&args.mode) {
        let exit_code = if let Err(error) = startup_image {
            log::error!("KMS startup image failed: {error}");
            1
        } else { 0 };
        // The font reader must finish before returning card ownership. Close
        // already opened controls/display, without creating a library, player
        // or network workers, or saving an App that was never constructed.
        record_boot_stage("startup-cancel-fonts-begin");
        drop(font_load);
        record_boot_stage("startup-cancel-fonts-returned");
        record_boot_stage("shutdown-platform-close-begin");
        drop(platform);
        record_boot_stage("shutdown-platform-close-returned");
        let code = finish_observers(&ui_runtime, exit_code);
        if code != 0 { std::process::exit(code); }
        return;
    }

    let notify = |msg: fn() -> UiMsg| {
        let tx = Mutex::new(tx.clone());
        move || {
            let _ = tx.lock().unwrap().send(msg());
        }
    };
    // Networking starts before the library opens (about 1.1 s on the Q2), so
    // Wi-Fi association and Tailscale overlap it (startup hunt 2026-11-10).
    let app_dir = std::env::current_exe().ok().and_then(|p| p.parent().map(PathBuf::from)).unwrap_or_default();
    let real_network = args.mode == Mode::Mainline;
    let network_dir = if args.mode == Mode::Mainline { PathBuf::from("/usr/libexec/q2-network") } else { app_dir.clone() };
    let net = net::Net::new(network_dir.join("wifi"),
        !real_network, Arc::new(notify(|| UiMsg::Net)));
    // Only the installed player owns the boot marker: rig and test launches
    // (Q2ND_INSTALLED_STORAGE=0, generated configs) must not change the next boot.
    let installed = std::env::var("Q2ND_INSTALLED_STORAGE").as_deref() == Ok("1");
    let net = if args.mode == Mode::Mainline && installed && std::path::Path::new("/state/network").is_dir() {
        net.with_autostart(net::AUTOSTART.into())
    } else { net };
    if config.wifi.enabled {
        ui_runtime.block_on(net.set_enabled_async(true, &config.wifi.networks));
    } else if args.mode == Mode::Mainline {
        // The kernel powers the Wi-Fi chip when it enumerates its SDIO host at
        // boot. With Wi-Fi off, the helper's down powers it off again (39 mW,
        // docs/validation/quick-experiments.md); nothing else would.
        ui_runtime.block_on(net.set_enabled_async(false, &[]));
    }
    let tail_state = if args.mode == Mode::Mainline { PathBuf::from("/state/network/tailscale") }
        else { paths.root.join("tailscale") };
    let mut tailscale = tailscale::Tailscale::new_notifying(network_dir, tail_state, !real_network, config.tailscale.enabled, Arc::new(notify(|| UiMsg::Tailscale)));
    log_startup("network ready");
    let lib = {
        let tx = Mutex::new(tx.clone());
        library::Library::open(
            paths.clone(),
            &config,
            Arc::new(move |ev| {
                let _ = tx.lock().unwrap().send(UiMsg::Lib(ev));
            }),
        )
    };
    log_startup("library ready");
    let outputs=if args.mode==Mode::Mainline {
        match audio::output::Monitor::start() {
            Ok(outputs)=>Some(outputs),
            Err(error)=>{log::warn!("USB DAC discovery unavailable: {error}");Some(audio::output::Monitor::detached())}
        }
    }else{Some(audio::output::Monitor::detached())};
    // Bluetooth headphones (docs/bluetooth.md): the thread starts only when switched on.
    let bluetooth = bluetooth::Bluetooth::new(
        if args.mode == Mode::Mainline { bluetooth::Options::mainline(&config.bluetooth) } else { bluetooth::Options::simulated(&paths.root) },
        Arc::new(notify(|| UiMsg::Bluetooth)));
    if let Some(outputs) = &outputs { bluetooth.set_output(outputs.bluetooth_switch()); }
    {
        let tx = Mutex::new(tx.clone());
        bluetooth.set_buttons(Arc::new(move |button| {
            use bluetooth::Button;
            let media = match button { Button::Play | Button::Pause | Button::Stop => input::Media::PlayPause, Button::Next => input::Media::Next, Button::Previous => input::Media::Prev,
                Button::VolumeUp => input::Media::VolumeUp, Button::VolumeDown => input::Media::VolumeDown };
            let _ = tx.lock().unwrap().send(UiMsg::Input(input::Raw::Media(media)));
        }));
    }
    bluetooth.set_volume_limit(config.playback.max_volume);
    bluetooth.set_enabled(config.bluetooth.enabled);
    let player = player::spawn_outputs(
        Arc::new(library::Tracks(lib.clone())),
        platform.audio_hw(),
        player::Settings {
            eq: config.playback.eq.clone(),
            volume: config.playback.volume,
            headphone_power: config.playback.headphone_power(),
            filter: config.playback.dac_filter,
            replay_gain: config.playback.replay_gain,
            gapless: config.playback.gapless,
        },
        Arc::new(notify(|| UiMsg::Player)),
        outputs,
    );

    record_boot_stage("fonts-join-begin");
    let fonts = match font_load.finish() {
        Ok(fonts) => fonts,
        Err(error) => {
            log::error!("{error}; startup cancelled");
            ui_runtime.block_on(tailscale.stop_async());
            player.shutdown();
            lib.save_now();
            drop(platform);
            std::process::exit(finish_observers(&ui_runtime, 1));
        }
    };
    log_startup("fonts ready");

    let mut app = App::new(lib.clone(), player.clone(), config, paths, net, tailscale, fonts, tx.clone());
    app.hardware = platform.hardware();
    app.bluetooth = bluetooth;
    ui_runtime.block_on(app.restore_queue());
    log_startup("UI ready");
    // Only announce readiness after a full frame can be rendered. The card
    // launcher uses this PID to confirm a candidate binary or roll it back.
    let frame = app.render();
    let code = match ui_runtime.block_on(platform.present(frame)) {
        Ok(()) => {
            app.presentation_completed();
            ui_runtime.block_on(app.sync_touchscreen(platform.as_mut()));
            log_startup("first frame");
            // Headless runs pin too when Q2ND_LOCK is set, so a scripted run
            // can compare the modes (resident::lock_mode).
            if args.mode == Mode::Mainline || (args.mode == Mode::Headless && std::env::var_os("Q2ND_LOCK").is_some()) {
                // After the first frame: reading in the code once must not delay it.
                drop(rt::spawn_blocking(|blocking| {
                    let pinned = platform::resident::lock_code(blocking);
                    // With Q2ND_LOCK=onfault the KiB are the ranges registered, not yet resident.
                    log::info!("code pinned ({:?}): {} KiB, {} KiB of the executable left unpinned, {} mappings failed", platform::resident::lock_mode(),
                        pinned.locked / 1024, pinned.unlocked / 1024, pinned.failed);
                }));
            }
            if let Some(runtime) = std::env::var_os("Q2ND_RUNTIME") {
                // Only a completed first presentation admits readiness.
                playtrace::serve(&rt::Blocking::dedicated_thread(), &PathBuf::from(&runtime), playtrace::global());
                let _ = std::fs::write(PathBuf::from(runtime).join("ready"), std::process::id().to_string());
            }
            let script_task=args.script.map(|path|script::run(path,tx.clone()));
            let mut rx = rx;
            // Install UI-thread health before entering executor polling.
            health::register(b'U');
            let code = ui_runtime.block_on(run(&mut app, platform.as_mut(), &mut rx, &mut trace));
            if let Some(script)=script_task {script.abort();let _=ui_runtime.block_on(script);}
            code
        }
        Err(error) => {
            // Use the same owned shutdown/drain as later failed flips. Never
            // publish readiness or lose the fatal diagnostic via an early exit.
            log::error!("KMS first presentation failed: {error}");
            1
        }
    };
    if let Some(trace) = trace {
        assert!(rt::may_wait_for_tasks(), "input trace closes after the UI runtime returns");
        trace.save(&rt::Blocking::dedicated_thread());
    }
    record_boot_stage("shutdown-begin");
    if ui_runtime.block_on(app.save_settings_async()) {
        record_boot_stage("shutdown-settings-saved");
    } else {
        log::warn!("Settings could not be saved during process shutdown");
        record_boot_stage("shutdown-settings-save-failed");
    }
    log::info!("stopping ({code})");
    let io = rt::io().stall();
    log::info!("io runtime: longest task poll {} us over {} polls", io.longest().as_micros(), io.polls());
    let ui = ui_runtime.stall();
    log::info!("ui runtime: longest poll {} us over {} polls", ui.longest().as_micros(), ui.polls());
    ui_runtime.block_on(lib.stop_shelf_auto_sync());
    ui_runtime.block_on(app.tailscale.stop_async());
    app.bluetooth.shutdown();
    record_boot_stage("shutdown-network-stopped");
    player.shutdown();
    record_boot_stage("shutdown-player-stopped");
    lib.save_now();
    record_boot_stage("shutdown-library-saved");
    if !ui_runtime.block_on(async { tokio::time::timeout(Duration::from_secs(1), app.settle_telemetry()).await.unwrap_or(false) }) {
        log::warn!("telemetry shutdown incomplete");
    }
    if matches!(args.mode, Mode::DrmTest | Mode::Mainline) {
        record_boot_stage("shutdown-platform-close-begin");
        drop(platform);
        record_boot_stage("shutdown-platform-close-returned");
    }
    std::process::exit(finish_observers(&ui_runtime, code));
}

/// Standalone generated workloads drain both observers on success and failure.
/// A workload requesting neither observer needs no shutdown runtime. Preserve
/// logger-before-numeric-seal ordering even when either bounded drain fails.
fn finish_fixture_logging() -> Result<(), String> {
    if !runtime_log::initialized() && !metrics::initialized() { return Ok(()); }
    // An invalid specification may return before the workload's activation.
    // Final markers must still enqueue rather than write before the bound.
    if metrics::initialized() { metrics::activate(); }
    let runtime = rt::Ui::new().map_err(|_| "Diagnostic log runtime unavailable".to_string())?;
    let logged = runtime.block_on(runtime_log::shutdown(Duration::from_secs(1))).record();
    if !logged { record_boot_stage("shutdown-logger-incomplete"); }
    record_boot_stage("shutdown-exit-requested");
    let recorded = runtime.block_on(metrics::settle());
    if !logged { Err("Diagnostic log drain incomplete".into()) }
    else if !recorded { Err("Diagnostic numeric drain incomplete".into()) }
    else { Ok(()) }
}

/// Stop logging first because its failure marker is itself numeric evidence.
/// The numeric seal admits only the preceding prefix; later background
/// observations are rejected outside it and cannot mutate its final proof.
fn finish_observers(runtime: &rt::Ui, code: i32) -> i32 {
    finish_logging(runtime);
    record_boot_stage("shutdown-exit-requested");
    if runtime.block_on(metrics::settle()) { code } else { 1 }
}

/// Ordinary exit boundary only; runtime producers never synchronously flush.
fn finish_logging(runtime: &rt::Ui) {
    assert!(rt::may_wait_for_tasks(), "await logger drain inside runtime code");
    if !runtime.block_on(runtime_log::shutdown(Duration::from_secs(1))).record() {
        record_boot_stage("shutdown-logger-incomplete");
    }
}

/// The main loop; returns the exit status.
async fn run(app: &mut App, platform: &mut dyn Platform, rx: &mut ui::UiRx, trace: &mut Option<perf::InputTrace>) -> i32 {
    loop {
        health::pulse();
        if TERMINATE.load(Ordering::Relaxed) { return 0; }
        if !platform.controls_ready() {
            log::error!("Input owner stopped; restarting the application under supervision");
            return 1;
        }
        let animating = app.animating();
        let mut timeout = if animating { Duration::from_millis(16) } else { app.next_wake() };
        if let Some(p) = platform.pump_interval() {
            timeout = timeout.min(p);
        }
        let mut msgs = Vec::new();
        let transitioning = app.screen_transitioning();
        let received = tokio::select! {
            message = tokio::time::timeout(health::wait(timeout), rx.recv()) => Some(message),
            () = app.restore_screen_async(platform, std::time::Instant::now()), if transitioning => None,
        };
        match received {
            Some(Ok(Some(m))) => msgs.push(m),
            Some(Ok(None)) => return 0,
            Some(Err(_)) | None => {}
        }
        while let Ok(m) = rx.try_recv() { msgs.push(m); }
        for m in msgs {
            let _latency = diagnostics::WallScope::new(1, match &m {
                UiMsg::Input(_) | UiMsg::TimedInput { .. } => 101,
                UiMsg::Loaded { .. } => 102, UiMsg::Net => 103,
                UiMsg::Tailscale => 104, UiMsg::Headphones => 105, _ => 100,
            });
            match m {
                UiMsg::TimedInput { raw, received } => {
                    if let Some(trace)=trace { trace.input(received); }
                    app.handle_async(UiMsg::TimedInput { raw, received },platform).await;
                }
                UiMsg::Screenshot(path) => {
                    // Own the rendered frame before handing encoding/card I/O
                    // to a finite worker. Await it to preserve script ordering.
                    let pixels = app.render().pixels.clone();
                    let output = path.clone();
                    let saved = rt::jobs().run_isolated(move |blocking| save_png(blocking, &output, &pixels))
                        .await.unwrap_or_else(|_| Err("Screenshot worker stopped".into()));
                    if let Err(e) = saved {
                        log::warn!("{}: {e}", path.display());
                    }
                }
                UiMsg::Quit => return 0,
                UiMsg::ScriptFailed(error) => { log::error!("headless script: {error}"); return 2; }
                m => app.handle_async(m, platform).await,
            }
        }
        {
            let _latency = diagnostics::WallScope::new(1, 106);
            app.tick_async(platform).await;
            app.sync_touchscreen(platform).await;
        }
        for r in app.take_requests() {
            if let Some(code) = request(app, platform, r).await {
                return code;
            }
        }
        if !platform.pump() {
            return 0;
        }
        if animating {
            app.mark_dirty();
        }
        if app.needs_draw() {
            let _latency = diagnostics::WallScope::new(1, 107);
            let started=trace.as_ref().map(|_| std::time::Instant::now());
            let frame = app.render();
            let drawn=trace.as_ref().map(|_| std::time::Instant::now());
            if let Err(error) = platform.present(frame).await {
                log::error!("KMS presentation failed: {error}");
                return 1;
            }
            app.presentation_completed();
            if let Some(trace)=trace {
                trace.frame(started.unwrap(),drawn.unwrap(),std::time::Instant::now());
            }
        }
        app.publish_script_observers();
    }
}

/// Carries out what a screen asked for; an exit status ends the app.
async fn request(app: &mut App, platform: &mut dyn Platform, r: Request) -> Option<i32> {
    match r {
        Request::Sleep => app.prepare_power(false),
        Request::PowerOff => app.prepare_power(true),
        Request::EnterPower { off } => {
            app.screen_on = false;
            app.sync_touchscreen(platform).await;
            let result = if off {
                platform.set_screen_async(false).await;
                platform.power_off().await
            } else {
                platform.blank_for_suspend().await;
                platform.suspend().await
            };
            let returned = if off { None } else { platform.take_resume_return() };
            if !platform.controls_ready() {
                // Reconstruct the complete platform through the ordinary,
                // bounded service restart path, as for a failed presentation.
                // A changed application identity remains a failed suspend gate.
                log::error!("Input did not recover; restarting the application under supervision");
                return Some(1);
            }
            // A successful suspend returns only after kernel/device resume.
            // A poweroff helper may refuse and return; restore the UI then.
            if !off || result.is_err() { app.finish_power_async(platform, result, returned).await; }
            else { app.await_power_off(); }
        }
        Request::SetBrightness(v) => platform.set_brightness_async(v).await,
        Request::Toast(t) => app.toast(t),
        Request::Wifi(on) => {
            if on { app.lib.set_offline_mode(false); }
            app.net.set_enabled_async(on, &app.config.wifi.networks).await;
        }
        Request::OfflineMode { offline, requested, networks } => {
            app.net.set_offline_mode_async(offline, requested, &networks).await;
        }
        Request::UsbDrive => app.open_usb_drive(),
        Request::Restart => app.begin_restart(),
        Request::RestartReady => return Some(0),
    }
    None
}

/// Saves the screen as the glass shows it, corners hidden.
fn save_png(_blocking: &rt::Blocking, path: &std::path::Path, pixels: &[u32]) -> Result<(), String> {
    let mut masked = pixels.to_vec();
    gfx::mask_corners(&mut masked);
    let mut rgb = Vec::with_capacity(pixels.len() * 3);
    for p in &masked {
        rgb.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, *p as u8]);
    }
    image::save_buffer(path, &rgb, gfx::WIDTH as u32, gfx::HEIGHT as u32, image::ColorType::Rgb8).map_err(|e| e.to_string())
}
