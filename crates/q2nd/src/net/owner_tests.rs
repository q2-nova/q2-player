//! The monitor task against a scripted system, on a paused clock: nothing
//! here sleeps for real. The last test runs the device backend (sockets, a
//! helper process, the I/O runtime thread) against a generated supplicant.
use super::events::{Events, Packet};
use super::owner::{Backend, Cmd, Owner};
use super::radio::Shared;
use super::*;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use tokio::sync::Semaphore;

const CONNECTED: &str = "wpa_state=COMPLETED\nssid=Home\nip_address=192.0.2.7\n";

#[derive(Default)]
struct Fake {
    /// `helper ACTION` and `wpa COMMAND`, in order.
    log: Mutex<Vec<String>>,
    /// The STATUS reply; empty for a supplicant that does not answer.
    status: Mutex<String>,
    /// The helper brought the radio up: the supplicant runs and answers.
    up: AtomicBool,
    scan_results: Mutex<String>,
    scan_gate: Mutex<Option<Arc<Semaphore>>>,
    fail_scan_results: AtomicBool,
    networks: Mutex<Vec<(u32, String)>>,
    /// Results of the next helper runs; true when empty.
    helper_results: Mutex<VecDeque<bool>>,
    /// When set, each helper run waits for a permit.
    gate: Mutex<Option<Arc<Semaphore>>>,
    in_helper: AtomicBool,
    overlapped: AtomicBool,
    attaches: AtomicUsize,
    packets: Mutex<Option<mpsc::UnboundedReceiver<Packet>>>,
    applied: Mutex<Option<String>>,
    markers: Mutex<Vec<bool>>,
    marker_gate: Mutex<Option<Arc<Semaphore>>>,
    in_marker: AtomicBool,
}

impl Fake {
    fn log(&self) -> Vec<String> { self.log.lock().unwrap().clone() }
    fn count(&self, entry: &str) -> usize { self.log().iter().filter(|l| *l == entry).count() }
    fn helpers(&self) -> Vec<String> { self.log().into_iter().filter(|l| l.starts_with("helper ")).collect() }
    fn position(&self, entry: &str) -> usize { self.log().iter().position(|l| l == entry).unwrap_or_else(|| panic!("{entry} not in {:?}", self.log())) }
    fn clear(&self) { self.log.lock().unwrap().clear(); }
}

struct FakeEvents(mpsc::UnboundedReceiver<Packet>);
impl Events for FakeEvents {
    async fn current(&self) -> bool { true }
    async fn recv(&mut self) -> Packet {
        match self.0.recv().await { Some(packet) => packet, None => std::future::pending().await }
    }
}

struct Probe(Arc<Fake>);
impl Backend for Probe {
    type Events = FakeEvents;
    async fn wpa(&self, cmd: &str) -> Option<String> {
        self.0.log.lock().unwrap().push(format!("wpa {cmd}"));
        if cmd == "SCAN" {
            let gate = self.0.scan_gate.lock().unwrap().clone();
            if let Some(gate) = gate { gate.acquire().await.unwrap().forget(); }
        }
        if cmd == "SCAN_RESULTS" && self.0.fail_scan_results.swap(false, Ordering::SeqCst) { return None; }
        let status = self.0.status.lock().unwrap().clone();
        if status.is_empty() || !self.0.up.load(Ordering::SeqCst) { return None; }
        Some(match cmd {
            "PING" => "PONG\n".into(),
            "STATUS" => status,
            "LIST_NETWORKS" => {
                let mut text = String::from("network id / ssid / bssid / flags\n");
                for (id, ssid) in self.0.networks.lock().unwrap().iter() { text += &format!("{id}\t{ssid}\tany\t\n"); }
                text
            }
            "ADD_NETWORK" => {
                let mut networks = self.0.networks.lock().unwrap();
                let id = networks.len() as u32;
                networks.push((id, String::new()));
                format!("{id}\n")
            }
            "SCAN_RESULTS" => self.0.scan_results.lock().unwrap().clone(),
            "SIGNAL_POLL" => "RSSI=-60\n".into(),
            other => {
                // `SET_NETWORK id ssid HEX` names the network.
                if let Some((id, hex)) = other.strip_prefix("SET_NETWORK ").and_then(|r| r.split_once(" ssid ")) {
                    let bytes: Vec<u8> = (0..hex.len() / 2).map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap()).collect();
                    let id: u32 = id.parse().unwrap();
                    self.0.networks.lock().unwrap().iter_mut().filter(|(n, _)| *n == id).for_each(|(_, s)| *s = String::from_utf8(bytes.clone()).unwrap());
                }
                if let Some(id) = other.strip_prefix("REMOVE_NETWORK ") {
                    let id: u32 = id.parse().unwrap();
                    self.0.networks.lock().unwrap().retain(|(n, _)| *n != id);
                }
                "OK\n".into()
            }
        })
    }
    async fn helper(&self, action: &'static str) -> bool {
        self.0.log.lock().unwrap().push(format!("helper {action}"));
        if self.0.in_helper.swap(true, Ordering::SeqCst) { self.0.overlapped.store(true, Ordering::SeqCst); }
        let gate = self.0.gate.lock().unwrap().clone();
        if let Some(gate) = gate { gate.acquire().await.unwrap().forget(); }
        self.0.in_helper.store(false, Ordering::SeqCst);
        let ok = self.0.helper_results.lock().unwrap().pop_front().unwrap_or(true);
        match action {
            "up" if ok => self.0.up.store(true, Ordering::SeqCst),
            "down" if ok => self.0.up.store(false, Ordering::SeqCst),
            _ => {}
        }
        ok
    }
    async fn attach(&self) -> Option<FakeEvents> {
        self.0.attaches.fetch_add(1, Ordering::SeqCst);
        if !self.0.up.load(Ordering::SeqCst) { return None; }
        self.0.packets.lock().unwrap().take().map(FakeEvents)
    }
    async fn rx_signal(&self) -> Option<u8> { None }
    async fn hint_age(&self) -> Option<Duration> { None }
    async fn read_applied(&self, _: PathBuf) -> Option<String> { self.0.applied.lock().unwrap().clone() }
    async fn write_applied(&self, _: PathBuf, digest: String) { *self.0.applied.lock().unwrap() = Some(digest); }
    async fn sync_marker(&self, _: PathBuf, wanted: bool) {
        self.0.in_marker.store(true, Ordering::SeqCst);
        let gate = self.0.marker_gate.lock().unwrap().clone();
        if let Some(gate) = gate { gate.acquire().await.unwrap().forget(); }
        // A write that takes time: the request must not be answered before it.
        tokio::time::sleep(Duration::from_millis(5)).await;
        self.0.markers.lock().unwrap().push(wanted);
    }
}

struct Rig {
    fake: Arc<Fake>,
    commands: mpsc::UnboundedSender<Cmd>,
    shared: Arc<Shared>,
    notified: Arc<AtomicUsize>,
    packets: mpsc::UnboundedSender<Packet>,
}

impl Rig {
    fn new() -> Rig {
        let (packets, incoming) = mpsc::unbounded_channel();
        let fake = Arc::new(Fake { status: Mutex::new(CONNECTED.into()), packets: Mutex::new(Some(incoming)), ..Default::default() });
        let shared = Shared::new(false, WifiState::default());
        let notified = Arc::new(AtomicUsize::new(0));
        let count = notified.clone();
        let (commands, inbox) = mpsc::unbounded_channel();
        tokio::spawn(Owner::new(Probe(fake.clone()), shared.clone(), Arc::new(move || { count.fetch_add(1, Ordering::SeqCst); })).run(inbox));
        Rig { fake, commands, shared, notified, packets }
    }
    async fn request(&self, requested: bool, offline: bool, networks: &[WifiNetwork]) {
        let (reply, answer) = oneshot::channel();
        self.commands.send(Cmd::Request { requested, offline, networks: networks.to_vec(), reply }).ok().unwrap();
        answer.await.unwrap();
    }
    async fn doze(&self, on: bool) {
        let (reply, answer) = oneshot::channel();
        self.commands.send(Cmd::Doze { on, reply }).ok().unwrap();
        answer.await.unwrap();
    }
    async fn take_fault(&self) -> Option<(bool, &'static str)> {
        let (reply, answer) = oneshot::channel();
        self.commands.send(Cmd::TakeFault { reply }).ok().unwrap();
        answer.await.unwrap()
    }
    fn tell(&self, command: Cmd) { self.commands.send(command).ok().unwrap(); }
    fn state(&self) -> WifiState { self.shared.state.lock().unwrap().clone() }
}

/// Lets the monitor run everything that is ready, without moving the clock.
async fn settle() {
    for _ in 0..200 { tokio::task::yield_now().await; }
}

fn home() -> WifiNetwork { WifiNetwork { ssid: "Home".into(), psk: "generated-password".into(), ..Default::default() } }

#[tokio::test(start_paused = true)]
async fn a_request_runs_the_helper_then_the_profiles_then_observes_and_publishes() {
    let rig = Rig::new();
    settle().await;
    assert!(rig.fake.log().is_empty(), "a radio that is off is left alone");
    rig.request(true, false, &[home()]).await;
    assert!(rig.shared.intent().enabled(), "the intent changed before the request was answered");
    settle().await;
    let f = &rig.fake;
    assert_eq!(f.helpers(), ["helper up", "helper dhcp"]);
    assert!(f.position("helper up") < f.position("wpa PING"));
    assert!(f.position("wpa PING") < f.position("wpa ADD_NETWORK"));
    assert!(f.position("wpa ADD_NETWORK") < f.position("wpa SAVE_CONFIG"));
    assert!(f.position("wpa SAVE_CONFIG") < f.position("wpa REASSOCIATE"));
    let last_status = f.log().iter().rposition(|l| l == "wpa STATUS").unwrap();
    assert!(f.position("wpa STATUS") < f.position("helper up"), "one look at the link before the helper");
    assert!(f.position("wpa REASSOCIATE") < last_status, "an observation follows the request");
    assert!(last_status < f.position("helper dhcp"));
    assert!(f.log().contains(&"wpa SET_NETWORK 0 ssid 486f6d65".to_string()));
    let state = rig.state();
    assert_eq!((state.ssid.as_deref(), state.ip.as_deref(), state.signal), (Some("Home"), Some("192.0.2.7"), 80));
    assert_eq!(state.known, ["Home"]);
    assert!(rig.notified.load(Ordering::SeqCst) >= 2, "after the request and after the new status");
    assert_eq!(f.attaches.load(Ordering::SeqCst), 2, "refused while the supplicant was down, then attached");
    assert!(!f.overlapped.load(Ordering::SeqCst));
}

/// The threaded test this replaces held the old operation lock; here the
/// queue gives the order.
#[tokio::test(start_paused = true)]
async fn an_explicit_choice_supersedes_an_inflight_down_failure_and_helpers_never_overlap() {
    let rig = Rig::new();
    rig.request(true, false, &[]).await;
    settle().await;
    rig.fake.clear();
    let gate = Arc::new(Semaphore::new(0));
    *rig.fake.gate.lock().unwrap() = Some(gate.clone());
    rig.fake.helper_results.lock().unwrap().push_back(false);
    rig.request(true, true, &[]).await;
    settle().await;
    assert_eq!(rig.fake.helpers(), ["helper down"]);
    // The helper is still running; the monitor answers all the same.
    rig.request(true, false, &[]).await;
    assert!(rig.shared.intent().enabled());
    settle().await;
    assert_eq!(rig.fake.helpers(), ["helper down"], "the new helper cannot overlap the old one");
    gate.add_permits(8);
    settle().await;
    // The refused `down` left the radio up; the stale failure runs no
    // restore helper, and the new request's `up` runs once.
    assert_eq!((rig.fake.count("helper down"), rig.fake.count("helper up")), (1, 1), "{:?}", rig.fake.helpers());
    assert!(rig.fake.position("helper down") < rig.fake.position("helper up"));
    assert!(!rig.fake.overlapped.load(Ordering::SeqCst));
    assert!(rig.shared.intent().enabled());
    assert!(!rig.shared.intent().offline);
    assert!(!rig.shared.fault_pending());
    assert!(rig.take_fault().await.is_none());
}

#[tokio::test(start_paused = true)]
async fn a_failed_offline_down_rolls_back_restores_the_radio_and_reports_once() {
    for restore_ok in [true, false] {
        let rig = Rig::new();
        rig.request(true, false, &[]).await;
        settle().await;
        rig.fake.clear();
        let before = rig.notified.load(Ordering::SeqCst);
        rig.fake.helper_results.lock().unwrap().extend([false, restore_ok]);
        let gate = Arc::new(Semaphore::new(0));
        *rig.fake.gate.lock().unwrap() = Some(gate.clone());
        rig.request(true, true, &[]).await;
        assert!(rig.shared.intent().offline, "Offline while the helper runs");
        gate.add_permits(8);
        settle().await;
        assert_eq!(rig.fake.helpers()[..2], ["helper down", "helper up"]);
        assert!(!rig.shared.intent().offline, "Offline Mode is rolled back");
        assert!(rig.shared.intent().requested);
        assert!(rig.notified.load(Ordering::SeqCst) > before);
        assert!(rig.shared.fault_pending());
        let (offline, message) = rig.take_fault().await.unwrap();
        assert!(!offline);
        assert_eq!(message.contains("restore failed"), !restore_ok);
        assert!(!rig.shared.fault_pending());
        assert!(rig.take_fault().await.is_none());
        assert!(rig.shared.intent().error.is_some(), "the row's notice survives the toast");
        assert!(!rig.fake.log().iter().any(|l| l == "wpa PING"), "no profile writes for an Offline request");
    }
}

/// The image may have started the supplicant before the app (the boot
/// marker): the address must not wait for the app's own helper run.
#[tokio::test(start_paused = true)]
async fn a_link_already_up_at_start_gets_its_address_before_the_helper_runs() {
    let rig = Rig::new();
    rig.fake.up.store(true, Ordering::SeqCst);
    rig.request(true, false, &[home()]).await;
    settle().await;
    assert_eq!(rig.fake.helpers(), ["helper dhcp", "helper up"]);
    assert!(rig.fake.position("wpa STATUS") < rig.fake.position("helper dhcp"));
    assert_eq!(rig.state().ip.as_deref(), Some("192.0.2.7"));
    assert_eq!(rig.fake.count("helper dhcp"), 1, "the observation after the helper finds the same association");
}

#[tokio::test(start_paused = true)]
async fn doze_runs_the_helper_only_and_a_failed_doze_is_rolled_back_silently() {
    let rig = Rig::new();
    rig.request(true, false, &[home()]).await;
    settle().await;
    assert_eq!(rig.shared.power_ups(), 0, "the first up powers a chip that was never taken down");
    rig.fake.clear();
    rig.doze(true).await;
    assert!(rig.shared.intent().doze);
    rig.doze(true).await;
    settle().await;
    assert_eq!(rig.fake.log(), ["helper down"], "one helper run, no profile work, no status while down");
    assert_eq!(rig.shared.power_ups(), 0, "down only");
    rig.doze(false).await;
    settle().await;
    assert_eq!(rig.fake.helpers(), ["helper down", "helper up", "helper dhcp"], "the link is new after a doze: DHCP again");
    assert!(!rig.fake.log().iter().any(|l| l == "wpa PING" || l.starts_with("wpa SET_NETWORK")), "the saved profiles are not sent again");
    assert_eq!(rig.shared.power_ups(), 1, "the wake powered the chip up again: Bluetooth is told");
    rig.fake.clear();
    rig.fake.helper_results.lock().unwrap().push_back(false);
    rig.doze(true).await;
    settle().await;
    assert_eq!(rig.fake.helpers()[..2], ["helper down", "helper up"], "down fails, then the link is brought back");
    assert!(!rig.shared.intent().doze);
    assert!(rig.shared.intent().enabled());
    assert!(rig.take_fault().await.is_none(), "nothing the owner asked for failed");
    assert_eq!(rig.shared.power_ups(), 1, "a failed down left the chip powered: no power-up to report");
    // Offline never dozes.
    rig.request(true, true, &[]).await;
    rig.doze(true).await;
    assert!(!rig.shared.intent().doze);
}

#[tokio::test(start_paused = true)]
async fn status_polls_follow_the_screen_and_stop_while_the_radio_is_down() {
    let rig = Rig::new();
    rig.request(true, false, &[]).await;
    settle().await;
    let polls = || rig.fake.count("wpa STATUS");
    let start = polls();
    tokio::time::sleep(Duration::from_millis(9_500)).await;
    assert_eq!(polls() - start, 3, "every three seconds with the screen on");
    // Screen off: one observation at once, then every thirty seconds.
    assert!(rig.shared.set_visible(false));
    rig.tell(Cmd::Wake);
    settle().await;
    assert_eq!(polls() - start, 4);
    tokio::time::sleep(Duration::from_secs(61)).await;
    assert_eq!(polls() - start, 6);
    assert_eq!(rig.fake.helpers(), ["helper up", "helper dhcp"], "DHCP once per association");
    // Radio down: the helper, then nothing at all until a command.
    rig.request(false, false, &[]).await;
    settle().await;
    rig.fake.clear();
    tokio::time::sleep(Duration::from_secs(3600)).await;
    assert!(rig.fake.log().is_empty(), "an hour with Wi-Fi off: {:?}", rig.fake.log());
    assert!(rig.state().ssid.is_some(), "the last status stays; the handle hides it while the radio is down");
    // And back.
    assert!(rig.shared.set_visible(true));
    rig.request(true, false, &[]).await;
    settle().await;
    assert_eq!(rig.fake.helpers(), ["helper up", "helper dhcp"], "a new generation needs DHCP again");
}

#[tokio::test(start_paused = true)]
async fn connectivity_events_cause_status_work_at_once_and_other_events_none() {
    let rig = Rig::new();
    rig.request(true, false, &[]).await;
    settle().await;
    let polls = || rig.fake.count("wpa STATUS");
    let start = polls();
    rig.packets.send(Packet::Other).unwrap();
    rig.packets.send(Packet::ScanResults).unwrap();
    settle().await;
    assert_eq!(polls(), start, "scan and signal events cost no status exchange");
    rig.packets.send(Packet::Relevant).unwrap();
    settle().await;
    assert_eq!(polls(), start + 1, "no clock time passed");
    rig.packets.send(Packet::NotFound).unwrap();
    settle().await;
    assert_eq!(polls(), start + 2);
    assert_eq!(rig.fake.count("wpa SCAN"), 0, "no hint file: the supplicant's own scan covered every channel");
    // A lost socket: the next observation attaches again (and finds none).
    rig.packets.send(Packet::Lost).unwrap();
    settle().await;
    assert_eq!(polls(), start + 3);
    assert_eq!(rig.fake.attaches.load(Ordering::SeqCst), 3, "down, attached, and again after the loss");
    // Polling carries on without events.
    tokio::time::sleep(Duration::from_millis(3_100)).await;
    assert_eq!(polls(), start + 4);
}

#[tokio::test(start_paused = true)]
async fn a_supplicant_that_does_not_answer_reads_as_off_and_is_polled_again() {
    let rig = Rig::new();
    rig.fake.status.lock().unwrap().clear();
    rig.request(true, false, &[]).await;
    settle().await;
    let state = rig.state();
    assert_eq!((state.ssid, state.ip, state.wpa_state.as_str()), (None, None, "OFF"));
    assert_eq!(rig.fake.helpers(), ["helper up"]);
    *rig.fake.status.lock().unwrap() = CONNECTED.into();
    tokio::time::sleep(Duration::from_millis(3_100)).await;
    assert_eq!(rig.state().ssid.as_deref(), Some("Home"));
    assert_eq!(rig.fake.helpers(), ["helper up", "helper dhcp"]);
}

#[tokio::test(start_paused = true)]
async fn a_different_network_in_the_same_generation_replaces_the_lease() {
    let rig = Rig::new();
    rig.request(true, false, &[]).await;
    settle().await;
    *rig.fake.status.lock().unwrap() = "wpa_state=COMPLETED\nssid=Fixture\nip_address=192.0.2.7\n".into();
    tokio::time::sleep(Duration::from_millis(3_100)).await;
    assert_eq!(rig.fake.helpers(), ["helper up", "helper dhcp", "helper renew"]);
    let state = rig.state();
    assert_eq!(state.ssid.as_deref(), Some("Fixture"));
    assert!(state.ip.is_none(), "the old address is not shown for the new network");
    tokio::time::sleep(Duration::from_millis(3_100)).await;
    assert_eq!(rig.fake.helpers().len(), 3, "once per change");
}

#[tokio::test(start_paused = true)]
async fn scan_completion_publishes_without_fixed_delay_or_extra_status_work() {
    let rig = Rig::new();
    *rig.fake.scan_results.lock().unwrap() = "bssid / frequency / signal level / flags / ssid\naa:bb:cc:dd:ee:01\t5180\t-50\t[WPA2-PSK-CCMP][ESS]\tGenerated\n".into();
    rig.request(true, false, &[]).await;
    settle().await;
    rig.tell(Cmd::Scan);
    settle().await;
    let status_calls = rig.fake.count("wpa STATUS");
    let now = tokio::time::Instant::now();
    rig.packets.send(Packet::ScanResults).unwrap();
    settle().await;
    assert_eq!(rig.state().scan[0].ssid, "Generated");
    assert_eq!(tokio::time::Instant::now(), now, "completion needs no timer advance");
    assert_eq!(rig.fake.count("wpa STATUS"), status_calls);
    assert_eq!(rig.fake.count("wpa SCAN_RESULTS"), 1);
    tokio::time::advance(Duration::from_millis(4100)).await;
    settle().await;
    assert_eq!(rig.fake.count("wpa SCAN_RESULTS"), 1, "completion retires the fallback");
    rig.tell(Cmd::Scan);
    settle().await;
    rig.request(true, false, &[]).await;
    rig.packets.send(Packet::ScanResults).unwrap();
    settle().await;
    assert_eq!(rig.fake.count("wpa SCAN_RESULTS"), 1, "superseded radio intent cannot publish a scan");
}

#[tokio::test(start_paused = true)]
async fn a_failed_event_result_read_keeps_the_bounded_scan_fallback() {
    let rig = Rig::new();
    *rig.fake.scan_results.lock().unwrap() = "bssid / frequency / signal level / flags / ssid\naa:bb:cc:dd:ee:01\t5180\t-50\t[WPA2-PSK-CCMP][ESS]\tGenerated\n".into();
    rig.request(true, false, &[]).await;
    settle().await;
    rig.tell(Cmd::Scan);
    settle().await;
    rig.fake.fail_scan_results.store(true, Ordering::SeqCst);
    rig.packets.send(Packet::ScanResults).unwrap();
    settle().await;
    assert_eq!(rig.fake.count("wpa SCAN_RESULTS"), 1);
    assert!(rig.state().scan.is_empty());
    tokio::time::advance(Duration::from_millis(4100)).await;
    settle().await;
    assert_eq!(rig.fake.count("wpa SCAN_RESULTS"), 2);
    assert_eq!(rig.state().scan[0].ssid, "Generated");
}

#[tokio::test(start_paused = true)]
async fn scan_completion_before_request_reply_is_not_lost() {
    let rig = Rig::new();
    *rig.fake.scan_results.lock().unwrap() = "bssid / frequency / signal level / flags / ssid\naa:bb:cc:dd:ee:01\t5180\t-50\t[WPA2-PSK-CCMP][ESS]\tGenerated\n".into();
    rig.request(true, false, &[]).await;
    settle().await;
    let gate = Arc::new(Semaphore::new(0));
    *rig.fake.scan_gate.lock().unwrap() = Some(gate.clone());
    rig.tell(Cmd::Scan);
    settle().await;
    assert_eq!(rig.fake.count("wpa SCAN"), 1);
    rig.packets.send(Packet::ScanResults).unwrap();
    settle().await;
    assert_eq!(rig.fake.count("wpa SCAN_RESULTS"), 0);
    gate.add_permits(1);
    settle().await;
    assert_eq!(rig.fake.count("wpa SCAN_RESULTS"), 1);
    assert_eq!(rig.state().scan[0].ssid, "Generated");
    // An earlier background completion must not consume the observation
    // window and hide the newly requested scan's later result.
    *rig.fake.scan_results.lock().unwrap() = "bssid / frequency / signal level / flags / ssid\naa:bb:cc:dd:ee:02\t5180\t-50\t[WPA2-PSK-CCMP][ESS]\tLater survey\n".into();
    rig.packets.send(Packet::ScanResults).unwrap();
    settle().await;
    assert_eq!(rig.fake.count("wpa SCAN_RESULTS"), 2);
    assert_eq!(rig.state().scan[0].ssid, "Later survey");
    tokio::time::advance(Duration::from_millis(4100)).await;
    settle().await;
    assert_eq!(rig.fake.count("wpa SCAN_RESULTS"), 2, "an observed window needs no final fallback read");
}

#[tokio::test(start_paused = true)]
async fn network_state_and_saved_profile_changes_notify_without_address_changes() {
    let rig = Rig::new();
    *rig.fake.status.lock().unwrap() = "wpa_state=DISCONNECTED\n".into();
    rig.request(true, false, &[]).await;
    settle().await;
    let before = rig.notified.load(Ordering::SeqCst);
    *rig.fake.status.lock().unwrap() = "wpa_state=SCANNING\n".into();
    rig.tell(Cmd::Wake);
    settle().await;
    assert_eq!(rig.state().wpa_state, "SCANNING");
    assert!(rig.notified.load(Ordering::SeqCst) > before);
    let before = rig.notified.load(Ordering::SeqCst);
    rig.fake.networks.lock().unwrap().push((7, "Generated saved network".into()));
    rig.tell(Cmd::Wake);
    settle().await;
    assert!(rig.state().known.iter().any(|name| name == "Generated saved network"));
    assert!(rig.notified.load(Ordering::SeqCst) > before);
    let before = rig.notified.load(Ordering::SeqCst);
    rig.tell(Cmd::Wake);
    settle().await;
    assert_eq!(rig.notified.load(Ordering::SeqCst), before, "unchanged publications add no UI wake");
}

#[tokio::test(start_paused = true)]
async fn a_scan_publishes_its_results_four_seconds_later_for_the_same_request_only() {
    let rig = Rig::new();
    *rig.fake.scan_results.lock().unwrap() = "bssid / frequency / signal level / flags / ssid\naa:bb:cc:dd:ee:01\t5180\t-50\t[WPA2-PSK-CCMP][ESS]\tHome\n".into();
    rig.request(true, false, &[]).await;
    settle().await;
    rig.tell(Cmd::Scan);
    settle().await;
    assert_eq!(rig.fake.count("wpa SCAN"), 1);
    assert_eq!(rig.fake.count("wpa SCAN_RESULTS"), 0);
    let notifications = rig.notified.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(3_900)).await;
    assert!(rig.state().scan.is_empty());
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(rig.fake.count("wpa SCAN_RESULTS"), 1);
    assert_eq!(rig.state().scan.len(), 1);
    assert_eq!(rig.state().scan[0].ssid, "Home");
    assert!(rig.notified.load(Ordering::SeqCst) > notifications, "scan publication wakes the UI owner");
    // A scan whose request was replaced before the results were due.
    rig.shared.state.lock().unwrap().scan.clear();
    rig.tell(Cmd::Scan);
    settle().await;
    rig.request(true, false, &[]).await;
    tokio::time::sleep(Duration::from_millis(4_100)).await;
    assert_eq!(rig.fake.count("wpa SCAN_RESULTS"), 1, "results of a superseded scan are not read");
    assert!(rig.state().scan.is_empty());
}

#[tokio::test(start_paused = true)]
async fn join_and_forget_run_in_order_for_the_current_request_only() {
    let rig = Rig::new();
    rig.request(true, false, &[]).await;
    settle().await;
    rig.fake.clear();
    rig.tell(Cmd::Join(home()));
    rig.tell(Cmd::JoinSaved("Home".into()));
    rig.tell(Cmd::Forget("Home".into()));
    settle().await;
    let f = &rig.fake;
    let selects: Vec<usize> = f.log().iter().enumerate().filter(|(_, l)| *l == "wpa SELECT_NETWORK 0").map(|(i, _)| i).collect();
    assert_eq!(selects.len(), 2, "the join and the saved join: {:?}", f.log());
    assert!(f.position("wpa ADD_NETWORK") < selects[0]);
    assert!(selects[1] < f.position("wpa REMOVE_NETWORK 0"));
    assert_eq!(f.count("wpa SAVE_CONFIG"), 3);
    assert!(f.networks.lock().unwrap().is_empty());
    // Asked under a request that is then replaced: dropped at their turn.
    f.clear();
    let gate = Arc::new(Semaphore::new(0));
    *f.gate.lock().unwrap() = Some(gate.clone());
    rig.request(true, true, &[]).await;
    rig.tell(Cmd::Join(home()));
    rig.tell(Cmd::Forget("Home".into()));
    gate.add_permits(8);
    settle().await;
    assert!(!f.log().iter().any(|l| l.contains("NETWORK")), "{:?}", f.log());
}

#[tokio::test(start_paused = true)]
async fn saved_profiles_are_sent_once_and_the_boot_marker_is_written_before_the_answer() {
    let rig = Rig::new();
    rig.tell(Cmd::Autostart(PathBuf::from("/generated/autostart")));
    rig.request(true, false, &[home()]).await;
    assert_eq!(*rig.fake.markers.lock().unwrap(), [true], "written when the request returns");
    settle().await;
    assert_eq!(rig.fake.count("wpa REASSOCIATE"), 1);
    assert_eq!(rig.fake.applied.lock().unwrap().as_deref(), Some(profile_digest(&[home()]).as_str()));
    // The same profiles again: nothing is sent, so a completed link stays.
    rig.fake.clear();
    rig.request(true, false, &[home()]).await;
    settle().await;
    assert!(!rig.fake.log().iter().any(|l| l.starts_with("wpa SET_NETWORK") || l == "wpa REASSOCIATE"), "{:?}", rig.fake.log());
    // A changed passphrase is sent.
    rig.request(true, false, &[WifiNetwork { psk: "generated-password-2".into(), ..home() }]).await;
    settle().await;
    assert_eq!(rig.fake.count("wpa REASSOCIATE"), 1);
    // The marker follows explicit requests, not a doze.
    rig.doze(true).await;
    rig.request(true, true, &[]).await;
    rig.request(false, false, &[]).await;
    assert_eq!(*rig.fake.markers.lock().unwrap(), [true, true, true, false, false]);
}

pub(super) fn task_for_ui_test() -> Net {
    let fake = Arc::new(Fake { status: Mutex::new(CONNECTED.into()), ..Default::default() });
    Net::with_backend(Probe(fake), Arc::new(|| {}))
}

/// The real owner/channel path, driven by the same current-thread UI runtime
/// as the player. Only the helper/supplicant backend is generated.
#[tokio::test(start_paused = true)]
async fn an_associated_radio_is_not_scanned_in_the_background_only_while_the_wifi_page_is_open() {
    let rig = Rig::new();
    *rig.fake.status.lock().unwrap() = "wpa_state=COMPLETED\nssid=Home\nbssid=02:00:00:00:00:01\nfreq=2437\nip_address=192.0.2.7\n".into();
    rig.request(true, false, &[home()]).await;
    settle().await;
    // An hour on 2.4 GHz with no 5 GHz BSS cached: band steering would scan, but the page is closed.
    for _ in 0..120 { tokio::time::advance(Duration::from_secs(30)).await; rig.tell(Cmd::Wake); settle().await; }
    let scans = |f: &Fake| f.log().iter().filter(|l| l.starts_with("wpa SCAN") && !l.starts_with("wpa SCAN_RESULTS")).count();
    assert_eq!(scans(&rig.fake), 0, "{:?}", rig.fake.log().iter().filter(|l| l.starts_with("wpa SCAN")).collect::<Vec<_>>());
    // The page opens: its own scan, and band steering may look at 5 GHz while it stays open.
    rig.fake.clear();
    rig.tell(Cmd::Scan);
    settle().await;
    tokio::time::advance(Duration::from_secs(3)).await;
    rig.tell(Cmd::Wake);
    settle().await;
    assert_eq!(rig.fake.count("wpa SCAN"), 1);
    assert_eq!(rig.fake.count(&format!("wpa {}", band::SCAN_5GHZ)), 1, "{:?}", rig.fake.log());
    // Dozed, a page scan does not wake the radio.
    rig.doze(true).await;
    settle().await;
    rig.fake.clear();
    rig.tell(Cmd::Scan);
    settle().await;
    assert!(rig.fake.log().iter().all(|l| !l.starts_with("wpa SCAN") && l != "helper up"), "{:?}", rig.fake.log());
}

#[test]
fn the_ui_runtime_can_drive_the_real_network_task() {
    let fake = Arc::new(Fake { status: Mutex::new(CONNECTED.into()), ..Default::default() });
    let gate = Arc::new(Semaphore::new(0));
    *fake.marker_gate.lock().unwrap() = Some(gate.clone());
    let net = Net::with_backend(Probe(fake.clone()), Arc::new(|| {}))
        .with_autostart(PathBuf::from("/generated/autostart"));
    crate::rt::Ui::new().unwrap().block_on(async {
        // The marker deliberately yields. A UI task must be able to run
        // before the command returns, and the marker must already be written.
        let progressed = std::cell::Cell::new(false);
        tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(async {
                net.set_enabled_async(true, &[]).await;
                assert!(progressed.get(), "a UI task releases the pending marker");
            }, async {
                while !fake.in_marker.load(Ordering::SeqCst) { tokio::task::yield_now().await; }
                progressed.set(true);
                gate.add_permits(1);
            });
        }).await.unwrap();
        *fake.marker_gate.lock().unwrap() = None;
        assert_eq!(*fake.markers.lock().unwrap(), [true]);
        net.set_doze_async(true).await;
        assert!(net.dozing());
        net.set_doze_async(false).await;
        assert!(!net.dozing());
        net.set_offline_mode_async(true, true, &[]).await;
        assert!(!net.wanted());
        net.set_enabled_async(true, &[]).await;
        assert!(net.wanted());
    });
}

#[test]
fn the_ui_runtime_consumes_real_task_faults_once() {
    let fake = Arc::new(Fake { helper_results: Mutex::new(VecDeque::from([false])), ..Default::default() });
    let net = Net::with_backend(Probe(fake), Arc::new(|| {}));
    crate::rt::Ui::new().unwrap().block_on(async {
        net.set_enabled_async(false, &[]).await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while !net.shared.fault_pending() { tokio::task::yield_now().await; }
        }).await.unwrap();
        assert!(net.take_fault_async().await.is_some());
        assert!(net.take_fault_async().await.is_none());
    });
}

/// The device backend end to end: a generated supplicant on a datagram socket
/// and a generated helper script run through `tokio::process` on `io`.
#[test]
fn the_handle_drives_the_real_backend_on_the_io_runtime() {
    use std::os::unix::net::UnixDatagram;
    let dir = std::env::temp_dir().join(format!("q2nd-net-{}-{}", std::process::id(), fastrand::u32(..)));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("wifi");
    std::fs::write(&script, "echo \"$1\" >> \"$(dirname \"$0\")/calls\"\n[ \"$1\" != down ]\n").unwrap();
    let daemon = UnixDatagram::bind(dir.join("wlan0")).unwrap();
    daemon.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = stop.clone();
    let server = std::thread::spawn(move || {
        let mut bytes = [0u8; 256];
        while !stopping.load(Ordering::SeqCst) {
            let Ok((n, peer)) = daemon.recv_from(&mut bytes) else { continue };
            let reply: &[u8] = match &bytes[..n] {
                b"PING" => b"PONG\n",
                b"STATUS" => CONNECTED.as_bytes(),
                b"LIST_NETWORKS" => b"network id / ssid / bssid / flags\n0\tHome\tany\t[CURRENT]\n",
                b"SIGNAL_POLL" => b"RSSI=-55\n",
                b"DETACH" => continue,
                _ => b"OK\n",
            };
            let _ = daemon.send_to(reply, peer.as_pathname().unwrap());
        }
    });
    let notified = Arc::new(AtomicUsize::new(0));
    let count = notified.clone();
    let net = Net::with_backend(backend::System::new(script, (dir.join("wlan0"), dir.clone())), Arc::new(move || { count.fetch_add(1, Ordering::SeqCst); }));
    let until = |what: &str, done: &dyn Fn() -> bool| {
        let end = std::time::Instant::now() + Duration::from_secs(20);
        while !done() {
            assert!(std::time::Instant::now() < end, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    };
    assert!(!net.state().enabled);
    net.set_enabled(true, &[]);
    assert!(net.wanted(), "the intent is read back at once");
    until("the status", &|| net.state().ip.is_some());
    let state = net.state();
    assert_eq!((state.ssid.as_deref(), state.ip.as_deref(), state.wpa_state.as_str()), (Some("Home"), Some("192.0.2.7"), "COMPLETED"));
    assert_eq!(net.signal_bars(), 3);
    let calls = || std::fs::read_to_string(dir.join("calls")).unwrap_or_default();
    // The generated supplicant answers from the start, as after a
    // boot-time association: DHCP first, then the app's own helper run.
    until("DHCP and the helper", &|| calls() == "dhcp\nup\n");
    until("both notices", &|| notified.load(Ordering::SeqCst) >= 2);
    // The generated helper fails `down`: a helper's exit status is not
    // success, Offline Mode is rolled back and the radio restored.
    net.set_offline_mode(true, true, &[]);
    assert!(!net.wanted());
    until("the rollback", &|| net.wanted());
    until("the restore", &|| calls().starts_with("dhcp\nup\ndown\nup\n"));
    until("the fault", &|| net.take_fault().is_some_and(|(offline, _)| !offline));
    assert!(net.take_fault().is_none());
    // Doze through the handle: refused `down` again, rolled back silently.
    net.set_doze(true);
    until("the doze rollback", &|| !net.dozing());
    assert!(net.take_fault().is_none());
    // Only this client's sockets were in the directory, and none is left.
    drop(net);
    until("the sockets to go", &|| std::fs::read_dir(&dir).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().starts_with("q2nd-")));
    log::info!("io runtime: longest poll {:?}", crate::rt::io().stall().longest());
    stop.store(true, Ordering::SeqCst);
    server.join().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Wakeups of the `io` thread per dark poll cycle with the device backend
/// and a generated supplicant (no events sent). Prints the figure; run alone:
/// `cargo test -p q2nd io_wakeups_per_dark_poll -- --ignored --nocapture`.
#[test]
#[ignore = "takes about two minutes; a measurement, run by hand"]
fn io_wakeups_per_dark_poll() {
    use std::os::unix::net::UnixDatagram;
    let dir = std::env::temp_dir().join(format!("q2nd-wake-{}-{}", std::process::id(), fastrand::u32(..)));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("wifi");
    std::fs::write(&script, "exit 0\n").unwrap();
    let daemon = UnixDatagram::bind(dir.join("wlan0")).unwrap();
    daemon.set_read_timeout(Some(Duration::from_millis(50))).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let stopping = stop.clone();
    let statuses = Arc::new(AtomicUsize::new(0));
    let seen = statuses.clone();
    let server = std::thread::spawn(move || {
        let mut bytes = [0u8; 256];
        while !stopping.load(Ordering::SeqCst) {
            let Ok((n, peer)) = daemon.recv_from(&mut bytes) else { continue };
            let reply: &[u8] = match &bytes[..n] {
                b"STATUS" => { seen.fetch_add(1, Ordering::SeqCst); CONNECTED.as_bytes() }
                b"LIST_NETWORKS" => b"network id / ssid / bssid / flags\n0\tHome\tany\t[CURRENT]\n",
                b"DETACH" => continue,
                _ => b"OK\n",
            };
            let _ = daemon.send_to(reply, peer.as_pathname().unwrap());
        }
    });
    let net = Net::with_backend(backend::System::new(script, (dir.join("wlan0"), dir.clone())), Arc::new(|| {}));
    net.set_enabled(true, &[]);
    let end = std::time::Instant::now() + Duration::from_secs(20);
    while net.state().ip.is_none() { assert!(std::time::Instant::now() < end); std::thread::sleep(Duration::from_millis(5)); }
    let io = std::fs::read_dir("/proc/self/task").unwrap().flatten()
        .find(|t| std::fs::read_to_string(t.path().join("comm")).is_ok_and(|c| c.trim() == "io")).unwrap().path();
    let switches = || std::fs::read_to_string(io.join("status")).unwrap().lines()
        .find_map(|l| l.strip_prefix("voluntary_ctxt_switches:").map(|v| v.trim().parse::<u64>().unwrap())).unwrap();
    std::thread::sleep(Duration::from_secs(5));
    let (s0, c0) = (switches(), statuses.load(Ordering::SeqCst));
    std::thread::sleep(Duration::from_secs(125));
    let (s1, c1) = (switches(), statuses.load(Ordering::SeqCst));
    println!("io wakeups {} over {} polls: {:.2} per poll", s1 - s0, c1 - c0, (s1 - s0) as f64 / (c1 - c0).max(1) as f64);
    drop(net);
    stop.store(true, Ordering::SeqCst);
    server.join().unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}
