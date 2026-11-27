//! Bluetooth: the switch and the controller's state, the paired devices
//! (each with its own page), Pair New Device, the preferred codec, and a
//! details page for diagnosis (long Center). Laid out as the Wi-Fi page is.

use super::controls::{Choice, Confirm};
use crate::bluetooth::{CODEC_SETTINGS, Device, Link, Phase, State, Step};
use crate::gfx::Rect;
use crate::input::Event;
use crate::ui::list::{ListView, Row, RowAction};
use crate::ui::{Action, Ctx, Draw, Screen};
use std::time::Instant;

type Addr = [u8; 6];

const TOGGLE: u32 = 1;
/// Starts a controller that failed again.
const RETRY_START: u32 = 2;
/// Repeats the last connection attempt.
const RETRY_ATTEMPT: u32 = 3;
const PAIR: u32 = 4;
const CODEC: u32 = 5;
const SEARCH: u32 = 6;
const CONNECT: u32 = 7;
const DISCONNECT: u32 = 8;
const FORGET: u32 = 9;
/// Paired devices are commands from here, by index; found devices from `FOUND`.
const PAIRED: u32 = 100;
const FOUND: u32 = 1000;

/// The controller's state, under the switch.
fn controller(state: &State) -> String {
    match &state.phase { Phase::Off | Phase::Starting => "Starting…".into(), Phase::Ready => "Ready".into(), Phase::Failed(message) => message.clone() }
}

/// Where an attempt to connect `addr` is, in words; `None` when there is none.
fn progress(state: &State, addr: Addr) -> Option<&'static str> {
    let paired = state.paired.iter().any(|d| d.addr == addr);
    match state.link {
        Link::Connecting(at) if at == addr => Some(match state.step {
            Some(Step::SinkCheck) => "Checking for Bluetooth audio…",
            Some(Step::Authentication) if !paired => "Pairing…",
            Some(Step::Encryption) => "Securing the connection…",
            _ => "Connecting…",
        }),
        Link::Connected(at) if at == addr && state.stream.is_none() => Some("Setting up audio…"),
        _ => None,
    }
}

/// "Just now", "5 minutes ago", "3 hours ago", "Yesterday", "4 days ago".
fn ago(now: u64, then: u64) -> String {
    let seconds = now.saturating_sub(then);
    match seconds {
        0..120 => "Just now".into(),
        120..3600 => format!("{} minutes ago", seconds / 60),
        3600..7200 => "1 hour ago".into(),
        7200..86_400 => format!("{} hours ago", seconds / 3600),
        86_400..172_800 => "Yesterday".into(),
        _ => format!("{} days ago", seconds / 86_400),
    }
}

/// A paired device's value and second line: playing or connected with the codec,
/// the step of an attempt, or when it was last connected.
fn device_status(state: &State, device: &Device, now: u64) -> (Option<&'static str>, String) {
    let addr = device.addr;
    match state.link {
        Link::Playing(at) if at == addr => (Some("Playing"), state.codec_detail().unwrap_or_default()),
        Link::Connected(at) if at == addr => (Some("Connected"), state.codec_detail().unwrap_or_else(|| "Setting up audio…".into())),
        Link::Connecting(at) if at == addr => (None, progress(state, addr).unwrap_or("Connecting…").into()),
        _ => (None, state.seen(addr).map_or_else(|| "Not connected yet".into(), |at| format!("Last connected {}", ago(now, at).to_lowercase()))),
    }
}

fn codec_label(setting: &str) -> &'static str {
    CODEC_SETTINGS.iter().find(|(name, _, _)| *name == setting).map_or("Automatic", |(_, shown, _)| *shown)
}

/// Connects `addr`, after disconnecting other headphones: one device at a time.
fn connect(ctx: &Ctx, state: &State, addr: Addr, checked: bool) {
    if state.connected().is_some_and(|other| other != addr) || matches!(state.link, Link::Connecting(other) if other != addr) { ctx.bluetooth.disconnect(); }
    if checked { ctx.bluetooth.connect_checked(addr); } else { ctx.bluetooth.connect(addr); }
}

/// Try Again after a failed attempt: the same device, the same way.
fn retry_attempt(ctx: &Ctx, state: &State) -> Action {
    let Some((addr, checked)) = state.attempt else { return Action::None };
    connect(ctx, state, addr, checked);
    Action::Toast(format!("Connecting to {}", state.name(addr)))
}

pub struct Bluetooth {
    list: ListView,
    /// Commands refer to the displayed devices, even if a newer state is
    /// published before its notification reaches this screen.
    displayed: State,
}

impl Bluetooth {
    pub fn new() -> Self { Self { list: ListView::settings(true), displayed: State::default() } }

    fn rows(enabled: bool, codec: &str, state: &State, now: u64) -> Vec<Row> {
        let toggle = Row::new("Bluetooth", RowAction::Cmd(TOGGLE)).key("#toggle").value(if enabled { "On" } else { "Off" });
        if !enabled { return vec![toggle] }
        let mut rows = vec![toggle.detail(controller(state))];
        if matches!(state.phase, Phase::Failed(_)) {
            rows.push(Row::new("Try Again", RowAction::Cmd(RETRY_START)).key("#retry-start"));
            return rows;
        }
        if !state.notice.is_empty() {
            rows.push(Row::new("Last Problem", RowAction::None).key("#notice").detail(state.notice.clone()));
            if let Some((addr, _)) = state.attempt.filter(|_| state.link == Link::Idle) {
                rows.push(Row::new("Try Again", RowAction::Cmd(RETRY_ATTEMPT)).key("#retry").detail(format!("Connect to {}", state.name(addr))));
            }
        }
        for (i, device) in state.paired.iter().enumerate() {
            let (value, detail) = device_status(state, device, now);
            let row = Row::screen(device.name.clone(), PAIRED + i as u32).key(format!("paired:{i}")).detail(detail);
            let row = match value { Some(value) => row.value(value), None => row };
            rows.push(if i == 0 { row.separated() } else { row });
        }
        rows.push(Row::screen("Pair New Device", PAIR).key("#pair").separated());
        rows.push(Row::screen("Preferred Codec", CODEC).key("#codec").value(codec_label(codec)));
        rows
    }

    fn refresh_rows(&mut self, ctx: &Ctx) {
        let state = ctx.bluetooth.state();
        self.list.set_rows(Self::rows(ctx.config.bluetooth.enabled, &ctx.config.bluetooth.codec, &state, crate::bluetooth::unix_now()));
        self.displayed = state;
    }
}

fn codec_choice(ctx: &Ctx) -> Box<dyn Screen> {
    // Automatic takes the best codec both sides have; a named one falls back to SBC.
    let current = CODEC_SETTINGS.iter().position(|(name, _, _)| *name == ctx.config.bluetooth.codec);
    Box::new(Choice::new("Preferred Codec", CODEC_SETTINGS.iter().map(|(_, shown, _)| (*shown).to_string()).collect(), current, Box::new(|i, ctx| {
        ctx.config.bluetooth.codec = CODEC_SETTINGS[i].0.into();
        *ctx.config_changed = true;
        ctx.bluetooth.set_codecs(crate::bluetooth::codecs(&ctx.config.bluetooth.codec));
        if ctx.bluetooth.state().stream.is_some() { Action::Many(vec![Action::Pop, Action::Toast("Used when the headphones next connect".into())]) } else { Action::Pop }
    })))
}

impl Screen for Bluetooth {

    fn clickable(&self, x: i32, y: i32) -> bool {
        self.list.hit(x, y).is_some()
    }

    fn click(&mut self, x: i32, y: i32, ctx: &mut Ctx) -> Action {
        if self.list.select_at(x, y) {
            self.event(Event::Center, ctx)
        } else {
            Action::None
        }
    }

    #[cfg(test)]
    fn audit(&self) -> crate::ui::hierarchy::Surface { crate::ui::hierarchy::Surface::list(&self.list) }
    fn title(&self) -> String { "Bluetooth".into() }
    fn enter(&mut self, ctx: &mut Ctx) { self.refresh_rows(ctx); }
    fn bluetooth_changed(&mut self, ctx: &mut Ctx) { self.refresh_rows(ctx); }

    fn event(&mut self, ev: Event, ctx: &mut Ctx) -> Action {
        match ev {
            Event::Scroll(t) => { self.list.wheel(t, ctx.now); Action::None }
            Event::WheelEnd => { self.list.end_spin(); Action::None }
            // Diagnosis: the controller, firmware, link and counters.
            Event::CenterLong => Action::Push(Box::new(Details::new())),
            Event::Center => {
                let Some(RowAction::Cmd(cmd)) = self.list.selected_row().map(|r| r.action.clone()) else { return Action::None };
                let state = &self.displayed;
                let action = match cmd {
                    TOGGLE => {
                        ctx.config.bluetooth.enabled = !ctx.config.bluetooth.enabled;
                        *ctx.config_changed = true;
                        ctx.bluetooth.set_enabled(ctx.config.bluetooth.enabled);
                        Action::None
                    }
                    RETRY_START => { ctx.bluetooth.retry(); Action::None }
                    RETRY_ATTEMPT => retry_attempt(ctx, state),
                    PAIR => return Action::Push(Box::new(PairNew::new())),
                    CODEC => return Action::Push(codec_choice(ctx)),
                    n if n >= PAIRED => {
                        let Some(device) = state.paired.get((n - PAIRED) as usize) else { return Action::None };
                        return Action::Push(Box::new(DeviceInfo::new(device.addr, device.name.clone())));
                    }
                    _ => Action::None,
                };
                self.refresh_rows(ctx);
                action
            }
            _ => Action::None,
        }
    }

    fn draw(&mut self, d: &mut Draw, area: Rect) { self.list.draw(d, area); }
    fn animate(&mut self, now: Instant) -> bool { self.list.animate(now) }
}

/// Pair New Device: a search that runs while the page is open, the devices it
/// finds, and the chosen one's pairing step by step. Pairing done, the page closes.
pub struct PairNew {
    list: ListView,
    displayed: State,
    chosen: Option<Addr>,
    done: Option<Action>,
}

impl PairNew {
    pub fn new() -> Self { Self { list: ListView::settings(true), displayed: State::default(), chosen: None, done: None } }

    /// Found devices that are not paired: headphones and speakers, then devices whose
    /// inquiry response lists an A2DP sink, then the rest; stronger signals first within each.
    fn candidates(state: &State) -> Vec<&Device> {
        let mut found: Vec<_> = state.found.iter().filter(|d| !state.paired.iter().any(|p| p.addr == d.addr)).collect();
        found.sort_by_key(|d| (!d.is_audio(), !d.takes_audio(), if d.rssi == 0 { i16::MAX } else { -i16::from(d.rssi) }));
        found
    }

    fn rows(state: &State, chosen: Option<Addr>) -> Vec<Row> {
        let status = |title: String, detail: &str| Row::new(title, RowAction::None).key("#status").detail(detail);
        let mut rows = Vec::new();
        match &state.phase {
            Phase::Failed(message) => {
                rows.push(status("Bluetooth Is Not Available".into(), message));
                rows.push(Row::new("Try Again", RowAction::Cmd(RETRY_START)).key("#retry-start"));
                return rows;
            }
            Phase::Off | Phase::Starting => rows.push(status("Starting Bluetooth…".into(), "The search begins when it is ready")),
            Phase::Ready => match chosen.and_then(|addr| progress(state, addr).map(|words| (addr, words))) {
                Some((addr, words)) => rows.push(status(format!("Pairing with {}", state.name(addr)), words)),
                None if !state.notice.is_empty() => {
                    rows.push(status("Pairing Did Not Work".into(), &state.notice));
                    if state.attempt.is_some() && state.link == Link::Idle { rows.push(Row::new("Try Again", RowAction::Cmd(RETRY_ATTEMPT)).key("#retry")); }
                }
                None if state.discovering || state.scanning => rows.push(status("Searching…".into(), "Put the headphones in pairing mode")),
                None => {
                    let found = Self::candidates(state).len();
                    rows.push(status("Search Finished".into(), if found == 0 { "No devices in pairing mode were found" } else { "" }));
                    rows.push(Row::new("Search Again", RowAction::Cmd(SEARCH)).key("#search"));
                }
            },
        }
        for (i, device) in Self::candidates(state).into_iter().enumerate() {
            let pairing = chosen == Some(device.addr) && progress(state, device.addr).is_some();
            let row = Row::new(device.name.clone(), RowAction::Cmd(FOUND + i as u32)).key(format!("found:{i}")).detail(device.kind());
            let row = match (pairing, device.signal()) { (true, _) => row.value("Pairing…"), (false, Some(signal)) => row.value(signal), (false, None) => row };
            rows.push(if i == 0 { row.separated() } else { row });
        }
        rows
    }

    fn refresh_rows(&mut self, ctx: &Ctx) {
        let state = ctx.bluetooth.state();
        self.list.set_rows(Self::rows(&state, self.chosen));
        self.displayed = state;
    }
}

impl Screen for PairNew {

    fn clickable(&self, x: i32, y: i32) -> bool {
        self.list.hit(x, y).is_some()
    }

    fn click(&mut self, x: i32, y: i32, ctx: &mut Ctx) -> Action {
        if self.list.select_at(x, y) {
            self.event(Event::Center, ctx)
        } else {
            Action::None
        }
    }

    #[cfg(test)]
    fn audit(&self) -> crate::ui::hierarchy::Surface { crate::ui::hierarchy::Surface::list(&self.list) }
    fn title(&self) -> String { "Pair New Device".into() }
    fn enter(&mut self, ctx: &mut Ctx) {
        ctx.bluetooth.discover();
        self.refresh_rows(ctx);
    }
    fn bluetooth_changed(&mut self, ctx: &mut Ctx) {
        self.refresh_rows(ctx);
        let state = &self.displayed;
        if let Some(addr) = self.chosen.filter(|addr| state.connected() == Some(*addr)) {
            // Paired: back to Bluetooth, where the device now heads the list.
            self.chosen = None;
            ctx.bluetooth.stop_discovery();
            self.done = Some(Action::Many(vec![Action::Pop, Action::Toast(format!("Connected to {}", state.name(addr)))]));
        }
    }
    fn completion(&mut self) -> Option<Action> { self.done.take() }

    fn event(&mut self, ev: Event, ctx: &mut Ctx) -> Action {
        match ev {
            Event::Scroll(t) => { self.list.wheel(t, ctx.now); Action::None }
            Event::WheelEnd => { self.list.end_spin(); Action::None }
            Event::Back => { ctx.bluetooth.stop_discovery(); Action::None }
            Event::Center => {
                let Some(RowAction::Cmd(cmd)) = self.list.selected_row().map(|r| r.action.clone()) else { return Action::None };
                let state = &self.displayed;
                match cmd {
                    // A new start clears the requests of the failed one: ask for the search again.
                    RETRY_START => { ctx.bluetooth.retry(); ctx.bluetooth.discover(); }
                    RETRY_ATTEMPT => { self.chosen = state.attempt.map(|a| a.0); retry_attempt(ctx, state); }
                    SEARCH => ctx.bluetooth.discover(),
                    n if n >= FOUND => {
                        let Some(device) = Self::candidates(state).get((n - FOUND) as usize).map(|d| (*d).clone()) else { return Action::None };
                        self.chosen = Some(device.addr);
                        // Another class of device is paired only if its SDP records show an A2DP sink.
                        connect(ctx, state, device.addr, !device.is_audio());
                    }
                    _ => {}
                }
                self.refresh_rows(ctx);
                Action::None
            }
            _ => Action::None,
        }
    }

    fn draw(&mut self, d: &mut Draw, area: Rect) { self.list.draw(d, area); }
    fn animate(&mut self, now: Instant) -> bool { self.list.animate(now) }
}

/// One paired device: connect or disconnect, its state, codec, type, when it was
/// last connected, and Forget.
pub struct DeviceInfo {
    addr: Addr,
    name: String,
    list: ListView,
    displayed: State,
    done: Option<Action>,
}

impl DeviceInfo {
    pub fn new(addr: Addr, name: String) -> Self { Self { addr, name, list: ListView::settings(true), displayed: State::default(), done: None } }

    fn rows(state: &State, addr: Addr, now: u64) -> Vec<Row> {
        let Some(device) = state.paired.iter().find(|d| d.addr == addr) else { return vec![Row::new("No Longer Paired", RowAction::None).key("#gone")] };
        let info = |title: &str| Row::new(title, RowAction::None).key(title);
        let mut rows = Vec::new();
        match state.link {
            Link::Connected(at) | Link::Playing(at) if at == addr => rows.push(Row::new("Disconnect", RowAction::Cmd(DISCONNECT)).key("#connection")),
            Link::Connecting(at) if at == addr => rows.push(Row::new("Stop Connecting", RowAction::Cmd(DISCONNECT)).key("#connection").detail(progress(state, addr).unwrap_or("Connecting…"))),
            _ => rows.push(Row::new("Connect", RowAction::Cmd(CONNECT)).key("#connection")
                .detail(if state.phase == Phase::Ready { "" } else { "Connects when Bluetooth is ready" })),
        }
        let status = match state.link {
            Link::Playing(at) if at == addr => "Playing",
            Link::Connected(at) if at == addr => "Connected",
            Link::Connecting(at) if at == addr => "Connecting…",
            _ => "Not connected",
        };
        rows.push(info("Status").value(status));
        if state.connected() == Some(addr) {
            let codec = info("Codec").detail(state.codec_detail().unwrap_or_else(|| "Setting up audio…".into()));
            rows.push(match &state.stream { Some(stream) => codec.value(crate::bluetooth::sample_rate(stream)), None => codec });
            if state.stream.is_some() && !state.codec_reason.is_empty() { rows.push(info("Codec Choice").detail(state.codec_reason.clone())); }
        } else {
            rows.push(info("Last Connected").value(state.seen(addr).map_or_else(|| "Not yet".into(), |at| ago(now, at))));
        }
        rows.push(info("Type").value(device.kind()));
        rows.push(Row::screen("Forget This Device…", FORGET).key("#forget").separated());
        rows
    }

    fn refresh_rows(&mut self, ctx: &Ctx) {
        let state = ctx.bluetooth.state();
        self.list.set_rows(Self::rows(&state, self.addr, crate::bluetooth::unix_now()));
        // Forgotten, or the headphones refused the stored key: nothing left to show.
        if self.displayed.paired.iter().any(|d| d.addr == self.addr) && !state.paired.iter().any(|d| d.addr == self.addr) { self.done = Some(Action::Pop); }
        self.displayed = state;
    }
}

impl Screen for DeviceInfo {

    fn clickable(&self, x: i32, y: i32) -> bool {
        self.list.hit(x, y).is_some()
    }

    fn click(&mut self, x: i32, y: i32, ctx: &mut Ctx) -> Action {
        if self.list.select_at(x, y) {
            self.event(Event::Center, ctx)
        } else {
            Action::None
        }
    }

    #[cfg(test)]
    fn audit(&self) -> crate::ui::hierarchy::Surface { crate::ui::hierarchy::Surface::list(&self.list) }
    fn title(&self) -> String { self.name.clone() }
    fn enter(&mut self, ctx: &mut Ctx) { self.refresh_rows(ctx); }
    fn bluetooth_changed(&mut self, ctx: &mut Ctx) { self.refresh_rows(ctx); }
    fn completion(&mut self) -> Option<Action> { self.done.take() }

    fn event(&mut self, ev: Event, ctx: &mut Ctx) -> Action {
        match ev {
            Event::Scroll(t) => { self.list.wheel(t, ctx.now); Action::None }
            Event::WheelEnd => { self.list.end_spin(); Action::None }
            Event::Center => {
                let Some(RowAction::Cmd(cmd)) = self.list.selected_row().map(|r| r.action.clone()) else { return Action::None };
                let (addr, state) = (self.addr, &self.displayed);
                let action = match cmd {
                    CONNECT => { connect(ctx, state, addr, false); Action::None }
                    DISCONNECT => { ctx.bluetooth.disconnect(); Action::None }
                    FORGET => return Action::Push(Box::new(Confirm::new(
                        "Forget Device?", "Remove this pairing.\nPair again to use it.", "Forget Device", move |ctx| {
                            ctx.bluetooth.forget(addr);
                            Action::Many(vec![Action::Pop, Action::Pop])
                        },
                    ))),
                    _ => Action::None,
                };
                self.refresh_rows(ctx);
                action
            }
            _ => Action::None,
        }
    }

    fn draw(&mut self, d: &mut Draw, area: Rect) { self.list.draw(d, area); }
    fn animate(&mut self, now: Instant) -> bool { self.list.animate(now) }
}

/// Bluetooth Details: the controller, firmware, link, codec, last error and the
/// counters, for diagnosis. No addresses and no device names.
pub struct Details { list: ListView }

impl Details {
    pub fn new() -> Self { Self { list: ListView::settings(true) } }

    fn rows(state: &State) -> Vec<Row> {
        let info = |title: &str| Row::new(title, RowAction::None).key(title);
        let phase = match &state.phase { Phase::Off => "Off", Phase::Starting => "Starting", Phase::Ready => "Ready", Phase::Failed(_) => "Failed" };
        let mut rows = vec![match &state.phase { Phase::Failed(message) => info("Controller").value(phase).detail(message.clone()), _ => info("Controller").value(phase) }];
        match state.controller {
            None => rows.push(info("Firmware").value("Not read yet")),
            Some(version) => {
                let loaded = if version.already_patched { "Kept from earlier" } else if version.rom_version.is_some() { "Patch loaded" } else { "ROM only" };
                let (revision, subversion) = state.diagnostics.patched_version.unwrap_or((version.hci_revision, version.lmp_subversion));
                let rom = version.rom_version.map_or_else(String::new, |rom| format!("ROM {rom} · "));
                rows.push(info("Firmware").value(loaded));
                rows.push(info("Version").detail(format!("{rom}revision {revision:#06x} · subversion {subversion:#06x}")));
            }
        }
        let link = match (state.link, state.step) {
            (Link::Idle, _) => "None",
            (Link::Connecting(_), Some(Step::SinkCheck)) => "Checking for audio",
            (Link::Connecting(_), Some(Step::Authentication)) => "Authenticating",
            (Link::Connecting(_), Some(Step::Encryption)) => "Encrypting",
            (Link::Connecting(_), _) => "Paging",
            (Link::Connected(_), _) => "Connected",
            (Link::Playing(_), _) => "Playing",
        };
        // The readings telemetry publishes every 250 ms (`State::link_stats`): RSSI and link quality while audio streams.
        let ls = &state.link_stats;
        let signal = match (ls.rssi, ls.link_quality) {
            (Some(rssi), Some(quality)) => format!("RSSI {rssi} · quality {quality}"),
            (Some(rssi), None) => format!("RSSI {rssi}"),
            (None, Some(quality)) => format!("Quality {quality}"),
            (None, None) => String::new(),
        };
        rows.push(info("Link").value(link).detail(signal));
        rows.push(match (state.codec_detail(), &state.stream) { (Some(codec), Some(stream)) => info("Codec").value(crate::bluetooth::sample_rate(stream)).detail(codec), _ => info("Codec").value("None") });
        if state.stream.is_some() && !state.codec_reason.is_empty() { rows.push(info("Codec Choice").detail(state.codec_reason.clone())); }
        rows.push(if state.last_error.is_empty() { info("Last Error").value("None") } else { info("Last Error").detail(state.last_error.clone()) });
        let d = &state.diagnostics;
        rows.push(info("Transport").detail(format!("H5 resent {} · timeouts {} · bad {} · rejected {}", ls.h5_retransmissions, ls.h5_resends, ls.h5_bad_frames, d.h5_rejected)));
        rows.push(info("Commands").detail(format!("Timeouts {} · refused {} · unparsed {} · scan timeouts {}", d.command_timeouts, d.command_errors.len(), d.unparsed, d.scan_timeouts)));
        rows.push(info("Scans").detail(format!("Results {} · completed {} · names {}", d.inquiry_results, d.inquiry_completes, d.name_replies)));
        rows.push(info("Audio").detail(format!("Dropped packets {} · underruns {} · stalls {}", state.counters.0, state.counters.2, ls.credit_stalls)));
        let r = state.resync;
        rows.push(info("Wi-Fi Power-Ups").value(r.checks.to_string()).detail(format!("Answered {} · lost {} · restarted {} · busy {}", r.alive, r.lost, r.reinits, r.busy)));
        rows.push(info("Waiting Requests").value(state.waiting.to_string()));
        rows
    }
}

impl Screen for Details {
    #[cfg(test)]
    fn audit(&self) -> crate::ui::hierarchy::Surface { crate::ui::hierarchy::Surface::list(&self.list) }
    fn title(&self) -> String { "Bluetooth Details".into() }
    fn enter(&mut self, ctx: &mut Ctx) { self.list.set_rows(Self::rows(&ctx.bluetooth.state())); }
    fn bluetooth_changed(&mut self, ctx: &mut Ctx) { self.list.set_rows(Self::rows(&ctx.bluetooth.state())); }
    fn event(&mut self, ev: Event, ctx: &mut Ctx) -> Action {
        match ev {
            Event::Scroll(t) => { self.list.wheel(t, ctx.now); Action::None }
            Event::WheelEnd => { self.list.end_spin(); Action::None }
            _ => Action::None,
        }
    }
    fn draw(&mut self, d: &mut Draw, area: Rect) { self.list.draw(d, area); }
    fn animate(&mut self, now: Instant) -> bool { self.list.animate(now) }
}

#[cfg(test)]
#[path = "bluetooth_tests.rs"]
mod tests;
