#![forbid(unsafe_code)]
//! Wi-Fi through wpa_supplicant: the network helper (`wifi`, run with `up`,
//! `down`, `dhcp`, `renew`) starts and stops the daemons, networks are added
//! over the supplicant's control socket and saved with `SAVE_CONFIG`, and the
//! helper's `udhcpc` runs once associated.
//!
//! [`Net`] is the UI's handle. Everything that waits (the control socket,
//! the helper, supplicant events, the status poll) runs in one task on the
//! I/O runtime (owner.rs), which also owns the radio's intent
//! (`q2_machines::radio`). The handle reads published copies and never
//! waits on a helper. A simulated `Net` (the simulator and tests) has no
//! task and holds the intent itself.
#![deny(clippy::disallowed_methods, clippy::disallowed_types)]

use crate::config::{WifiNetwork, WifiSecurity};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
mod backend;
mod band;
mod events;
mod kick;
mod owner;
#[cfg(test)]
mod owner_tests;
mod radio;
mod search;
mod socket_path;
#[cfg(test)]
mod radio_tests;
use owner::Cmd;
use radio::{Radio, Shared};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

const CTRL_DIR: &str = "/var/run/wpa_supplicant";
const IFACE: &str = "wlan0";
/// The driver's received-signal percentage; reading it doesn't wake the radio
/// (Q2 Pod's `WIFI_SIGNAL`).
const RX_SIGNAL: &str = "/proc/net/rtl8733bs/wlan0/rx_signal";

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScanResult {
    pub ssid: String,
    /// dBm.
    pub signal: i32,
    pub secured: bool,
    pub security: WifiSecurity,
}

#[derive(Debug, Clone, Default)]
pub struct WifiState {
    pub enabled: bool,
    /// A fixed, actionable helper fault; never backend output or credentials.
    pub error: Option<String>,
    pub ssid: Option<String>,
    pub ip: Option<String>,
    /// 0-100.
    pub signal: u8,
    pub scan: Vec<ScanResult>,
    /// Networks wpa_supplicant knows.
    pub known: Vec<String>,
    /// wpa_supplicant's state, e.g. COMPLETED, SCANNING, 4WAY_HANDSHAKE.
    pub wpa_state: String,
    /// The associated BSS and its frequency (MHz), while COMPLETED.
    pub bssid: Option<String>,
    pub freq: Option<u32>,
}

pub struct Net {
    shared: Arc<Shared>,
    driver: Driver,
    simulated: bool,
    /// Exists while the latest request is Wi-Fi On without Offline Mode:
    /// init then starts the supplicant at boot, overlapping association with
    /// the player's start. None off mainline.
    autostart: Option<PathBuf>,
}

/// Who owns the radio's intent.
enum Driver {
    /// Simulated Wi-Fi: no helper, no task; the handle owns the intent.
    Inline(Mutex<Radio>),
    /// The monitor task on the I/O runtime.
    Task(mpsc::UnboundedSender<Cmd>),
}

/// The installed image's boot marker for an early `wifi up` (os/install/root-init).
pub const AUTOSTART: &str = "/state/network/autostart";

/// Brings the marker in line with `wanted`; writes only on a change.
fn sync_autostart(_: &crate::rt::Blocking, path: &Path, wanted: bool) {
    let present = path.exists();
    let result = match (wanted, present) {
        (true, false) => std::fs::write(path, b""),
        (false, true) => std::fs::remove_file(path),
        _ => return,
    };
    if let Err(e) = result { log::warn!("Wi-Fi boot marker: {e}"); }
}

/// The same for a simulated `Net`, which has no runtime to hand the write
/// to. Only tests give a simulated `Net` a marker.
fn sync_autostart_simulated(path: &Path, wanted: bool) {
    let present = path.exists();
    let _ = match (wanted, present) {
        (true, false) => std::fs::write(path, b""),
        (false, true) => std::fs::remove_file(path),
        _ => return,
    };
}

/// wpa_supplicant status polls with the screen on and off. Association
/// itself is wpa_supplicant's job and udhcpc keeps the lease; with the screen
/// dark a slower poll only delays the status shown and the DHCP retrigger.
const POLL_VISIBLE: Duration = Duration::from_secs(3);
const POLL_DARK: Duration = Duration::from_secs(30);

/// The control socket, and where our end of the exchange is bound (wpa_supplicant
/// replies to that path). Tests point both at a wpa_supplicant in a container.
fn ctrl_paths() -> (PathBuf, PathBuf) {
    match std::env::var_os("Q2ND_WPA_CTRL") {
        Some(p) => {
            let p = PathBuf::from(p);
            let dir = p.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/tmp"));
            (p, dir)
        }
        None => (Path::new(CTRL_DIR).join(IFACE), PathBuf::from("/tmp")),
    }
}

/// `SIGNAL_POLL`'s `RSSI=` as the driver's 0-100 strength: -100 dBm is 0,
/// -50 dBm and above 100.
fn signal_percent(poll: &str) -> Option<u8> {
    let dbm: i32 = poll.lines().find_map(|l| l.strip_prefix("RSSI="))?.trim().parse().ok()?;
    Some(((dbm + 100) * 2).clamp(0, 100) as u8)
}

/// Parses `SCAN_RESULTS`: bssid, frequency, signal, flags, ssid per line.
pub fn parse_scan(text: &str) -> Vec<ScanResult> {
    let mut out: Vec<ScanResult> = Vec::new();
    for line in text.lines().skip(1) {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 5 || f[4].is_empty() {
            continue;
        }
        let r = ScanResult {
            ssid: unescape(f[4]),
            signal: f[2].parse().unwrap_or(-100),
            secured: f[3].contains("WPA") || f[3].contains("WEP") || f[3].contains("SAE"),
            security: if f[3].contains("SAE") {
                if f[3].contains("PSK") { WifiSecurity::Wpa2Wpa3 } else { WifiSecurity::Wpa3 }
            } else { WifiSecurity::Wpa2 },
        };
        match out.iter_mut().find(|o| o.ssid == r.ssid) {
            Some(o) if o.signal < r.signal => *o = r,
            Some(_) => {}
            None => out.push(r),
        }
    }
    out.sort_by(|a, b| b.signal.cmp(&a.signal));
    out
}

/// Parses `LIST_NETWORKS`: id, ssid, bssid, flags per line.
pub fn parse_networks(text: &str) -> Vec<(u32, String)> {
    text.lines()
        .skip(1)
        .filter_map(|l| {
            let mut f = l.split('\t');
            let id = f.next()?.parse().ok()?;
            Some((id, unescape(f.next()?)))
        })
        .collect()
}

/// wpa_supplicant writes SSID bytes outside printable ASCII as `\xNN`, and
/// `\n`, `\r`, `\t`, `\e`, `\"` and `\\` for those characters
/// (`printf_encode`). Works on bytes throughout: the text is another
/// process's, and an escape may be cut short or followed by any character.
fn unescape(s: &str) -> String {
    let hex = |digit: u8| (digit as char).to_digit(16).map(|value| value as u8);
    let mut bytes = Vec::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1] == b'x' {
            if let (Some(high), Some(low)) = (hex(b[i + 2]), hex(b[i + 3])) {
                bytes.push(high << 4 | low);
                i += 4;
                continue;
            }
        }
        if b[i] == b'\\' && i + 1 < b.len() {
            bytes.push(match b[i + 1] { b'n' => b'\n', b'r' => b'\r', b't' => b'\t', b'e' => 0x1b, other => other });
            i += 2;
            continue;
        }
        bytes.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// `key=value` lines of `STATUS`.
fn parse_status(text: &str, st: &mut WifiState) {
    st.ssid = None;
    st.ip = None;
    st.bssid = None;
    st.freq = None;
    st.wpa_state.clear();
    for line in text.lines() {
        match line.split_once('=') {
            Some(("wpa_state", v)) => st.wpa_state = v.to_string(),
            Some(("ssid", v)) => st.ssid = Some(unescape(v)),
            Some(("ip_address", v)) => st.ip = Some(v.to_string()),
            Some(("bssid", v)) => st.bssid = Some(v.to_string()),
            Some(("freq", v)) => st.freq = v.parse().ok(),
            _ => {}
        }
    }
    if st.wpa_state != "COMPLETED" {
        st.ssid = None;
        st.bssid = None;
        st.freq = None;
    }
}

/// The control commands that add a network: the SSID in hex (any bytes),
/// the passphrase quoted (wpa_supplicant takes it up to the last quote).
pub fn add_commands(id: u32, n: &WifiNetwork) -> Vec<String> {
    // A missing SAE password is invalid, never a request for an open profile.
    if n.psk.is_empty() && n.security != WifiSecurity::Wpa2 { return Vec::new(); }
    let hex: String = n.ssid.bytes().map(|b| format!("{b:02x}")).collect();
    let mut c = vec![format!("SET_NETWORK {id} ssid {hex}")];
    if n.psk.is_empty() {
        c.push(format!("SET_NETWORK {id} key_mgmt NONE"));
        c.push(format!("SET_NETWORK {id} ieee80211w 0"));
    } else {
        if n.security != WifiSecurity::Wpa3 {
            c.push(format!("SET_NETWORK {id} psk \"{}\"", n.psk));
        }
        match n.security {
            WifiSecurity::Wpa2 => {
                c.push(format!("SET_NETWORK {id} proto WPA RSN"));
                c.push(format!("SET_NETWORK {id} key_mgmt WPA-PSK"));
                c.push(format!("SET_NETWORK {id} ieee80211w 0"));
            }
            WifiSecurity::Wpa2Wpa3 | WifiSecurity::Wpa3 => {
                c.push(format!("SET_NETWORK {id} proto RSN"));
                c.push(format!("SET_NETWORK {id} sae_password \"{}\"", n.psk));
                // Support both SAE hunting-and-pecking and hash-to-element.
                c.push(format!("SET_NETWORK {id} sae_pwe 2"));
                let (keys, pmf) = if n.security == WifiSecurity::Wpa3 { ("SAE", 2) } else { ("SAE WPA-PSK", 1) };
                c.push(format!("SET_NETWORK {id} key_mgmt {keys}"));
                c.push(format!("SET_NETWORK {id} ieee80211w {pmf}"));
            }
        }
    }
    c
}

impl Net {
    /// The Wi-Fi interface. `script` is the helper run for up/down;
    /// `simulated` gives a fixed connected network and runs nothing (the
    /// simulator and tests). `notify` is called from the I/O runtime when the
    /// state changed and must not block. The saved choice starts as on only
    /// when simulated; the caller applies the owner's with `set_enabled_async`.
    pub fn new(script: PathBuf, simulated: bool, notify: Arc<dyn Fn() + Send + Sync>) -> Net {
        if simulated {
            // A connected simulated network needs an address too: the doze policy
            // requires both association and IP readiness. This is a synthetic
            // observation, not an address assigned to any host interface.
            let state = WifiState { enabled: true, ssid: Some("Simulator".into()), bssid: Some("02:00:00:00:00:01".into()), ip: Some("192.0.2.10".into()), signal: 80,
                wpa_state: "COMPLETED".into(), known: vec!["Simulator".into()], ..Default::default() };
            let shared = Shared::new(true, state);
            return Net { driver: Driver::Inline(Mutex::new(Radio::new(shared.clone()))), shared, simulated, autostart: None };
        }
        Self::with_backend(backend::System::new(script, ctrl_paths()), notify)
    }

    /// A real `Net` over `backend`: starts the monitor task.
    fn with_backend<B: owner::Backend>(backend: B, notify: Arc<dyn Fn() + Send + Sync>) -> Net {
        let shared = Shared::new(false, WifiState::default());
        let (commands, inbox) = mpsc::unbounded_channel();
        crate::rt::io().spawn(owner::Owner::new(backend, shared.clone(), notify).run(inbox));
        Net { shared, driver: Driver::Task(commands), simulated: false, autostart: None }
    }

    /// Sends a command and awaits its reply without blocking the UI runtime.
    /// Intent replies run between I/O steps; explicit requests also await the
    /// boot-marker write before returning.
    /// None when the monitor is gone.
    async fn ask<T>(commands: &mpsc::UnboundedSender<Cmd>, command: impl FnOnce(oneshot::Sender<T>) -> Cmd) -> Option<T> {
        let (reply, answer) = oneshot::channel();
        commands.send(command(reply)).ok()?;
        answer.await.ok()
    }

    fn tell(&self, command: Cmd) {
        if let Driver::Task(commands) = &self.driver { let _ = commands.send(command); }
    }

    /// The radio as the UI shows it. With the radio meant to be down (Wi-Fi
    /// off, Offline Mode or a doze) it reports no network, address or scan,
    /// whatever the supplicant last said. Never blocks on a helper.
    pub fn state(&self) -> WifiState {
        let intent = self.shared.intent();
        let mut state = self.shared.state.lock().unwrap().clone();
        state.enabled = intent.enabled();
        state.error = intent.error.map(str::to_owned);
        if !state.enabled {
            state.ssid = None; state.ip = None; state.signal = 0;
            state.scan.clear(); state.wpa_state = "OFF".into();
        }
        state
    }

    /// Polls every three seconds with the screen on, at once when it turns on.
    pub fn set_visible(&self, on: bool) {
        if self.shared.set_visible(on) { self.tell(Cmd::Wake); }
    }

    /// 0-3 bars for the status bar; 0 when not connected.
    pub fn signal_bars(&self) -> u8 {
        let s = self.state();
        if s.ssid.is_none() {
            return 0;
        }
        match s.signal {
            0..=29 => 1,
            30..=59 => 2,
            _ => 3,
        }
    }

    /// Turns Wi-Fi on, adding `networks` (from config.toml) if wpa_supplicant
    /// doesn't know them yet, or off.
    ///
    /// Returns once the intent has changed; the monitor applies it. This is
    /// an explicit choice: it takes a new generation of the radio's intent
    /// (`q2_machines::radio`), ends a doze, and supersedes any helper still
    /// running for an older request. A failure is reported once through
    /// [`take_fault_async`](Self::take_fault_async).
    pub async fn set_enabled_async(&self, on: bool, networks: &[WifiNetwork]) {
        // An explicit On wins over temporary Offline suppression.
        let offline = !on && self.shared.intent().offline;
        self.request_radio(on, offline, networks).await;
    }

    /// Enters or leaves Offline Mode: the radio goes down without changing
    /// the saved Wi-Fi choice `requested`, and comes back with it. Returns
    /// once the intent has changed. A failed radio-down rolls Offline back
    /// and reports through [`take_fault_async`](Self::take_fault_async), whose flag tells
    /// the caller to restore the library's Offline setting too.
    pub async fn set_offline_mode_async(&self, offline: bool, requested: bool, networks: &[WifiNetwork]) {
        self.request_radio(requested, offline, networks).await;
    }

    /// Automatic radio-down while cached audio plays (see radio.rs). Saved
    /// networks are already configured, so waking needs only `up` and DHCP.
    ///
    /// Does nothing unless the saved choice is on and Offline Mode is off,
    /// or when the doze state would not change. A failed radio-down is
    /// rolled back silently, so a caller must read [`dozing`](Self::dozing)
    /// rather than assume the request held.
    pub async fn set_doze_async(&self, on: bool) {
        match &self.driver {
            Driver::Inline(radio) => {
                if radio.lock().unwrap().doze(on).is_some() { owner::log_doze(on); }
            }
            Driver::Task(commands) => { Self::ask(commands, |reply| Cmd::Doze { on, reply }).await; }
        }
    }

    /// The radio's intent is in a doze now (the single source of truth; the
    /// library's `radio_dozing` follows it on the UI tick).
    pub fn dozing(&self) -> bool { self.shared.intent().doze }
    /// Times the combo chip powered up again after the helper powered it down
    /// (radio.rs `Shared::power_ups`); the UI passes each to Bluetooth.
    pub fn power_ups(&self) -> u32 { self.shared.power_ups() }
    /// No real Wi-Fi: the simulator and tests.
    pub fn simulated(&self) -> bool { self.simulated }

    /// A helper run as the device would report it (UI tests with the inline driver).
    #[cfg(test)]
    pub(crate) fn simulate_helper(&self, up: bool) { self.shared.helper_succeeded(up); }

    #[cfg(test)]
    pub(crate) fn simulate_address(&self, ip: Option<&str>) {
        self.shared.state.lock().unwrap().ip = ip.map(str::to_string);
    }

    #[cfg(test)]
    pub(crate) fn simulate_scan(&self, scan: Vec<ScanResult>) {
        assert!(self.simulated, "generated scan requires simulated network");
        self.shared.state.lock().unwrap().scan = scan;
    }

    #[cfg(test)]
    pub(crate) fn task_for_ui_test() -> Self { owner_tests::task_for_ui_test() }

    /// The owner's saved choice is Wi-Fi On and Offline Mode is off.
    pub fn wanted(&self) -> bool { let i = self.shared.intent(); i.requested && !i.offline }

    /// The UI consumes only a fault belonging to the latest request. Offline
    /// down failure restores library access as well as the saved radio request.
    pub async fn take_fault_async(&self) -> Option<(bool, &'static str)> {
        // The common case costs no hop to the monitor.
        if !self.shared.fault_pending() { return None; }
        match &self.driver {
            Driver::Inline(radio) => radio.lock().unwrap().take_fault(),
            Driver::Task(commands) => Self::ask(commands, |reply| Cmd::TakeFault { reply }).await.flatten(),
        }
    }

    #[cfg(test)]
    pub(crate) fn fail_current_radio_request(&self) {
        let Driver::Inline(radio) = &self.driver else { panic!("simulated Wi-Fi only") };
        let mut radio = radio.lock().unwrap();
        let intent = radio.intent();
        radio::apply_radio(&mut radio, intent, |_| false, || {});
    }

    /// Keeps `path` (normally [`AUTOSTART`]) in step with each radio request.
    pub fn with_autostart(mut self, path: PathBuf) -> Net {
        self.tell(Cmd::Autostart(path.clone()));
        self.autostart = Some(path);
        self
    }

    async fn request_radio(&self, requested: bool, offline: bool, networks: &[WifiNetwork]) {
        match &self.driver {
            Driver::Inline(radio) => {
                radio.lock().unwrap().request(requested, offline);
                if let Some(path) = &self.autostart { sync_autostart_simulated(path, requested && !offline); }
            }
            // Answered once the intent changed and the boot marker is written.
            Driver::Task(commands) => { Self::ask(commands, |reply| Cmd::Request { requested, offline, networks: networks.to_vec(), reply }).await; }
        }
    }

    #[cfg(test)]
    pub fn set_enabled(&self, on: bool, networks: &[WifiNetwork]) {
        crate::rt::Ui::new().unwrap().block_on(self.set_enabled_async(on, networks));
    }
    #[cfg(test)]
    pub fn set_offline_mode(&self, offline: bool, requested: bool, networks: &[WifiNetwork]) {
        crate::rt::Ui::new().unwrap().block_on(self.set_offline_mode_async(offline, requested, networks));
    }
    #[cfg(test)]
    pub fn set_doze(&self, on: bool) {
        crate::rt::Ui::new().unwrap().block_on(self.set_doze_async(on));
    }
    #[cfg(test)]
    pub fn take_fault(&self) -> Option<(bool, &'static str)> {
        crate::rt::Ui::new().unwrap().block_on(self.take_fault_async())
    }

    /// Joins a network now and saves it.
    pub fn join(&self, n: WifiNetwork) {
        self.tell(Cmd::Join(n));
    }

    /// Joins a network wpa_supplicant already has.
    pub fn join_saved(&self, ssid: &str) {
        self.tell(Cmd::JoinSaved(ssid.to_string()));
    }

    pub fn forget(&self, ssid: &str) {
        self.tell(Cmd::Forget(ssid.to_string()));
    }

    pub fn scan(&self) {
        if !self.shared.intent().enabled() { return; }
        if self.simulated {
            let mut s = self.shared.state.lock().unwrap();
            s.scan = vec![
                ScanResult { ssid: "Simulator".into(), signal: -40, secured: true, security: WifiSecurity::Wpa2 },
                ScanResult { ssid: "Neighbours".into(), signal: -70, secured: true, security: WifiSecurity::Wpa2 },
                ScanResult { ssid: "Café".into(), signal: -80, secured: false, security: WifiSecurity::Wpa2 },
            ];
            return;
        }
        self.tell(Cmd::Scan);
    }
}

/// What `configure_networks` would send for `networks`, as a digest. A
/// mismatch only costs one reconfiguration, so std's hasher is enough.
fn profile_digest(networks: &[WifiNetwork]) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for n in networks { n.ssid.hash(&mut h); add_commands(0, n).hash(&mut h); }
    format!("{:016x}\n", h.finish())
}

// A disable/re-enable may complete between monitor observations. An association
// from a new radio generation needs DHCP even if no disconnected poll occurred.
fn association_changed(previous: &mut Option<(u64, bool)>, generation: u64, connected: bool) -> bool {
    let acquired = connected && *previous != Some((generation, true));
    *previous = Some((generation, connected));
    acquired
}

/// What the address owner must do for one observation.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Lease { Keep, Replace }

// The lease belongs to the network it was obtained on. udhcpc does not see
// wpa_supplicant change networks (after a resume or a roam it may join another
// saved network without an observed disconnect), and keeps the old address and
// route until a renewal that can be hours away, so nothing gets through. Track
// the network the current client bound on, per radio generation; a different
// network in the same generation replaces the lease. A new generation starts
// from `down`, which already stopped the client, so ordinary `dhcp` applies.
fn lease_for(bound: &mut Option<(u64, String)>, generation: u64, ssid: Option<&str>) -> Lease {
    let Some(ssid) = ssid else { return Lease::Keep };
    let replace = matches!(bound, Some((g, s)) if *g == generation && s != ssid);
    *bound = Some((generation, ssid.to_string()));
    if replace { Lease::Replace } else { Lease::Keep }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_follows_the_network_it_was_bound_on() {
        let mut bound = None;
        // First association of a generation: ordinary DHCP, nothing to replace.
        assert_eq!(lease_for(&mut bound, 1, Some("Home")), Lease::Keep);
        // Same network again, including after a disconnected observation.
        assert_eq!(lease_for(&mut bound, 1, None), Lease::Keep);
        assert_eq!(lease_for(&mut bound, 1, Some("Home")), Lease::Keep);
        // Another saved network in the same generation, with or without an
        // observed disconnect in between: the old lease cannot route here.
        assert_eq!(lease_for(&mut bound, 1, Some("Fixture")), Lease::Replace);
        assert_eq!(lease_for(&mut bound, 1, None), Lease::Keep);
        assert_eq!(lease_for(&mut bound, 1, Some("Home")), Lease::Replace);
        assert_eq!(lease_for(&mut bound, 1, Some("Home")), Lease::Keep);
        // A new radio generation was brought up by `down`/`up`: ordinary DHCP.
        assert_eq!(lease_for(&mut bound, 2, Some("Fixture")), Lease::Keep);
        assert_eq!(lease_for(&mut bound, 2, Some("Fixture")), Lease::Keep);
    }

    /// wpa_supplicant's `printf_encode` (src/utils/common.c): how an SSID's
    /// bytes appear in scan results, the network list and status.
    fn supplicant_text(ssid: &[u8]) -> String {
        let mut text = String::new();
        for &byte in ssid {
            match byte {
                b'"' => text.push_str("\\\""),
                b'\\' => text.push_str("\\\\"),
                0x1b => text.push_str("\\e"),
                b'\n' => text.push_str("\\n"),
                b'\r' => text.push_str("\\r"),
                b'\t' => text.push_str("\\t"),
                32..=126 => text.push(byte as char),
                _ => text.push_str(&format!("\\x{byte:02x}")),
            }
        }
        text
    }

    #[test]
    fn ssid_text_decodes_to_the_bytes_the_supplicant_encoded() {
        let mut rng = fastrand::Rng::with_seed(0x5353_4944);
        for case in 0..20_000 {
            let ssid: Vec<u8> = (0..rng.usize(0..33)).map(|_| match case % 3 {
                0 => rng.u8(..),
                1 => *b"\\\"\n\r\t\x1bx41 e\xc3\xa9"[rng.usize(..13)..].first().unwrap(),
                _ => rng.u8(32..127),
            }).collect();
            assert_eq!(unescape(&supplicant_text(&ssid)), String::from_utf8_lossy(&ssid), "{ssid:?}");
        }
        // A control byte in a saved network's name still equals the name the
        // player configured, so the network is not added again.
        assert_eq!(parse_networks("network id / ssid / bssid / flags\n0\tA\\nB\tany\t\n")[0].1, "A\nB");
    }

    #[test]
    fn supplicant_text_of_any_shape_parses_without_a_panic() {
        // The control socket's reply is text from another process. Escapes
        // cut short, or followed by characters of more than one byte, used
        // to slice a `str` inside a character.
        for text in ["\\xa\u{e9}", "\\x\u{20ac}", "\\x", "\\", "\\x4", "\\xg1", "\u{e9}\\", "\\x\u{e9}\u{e9}"] {
            let _ = unescape(text);
        }
        assert_eq!(unescape("\\xa\u{e9}"), "xa\u{e9}");
        let mut rng = fastrand::Rng::with_seed(0x5750_4121);
        let pieces = ["\\", "x", "\\x", "4", "a", "\t", "\n", "=", "\u{e9}", "\u{20ac}", "\u{1f3b5}", "ssid", "wpa_state=COMPLETED", "[WPA2-PSK-CCMP][SAE]", "-55", "0", "\0"];
        for _ in 0..20_000 {
            let text: String = (0..rng.usize(0..40)).map(|_| pieces[rng.usize(..pieces.len())]).collect();
            let _ = unescape(&text);
            for result in parse_scan(&text) { assert!(!result.ssid.is_empty()); }
            let _ = parse_networks(&text);
            let _ = signal_percent(&text);
            parse_status(&text, &mut WifiState::default());
            let _ = band::parse_bss(&text);
        }
    }

    #[test]
    fn signal_poll_maps_rssi_to_the_driver_scale() {
        assert_eq!(signal_percent("RSSI=-55\nLINKSPEED=433\nNOISE=9999\nFREQUENCY=5180\n"), Some(90));
        assert_eq!(signal_percent("RSSI=-120\n"), Some(0));
        assert_eq!(signal_percent("RSSI=-30\n"), Some(100));
        assert_eq!(signal_percent("FAIL\n"), None);
    }

    #[test]
    fn scan_results() {
        let text = "bssid / frequency / signal level / flags / ssid\n\
            aa:bb:cc:dd:ee:01\t2412\t-71\t[WPA2-PSK-CCMP][ESS]\tHome\n\
            aa:bb:cc:dd:ee:02\t5180\t-50\t[WPA2-PSK-CCMP][ESS]\tHome\n\
            aa:bb:cc:dd:ee:03\t2437\t-60\t[ESS]\tCaf\\xc3\\xa9\n\
            aa:bb:cc:dd:ee:04\t2437\t-30\t[ESS]\t\n";
        let r = parse_scan(text);
        assert_eq!(r.len(), 2, "the hidden network is left out, Home once");
        assert_eq!(r[0], ScanResult { ssid: "Home".into(), signal: -50, secured: true, security: WifiSecurity::Wpa2 });
        assert_eq!(r[1].ssid, "Café");
        assert!(!r[1].secured);
    }

    #[test]
    fn networks() {
        let t = "network id / ssid / bssid / flags\n0\tHome\tany\t[CURRENT]\n3\tCaf\\xc3\\xa9\tany\t\n";
        assert_eq!(parse_networks(t), vec![(0, "Home".to_string()), (3, "Café".to_string())]);
    }

    #[test]
    fn status() {
        let mut st = WifiState::default();
        parse_status("bssid=aa\nfreq=2437\nssid=Home\nwpa_state=COMPLETED\nip_address=192.168.1.5\n", &mut st);
        assert_eq!(st.ssid.as_deref(), Some("Home"));
        assert_eq!(st.ip.as_deref(), Some("192.168.1.5"));
        assert_eq!((st.bssid.as_deref(), st.freq), (Some("aa"), Some(2437)));
        parse_status("wpa_state=SCANNING\nbssid=aa\nfreq=5180\n", &mut st);
        assert!(st.ssid.is_none());
        assert_eq!((st.bssid.as_deref(), st.freq), (None, None), "no BSS unless associated");
        assert!(st.ip.is_none());
        parse_status("ssid=Generated\n", &mut st);
        assert!(st.wpa_state.is_empty());
        assert!(st.ssid.is_none());
    }

    /// Against a real wpa_supplicant: tests/e2e/wpa-test.sh starts one.
    #[tokio::test]
    #[ignore]
    async fn wpa_supplicant_round_trip() {
        use owner::{Backend, ensure_network, wait_for_wpa, wpa_ok};
        let b = backend::System::new(PathBuf::from("unused-generated-helper"), ctrl_paths());
        assert!(wait_for_wpa(&b, 5).await, "no wpa_supplicant at Q2ND_WPA_CTRL");
        let home = WifiNetwork { ssid: "Café \"Home\"".into(), psk: "pass \"word\" 1".into(), ..Default::default() };
        let id = ensure_network(&b, &home).await.expect("added");
        assert!(ensure_network(&b, &WifiNetwork { ssid: "Open".into(), psk: String::new(), ..Default::default() }).await.is_some());
        assert_eq!(ensure_network(&b, &home).await, Some(id), "the same network again is updated, not added");
        assert!(wpa_ok(&b, "SAVE_CONFIG").await);
        let known: Vec<String> = parse_networks(&b.wpa("LIST_NETWORKS").await.unwrap()).into_iter().map(|(_, s)| s).collect();
        assert_eq!(known, ["Café \"Home\"", "Open"]);
        // The passphrase reads back masked; the saved file has it.
        let conf = std::fs::read_to_string(std::env::var("Q2ND_WPA_CONF").unwrap()).unwrap();
        assert!(conf.contains("psk=\"pass \"word\" 1\""), "{conf}");
        assert!(conf.contains("key_mgmt=NONE"), "{conf}");
        for (id, _) in parse_networks(&b.wpa("LIST_NETWORKS").await.unwrap()) {
            assert!(wpa_ok(&b, &format!("REMOVE_NETWORK {id}")).await);
        }
    }

    #[test]
    fn add_network_commands() {
        let c = add_commands(2, &WifiNetwork { ssid: "My \"Net\"".into(), psk: "pa\"ss".into(), ..Default::default() });
        assert_eq!(c[0], "SET_NETWORK 2 ssid 4d7920224e657422");
        assert_eq!(c[1], "SET_NETWORK 2 psk \"pa\"ss\"");
        let c = add_commands(0, &WifiNetwork { ssid: "Open".into(), psk: String::new(), ..Default::default() });
        assert_eq!(c[1], "SET_NETWORK 0 key_mgmt NONE");
    }

    #[test]
    fn sae_scan_and_commands_preserve_security_modes() {
        let scan = parse_scan("bssid / frequency / signal level / flags / ssid\n00:00:00:00:00:01\t2412\t-45\t[WPA2-SAE-CCMP][ESS]\tSAE\n00:00:00:00:00:02\t2412\t-50\t[WPA2-PSK+SAE-CCMP][ESS]\tTransition\n");
        assert_eq!(scan[0].security, WifiSecurity::Wpa3);
        assert_eq!(scan[1].security, WifiSecurity::Wpa2Wpa3);
        for (security, keys, pmf) in [(WifiSecurity::Wpa3, "SAE", 2), (WifiSecurity::Wpa2Wpa3, "SAE WPA-PSK", 1)] {
            let commands = add_commands(3, &WifiNetwork { ssid: "Synthetic".into(), psk: "generated-password".into(), security });
            assert!(commands.contains(&format!("SET_NETWORK 3 key_mgmt {keys}")));
            assert!(commands.contains(&format!("SET_NETWORK 3 ieee80211w {pmf}")));
            assert!(commands.contains(&"SET_NETWORK 3 sae_pwe 2".into()));
            assert_eq!(commands.iter().any(|c| c.starts_with("SET_NETWORK 3 psk ")), security == WifiSecurity::Wpa2Wpa3);
        }
        let old: WifiNetwork = toml::from_str("ssid='Synthetic'\npsk='generated-password'\n").unwrap();
        assert_eq!(old.security, WifiSecurity::Wpa2);
        assert!(add_commands(3, &WifiNetwork { security: WifiSecurity::Wpa3, ..Default::default() }).is_empty());
    }
}
