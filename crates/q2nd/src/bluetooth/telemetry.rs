#![forbid(unsafe_code)]
//! The Bluetooth numbers UX reports carry (`ux::bluetooth`) and the
//! flight-recorder marks for stalls, transport errors, LDAC rate changes and
//! state changes. Numbers and fixed codes only (docs/telemetry.md).
use super::media::Media;
use super::{uart, Link, Phase, State};
use crate::ux::{self, bluetooth::Bluetooth, Mark};
use q2_bt::codec::Kind;
use q2_bt::host::{Host, LinkStats};

/// How often the published numbers are refreshed, and the UART counters read.
const PUBLISH_MS: u64 = 250;
const ERRORS_MS: u64 = 1000;
/// Transport marks: at most one of each kind this often.
const TRANSPORT_MARK_MS: u64 = 1000;

/// The state code of `ux::bluetooth::Bluetooth::state`.
pub(super) fn state_code(state: &State) -> u8 {
    match (&state.phase, &state.link) {
        (Phase::Off, _) => 0,
        (Phase::Starting, _) => 1,
        (Phase::Failed(_), _) => 7,
        (Phase::Ready, Link::Idle) => 2,
        (Phase::Ready, Link::Connecting(_)) => 3,
        (Phase::Ready, Link::Connected(_)) if state.stream.is_none() => 4,
        (Phase::Ready, Link::Connected(_)) => 5,
        (Phase::Ready, Link::Playing(_)) => 6,
    }
}
pub(super) fn codec_code(kind: Kind) -> u8 {
    match kind { Kind::Sbc => 1, Kind::Aac => 2, Kind::AptX => 3, Kind::AptXHd => 4, Kind::Ldac => 5 }
}

/// What the previous marks were computed from.
#[derive(Default)]
struct Seen { resends: u32, bad: u32, out_of_sequence: u32, uart: u32, credit_stalls: u32 }

pub(super) struct Telemetry {
    baud: u32,
    flow: bool,
    adaptive: bool,
    /// Bring-ups that failed at their UART setting and were tried at the next one (`uart_settings`).
    uart_fallbacks: u32,
    publish_at: u64,
    errors_at: u64,
    /// UART counts when Bluetooth started, and since then.
    uart_base: Option<uart::Errors>,
    uart: Option<uart::Errors>,
    seen: Seen,
    /// When each transport mark (tags 0–3) was last recorded.
    marked_at: [Option<u64>; 4],
    state: (u8, u8),
    /// Late wakes (20 ms or more), the worst, and the longest UART write, in ms.
    wake_late: u32,
    wake_late_max_ms: u32,
    write_max_ms: u32,
}

/// A wake this late counts as starvation; one this late is also marked.
const WAKE_LATE_MS: u64 = 20;
const WAKE_MARK_MS: u64 = 50;

impl Telemetry {
    pub(super) fn new(baud: u32, flow: bool, adaptive: bool, uart_fallbacks: u32) -> Self {
        Self { baud, flow, adaptive, uart_fallbacks, publish_at: 0, errors_at: 0, uart_base: None, uart: None, seen: Seen::default(), marked_at: [None; 4], state: (0, 0), wake_late: 0, wake_late_max_ms: 0, write_max_ms: 0 }
    }

    /// The thread woke `late_ms` after the time it asked for (a timed-out wait).
    pub(super) fn woke(&mut self, late_ms: u64) {
        if late_ms < WAKE_LATE_MS { return }
        self.wake_late += 1;
        self.wake_late_max_ms = self.wake_late_max_ms.max(late_ms.min(u64::from(u32::MAX)) as u32);
        if late_ms >= WAKE_MARK_MS { ux::record(Mark::BtStall, ux::tagged(3, late_ms)); }
    }
    /// A UART write took `ms`.
    pub(super) fn wrote(&mut self, ms: u64) {
        let ms = ms.min(u64::from(u32::MAX)) as u32;
        if ms > self.write_max_ms { self.write_max_ms = ms; if ms >= WAKE_MARK_MS as u32 { ux::record(Mark::BtStall, ux::tagged(4, ms.into())); } }
    }

    /// The UART's error counts, from the wire, now and then.
    pub(super) fn uart_errors(&mut self, now_ms: u64, read: impl FnOnce() -> Option<uart::Errors>) {
        if now_ms < self.errors_at { return }
        self.errors_at = now_ms + ERRORS_MS;
        let Some(now) = read() else { return };
        let base = *self.uart_base.get_or_insert(now);
        self.uart = Some(now.since(base));
    }

    /// Whether the numbers are due to be published (every [`PUBLISH_MS`]).
    pub(super) fn due(&mut self, now_ms: u64) -> bool {
        if now_ms < self.publish_at { return false }
        self.publish_at = now_ms + PUBLISH_MS;
        true
    }

    /// Marks what happened since the last call and, given the state, publishes the numbers.
    pub(super) fn update(&mut self, now_ms: u64, host: &Host, media: Option<&mut Media>, state: Option<State>, dropped: u32, underruns: u32) {
        let link = host.link_stats();
        let mut stats = None;
        if let Some(media) = media {
            while let Some((ms, restarted)) = media.take_stall() { ux::record(Mark::BtStall, ux::tagged(u32::from(restarted), ms.into())); }
            for (level, kbps) in media.take_level_changes() { ux::record(Mark::BtRate, ux::tagged(level.into(), kbps.into())); }
            stats = Some(media.stats());
        }
        if link.credit_stalls != self.seen.credit_stalls {
            self.seen.credit_stalls = link.credit_stalls;
            ux::record(Mark::BtStall, ux::tagged(2, link.credit_stall_last_ms.into()));
        }
        self.transport(now_ms, &link);
        let Some(state) = state else { return };
        let stream = state.stream;
        let code = (state_code(&state), stream.map_or(0, |s| codec_code(s.codec.kind())));
        if code != self.state {
            self.state = code;
            ux::record(Mark::BtState, ux::tagged(code.0.into(), code.1.into()));
        }
        let media = stats.unwrap_or_default();
        let uart = self.uart;
        let device = state.connected().and_then(|addr| state.paired.iter().chain(&state.found).find(|d| d.addr == addr));
        // The Device ID the player keeps for the device (the one SDP read after a stream opens, docs/bluetooth.md).
        let id = state.output.as_ref().and_then(|o| o.saved.id);
        let bitpool = stream.and_then(|s| match s.codec { q2_bt::codec::Codec::Sbc(p) => Some(p.bitpool), _ => None });
        ux::bluetooth::publish(&Bluetooth {
            state: code.0, codec: code.1, rate: stream.map_or(0, |s| s.codec.rate()), kbps: if stream.is_some() { media.kbps } else { 0 },
            ldac_level: media.ldac_level, ldac_adaptive: self.adaptive, baud: self.baud, flow_control: self.flow, uart_fallbacks: self.uart_fallbacks,
            rssi: link.rssi, link_quality: link.link_quality, acl_buffers: link.acl_buffers.into(), acl_in_use: link.acl_in_use.into(),
            h5_retransmissions: link.h5_retransmissions, h5_resends: link.h5_resends, h5_bad_frames: link.h5_bad_frames, h5_out_of_sequence: link.h5_out_of_sequence,
            credit_stalls: link.credit_stalls, credit_stall_ms: link.credit_stall_ms, credit_stall_max_ms: link.credit_stall_max_ms,
            late: media.late, restarts: media.restarts, behind_max_ms: media.behind_max_ms, ldac_changes: media.ldac_changes, queue_max: media.queue_max,
            dropped, underruns,
            uart_frame: uart.map(|u| u.frame), uart_overrun: uart.map(|u| u.overrun), uart_parity: uart.map(|u| u.parity), uart_buffer: uart.map(|u| u.buffer),
            device_class: device.map(|d| d.class), device_name: device.map(|d| ux::bluetooth::report_name(&d.name)),
            device_vendor_source: id.map(|i| i.source.into()), device_vendor: id.map(|i| i.vendor.into()), device_product: id.map(|i| i.product.into()),
            device_version: id.map(|i| i.version.into()), tx_power: link.tx_power, afh_channels: link.afh_channels,
            mtu: stream.map_or(0, |s| s.mtu as u32), sbc_bitpool: bitpool,
            wake_late: self.wake_late, wake_late_max_ms: self.wake_late_max_ms, write_max_ms: self.write_max_ms, encode_max_us: media.encode_max_us,
        });
    }

    /// One mark per kind of transport error, at most each second, with the count since the last.
    fn transport(&mut self, now_ms: u64, link: &LinkStats) {
        let uart = self.uart.map_or(0, uart::Errors::total);
        let counts = [(link.h5_resends, self.seen.resends), (link.h5_bad_frames, self.seen.bad), (link.h5_out_of_sequence, self.seen.out_of_sequence), (uart, self.seen.uart)];
        for (tag, (now, before)) in counts.into_iter().enumerate() {
            if now == before || self.marked_at[tag].is_some_and(|at| now_ms < at + TRANSPORT_MARK_MS) { continue }
            self.marked_at[tag] = Some(now_ms);
            ux::record(Mark::BtTransport, ux::tagged(tag as u32, now.wrapping_sub(before).into()));
            match tag { 0 => self.seen.resends = now, 1 => self.seen.bad = now, 2 => self.seen.out_of_sequence = now, _ => self.seen.uart = now }
        }
    }

    /// The UART's receive errors since Bluetooth started.
    pub(super) fn uart(&self) -> Option<uart::Errors> { self.uart }
}

/// Bluetooth stopped: reports carry no Bluetooth part.
pub(super) fn off() {
    ux::record(Mark::BtState, ux::tagged(0, 0));
    ux::bluetooth::clear();
}
