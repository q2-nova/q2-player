#![forbid(unsafe_code)]
//! Emulator integration mode: real standard KMS/backlight, scripted input and
//! optional ALSA in diagnostic mode; normal mainline mode requires ALSA and evdev.
use super::{Battery, InputSink, Platform};
use std::sync::{Arc, atomic::AtomicI32};
use crate::audio::alsa::{AudioHw, SoftHw, FAKE};
use crate::gfx::Canvas;
use super::display::Display;

use super::controls::{Backlight, WriteTicket};
#[cfg(test)]
use super::controls::backlight_value;

fn shutdown_error(child: &mut Option<std::process::Child>) -> Option<String> {
    match child.as_mut()?.try_wait() {
        Ok(None) => None,
        Ok(Some(_)) => {
            child.take();
            Some("Power off failed; player is still running".into())
        }
        Err(_) => Some("Power off status unavailable".into()),
    }
}

/// The launch belongs to the platform, not a cancellable UI callback. Waiting
/// for a finite worker leaves the UI live and retains its transaction barrier.
/// Once started, the helper must survive this player and owns its teardown.
#[derive(Default)]
struct Shutdown {
    launch: Option<tokio::sync::oneshot::Receiver<Result<std::process::Child, String>>>,
    child: Option<std::process::Child>,
}
impl Shutdown {
    fn start(&mut self) -> Result<(), String> {
        self.start_with(launch_shutdown)
    }

    fn start_with(&mut self, launch: impl FnOnce(&crate::rt::Blocking) -> Result<std::process::Child, String> + Send + 'static) -> Result<(), String> {
        if self.launch.is_some() || self.child.is_some() { return Err("Power off is already running".into()); }
        let (send, receive) = tokio::sync::oneshot::channel();
        self.launch = Some(receive);
        crate::rt::jobs().run_isolated(move |blocking| { let _ = send.send(launch(blocking)); });
        Ok(())
    }

    fn error(&mut self) -> Option<String> {
        if let Some(launch) = &mut self.launch {
            match launch.try_recv() {
                Err(tokio::sync::oneshot::error::TryRecvError::Empty) => return None,
                Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                    self.launch.take();
                    return Some("Shutdown launcher stopped".into());
                }
                Ok(result) => {
                    self.launch.take();
                    match result {
                        Ok(child) => self.child = Some(child),
                        Err(error) => return Some(error),
                    }
                }
            }
        }
        shutdown_error(&mut self.child)
    }

    async fn started(&mut self) -> Result<(), String> {
        // Keep the receiver in the platform across cancellation. Once the
        // receive completes, install the child before yielding again.
        let result = self.launch.as_mut().ok_or("Shutdown launch unavailable")?
            .await.unwrap_or_else(|_| Err("Shutdown launcher stopped".into()));
        self.launch.take();
        self.child = Some(result?);
        Ok(())
    }
}

fn launch_shutdown(_blocking: &crate::rt::Blocking) -> Result<std::process::Child, String> {
    let output = std::fs::OpenOptions::new().create(true).append(true)
        .open("/run/log/poweroff.log").map_err(|_| "Shutdown diagnostics unavailable")?;
    std::fs::write("/run/q2/poweroff-status", "requested\n").map_err(|_| "Shutdown status unavailable")?;
    std::process::Command::new("/sbin/q2-safe-poweroff")
        .current_dir("/").stdin(std::process::Stdio::null())
        .stdout(output.try_clone().map_err(|_| "Shutdown diagnostics unavailable")?)
        .stderr(output).spawn().map_err(|_| "Shutdown helper unavailable".into())
}

/// Persistent stages live with the platform, never with a cancellable UI
/// future. Re-entry resumes recovery rather than issuing a second suspend.
#[derive(Default)]
enum SleepPhase {
    #[default]
    Idle,
    Closing,
    Helper(SleepHelper),
    Reopening(Result<(), String>),
}
struct SleepHelper {
    kernel_return: Option<tokio::sync::oneshot::Receiver<()>>,
    result: Option<tokio::sync::oneshot::Receiver<(Result<(), String>, std::time::Instant)>>,
}
impl SleepHelper {
    fn start(program: &'static str, retain_drm: bool) -> Self {
        Self::start_with(move |blocking| {
            let mut command = tokio::process::Command::new(program);
            command.env("Q2_NOTIFY_KERNEL_RETURN", "1").stdout(std::process::Stdio::piped());
            command.env_remove("Q2_RETAIN_DRM_FOR_PM");
            if retain_drm { command.env("Q2_RETAIN_DRM_FOR_PM", "1"); }
            super::process::spawn_reactor_child(blocking, command)
        })
    }
    fn start_with(spawn: impl FnOnce(&crate::rt::Blocking) -> std::io::Result<tokio::process::Child> + Send + 'static) -> Self {
        let (send, result) = tokio::sync::oneshot::channel();
        let (notify, kernel_return) = tokio::sync::oneshot::channel();
        crate::rt::io().spawn(async move {
            // Retain this coordinator across observer cancellation. Spawn's
            // exec handshake is finite work; only exit waits on the reactor.
            let status = match crate::rt::spawn_blocking(spawn).await {
                Ok(Ok(mut child)) => {
                    // Only this invocation owns the pipe. Legacy helpers and
                    // pre-entry failures fall back to the real exit boundary.
                    if let Some(mut output) = child.stdout.take() {
                        use tokio::io::AsyncReadExt;
                        let expected = b"Q2_KERNEL_RETURN\n";
                        let mut bytes = [0; 17];
                        let mut notify = Some(notify);
                        let observe = async {
                            if output.read_exact(&mut bytes).await.is_ok() && &bytes == expected {
                                let _ = notify.take().unwrap().send(());
                            }
                            // Drain unexpected output so a verbose failure
                            // cannot fill the pipe and block helper cleanup.
                            let mut discard = [0; 512];
                            while let Ok(1..) = output.read(&mut discard).await {}
                        };
                        tokio::select! {
                            status = child.wait() => status,
                            () = observe => child.wait().await,
                        }
                    } else {
                        child.wait().await
                    }
                }
                Ok(Err(error)) => Err(error),
                Err(_) => Err(std::io::Error::other("Sleep helper launch stopped")),
            };
            let returned = std::time::Instant::now();
            let result = match status {
                Ok(status) if status.success() => Ok(()),
                _ => Err("Sleep failed; see system status".into()),
            };
            let _ = send.send((result, returned));
        });
        Self { result: Some(result), kernel_return: Some(kernel_return) }
    }
    async fn wait(&mut self) -> (Result<(), String>, std::time::Instant) {
        let result = self.result.as_mut().unwrap().await.unwrap_or_else(|_| {
            (Err("Sleep helper stopped".into()), std::time::Instant::now())
        });
        self.result.take();
        result
    }
}
impl Drop for SleepHelper {
    fn drop(&mut self) {
        if let Some(result) = self.result.take() {
            assert!(crate::rt::may_wait_for_tasks(), "pending sleep helper belongs to the platform outside the UI executor");
            let _ = result.blocking_recv();
        }
    }
}

async fn suspend_input(
    phase: &mut SleepPhase, input: &mut Option<super::evdev::Input>,
    sink: InputSink, step: Arc<AtomicI32>, returned: &mut Option<std::time::Instant>,
    program: &'static str, api: Option<super::evdev::Api>, retain_drm: bool, start_resume: impl FnOnce(),
) -> Result<(), String> {
    if matches!(*phase, SleepPhase::Idle) {
        *returned = None;
        *phase = SleepPhase::Closing;
    }
    if matches!(*phase, SleepPhase::Closing) {
        if let Some(input) = input.as_mut() { input.close().await; }
        input.take();
        *phase = SleepPhase::Helper(SleepHelper::start(program, retain_drm));
    }
    if let SleepPhase::Helper(helper) = &mut *phase {
        if let Some(notice) = helper.kernel_return.as_mut() {
            let _ = notice.await;
            helper.kernel_return = None;
            start_resume();
        }
        let (result, timestamp) = helper.wait().await;
        *returned = Some(timestamp);
        *phase = SleepPhase::Reopening(result);
    }
    // Reopen even after a helper failure. Initial state snapshots
    // suppress already-held wake keys and touch until release.
    let recovery = super::evdev::Input::open_api_into(
        input, api, sink, step).await;
    let SleepPhase::Reopening(result) = std::mem::take(phase) else { unreachable!() };
    recovery.map_err(|_| "Input did not recover after sleep".to_string())?;
    result
}

fn start_resume(
    display: &mut Option<Display>,
    resume_display: &mut Option<Result<(), String>>,
    resume_battery: &mut Option<tokio::task::JoinHandle<Battery>>,
) {
    *resume_display = Some(display.as_mut().ok_or_else(|| "KMS owner unavailable".to_string())
        .and_then(|display| display.start_mode(true).map_err(|error| error.to_string())));
    *resume_battery = Some(crate::rt::io().spawn(super::controls::battery("/sys/class/power_supply".into())));
}

const SYSTEM_DRM_PM_MARKER: &str = "/run/q2/experimental-system-drm-pm";

fn system_drm_pm_requested(_blocking: &crate::rt::Blocking, path: &std::path::Path) -> bool {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path).and_then(|marker| marker.metadata())
        .is_ok_and(|metadata| metadata.is_file() && metadata.uid() == 0
            && metadata.mode() & 0o7777 == 0o600 && metadata.len() == 0)
}

#[derive(Default)]
enum ControlPhase {
    #[default]
    Idle,
    BacklightOff(WriteTicket),
    Disable,
    Enable,
    Backlight(WriteTicket),
}

pub struct DrmTest {
    display: Option<Display>,
    brightness: u8,
    backlight: Backlight,
    control_phase: ControlPhase,
    desired_screen: Option<bool>,
    brightness_dirty: bool,
    on: bool,
    alsa: bool,
    mainline: bool,
    resume_return: Option<std::time::Instant>,
    resume_display: Option<Result<(), String>>,
    resume_battery: Option<tokio::task::JoinHandle<Battery>>,
    sleep: SleepPhase,
    shutdown: Shutdown,
    input: Option<super::evdev::Input>,
    touchscreen: super::touchscreen::Touchscreen,
    input_sink: InputSink,
    wheel_step: Arc<AtomicI32>,
    clicker: Option<super::clicker::Clicker>,
    headphones: Option<super::headphones::Owner>,
}

/// The installed launcher has admitted the devices. Mixer muting owns neither
/// DRM nor input, and the player takes codec ownership only after both finish.
fn with_startup_mixer<T>(
    open: impl FnOnce() -> Result<T, String>,
    prepare: impl Fn() -> Result<(), String> + Sync,
) -> Result<T, String> {
    std::thread::scope(|scope| {
        let worker = std::thread::Builder::new().name("startup-mixer".into())
            .spawn_scoped(scope, &prepare);
        let platform = open();
        // Drain even on display/input error: a service handoff must never
        // return while the mixer still has access to the hardware.
        let prepared = match worker {
            Ok(worker) => worker.join().unwrap_or_else(|_| Err("Initial mixer preparation panicked".into())),
            Err(_) if platform.is_ok() => prepare(),
            Err(_) => Ok(()),
        };
        let platform = platform?;
        prepared?;
        Ok(platform)
    })
}

impl DrmTest {
    pub fn open(brightness: u8, sink: InputSink, wheel_step: Arc<AtomicI32>, mainline: bool) -> Result<Self, String> {
        if mainline && !std::path::Path::new("/dev/snd/pcmC0D0p").exists() {
            return Err("mainline ALSA playback device unavailable".into());
        }
        let alsa = mainline || std::env::var("Q2ND_DRM_ALSA").as_deref() == Ok("1");
        let open = || Self::open_display_and_input(brightness, sink, wheel_step, mainline, alsa);
        let mut result = if alsa {
            with_startup_mixer(open, || {
                crate::record_profile_stage("profile-platform-mixer-begin");
                let prepared = crate::audio::mixer::prepare_q2_playback();
                crate::record_profile_stage("profile-platform-mixer-returned");
                prepared
            })?
        } else {
            crate::record_profile_stage("profile-platform-mixer-returned");
            open()?
        };
        if mainline {
            result.touchscreen = super::touchscreen::Touchscreen::discover();
            match super::headphones::Headphones::open() {
                Ok(headphones) => result.headphones = Some(super::headphones::Owner::new(headphones)),
                Err(error) => log::warn!("Mainline headphone switches unavailable: {error}"),
            }
            match super::clicker::Clicker::open() {
                Ok(clicker) => result.clicker = Some(clicker),
                Err(error) => log::warn!("Mainline clicker unavailable: {error}"),
            }
        }
        result.set_brightness(brightness);
        crate::record_profile_stage("profile-platform-controls-returned");
        log::info!("Mainline KMS/backlight active; input={}; audio={}",
            if result.input.is_some() { "evdev" } else { "scripted" },
            if alsa { "ALSA hw:Q2,0" } else { "simulated" });
        Ok(result)
    }

    fn open_display_and_input(
        brightness: u8, sink: InputSink, wheel_step: Arc<AtomicI32>, mainline: bool, alsa: bool,
    ) -> Result<Self, String> {
        crate::record_profile_stage("profile-platform-display-begin");
        let display = Display::open()?;
        crate::record_profile_stage("profile-platform-display-returned");
        let mut result = Self { display: Some(display), brightness,
            backlight: Backlight::new("/sys/class/backlight/backlight".into()), control_phase: ControlPhase::Idle,
            desired_screen: None, brightness_dirty: false, on: true, alsa, mainline, resume_return: None, resume_display: None, resume_battery: None, sleep: SleepPhase::Idle, shutdown: Shutdown::default(), input: None, touchscreen: Default::default(), input_sink: sink.clone(), wheel_step: wheel_step.clone(), clicker: None, headphones: None };
        if mainline || std::env::var("Q2ND_DRM_INPUT").as_deref() == Ok("1") {
            result.input = Some(super::evdev::Input::open(sink, wheel_step)?);
        }
        crate::record_profile_stage("profile-platform-input-returned");
        Ok(result)
    }

    async fn retain_dark_scanout(&mut self) -> bool {
        self.settle_controls().await;
        let Some(display) = self.display.as_mut() else { return false; };
        if !self.on || display.settle().await.is_err() || !display.enabled() { return false; }
        // Submit through the existing ordered writer. Keep the logical screen
        // dark only after actual sysfs confirms that the PWM is at zero.
        self.control_phase = ControlPhase::Backlight(self.backlight.write(0));
        self.settle_controls().await;
        if !self.backlight.dark_readback().await { return false; }
        self.on = false;
        self.brightness_dirty = false;
        true
    }

    fn backlight(&mut self, level: u8) {
        self.backlight.write_ordinary(level);
    }

    /// Recover the exact accepted phase after cancellation, before any later
    /// modeset/presentation. The opaque display never enters a sysfs worker.
    pub(super) async fn settle_controls(&mut self) {
        loop {
            match &mut self.control_phase {
                ControlPhase::Idle => {
                    if let Some(on) = self.desired_screen.take() {
                        self.control_phase = if on { ControlPhase::Enable }
                            else { ControlPhase::BacklightOff(self.backlight.write(0)) };
                    } else if self.on && self.brightness_dirty && self.resume_display.is_none() {
                        self.brightness_dirty = false;
                        self.control_phase = ControlPhase::Backlight(self.backlight.write(self.brightness));
                    } else { return; }
                }
                ControlPhase::BacklightOff(ticket) => {
                    ticket.wait().await;
                    self.control_phase = ControlPhase::Disable;
                }
                ControlPhase::Disable => {
                    let result = self.display.as_mut().unwrap().enable_async(false).await;
                    self.control_phase = ControlPhase::Idle;
                    match result {
                        Ok(()) => self.on = false,
                        Err(error) => { log::error!("KMS blanking failed: {error}"); return; }
                    }
                }
                ControlPhase::Enable => {
                    let result = self.display.as_mut().unwrap().enable_async(true).await;
                    match result {
                        Ok(()) => {
                            self.on = true;
                            self.brightness_dirty = false;
                            self.control_phase = ControlPhase::Backlight(self.backlight.write(self.brightness));
                        }
                        Err(error) => {
                            self.control_phase = ControlPhase::Idle;
                            log::error!("KMS blanking failed: {error}"); return;
                        }
                    }
                }
                ControlPhase::Backlight(ticket) => {
                    ticket.wait().await;
                    self.control_phase = ControlPhase::Idle;
                }
            }
        }
    }

}

#[cfg(test)]
mod tests {
    use super::{backlight_value, with_startup_mixer};

    #[test]
    fn startup_drains_the_mixer_when_display_or_input_fails() {
        use std::sync::{atomic::{AtomicBool, Ordering}, mpsc};
        use std::time::Duration;

        let (mixer_entered, entered) = mpsc::channel();
        let (platform_failed, failed) = mpsc::channel();
        let failed = std::sync::Mutex::new(failed);
        let drained = &AtomicBool::new(false);
        let result = with_startup_mixer(
            || -> Result<(), String> {
                entered.recv_timeout(Duration::from_secs(2)).unwrap();
                platform_failed.send(()).unwrap();
                Err("input unavailable".into())
            },
            move || {
                mixer_entered.send(()).unwrap();
                failed.lock().unwrap().recv_timeout(Duration::from_secs(2)).unwrap();
                drained.store(true, Ordering::Release);
                Err("mixer unavailable".into())
            },
        );
        assert_eq!(result, Err("input unavailable".into()));
        assert!(drained.load(Ordering::Acquire));
    }

    #[test]
    fn startup_cannot_admit_the_open_platform_if_muting_fails() {
        assert_eq!(
            with_startup_mixer(|| Ok("opened platform"), || Err("DAC mute did not verify".into())),
            Err("DAC mute did not verify".into()),
        );
        assert_eq!(
            with_startup_mixer(|| Ok("opened platform"), || Ok(())),
            Ok("opened platform"),
        );
    }

    #[test]
    fn repeated_screen_observers_join_the_accepted_backlight_without_requeue() {
        use super::*;
        use crate::platform::Platform;
        use std::{future::{Future, poll_fn}, task::Poll, time::Duration};
        let root = crate::library::generated_test_directory("joined-backlight");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("max_brightness"), "100").unwrap();
        let (done, ticket) = WriteTicket::fixture();
        let mut platform = DrmTest {
            // Any accidental additional Enable reaches this absent display
            // and fails, rather than silently treating a repeat as harmless.
            display: None, brightness: 40,
            backlight: Backlight::new(root.clone()),
            control_phase: ControlPhase::Backlight(ticket), desired_screen: None,
            brightness_dirty: false, on: true, alsa: false, mainline: false,
            resume_return: None, resume_display: None, resume_battery: None, sleep: SleepPhase::Idle, shutdown: Shutdown::default(),
            input: None, input_sink: Arc::new(|_| {}), wheel_step: Arc::new(AtomicI32::new(20)),
            clicker: None, headphones: None, touchscreen: Default::default(),
        };
        crate::rt::Ui::new().unwrap().block_on(async {
            for on in std::iter::once(false).chain(std::iter::repeat_n(true, 32)) {
                let observer = platform.set_screen_async(on);
                tokio::pin!(observer);
                poll_fn(|cx| { assert!(observer.as_mut().poll(cx).is_pending()); Poll::Ready(()) }).await;
            }
            assert!(platform.desired_screen.is_none(), "latest target matches the accepted operation");
            done.send_replace(true);
            tokio::time::timeout(Duration::from_secs(1), platform.set_screen_async(true)).await.unwrap();
            assert!(matches!(platform.control_phase, ControlPhase::Idle));
            assert!(platform.desired_screen.is_none());
        });
        drop(platform);
        assert_eq!(std::fs::read_to_string(root.join("brightness")).unwrap(), "0");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn active_scanout_is_retained_only_after_confirmed_dark_pwm() {
        use super::*;
        for actual in ["0", "40"] {
            let root = crate::library::generated_test_directory("system-drm-pm");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("max_brightness"), "100").unwrap();
            std::fs::write(root.join("brightness"), "40").unwrap();
            std::fs::write(root.join("actual_brightness"), actual).unwrap();
            let (mut display, _) = super::super::display::tests::dark_resume_fixture();
            display.enable(true).unwrap();
            let mut platform = DrmTest {
                display: Some(display), brightness: 40, backlight: Backlight::new(root.clone()),
                control_phase: ControlPhase::Idle, desired_screen: None, brightness_dirty: false,
                on: true, alsa: false, mainline: false, resume_return: None,
                resume_display: None, resume_battery: None, sleep: SleepPhase::Idle,
                shutdown: Shutdown::default(), input: None, touchscreen: Default::default(), input_sink: Arc::new(|_| {}),
                wheel_step: Arc::new(AtomicI32::new(20)), clicker: None, headphones: None,
            };
            crate::rt::Ui::new().unwrap().block_on(async {
                assert_eq!(platform.retain_dark_scanout().await, actual == "0");
                assert_eq!(std::fs::read_to_string(root.join("brightness")).unwrap(), "0");
                assert!(platform.display.as_ref().unwrap().enabled());
                assert_eq!(platform.on, actual != "0");
                platform.set_screen_async(false).await;
                assert!(!platform.display.as_ref().unwrap().enabled());
                assert!(!platform.retain_dark_scanout().await);
            });
            drop(platform);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn resumed_display_stays_dark_through_modeset_and_until_a_valid_fresh_flip() {
        use super::*;
        use std::io::Write;
        for completion in [1, 2] {
            let root = crate::library::generated_test_directory("resume-backlight");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join("max_brightness"), "100").unwrap();
            std::fs::write(root.join("brightness"), "0").unwrap();
            let (display, mut flip) = super::super::display::tests::dark_resume_fixture();
            let mut platform = DrmTest {
                display: Some(display), brightness: 40, backlight: Backlight::new(root.clone()),
                control_phase: ControlPhase::Idle, desired_screen: None, brightness_dirty: false,
                on: false, alsa: false, mainline: false, resume_return: None,
                resume_display: None, resume_battery: None, sleep: SleepPhase::Idle,
                shutdown: Shutdown::default(), input: None, touchscreen: Default::default(), input_sink: Arc::new(|_| {}),
                wheel_step: Arc::new(AtomicI32::new(20)), clicker: None, headphones: None,
            };
            let brightness = || std::fs::read_to_string(root.join("brightness")).unwrap();
            let frame = Canvas::new(375, 320);
            crate::rt::Ui::new().unwrap().block_on(async {
                platform.resume_display = Some(Ok(()));
                platform.display.as_mut().unwrap().start_mode(true).unwrap();
                platform.set_screen_async(true).await;
                platform.set_brightness_async(75).await;
                assert_eq!(brightness(), "0", "enabled scanout must still be dark");
                let present = platform.present(&frame);
                tokio::pin!(present);
                tokio::select! {
                    biased;
                    _ = &mut present => panic!("accepted scanout flip has not completed"),
                    _ = tokio::task::yield_now() => {},
                }
                assert_eq!(brightness(), "0", "an incomplete flip must stay dark");
                flip.write_all(&[completion]).unwrap();
                assert_eq!(present.await.is_ok(), completion == 1);
                assert_eq!(brightness(), if completion == 1 { "75" } else { "0" });
            });
            drop(platform);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn suspend_recovers_input_after_helper_refusal_and_cancelled_reopen() {
        use super::*;
        use crate::platform::evdev::{Input, tests::{Fixture, MOCK_TEST, HOOKS, mock_api, release_hooks}};
        use std::sync::atomic::AtomicI32;
        let _exclusive = MOCK_TEST.lock().unwrap();
        let _fixture = Fixture::new();
        let ui = crate::rt::Ui::new().unwrap();
        let mut phase = SleepPhase::Idle;
        let mut returned = None;
        let sink: InputSink = Arc::new(|_| {});
        let step = Arc::new(AtomicI32::new(20));
        let mut input = Some(Input::open_api(mock_api(), sink.clone(), step.clone()).unwrap());
        HOOKS.lock().unwrap().stall_open = true;
        let mut started = false;
        ui.block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                tokio::select! {
                    result = suspend_input(&mut phase, &mut input, sink.clone(), step.clone(), &mut returned, "/bin/false", Some(mock_api()), false, || started = true) => panic!("stalled recovery completed: {result:?}"),
                    _ = async {
                        while HOOKS.lock().unwrap().entered == 0 { tokio::time::sleep(std::time::Duration::from_millis(2)).await; }
                    } => {}
                }
            }).await.unwrap();
        });
        assert!(started, "helper refusal must start display/battery recovery before input reopens");
        assert!(matches!(phase, SleepPhase::Reopening(Err(_))));
        assert!(input.is_some() && !input.as_ref().unwrap().is_ready());
        let boundary = returned.unwrap();
        release_hooks();
        let error = ui.block_on(suspend_input(&mut phase, &mut input, sink.clone(), step.clone(), &mut returned,
            "/bin/true", Some(mock_api()), false, || panic!("resume work was started twice"))).unwrap_err();
        assert_eq!(error, "Sleep failed; see system status", "must retain original refusal rather than run the new helper");
        assert_eq!(returned, Some(boundary));
        assert!(matches!(phase, SleepPhase::Idle));
        assert!(input.as_ref().unwrap().is_ready(), "helper refusal still restores usable input");
        ui.block_on(input.as_mut().unwrap().close());
    }

    #[test]
    fn kernel_marker_can_prepare_a_dark_panel_while_helper_still_owns_failure() {
        use std::{process::Stdio, time::Duration};
        let mut helper = super::SleepHelper::start_with(|blocking| {
            let mut child = tokio::process::Command::new("/bin/sh");
            child.args(["-c", "printf 'Q2_KERNEL_RETURN\\n'; sleep 0.15; exit 1"]).stdout(Stdio::piped());
            super::super::process::spawn_reactor_child(blocking, child)
        });
        crate::rt::Ui::new().unwrap().block_on(async {
            helper.kernel_return.as_mut().unwrap().await.unwrap();
            assert!(tokio::time::timeout(Duration::from_millis(5), helper.wait()).await.is_err());
            assert!(helper.wait().await.0.is_err());
        });
    }

    #[test]
    fn sleep_helper_cancelled_wait_retains_result_and_return_boundary() {
        let (send, result) = tokio::sync::oneshot::channel();
        let mut helper = super::SleepHelper { result: Some(result), kernel_return: None };
        let ui = crate::rt::Ui::new().unwrap();
        ui.block_on(async {
            assert!(tokio::time::timeout(std::time::Duration::from_millis(10), helper.wait()).await.is_err());
        });
        let returned = std::time::Instant::now();
        send.send((Err("generated refusal".into()), returned)).unwrap();
        let (status, observed) = ui.block_on(helper.wait());
        assert_eq!(status, Err("generated refusal".into()));
        assert_eq!(observed, returned);
        assert!(helper.result.is_none());
    }

    #[test]
    fn held_sleep_spawn_keeps_io_live_and_cancelled_observer_still_joins_child() {
        use std::{sync::mpsc, time::Duration};
        let (entered, observed) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let (created, pid) = mpsc::channel();
        let mut helper = super::SleepHelper::start_with(move |blocking| {
            assert!(crate::rt::on_pool() && !crate::rt::on_executor());
            let _ = entered.send(());
            wait.recv_timeout(Duration::from_secs(5)).unwrap();
            let child = crate::platform::process::spawn_reactor_child(blocking, tokio::process::Command::new("/bin/true"))?;
            let _ = created.send(child.id().unwrap() as i32);
            Ok(child)
        });
        let reached = observed.recv_timeout(Duration::from_secs(3));
        let ui = crate::rt::Ui::new().unwrap();
        let (cancelled, live) = ui.block_on(async {
            let cancelled = tokio::time::timeout(Duration::from_millis(10), helper.wait()).await.is_err();
            let live = tokio::time::timeout(Duration::from_secs(1), crate::rt::io().spawn(async { 17 })).await;
            (cancelled, live)
        });
        let (joined, done) = mpsc::channel();
        let dropper = std::thread::spawn(move || { drop(helper); let _ = joined.send(()); });
        let _ = release.send(());
        let retired = done.recv_timeout(Duration::from_secs(3));
        dropper.join().unwrap();
        reached.unwrap(); retired.unwrap();
        assert!(cancelled);
        assert_eq!(live.unwrap().unwrap(), 17);
        let pid = pid.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(crate::platform::process::generated::wait_nonblocking(pid).unwrap_err().raw_os_error(), Some(libc::ECHILD),
            "ordinary helper Drop joins the exact child even after a cancelled wait");
    }

    #[test]
    fn sleep_helper_success_failure_and_missing_executable_are_observed() {
        let ui = crate::rt::Ui::new().unwrap();
        for (program, success) in [("/bin/true", true), ("/bin/false", false), ("/generated-missing-q2-sleep-helper", false)] {
            let before = std::time::Instant::now();
            let mut helper = super::SleepHelper::start(program, false);
            let (status, returned) = ui.block_on(helper.wait());
            assert_eq!(status.is_ok(), success);
            assert!(returned >= before && returned <= std::time::Instant::now());
        }
    }

    #[test]
    fn stalled_shutdown_launch_keeps_executor_live_and_refuses_duplicates() {
        use std::time::Duration;
        let ui = crate::rt::Ui::new().unwrap();
        let mut shutdown = super::Shutdown::default();
        let (entered, arrived) = tokio::sync::oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        ui.block_on(async {
            shutdown.start_with(move |_| {
                let _ = entered.send(());
                gate.recv_timeout(Duration::from_secs(5)).unwrap();
                Err("Generated preparation refused".into())
            }).unwrap();
            tokio::time::timeout(Duration::from_secs(5), arrived).await.unwrap().unwrap();
            assert!(shutdown.error().is_none());
            assert!(shutdown.start_with(|_| panic!("duplicate launch")).is_err());
            // Poll and cancel the observer while the owned launch is held.
            tokio::select! {
                biased;
                _ = shutdown.started() => panic!("held launch acknowledged"),
                _ = tokio::task::yield_now() => {},
            }
            assert!(shutdown.launch.is_some());
            // A launch waiting on external work must not occupy the reactor.
            assert_eq!(crate::rt::io().spawn(async { 42 }).await.unwrap(), 42);
            release.send(()).unwrap();
            let error = tokio::time::timeout(Duration::from_secs(5), shutdown.started()).await.unwrap().unwrap_err();
            assert_eq!(error, "Generated preparation refused");
            assert!(shutdown.launch.is_none() && shutdown.child.is_none());
            assert!(shutdown.start_with(|_| Err("Generated retry refused".into())).is_ok());
            tokio::time::timeout(Duration::from_secs(5), async {
                while shutdown.error().is_none() { tokio::task::yield_now().await; }
            }).await.unwrap();
        });
    }

    #[test]
    fn shutdown_launcher_loss_recovers_and_owner_drop_does_not_cancel_launch() {
        use std::time::Duration;
        let (send, receive) = tokio::sync::oneshot::channel();
        let mut shutdown = super::Shutdown { launch: Some(receive), child: None };
        drop(send);
        assert_eq!(shutdown.error().as_deref(), Some("Shutdown launcher stopped"));
        assert!(shutdown.launch.is_none());
        let (entered, arrived) = tokio::sync::oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let (finished, complete) = tokio::sync::oneshot::channel();
        crate::rt::Ui::new().unwrap().block_on(async {
            shutdown.start_with(move |_| {
                let _ = entered.send(());
                gate.recv_timeout(Duration::from_secs(5)).unwrap();
                let _ = finished.send(());
                Err("Generated helper only".into())
            }).unwrap();
            tokio::time::timeout(Duration::from_secs(5), arrived).await.unwrap().unwrap();
            drop(shutdown);
            release.send(()).unwrap();
            tokio::time::timeout(Duration::from_secs(5), complete).await.unwrap().unwrap();
        });
    }

    #[test]
    fn shutdown_helper_exit_and_signal_are_failures_while_player_is_alive() {
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};
        for killed in [false, true] {
            let mut child = Command::new("sh").args(["-c", "read line"])
                .stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
            let stdin = child.stdin.take();
            let mut child = Some(child);
            assert!(super::shutdown_error(&mut child).is_none());
            if killed { child.as_mut().unwrap().kill().unwrap(); }
            else { drop(stdin); }
            let deadline = Instant::now() + Duration::from_secs(2);
            while super::shutdown_error(&mut child).is_none() {
                assert!(Instant::now() < deadline);
                std::thread::yield_now();
            }
            assert!(child.is_none());
            assert!(super::shutdown_error(&mut child).is_none());
        }
        let mut child = Command::new("sh").args(["-c", "exit 0"]).spawn().unwrap();
        assert!(child.wait().unwrap().success());
        assert!(super::shutdown_error(&mut Some(child)).is_some(), "even exit zero is not power-off while our player is alive");
    }

    #[test]
    fn brightness_mapping_keeps_off_zero_and_supports_old_and_fine_tables() {
        for maximum in [0, 9, 100, 255] {
            assert_eq!(backlight_value(0, maximum), 0);
            assert_eq!(backlight_value(100, maximum), maximum);
            assert_eq!(backlight_value(255, maximum), maximum);
            let values: Vec<_> = (1..=100).map(|p| backlight_value(p, maximum)).collect();
            assert!(values.windows(2).all(|v| v[0] <= v[1]));
            if maximum > 0 { assert!(values[0] > 0); }
            let unique = values.iter().collect::<std::collections::BTreeSet<_>>().len();
            assert_eq!(unique, maximum.clamp(1, 100) as usize);
        }
    }
}

impl Drop for DrmTest {
    fn drop(&mut self) {
        self.sleep = SleepPhase::Idle;
        self.clicker.take();
        self.input.take();
        self.backlight(0);
        drop(self.display.take());
    }
}

impl Platform for DrmTest {
    fn click(&mut self) {
        if let Some(clicker) = &self.clicker { clicker.click(); }
    }
    fn click_tone(&mut self, hz: u16) {
        if let Some(clicker) = &self.clicker { clicker.tone(hz); }
    }
    fn present<'a>(&'a mut self, frame: &'a Canvas) -> super::PresentFuture<'a> {
        Box::pin(async move {
            self.settle_controls().await;
            let display = self.display.as_mut().ok_or("KMS owner unavailable")?;
            if let Some(result) = &self.resume_display {
                result.clone()?;
                display.enable_async(true).await.map_err(|error| error.to_string())?;
            }
            display.present(&frame.pixels).await.map_err(|error| error.to_string())?;
            if self.resume_display.take().is_some() {
                self.on = true;
                self.brightness_dirty = false;
                self.control_phase = ControlPhase::Backlight(self.backlight.write(self.brightness));
                self.settle_controls().await;
            }
            Ok(())
        })
    }
    fn set_screen(&mut self, on: bool) {
        assert!(crate::rt::may_wait_for_tasks(), "await screen controls on runtime/pool callers");
        if !on { self.backlight(0); }
        if let Err(error) = self.display.as_mut().unwrap().enable(on) {
            log::error!("KMS blanking failed: {error}");
            return;
        }
        self.on = on;
        if on { self.backlight(self.brightness); }
    }
    fn set_brightness(&mut self, level: u8) {
        assert!(crate::rt::may_wait_for_tasks(), "await brightness controls on runtime/pool callers");
        self.brightness = level.clamp(1, 100);
        if self.on { self.backlight(self.brightness); }
    }
    fn battery(&mut self) -> Battery {
        assert!(crate::rt::may_wait_for_tasks(), "await battery observations on runtime/pool callers");
        let (reply, completed) = tokio::sync::oneshot::channel();
        crate::rt::io().spawn(async move {
            let battery = super::controls::battery("/sys/class/power_supply".into()).await;
            let _ = reply.send(battery);
        });
        completed.blocking_recv().unwrap_or_default()
    }
    fn headphone_events(&self) -> bool {
        self.mainline && self.input.as_ref().is_some_and(super::evdev::Input::is_ready)
    }
    fn headphones(&mut self) -> Option<bool> {
        self.headphones.as_mut()?.get_ordinary()
    }
    fn set_screen_async(&mut self, on: bool) -> super::ControlFuture<'_, ()> {
        Box::pin(async move {
            if let Some(prepared) = &mut self.resume_display {
                if on {
                    // Join preparation while the PWM remains at zero. Only
                    // a successful fresh presentation may restore brightness.
                    if prepared.is_ok() {
                        *prepared = self.display.as_mut().unwrap().settle().await.map_err(|error| error.to_string());
                    }
                    return;
                }
                self.resume_display = None;
                self.desired_screen = Some(false);
                self.settle_controls().await;
                return;
            }
            // Join an identical accepted target. A new observer must not
            // enqueue another enable/backlight write behind the held one.
            let active = match self.control_phase {
                ControlPhase::BacklightOff(_) | ControlPhase::Disable => false,
                ControlPhase::Enable | ControlPhase::Backlight(_) => true,
                ControlPhase::Idle => self.on || (!on && self.display.as_ref().is_some_and(Display::enabled)),
            };
            self.desired_screen = (on != active).then_some(on);
            self.settle_controls().await;
        })
    }
    fn blank_for_suspend(&mut self) -> super::ControlFuture<'_, ()> {
        Box::pin(async move {
            let requested = self.mainline && self.on && crate::rt::jobs().run_isolated(|blocking| {
                system_drm_pm_requested(blocking, std::path::Path::new(SYSTEM_DRM_PM_MARKER))
            }).await.unwrap_or(false);
            if requested && self.retain_dark_scanout().await {
                crate::record_boot_stage("experimental-system-drm-pm-accepted");
                return;
            }
            self.set_screen_async(false).await;
        })
    }
    fn set_brightness_async(&mut self, level: u8) -> super::ControlFuture<'_, ()> {
        Box::pin(async move {
            self.brightness = level.clamp(1, 100);
            self.brightness_dirty = true;
            self.settle_controls().await;
        })
    }
    fn battery_async(&mut self) -> super::ControlFuture<'_, Battery> {
        Box::pin(super::controls::battery("/sys/class/power_supply".into()))
    }
    fn start_battery_refresh(&mut self) -> Option<super::ControlFuture<'static, Battery>> {
        if let Some(refresh) = self.resume_battery.take() {
            return Some(Box::pin(async move { refresh.await.unwrap_or_default() }));
        }
        Some(Box::pin(super::controls::battery("/sys/class/power_supply".into())))
    }
    fn headphones_async(&mut self) -> super::ControlFuture<'_, Option<bool>> {
        Box::pin(async move {
            match self.headphones.as_mut() { Some(owner) => owner.get().await, None => None }
        })
    }
    fn hardware(&self) -> super::Hardware {
        if !self.mainline { return super::Hardware::default(); }
        super::Hardware {
            clicker: self.clicker.is_some(),
            headphone_switch: self.headphones.is_some(),
            touchscreen: self.touchscreen.available(),
        }
    }
    fn set_touchscreen(&mut self, enabled: bool) -> super::ControlFuture<'_, Result<(), String>> {
        Box::pin(async move {
            if enabled {
                self.touchscreen.open(self.input_sink.clone()).await
            } else {
                self.touchscreen.close().await;
                Ok(())
            }
        })
    }
    fn audio_hw(&mut self) -> Box<dyn AudioHw> {
        if self.alsa && self.mainline {
            return Box::new(crate::audio::alsa::MainlineHw("hw:Q2,0".into()));
        }
        Box::new(SoftHw(if self.alsa { "hw:Q2,0" } else { FAKE }.into()))
    }
    fn suspend(&mut self) -> super::SuspendFuture<'_> {
        Box::pin(async move {
            if !self.mainline { return Err("Suspend unavailable in DRM test mode".into()); }
            self.touchscreen.close().await;
            let retain_drm = !self.on && self.display.as_ref().is_some_and(Display::enabled);
            suspend_input(&mut self.sleep, &mut self.input, self.input_sink.clone(), self.wheel_step.clone(),
                &mut self.resume_return, "/sbin/q2-system-sleep", None, retain_drm, || {
                    // A completed helper can no longer enter suspend. Start the
                    // dark modeset and a fresh observation before reopening input.
                    start_resume(&mut self.display, &mut self.resume_display, &mut self.resume_battery);
                }).await
        })
    }
    fn power_off(&mut self) -> super::SuspendFuture<'_> {
        Box::pin(async move {
            if !self.mainline { return Err("Power off unavailable in DRM test mode".into()); }
            self.touchscreen.close().await;
            // A cancelled observer resumes the same launch. A running helper
            // still rejects a distinct request rather than spawning twice.
            if self.shutdown.launch.is_none() { self.shutdown.start()?; }
            self.shutdown.started().await
        })
    }
    fn take_resume_return(&mut self) -> Option<std::time::Instant> { self.resume_return.take() }
    fn power_off_error(&mut self) -> Option<String> { self.shutdown.error() }
    fn controls_ready(&self) -> bool { !self.mainline || (matches!(self.sleep, SleepPhase::Idle) && self.input.as_ref().is_some_and(super::evdev::Input::is_ready)) }
    fn is_device(&self) -> bool { self.mainline }
}
