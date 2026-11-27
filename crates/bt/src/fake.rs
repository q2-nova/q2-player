//! A controller with headphones in range, for tests, the simulator and the
//! emulator: the Realtek ROM and patch download, inquiry, pairing, and the
//! headphones' side of L2CAP, SDP (server, with a Device ID record), AVDTP
//! (sink) and AVRCP (controller of the player; target for absolute volume),
//! as acceptor or, after `reconnect`, as initiator. A device
//! need not be of the audio class: a computer that takes audio is one with
//! another `class` and `sdp_sink`.
use crate::h5::{self, H5};
use crate::host::{Addr, INITIAL_BAUD};
use crate::{avrcp, rtl, sdp};
use std::collections::VecDeque;

#[derive(Clone, Debug)]
pub struct Headphones {
    pub addr: Addr,
    pub name: String,
    /// Whether the inquiry response carries the name (extended inquiry response).
    pub named_in_inquiry: bool,
    pub pairing_mode: bool,
    pub simple_pairing: bool,
    pub key: Option<[u8; 16]>,
    /// SBC capability bytes offered to the source.
    pub sbc: [u8; 4],
    /// Further Media Codec elements (`codec::capabilities` format), each on its own endpoint after the SBC one.
    pub codecs: Vec<Vec<u8>>,
    pub media_mtu: u16,
    /// Starts the stream itself after configuring it (as some headphones do on reconnect).
    pub starts_stream: bool,
    pub powered: bool,
    /// Class of device, as the inquiry response carries it.
    pub class: u32,
    /// Its SDP server has an A2DP sink record (0x110b).
    pub sdp_sink: bool,
    /// 16-bit service UUIDs in the extended inquiry response (named devices only).
    pub eir_uuids: Vec<u16>,
    /// AVRCP 1.4 absolute volume: reports the volume event and takes SetAbsoluteVolume.
    pub absolute_volume: bool,
    /// The amplifier's level, 0..=127.
    pub volume: u8,
    /// Levels the amplifier has (it rounds a level set to one of them); `None`: all 128.
    pub volume_steps: Option<u8>,
    /// Also notifies a level the source set (some headphones do).
    pub echo_volume: bool,
    /// The Device ID record its SDP server has.
    pub device_id: Option<sdp::DeviceId>,
    /// Answers the IO capability exchange and then never asks for the user confirmation:
    /// pairing stalls (a device that waits for a button press nobody makes).
    pub stalls_pairing: bool,
    /// As initiator after a reconnect, configures SBC whatever else it has (as some headphones do).
    pub configures_sbc: bool,
}
impl Headphones {
    pub fn new(last: u8, name: &str) -> Self {
        Self { addr: [last, 0x22, 0x33, 0x44, 0x55, 0x66], name: name.into(), named_in_inquiry: true, pairing_mode: true, simple_pairing: true,
            key: None, sbc: [0x3f, 0xff, 2, 53], codecs: Vec::new(), media_mtu: 895, starts_stream: false, powered: true,
            class: 0x24_0404, sdp_sink: true, eir_uuids: Vec::new(), absolute_volume: false, volume: 0x20, volume_steps: None, echo_volume: false, device_id: None, stalls_pairing: false, configures_sbc: false }
    }
    /// A desktop computer (major class computer, minor desktop) that takes audio:
    /// an A2DP sink in its SDP records, nothing in its inquiry response but its name.
    pub fn computer(last: u8, name: &str) -> Self { Self { class: 0x7c_0104, ..Self::new(last, name) } }
}

/// What the headphones observed.
#[derive(Default, Debug)]
pub struct Log {
    pub commands: Vec<u16>,
    pub image: Vec<u8>,
    pub local_name: String,
    pub scan_enable: u8,
    pub configured: Option<[u8; 4]>,
    /// The Media Codec element of the last configuration, whatever the codec.
    pub configured_element: Vec<u8>,
    /// Media packets after their RTP header (aptX: whole packets), concatenated.
    pub media: Vec<u8>,
    pub markers: u32,
    pub streaming: bool,
    pub media_packets: u32,
    pub media_sequence_gaps: u32,
    pub sbc: Vec<u8>,
    pub sbc_frames: u32,
    pub last_timestamp: u32,
    /// The frame count byte (low four bits) of the last RTP media packet.
    pub media_frames: u8,
    /// Responses from the source to the headphones' commands.
    pub avrcp_responses: Vec<Vec<u8>>,
    /// The PDUs of the source's vendor-dependent commands, in order.
    pub avrcp_commands: Vec<u8>,
    /// Levels the source set with SetAbsoluteVolume, after the amplifier's rounding.
    pub volumes_set: Vec<u8>,
    pub device_id_queries: u32,
    pub sdp_responses: Vec<Vec<u8>>,
    /// Service searches the source sent to the device's SDP server.
    pub sdp_searches: u32,
    pub uart_garbage: u32,
    /// Media packets as the controller sent them over the air: (ms, RTP timestamp, frame count byte).
    pub arrivals: Vec<(u64, u32, u8)>,
    /// ACL packets received while every controller buffer was full (a host that ignores credits).
    pub acl_overflow: u32,
    /// Parameters of each Realtek profile report (0xfc19).
    pub profile_reports: Vec<Vec<u8>>,
}

/// The radio side of the ACL path: how long the controller takes to send one
/// ACL packet over the air, and when it sends nothing (interference, Wi-Fi
/// coexistence). The default sends every packet at once.
#[derive(Clone, Debug, Default)]
pub struct Air {
    /// Air time per ACL packet, in microseconds; 0 completes it at the next poll.
    pub packet_us: u64,
    /// [start, end) in ms: nothing is sent.
    pub stalls: Vec<(u64, u64)>,
    /// [start, end, factor) in ms: each packet takes `factor` times as long (retries).
    pub slow: Vec<(u64, u64, u64)>,
}
impl Air {
    fn stalled(&self, now: u64) -> bool { self.stalls.iter().any(|&(a, b)| (a..b).contains(&now)) }
    fn factor(&self, now: u64) -> u64 { self.slow.iter().find(|s| (s.0..s.1).contains(&now)).map_or(1, |s| s.2.max(1)) }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Role { Signal, Media, Control,
    /// The device's own SDP request (`sdp_query`).
    Sdp,
    /// The device's SDP server, for the source's requests.
    SdpServer }
struct Channel { local: u16, remote: u16, role: Role, open: bool, configured: (bool, bool) }

struct Link { device: usize, handle: u16, initiator: bool, encrypted: bool, channels: Vec<Channel>, reassembly: Vec<u8>, next_cid: u16, id: u8, label: u8, expected_sequence: Option<u16>, step: u8, pending_sdp: Option<Vec<u8>>, /// The source's endpoint this side configures as initiator.
    acp: u8,
    /// The source's registration for the volume event.
    volume_label: Option<u8> }

pub struct Fake {
    h5: H5,
    now: u64,
    baud: u32,
    tx_baud: u32,
    next_baud: Option<u32>,
    pub patched: bool,
    pub rom_version: u8,
    pub devices: Vec<Headphones>,
    pub log: Log,
    /// HCI events and ACL packets due at a time.
    timed: Vec<(u64, u8, Vec<u8>)>,
    link: Option<Link>,
    page_scan: bool,
    /// Stops answering stream signalling (a hung sink).
    pub mute_signalling: bool,
    /// The device's SDP server counts requests and never answers.
    pub mute_sdp: bool,
    /// Never reports an inquiry complete (seen once on the Q2; cause unknown).
    pub swallow_inquiry_complete: bool,
    /// Events that would name the patch as restarted: drops the H5 link once after the download.
    pub restart_link_after_patch: bool,
    pending_patch_restart: bool,
    /// ACL packets in the controller's buffers, not yet sent over the air: handle, and the
    /// RTP timestamp and frame count byte when the packet completed a media packet.
    acl_packets: VecDeque<(u16, Option<(u32, u8)>)>,
    sleep_after: Option<u64>,
    pub air: Air,
    /// The air is busy until this time, in microseconds.
    air_until_us: u64,
    /// The RSSI the controller reports for the link.
    pub rssi: i8,
    /// Delays each bring-up reply (version, ROM version, download, reset) by this many ms:
    /// a slow controller start. Not the rate change, whose reply must leave at the old rate.
    pub bring_up_delay: u64,
    /// How long a page to a device that does not answer lasts before Connection
    /// Complete reports the page timeout (a real controller: 5.12 s).
    pub page_ms: u64,
    /// Adds this many ms to a page that is answered and to the pairing exchange: the pace
    /// of a real controller and headphones, for the simulator.
    pub link_delay: u64,
    /// Answers this command with status 0x1f (unspecified error) while the UART runs at this rate.
    pub refuse_at: Option<(u32, u16)>,
}

const HANDLE: u16 = 0x000b;
pub const CONTROLLER_ADDR: Addr = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06];

impl Fake {
    pub fn new(now: u64) -> Self {
        Self { h5: H5::new(now), now, baud: INITIAL_BAUD, tx_baud: INITIAL_BAUD, next_baud: None, patched: false, rom_version: 2, devices: vec![Headphones::new(0x11, "Fixture Headphones")],
            log: Log::default(), timed: Vec::new(), link: None, page_scan: false, mute_signalling: false, mute_sdp: false, swallow_inquiry_complete: false, restart_link_after_patch: false,
            pending_patch_restart: false, acl_packets: VecDeque::new(), sleep_after: None, air: Air::default(), air_until_us: 0, rssi: -3, bring_up_delay: 0, page_ms: 200, link_delay: 0, refuse_at: None }
    }
    pub fn baud(&self) -> u32 { self.baud }
    /// The controller's UART sleeps after `ms` without traffic and must be woken (Realtek's low-power mode).
    pub fn sleepy(mut self, ms: u64) -> Self { self.sleep_after = Some(ms); self.h5.sleep_after = Some(ms); self }
    /// The rate at which the bytes of the last `poll` were sent.
    pub fn tx_baud(&self) -> u32 { self.tx_baud }
    /// A controller that kept its patch and fast rate from an earlier start.
    pub fn already_patched(mut self, baud: u32) -> Self { self.patched = true; self.baud = baud; self.tx_baud = baud; self }
    pub fn connected(&self) -> bool { self.link.is_some() }
    /// BT_DIS_N dropped and rose again: the controller is back in its ROM at the initial
    /// rate, without its patch, link or scan settings, and its H5 link waits for a new sync.
    pub fn power_cycle(&mut self, now: u64) {
        self.now = now;
        self.patched = false;
        (self.baud, self.tx_baud, self.next_baud) = (INITIAL_BAUD, INITIAL_BAUD, None);
        self.h5 = H5::new(now);
        self.h5.sleep_after = self.sleep_after;
        self.timed.clear();
        self.acl_packets.clear();
        self.link = None;
        self.page_scan = false;
        self.log.streaming = false;
        self.pending_patch_restart = false;
    }

    fn event_at(&mut self, delay: u64, code: u8, parameters: &[u8]) {
        let mut packet = vec![code, parameters.len() as u8]; packet.extend_from_slice(parameters);
        self.timed.push((self.now + delay, h5::EVT, packet));
    }
    fn event(&mut self, code: u8, parameters: &[u8]) { self.event_at(0, code, parameters); }
    fn complete(&mut self, opcode: u16, returned: &[u8]) {
        let mut p = vec![1, opcode as u8, (opcode >> 8) as u8]; p.extend_from_slice(returned);
        let delay = if matches!(opcode, 0x1001 | 0xfc6d | 0xfc20 | 0x0c03) { self.bring_up_delay } else { 0 };
        self.event_at(delay, 0x0e, &p);
    }
    fn status(&mut self, opcode: u16, status: u8) { self.event(0x0f, &[status, 1, opcode as u8, (opcode >> 8) as u8]); }

    /// Bytes from the host's UART, sent at `host_baud`.
    pub fn rx(&mut self, bytes: &[u8], host_baud: u32, now: u64) {
        self.now = now;
        if host_baud != self.baud { self.log.uart_garbage += bytes.len() as u32; return }
        let mut packets = Vec::new();
        self.h5.rx(bytes, now, &mut packets);
        for (ty, payload) in packets {
            match ty {
                h5::CMD if payload.len() >= 3 => self.command(u16::from_le_bytes([payload[0], payload[1]]), &payload[3..]),
                h5::ACL if payload.len() >= 4 => {
                    // Read Buffer Size reports 8 buffers.
                    if self.acl_packets.len() >= 8 { self.log.acl_overflow += 1; }
                    let before = self.log.media_packets;
                    self.acl(payload[1] >> 4, &payload[4..]);
                    let media = (self.log.media_packets != before).then(|| (self.log.last_timestamp, self.log.media_frames));
                    self.acl_packets.push_back((u16::from_le_bytes([payload[0], payload[1]]) & 0x0fff, media));
                }
                _ => {}
            }
        }
    }

    /// Bytes for the host's UART. The caller drops them if the host listens at another rate than `tx_baud()`.
    pub fn poll(&mut self, now: u64, wire: &mut Vec<u8>) -> Option<u64> {
        self.now = now;
        self.step();
        self.tx_baud = self.baud;
        let now_us = now * 1000;
        while !self.acl_packets.is_empty() && !self.air.stalled(now) && (self.air.packet_us == 0 || self.air_until_us <= now_us) {
            let (handle, media) = self.acl_packets.pop_front().unwrap();
            if self.air.packet_us > 0 { self.air_until_us = self.air_until_us.max(now_us) + self.air.packet_us * self.air.factor(now); }
            if let Some((timestamp, frames)) = media { self.log.arrivals.push((now, timestamp, frames)); }
            self.event(0x13, &[1, handle as u8, (handle >> 8) as u8, 1, 0]);
        }
        let mut due: Vec<_> = Vec::new();
        self.timed.retain(|(at, ty, packet)| if *at <= now { due.push((*at, *ty, packet.clone())); false } else { true });
        due.sort_by_key(|d| d.0);
        for (_, ty, packet) in due { self.h5.send(ty, packet); }
        let deadline = self.h5.poll(now, wire);
        if !wire.is_empty() { if let Some(baud) = self.next_baud.take() { self.baud = baud; } }
        if self.pending_patch_restart && self.h5.backlog() == 0 { self.pending_patch_restart = false; self.h5 = H5::new(now); self.h5.sleep_after = self.sleep_after; }
        let air = (!self.acl_packets.is_empty()).then(|| self.air_until_us.div_ceil(1000).max(now + 1));
        [deadline, self.timed.iter().map(|t| t.0).min(), air].into_iter().flatten().min()
    }

    fn device(&self, addr: &[u8]) -> Option<usize> { self.devices.iter().position(|d| d.addr[..] == *addr && d.powered) }

    fn command(&mut self, opcode: u16, p: &[u8]) {
        self.log.commands.push(opcode);
        if self.refuse_at == Some((self.baud, opcode)) { return self.complete(opcode, &[0x1f]) }
        let addr = p.get(..6).unwrap_or(&[0; 6]).to_vec();
        match opcode {
            0x1001 => {
                let (revision, subversion) = if self.patched { (0x1143u16, 0xd5c8u16) } else { (0x000f, 0x8723) };
                let mut r = vec![0, 0x0a]; r.extend_from_slice(&revision.to_le_bytes()); r.push(0x0a);
                r.extend_from_slice(&0x005du16.to_le_bytes()); r.extend_from_slice(&subversion.to_le_bytes());
                self.complete(opcode, &r);
            }
            0xfc6d => self.complete(opcode, &[0, self.rom_version]),
            0xfc17 => {
                let code = u32::from_le_bytes(p.get(..4).unwrap_or(&[0; 4]).try_into().unwrap());
                match rtl::baud_of(code) { Some(baud) => { self.next_baud = Some(baud); self.complete(opcode, &[0]); } None => self.complete(opcode, &[0x12]) }
            }
            0xfc20 => {
                let index = p.first().copied().unwrap_or(0);
                self.log.image.extend_from_slice(p.get(1..).unwrap_or(&[]));
                self.complete(opcode, &[0, index]);
                if index & 0x80 != 0 { self.patched = true; self.pending_patch_restart = self.restart_link_after_patch; }
            }
            0x1407 => { let mut r = vec![0, 0, 0]; r.extend_from_slice(&((self.now * 16 / 5) as u32).to_le_bytes()); r.extend_from_slice(&[0, 0]); self.complete(opcode, &r); }
            0x1005 => self.complete(opcode, &[0, 0xfd, 0x03, 0xff, 0x08, 0x00, 0x00, 0x00]),
            0xfc19 => { self.log.profile_reports.push(p.to_vec()); self.complete(opcode, &[0]); }
            // Get Link Quality and Read RSSI on the one link.
            0x1403 => self.complete(opcode, &[0, HANDLE as u8, (HANDLE >> 8) as u8, 0xff]),
            0x1405 => self.complete(opcode, &[0, HANDLE as u8, (HANDLE >> 8) as u8, self.rssi as u8]),
            // Read Transmit Power Level 4 dBm; AFH with 3 of 79 channels left out.
            0x0c2d => self.complete(opcode, &[0, HANDLE as u8, (HANDLE >> 8) as u8, 4]),
            0x1406 => self.complete(opcode, &[0, HANDLE as u8, (HANDLE >> 8) as u8, 1, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x1f, 0x7f]),
            0x1009 => { let mut r = vec![0]; r.extend_from_slice(&CONTROLLER_ADDR); self.complete(opcode, &r); }
            0x0c13 => { self.log.local_name = String::from_utf8_lossy(&p[..p.iter().position(|&b| b == 0).unwrap_or(p.len())]).into_owned(); self.complete(opcode, &[0]); }
            0x0c1a => { self.log.scan_enable = p.first().copied().unwrap_or(0); self.page_scan = self.log.scan_enable & 2 != 0; self.complete(opcode, &[0]); }
            0x0401 => {
                self.status(opcode, 0);
                for (i, d) in self.devices.clone().iter().enumerate().filter(|(_, d)| d.powered && d.pairing_mode) {
                    let mut e = vec![1]; e.extend_from_slice(&d.addr);
                    let class = [d.class as u8, (d.class >> 8) as u8, (d.class >> 16) as u8];
                    if d.named_in_inquiry {
                        e.extend_from_slice(&[1, 0, class[0], class[1], class[2], 0, 0, 0xc8]);
                        let mut eir = vec![d.name.len() as u8 + 1, 0x09]; eir.extend_from_slice(d.name.as_bytes());
                        if !d.eir_uuids.is_empty() {
                            eir.extend_from_slice(&[d.eir_uuids.len() as u8 * 2 + 1, 0x03]);
                            for uuid in &d.eir_uuids { eir.extend_from_slice(&uuid.to_le_bytes()); }
                        }
                        eir.resize(240, 0);
                        e.extend(eir);
                        self.event_at(40 + 10 * i as u64, 0x2f, &e);
                    } else {
                        e.extend_from_slice(&[1, 0, class[0], class[1], class[2], 0, 0, 0xb0]);
                        self.event_at(40 + 10 * i as u64, 0x22, &e);
                    }
                }
                if !self.swallow_inquiry_complete { self.event_at(300, 0x01, &[0]); }
            }
            0x0402 => { self.timed.retain(|t| !(t.1 == h5::EVT && matches!(t.2[0], 0x01 | 0x22 | 0x2f))); self.complete(opcode, &[0]); }
            0x0419 => {
                self.status(opcode, 0);
                let mut e = vec![0]; e.extend_from_slice(&addr);
                match self.device(&addr) {
                    Some(i) => { let mut name = self.devices[i].name.clone().into_bytes(); name.resize(248, 0); e.extend(name); self.event_at(30, 0x07, &e); }
                    None => { e[0] = 0x04; e.resize(255, 0); self.event_at(100, 0x07, &e); }
                }
            }
            0x0405 => {
                self.status(opcode, 0);
                let mut e = vec![0, HANDLE as u8, (HANDLE >> 8) as u8]; e.extend_from_slice(&addr); e.extend_from_slice(&[1, 0]);
                match self.device(&addr).filter(|&i| self.link.is_none() && (self.devices[i].pairing_mode || self.devices[i].key.is_some())) {
                    Some(device) => { self.link = Some(Link::new(device, false)); self.event_at(50 + self.link_delay, 0x03, &e); }
                    None => { e[0] = 0x04; self.event_at(self.page_ms, 0x03, &e); }
                }
            }
            0x0408 => {
                // Create Connection Cancel: a page still under way ends with Unknown Connection Identifier.
                let pending = self.timed.iter().position(|t| t.1 == h5::EVT && t.2[0] == 0x03 && t.2.get(5..11) == Some(&addr[..]));
                let mut r = vec![if pending.is_some() { 0 } else { 0x02 }]; r.extend_from_slice(&addr);
                self.complete(opcode, &r);
                if let Some(at) = pending {
                    let mut e = self.timed.remove(at).2[2..].to_vec();
                    e[0] = 0x02;
                    if self.link.as_ref().is_some_and(|l| !l.initiator && !l.encrypted) { self.link = None; }
                    self.event_at(5, 0x03, &e);
                }
            }
            0x0409 => {
                self.status(opcode, 0);
                let mut e = vec![0, HANDLE as u8, (HANDLE >> 8) as u8]; e.extend_from_slice(&addr); e.extend_from_slice(&[1, 0]);
                self.event_at(20, 0x03, &e);
                // The headphones authenticate with their stored key, then open the stream.
                self.event_at(40, 0x17, &addr);
            }
            0x040a => { self.status(opcode, 0); self.link = None; }
            0x0411 => { self.status(opcode, 0); if let Some(link) = &self.link { let a = self.devices[link.device].addr; self.event_at(10, 0x17, &a); } }
            0x040b => {
                self.complete(opcode, &[0]);
                let Some(link) = &self.link else { return };
                let (initiator, device) = (link.initiator, &self.devices[link.device]);
                let status = match device.key { Some(key) if p.get(6..22) == Some(&key[..]) => 0, Some(_) => 0x05, None => 0x06 };
                if initiator {
                    if status == 0 { self.encrypted(); } else { self.event_at(10, 0x05, &[0, HANDLE as u8, 0, 0x05]); self.link = None; }
                } else { self.event_at(10, 0x06, &[status, HANDLE as u8, 0]); }
            }
            0x040c => {
                self.complete(opcode, &[0]);
                let Some(link) = &self.link else { return };
                let device = &self.devices[link.device];
                if link.initiator || !device.pairing_mode { self.event_at(10, 0x06, &[0x06, HANDLE as u8, 0]); }
                else if device.simple_pairing { self.event_at(10, 0x31, &addr); } else { self.event_at(10, 0x16, &addr); }
            }
            0x042b => {
                self.complete(opcode, &[0]);
                let mut e = addr.clone(); e.extend_from_slice(&[3, 0, 4]);
                self.event_at(5, 0x32, &e);
                if self.link.as_ref().is_some_and(|l| self.devices[l.device].stalls_pairing) { return }
                let mut e = addr.clone(); e.extend_from_slice(&[0x40, 0xe2, 0x01, 0x00]);
                self.event_at(10 + self.link_delay, 0x33, &e);
            }
            0x042c | 0x040d => {
                self.complete(opcode, &[0]);
                let Some(link) = &self.link else { return };
                let device = link.device;
                if opcode == 0x040d && p.get(6..11) != Some(&[4, b'0', b'0', b'0', b'0'][..]) { self.event_at(10, 0x06, &[0x05, HANDLE as u8, 0]); return }
                let key: [u8; 16] = std::array::from_fn(|i| (i as u8).wrapping_mul(17) ^ self.devices[device].addr[0] ^ self.log.commands.len() as u8);
                self.devices[device].key = Some(key);
                if opcode == 0x042c { let mut e = vec![0]; e.extend_from_slice(&addr); self.event_at(5, 0x36, &e); }
                let mut e = addr.clone(); e.extend_from_slice(&key); e.push(if opcode == 0x042c { 4 } else { 0 });
                self.event_at(10, 0x18, &e);
                self.event_at(15, 0x06, &[0, HANDLE as u8, 0]);
            }
            0x0413 => { self.status(opcode, 0); self.encrypted(); }
            0x0406 => {
                self.status(opcode, 0);
                if self.link.take().is_some() { self.log.streaming = false; self.event_at(10, 0x05, &[0, HANDLE as u8, 0, 0x16]); }
            }
            _ => self.complete(opcode, &[0]),
        }
    }

    fn encrypted(&mut self) {
        self.event_at(10, 0x08, &[0, HANDLE as u8, 0, 1]);
        let Some(link) = &mut self.link else { return };
        link.encrypted = true;
        if link.initiator { link.step = 1; }
    }

    /// The headphones are switched on and page the player they are bonded to.
    pub fn reconnect(&mut self, device: usize) -> bool {
        if !self.page_scan || self.link.is_some() || !self.devices[device].powered || self.devices[device].key.is_none() { return false }
        self.link = Some(Link::new(device, true));
        let mut e = self.devices[device].addr.to_vec(); e.extend_from_slice(&[0x04, 0x04, 0x24, 1]);
        self.event(0x04, &e);
        true
    }
    /// The headphones go out of range or are switched off: supervision timeout.
    pub fn drop_link(&mut self) {
        if self.link.take().is_some() { self.log.streaming = false; self.event(0x05, &[0, HANDLE as u8, 0, 0x08]); }
    }
    /// A button on the headphones (AV/C operation id, for example 0x44 play, 0x46 pause, 0x4b forward).
    pub fn press(&mut self, id: u8) -> bool {
        let Some(link) = &mut self.link else { return false };
        let Some(remote) = link.channel(Role::Control) else { return false };
        link.label = (link.label + 1) & 0xf;
        let (down, up) = (avrcp::pass_through(link.label, id, false), avrcp::pass_through(link.label, id, true));
        self.l2cap(remote, &down); self.l2cap(remote, &up);
        true
    }
    /// Sends one AVCTP packet on the control channel (a command of the headphones' own).
    pub fn send_control(&mut self, packet: &[u8]) -> bool {
        let Some(remote) = self.link.as_ref().and_then(|link| link.channel(Role::Control)) else { return false };
        self.l2cap(remote, packet);
        true
    }
    /// The headphones' volume buttons. With absolute volume they change their own
    /// level and notify the source's registration; without it they send the
    /// AV/C pass-through volume up or down. False if there is nothing to send it on.
    pub fn volume_button(&mut self, up: bool) -> bool {
        let Some(link) = &mut self.link else { return false };
        let Some(remote) = link.channel(Role::Control) else { return false };
        let device = &mut self.devices[link.device];
        if !device.absolute_volume { return self.press(if up { 0x41 } else { 0x42 }) }
        let Some(label) = link.volume_label.take() else { return false };
        device.volume = if up { device.volume.saturating_add(8).min(127) } else { device.volume.saturating_sub(8) };
        let changed = avrcp::response(label, avrcp::CODES.2, 0x31, &[avrcp::VOLUME_CHANGED, device.volume]);
        self.l2cap(remote, &changed);
        true
    }
    /// Sends one SDP request PDU over a new channel, as headphones do before connecting AVRCP.
    pub fn sdp_query(&mut self, request: Vec<u8>) -> bool {
        let Some(link) = &mut self.link else { return false };
        link.pending_sdp = Some(request);
        link.connect(Role::Sdp, 1).map(|pdu| self.signal_raw(pdu)).is_some()
    }

    fn l2cap(&mut self, cid: u16, payload: &[u8]) {
        let mut pdu = (payload.len() as u16).to_le_bytes().to_vec(); pdu.extend_from_slice(&cid.to_le_bytes()); pdu.extend_from_slice(payload);
        // Fragment as a controller with small buffers would.
        for (i, chunk) in pdu.chunks(48).enumerate() {
            let mut packet = (HANDLE | if i == 0 { 0x2000 } else { 0x1000 }).to_le_bytes().to_vec();
            packet.extend_from_slice(&(chunk.len() as u16).to_le_bytes()); packet.extend_from_slice(chunk);
            self.timed.push((self.now, h5::ACL, packet));
        }
    }
    fn signal_raw(&mut self, pdu: Vec<u8>) { self.l2cap(1, &pdu); }
    fn signal(&mut self, code: u8, id: u8, data: &[u8]) {
        let mut pdu = vec![code, id, data.len() as u8, 0]; pdu.extend_from_slice(data);
        self.signal_raw(pdu);
    }

    fn acl(&mut self, flags: u8, data: &[u8]) {
        let Some(link) = &mut self.link else { return };
        if flags & 3 == 2 { link.reassembly.clear(); }
        link.reassembly.extend_from_slice(data);
        if link.reassembly.len() < 4 { return }
        let len = u16::from_le_bytes([link.reassembly[0], link.reassembly[1]]) as usize;
        if link.reassembly.len() < 4 + len { return }
        let pdu = std::mem::take(&mut link.reassembly);
        let (cid, payload) = (u16::from_le_bytes([pdu[2], pdu[3]]), pdu[4..4 + len].to_vec());
        if cid == 1 { self.signalling(&payload); return }
        let Some((role, remote)) = link.channels.iter().find(|c| c.local == cid).map(|c| (c.role, c.remote)) else { return };
        match role {
            Role::Signal => self.avdtp(remote, &payload),
            Role::Media => self.media(&payload),
            Role::Control if avrcp::is_response(&payload) => self.log.avrcp_responses.push(payload),
            Role::Control => self.avrcp_target(remote, &payload),
            Role::Sdp => self.log.sdp_responses.push(payload),
            Role::SdpServer => self.sdp_serve(remote, &payload),
        }
    }

    /// The headphones as an AVRCP target: events, volume registration and absolute volume.
    fn avrcp_target(&mut self, remote: u16, packet: &[u8]) {
        let Some(link) = &mut self.link else { return };
        let Some((label, _ctype, pdu, parameters)) = avrcp::parse_command(packet) else { return };
        self.log.avrcp_commands.push(pdu);
        let device = &mut self.devices[link.device];
        let (accepted, stable, changed, interim) = avrcp::CODES;
        let mut echo = None;
        let response = match (pdu, parameters.first().copied()) {
            (0x10, Some(3)) => avrcp::response(label, stable, pdu, &if device.absolute_volume { vec![3, 2, 0x01, avrcp::VOLUME_CHANGED] } else { vec![3, 1, 0x01] }),
            (0x31, Some(avrcp::VOLUME_CHANGED)) if device.absolute_volume => {
                link.volume_label = Some(label);
                avrcp::response(label, interim, pdu, &[avrcp::VOLUME_CHANGED, device.volume])
            }
            (0x50, Some(level)) if device.absolute_volume => {
                let level = match device.volume_steps {
                    Some(steps) if steps > 0 => { let step = (u32::from(level & 0x7f) * u32::from(steps) + 63) / 127; ((step * 127 + u32::from(steps) / 2) / u32::from(steps)) as u8 }
                    _ => level & 0x7f,
                };
                device.volume = level;
                self.log.volumes_set.push(level);
                if device.echo_volume { echo = link.volume_label.take().map(|l| avrcp::response(l, changed, 0x31, &[avrcp::VOLUME_CHANGED, level])); }
                avrcp::response(label, accepted, pdu, &[level])
            }
            // Not implemented.
            _ => avrcp::response(label, 0x8, pdu, &[]),
        };
        self.l2cap(remote, &response);
        if let Some(echo) = echo { self.l2cap(remote, &echo); }
    }

    /// The device's SDP server: a service search finds its A2DP sink record when it has one;
    /// an attribute search for the Device ID record returns its vendor and product.
    fn sdp_serve(&mut self, remote: u16, request: &[u8]) {
        let Some(link) = &self.link else { return };
        let (sink, id) = (self.devices[link.device].sdp_sink, self.devices[link.device].device_id);
        let tid = [request.get(1).copied().unwrap_or(0), request.get(2).copied().unwrap_or(0)];
        let response = if request.first() == Some(&0x06) && request.windows(3).any(|w| w == [0x19, 0x12, 0x00]) {
            self.log.device_id_queries += 1;
            sdp::attribute_response(u16::from_be_bytes(tid), &id.map(sdp::device_id_attributes).into_iter().collect::<Vec<_>>())
        } else if request.first() == Some(&0x02) {
            self.log.sdp_searches += 1;
            let asks_sink = request.windows(3).any(|w| w == [0x19, 0x11, 0x0b]);
            let handles: &[u32] = if asks_sink && sink { &[0x0001_0005] } else { &[] };
            let mut body = vec![0, handles.len() as u8, 0, handles.len() as u8];
            for h in handles { body.extend_from_slice(&h.to_be_bytes()); }
            body.push(0);
            if self.mute_sdp { return }
            let mut out = vec![0x03, tid[0], tid[1], 0, body.len() as u8]; out.extend(body);
            out
        } else { vec![0x01, tid[0], tid[1], 0, 2, 0, 3] };
        self.l2cap(remote, &response);
    }

    fn signalling(&mut self, pdu: &[u8]) {
        if pdu.len() < 4 { return }
        let (code, id, data) = (pdu[0], pdu[1], &pdu[4..]);
        let word = |i: usize| u16::from_le_bytes([data.get(i).copied().unwrap_or(0), data.get(i + 1).copied().unwrap_or(0)]);
        let Some(link) = &mut self.link else { return };
        let mtu = self.devices[link.device].media_mtu;
        match code {
            0x02 => {
                let (psm, remote) = (word(0), word(2));
                let role = match psm { 0x19 if link.channels.iter().any(|c| c.role == Role::Signal) => Role::Media, 0x19 => Role::Signal, 0x17 => Role::Control, 0x01 => Role::SdpServer, _ => {
                    let mut r = vec![0, 0]; r.extend_from_slice(&remote.to_le_bytes()); r.extend_from_slice(&[2, 0, 0, 0]);
                    return self.signal(0x03, id, &r);
                } };
                if !link.encrypted && role != Role::SdpServer {
                    // Security block.
                    let mut r = vec![0, 0]; r.extend_from_slice(&remote.to_le_bytes()); r.extend_from_slice(&[3, 0, 0, 0]);
                    return self.signal(0x03, id, &r);
                }
                let local = link.next_cid; link.next_cid += 1;
                link.channels.push(Channel { local, remote, role, open: false, configured: (false, false) });
                let mut r = local.to_le_bytes().to_vec(); r.extend_from_slice(&remote.to_le_bytes()); r.extend_from_slice(&[0, 0, 0, 0]);
                self.signal(0x03, id, &r);
                let mut c = remote.to_le_bytes().to_vec(); c.extend_from_slice(&[0, 0, 1, 2]); c.extend_from_slice(&mtu.to_le_bytes());
                self.signal(0x04, 0x70 + local as u8, &c);
            }
            0x03 => {
                let (remote, local, result) = (word(0), word(2), word(4));
                if result != 0 { return }
                if let Some(c) = link.channels.iter_mut().find(|c| c.local == local) { c.remote = remote; }
                let mut c = remote.to_le_bytes().to_vec(); c.extend_from_slice(&[0, 0, 1, 2]); c.extend_from_slice(&mtu.to_le_bytes());
                self.signal(0x04, 0x70 + local as u8, &c);
            }
            0x04 => {
                let Some(c) = link.channels.iter_mut().find(|c| c.local == word(0)) else { return };
                c.configured.0 = true;
                let mut r = c.remote.to_le_bytes().to_vec(); r.extend_from_slice(&[0, 0, 0, 0]);
                self.signal(0x05, id, &r);
                self.opened();
            }
            0x05 => {
                if let Some(c) = link.channels.iter_mut().find(|c| c.local == word(0)) { c.configured.1 = true; }
                self.opened();
            }
            0x06 => {
                let (local, remote) = (word(0), word(2));
                link.channels.retain(|c| c.local != local);
                let mut r = local.to_le_bytes().to_vec(); r.extend_from_slice(&remote.to_le_bytes());
                self.signal(0x07, id, &r);
            }
            _ => {}
        }
    }

    fn opened(&mut self) {
        let Some(link) = &mut self.link else { return };
        let Some(channel) = link.channels.iter_mut().find(|c| !c.open && c.configured == (true, true)) else { return };
        channel.open = true;
        let (role, remote) = (channel.role, channel.remote);
        match role {
            Role::Sdp => if let Some(request) = link.pending_sdp.take() { self.l2cap(remote, &request); },
            // As initiator: discover once signalling is open; start or stop once media is.
            Role::Signal if link.initiator => { link.step = 2; self.l2cap(remote, &[0x10, 1]); }
            Role::Media if link.initiator => {
                let seid = link.acp << 2;
                let signal = link.channel(Role::Signal).unwrap();
                let start = self.devices[link.device].starts_stream;
                if start { link.step = 6; }
                let control = link.connect(Role::Control, 0x17);
                if start { self.l2cap(signal, &[0x50, 7, seid]); }
                if let Some(pdu) = control { self.signal_raw(pdu); }
            }
            _ => {}
        }
    }

    fn avdtp(&mut self, remote: u16, packet: &[u8]) {
        if packet.len() < 2 || self.mute_signalling { return }
        let Some(link) = &mut self.link else { return };
        let device = &self.devices[link.device];
        let (label, kind, signal, body) = (packet[0] >> 4, packet[0] & 3, packet[1] & 0x3f, &packet[2..]);
        if kind == 0 {
            let accept = label << 4 | 2;
            let mut response = vec![accept, signal];
            match signal {
                1 => for i in 0..=device.codecs.len() as u8 { response.extend_from_slice(&[(5 + i) << 2, 0x08]); },
                2 | 12 => match body.first().map(|seid| (seid >> 2) as usize) {
                    Some(5) => { response.extend_from_slice(&[1, 0, 7, 6, 0, 0]); response.extend_from_slice(&device.sbc); }
                    Some(seid) if seid > 5 && seid - 6 < device.codecs.len() => {
                        let element = &device.codecs[seid - 6];
                        response.extend_from_slice(&[1, 0, 7, element.len() as u8]); response.extend_from_slice(element);
                    }
                    _ => response = vec![label << 4 | 3, signal, 0x12],
                },
                3 => {
                    let (seid, element) = ((body.first().copied().unwrap_or(0) >> 2) as usize, body.get(6..).unwrap_or(&[]).to_vec());
                    let sbc: Option<[u8; 4]> = element.get(2..6).and_then(|c| c.try_into().ok()).filter(|_| seid == 5 && element.len() == 6);
                    let other = seid > 5 && device.codecs.get(seid - 6).is_some_and(|offer| {
                        crate::codec::identify(offer).map(|o| o.0) == crate::codec::identify(&element).map(|c| c.0) && crate::codec::identify(&element).is_some()
                    });
                    match sbc.filter(|c| c[0] & device.sbc[0] == c[0] && c[1] & device.sbc[1] == c[1] && c[3] <= device.sbc[3]) {
                        Some(codec) => { self.log.configured = Some(codec); self.log.configured_element = element; }
                        None if other => { self.log.configured = None; self.log.configured_element = element; }
                        None => response = vec![label << 4 | 3, signal, 7, 0x29],
                    }
                }
                6 => {}
                7 => self.log.streaming = true,
                9 => self.log.streaming = false,
                8 | 10 => { self.log.streaming = false; self.log.configured = None; }
                _ => response = vec![label << 4 | 1, signal],
            }
            return self.l2cap(remote, &response);
        }
        // Responses to the headphones' own configuration (initiator after a reconnect).
        if kind != 2 || !link.initiator { return }
        // Headphones with another codec configure that one on the source's endpoint for it; else SBC.
        let preferred = device.codecs.first().filter(|_| !device.configures_sbc).and_then(|offer| crate::codec::choose(offer, false).map(|c| (c.kind().seid(), crate::codec::configuration(&c, offer))));
        if link.step == 2 { link.acp = preferred.as_ref().map_or(1, |p| p.0); }
        let seid = link.acp << 2;
        match (link.step, signal) {
            (2, 1) => { link.step = 3; self.l2cap(remote, &[0x20, 2, seid]); }
            (3, 2) => {
                link.step = 4;
                let element = match preferred {
                    Some((_, element)) => { self.log.configured = None; element }
                    None => {
                        let codec = [0x21, 0x15, 2, device.sbc[3].min(body.get(9).copied().unwrap_or(53))];
                        self.log.configured = Some(codec);
                        [&[0, 0][..], &codec].concat()
                    }
                };
                self.log.configured_element = element.clone();
                let mut p = vec![0x30, 3, seid, 5 << 2, 1, 0, 7, element.len() as u8]; p.extend(element);
                self.l2cap(remote, &p);
            }
            (4, 3) => { link.step = 5; self.l2cap(remote, &[0x40, 6, seid]); }
            (5, 6) => if let Some(pdu) = link.connect(Role::Media, 0x19) { self.signal_raw(pdu); },
            (6, 7) => self.log.streaming = true,
            _ => {}
        }
    }

    fn media(&mut self, packet: &[u8]) {
        // aptX is sent without an RTP header.
        if self.log.configured_element.get(1..8) == Some(&[0xff, 0x4f, 0x00, 0x00, 0x00, 0x01, 0x00]) {
            self.log.media_packets += 1;
            if self.log.media.len() < 4 << 20 { self.log.media.extend_from_slice(packet); }
            return;
        }
        if packet.len() < 13 || packet[0] != 0x80 || packet[1] & 0x7f != 0x60 { return }
        if packet[1] & 0x80 != 0 { self.log.markers += 1; }
        if self.log.media.len() < 4 << 20 { self.log.media.extend_from_slice(&packet[12..]); }
        let Some(link) = &mut self.link else { return };
        let sequence = u16::from_be_bytes([packet[2], packet[3]]);
        if link.expected_sequence.is_some_and(|e| e != sequence) { self.log.media_sequence_gaps += 1; }
        link.expected_sequence = Some(sequence.wrapping_add(1));
        self.log.last_timestamp = u32::from_be_bytes(packet[4..8].try_into().unwrap());
        self.log.media_frames = packet[12] & 0x0f;
        self.log.media_packets += 1;
        self.log.sbc_frames += u32::from(packet[12] & 0x0f);
        if self.log.sbc.len() < 4 << 20 { self.log.sbc.extend_from_slice(&packet[13..]); }
    }
}

impl Link {
    fn new(device: usize, initiator: bool) -> Self {
        Self { device, handle: HANDLE, initiator, encrypted: false, channels: Vec::new(), reassembly: Vec::new(), next_cid: 0x0060, id: 0, label: 0,
            expected_sequence: None, step: 0, pending_sdp: None, acp: 1, volume_label: None }
    }
    fn channel(&self, role: Role) -> Option<u16> { self.channels.iter().find(|c| c.role == role && c.open).map(|c| c.remote) }
    /// The connection request for a new channel.
    fn connect(&mut self, role: Role, psm: u16) -> Option<Vec<u8>> {
        let _ = self.handle;
        let local = self.next_cid; self.next_cid += 1;
        self.channels.push(Channel { local, remote: 0, role, open: false, configured: (false, false) });
        self.id += 1;
        let mut pdu = vec![0x02, self.id, 4, 0]; pdu.extend_from_slice(&psm.to_le_bytes()); pdu.extend_from_slice(&local.to_le_bytes());
        Some(pdu)
    }
}

impl Fake {
    /// Advances the headphones' own connection steps (initiator role).
    fn step(&mut self) {
        let Some(link) = &mut self.link else { return };
        if link.initiator && link.encrypted && link.step == 1 {
            link.step = 0;
            if let Some(pdu) = link.connect(Role::Signal, 0x19) { self.signal_raw(pdu); }
        }
    }
}
