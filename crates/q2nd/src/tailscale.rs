#![forbid(unsafe_code)]
//! Card-local Tailscale, supervised off the UI thread. Userspace networking
//! needs neither a TUN driver nor changes to stock DNS/routing.
use std::io::{self, Read, Write};
use std::os::unix::fs::FileTypeExt;
use crate::metrics::{Event, ProbePhase};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
mod spawn;
use spawn::Child;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use std::sync::{Arc, Mutex};
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use std::future::Future;

pub const PROXY: &str = "http://127.0.0.1:1055";
const SOCKET: &str = "/tmp/q2nd-tailscale.sock";
const MAINLINE_CACHE: &str = "/run/q2/tailscale-cache";
/// Timer slack of the daemon's threads, in ns; 0 reverts to the kernel's
/// 50 µs. Go's sysmon sleeps in 20 µs steps when busy. With 1 ms on the
/// Q2 (player paused): proxy 3.83–4.03 MiB/s against 3.37–3.68,
/// 262–271 ms CPU per MiB against 312–330, idle CPU 39 ms/s against 51–54.
const DAEMON_TIMER_SLACK_NS: u64 = 1_000_000;
/// The Go collector's soft memory target. Anonymous RSS 6.2–6.8 MB, peak
/// RSS 23.6 MB with 32 MiB. 16 MiB makes the collector work harder only in
/// bursts above it.
const GO_MEMORY_LIMIT: &str = "16MiB";
/// Hard cap of tailscaled's data segment (RLIMIT_DATA), so a runaway daemon
/// exits (and is restarted after 15 s) instead of pushing the Q2 into
/// thrash. V0.123 VmData is 48.3 MB: 33.8 MB of .bss, of which 32 MiB is
/// Go's FIPS entropy scratch buffer that is never touched, and 14.1 MB of
/// heap, stacks and data. 64 MiB is the .bss plus twice the rest.
const DAEMON_DATA_LIMIT: u64 = 64 << 20;

/// Anonymous provenance of the latest published observation, not peer reachability.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum Observation {
    #[default] Unknown = 0, Off = 1, Simulated = 2, Starting = 3, Running = 4,
    NeedsLogin = 5, NeedsMachineAuth = 6, Stopped = 7, OtherBackend = 8,
    SocketMissing = 9, Timeout = 10, InvalidStatus = 11, Error = 12,
    DaemonFailure = 13, SpawnFailure = 14, SupervisorRestart = 15,
}
impl Observation {
    /// Whether Tailscale can carry requests: running, known not to, or not
    /// known (a status read that timed out or failed says nothing).
    pub fn transport(self) -> Option<bool> {
        match self {
            Self::Running => Some(true),
            Self::Off | Self::Starting | Self::NeedsLogin | Self::NeedsMachineAuth | Self::Stopped | Self::OtherBackend
            | Self::SocketMissing | Self::DaemonFailure | Self::SpawnFailure | Self::SupervisorRestart => Some(false),
            Self::Unknown | Self::Simulated | Self::Timeout | Self::InvalidStatus | Self::Error => None,
        }
    }
}

fn error_observation(error: &io::Error) -> Observation {
    if error.get_ref().is_some_and(|cause| cause.is::<serde_json::Error>()) {
        Observation::InvalidStatus
    } else { match error.kind() {
        io::ErrorKind::NotFound => Observation::SocketMissing,
        io::ErrorKind::TimedOut => Observation::Timeout,
        _ => Observation::Error,
    } }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct State {
    pub observation: Observation,
    pub status: String,
    pub address: String,
    pub auth_url: String,
    pub health: String,
}

fn parse_status(bytes: &[u8]) -> Result<State, serde_json::Error> {
    let v: serde_json::Value = serde_json::from_slice(bytes)?;
    let backend = v["BackendState"].as_str().unwrap_or("Unknown");
    Ok(State {
        observation: match backend {
            "Running" => Observation::Running, "NeedsLogin" => Observation::NeedsLogin,
            "NeedsMachineAuth" => Observation::NeedsMachineAuth, "Starting" => Observation::Starting,
            "Stopped" => Observation::Stopped, _ => Observation::OtherBackend,
        },
        status: match backend {
            "Running" => "Connected",
            "NeedsLogin" => "Sign In Required",
            "NeedsMachineAuth" => "Approve in Admin Console",
            "Starting" => "Connecting…",
            "Stopped" => "Stopped",
            other => other,
        }.into(),
        address: v["TailscaleIPs"].as_array().and_then(|a| a.first()).and_then(|s| s.as_str()).unwrap_or("").into(),
        auth_url: v["AuthURL"].as_str().unwrap_or("").into(),
        health: v["Health"].as_array().map(|a| a.iter().filter_map(|s| s.as_str()).collect::<Vec<_>>().join("; ")).unwrap_or_default(),
    })
}

enum Request { Enable(bool), Login, Logout, Stop, #[cfg(test)] Panic }

// Poll promptly only while the newly spawned daemon creates its local socket.
// Network/authentication retries retain the existing bounded cadence. These
// deadlines do not assert connectivity: only LocalAPI supplies backend state.
const SOCKET_START_WINDOW: Duration = Duration::from_secs(5);
// On Q2 the socket can appear after the initial fast window. A missing local
// socket is not a failed network request: keep checking it promptly for a
// bounded startup period, without accelerating other errors or later retries.
const SOCKET_MISSING_WINDOW: Duration = Duration::from_secs(15);
const SOCKET_START_POLL: Duration = Duration::from_millis(100);
const UP_STATUS_POLL: Duration = Duration::from_millis(500);
const UP_STATUS_TIMEOUT: Duration = Duration::from_secs(1);
const POST_UP_WINDOW: Duration = Duration::from_secs(3);
// The owner, V0.126: approval on the phone must show on the Q2 without a
// toggle. From Sign In until connected, at most SIGN_IN_WINDOW, the status is
// read every SIGN_IN_POLL (otherwise every 3 s while not connected).
const SIGN_IN_WINDOW: Duration = Duration::from_secs(300);
const SIGN_IN_POLL: Duration = Duration::from_secs(1);
/// Whether a sign-in started at most SIGN_IN_WINDOW ago is still waiting.
fn signing_in(until: Option<Instant>, connected: bool, now: Instant) -> bool {
    !connected && until.is_some_and(|end| now < end)
}
fn waiting_for_socket(socket_age: Option<Duration>, socket_missing: bool) -> bool {
    socket_age.is_some_and(|age| age < SOCKET_START_WINDOW
        || (socket_missing && age < SOCKET_MISSING_WINDOW))
}
fn poll_interval(enabled: bool, connected: bool, initial: bool, socket_age: Option<Duration>, socket_missing: bool) -> Duration {
    if !enabled { Duration::from_secs(3600) }
    else if initial { Duration::ZERO }
    else if waiting_for_socket(socket_age, socket_missing) { SOCKET_START_POLL }
    else if connected { Duration::from_secs(30) }
    else { Duration::from_secs(3) }
}

// Diagnostic append budget is per worker lifetime, including daemon restarts.
// It adds no requests and never resets the polling/deadline policy. An explicit
// limit record distinguishes exhausted instrumentation from absent attempts.
const PROBE_ATTEMPTS: u64 = 64;
const PROBE_WINDOW: Duration = Duration::from_secs(60);
#[derive(Default)]
struct ProbeBudget { started: Option<Instant>, attempts: u64, closed: bool }
impl ProbeBudget {
    fn start(&mut self) { self.started.get_or_insert_with(Instant::now); }
    fn next_at(&mut self, now: Instant) -> (Option<u64>, bool) {
        let Some(start) = self.started else { return (None, false) };
        if self.closed { return (None, false); }
        if self.attempts >= PROBE_ATTEMPTS || now.saturating_duration_since(start) >= PROBE_WINDOW {
            self.closed = true;
            return (None, true);
        }
        self.attempts += 1;
        (Some(self.attempts), false)
    }
}
fn probe_result<T>(result: &io::Result<T>) -> ProbePhase {
    match result {
        Ok(_) => ProbePhase::Success,
        Err(error) => match error.kind() {
            io::ErrorKind::TimedOut => ProbePhase::Timeout,
            io::ErrorKind::NotFound => ProbePhase::NotFound,
            _ => ProbePhase::Error,
        }
    }
}

pub struct Tailscale {
    worker: Arc<Owner>,
}

struct Owner {
    inner: Mutex<Worker>,
    wake: tokio::sync::Notify,
    notify: Arc<dyn Fn() + Send + Sync>,
}

/// The Tailscale daemon an image ships: tailscale-rs (`tsrsd`, the default
/// image) or the Go tailscaled and CLI (an OS build option). Each keeps its
/// own node identity: tailscale-rs in `tailscale-rs/keys.json`, Go in
/// `tailscale/tailscaled.state`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Daemon { Rs, Go }
type StartOwner = Box<dyn Fn(Arc<Owner>, Option<tokio::task::JoinHandle<()>>)
    -> (tokio::task::JoinHandle<()>,Arc<Completion>) + Send>;

// Requests, published status and the final disabled retirement decision share
// one lock. A request belongs to the live owner or creates its successor.
struct Worker {
    state: State,
    start: Option<StartOwner>,
    thread: Option<tokio::task::JoinHandle<()>>,
    completion: Option<Arc<Completion>>,
    requests: VecDeque<Request>,
    running: bool,
}
impl Worker {
    fn start(&mut self, owner: Arc<Owner>) {
        if self.running { return; }
        if let Some(start) = &self.start {
            let previous = self.thread.take();
            self.running = true;
            // Join off UI before any successor process work. One handle owns
            // completion transitively, without a growing list of old owners.
            let (task,completion)=start(owner,previous);
            self.thread=Some(task);self.completion=Some(completion);
        }
    }
}
#[derive(Default)]
struct Completion {done:std::sync::atomic::AtomicBool,wake:tokio::sync::Notify}
impl Completion {
    async fn wait(&self) {
        loop {
            let notified=self.wake.notified();
            tokio::pin!(notified);notified.as_mut().enable();
            if self.done.load(std::sync::atomic::Ordering::Acquire){return;}
            notified.await;
        }
    }
}
struct Complete(Arc<Completion>);
impl Drop for Complete {
    fn drop(&mut self) {self.0.done.store(true,std::sync::atomic::Ordering::Release);self.0.wake.notify_waiters();}
}
impl Owner {
    async fn next(&self, timeout: Duration) -> Option<Request> {
        let until=tokio::time::Instant::now()+timeout;
        loop {
            let notified=self.wake.notified();
            if let Some(request)=self.inner.lock().unwrap().requests.pop_front() {return Some(request);}
            if tokio::time::timeout_at(until,notified).await.is_err() {return None;}
        }
    }
    async fn pending(&self) {
        loop {
            let notified=self.wake.notified();
            if !self.inner.lock().unwrap().requests.is_empty() {return;}
            notified.await;
        }
    }

    fn publish(&self, state: State) {
        let changed = {
            let mut owner = self.inner.lock().unwrap();
            // Provenance-only changes do not add UI notifications or alter redraw cadence.
            let changed = owner.state.status != state.status || owner.state.address != state.address
                || owner.state.auth_url != state.auth_url || owner.state.health != state.health;
            owner.state = state;
            changed
        };
        // Publish first and call outside the owner lock: consumers may read it.
        if changed { (self.notify)(); }
    }

    // Called only AFTER both children are joined. Off and retirement are one
    // atomic publication; a queued re-enable stays with this live owner.
    fn disabled(&self) -> bool {
        let mut owner = self.inner.lock().unwrap();
        let state = State {status: "Off".into(), observation: Observation::Off, ..State::default()};
        let changed = owner.state != state;
        owner.state = state;
        let retired = owner.requests.is_empty();
        if retired { owner.running = false; }
        drop(owner);
        if changed { (self.notify)(); }
        retired
    }
}

enum Wakeup {
    Request(Option<Request>),
    Cli(Option<io::Result<std::process::ExitStatus>>),
    Daemon(io::Result<std::process::ExitStatus>),
}

async fn wait_cli(up: &mut Option<CliJob>) -> Option<io::Result<std::process::ExitStatus>> {
    let Some(job) = up.as_mut() else { return std::future::pending().await };
    let remaining = job.remaining();
    let Some(child) = job.child.as_mut() else { return std::future::pending().await };
    // Child::wait is cancellation safe. Do not take or kill the child here:
    // a simultaneously queued command must leave it available for joined cleanup.
    tokio::time::timeout(remaining, child.wait()).await.ok()
}

async fn wait_daemon(daemon: &mut Option<Child>) -> io::Result<std::process::ExitStatus> {
    match daemon.as_mut() {
        Some(child) => child.wait().await,
        None => std::future::pending().await,
    }
}

async fn wait_for_work(owner: &Owner, interval: Duration, up: &mut Option<CliJob>, daemon: &mut Option<Child>) -> Wakeup {
    tokio::select! {
        biased;
        request = owner.next(interval) => Wakeup::Request(request),
        status = wait_cli(up) => Wakeup::Cli(status),
        status = wait_daemon(daemon) => Wakeup::Daemon(status),
    }
}

impl Tailscale {
    #[cfg(test)]
    pub fn new(app: PathBuf, data: PathBuf, simulated: bool, enabled: bool) -> Self {
        Self::new_notifying(app, data, simulated, enabled, Arc::new(|| {}))
    }
    /// Supervises the daemon the image ships in `app` ([`resolve`]).
    pub fn new_notifying(app: PathBuf, data: PathBuf, simulated: bool, enabled: bool, notify: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self::at_socket_notifying(app, data, simulated, enabled, PathBuf::from(SOCKET), notify)
    }
    #[cfg(test)]
    fn at_socket(app: PathBuf, data: PathBuf, simulated: bool, enabled: bool, socket: PathBuf) -> Self {
        Self::at_socket_notifying(app, data, simulated, enabled, socket, Arc::new(|| {}))
    }
    fn at_socket_notifying(app: PathBuf, data: PathBuf, simulated: bool, enabled: bool, socket: PathBuf, notify: Arc<dyn Fn() + Send + Sync>) -> Self {
        let start: StartOwner = Box::new(move |owner, previous| {
            let (app, data, socket) = (app.clone(), data.clone(), socket.clone());
            let completion=Arc::new(Completion::default());let complete=Complete(completion.clone());
            let task=crate::rt::io().spawn(async move {
            let _complete=complete;
            if let Some(previous) = previous { let _ = previous.await; }
            let mut daemon: Option<Child> = None;
            let mut spawn_owner: Option<spawn::Owner> = None;
            let mut up: Option<CliJob> = None;
            let mut fast_until: Option<Instant> = None;
            let mut enabled = false;
            let mut login = false;
            let mut attempted = Instant::now() - Duration::from_secs(60);
            let mut retry_after = Duration::from_secs(3);
            let mut last_observation = Instant::now();
            let mut initial = true;
            let mut socket_started: Option<Instant> = None;
            let mut socket_missing = false;
            let mut local_ready = false;
            let mut backend_running = false;
            let mut probes = ProbeBudget::default();
            // This worker is the only status publisher. Keep its polling state
            // locally instead of reacquiring the UI-facing snapshot mutex.
            let mut connected = false;
            // Forget-this-device, the running daemon's kind and its CLI, and
            // recent daemon exits (a first tailscale-rs restart is prompt).
            let mut logout = false;
            let mut running: Option<Daemon> = None;
            let mut cli = app.join("tailscale");
            let mut exits: VecDeque<Instant> = VecDeque::new();
            let mut sign_in_until: Option<Instant> = None;
            loop {
            let outcome={
            let mut work=Box::pin(async {
            loop {
                // Process queued toggles before touching processes.
                let socket_age = socket_started.map(|start| start.elapsed());
                let mut interval = poll_interval(enabled, connected, initial, socket_age, socket_missing);
                if enabled && up.is_some() { interval = interval.min(UP_STATUS_POLL); }
                else if enabled && !connected && fast_until.is_some_and(|end| Instant::now() < end) {
                    interval = interval.min(UP_STATUS_POLL);
                }
                if enabled && signing_in(sign_in_until, connected, Instant::now()) { interval = interval.min(SIGN_IN_POLL); }
                initial = false;
                let mut daemon_exit = None;
                let request = match wait_for_work(&owner, interval, &mut up, &mut daemon).await {
                    Wakeup::Request(request) => request,
                    Wakeup::Cli(status) => {
                        if let (Some(job), Some(status)) = (up.as_mut(), status) { job.observed(status); }
                        None
                    }
                    Wakeup::Daemon(status) => { daemon_exit = Some(status); None }
                };
                match request {
                    Some(Request::Stop) => break,
                    #[cfg(test)] Some(Request::Panic)=>panic!("generated supervisor panic"),
                    Some(Request::Enable(on)) => {
                        if enabled != on { login = on; }
                        enabled = on;
                        attempted = Instant::now() - Duration::from_secs(60);
                        retry_after = Duration::from_secs(3);
                    }
                    Some(Request::Login) => {
                        login = true; attempted = Instant::now() - Duration::from_secs(60); retry_after = Duration::from_secs(3);
                        sign_in_until = Some(Instant::now() + SIGN_IN_WINDOW);
                    }
                    Some(Request::Logout) => { logout = true; attempted = Instant::now() - Duration::from_secs(60); retry_after = Duration::from_secs(3); }
                    None => {}
                }
                if !enabled {
                    connected = false;
                    cancel_up_async(&mut up).await;
                    fast_until = None;
                    stop_child_async(&mut daemon).await;
                    stop_spawn_owner(&mut spawn_owner).await;
                    socket_started = None;
                    socket_missing = false;
                    running = None;
                    logout = false;
                    if owner.disabled() { break; }
                    continue;
                }
                if simulated {
                    connected = false;
                    owner.publish(State { observation: Observation::Simulated, status: "Available on Q2".into(), ..State::default() });
                    continue;
                }
                // The up command owns its child but never owns this worker's
                // status/disable channel. Reap promptly, then refresh immediately.
                if let Some(job)=up.as_mut() {job.expire().await;}
                if up.as_mut().is_some_and(|job| job.poll_now().is_some()) {
                    if let Some(job) = up.as_ref() {
                        if let Some(completion) = job.completion { stage(job.purpose.stage(completion)).await; }
                    }
                    let returned = up.as_ref().map_or("tailscale-up-returned", |job| job.purpose.returned());
                    up = None;
                    stage(returned).await;
                    fast_until = Some(Instant::now() + POST_UP_WINDOW);
                    attempted = Instant::now() - Duration::from_secs(60);
                }
                if let Some(child) = daemon.as_mut() {
                    let failure = match daemon_exit.map(|status| status.map(Some)).unwrap_or_else(|| child.try_wait()) {
                        Ok(Some(exit)) => Some(match (exit.code(), exit.signal()) {
                            (Some(code), _) => format!("Tailscale exited with code {code}. Check device networking and storage."),
                            (_, Some(signal)) => format!("Tailscale stopped with signal {signal}. Check device resources."),
                            _ => "Tailscale stopped. Check device networking and storage.".into(),
                        }),
                        Err(_) => Some("Could not check the Tailscale process.".into()),
                        Ok(None) => None,
                    };
                    if let Some(health) = failure {
                        connected = false;
                        cancel_up_async(&mut up).await;
                        fast_until = None;
                        stop_child_async(&mut daemon).await;
                        socket_started = None;
                        socket_missing = false;
                        let rs = running.take() == Some(Daemon::Rs);
                        owner.publish(State { observation: Observation::DaemonFailure, status: "Could Not Start".into(), health, ..State::default() });
                        login = true;
                        attempted = Instant::now();
                        exits.retain(|at| at.elapsed() < DAEMON_EXIT_WINDOW);
                        exits.push_back(Instant::now());
                        retry_after = restart_delay(rs, exits.len());
                        continue;
                    }
                }
                let waiting_socket = waiting_for_socket(socket_started.map(|start| start.elapsed()), socket_missing);
                let fast_status = up.is_some() || fast_until.is_some_and(|end| Instant::now() < end);
                if fast_status { retry_after = UP_STATUS_POLL; }
                else if signing_in(sign_in_until, connected, Instant::now()) { retry_after = retry_after.min(SIGN_IN_POLL); }
                if !waiting_socket && attempted.elapsed() < retry_after { continue; }
                retry_after = Duration::from_secs(3);
                attempted = Instant::now();
                if daemon.is_none() {
                    connected = false;
                    match start_async(&mut spawn_owner, &app, &data, &socket, exits.len()).await {
                        Ok((child, resolved)) => {
                            daemon = Some(child);
                            running = Some(resolved.kind);
                            cli = resolved.cli.clone();
                            probes.start();
                            stage("tailscale-daemon-spawned").await;
                            local_ready = false;
                            backend_running = false;
                            socket_started = Some(Instant::now());
                            socket_missing = false;
                            owner.publish(State { observation: Observation::Starting, status: "Starting…".into(), ..State::default() });
                            profile(crate::process_profile::Actor::Daemon,daemon.as_ref().unwrap(),resolved.path).await;
                        }
                        Err(e) => owner.publish(State { observation: Observation::SpawnFailure, status: "Could Not Start".into(), health: e.to_string(), ..State::default() }),
                    }
                    continue;
                }
                // Wait for the local socket; Wi-Fi/control-plane availability is
                // handled by tailscaled itself. Never block input or playback.
                let timeout = if fast_status { UP_STATUS_TIMEOUT } else { Duration::from_secs(5) };
                let timeout = up.as_ref().map(|job| job.remaining().min(timeout)).unwrap_or(timeout);
                let (probe_id, limited) = probes.next_at(Instant::now());
                let mut trace=Vec::new();
                if limited {trace.push((Event::TailscaleProbe(ProbePhase::Limit),Duration::from_micros(probes.attempts),crate::playtrace::boottime_us()));}
                let mut mark=|phase| {
                    if let Some(id)=probe_id {trace.push((Event::TailscaleProbe(phase),Duration::from_micros(id),crate::playtrace::boottime_us()));}
                };
                let result=tokio::select! {
                    biased;
                    ()=owner.pending()=>None,
                    result=observed_status_async(&socket,timeout,if probe_id.is_some(){Some(&mut mark)}else{None})=>Some(result),
                };
                let observed=Instant::now();let observed_us=crate::playtrace::boottime_us();
                flush_trace(trace).await;
                let Some(result)=result else {continue;};
                match result {
                    Ok(st) => {
                        if st.status == "Connected" && !connected {
                            flush_trace(vec![(Event::TailscaleReady,observed.saturating_duration_since(last_observation),observed_us)]).await;
                        }
                        last_observation = observed;
                        socket_started = None;
                        socket_missing = false;
                        if !local_ready { stage("tailscale-localapi-ready").await; local_ready = true; }
                        if !backend_running && st.status == "Connected" {
                            stage("tailscale-backend-running-observed").await;
                            backend_running = true;
                        }
                        let running = st.status == "Connected";
                        if running { sign_in_until = None; }
                        connected = running;
                        owner.publish(st);
                        // Already Running from saved state: `up` would only
                        // re-apply the same preferences, loading a 35 MB CLI
                        // during startup (startup hunt idea 3b).
                        if login && running {
                            stage("tailscale-up-skipped-running").await;
                            login = false;
                        }
                        if login && up.is_none() {
                            // up times out while waiting for browser approval, but
                            // tailscaled keeps the login URL and accepts approval.
                            stage("tailscale-up-begin").await;
                            match CliJob::start_async(spawn_owner.as_ref().expect("daemon owns spawn parent"), &cli, &socket, Duration::from_secs(8)).await {
                                Ok(job) => {up=Some(job);profile(crate::process_profile::Actor::Up,up.as_ref().unwrap().child.as_ref().unwrap(),cli.clone()).await;},
                                Err(_) => {
                                    stage("tailscale-up-spawn-error").await;
                                    stage("tailscale-up-returned").await;
                                    fast_until = Some(Instant::now() + POST_UP_WINDOW);
                                }
                            }
                            login = false;
                        }
                        if logout && up.is_none() {
                            // Forget this device. A later Sign In asks for a new link.
                            stage("tailscale-logout-begin").await;
                            match CliJob::logout_async(spawn_owner.as_ref().expect("daemon owns spawn parent"), &cli, &socket, Duration::from_secs(8)).await {
                                Ok(job) => up = Some(job),
                                Err(_) => { stage("tailscale-logout-spawn-error").await; stage("tailscale-logout-returned").await; }
                            }
                            fast_until = Some(Instant::now() + POST_UP_WINDOW);
                            logout = false;
                        }
                    }
                    Err(error) => {
                        connected = false;
                        socket_missing = error.kind() == io::ErrorKind::NotFound;
                        owner.publish(State {
                            observation: error_observation(&error),
                            status: "Waiting for Tailscale".into(),
                            health: "Waiting for network or device time".into(),
                            ..State::default()
                        });
                    }
                }
            }
            });
            std::future::poll_fn(|cx| {
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(||work.as_mut().poll(cx))) {
                    Ok(std::task::Poll::Ready(()))=>std::task::Poll::Ready(true),
                    Ok(std::task::Poll::Pending)=>std::task::Poll::Pending,
                    Err(_)=>std::task::Poll::Ready(false),
                }
            }).await
            };
            cancel_up_async(&mut up).await;
            stop_child_async(&mut daemon).await;
            stop_spawn_owner(&mut spawn_owner).await;
            running = None;
            if outcome {break;}
            // Panic recovery joins children before retrying. Requests retain
            // priority, including Stop/disable queued during that cleanup.
            connected=false;socket_started=None;socket_missing=false;fast_until=None;
            login=enabled;attempted=Instant::now();retry_after=Duration::from_secs(15);
            owner.publish(State {observation: Observation::SupervisorRestart,status:"Could Not Start".into(),health:"Tailscale supervisor restarted".into(),..State::default()});
            }
        });(task,completion) });
        let worker = Arc::new(Owner {inner: Mutex::new(Worker {
            state: State {status: "Off".into(), observation: Observation::Off, ..State::default()},
            start: Some(start), thread: None, completion:None, requests: VecDeque::new(), running: false,
        }), wake: tokio::sync::Notify::new(), notify });
        let result = Self { worker };
        if enabled { result.set_enabled(true); }
        result
    }

    #[cfg(test)]
    pub(crate) fn publish_generated(&self, state: State) { self.worker.publish(state); }

    pub fn state(&self) -> State { self.worker.inner.lock().unwrap().state.clone() }
    fn request(&self, request: Request) {
        let mut worker = self.worker.inner.lock().unwrap();
        if worker.start.is_none() { return; }
        // Login and redundant disable do not activate a fully disabled owner.
        if !worker.running && !matches!(request, Request::Enable(true)) { return; }
        worker.requests.push_back(request);
        worker.start(self.worker.clone());
        self.worker.wake.notify_one();
    }
    pub fn set_enabled(&self, on: bool) { self.request(Request::Enable(on)); }
    pub fn login(&self) { self.request(Request::Login); }
    /// Forget this device: the daemon's logout (tailscale-rs deletes its key
    /// file and restarts; Go logs out). Signing in again needs approval.
    pub fn logout(&self) { self.request(Request::Logout); }
    fn begin_stop(&mut self)->Option<Arc<Completion>> {
        let mut worker=self.worker.inner.lock().unwrap();
        worker.start=None;
        worker.requests.clear();worker.requests.push_back(Request::Stop);
        worker.running=false;self.worker.wake.notify_one();worker.completion.clone()
    }
    pub async fn stop_async(&mut self) {
        if let Some(completion)=self.begin_stop() {completion.wait().await;}
        let task=self.worker.inner.lock().unwrap().thread.take();
        if let Some(task)=task {let _=task.await;}
    }
    #[cfg(test)]
    fn stop(&mut self) {
        if let Some(completion)=self.begin_stop() {
            generated_wait(async move {completion.wait().await}).unwrap();
        }
        let task=self.worker.inner.lock().unwrap().thread.take();
        if let Some(task)=task {
            let _=generated_wait(async move {task.await});
        }
    }
}
impl Drop for Tailscale {
    fn drop(&mut self) {
        // Dropping a JoinHandle detaches, never aborts. The owner still receives
        // Stop and joins both children; UI/drop never blocks a runtime thread.
        self.begin_stop();
        self.worker.inner.lock().unwrap().thread.take();
    }
}

async fn stage(name:&'static str) {
    // The initialized numeric owner only admits bounded memory here. No
    // general worker or ordinary fallback is needed by this runtime producer.
    crate::metrics::boot_current_at(name, crate::playtrace::boottime_us());
}
async fn flush_trace(trace:Vec<(Event,Duration,u64)>) {
    for (event,value,time) in trace {crate::metrics::record_at(event,value,time);}
}
/// The supervised daemon's process ID, for memory telemetry
/// (platform/memory.rs checks its name before reading it); 0 none.
static DAEMON_PID: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
pub(crate) fn daemon_pid() -> Option<u32> {
    Some(DAEMON_PID.load(std::sync::atomic::Ordering::Relaxed)).filter(|pid| *pid != 0)
}

async fn profile(kind:crate::process_profile::Actor,child:&Child,path:PathBuf) {
    if let Some(pid)=child.id() {
        if matches!(kind, crate::process_profile::Actor::Daemon) { DAEMON_PID.store(pid, std::sync::atomic::Ordering::Relaxed); }
        // The supervisor retains the unreaped child and joins this finite job
        // before any further child operation or queued request is processed.
        let _=crate::rt::spawn_blocking(move |blocking|crate::process_profile::record_pid(blocking,kind,pid,&path)).await;
    }
}

/// `slack_ns`: the child's timer slack (0 = kernel default).
fn command(path: &Path, slack_ns: u64) -> Command {
    let mut cmd = Command::new(path);
    // The Q2's two Linux CPUs are SMT threads of one XBurst2 core. One Go
    // thread moved 96 MB through the proxy as fast as two, with 35% less CPU,
    // and wakes 29% less at idle.
    cmd.env("GOMEMLIMIT", GO_MEMORY_LIMIT).env("GOGC", "50").env("GOMAXPROCS", "1").stdin(Stdio::null());
    if path.parent() == Some(Path::new("/usr/libexec/q2-network")) {
        // The installed root is immutable. Both the daemon and CLI must put
        // their optional cache in RAM, independently of persistent node state.
        cmd.env("XDG_CACHE_HOME", MAINLINE_CACHE);
    }
    // A crash/restart must not leave a daemon using the card during stock USB
    // storage. Linux sends SIGTERM if the owning worker thread disappears.
    // The CLI is the same Go binary (multicall) with the same .bss.
    crate::platform::process::supervised_child(&mut cmd, slack_ns, Some(DAEMON_DATA_LIMIT));
    cmd
}

/// The tailscale-rs daemon (os/tsrsd).
const RS_DAEMON: &str = "tsrsd";
/// Daemon exits counted for the restart delay.
const DAEMON_EXIT_WINDOW: Duration = Duration::from_secs(600);

/// How long to wait before restarting an exited daemon. tailscale-rs aborts
/// on a panic (upstream #439, #436, #495) and starts in about 2 s with its
/// stored key, so its first two exits in [`DAEMON_EXIT_WINDOW`] restart after
/// 3 s; repeated exits, and any Go exit, wait 15 s.
fn restart_delay(rs: bool, recent_exits: usize) -> Duration {
    if rs && recent_exits <= 2 { Duration::from_secs(3) } else { Duration::from_secs(15) }
}

/// The daemon to run, its executable and its CLI.
#[derive(Clone, Debug, PartialEq)]
struct Resolved { kind: Daemon, path: PathBuf, cli: PathBuf }

/// The daemon the image ships: tsrsd when present (the default image),
/// otherwise the Go tailscaled and CLI (an image built with the Go option,
/// or one without tailscale-rs, so an app-only update keeps Tailscale
/// working).
fn resolve(_blocking: &crate::rt::Blocking, app: &Path) -> io::Result<Resolved> {
    let rs = app.join(RS_DAEMON);
    let (go, go_cli) = (app.join("tailscaled"), app.join("tailscale"));
    if rs.is_file() { return Ok(Resolved { kind: Daemon::Rs, path: rs.clone(), cli: rs }); }
    if go.is_file() && go_cli.is_file() { return Ok(Resolved { kind: Daemon::Go, path: go, cli: go_cli }); }
    Err(io::Error::other("The image has no Tailscale daemon (tsrsd or tailscaled)"))
}

/// tailscale-rs keeps its keys beside tailscaled's directory, never in it:
/// `.../tailscale` gives `.../tailscale-rs`.
fn rs_state(data: &Path) -> PathBuf {
    let name = data.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    data.with_file_name(format!("{name}-rs"))
}

fn prepared(_blocking: &crate::rt::Blocking, app: &Path, data: &Path, socket: &Path, recent_exits: usize) -> io::Result<(Command, Resolved)> {
    let resolved = resolve(_blocking, app)?;
    std::fs::create_dir_all(data)?;
    if app == Path::new("/usr/libexec/q2-network") { std::fs::create_dir_all(MAINLINE_CACHE)?; }
    let log = if app == Path::new("/usr/libexec/q2-network") { PathBuf::from("/run/log/tailscale.log") }
        else { data.join("daemon.log") };
    if let Some(parent) = log.parent() { std::fs::create_dir_all(parent)?; }
    if log.metadata().is_ok_and(|m| m.len() > 256 * 1024) { let _ = std::fs::rename(&log, log.with_extension("log.old")); }
    let mut output = std::fs::OpenOptions::new().create(true).append(true).open(log)?;
    // One line per start, so a log shows restarts by q2nd (fixed words only).
    let _ = writeln!(output, "q2nd: daemon start backend={} recent_exits={recent_exits}",
        match resolved.kind { Daemon::Rs => "rs", Daemon::Go => "go" });
    let mut cmd=command(&resolved.path, DAEMON_TIMER_SLACK_NS);
    match resolved.kind {
        Daemon::Go => { cmd
            .arg("--tun=userspace-networking")
            .arg("--outbound-http-proxy-listen=127.0.0.1:1055")
            .arg("--socks5-server=127.0.0.1:1055")
            .arg(format!("--socket={}", socket.display()))
            .arg(format!("--statedir={}", data.display()))
            .arg(format!("--state={}", data.join("tailscaled.state").display())); }
        Daemon::Rs => { cmd
            .arg("--outbound-http-proxy-listen=127.0.0.1:1055")
            .arg(format!("--socket={}", socket.display()))
            .arg(format!("--statedir={}", rs_state(data).display())); }
    }
    cmd.stdout(output.try_clone()?).stderr(output);
    Ok((cmd, resolved))
}

async fn start_async(owner:&mut Option<spawn::Owner>,app:&Path,data:&Path,socket:&Path,recent_exits:usize)->io::Result<(Child,Resolved)> {
    let (app,data,socket)=(app.to_owned(),data.to_owned(),socket.to_owned());
    let (command,resolved)=crate::rt::spawn_blocking(move |blocking|prepared(blocking,&app,&data,&socket,recent_exits)).await
        .map_err(|_|io::Error::other("Tailscale preparation stopped"))??;
    // Linux binds PDEATHSIG to the spawning OS thread. Keep that parent alive
    // separately, while the I/O executor can continue serving sockets/timers.
    if owner.is_none() { *owner = Some(spawn::Owner::start().await?); }
    Ok((owner.as_ref().unwrap().spawn(command).await?,resolved))
}

/// Bounded LocalAPI exchange. Socket waits are reactor-owned; filesystem
/// observation and JSON work are finite worker jobs, with one total deadline.
fn mark_probe(probe:&mut Option<&mut (dyn FnMut(ProbePhase)+Send)>,phase:ProbePhase) {
    if let Some(mark)=probe.as_mut(){mark(phase);}
}
async fn observed_status_async(socket:&Path,timeout:Duration,mut probe:Option<&mut (dyn FnMut(ProbePhase)+Send)>)->io::Result<State> {
    mark_probe(&mut probe,ProbePhase::Begin);
    let result=tokio::time::timeout(timeout,async {
        let bytes=local_status_inner(socket,&mut probe).await?;
        let parsed=crate::rt::spawn_blocking(move |_|parse_status(&bytes)).await
            .map_err(|_|io::Error::other("Tailscale status parser stopped"))?;
        parsed.map_err(|error|{mark_probe(&mut probe,ProbePhase::InvalidStatus);io::Error::other(error)})
    }).await.unwrap_or_else(|_|Err(io::ErrorKind::TimedOut.into()));
    mark_probe(&mut probe,probe_result(&result));result
}
async fn local_status_inner(socket:&Path,probe:&mut Option<&mut (dyn FnMut(ProbePhase)+Send)>)->io::Result<Vec<u8>> {
    if probe.is_some() {
        let socket=socket.to_owned();
        let phase=crate::rt::spawn_blocking(move |_|match std::fs::symlink_metadata(socket) {
            Ok(m) if m.file_type().is_socket()=>ProbePhase::SocketPresent,
            Ok(_)=>ProbePhase::SocketOther,
            Err(error) if error.kind()==io::ErrorKind::NotFound=>ProbePhase::SocketMissing,
            Err(_)=>ProbePhase::SocketCheckError,
        }).await.unwrap_or(ProbePhase::SocketCheckError);
        mark_probe(probe,phase);
    }
    mark_probe(probe,ProbePhase::ConnectBegin);
    let mut stream=tokio::net::UnixStream::connect(socket).await?;
    mark_probe(probe,ProbePhase::Connected);
    stream.write_all(b"GET /localapi/v0/status?peers=false HTTP/1.0\r\nHost: local-tailscaled.sock\r\n\r\n").await?;
    mark_probe(probe,ProbePhase::RequestWritten);
    status_body(&mut stream,probe).await
}
async fn status_body(stream:&mut (impl tokio::io::AsyncRead+Unpin),probe:&mut Option<&mut (dyn FnMut(ProbePhase)+Send)>)->io::Result<Vec<u8>> {
    let mut bytes=Vec::new();let mut header_seen=false;let mut buffer=[0;8192];
    loop {
        let n=match stream.read(&mut buffer).await {
            Err(error) if error.kind()==io::ErrorKind::Interrupted=>continue,
            result=>result?,
        };
        if n==0 {mark_probe(probe,ProbePhase::ResponseEnd);break;}
        if bytes.len()+n>128*1024{return Err(io::Error::other("Tailscale status too large"));}
        if bytes.is_empty(){mark_probe(probe,ProbePhase::FirstResponse);}
        let old_len=bytes.len();bytes.extend_from_slice(&buffer[..n]);
        if probe.is_some() && !header_seen && bytes[old_len.saturating_sub(3)..].windows(4).any(|w|w==b"\r\n\r\n") {
            header_seen=true;mark_probe(probe,ProbePhase::HeaderReceived);
        }
    }
    let end=bytes.windows(4).position(|w|w==b"\r\n\r\n").ok_or_else(||io::Error::other("Tailscale status: no response"))?;
    if !bytes.starts_with(b"HTTP/1.0 200") && !bytes.starts_with(b"HTTP/1.1 200") {return Err(io::Error::other("Tailscale status failed"));}
    Ok(bytes.split_off(end+4))
}
#[cfg(test)]
fn observed_status(socket:&Path,timeout:Duration,probe:Option<&mut (dyn FnMut(ProbePhase)+Send)>)->io::Result<State> {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(observed_status_async(socket,timeout,probe))
}
#[cfg(test)]
fn local_status(socket:&Path,timeout:Duration)->io::Result<Vec<u8>> {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        tokio::time::timeout(timeout,local_status_inner(socket,&mut None)).await.unwrap_or_else(|_|Err(io::ErrorKind::TimedOut.into()))
    })
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum CliCompletion { Success, Nonzero, Signal, WaitError, Deadline }
impl CliCompletion {
    fn stage(self) -> &'static str {
        match self {
            Self::Success => "tailscale-up-exit-zero",
            Self::Nonzero => "tailscale-up-exit-nonzero",
            Self::Signal => "tailscale-up-exit-signal",
            Self::WaitError => "tailscale-up-wait-error",
            Self::Deadline => "tailscale-up-deadline",
        }
    }
}
/// What a CLI job does: sign in (`up`) or forget this device (`logout`).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Purpose { Up, Logout }
impl Purpose {
    fn stage(self, completion: CliCompletion) -> &'static str {
        match (self, completion) {
            (Self::Up, c) => c.stage(),
            (Self::Logout, CliCompletion::Success) => "tailscale-logout-exit-zero",
            (Self::Logout, _) => "tailscale-logout-failed",
        }
    }
    fn returned(self) -> &'static str {
        match self { Self::Up => "tailscale-up-returned", Self::Logout => "tailscale-logout-returned" }
    }
}
struct CliJob {
    child: Option<Child>,
    deadline: Instant,
    completion: Option<CliCompletion>,
    purpose: Purpose,
}
impl CliJob {
    /// `cli`: the Go CLI, or tsrsd itself, whose subcommands are the CLI shim.
    async fn start_async(owner: &spawn::Owner, cli: &Path, socket: &Path, timeout: Duration) -> io::Result<Self> {
        Self::spawn_async(owner, cli, socket, timeout, Purpose::Up, &["up", "--hostname=q2", "--accept-dns=false", "--timeout=5s"]).await
    }
    async fn logout_async(owner: &spawn::Owner, cli: &Path, socket: &Path, timeout: Duration) -> io::Result<Self> {
        Self::spawn_async(owner, cli, socket, timeout, Purpose::Logout, &["logout"]).await
    }
    async fn spawn_async(owner: &spawn::Owner, cli: &Path, socket: &Path, timeout: Duration, purpose: Purpose, args: &[&str]) -> io::Result<Self> {
        // Authentication URLs/status come from LocalAPI. The command's
        // output was unused; discard it without a pipe or detached reader.
        let mut command = command(cli, 0);
        command.arg(format!("--socket={}", socket.display()))
            .args(args)
            .stdout(Stdio::null()).stderr(Stdio::null());
        let child = owner.spawn(command).await?;
        let deadline=Instant::now()+timeout;
        Ok(Self { child: Some(child), deadline, completion: None, purpose })
    }
    fn remaining(&self) -> Duration { self.deadline.saturating_duration_since(Instant::now()) }
    fn poll_now(&mut self) -> Option<io::Result<()>> {
        if let Some(done)=self.completion {
            return Some(if done==CliCompletion::Success {Ok(())} else if done==CliCompletion::Deadline {Err(io::ErrorKind::TimedOut.into())} else {Err(io::Error::other("Tailscale command failed"))});
        }
        let child = self.child.as_mut()?;
        match child.try_wait() {
            Ok(Some(status)) => { self.observed(Ok(status)); self.poll_now() }
            Err(error) => { self.observed(Err(io::Error::new(error.kind(), error.to_string()))); Some(Err(error)) }
            Ok(None) => None,
        }
    }
    fn observed(&mut self, status: io::Result<std::process::ExitStatus>) {
        self.child = None;
        self.completion = Some(match status {
            Ok(status) if status.success() => CliCompletion::Success,
            Ok(status) if status.signal().is_some() => CliCompletion::Signal,
            Ok(_) => CliCompletion::Nonzero,
            Err(_) => CliCompletion::WaitError,
        });
    }
    async fn expire(&mut self) {
        if self.child.is_some() && self.remaining().is_zero() {
            if self.poll_now().is_none() {self.cancel_async().await;self.completion=Some(CliCompletion::Deadline);}
        }
    }
    async fn cancel_async(&mut self) {
        if let Some(mut child)=self.child.take() {
            if matches!(child.try_wait(),Ok(None)) {let _=child.kill().await;}
        }
    }
    #[cfg(test)]
    fn start(app:&Path,socket:&Path,timeout:Duration)->io::Result<Self> {
        let (app,socket)=(app.to_owned(),socket.to_owned());
        generated_wait(async move {crate::rt::io().spawn(async move {let owner=spawn::Owner::start().await?; Self::start_async(&owner,&app.join("tailscale"),&socket,timeout).await}).await.unwrap()}).unwrap()
    }
    #[cfg(test)]
    fn poll(&mut self)->Option<io::Result<()>> {
        // Only external synchronous test callers use this adapter.
        let mut job=Self {child:self.child.take(),deadline:self.deadline,completion:self.completion,purpose:self.purpose};
        let (job,result)=generated_wait(async move {job.expire().await;let result=job.poll_now();(job,result)}).unwrap();
        *self=job;result
    }
}
async fn cancel_up_async(up:&mut Option<CliJob>) {
    if let Some(mut job)=up.take() {
        job.cancel_async().await;
        if job.purpose == Purpose::Up { stage("tailscale-up-cancelled").await; }
        stage(job.purpose.returned()).await;
    }
}
async fn stop_spawn_owner(owner: &mut Option<spawn::Owner>) {
    if let Some(owner) = owner.take() { owner.stop().await; }
}
async fn stop_child_async(child:&mut Option<Child>) {
    if let Some(mut child)=child.take() {
        if !crate::platform::process::terminate_owned_child(&mut child) {return;}
        if tokio::time::timeout(Duration::from_secs(3),child.wait()).await.is_err() {let _=child.kill().await;}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    pub(super) fn fixture() -> PathBuf {
        // Cargo runs this test binary in its package directory. Keep the Unix
        // socket name relative/short and every fixture below ignored build/.
        let root=std::env::var_os("Q2_GENERATED_TEST_ROOT").map(PathBuf::from)
            .unwrap_or_else(||PathBuf::from("../../build/perf"));
        let path = root.join(format!("ts-{:08x}", fastrand::u32(..)));
        std::fs::create_dir_all(&path).unwrap();
        path
    }
    fn fake_cli(dir: &Path, script: &str) {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("tailscale");
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    fn complete(job: &mut CliJob) -> io::Result<()> {
        loop {
            if let Some(result) = job.poll() { return result; }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn waiting_owner() -> Owner {
        Owner { inner: Mutex::new(Worker { state: State::default(), start: None,
            thread: None, completion: None, requests: VecDeque::new(), running: true }),
            wake: tokio::sync::Notify::new(), notify: Arc::new(|| {}) }
    }

    #[test]
    fn observation_provenance_distinguishes_backend_transport_and_parse_without_strings() {
        for (backend, expected) in [("Running", Observation::Running), ("NeedsLogin", Observation::NeedsLogin),
            ("NeedsMachineAuth", Observation::NeedsMachineAuth), ("Starting", Observation::Starting),
            ("Stopped", Observation::Stopped), ("GeneratedUnknown", Observation::OtherBackend)] {
            let state = parse_status(format!(r#"{{"BackendState":"{backend}"}}"#).as_bytes()).unwrap();
            assert_eq!(state.observation, expected);
        }
        assert_eq!(error_observation(&io::ErrorKind::TimedOut.into()), Observation::Timeout);
        assert_eq!(error_observation(&io::ErrorKind::NotFound.into()), Observation::SocketMissing);
        let error = parse_status(b"not JSON").unwrap_err();
        assert_eq!(error_observation(&io::Error::other(error)), Observation::InvalidStatus);
        assert_eq!(error_observation(&io::Error::other("generated")), Observation::Error);
        let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let count = seen.clone(); let mut owner = waiting_owner();
        owner.notify = Arc::new(move || { count.fetch_add(1, std::sync::atomic::Ordering::Relaxed); });
        let mut state = State {status: "Waiting for Tailscale".into(), observation: Observation::Timeout, ..State::default()};
        owner.publish(state.clone());
        state.observation = Observation::SocketMissing;
        owner.publish(state);
        assert_eq!(seen.load(std::sync::atomic::Ordering::Relaxed), 1, "provenance does not change UI notification cadence");
        assert_eq!(owner.inner.lock().unwrap().state.observation, Observation::SocketMissing);
    }

    #[test]
    fn state_notifications_follow_publication_deduplicate_and_release_owner_lock() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let owner = Arc::new_cyclic(|weak: &std::sync::Weak<Owner>| {
            let weak = weak.clone(); let seen = seen.clone();
            let mut owner = waiting_owner();
            owner.notify = Arc::new(move || {
                let owner = weak.upgrade().unwrap();
                let state = owner.inner.try_lock().expect("publication released the owner lock").state.clone();
                seen.lock().unwrap().push(state);
            });
            owner
        });
        let connected = State { status: "Connected".into(), address: "100.64.0.1".into(), ..State::default() };
        owner.publish(connected.clone()); owner.publish(connected.clone());
        let changed = State { health: "Generated health change".into(), ..connected.clone() };
        owner.publish(changed.clone());
        owner.inner.lock().unwrap().requests.push_back(Request::Enable(true));
        assert!(!owner.disabled(), "queued re-enable retains the live owner");
        owner.inner.lock().unwrap().requests.clear();
        assert!(owner.disabled());
        assert_eq!(*seen.lock().unwrap(), vec![connected, changed, State {status: "Off".into(), observation: Observation::Off, ..State::default()}]);
    }

    #[test]
    fn child_completion_wakes_without_the_status_poll_deadline() {
        let dir = fixture();
        fake_cli(&dir, "#!/bin/sh\nexit 0\n");
        let app = dir.clone();
        generated_wait(crate::rt::io().spawn(async move {
            let parent = spawn::Owner::start().await.unwrap();
            let owner = waiting_owner();
            let mut up = Some(CliJob::start_async(&parent, &app.join("tailscale"), Path::new(SOCKET), Duration::from_secs(8)).await.unwrap());
            let mut daemon = None;
            let result = tokio::time::timeout(Duration::from_secs(3),
                wait_for_work(&owner, Duration::from_secs(3600), &mut up, &mut daemon)).await.unwrap();
            let Wakeup::Cli(Some(status)) = result else { panic!("child exit must wake the owner directly") };
            let job = up.as_mut().unwrap();
            job.observed(status);
            assert_eq!(job.completion, Some(CliCompletion::Success));
            assert!(job.poll_now().unwrap().is_ok());
            drop(up);
            parent.stop().await;
        })).unwrap().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn queued_commands_win_and_cancelled_exit_wait_keeps_child_owned() {
        generated_wait(crate::rt::io().spawn(async {
            let parent = spawn::Owner::start().await.unwrap();
            let owner = waiting_owner();
            let mut command = command(Path::new("/bin/sleep"), 0);
            command.arg("30");
            let mut daemon = Some(parent.spawn(command).await.unwrap());
            let mut up = None;
            tokio::select! {
                biased;
                _ = wait_for_work(&owner, Duration::from_secs(3600), &mut up, &mut daemon) => panic!("live child completed"),
                () = tokio::task::yield_now() => {},
            }
            assert!(daemon.as_mut().unwrap().try_wait().unwrap().is_none());
            owner.inner.lock().unwrap().requests.push_back(Request::Stop);
            assert!(matches!(wait_for_work(&owner, Duration::from_secs(3600), &mut up, &mut daemon).await,
                Wakeup::Request(Some(Request::Stop))));
            stop_child_async(&mut daemon).await;
            assert!(daemon.is_none());
            parent.stop().await;
        })).unwrap().unwrap();
    }

    #[test]
    fn daemon_exit_and_cli_deadline_are_direct_owner_events() {
        let dir = fixture();
        fake_cli(&dir, "#!/bin/sh\nexec sleep 30\n");
        let app = dir.clone();
        generated_wait(crate::rt::io().spawn(async move {
            let parent = spawn::Owner::start().await.unwrap();
            let owner = waiting_owner();
            let mut command = command(Path::new("/bin/sh"), 0);
            command.args(["-c", "exit 7"]);
            let mut daemon = Some(parent.spawn(command).await.unwrap());
            let mut up = None;
            let result = tokio::time::timeout(Duration::from_secs(3),
                wait_for_work(&owner, Duration::from_secs(3600), &mut up, &mut daemon)).await.unwrap();
            let Wakeup::Daemon(Ok(status)) = result else { panic!("daemon exit did not wake owner") };
            assert_eq!(status.code(), Some(7));
            assert_eq!(daemon.as_mut().unwrap().try_wait().unwrap().unwrap().code(), Some(7));
            stop_child_async(&mut daemon).await;
            up = Some(CliJob::start_async(&parent, &app.join("tailscale"), Path::new(SOCKET), Duration::from_millis(20)).await.unwrap());
            let result = tokio::time::timeout(Duration::from_secs(3),
                wait_for_work(&owner, Duration::from_secs(3600), &mut up, &mut daemon)).await.unwrap();
            assert!(matches!(result, Wakeup::Cli(None)));
            let job = up.as_mut().unwrap();
            assert!(job.child.is_some(), "deadline observation must not abandon owned cleanup");
            job.expire().await;
            assert_eq!(job.completion, Some(CliCompletion::Deadline));
            assert!(job.child.is_none());
            drop(up);
            parent.stop().await;
        })).unwrap().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn probe_budget_caps_attempts_and_window_once_without_resetting() {
        let now = Instant::now();
        let mut budget = ProbeBudget::default();
        assert_eq!(budget.next_at(now), (None, false));
        budget.started = Some(now);
        for id in 1..=PROBE_ATTEMPTS { assert_eq!(budget.next_at(now), (Some(id), false)); }
        assert_eq!(budget.next_at(now), (None, true));
        budget.start();
        assert_eq!(budget.next_at(now), (None, false));
        let mut budget = ProbeBudget { started: Some(now), ..ProbeBudget::default() };
        assert_eq!(budget.next_at(now + PROBE_WINDOW), (None, true));
        assert_eq!(budget.next_at(now + PROBE_WINDOW), (None, false));
    }

    #[test]
    fn probe_missing_socket_is_separate_from_request_or_response_wait() {
        let dir = fixture();
        let mut phases = Vec::new();
        let result = observed_status(&dir.join("missing.sock"), Duration::from_millis(50), Some(&mut |p| phases.push(p)));
        let error = result.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        assert_eq!(poll_interval(true, false, false, Some(Duration::from_secs(6)), error.kind() == io::ErrorKind::NotFound), SOCKET_START_POLL);
        assert_eq!(poll_interval(true, false, false, Some(SOCKET_MISSING_WINDOW), error.kind() == io::ErrorKind::NotFound), Duration::from_secs(3));
        assert_eq!(phases, [ProbePhase::Begin, ProbePhase::SocketMissing, ProbePhase::ConnectBegin, ProbePhase::NotFound]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn probe_boundaries_follow_real_split_response_and_eof() {
        let dir = fixture();
        let socket = dir.join("phases.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let (tx, rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut c, _) = listener.accept().unwrap();
            let mut request = [0; 512];
            let n = c.read(&mut request).unwrap();
            assert!(request[..n].starts_with(b"GET /localapi/v0/status?peers=false HTTP/1.0"));
            c.write_all(b"HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n").unwrap();
            assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), ProbePhase::FirstResponse);
            // Split the header terminator across two reads, with explicit
            // callback synchronization rather than guessed sleep delays.
            c.write_all(b"\r\n{\"BackendState\":\"Running\"}").unwrap();
            assert_eq!(rx.recv_timeout(Duration::from_secs(2)).unwrap(), ProbePhase::HeaderReceived);
        });
        let mut phases = Vec::new();
        let state = observed_status(&socket, Duration::from_secs(3), Some(&mut |p| {
            phases.push(p);
            if matches!(p, ProbePhase::FirstResponse | ProbePhase::HeaderReceived) { tx.send(p).unwrap(); }
        })).unwrap();
        assert_eq!(state.status, "Connected");
        server.join().unwrap();
        assert_eq!(phases, [ProbePhase::Begin, ProbePhase::SocketPresent, ProbePhase::ConnectBegin,
            ProbePhase::Connected, ProbePhase::RequestWritten, ProbePhase::FirstResponse,
            ProbePhase::HeaderReceived, ProbePhase::ResponseEnd, ProbePhase::Success]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[tokio::test]
    async fn probe_does_not_turn_complete_body_without_eof_into_readiness() {
        let dir = fixture();
        let socket = dir.join("eof.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (mut connection, _) = listener.accept().await.unwrap();
            let mut request = [0; 512];
            connection.read(&mut request).await.unwrap();
            connection.write_all(b"HTTP/1.0 200 OK\r\n\r\n{\"BackendState\":\"Running\"}").await.unwrap();
            // The task result owns the open stream until the client has timed out.
            connection
        });
        let mut phases = Vec::new();
        let result = observed_status_async(&socket, crate::library::TEST_LIVENESS, Some(&mut |phase| {
            phases.push(phase);
            if phase == ProbePhase::HeaderReceived {
                // Allow real setup to finish, then let the current-thread runtime
                // advance its clock to the probe deadline while EOF is withheld.
                tokio::time::pause();
            }
        })).await;
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert!(phases.contains(&ProbePhase::HeaderReceived));
        assert!(!phases.contains(&ProbePhase::ResponseEnd));
        assert_eq!(phases.last(), Some(&ProbePhase::Timeout));
        drop(server.await.unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn probe_invalid_status_remains_error_after_successful_transport() {
        let dir = fixture();let socket = dir.join("invalid.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut c, _) = listener.accept().unwrap();let mut request = [0; 512];c.read(&mut request).unwrap();
            c.write_all(b"HTTP/1.0 200 OK\r\n\r\ninvalid-generated-json").unwrap();
        });
        let mut phases = Vec::new();
        let error = observed_status(&socket, Duration::from_secs(2), Some(&mut |p| phases.push(p))).unwrap_err();
        server.join().unwrap();
        for age in [SOCKET_START_WINDOW, Duration::from_secs(6), SOCKET_MISSING_WINDOW] {
            assert_eq!(poll_interval(true, false, false, Some(age), error.kind() == io::ErrorKind::NotFound), Duration::from_secs(3));
        }
        assert_eq!(&phases[phases.len()-3..], &[ProbePhase::ResponseEnd, ProbePhase::InvalidStatus, ProbePhase::Error]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn startup_poll_schedule_is_immediate_bounded_and_opt_in() {
        // Virtual-time transitions, including exactly the fast-poll deadline.
        for millis in 0..=20_000 {
            let age = Some(Duration::from_millis(millis));
            for missing in [false, true] {
                assert_eq!(poll_interval(false, false, true, age, missing), Duration::from_secs(3600));
                assert_eq!(poll_interval(true, false, true, age, missing), Duration::ZERO);
                let expected = if millis < 5_000 || (missing && millis < 15_000) { SOCKET_START_POLL } else { Duration::from_secs(3) };
                assert_eq!(poll_interval(true, false, false, age, missing), expected);
                assert_eq!(waiting_for_socket(age, missing), expected == SOCKET_START_POLL);
            }
        }
        assert_eq!(poll_interval(true, true, false, None, false), Duration::from_secs(30));
        assert_eq!(poll_interval(true, false, false, None, false), Duration::from_secs(3));
        assert!(!waiting_for_socket(None, true));
    }

    #[test]
    fn disabled_owner_stays_threadless_until_concurrent_first_enable() {
        let root = crate::library::generated_test_directory("tailscale-lazy");
        let tailscale = Arc::new(Tailscale::at_socket(root.clone(), root.join("state"), true, false, root.join("socket")));
        assert!(tailscale.worker.inner.lock().unwrap().thread.is_none());
        tailscale.set_enabled(false);tailscale.login();
        assert!(tailscale.worker.inner.lock().unwrap().thread.is_none());
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let threads: Vec<_> = (0..8).map(|_| {
            let t=tailscale.clone();let b=barrier.clone();
            std::thread::spawn(move || { b.wait();t.set_enabled(true);t.worker.inner.lock().unwrap().thread.as_ref().unwrap().id() })
        }).collect();
        let ids: Vec<_> = threads.into_iter().map(|t|t.join().unwrap()).collect();
        assert!(ids.iter().all(|id|*id==ids[0]), "first activation must have one owner");
        let wait = |status: &str| {
            let deadline=Instant::now()+Duration::from_secs(2);
            while tailscale.state().status!=status {
                assert!(Instant::now()<deadline,"command did not wake the worker");
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        wait("Available on Q2");
        tailscale.set_enabled(false);wait("Off");
        let deadline=Instant::now()+Duration::from_secs(2);
        while tailscale.worker.inner.lock().unwrap().running {
            assert!(Instant::now()<deadline);std::thread::yield_now();
        }
        tailscale.set_enabled(true);wait("Available on Q2");
        assert_ne!(tailscale.worker.inner.lock().unwrap().thread.as_ref().unwrap().id(),ids[0]);
        let mut tailscale=Arc::try_unwrap(tailscale).ok().unwrap();tailscale.stop();
        tailscale.set_enabled(true);
        assert!(tailscale.worker.inner.lock().unwrap().thread.is_none(), "explicit stop must not resurrect an owner");
    }

    #[test]
    fn stopping_before_first_enable_does_not_create_an_owner() {
        let root=crate::library::generated_test_directory("tailscale-never-started");
        let mut tailscale=Tailscale::at_socket(root.clone(),root.join("state"),true,false,root.join("socket"));
        tailscale.stop();tailscale.set_enabled(true);
        let worker=tailscale.worker.inner.lock().unwrap();
        assert!(worker.start.is_none() && worker.thread.is_none());
    }

    #[test]
    fn disabled_worker_never_spawns_and_enabled_child_keeps_go_limits() {
        use std::os::unix::fs::PermissionsExt;
        assert!(!crate::config::Tailscale::default().enabled);
        let dir = fixture();
        let daemon = dir.join("tailscaled");
        std::fs::write(&daemon, "#!/bin/sh\nset -e\nd=$(dirname \"$0\")\nread slack < /proc/$$/timerslack_ns\nprintf '%s %s %s %s %s\\n' \"$GOMAXPROCS\" \"$GOMEMLIMIT\" \"$GOGC\" \"$slack\" \"$(ulimit -d)\" >\"$d/spawn-env.tmp\"\nmv \"$d/spawn-env.tmp\" \"$d/spawn-env\"\nexec sleep 30\n").unwrap();
        std::fs::write(dir.join("tailscale"), "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&daemon, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(dir.join("tailscale"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut tailscale = Tailscale::at_socket(dir.clone(), dir.join("state"), false, false, dir.join("limits.sock"));
        // Login is an action within the enabled feature, not implicit enable.
        tailscale.set_enabled(false);
        tailscale.login();
        assert!(tailscale.worker.inner.lock().unwrap().thread.is_none());
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(tailscale.state().status, "Off");
        assert!(!dir.join("spawn-env").exists());
        assert!(!dir.join("state").exists());
        tailscale.set_enabled(true);
        let deadline = Instant::now() + Duration::from_secs(3);
        while !dir.join("spawn-env").exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        // stop() reaps the generated sleep process before fixture cleanup.
        tailscale.stop();
        assert_eq!(std::fs::read_to_string(dir.join("spawn-env")).unwrap(), format!("1 {GO_MEMORY_LIMIT} 50 {DAEMON_TIMER_SLACK_NS} {}\n", DAEMON_DATA_LIMIT >> 10));
        std::fs::remove_dir_all(dir).unwrap();
    }

    fn executable(path: &Path, script: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, script).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn sign_in_polls_fast_until_connected_or_the_window_ends() {
        let now = Instant::now();
        let until = Some(now + SIGN_IN_WINDOW);
        assert!(signing_in(until, false, now));
        assert!(signing_in(until, false, now + SIGN_IN_WINDOW - Duration::from_millis(1)));
        assert!(!signing_in(until, false, now + SIGN_IN_WINDOW), "a few minutes after Sign In the normal cadence returns");
        assert!(!signing_in(until, true, now), "connected");
        assert!(!signing_in(None, false, now));
        assert!(SIGN_IN_POLL < Duration::from_secs(3), "faster than the not-connected cadence");
    }

    #[test]
    fn the_daemon_is_the_one_the_image_ships_and_tsrsd_wins_over_go() {
        let dir = fixture();
        let blocking = crate::rt::Blocking::dedicated_thread();
        let resolve = |dir: &Path| resolve(&blocking, dir);
        assert!(resolve(&dir).unwrap_err().to_string().contains("Install Tailscale"));
        // An image built with the Go option, or from before tailscale-rs.
        executable(&dir.join("tailscaled"), "#!/bin/sh\n");
        assert!(resolve(&dir).is_err(), "the Go daemon needs its CLI");
        executable(&dir.join("tailscale"), "#!/bin/sh\n");
        let r = resolve(&dir).unwrap();
        assert_eq!((r.kind, r.path.file_name().unwrap().to_str(), r.cli.file_name().unwrap().to_str()),
            (Daemon::Go, Some("tailscaled"), Some("tailscale")));
        // An older image with both daemons: tailscale-rs.
        executable(&dir.join(RS_DAEMON), "#!/bin/sh\n");
        let r = resolve(&dir).unwrap();
        assert_eq!(r.kind, Daemon::Rs);
        assert_eq!(r.cli, r.path, "tsrsd is its own CLI");
        // The default image: tailscale-rs only.
        for name in ["tailscaled", "tailscale"] { std::fs::remove_file(dir.join(name)).unwrap(); }
        assert_eq!(resolve(&dir).unwrap().kind, Daemon::Rs);
        assert_eq!(rs_state(Path::new("/state/network/tailscale")), Path::new("/state/network/tailscale-rs"));
        assert_eq!((restart_delay(true, 1), restart_delay(true, 3), restart_delay(false, 1)),
            (Duration::from_secs(3), Duration::from_secs(15), Duration::from_secs(15)));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rs_daemon_gets_its_own_state_directory_and_never_tailscaled_state() {
        let dir = fixture();
        executable(&dir.join(RS_DAEMON), "#!/bin/sh\n");
        let data = dir.join("tailscale");
        let blocking = crate::rt::Blocking::dedicated_thread();
        let (cmd, resolved) = prepared(&blocking, &dir, &data, Path::new("/run/generated.sock"), 0).unwrap();
        assert_eq!(resolved.kind, Daemon::Rs);
        let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args, ["--outbound-http-proxy-listen=127.0.0.1:1055", "--socket=/run/generated.sock",
            &format!("--statedir={}", dir.join("tailscale-rs").display())]);
        assert!(std::fs::read_to_string(data.join("daemon.log")).unwrap().contains("q2nd: daemon start backend=rs recent_exits=0"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_supervisor_runs_only_the_daemon_the_image_ships() {
        // Each fake daemon records its name and arguments, then stays up.
        let record = "#!/bin/sh\nd=$(dirname \"$0\")\nprintf '%s %s\\n' \"$(basename \"$0\")\" \"$*\" >> \"$d/started\"\nexec sleep 30\n";
        let started = |dir: &Path| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let lines = std::fs::read_to_string(dir.join("started")).unwrap_or_default();
                if !lines.is_empty() { return lines; }
                assert!(Instant::now() < deadline, "no daemon started");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        // Both daemons (an older image): tsrsd, and never the Go daemon.
        let both = fixture();
        executable(&both.join(RS_DAEMON), record);
        executable(&both.join("tailscaled"), record);
        executable(&both.join("tailscale"), "#!/bin/sh\nexit 0\n");
        let mut tailscale = Tailscale::at_socket(both.clone(), both.join("tailscale-state"), false, true, both.join("both.sock"));
        assert!(started(&both).starts_with("tsrsd "));
        std::thread::sleep(Duration::from_millis(200));
        tailscale.stop();
        let lines = std::fs::read_to_string(both.join("started")).unwrap();
        assert!(lines.lines().all(|l| l.starts_with("tsrsd ")), "{lines}");
        // The Go option's image: the Go daemon with its own state file.
        let go = fixture();
        executable(&go.join("tailscaled"), record);
        executable(&go.join("tailscale"), "#!/bin/sh\nexit 0\n");
        let mut tailscale = Tailscale::at_socket(go.clone(), go.join("tailscale-state"), false, true, go.join("go.sock"));
        let lines = started(&go);
        assert!(lines.starts_with("tailscaled ") && lines.contains("--state="), "{lines}");
        tailscale.stop();
        for dir in [both, go] { std::fs::remove_dir_all(dir).unwrap(); }
    }

    #[test]
    fn installed_daemon_and_cli_put_optional_cache_in_ram() {
        for name in ["tailscaled", "tailscale"] {
            let cmd = command(&Path::new("/usr/libexec/q2-network").join(name), 0);
            let env: std::collections::HashMap<_, _> = cmd.get_envs()
                .filter_map(|(key, value)| value.map(|value| (key.to_string_lossy().into_owned(), value.to_string_lossy().into_owned()))).collect();
            assert_eq!(env.get("XDG_CACHE_HOME").map(String::as_str), Some(MAINLINE_CACHE));
            assert_eq!(env.get("GOMAXPROCS").map(String::as_str), Some("1"));
        }
    }

    #[test]
    fn fast_exit_is_visible_instead_of_a_permanent_starting_message() {
        use std::os::unix::fs::PermissionsExt;
        let dir = fixture();
        for name in ["tailscaled", "tailscale"] {
            let file = dir.join(name);
            std::fs::write(&file, "#!/bin/sh\nexit 7\n").unwrap();
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut tailscale = Tailscale::at_socket(dir.clone(), dir.join("state"), false, false, dir.join("exit.sock"));
        tailscale.set_enabled(true);
        let deadline = Instant::now() + Duration::from_secs(5);
        while tailscale.state().status != "Could Not Start" && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        let status = tailscale.state(); tailscale.stop();
        assert_eq!(status.status, "Could Not Start");
        assert!(status.health.contains("code 7"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn login_approval_and_health_states() {
        let s = parse_status(br#"{"BackendState":"NeedsLogin","AuthURL":"https://login.tailscale.com/a/example","TailscaleIPs":null,"Health":["No Wi-Fi"]}"#).unwrap();
        assert_eq!(s.status, "Sign In Required");
        assert_eq!(s.auth_url, "https://login.tailscale.com/a/example");
        assert_eq!(s.health, "No Wi-Fi");
        assert!(s.address.is_empty());
        assert_eq!(parse_status(br#"{"BackendState":"NeedsMachineAuth"}"#).unwrap().status, "Approve in Admin Console");
        let s = parse_status(br#"{"BackendState":"Running","TailscaleIPs":["100.64.1.2","fd7a::1"],"Health":null}"#).unwrap();
        assert_eq!(s.status, "Connected");
        assert_eq!(s.address, "100.64.1.2");
        assert!(s.auth_url.is_empty());
        assert!(parse_status(b"not JSON").is_err());
    }

    #[test]
    fn cli_timeout_kills_and_reaps_child() {
        let dir = fixture();
        fake_cli(&dir, "#!/bin/sh\nexec sleep 30\n");
        let mut job = CliJob::start(&dir, Path::new(SOCKET), Duration::from_secs(8)).unwrap();
        let pid = job.child.as_ref().unwrap().id().unwrap() as i32;
        job.deadline = Instant::now();
        let error = job.poll().unwrap().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(job.completion, Some(CliCompletion::Deadline));
        assert!(job.child.is_none());
        assert_eq!(crate::platform::process::generated::wait_nonblocking(pid).unwrap_err().raw_os_error(), Some(libc::ECHILD));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn unused_cli_output_finishes_without_a_pipe_deadlock() {
        let dir = fixture();
        fake_cli(&dir, "#!/bin/sh\nexec head -c 100000 /dev/zero\n");
        complete(&mut CliJob::start(&dir, Path::new(SOCKET), Duration::from_secs(3)).unwrap()).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn actual_cli_completion_distinguishes_exit_from_signal() {
        let dir = fixture();
        for (script, expected) in [("exit 0", CliCompletion::Success),
                ("exit 7", CliCompletion::Nonzero), ("kill -TERM $$", CliCompletion::Signal)] {
            fake_cli(&dir, &format!("#!/bin/sh\n{script}\n"));
            let mut job = CliJob::start(&dir, Path::new(SOCKET), Duration::from_secs(3)).unwrap();
            let result = complete(&mut job);
            assert_eq!(result.is_ok(), expected == CliCompletion::Success);
            assert_eq!(job.completion, Some(expected));
            assert!(job.child.is_none());
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reads_status_from_local_api_socket() {
        let dir = fixture();
        let socket = dir.join("ts.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for reply in [&b"HTTP/1.0 200 OK\r\nContent-Type: application/json\r\n\r\n{\"BackendState\":\"Running\"}"[..],
                          b"HTTP/1.0 403 Forbidden\r\n\r\ndenied"] {
                let (mut c, _) = listener.accept().unwrap();
                let mut req = [0; 512];
                let n = c.read(&mut req).unwrap();
                requests.push(String::from_utf8_lossy(&req[..n]).into_owned());
                c.write_all(reply).unwrap();
            }
            requests
        });
        let body = local_status(&socket, Duration::from_secs(3)).unwrap();
        assert_eq!(parse_status(&body).unwrap().status, "Connected");
        assert!(local_status(&socket, Duration::from_secs(3)).is_err());
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("GET /localapi/v0/status?peers=false HTTP/1.0\r\n"));
        assert!(requests[0].contains("Host: local-tailscaled.sock\r\n"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cancelling_up_reaps_its_owned_child_and_preserves_required_flags() {
        let dir = fixture();
        let args = dir.join("args");
        fake_cli(&dir, &format!("#!/bin/sh\nprintf '%s\\n' \"$GOMAXPROCS\" \"$@\" > '{}'\n", args.display()));
        complete(&mut CliJob::start(&dir, Path::new(SOCKET), Duration::from_secs(3)).unwrap()).unwrap();
        let lines = std::fs::read_to_string(args).unwrap();
        assert_eq!(lines.lines().collect::<Vec<_>>(), ["1", &format!("--socket={SOCKET}"), "up", "--hostname=q2", "--accept-dns=false", "--timeout=5s"]);
        fake_cli(&dir, "#!/bin/sh\nexec sleep 30\n");
        let mut up = Some(CliJob::start(&dir, Path::new(SOCKET), Duration::from_secs(8)).unwrap());
        let pid = up.as_ref().unwrap().child.as_ref().unwrap().id().unwrap() as i32;
        up=generated_wait(async move {cancel_up_async(&mut up).await;up}).unwrap();
        assert!(up.is_none());
        assert_eq!(crate::platform::process::generated::wait_nonblocking(pid).unwrap_err().raw_os_error(), Some(libc::ECHILD));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn default_off_never_starts_a_daemon_or_up_job() {
        let dir = fixture();
        let data = dir.join("never-created");
        let mut worker = Tailscale::new(dir.clone(), data.clone(), false, false);
        worker.stop();
        assert!(!data.exists());
        assert_eq!(worker.state().status, "Off");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn local_api_deadline_covers_a_trickling_response() {
        let dir = fixture();
        let socket = dir.join("trickle.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0; 512];
            let _ = stream.read(&mut request);
            for _ in 0..100 {
                if stream.write_all(b"x").is_err() { break; }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let start = Instant::now();
        let error = local_status(&socket, Duration::from_millis(50)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(start.elapsed() < Duration::from_secs(1));
        server.join().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn running_status_and_disable_progress_while_up_is_stalled() {
        use std::os::unix::fs::PermissionsExt;
        let dir = fixture();
        let daemon = dir.join("tailscaled");
        // The generated LocalAPI runs in Rust so this lifecycle test needs only
        // the player's existing shell, not a host-only Python interpreter.
        struct LocalApi {
            stop: Arc<std::sync::atomic::AtomicBool>,
            server: Option<std::thread::JoinHandle<()>>,
        }
        impl Drop for LocalApi {
            fn drop(&mut self) {
                self.stop.store(true, std::sync::atomic::Ordering::Release);
                self.server.take().unwrap().join().unwrap();
            }
        }
        let socket = dir.join("worker.sock");
        let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let stopping = stop.clone();
        let server_root = dir.clone();
        let local_api = LocalApi {
            stop,
            server: Some(std::thread::spawn(move || {
                while !stopping.load(std::sync::atomic::Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut client, _)) => {
                            client.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
                            client.set_write_timeout(Some(Duration::from_millis(100))).unwrap();
                            let mut request = [0; 4096];
                            if client.read(&mut request).is_err() { continue; }
                            let state = if server_root.join("cli-started").exists()
                                && server_root.join("daemon-pid").exists()
                            {
                                "Running"
                            } else {
                                "Starting"
                            };
                            let body = format!("HTTP/1.0 200 OK\r\n\r\n{{\"BackendState\":\"{state}\"}}");
                            // Cancellation may close an accepted request before its reply.
                            let _ = client.write_all(body.as_bytes());
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        Err(error) => panic!("generated LocalAPI accept: {error}"),
                    }
                }
            })),
        };
        std::fs::write(&daemon, "#!/bin/sh\nset -e\nd=$(dirname \"$0\")\nprintf '%s\\n' \"$$\" > \"$d/daemon-pid.tmp\"\nmv \"$d/daemon-pid.tmp\" \"$d/daemon-pid\"\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&daemon, std::fs::Permissions::from_mode(0o755)).unwrap();
        fake_cli(&dir, "#!/bin/sh\nset -e\nd=$(dirname \"$0\")\nprintf '%s\\n' \"$$\" > \"$d/cli-pid.tmp\"\nmv \"$d/cli-pid.tmp\" \"$d/cli-pid\"\n: > \"$d/cli-started\"\nexec sleep 30\n");
        let mut worker = Tailscale::at_socket(dir.clone(), dir.join("state"), false, true, dir.join("worker.sock"));
        let deadline = Instant::now() + Duration::from_secs(4);
        while worker.state().status != "Connected" && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(worker.state().status, "Connected", "a pending up command must not hide backend status");
        let cli_pid: i32 = std::fs::read_to_string(dir.join("cli-pid")).unwrap().trim().parse().unwrap();
        assert!(crate::platform::process::generated::exists(cli_pid), "up is still pending while status is Connected");
        let daemon_pid: i32 = std::fs::read_to_string(dir.join("daemon-pid")).unwrap().trim().parse().unwrap();
        assert!(crate::platform::process::generated::exists(daemon_pid), "generated daemon remains owned until disable");
        worker.set_enabled(false);
        let deadline = Instant::now() + Duration::from_secs(3);
        while worker.state().status != "Off" && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(worker.state().status, "Off");
        worker.stop();
        for pid in [cli_pid, daemon_pid] {
            assert_eq!(crate::platform::process::generated::wait_nonblocking(pid).unwrap_err().raw_os_error(), Some(libc::ECHILD));
        }
        drop(local_api);
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[cfg(test)]
#[path = "tailscale_retirement_tests.rs"]
mod retirement_tests;

#[cfg(test)]
#[path = "tailscale_fixture_tests.rs"]
mod fixture_tests;

#[cfg(test)]
fn generated_wait<T>(future:impl Future<Output=T>)->Option<T> {
    Some(tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(future))
}
#[cfg(test)]
#[path="tailscale_async_tests.rs"]
mod async_tests;
