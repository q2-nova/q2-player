//! Bluetooth audio numbers carried by UX reports (`crate::bluetooth`
//! publishes them; [`super::send`] reads them). Numbers and fixed codes,
//! plus the connected headphones' advertised name. Never an address. 32-bit
//! atomics only, the name's bytes included: a report may see one torn update.
use std::sync::atomic::{AtomicU32, Ordering};

/// The Bluetooth side at the moment of a report. Counters from the Bluetooth
/// host and the UART are since Bluetooth was switched on; the stream's own
/// (`late` to `queue_max`) since the stream opened.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bluetooth {
    /// 1 starting, 2 ready, 3 connecting, 4 connected, 5 stream open, 6 streaming, 7 failed.
    pub state: u8,
    /// 0 no stream, 1 SBC, 2 AAC, 3 aptX, 4 aptX HD, 5 LDAC.
    pub codec: u8,
    /// The stream's sampling rate in Hz, and its bit rate now in kbit/s.
    pub rate: u32,
    pub kbps: u32,
    /// LDAC: the level of `q2_ldac::LEVELS` in use (0 HQ … 4 MQ) and whether it adapts.
    pub ldac_level: Option<u8>,
    pub ldac_adaptive: bool,
    /// The UART after bring-up.
    pub baud: u32,
    pub flow_control: bool,
    /// Bring-ups since the player started that failed at their UART setting and were
    /// tried at the next one (3 Mbaud with RTS/CTS, then 1.5 Mbaud with, then without).
    #[serde(default)]
    pub uart_fallbacks: u32,
    /// Read RSSI (dB from the golden range) and Get Link Quality (0–255), while streaming.
    pub rssi: Option<i8>,
    pub link_quality: Option<u8>,
    /// Controller ACL buffers, and those holding packets now.
    pub acl_buffers: u32,
    pub acl_in_use: u32,
    /// H5: packets sent again, resend timeouts, damaged frames and out-of-sequence packets received.
    pub h5_retransmissions: u32,
    pub h5_resends: u32,
    pub h5_bad_frames: u32,
    pub h5_out_of_sequence: u32,
    /// Waits for a controller buffer (Number of Completed Packets): episodes of 50 ms or more, total and longest ms.
    pub credit_stalls: u32,
    pub credit_stall_ms: u32,
    pub credit_stall_max_ms: u32,
    /// The stream: packets sent more than 40 ms late, schedule restarts after 300 ms behind,
    /// the worst lateness (ms), LDAC level changes, the deepest queue (packets).
    pub late: u32,
    pub restarts: u32,
    pub behind_max_ms: u32,
    pub ldac_changes: u32,
    pub queue_max: u32,
    /// Media packets dropped before the controller took them; PCM underruns.
    pub dropped: u32,
    pub underruns: u32,
    /// The kernel's UART receive errors (TIOCGICOUNT): framing, FIFO overrun, parity, tty buffer overrun.
    pub uart_frame: Option<u32>,
    pub uart_overrun: Option<u32>,
    pub uart_parity: Option<u32>,
    pub uart_buffer: Option<u32>,
    /// The connected headphones: class of device, advertised name (at most 64
    /// characters), and their Device ID record (vendor source 1 Bluetooth SIG,
    /// 2 USB-IF; vendor, product, version).
    pub device_class: Option<u32>,
    pub device_name: Option<String>,
    pub device_vendor_source: Option<u32>,
    pub device_vendor: Option<u32>,
    pub device_product: Option<u32>,
    pub device_version: Option<u32>,
    /// Transmit power (dBm) and the channels adaptive frequency hopping uses (of 79).
    pub tx_power: Option<i8>,
    pub afh_channels: Option<u8>,
    /// The media channel's MTU and, for SBC, the bitpool.
    pub mtu: u32,
    pub sbc_bitpool: Option<u8>,
    /// The Bluetooth thread woke 20 ms or more after it asked to (CPU starvation):
    /// how often and the worst; the longest UART write; the slowest encode of one chunk (µs).
    pub wake_late: u32,
    pub wake_late_max_ms: u32,
    pub write_max_ms: u32,
    pub encode_max_us: u32,
}

/// The longest device name a report carries.
pub const NAME_MAX: usize = 64;

/// A name fit for a report: printable characters, at most [`NAME_MAX`] of them
/// and [`NAME_MAX`] bytes of UTF-8.
pub fn report_name(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars().filter(|c| !c.is_control()).take(NAME_MAX) {
        if out.len() + c.len_utf8() > NAME_MAX { break }
        out.push(c);
    }
    out
}
/// The name's length and bytes, four to a cell.
const NAME_CELLS: usize = 1 + NAME_MAX / 4;

const FIELDS: usize = 44;
/// "Not known" in an optional field.
const NONE: u32 = u32::MAX;

pub(crate) struct Cells([AtomicU32; FIELDS], [AtomicU32; NAME_CELLS]);

impl Cells {
    pub(crate) const fn new() -> Self { Self([const { AtomicU32::new(0) }; FIELDS], [const { AtomicU32::new(0) }; NAME_CELLS]) }

    pub(crate) fn store(&self, b: &Bluetooth) {
        let opt = |v: Option<u32>| v.unwrap_or(NONE);
        let values = [u32::from(b.state), u32::from(b.codec), b.rate, b.kbps, opt(b.ldac_level.map(u32::from)), u32::from(b.ldac_adaptive),
            b.baud, u32::from(b.flow_control), opt(b.rssi.map(|r| r as u8 as u32)), opt(b.link_quality.map(u32::from)),
            b.acl_buffers, b.acl_in_use, b.h5_retransmissions, b.h5_resends, b.h5_bad_frames, b.h5_out_of_sequence,
            b.credit_stalls, b.credit_stall_ms, b.credit_stall_max_ms, b.late, b.restarts, b.behind_max_ms, b.ldac_changes, b.queue_max,
            b.dropped, b.underruns, opt(b.uart_frame), opt(b.uart_overrun), opt(b.uart_parity), opt(b.uart_buffer),
            opt(b.device_class), opt(b.device_vendor_source), opt(b.device_vendor), opt(b.device_product), opt(b.device_version),
            opt(b.tx_power.map(|p| p as u8 as u32)), opt(b.afh_channels.map(u32::from)), b.mtu, opt(b.sbc_bitpool.map(u32::from)),
            b.wake_late, b.wake_late_max_ms, b.write_max_ms, b.encode_max_us, b.uart_fallbacks];
        let name = b.device_name.as_deref().map(report_name);
        let bytes = name.as_deref().map_or(&[][..], str::as_bytes);
        self.1[0].store(name.as_ref().map_or(NONE, |n| n.len() as u32), Ordering::Relaxed);
        for (i, cell) in self.1[1..].iter().enumerate() {
            let word: [u8; 4] = std::array::from_fn(|j| bytes.get(i * 4 + j).copied().unwrap_or(0));
            cell.store(u32::from_le_bytes(word), Ordering::Relaxed);
        }
        // The state last: a reader that sees it sees the rest of this or the previous update.
        for (cell, value) in self.0.iter().zip(values).skip(1) { cell.store(value, Ordering::Relaxed); }
        self.0[0].store(values[0], Ordering::Release);
    }

    /// None while Bluetooth is off.
    pub(crate) fn load(&self) -> Option<Bluetooth> {
        let state = self.0[0].load(Ordering::Acquire);
        if state == 0 { return None }
        let v: [u32; FIELDS] = std::array::from_fn(|i| self.0[i].load(Ordering::Relaxed));
        let opt = |x: u32| (x != NONE).then_some(x);
        Some(Bluetooth {
            state: state.min(255) as u8, codec: v[1].min(255) as u8, rate: v[2], kbps: v[3], ldac_level: opt(v[4]).map(|l| l.min(255) as u8),
            ldac_adaptive: v[5] != 0, baud: v[6], flow_control: v[7] != 0, rssi: opt(v[8]).map(|r| r as u8 as i8), link_quality: opt(v[9]).map(|q| q.min(255) as u8),
            acl_buffers: v[10], acl_in_use: v[11], h5_retransmissions: v[12], h5_resends: v[13], h5_bad_frames: v[14], h5_out_of_sequence: v[15],
            credit_stalls: v[16], credit_stall_ms: v[17], credit_stall_max_ms: v[18], late: v[19], restarts: v[20], behind_max_ms: v[21],
            ldac_changes: v[22], queue_max: v[23], dropped: v[24], underruns: v[25],
            uart_frame: opt(v[26]), uart_overrun: opt(v[27]), uart_parity: opt(v[28]), uart_buffer: opt(v[29]),
            device_class: opt(v[30]), device_vendor_source: opt(v[31]), device_vendor: opt(v[32]), device_product: opt(v[33]), device_version: opt(v[34]),
            tx_power: opt(v[35]).map(|p| p as u8 as i8), afh_channels: opt(v[36]).map(|c| c.min(255) as u8), mtu: v[37], sbc_bitpool: opt(v[38]).map(|b| b.min(255) as u8),
            wake_late: v[39], wake_late_max_ms: v[40], write_max_ms: v[41], encode_max_us: v[42], uart_fallbacks: v[43],
            device_name: self.name(),
        })
    }

    fn name(&self) -> Option<String> {
        let len = self.1[0].load(Ordering::Relaxed);
        if len == NONE { return None }
        let bytes: Vec<u8> = self.1[1..].iter().flat_map(|c| c.load(Ordering::Relaxed).to_le_bytes()).take((len as usize).min(NAME_MAX)).collect();
        // A torn read can cut a character: keep what decodes.
        Some(String::from_utf8_lossy(&bytes).trim_end_matches('\u{fffd}').to_string())
    }

    pub(crate) fn clear(&self) { self.0[0].store(0, Ordering::Release); }
}

static CELLS: Cells = Cells::new();

/// The Bluetooth thread's latest numbers.
pub fn publish(bluetooth: &Bluetooth) { CELLS.store(bluetooth) }
/// Bluetooth switched off: reports carry no Bluetooth part.
pub fn clear() { CELLS.clear() }
pub fn current() -> Option<Bluetooth> { CELLS.load() }

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_field_survives_the_cells_and_off_reads_as_none() {
        let cells = Cells::new();
        assert_eq!(cells.load(), None);
        let full = Bluetooth { device_class: Some(0x24_0418), device_name: Some("Generated Headphones".into()), device_vendor_source: Some(1),
            device_vendor: Some(0x009e), device_product: Some(0x4020), device_version: Some(0x0100), tx_power: Some(-2), afh_channels: Some(76), mtu: 895,
            sbc_bitpool: Some(53), wake_late: 3, wake_late_max_ms: 41, write_max_ms: 2, encode_max_us: 900, state: 6, codec: 5, rate: 44_100, kbps: 606, ldac_level: Some(1), ldac_adaptive: true, baud: 1_500_000, flow_control: false, uart_fallbacks: 1,
            rssi: Some(-7), link_quality: Some(255), acl_buffers: 8, acl_in_use: 3, h5_retransmissions: 12, h5_resends: 3, h5_bad_frames: 2,
            h5_out_of_sequence: 1, credit_stalls: 4, credit_stall_ms: 900, credit_stall_max_ms: 310, late: 40, restarts: 1, behind_max_ms: 320,
            ldac_changes: 2, queue_max: 17, dropped: 0, underruns: 1, uart_frame: Some(0), uart_overrun: Some(5), uart_parity: Some(2), uart_buffer: None };
        cells.store(&full);
        assert_eq!(cells.load(), Some(full.clone()));
        let sparse = Bluetooth { state: 2, ldac_level: None, rssi: None, link_quality: None, uart_frame: None, uart_overrun: None, uart_parity: None,
            device_name: None, device_vendor: None, tx_power: None, ..full };
        cells.store(&sparse);
        assert_eq!(cells.load(), Some(sparse.clone()));
        assert_eq!(report_name("Bad\nName"), "BadName");
        assert_eq!(report_name(&"x".repeat(100)).len(), NAME_MAX);
        assert_eq!(report_name(&"é".repeat(40)).len(), NAME_MAX, "64 bytes of UTF-8 at most");
        cells.store(&Bluetooth { device_name: Some("Kopfhörer Ü".into()), ..sparse.clone() });
        assert_eq!(cells.load().unwrap().device_name.as_deref(), Some("Kopfhörer Ü"));
        cells.clear();
        assert_eq!(cells.load(), None);
    }

    #[test]
    fn the_wire_format_is_plain_numbers_and_nulls() {
        let b = Bluetooth { state: 6, codec: 5, rssi: Some(-7), ..Bluetooth::default() };
        let json = serde_json::to_value(&b).unwrap();
        assert_eq!(json["rssi"], -7);
        assert!(json["link_quality"].is_null() && json["uart_overrun"].is_null());
        // Every numeric cell, and the name.
        assert_eq!(json.as_object().unwrap().len(), FIELDS + 1);
        assert_eq!(serde_json::from_value::<Bluetooth>(json).unwrap(), b);
    }
}
