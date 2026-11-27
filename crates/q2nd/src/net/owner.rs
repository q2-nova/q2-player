//! The Wi-Fi monitor: one task on the I/O runtime that owns the radio's
//! intent and performs every operation on the supplicant and the helper.
//!
//! Operations run one at a time, in the order they were asked for: a radio
//! request (helper up/down, then the saved profiles), a join, a forget, the
//! two halves of a scan, and the monitor's own observation (status, DHCP,
//! band steering, the search policy). While one runs the task still takes
//! commands, so a newer request changes the intent at once and the running
//! operation, which checks the generation it works for, stops at its next
//! step. A request, a doze change and an owner action are preceded by one
//! observation, as the monitor thread's wakeup preceded the worker thread:
//! a link that is already up at start gets its address before the helper
//! runs. The task is the only caller of the radio machine's `transition`
//! (radio.rs); the result of an operation comes back to it as a value and it
//! applies the consequence.
//!
//! Between operations it waits for a command, a supplicant event or the
//! next status poll. With the radio meant to be down it waits for a command
//! only: nothing can change until one arrives.
use super::events::{Events, Packet};
use super::radio::{AfterHelper, Intent, Radio, Shared};
use super::search::{Link, Search, Step};
use super::{Lease, POLL_DARK, POLL_VISIBLE, WifiState, association_changed, band, kick, lease_for};
use super::{add_commands, parse_networks, parse_scan, parse_status, profile_digest, signal_percent};
use crate::config::WifiNetwork;
use std::collections::VecDeque;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

/// What the monitor needs from the system. `backend::System` is the device;
/// tests script one.
pub(super) trait Backend: Send + Sync + 'static {
    type Events: Events + 'static;
    /// One request/response exchange with wpa_supplicant; None when it does
    /// not answer.
    fn wpa(&self, cmd: &str) -> impl Future<Output = Option<String>> + Send;
    /// Runs the network helper with `action`; true when it exits 0.
    fn helper(&self, action: &'static str) -> impl Future<Output = bool> + Send;
    /// Subscribes to supplicant events; None keeps the polling fallback.
    fn attach(&self) -> impl Future<Output = Option<Self::Events>> + Send;
    /// The driver's received-signal percentage, where it has one.
    fn rx_signal(&self) -> impl Future<Output = Option<u8>> + Send;
    /// The age of the helper's first-scan hint (net/kick.rs).
    fn hint_age(&self) -> impl Future<Output = Option<Duration>> + Send;
    /// The digest of the profiles last saved to the supplicant.
    fn read_applied(&self, path: PathBuf) -> impl Future<Output = Option<String>> + Send;
    fn write_applied(&self, path: PathBuf, digest: String) -> impl Future<Output = ()> + Send;
    /// Brings the boot marker in line with `wanted`.
    fn sync_marker(&self, path: PathBuf, wanted: bool) -> impl Future<Output = ()> + Send;
}

/// What the handle asks of the monitor.
pub(super) enum Cmd {
    /// An explicit Wi-Fi or Offline choice. Answered when the intent has
    /// changed and the boot marker, if there is one, is written.
    Request { requested: bool, offline: bool, networks: Vec<WifiNetwork>, reply: oneshot::Sender<()> },
    /// The doze policy starts or ends a doze. Answered when the intent has
    /// been asked (it changes only where the machine allows).
    Doze { on: bool, reply: oneshot::Sender<()> },
    TakeFault { reply: oneshot::Sender<Option<(bool, &'static str)>> },
    Join(WifiNetwork),
    JoinSaved(String),
    Forget(String),
    Scan,
    /// The screen turned on or off: observe now.
    Wake,
    /// Keep this boot marker in step with each request.
    Autostart(PathBuf),
}

pub(super) fn log_doze(on: bool) {
    log::info!("Wi-Fi {} for cached playback", if on { "dozing" } else { "waking" });
}

pub(super) async fn wpa_ok<B: Backend>(b: &B, cmd: &str) -> bool {
    b.wpa(cmd).await.is_some_and(|r| r.trim() == "OK")
}

/// Adds or updates a network; returns its id.
pub(super) async fn ensure_network<B: Backend>(b: &B, n: &WifiNetwork) -> Option<u32> {
    let known = parse_networks(&b.wpa("LIST_NETWORKS").await?);
    let id = match known.iter().find(|(_, s)| *s == n.ssid) {
        Some((id, _)) => *id,
        None => b.wpa("ADD_NETWORK").await?.trim().parse().ok()?,
    };
    // Do not leave an older, weaker live profile enabled if an explicit SAE
    // update fails (for example when running an older WPA2-only supplicant).
    if !wpa_ok(b, &format!("DISABLE_NETWORK {id}")).await { return None; }
    let commands = add_commands(id, n);
    if commands.is_empty() {
        log::warn!("SAE Wi-Fi configuration requires a password");
        return None;
    }
    for c in commands {
        if !wpa_ok(b, &c).await {
            log::warn!("wpa_supplicant refused a setting for {}", n.ssid);
            return None;
        }
    }
    wpa_ok(b, &format!("ENABLE_NETWORK {id}")).await;
    Some(id)
}

/// Waits for wpa_supplicant's control socket after a start.
pub(super) async fn wait_for_wpa<B: Backend>(b: &B, secs: u64) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if b.wpa("PING").await.is_some_and(|r| r.trim() == "PONG") {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    false
}

/// `applied` records the profiles last saved: with the same profiles already
/// in the supplicant, nothing is sent. Re-sending them disables, re-enables
/// and reassociates, which on an early boot association (root-init) drops a
/// completed link for a full-band scan.
async fn configure_networks<B: Backend>(b: &B, shared: &Shared, generation: u64, networks: &[WifiNetwork], applied: Option<&Path>) {
    if !networks.is_empty() && wait_for_wpa(b, 10).await && shared.current(generation) {
        let digest = profile_digest(networks);
        if let Some(path) = applied {
            let known = b.wpa("LIST_NETWORKS").await.map(|t| parse_networks(&t)).unwrap_or_default();
            if b.read_applied(path.to_path_buf()).await.is_some_and(|d| d == digest)
                && networks.iter().all(|n| known.iter().any(|(_, s)| *s == n.ssid)) {
                return;
            }
        }
        let mut added = false;
        for n in networks {
            if !shared.current(generation) { return; }
            added |= ensure_network(b, n).await.is_some();
        }
        if added && shared.current(generation) {
            if wpa_ok(b, "SAVE_CONFIG").await {
                if let Some(path) = applied { b.write_applied(path.to_path_buf(), digest).await; }
            }
            wpa_ok(b, "REASSOCIATE").await;
        }
    }
}

/// Select a network (which disables the others), then let them back in for
/// later, and keep the change.
async fn select_and_save<B: Backend>(b: &B, id: u32) {
    wpa_ok(b, &format!("SELECT_NETWORK {id}")).await;
    wpa_ok(b, "ENABLE_NETWORK all").await;
    wpa_ok(b, "SAVE_CONFIG").await;
}

/// What the monitor remembers between observations.
struct Monitor<E> {
    search: Option<(u64, Search)>,
    association: Option<(u64, bool)>,
    bound: Option<(u64, String)>,
    steer: band::Steer,
    kick: kick::Kick,
    last_observation: Instant,
    events: Option<E>,
    /// A NOT-FOUND event arrived since the last observation.
    not_found: bool,
    /// When the Wi-Fi page last asked for a scan (it does on opening and on refresh).
    page_at: Option<Instant>,
}

/// The Wi-Fi page counts as open this long after its last scan request.
const PAGE_OPEN: Duration = Duration::from_secs(120);

/// Why the player asked for a scan (`ux::Mark::WifiScan`). The supplicant's own
/// scans show as `WifiEvent` scan-started marks without one of these.
#[derive(Clone, Copy)]
#[repr(u32)]
enum ScanReason {
    /// The hinted first scan (last channel only) found no saved network: all channels.
    NotFound = 1,
    /// The Wi-Fi page is open and the link is on 2.4 GHz: the 5 GHz channels, for band steering.
    BandSteer = 2,
    /// The Wi-Fi page asked.
    Page = 3,
    /// Not associated: the search policy asks the supplicant to reconnect, which scans.
    Reconnect = 4,
}
fn mark_scan(reason: ScanReason) { crate::ux::record(crate::ux::Mark::WifiScan, reason as u32); }

impl<E> Monitor<E> {
    fn new() -> Self {
        Monitor { search: None, association: None, bound: None, steer: band::Steer::default(), kick: kick::Kick::default(),
            last_observation: Instant::now(), events: None, not_found: false, page_at: None }
    }
    /// The radio is meant to be down: forget the link.
    fn reset(&mut self) {
        self.association = None;
        self.events = None;
        self.not_found = false;
        self.search = None;
        self.steer.reset();
    }
}

/// One observation of an enabled radio: status, DHCP, band steering and the
/// search policy. Returns the state to publish.
async fn observe<B: Backend>(b: &B, shared: &Shared, m: &mut Monitor<B::Events>, intent: Intent, action: bool) -> WifiState {
    // The policies take `std` instants; tokio's clock supplies them so a
    // paused test clock moves them too.
    let now = || Instant::now().into_std();
    if let Some(events) = m.events.as_ref() {
        if !events.current().await { m.events = None; m.not_found = false; }
    }
    if m.events.is_none() { m.events = b.attach().await; }
    let mut new = shared.state.lock().unwrap().clone();
    match b.wpa("STATUS").await {
        Some(text) => parse_status(&text, &mut new),
        None => {
            m.events = None;
            m.not_found = false;
            new.ssid = None;
            new.ip = None;
            new.wpa_state = "OFF".into();
        }
    }
    new.known = b.wpa("LIST_NETWORKS").await.map(|t| parse_networks(&t).into_iter().map(|(_, s)| s).collect()).unwrap_or_default();
    new.signal = match b.rx_signal().await {
        Some(signal) => signal,
        // Without the driver's file, the supplicant's RSSI.
        None => {
            let polled = if new.ssid.is_some() { b.wpa("SIGNAL_POLL").await.and_then(|t| signal_percent(&t)) } else { None };
            polled.unwrap_or(if new.ssid.is_some() { 60 } else { 0 })
        }
    };
    // Associated: get an address.
    let connected = new.wpa_state == "COMPLETED";
    let acquired = association_changed(&mut m.association, intent.generation, connected);
    let lease = lease_for(&mut m.bound, intent.generation, new.ssid.as_deref().filter(|_| connected));
    if acquired {
        crate::metrics::record(crate::metrics::Event::WifiAssociated, m.last_observation.elapsed());
        crate::record_boot_stage("wlan-associated-observed");
    }
    if shared.current(intent.generation) {
        if lease == Lease::Replace {
            log::info!("Wi-Fi joined a different network; replacing the DHCP lease");
            if b.helper("renew").await { new.ip = None; }
        } else if acquired {
            b.helper("dhcp").await;
        }
        // Prefer the network's 5 GHz BSS (band.rs), only while the Wi-Fi page is open:
        // no background scan while associated (Product feedback, 2026-11-22: full scans only when
        // the known networks cannot be found or the page is open).
        let page_open = m.page_at.is_some_and(|at| Instant::now().saturating_duration_since(at) <= PAGE_OPEN);
        match (connected, new.ssid.as_deref(), new.bssid.as_deref(), new.freq) {
            (true, Some(ssid), Some(bssid), Some(freq)) if page_open && m.steer.wants(bssid) => {
                let scan = if freq < 5000 { b.wpa("SCAN_RESULTS").await.map(|t| band::parse_bss(&t)).unwrap_or_default() } else { Vec::new() };
                match m.steer.check(now(), ssid, bssid, freq, &scan) {
                    band::Action::Roam(target) => {
                        let ok = wpa_ok(b, &format!("ROAM {target}")).await;
                        log::info!("Wi-Fi: on {freq} MHz; roaming to the network's 5 GHz BSS ({})", if ok { "requested" } else { "refused" });
                    }
                    band::Action::Scan => {
                        let ok = wpa_ok(b, band::SCAN_5GHZ).await;
                        if ok { mark_scan(ScanReason::BandSteer); }
                        log::info!("Wi-Fi: on {freq} MHz; scanning the 5 GHz channels once ({})", if ok { "requested" } else { "refused" });
                    }
                    band::Action::Stay(best) => log::info!("Wi-Fi: on {freq} MHz; no usable 5 GHz BSS of the network (strongest {})",
                        best.map_or("none seen".to_string(), |d| format!("{d} dBm"))),
                    band::Action::None => {}
                }
            }
            (false, ..) => m.steer.reset(),
            _ => {}
        }
    }
    m.last_observation = Instant::now();
    // Not associated: search on a backoff instead of letting the
    // supplicant rescan every few seconds (search.rs).
    let screen = shared.visible();
    let link = Link::from_wpa_state(&new.wpa_state);
    if m.search.as_ref().is_none_or(|(g, _)| *g != intent.generation) {
        // A previous app instance may have left the supplicant parked.
        if new.wpa_state == "DISCONNECTED" && wpa_ok(b, "RECONNECT").await { mark_scan(ScanReason::Reconnect); }
        m.search = Some((intent.generation, Search::new(now(), screen)));
    }
    let (_, policy) = m.search.as_mut().unwrap();
    if action && policy.action(now()) == Step::Seek && wpa_ok(b, "RECONNECT").await { mark_scan(ScanReason::Reconnect); }
    let step = policy.step(now(), link, screen);
    // A hinted first scan that missed: full scan now (kick.rs).
    m.kick.observe(now(), intent.generation, new.wpa_state != "OFF");
    let missed = m.events.is_some() && std::mem::take(&mut m.not_found);
    let hinted = missed && b.hint_age().await.is_some_and(|age| age <= kick::HINT_AGE);
    if m.kick.wanted(now(), intent.generation, missed, hinted, link == Link::Idle, step == Step::Park || policy.parked()) && wpa_ok(b, "SCAN").await {
        m.kick.sent(intent.generation);
        mark_scan(ScanReason::NotFound);
        log::info!("Wi-Fi: first scan found no saved network; scanning all channels");
    }
    match step {
        Step::Park => {
            if let Some(r) = b.wpa("SCAN_RESULTS").await { new.scan = parse_scan(&r); }
            if wpa_ok(b, "DISCONNECT").await { log::info!("Wi-Fi: no saved network found; next search in {} s", policy.deadline().map_or(0, |d| d.saturating_duration_since(now()).as_secs())); }
        }
        Step::Seek => { if wpa_ok(b, "RECONNECT").await { mark_scan(ScanReason::Reconnect); } }
        Step::Hold => {}
    }
    new
}

/// An operation waiting for its turn. Each carries the intent it was asked
/// under and is dropped at its turn if a newer request replaced that.
enum Op {
    /// Helper up or down for a request; `networks` are synchronized after
    /// an `up` (None for a doze, whose profiles are already saved).
    Apply { intent: Intent, networks: Option<Vec<WifiNetwork>>, applied: Option<PathBuf> },
    Join { intent: Intent, network: WifiNetwork },
    JoinSaved { intent: Intent, ssid: String },
    Forget { intent: Intent, ssid: String },
    ScanStart { intent: Intent },
    ScanResults { intent: Intent },
}

/// What a finished step hands back to the owner.
enum Done<E> {
    Helper { intent: Intent, ok: bool, networks: Option<Vec<WifiNetwork>>, applied: Option<PathBuf> },
    Restored { generation: u64, ok: bool },
    /// A radio request is complete.
    Applied,
    /// A join or forget ran.
    Plain,
    ScanStarted { intent: Intent, ok: bool, events_before: u64 },
    ScanResults { intent: Intent, text: Option<String> },
    Observed { monitor: Monitor<E>, intent: Intent, state: WifiState },
}

type Running<E> = Pin<Box<dyn Future<Output = Done<E>> + Send>>;

struct ScanWait {
    until: Instant,
    intent: Intent,
    observed: bool,
}

pub(super) struct Owner<B: Backend> {
    backend: Arc<B>,
    shared: Arc<Shared>,
    radio: Radio,
    notify: Arc<dyn Fn() + Send + Sync>,
    queue: VecDeque<Op>,
    /// The step of the operation that has the turn.
    running: Option<Running<B::Events>>,
    /// Home between observations; with the observation while one runs.
    monitor: Option<Monitor<B::Events>>,
    /// An observation is wanted as soon as the queue is empty.
    observe_due: bool,
    /// A command asked for an observation before its operation takes the
    /// turn: the monitor thread used to be woken before the worker started,
    /// so a link that is already up (the boot-time association) gets its
    /// address without waiting for the helper.
    observe_first: bool,
    /// An owner network action since the last observation (search.rs).
    action: bool,
    /// The next status poll.
    deadline: Option<Instant>,
    /// Scans whose results are due.
    scans: Vec<ScanWait>,
    /// Generic completion events aren't bound to one of our SCAN calls.
    /// A completed survey refreshes outstanding
    /// requests for the current radio intent; the result is read from the
    /// current daemon. Latch events received before SCAN's reply as well.
    scan_events: u64,
    autostart: Option<PathBuf>,
    /// Boot marker writes waiting, each with the request it answers.
    markers: VecDeque<(bool, oneshot::Sender<()>)>,
    marking: Option<(Pin<Box<dyn Future<Output = ()> + Send>>, oneshot::Sender<()>)>,
}

/// Why the task woke.
enum Wakeup<E> {
    Command(Option<Cmd>),
    Step(Done<E>),
    Marked,
    Packet(Packet),
    Timer,
}

async fn step_of<E>(running: &mut Option<Running<E>>) -> Done<E> {
    match running { Some(step) => step.as_mut().await, None => std::future::pending().await }
}

async fn marker_of(marking: &mut Option<(Pin<Box<dyn Future<Output = ()> + Send>>, oneshot::Sender<()>)>) {
    match marking { Some((write, _)) => write.as_mut().await, None => std::future::pending().await }
}

async fn packet_of<E: Events>(monitor: &mut Option<Monitor<E>>) -> Packet {
    match monitor.as_mut().and_then(|m| m.events.as_mut()) { Some(events) => events.recv().await, None => std::future::pending().await }
}

async fn timer_of(deadline: Option<Instant>) {
    match deadline { Some(at) => tokio::time::sleep_until(at).await, None => std::future::pending().await }
}

impl<B: Backend> Owner<B> {
    pub fn new(backend: B, shared: Arc<Shared>, notify: Arc<dyn Fn() + Send + Sync>) -> Self {
        Owner { backend: Arc::new(backend), radio: Radio::new(shared.clone()), shared, notify, queue: VecDeque::new(), running: None,
            monitor: Some(Monitor::new()), observe_due: true, observe_first: false, action: false, deadline: None, scans: Vec::new(), scan_events: 0,
            autostart: None, markers: VecDeque::new(), marking: None }
    }

    /// Runs until the handle is dropped.
    pub async fn run(mut self, mut commands: mpsc::UnboundedReceiver<Cmd>) {
        loop {
            self.start_work();
            let timer = self.scans.iter().map(|scan| scan.until).chain(self.deadline).min();
            let wakeup = tokio::select! {
                biased;
                command = commands.recv() => Wakeup::Command(command),
                done = step_of(&mut self.running) => Wakeup::Step(done),
                () = marker_of(&mut self.marking) => Wakeup::Marked,
                packet = packet_of(&mut self.monitor) => Wakeup::Packet(packet),
                () = timer_of(timer) => Wakeup::Timer,
            };
            match wakeup {
                Wakeup::Command(Some(command)) => self.command(command),
                Wakeup::Command(None) => return,
                Wakeup::Step(done) => { self.running = None; self.finished(done); }
                Wakeup::Marked => { if let Some((_, reply)) = self.marking.take() { let _ = reply.send(()); } }
                Wakeup::Packet(packet) => self.packet(packet),
                Wakeup::Timer => self.timers(Instant::now()),
            }
        }
    }

    fn command(&mut self, command: Cmd) {
        match command {
            Cmd::Request { requested, offline, networks, reply } => {
                let intent = self.radio.request(requested, offline);
                self.observe_first = true;
                let applied = self.autostart.as_ref().map(|p| p.with_file_name("profiles-applied"));
                self.queue.push_back(Op::Apply { intent, networks: Some(networks), applied });
                if self.autostart.is_some() { self.markers.push_back((requested && !offline, reply)); } else { let _ = reply.send(()); }
            }
            Cmd::Doze { on, reply } => {
                if let Some(intent) = self.radio.doze(on) {
                    log_doze(on);
                    self.observe_first = true;
                    self.queue.push_back(Op::Apply { intent, networks: None, applied: None });
                }
                let _ = reply.send(());
            }
            Cmd::TakeFault { reply } => { let _ = reply.send(self.radio.take_fault()); }
            Cmd::Join(network) => { self.mark_action(); self.queue.push_back(Op::Join { intent: self.radio.intent(), network }); }
            Cmd::JoinSaved(ssid) => { self.mark_action(); self.queue.push_back(Op::JoinSaved { intent: self.radio.intent(), ssid }); }
            Cmd::Forget(ssid) => self.queue.push_back(Op::Forget { intent: self.radio.intent(), ssid }),
            Cmd::Scan => {
                self.mark_action();
                if let Some(monitor) = self.monitor.as_mut() { monitor.page_at = Some(Instant::now()); }
                self.queue.push_back(Op::ScanStart { intent: self.radio.intent() });
            }
            Cmd::Wake => self.observe_due = true,
            Cmd::Autostart(path) => self.autostart = Some(path),
        }
    }

    /// Tells the search policy an owner network action is under way.
    fn mark_action(&mut self) {
        self.action = true;
        self.observe_first = true;
    }

    /// Gives the turn to the next operation, or to an observation when none
    /// waits, and starts the next boot marker write.
    fn start_work(&mut self) {
        if self.marking.is_none() {
            if let (Some((wanted, reply)), Some(path)) = (self.markers.pop_front(), self.autostart.clone()) {
                let backend = self.backend.clone();
                self.marking = Some((Box::pin(async move { backend.sync_marker(path, wanted).await }), reply));
            }
        }
        if self.running.is_none() && self.observe_first {
            self.observe_first = false;
            self.monitor_turn();
        }
        while self.running.is_none() {
            let Some(op) = self.queue.pop_front() else { break };
            self.running = self.begin(op);
        }
        if self.running.is_none() && self.observe_due {
            self.monitor_turn();
        }
    }

    /// The monitor's turn: an observation when the radio is meant to be up,
    /// else the link is forgotten and nothing is scheduled.
    fn monitor_turn(&mut self) {
        let Some(mut monitor) = self.monitor.take() else { return };
        self.observe_due = false;
        self.deadline = None;
        let intent = self.radio.intent();
        if !intent.enabled() {
            monitor.reset();
            self.monitor = Some(monitor);
            return;
        }
        let (backend, shared, action) = (self.backend.clone(), self.shared.clone(), std::mem::take(&mut self.action));
        self.running = Some(Box::pin(async move {
            let state = observe(&*backend, &shared, &mut monitor, intent, action).await;
            Done::Observed { monitor, intent, state }
        }));
    }

    /// The first step of `op`, or None when a newer request replaced the
    /// one it was asked under.
    fn begin(&mut self, op: Op) -> Option<Running<B::Events>> {
        let backend = self.backend.clone();
        match op {
            Op::Apply { intent, networks, applied } => {
                if !self.radio.current(intent.generation) { self.applied(); return None; }
                Some(Box::pin(async move {
                    let ok = backend.helper(if intent.enabled() { "up" } else { "down" }).await;
                    Done::Helper { intent, ok, networks, applied }
                }))
            }
            Op::Join { intent, network } => {
                if !intent.enabled() || !self.radio.current(intent.generation) { return None; }
                Some(Box::pin(async move {
                    if let Some(id) = ensure_network(&*backend, &network).await { select_and_save(&*backend, id).await; }
                    Done::Plain
                }))
            }
            Op::JoinSaved { intent, ssid } => {
                if !intent.enabled() || !self.radio.current(intent.generation) { return None; }
                Some(Box::pin(async move {
                    let known = backend.wpa("LIST_NETWORKS").await.map(|t| parse_networks(&t)).unwrap_or_default();
                    if let Some((id, _)) = known.iter().find(|(_, s)| *s == ssid) { select_and_save(&*backend, *id).await; }
                    Done::Plain
                }))
            }
            Op::Forget { intent, ssid } => {
                if !intent.enabled() || !self.radio.current(intent.generation) { return None; }
                Some(Box::pin(async move {
                    let known = backend.wpa("LIST_NETWORKS").await.map(|t| parse_networks(&t)).unwrap_or_default();
                    for (id, _) in known.iter().filter(|(_, s)| *s == ssid) {
                        wpa_ok(&*backend, &format!("REMOVE_NETWORK {id}")).await;
                    }
                    wpa_ok(&*backend, "SAVE_CONFIG").await;
                    Done::Plain
                }))
            }
            Op::ScanStart { intent } => {
                // A dozed or switched-off radio is not woken for a scan.
                if !self.radio.current(intent.generation) || !intent.enabled() { return None; }
                let events_before = self.scan_events;
                Some(Box::pin(async move {
                    let ok = backend.wpa("SCAN").await.is_some();
                    if ok { mark_scan(ScanReason::Page); }
                    Done::ScanStarted { intent, ok, events_before }
                }))
            }
            Op::ScanResults { intent } => {
                if !self.radio.current(intent.generation) { return None; }
                Some(Box::pin(async move { Done::ScanResults { intent, text: backend.wpa("SCAN_RESULTS").await } }))
            }
        }
    }

    /// Applies the consequence of a finished step; a radio request may go on
    /// to a further step, which keeps the turn.
    fn finished(&mut self, done: Done<B::Events>) {
        let backend = self.backend.clone();
        match done {
            Done::Helper { intent, ok, networks, applied } => {
                // The chip's power follows every successful helper run, whatever request it was for.
                if ok { self.shared.helper_succeeded(intent.enabled()); }
                match self.radio.helper_finished(intent, ok) {
                    AfterHelper::Configure => match networks {
                        Some(networks) => {
                            let shared = self.shared.clone();
                            self.running = Some(Box::pin(async move {
                                configure_networks(&*backend, &shared, intent.generation, &networks, applied.as_deref()).await;
                                Done::Applied
                            }));
                        }
                        None => self.applied(),
                    },
                    AfterHelper::RestoreUp => {
                        self.running = Some(Box::pin(async move { Done::Restored { generation: intent.generation, ok: backend.helper("up").await } }));
                    }
                    AfterHelper::Done => self.applied(),
                }
            }
            Done::Restored { generation, ok } => {
                if ok { self.shared.helper_succeeded(true); }
                self.radio.restore_finished(generation, ok);
                self.applied();
            }
            Done::Applied => self.applied(),
            Done::Plain => {}
            Done::ScanStarted { intent, ok, events_before } => {
                if ok && self.radio.current(intent.generation) {
                    let observed = events_before != self.scan_events;
                    if observed { self.queue.push_back(Op::ScanResults { intent }); }
                    // Keep the original bounded observation window: a queued
                    // background completion can precede this SCAN's result.
                    // Publish later completions too; only a window with no
                    // successful read needs a fallback when it expires.
                    self.scans.push(ScanWait { until: Instant::now() + Duration::from_secs(4), intent, observed: false });
                }
            }
            Done::ScanResults { intent, text } => {
                if let Some(text) = text {
                    for scan in &mut self.scans {
                        if scan.intent.generation == intent.generation { scan.observed = true; }
                    }
                    let mut changed = false;
                    self.radio.publish(intent.generation, || {
                        let mut state = self.shared.state.lock().unwrap();
                        let scan = parse_scan(&text);
                        changed = state.scan != scan;
                        state.scan = scan;
                    });
                    if changed { (self.notify)(); }
                }
            }
            Done::Observed { monitor, intent, state } => {
                let mut changed = false;
                self.radio.publish(intent.generation, || {
                    let mut s = self.shared.state.lock().unwrap();
                    changed = s.ssid != state.ssid || s.bssid != state.bssid || s.signal / 30 != state.signal / 30 || s.ip != state.ip
                        || s.wpa_state != state.wpa_state || s.known != state.known || s.scan != state.scan;
                    if s.ip != state.ip && state.ip.is_some() {
                        crate::record_boot_stage("wlan-address-observed");
                    }
                    *s = state;
                });
                if changed { (self.notify)(); }
                let poll = if self.shared.visible() { POLL_VISIBLE } else { POLL_DARK };
                // A kick refused with FAIL-BUSY retries soon, inside its window.
                let poll = if monitor.kick.pending() { poll.min(Duration::from_secs(1)) } else { poll };
                let wait = monitor.search.as_ref().and_then(|(_, s)| s.deadline())
                    .map_or(poll, |d| poll.min(d.saturating_duration_since(Instant::now().into_std()).max(Duration::from_millis(200))));
                self.deadline = Some(Instant::now() + wait);
                self.monitor = Some(monitor);
            }
        }
    }

    /// A radio request ended, carried out or superseded: look at the link
    /// and tell the UI.
    fn applied(&mut self) {
        self.observe_due = true;
        (self.notify)();
    }

    fn packet(&mut self, packet: Packet) {
        match packet {
            Packet::Relevant => self.observe_due = true,
            Packet::NotFound => {
                if let Some(monitor) = &mut self.monitor { monitor.not_found = true; }
                self.observe_due = true;
            }
            Packet::Other => {}
            Packet::ScanResults => {
                self.scan_events = self.scan_events.wrapping_add(1);
                let intent = self.radio.intent();
                let mut wanted = false;
                self.scans.retain_mut(|scan| {
                    if scan.intent.generation != intent.generation { return false; }
                    wanted = true;
                    true
                });
                if wanted { self.queue.push_back(Op::ScanResults { intent }); }
            }
            Packet::Lost => {
                if let Some(monitor) = &mut self.monitor { monitor.events = None; monitor.not_found = false; }
                self.observe_due = true;
            }
        }
    }

    fn timers(&mut self, now: Instant) {
        if self.deadline.is_some_and(|at| at <= now) {
            self.deadline = None;
            self.observe_due = true;
        }
        let intent = self.radio.intent();
        let mut fallback = false;
        self.scans.retain(|scan| {
            if scan.intent.generation != intent.generation { return false; }
            if scan.until > now { return true; }
            fallback |= !scan.observed;
            false
        });
        if fallback { self.queue.push_back(Op::ScanResults { intent }); }
    }
}
