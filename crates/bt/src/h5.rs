//! Three-wire UART transport (Bluetooth Core, volume 4 part D; "H5").
//! SLIP framing, link establishment, sequence numbers, acknowledgement and
//! retransmission. Data integrity check (CRC) is not requested; a received
//! CRC is dropped unchecked, as Linux's hci_h5 does.
use std::collections::VecDeque;

pub const ACK: u8 = 0;
pub const CMD: u8 = 1;
pub const ACL: u8 = 2;
pub const EVT: u8 = 4;
pub const LINK: u8 = 15;
const SYNC: [u8; 2] = [0x01, 0x7e];
const SYNC_RSP: [u8; 2] = [0x02, 0x7d];
const CONFIG: [u8; 2] = [0x03, 0xfc];
const CONFIG_RSP: [u8; 2] = [0x04, 0x7b];
const WAKEUP: [u8; 2] = [0x05, 0xfa];
const WOKEN: [u8; 2] = [0x06, 0xf9];
const SLEEP: [u8; 2] = [0x07, 0x78];
const WAKE_MS: u64 = 50;
const WINDOW: u8 = 4;
const LINK_MS: u64 = 250;
const RESEND_MS: u64 = 250;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Link { Uninitialized, Initialized, Active }

pub struct H5 {
    link: Link,
    frame: Vec<u8>,
    escaped: bool,
    /// Sequence number of the next new reliable packet.
    seq: u8,
    /// Sequence number expected from the peer; sent as our acknowledgement.
    expect: u8,
    ack_due: bool,
    window: u8,
    unacked: VecDeque<(u8, u8, Vec<u8>)>,
    queue: VecDeque<(u8, Vec<u8>)>,
    link_at: u64,
    resend_at: u64,
    control: VecDeque<Vec<u8>>,
    /// The peer restarted link establishment while the link was active.
    pub peer_reset: bool,
    /// Reliable packets sent again (each packet of each resend counts).
    pub retransmissions: u32,
    /// Resend timeouts: each is one stall of `resend_ms` with packets unacknowledged.
    pub resends: u32,
    /// Frames dropped: the sum of `bad_frames` and `out_of_sequence`.
    pub rejected: u32,
    /// Frames with a bad header checksum, a length that disagrees with the header,
    /// a bad SLIP escape or no end (bytes lost or corrupted on the line).
    pub bad_frames: u32,
    /// Reliable packets that arrived with another sequence number than expected
    /// (a packet before them was lost, or a duplicate of one already taken).
    pub out_of_sequence: u32,
    /// How long an unacknowledged reliable packet waits before it is sent again.
    resend_ms: u64,
    /// The peer said it sleeps: it must be woken before it is sent anything.
    peer_asleep: bool,
    wake_at: u64,
    /// This side sleeps when idle for this long (the fake controller; Realtek controllers do).
    pub sleep_after: Option<u64>,
    asleep: bool,
    last_activity: u64,
    /// Sleep, wakeup and woken messages received.
    pub sleep_messages: [u32; 3],
}

fn slip(out: &mut Vec<u8>, bytes: &[u8]) {
    for &b in bytes {
        match b { 0xc0 => out.extend_from_slice(&[0xdb, 0xdc]), 0xdb => out.extend_from_slice(&[0xdb, 0xdd]), b => out.push(b) }
    }
}

impl H5 {
    pub fn new(now: u64) -> Self {
        Self { link: Link::Uninitialized, frame: Vec::new(), escaped: false, seq: 0, expect: 0, ack_due: false, window: WINDOW,
            unacked: VecDeque::new(), queue: VecDeque::new(), link_at: now, resend_at: 0, control: VecDeque::new(),
            peer_reset: false, retransmissions: 0, resends: 0, rejected: 0, bad_frames: 0, out_of_sequence: 0, resend_ms: RESEND_MS, peer_asleep: false, wake_at: 0, sleep_after: None, asleep: false,
            last_activity: now, sleep_messages: [0; 3] }
    }
    pub fn link(&self) -> Link { self.link }
    /// Sets the resend timeout (at least 20 ms); 250 ms until called.
    pub fn set_resend_ms(&mut self, ms: u64) { self.resend_ms = ms.max(20); }
    /// Reliable packets sent and not yet acknowledged.
    pub fn unacked(&self) -> usize { self.unacked.len() }
    /// Packets accepted from the host and not yet acknowledged by the controller.
    pub fn backlog(&self) -> usize { self.queue.len() + self.unacked.len() }
    /// Starts link establishment again, keeping queued packets.
    pub fn restart(&mut self, now: u64) {
        let pending: Vec<_> = self.unacked.drain(..).map(|(_, ty, p)| (ty, p)).collect();
        for packet in pending.into_iter().rev() { self.queue.push_front(packet); }
        self.link = Link::Uninitialized; self.seq = 0; self.expect = 0; self.ack_due = false; self.link_at = now;
        self.frame.clear(); self.escaped = false; self.control.clear();
        self.peer_asleep = false; self.asleep = false; self.last_activity = now;
    }
    /// Queues one reliable packet (`CMD` or `ACL`).
    pub fn send(&mut self, ty: u8, payload: Vec<u8>) { self.queue.push_back((ty, payload)); }

    fn emit(&self, wire: &mut Vec<u8>, ty: u8, reliable: bool, seq: u8, payload: &[u8]) {
        let b0 = (if reliable { seq | 0x80 } else { 0 }) | (if self.link == Link::Active { self.expect << 3 } else { 0 });
        let b1 = ty | ((payload.len() & 0xf) as u8) << 4;
        let b2 = (payload.len() >> 4) as u8;
        let header = [b0, b1, b2, !(b0.wrapping_add(b1).wrapping_add(b2))];
        wire.push(0xc0); slip(wire, &header); slip(wire, payload); wire.push(0xc0);
    }

    /// Feeds bytes from the UART; complete HCI packets are appended to `out`.
    pub fn rx(&mut self, bytes: &[u8], now: u64, out: &mut Vec<(u8, Vec<u8>)>) {
        for &b in bytes {
            match b {
                0xc0 => {
                    if !self.frame.is_empty() { let frame = std::mem::take(&mut self.frame); self.packet(&frame, now, out); }
                    self.escaped = false;
                }
                0xdb => self.escaped = true,
                b if self.escaped => {
                    self.escaped = false;
                    match b { 0xdc => self.frame.push(0xc0), 0xdd => self.frame.push(0xdb), _ => { self.frame.clear(); self.bad_frame(); } }
                }
                b => if self.frame.len() < 4 + 4095 + 2 { self.frame.push(b) } else { self.frame.clear(); self.bad_frame(); },
            }
        }
    }

    fn bad_frame(&mut self) { self.bad_frames += 1; self.rejected += 1; }

    fn packet(&mut self, frame: &[u8], now: u64, out: &mut Vec<(u8, Vec<u8>)>) {
        if frame.len() < 4 || frame[..4].iter().fold(0u8, |a, &b| a.wrapping_add(b)) != 0xff { self.bad_frame(); return; }
        let (seq, ack, crc, reliable) = (frame[0] & 7, frame[0] >> 3 & 7, frame[0] & 0x40 != 0, frame[0] & 0x80 != 0);
        let (ty, len) = (frame[1] & 0xf, (frame[1] >> 4) as usize | (frame[2] as usize) << 4);
        if frame.len() != 4 + len + if crc { 2 } else { 0 } { self.bad_frame(); return; }
        let payload = &frame[4..4 + len];
        if ty == LINK { self.link_control(payload, now); return; }
        if self.link != Link::Active { return; }
        // A peer that sends is awake.
        self.peer_asleep = false; self.last_activity = now;
        // Acknowledge: `ack` is the next sequence number the peer expects.
        if let Some(n) = (0..=self.unacked.len()).find(|&n| self.unacked.get(n).map_or(self.seq, |p| p.0) == ack) {
            if n > 0 { self.unacked.drain(..n); self.resend_at = now + self.resend_ms; }
        }
        if reliable {
            self.ack_due = true;
            if seq != self.expect { self.out_of_sequence += 1; self.rejected += 1; return; }
            self.expect = (self.expect + 1) & 7;
        }
        if ty != ACK { out.push((ty, payload.to_vec())); }
    }

    fn link_control(&mut self, payload: &[u8], now: u64) {
        let Some(kind) = payload.get(..2) else { return };
        if kind == SYNC {
            if self.link == Link::Active { self.peer_reset = true; self.restart(now); }
            self.control.push_back(SYNC_RSP.to_vec());
        } else if kind == SYNC_RSP {
            if self.link == Link::Uninitialized { self.link = Link::Initialized; self.link_at = now; }
        } else if kind == CONFIG {
            if self.link != Link::Uninitialized { self.control.push_back(CONFIG_RSP.to_vec()); }
        } else if kind == CONFIG_RSP && self.link == Link::Initialized {
            if let Some(&field) = payload.get(2) { self.window = (field & 7).clamp(1, WINDOW); }
            self.link = Link::Active; self.resend_at = now + self.resend_ms; self.last_activity = now;
        } else if self.link == Link::Active && kind == SLEEP {
            self.sleep_messages[0] += 1; self.peer_asleep = true; self.wake_at = now;
        } else if self.link == Link::Active && kind == WAKEUP {
            // The peer wants to send (or wakes us): answer, or it waits for ever.
            self.sleep_messages[1] += 1; self.asleep = false; self.last_activity = now;
            self.control.push_back(WOKEN.to_vec());
        } else if self.link == Link::Active && kind == WOKEN {
            self.sleep_messages[2] += 1; self.peer_asleep = false; self.asleep = false; self.last_activity = now;
        }
    }

    /// Appends the bytes due now to `wire`; returns when to call again at the latest.
    pub fn poll(&mut self, now: u64, wire: &mut Vec<u8>) -> Option<u64> {
        while let Some(message) = self.control.pop_front() { self.emit(wire, LINK, false, 0, &message); }
        match self.link {
            Link::Uninitialized | Link::Initialized => {
                if now >= self.link_at {
                    let message: &[u8] = if self.link == Link::Uninitialized { &SYNC } else { &[CONFIG[0], CONFIG[1], WINDOW] };
                    self.emit(wire, LINK, false, 0, message);
                    self.link_at = now + LINK_MS;
                }
                Some(self.link_at)
            }
            Link::Active => {
                let waiting = !self.queue.is_empty() || !self.unacked.is_empty();
                if waiting && (self.peer_asleep || self.asleep) {
                    // Wake the peer (or, having slept ourselves, announce that we send again) and hold the data.
                    if now >= self.wake_at { self.emit(wire, LINK, false, 0, &WAKEUP); self.wake_at = now + WAKE_MS; }
                    return Some(self.wake_at);
                }
                if waiting || self.ack_due { self.last_activity = now; }
                if let Some(idle) = self.sleep_after.filter(|_| !self.asleep && !waiting && !self.ack_due) {
                    if now >= self.last_activity + idle { self.asleep = true; self.wake_at = now; self.emit(wire, LINK, false, 0, &SLEEP); }
                }
                let mut sent = false;
                if !self.unacked.is_empty() && now >= self.resend_at {
                    for (seq, ty, payload) in &self.unacked { self.emit(wire, *ty, true, *seq, payload); }
                    self.retransmissions += self.unacked.len() as u32;
                    self.resends += 1;
                    self.resend_at = now + self.resend_ms; sent = true;
                }
                while self.unacked.len() < self.window as usize {
                    let Some((ty, payload)) = self.queue.pop_front() else { break };
                    self.emit(wire, ty, true, self.seq, &payload);
                    if self.unacked.is_empty() { self.resend_at = now + self.resend_ms; }
                    self.unacked.push_back((self.seq, ty, payload));
                    self.seq = (self.seq + 1) & 7; sent = true;
                }
                if self.ack_due && !sent { self.emit(wire, ACK, false, 0, &[]); }
                self.ack_due = false;
                (!self.unacked.is_empty()).then_some(self.resend_at)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pump(a: &mut H5, b: &mut H5, now: u64, lose: &mut dyn FnMut(&[u8]) -> bool) -> (Vec<(u8, Vec<u8>)>, Vec<(u8, Vec<u8>)>) {
        let (mut to_a, mut to_b) = (Vec::new(), Vec::new());
        for _ in 0..4 {
            let (mut wa, mut wb) = (Vec::new(), Vec::new());
            a.poll(now, &mut wa); b.poll(now, &mut wb);
            if !lose(&wa) { b.rx(&wa, now, &mut to_b); }
            if !lose(&wb) { a.rx(&wb, now, &mut to_a); }
        }
        (to_a, to_b)
    }

    fn establish() -> (H5, H5, u64) {
        let (mut a, mut b) = (H5::new(0), H5::new(0));
        let mut now = 0;
        while a.link() != Link::Active || b.link() != Link::Active {
            pump(&mut a, &mut b, now, &mut |_| false); now += 50;
            assert!(now < 5000, "link establishment");
        }
        (a, b, now)
    }

    #[test]
    fn header_matches_the_specification_example() {
        let mut h5 = H5::new(0);
        h5.link = Link::Active;
        h5.send(CMD, vec![0x03, 0x0c, 0x00]);
        let mut wire = Vec::new();
        h5.poll(0, &mut wire);
        // Reliable, seq 0, ack 0, type 1, length 3; header bytes sum to 0xff.
        assert_eq!(wire, [0xc0, 0x80, 0x31, 0x00, 0x4e, 0x03, 0x0c, 0x00, 0xc0]);
    }

    #[test]
    fn slip_escapes_delimiters_in_headers_and_payloads() {
        let (mut a, mut b, now) = establish();
        let payload = vec![0xc0, 0xdb, 0xdc, 0xdd, 0x00, 0xc0];
        a.send(ACL, payload.clone());
        let mut wire = Vec::new();
        a.poll(now, &mut wire);
        assert_eq!(wire.iter().filter(|&&b| b == 0xc0).count(), 2);
        let mut out = Vec::new();
        b.rx(&wire, now, &mut out);
        assert_eq!(out, [(ACL, payload)]);
    }

    #[test]
    fn every_packet_arrives_once_in_order_through_loss() {
        let (mut a, mut b, mut now) = establish();
        let mut seed = 99u32;
        let mut lose = |wire: &[u8]| { seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223); !wire.is_empty() && seed >> 24 < 90 };
        let (mut got_a, mut got_b) = (Vec::new(), Vec::new());
        for n in 0..200u32 {
            a.send(CMD, n.to_le_bytes().to_vec());
            if n % 3 == 0 { b.send(EVT, (n / 3).to_le_bytes().to_vec()); }
            let (ra, rb) = pump(&mut a, &mut b, now, &mut lose);
            got_a.extend(ra); got_b.extend(rb); now += 100;
        }
        for _ in 0..100 {
            let (ra, rb) = pump(&mut a, &mut b, now, &mut lose);
            got_a.extend(ra); got_b.extend(rb); now += 300;
        }
        assert_eq!(got_b, (0..200u32).map(|n| (CMD, n.to_le_bytes().to_vec())).collect::<Vec<_>>());
        assert_eq!(got_a, (0..67u32).map(|n| (EVT, n.to_le_bytes().to_vec())).collect::<Vec<_>>());
        assert!(a.retransmissions > 0 && a.backlog() == 0 && b.backlog() == 0);
    }

    #[test]
    fn a_sleeping_peer_is_woken_before_data_and_its_wakeup_is_answered() {
        let (mut host, mut controller, mut now) = establish();
        controller.sleep_after = Some(100);
        let (mut to_host, mut to_controller) = (Vec::new(), Vec::new());
        for round in 0..400u32 {
            // The controller has an event after a long silence (an inquiry result), then the host a command.
            if round == 100 { controller.send(EVT, vec![0x22, 0]); }
            if round == 250 { host.send(CMD, vec![1, 4, 0]); }
            let (ra, rb) = pump(&mut host, &mut controller, now, &mut |_| false);
            to_host.extend(ra); to_controller.extend(rb); now += 10;
        }
        assert_eq!((to_host, to_controller), (vec![(EVT, vec![0x22, 0])], vec![(CMD, vec![1, 4, 0])]));
        // The host heard the sleeps and the wakeup; the controller heard the host's wakeup.
        assert!(host.sleep_messages[0] >= 2 && host.sleep_messages[1] >= 1 && host.sleep_messages[2] >= 1, "{:?}", host.sleep_messages);
        assert!(controller.sleep_messages[1] >= 1 && controller.sleep_messages[2] >= 1, "{:?}", controller.sleep_messages);
    }

    #[test]
    fn corrupt_frames_are_counted_and_dropped() {
        let (mut a, mut b, now) = establish();
        a.send(CMD, vec![1, 2, 3]);
        let mut wire = Vec::new();
        a.poll(now, &mut wire);
        let mut bad = wire.clone(); bad[2] ^= 0x10;
        let mut out = Vec::new();
        b.rx(&bad, now, &mut out);
        b.rx(&[0xc0, 0x00, 0xc0, 0xdb, 0x00, 0xc0], now, &mut out);
        assert!(out.is_empty() && b.rejected >= 2);
        assert_eq!((b.bad_frames, b.out_of_sequence), (b.rejected, 0));
        b.rx(&wire, now, &mut out);
        assert_eq!(out.len(), 1);
        // The same packet again (a resend whose first copy arrived) is out of sequence, not damaged.
        b.rx(&wire, now, &mut out);
        assert_eq!((out.len(), b.out_of_sequence), (1, 1));
    }

    #[test]
    fn a_peer_that_restarts_is_reported_and_the_link_recovers() {
        let (mut a, _, mut now) = establish();
        a.send(CMD, vec![7]);
        let mut b = H5::new(now);
        let mut got = Vec::new();
        for _ in 0..40 { got.extend(pump(&mut a, &mut b, now, &mut |_| false).1); now += 100; }
        assert!(a.peer_reset && a.link() == Link::Active && b.link() == Link::Active);
        assert_eq!(got, [(CMD, vec![7])]);
    }
}
