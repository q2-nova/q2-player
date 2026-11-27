//! The host stack: controller bring-up (Realtek patch download), inquiry,
//! pairing (Secure Simple Pairing "Just Works", legacy PIN 0000), one ACL
//! link, L2CAP basic mode, the SDP server, an AVDTP source endpoint (SBC)
//! as initiator or acceptor, the AVRCP target, and the AVRCP controller of
//! the headphones' absolute volume. After the stream opens, the headphones'
//! Device ID record is read once.
//!
//! No I/O and no clock: the caller feeds UART bytes to [`Host::rx`], calls
//! [`Host::poll`] with the time in milliseconds, writes the bytes it returns
//! and then handles [`Host::events`] in order.
use crate::codec::{self, Codec, Kind};
use crate::{avrcp, h5, rtl, sdp};
use std::collections::VecDeque;

/// A device address in wire order (least significant byte first).
pub type Addr = [u8; 6];

/// Narrow SBC offer of the temporary bench receiver, including media type and codec.
pub const BENCH_SBC_OFFER: [u8; 6] = [0, 0, 0x21, 0x15, 53, 53];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Key { pub addr: Addr, pub key: [u8; 16], pub kind: u8 }

#[derive(Clone, Default)]
pub struct Settings {
    pub name: String,
    /// UART rate after the patch starts; the rate must have a Realtek code.
    pub baud: u32,
    pub flow: bool,
    /// A UART setting the controller may still run at: that of a bring-up of this process
    /// that failed after the controller took its rate, while nothing reset the controller (the
    /// Wi-Fi side keeps it powered). The sync tries it after 115200 and `baud`; a controller
    /// that answers there is moved to `baud` with the vendor rate command before its start-up
    /// commands.
    pub previous: Option<(u32, bool)>,
    pub firmware: Option<Vec<u8>>,
    pub config: Option<Vec<u8>>,
    pub keys: Vec<Key>,
    /// The codecs this build can encode, most preferred first. Empty means SBC only.
    pub codecs: Vec<Kind>,
    /// Disposable bench mode. Allow only this peer, including an inbound first pairing.
    pub bench_peer: Option<Addr>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Version {
    pub hci_version: u8, pub hci_revision: u16, pub lmp_version: u8, pub manufacturer: u16, pub lmp_subversion: u16,
    pub rom_version: Option<u8>,
    /// The controller already ran at the fast rate, so its patch was loaded earlier.
    pub already_patched: bool,
}

/// Counts for bring-up diagnosis. No addresses or names.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Diagnostics {
    pub events: u32,
    pub inquiry_results: u32,
    pub inquiry_completes: u32,
    pub name_replies: u32,
    /// Commands the controller refused: (opcode, status), the first eight.
    pub command_errors: Vec<(u16, u8)>,
    pub command_timeouts: u32,
    /// Scans ended by the host because the controller never reported the inquiry complete.
    pub scan_timeouts: u32,
    /// H5 sleep, wakeup and woken messages from the controller.
    pub h5_sleep: [u32; 3],
    pub h5_retransmissions: u32,
    pub h5_rejected: u32,
    /// Events by code: (code, count).
    pub by_code: Vec<(u8, u32)>,
    /// The last events: (ms, event code, opcode for command complete/status, status).
    pub timeline: Vec<(u64, u8, u16, u8)>,
    /// Return parameters of `Command::Raw` commands, status first: (opcode, bytes).
    pub replies: Vec<(u16, Vec<u8>)>,
    /// The local version read again after the patch download: (HCI revision, LMP subversion).
    pub patched_version: Option<(u16, u16)>,
    /// H5 packets the host did not parse: an event whose length byte disagrees with
    /// the packet, or a packet type other than event and ACL data.
    pub unparsed: u32,
    /// Service searches for an A2DP sink before pairing: (asked, answered with a sink, answered without).
    pub sink_checks: (u32, u32, u32),
}

/// Counts for the audio path, for telemetry: numbers only, no addresses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LinkStats {
    /// ACL buffers the controller has (Read Buffer Size) and those holding packets now.
    pub acl_buffers: u16,
    pub acl_in_use: u16,
    /// Time with ACL data waiting and no controller buffer free (no Number of
    /// Completed Packets yet): in all, the longest, and episodes of [`CREDIT_STALL_MS`] or more.
    pub credit_stall_ms: u32,
    pub credit_stall_max_ms: u32,
    pub credit_stalls: u32,
    /// The length of the last episode counted in `credit_stalls`.
    pub credit_stall_last_ms: u32,
    /// Reliable H5 packets sent again, resend timeouts, damaged frames received
    /// and reliable packets received out of sequence.
    pub h5_retransmissions: u32,
    pub h5_resends: u32,
    pub h5_bad_frames: u32,
    pub h5_out_of_sequence: u32,
    /// The last Read RSSI (dB relative to the golden receive power range) and
    /// Get Link Quality (0–255) answers while a link is up.
    pub rssi: Option<i8>,
    pub link_quality: Option<u8>,
    /// Read Transmit Power Level (current, dBm) and the channels in use by
    /// adaptive frequency hopping (of 79; Read AFH Channel Map).
    pub tx_power: Option<i8>,
    pub afh_channels: Option<u8>,
}

/// A credit stall at least this long counts as one in [`LinkStats::credit_stalls`].
pub const CREDIT_STALL_MS: u64 = 50;
/// How often RSSI and link quality are read while audio streams.
const QUALITY_MS: u64 = 2000;
/// The H5 resend timeout once the controller runs at the fast rate: the window
/// (4 packets of at most 1 kB) takes under 30 ms on the wire at 1.5 Mbaud. A lost
/// frame stalls audio this long, so it must stay well inside the headphones' buffer.
const FAST_RESEND_MS: u64 = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stream { pub codec: Codec, /// The media channel's MTU: the most one media packet may carry, RTP header included.
    pub mtu: usize }

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command { Scan, StopScan, Connect(Addr),
    /// Connects, asks the device's SDP server for an A2DP sink (0x110b) before
    /// pairing, and pairs only when it has one; otherwise `NotSink` and disconnect.
    ConnectSink(Addr),
    Disconnect, Forget(Addr), Start, Suspend,
    /// The player's volume, 0..=127: sent to headphones that take absolute volume
    /// (SetAbsoluteVolume), and reported to headphones that registered for it.
    SetVolume(u8),
    /// Any HCI command, for diagnosis; its reply is kept in `Diagnostics::replies`.
    Raw(u16, Vec<u8>),
    /// Asks a ready controller for its version: `Alive` when it answers, `Lost` when it
    /// does not within [`CHECK_MS`] (it lost power or was reset since bring-up).
    Check,
    /// Runs the start-up commands again (HCI Reset and the configuration) on a ready
    /// controller that kept its patch, so it starts over as at bring-up; `Ready` follows.
    /// Refused while a link, a connection attempt, a scan or a check is open.
    Reinit,
    /// Ends an outgoing page before it times out (HCI Create Connection Cancel):
    /// `ConnectFailed` with status 0x02 follows when the controller had not connected.
    CancelConnect }

/// Where a connection attempt is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Paging the device (HCI Create Connection), or accepting its page.
    Page,
    /// `ConnectSink`: asking the device's SDP server for an A2DP sink.
    SinkCheck,
    /// Authentication: with the stored key, or pairing (Secure Simple Pairing or PIN) without one.
    Authentication,
    /// Turning encryption on.
    Encryption,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// Retune the UART (8 data bits, even parity) once the bytes already returned are sent.
    Uart { baud: u32, flow: bool },
    Controller(Version),
    Ready { addr: Addr },
    Failed(String),
    /// `sink`: the extended inquiry response lists the A2DP sink service (0x110b).
    Found { addr: Addr, class: u32, rssi: i8, name: Option<String>, sink: bool },
    ScanFinished,
    Connecting(Addr),
    /// The link is authenticated and encrypted.
    Connected { addr: Addr, incoming: bool },
    ConnectFailed { addr: Addr, status: u8 },
    /// `ConnectSink`: the device's SDP server has no A2DP sink. The link is closed unpaired.
    NotSink(Addr),
    Disconnected { addr: Addr, reason: u8 },
    LinkKey(Key),
    /// The stored key was refused; it has been dropped and the device must be paired again.
    KeyRejected(Addr),
    /// The audio stream opened (`Some`) or closed.
    Stream(Option<Stream>),
    Streaming(bool),
    Button(avrcp::Button),
    /// The headphones take absolute volume: they answered the volume registration.
    AbsoluteVolume,
    /// The headphones' volume, 0..=127. `initial`: the first report after the
    /// registration, before the player has set it; otherwise a change made on the
    /// headphones (their buttons). Levels the player set are not reported back.
    Volume { level: u8, initial: bool },
    /// Actual amplifier level from a controller response, including rounded set acknowledgements.
    AmplifierLevel(u8),
    /// The headphones' Device ID record (vendor and product), read after the stream opened.
    DeviceId(sdp::DeviceId),
    /// `Check`: the controller answered.
    Alive,
    /// `Check`: the controller did not answer; it needs a new bring-up.
    Lost,
    /// A connection attempt reached `step` (`Connecting` is the page).
    Step { addr: Addr, step: Step },
    /// A connection attempt did not get past `step` in time ([`PAGE_LIMIT_MS`],
    /// [`SECURE_LIMIT_MS`], the sink check's 10 s). The host has given up on it: a page
    /// is cancelled, a link closed, and a new `Connect` is taken at once.
    TimedOut { addr: Addr, step: Step },
}

/// From Create Connection, or accepting a page, to Connection Complete. The controller's
/// own page timeout is 5.12 s (start-up commands); this covers a controller that never reports.
pub const PAGE_LIMIT_MS: u64 = 12_000;
/// From a new link to an encrypted one: authentication, pairing and encryption.
pub const SECURE_LIMIT_MS: u64 = 30_000;
/// After the host closes a link for a timeout, how long it waits for Disconnection
/// Complete before it forgets the link anyway.
const CLOSE_LIMIT_MS: u64 = 3000;

/// How long `Command::Check` waits for the controller's answer.
pub const CHECK_MS: u64 = 3000;

const INQUIRY: u16 = 0x0401;
const INQUIRY_CANCEL: u16 = 0x0402;
const CREATE_CONNECTION: u16 = 0x0405;
const CREATE_CONNECTION_CANCEL: u16 = 0x0408;
const REMOTE_NAME_CANCEL: u16 = 0x041a;
const DISCONNECT: u16 = 0x0406;
const ACCEPT_CONNECTION: u16 = 0x0409;
const REJECT_CONNECTION: u16 = 0x040a;
const LINK_KEY_REPLY: u16 = 0x040b;
const LINK_KEY_NEGATIVE: u16 = 0x040c;
const PIN_REPLY: u16 = 0x040d;
const PIN_NEGATIVE_REPLY: u16 = 0x040e;
const AUTHENTICATION_REQUESTED: u16 = 0x0411;
const SET_ENCRYPTION: u16 = 0x0413;
const REMOTE_NAME: u16 = 0x0419;
const IO_CAPABILITY_REPLY: u16 = 0x042b;
const USER_CONFIRMATION_REPLY: u16 = 0x042c;
const USER_CONFIRMATION_NEGATIVE_REPLY: u16 = 0x042d;
const IO_CAPABILITY_NEGATIVE_REPLY: u16 = 0x0434;
const USER_PASSKEY_NEGATIVE: u16 = 0x042f;
const REMOTE_OOB_NEGATIVE: u16 = 0x0433;
const WRITE_LINK_POLICY: u16 = 0x080f;
const SET_EVENT_MASK: u16 = 0x0c01;
const RESET: u16 = 0x0c03;
const WRITE_LOCAL_NAME: u16 = 0x0c13;
const WRITE_PAGE_TIMEOUT: u16 = 0x0c18;
const WRITE_SCAN_ENABLE: u16 = 0x0c1a;
const WRITE_CLASS: u16 = 0x0c24;
const WRITE_INQUIRY_MODE: u16 = 0x0c45;
const WRITE_SSP_MODE: u16 = 0x0c56;
const READ_LOCAL_VERSION: u16 = 0x1001;
const READ_BUFFER_SIZE: u16 = 0x1005;
const READ_BD_ADDR: u16 = 0x1009;
const GET_LINK_QUALITY: u16 = 0x1403;
const READ_RSSI: u16 = 0x1405;
const READ_AFH_MAP: u16 = 0x1406;
const READ_TX_POWER: u16 = 0x0c2d;
const RTL_BAUD: u16 = 0xfc17;
const RTL_DOWNLOAD: u16 = 0xfc20;
const RTL_ROM_VERSION: u16 = 0xfc6d;
/// Realtek's profile report (stock `rtk_coex.c`, HCI_VENDOR_SET_PROFILE_REPORT_COMMAND):
/// tells the controller's firmware which links carry which profile, which it passes
/// to the Wi-Fi firmware's coexistence logic. Parameters: link count, then per link
/// the handle and a profile bitmap, then the active-profile bitmap (bit 2 A2DP).
const RTL_PROFILE_REPORT: u16 = 0xfc19;
const PROFILE_A2DP: u8 = 1 << 2;
/// Portable audio player: audio and capturing services, audio/video major class.
pub const CLASS_OF_DEVICE: [u8; 3] = [0x1c, 0x04, 0x28];
pub const INITIAL_BAUD: u32 = 115_200;
const PSM_SDP: u16 = 0x0001;
const PSM_AVCTP: u16 = 0x0017;
const PSM_AVDTP: u16 = 0x0019;
const DEFAULT_MTU: usize = 672;
const SINK_CHECK_TID: u16 = 0x0b0b;
/// The SDP channel and its one search, from the link's creation.
const SINK_CHECK_MS: u64 = 10_000;
const DEVICE_ID_TID: u16 = 0x0d1d;
/// After the media channel opens, before the Device ID query; and how long it may take.
const DEVICE_ID_AFTER_MS: u64 = 3_000;
const DEVICE_ID_MS: u64 = 5_000;
/// How long an AVRCP command to the headphones waits for its response.
const AVRCP_WAIT_MS: u64 = 2_000;
/// Our SBC capabilities: 44.1 and 48 kHz, stereo and joint stereo, 4-16 blocks, 8 subbands, both allocations.

struct Rd<'a>(&'a [u8]);
impl Rd<'_> {
    fn u8(&mut self) -> u8 { let Some((&v, rest)) = self.0.split_first() else { return 0 }; self.0 = rest; v }
    fn u16(&mut self) -> u16 { u16::from(self.u8()) | u16::from(self.u8()) << 8 }
    fn addr(&mut self) -> Addr { let mut a = [0; 6]; for b in &mut a { *b = self.u8(); } a }
    fn skip(&mut self, n: usize) { self.0 = self.0.get(n..).unwrap_or(&[]); }
}

/// Everything the link logic produces; separate from the link so both can be borrowed.
struct Out {
    now: u64,
    acl_mtu: usize,
    acl: VecDeque<Vec<u8>>,
    media: VecDeque<Vec<Vec<u8>>>,
    media_dropped: u32,
    events: VecDeque<Event>,
    commands: VecDeque<(u16, Vec<u8>)>,
    codecs: Vec<Kind>,
    bench_receiver: bool,
    /// The player's volume, 0..=127, for headphones that ask for it.
    volume: u8,
    volume_limit: u8,
}
impl Out {
    fn l2cap(&mut self, handle: u16, cid: u16, payload: &[u8], media: bool) {
        let mut pdu = Vec::with_capacity(4 + payload.len());
        pdu.extend_from_slice(&(payload.len() as u16).to_le_bytes());
        pdu.extend_from_slice(&cid.to_le_bytes());
        pdu.extend_from_slice(payload);
        let fragments: Vec<Vec<u8>> = pdu.chunks(self.acl_mtu.max(27)).enumerate().map(|(i, chunk)| {
            let mut packet = Vec::with_capacity(4 + chunk.len());
            packet.extend_from_slice(&(handle | if i == 0 { 0x2000 } else { 0x1000 }).to_le_bytes());
            packet.extend_from_slice(&(chunk.len() as u16).to_le_bytes());
            packet.extend_from_slice(chunk);
            packet
        }).collect();
        if media {
            // Late audio is worth less than current audio: drop the oldest queued packet.
            if self.media.len() >= 12 { self.media.pop_front(); self.media_dropped += 1; }
            self.media.push_back(fragments);
        } else { self.acl.extend(fragments); }
    }
    fn signal(&mut self, handle: u16, code: u8, id: u8, data: &[u8]) {
        let mut pdu = vec![code, id, data.len() as u8, (data.len() >> 8) as u8];
        pdu.extend_from_slice(data);
        self.l2cap(handle, 0x0001, &pdu, false);
    }
    fn command(&mut self, opcode: u16, parameters: &[u8]) { self.commands.push_back((opcode, parameters.to_vec())); }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role { Sdp, Signal, Media, Control,
    /// Our own SDP request channel (the sink check).
    SdpClient }
struct Channel { local: u16, remote: u16, role: Role, ours_done: bool, theirs_done: bool, mtu: usize, open: bool }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Av { Idle, Discover, Capabilities, Configure, Opening, Configured, MediaWait, Open, Starting, Streaming, Suspending,
    /// Closing the sink's own stream to configure a better codec.
    Closing }

/// How the open stream's codec was chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// We configured the best codec both sides have, by the preference.
    Ours,
    /// The headphones configured it; `checked`: their other endpoints offer nothing we prefer.
    Theirs { checked: bool },
    /// The headphones configured `from`; we closed that stream and configured the best both have.
    Upgraded { from: Kind },
}

/// The AVRCP controller's side: the headphones' absolute volume.
#[derive(Default)]
struct Remote {
    label: u8,
    /// The command awaiting its response: (label, PDU, deadline).
    waiting: Option<(u8, u8, u64)>,
    /// The headphones answered the volume registration.
    absolute: bool,
    /// Their level, as last reported or accepted.
    level: Option<u8>,
    /// A level to set once nothing is waiting.
    wanted: Option<u8>,
    /// A volume registration to renew once nothing is waiting.
    register: bool,
}

struct Link {
    addr: Addr,
    handle: u16,
    incoming: bool,
    secured: bool,
    had_key: bool,
    auth_sent: bool,
    auth_at: Option<u64>,
    connect_at: Option<u64>,
    reassembly: Vec<u8>,
    channels: Vec<Channel>,
    next_cid: u16,
    signal_id: u8,
    /// Connection requests held until the link is encrypted: (identifier, their CID, PSM).
    waiting: Vec<(u8, u16, u16)>,
    av: Av,
    av_label: u8,
    av_wait: Option<(u8, u64)>,
    av_initiate_at: Option<u64>,
    remote_seid: u8,
    stream: Option<Codec>,
    /// The sink's endpoints still to be asked for their capabilities, and the offers read so far.
    unasked: Vec<u8>,
    offers: Vec<(u8, Vec<u8>)>,
    want_stream: bool,
    sequence: u16,
    timestamp: u32,
    control_at: Option<u64>,
    status_label: Option<u8>,
    /// The headphones' registration for the player's volume (target side).
    volume_label: Option<u8>,
    remote: Remote,
    /// When to ask for the Device ID record; until when the query may run; whether it runs.
    id_at: Option<u64>,
    id_until: Option<u64>,
    id_query: bool,
    /// `ConnectSink`: the SDP check is running and pairing waits for it; its deadline.
    checking: bool,
    check_at: Option<u64>,
    /// The step reached before the link is encrypted, and the deadline for encryption.
    step: Step,
    secure_by: Option<u64>,
    /// The headphones configured the stream themselves.
    remote_configured: bool,
    /// Their choice was checked against their other endpoints (once per link); during the
    /// check, the endpoints still to ask, the offers read and the endpoint asked.
    probed: bool,
    probe: Option<(Vec<u8>, Vec<(u8, Vec<u8>)>, u8)>,
    /// The better stream to configure once theirs is closed, and theirs to fall back to:
    /// (their endpoint, the stream, their offer).
    upgrade: Option<(u8, Codec, Vec<u8>)>,
    fallback: Option<(u8, Codec, Vec<u8>)>,
    origin: Origin,
    /// Closed by the host after a timeout: forgotten at this time if the controller says nothing.
    closing_at: Option<u64>,
}

impl Link {
    fn new(addr: Addr, handle: u16, incoming: bool, now: u64) -> Self {
        Self { addr, handle, incoming, secured: false, had_key: false, auth_sent: false, auth_at: None, connect_at: None, reassembly: Vec::new(),
            channels: Vec::new(), next_cid: 0x0040, signal_id: 0, waiting: Vec::new(), av: Av::Idle, av_label: 0, av_wait: None,
            av_initiate_at: None, remote_seid: 0, stream: None, unasked: Vec::new(), offers: Vec::new(), want_stream: false, sequence: 0, timestamp: 0, control_at: None, status_label: None,
            volume_label: None, remote: Remote::default(), id_at: None, id_until: None, id_query: false,
            checking: false, check_at: None, step: Step::Page, secure_by: Some(now + SECURE_LIMIT_MS), closing_at: None,
            remote_configured: false, probed: false, probe: None, upgrade: None, fallback: None, origin: Origin::Ours }
    }
    fn reach(&mut self, out: &mut Out, step: Step) {
        if self.step == step || self.secured { return }
        self.step = step;
        out.events.push_back(Event::Step { addr: self.addr, step });
    }
    fn channel(&self, role: Role) -> Option<&Channel> { self.channels.iter().find(|c| c.role == role && c.open) }
    fn stream_info(&self) -> Option<Stream> {
        let media = self.channel(Role::Media)?;
        matches!(self.av, Av::Open | Av::Starting | Av::Streaming | Av::Suspending)
            .then(|| Stream { codec: self.stream.unwrap(), mtu: media.mtu.min(DEFAULT_MTU * 2) })
    }
    fn next_id(&mut self) -> u8 { self.signal_id = self.signal_id % 255 + 1; self.signal_id }
    fn authenticate(&mut self, out: &mut Out) {
        // A device under the sink check is not paired before the check passes.
        if self.secured || self.auth_sent || self.checking { return }
        self.auth_sent = true; self.auth_at = None;
        self.reach(out, Step::Authentication);
        out.command(AUTHENTICATION_REQUESTED, &self.handle.to_le_bytes());
    }

    fn connect(&mut self, out: &mut Out, psm: u16, role: Role) {
        let local = self.next_cid; self.next_cid += 1;
        self.channels.push(Channel { local, remote: 0, role, ours_done: false, theirs_done: false, mtu: DEFAULT_MTU, open: false });
        let id = self.next_id();
        let mut data = psm.to_le_bytes().to_vec(); data.extend_from_slice(&local.to_le_bytes());
        out.signal(self.handle, 0x02, id, &data);
    }
    fn configure(&mut self, out: &mut Out, remote: u16) {
        let id = self.next_id();
        let mut data = remote.to_le_bytes().to_vec(); data.extend_from_slice(&[0, 0]);
        out.signal(self.handle, 0x04, id, &data);
    }
    fn accept(&mut self, out: &mut Out, id: u8, remote: u16, psm: u16) {
        let role = match psm {
            PSM_SDP => Role::Sdp,
            PSM_AVCTP if !self.channels.iter().any(|c| c.role == Role::Control) => Role::Control,
            PSM_AVDTP if !self.channels.iter().any(|c| c.role == Role::Signal) => Role::Signal,
            PSM_AVDTP if !self.channels.iter().any(|c| c.role == Role::Media) => Role::Media,
            _ => {
                // PSM not supported, or no resources for another channel of that kind.
                let mut data = vec![0, 0]; data.extend_from_slice(&remote.to_le_bytes());
                data.extend_from_slice(&[if matches!(psm, PSM_AVCTP | PSM_AVDTP) { 4 } else { 2 }, 0, 0, 0]);
                out.signal(self.handle, 0x03, id, &data);
                return;
            }
        };
        let local = self.next_cid; self.next_cid += 1;
        self.channels.push(Channel { local, remote, role, ours_done: false, theirs_done: false, mtu: DEFAULT_MTU, open: false });
        let mut data = local.to_le_bytes().to_vec(); data.extend_from_slice(&remote.to_le_bytes()); data.extend_from_slice(&[0, 0, 0, 0]);
        out.signal(self.handle, 0x03, id, &data);
        self.configure(out, remote);
    }

    fn secured(&mut self, out: &mut Out) {
        if self.secured { return }
        self.secured = true; self.auth_at = None; self.secure_by = None;
        out.events.push_back(Event::Connected { addr: self.addr, incoming: self.incoming });
        for (id, remote, psm) in std::mem::take(&mut self.waiting) { self.accept(out, id, remote, psm); }
        // Headphones that reconnect open the channels themselves; give them time before we do.
        self.connect_at = Some(out.now + if self.incoming { 2000 } else { 0 });
    }

    /// Starts the sink check: an SDP channel, then one service search.
    fn check_sink(&mut self, out: &mut Out) {
        self.checking = true;
        self.check_at = Some(out.now + SINK_CHECK_MS);
        self.reach(out, Step::SinkCheck);
        self.connect(out, PSM_SDP, Role::SdpClient);
    }
    /// The sink check ended: `Some(true)` pairs, anything else closes the link unpaired.
    fn checked(&mut self, out: &mut Out, sink: Option<bool>, counts: &mut (u32, u32, u32)) {
        if !self.checking { return }
        self.checking = false; self.check_at = None;
        if let Some(at) = self.channels.iter().position(|c| c.role == Role::SdpClient && c.open) { self.close_channel(out, at); }
        match sink {
            // Pairing gets its full time after the check, which a computer may need to confirm it.
            Some(true) => { counts.1 += 1; self.secure_by = Some(out.now + SECURE_LIMIT_MS); self.authenticate(out); }
            Some(false) => {
                counts.2 += 1;
                out.events.push_back(Event::NotSink(self.addr));
                out.command(DISCONNECT, &[self.handle as u8, (self.handle >> 8) as u8, 0x13]);
            }
            None => out.command(DISCONNECT, &[self.handle as u8, (self.handle >> 8) as u8, 0x13]),
        }
    }

    fn tick(&mut self, out: &mut Out, counts: &mut (u32, u32, u32)) -> Option<u64> {
        let now = out.now;
        // No answer to the sink check: close the link unpaired.
        if self.check_at.is_some_and(|at| now >= at) {
            out.events.push_back(Event::TimedOut { addr: self.addr, step: Step::SinkCheck });
            self.secure_by = None; self.closing_at = Some(now + CLOSE_LIMIT_MS);
            self.checked(out, None, counts);
        }
        // Not encrypted in time: authentication or pairing stalled. Give up on the link.
        if self.secure_by.is_some_and(|at| now >= at) {
            self.secure_by = None; self.auth_at = None; self.check_at = None; self.checking = false;
            out.events.push_back(Event::TimedOut { addr: self.addr, step: self.step });
            out.command(DISCONNECT, &[self.handle as u8, (self.handle >> 8) as u8, 0x13]);
            self.closing_at = Some(now + CLOSE_LIMIT_MS);
        }
        if self.auth_at.is_some_and(|at| now >= at) && !self.checking { self.authenticate(out); }
        if self.secured && self.connect_at.is_some_and(|at| now >= at) {
            self.connect_at = None;
            if !self.channels.iter().any(|c| c.role == Role::Signal) { self.connect(out, PSM_AVDTP, Role::Signal); }
        }
        if self.av_initiate_at.is_some_and(|at| now >= at) {
            self.av_initiate_at = None;
            if self.av == Av::Idle && self.channel(Role::Signal).is_some() { self.av = Av::Discover; self.av_command(out, 1, &[]); }
        }
        if self.control_at.is_some_and(|at| now >= at) {
            self.control_at = None;
            if !self.channels.iter().any(|c| c.role == Role::Control) { self.connect(out, PSM_AVCTP, Role::Control); }
        }
        if self.av_wait.is_some_and(|(_, at)| now >= at) {
            self.av_wait = None;
            if self.probe.take().is_some() {
                // No answer about their other endpoints: keep the stream they set up.
                self.announce(out);
            } else {
                // The headphones stopped answering stream signalling.
                out.command(DISCONNECT, &[self.handle as u8, (self.handle >> 8) as u8, 0x13]);
            }
        }
        if self.remote.waiting.is_some_and(|(_, _, at)| now >= at) {
            // No response from the headphones: give up on that command.
            self.remote.waiting = None;
            self.control_flush(out);
        }
        if self.id_at.is_some_and(|at| now >= at) {
            self.id_at = None;
            if !self.checking && !self.channels.iter().any(|c| c.role == Role::SdpClient) {
                self.id_query = true; self.id_until = Some(now + DEVICE_ID_MS);
                self.connect(out, PSM_SDP, Role::SdpClient);
            }
        }
        if self.id_until.is_some_and(|at| now >= at) {
            self.id_until = None;
            if self.id_query {
                self.id_query = false;
                if let Some(at) = self.channels.iter().position(|c| c.role == Role::SdpClient && c.open) { self.close_channel(out, at); }
            }
        }
        [self.auth_at.filter(|_| !self.checking), self.connect_at, self.av_initiate_at, self.control_at, self.av_wait.map(|w| w.1), self.check_at,
            self.remote.waiting.map(|w| w.2), self.id_at, self.id_until, self.secure_by, self.closing_at].into_iter().flatten().min()
    }

    fn acl(&mut self, out: &mut Out, flags: u8, data: &[u8], playing: bool, counts: &mut (u32, u32, u32)) {
        if flags & 3 == 2 { self.reassembly.clear(); } else if self.reassembly.is_empty() { return }
        if self.reassembly.len() + data.len() > 4 + 65535 { self.reassembly.clear(); return }
        self.reassembly.extend_from_slice(data);
        if self.reassembly.len() < 4 { return }
        let len = u16::from_le_bytes([self.reassembly[0], self.reassembly[1]]) as usize;
        if self.reassembly.len() < 4 + len { return }
        let pdu = std::mem::take(&mut self.reassembly);
        let (cid, payload) = (u16::from_le_bytes([pdu[2], pdu[3]]), &pdu[4..4 + len]);
        if cid == 0x0001 { self.signalling(out, payload); return }
        let Some(channel) = self.channels.iter().find(|c| c.local == cid && c.open) else { return };
        let (remote, mtu, role) = (channel.remote, channel.mtu, channel.role);
        match role {
            Role::Sdp => if let Some(response) = sdp::respond(payload, mtu) { out.l2cap(self.handle, remote, &response, false); },
            Role::Signal => self.avdtp(out, payload),
            Role::Control if avrcp::is_response(payload) => self.control_reply(out, payload),
            Role::Control => {
                let (response, request) = avrcp::handle_limited(payload, playing, out.volume, out.volume_limit);
                if let Some(response) = response { out.l2cap(self.handle, remote, &response, false); }
                match request {
                    Some(avrcp::Request::Button(button)) => out.events.push_back(Event::Button(button)),
                    Some(avrcp::Request::StatusRegistration(label)) => self.status_label = Some(label),
                    Some(avrcp::Request::VolumeRegistration(label)) => self.volume_label = Some(label),
                    Some(avrcp::Request::Volume(level)) => { out.volume = level; out.events.push_back(Event::Volume { level, initial: false }); }
                    None => {}
                }
            }
            Role::SdpClient if self.checking => {
                let sink = sdp::search_count(payload, SINK_CHECK_TID).map(|n| n > 0);
                self.checked(out, sink, counts);
            }
            Role::SdpClient => if self.id_query {
                self.id_query = false; self.id_until = None;
                if let Some(id) = sdp::device_id(payload, DEVICE_ID_TID) { out.events.push_back(Event::DeviceId(id)); }
                if let Some(at) = self.channels.iter().position(|c| c.role == Role::SdpClient && c.open) { self.close_channel(out, at); }
            },
            Role::Media => {}
        }
    }

    fn signalling(&mut self, out: &mut Out, mut pdu: &[u8]) {
        while pdu.len() >= 4 {
            let (code, id, len) = (pdu[0], pdu[1], u16::from_le_bytes([pdu[2], pdu[3]]) as usize);
            let Some(data) = pdu.get(4..4 + len) else { return };
            pdu = &pdu[4 + len..];
            let mut r = Rd(data);
            match code {
                0x02 => {
                    let (psm, remote) = (r.u16(), r.u16());
                    if self.secured || psm == PSM_SDP { self.accept(out, id, remote, psm); } else {
                        // Pending until the link is encrypted (authentication pending).
                        let mut data = vec![0, 0]; data.extend_from_slice(&remote.to_le_bytes()); data.extend_from_slice(&[1, 0, 1, 0]);
                        out.signal(self.handle, 0x03, id, &data);
                        self.waiting.push((id, remote, psm));
                        if self.auth_at.is_none() { self.auth_at = Some(out.now + 300); }
                    }
                }
                0x03 => {
                    let (remote, local, result) = (r.u16(), r.u16(), r.u16());
                    let Some(at) = self.channels.iter().position(|c| c.local == local && !c.open) else { continue };
                    match result {
                        0 => { self.channels[at].remote = remote; self.configure(out, remote); }
                        1 => {}
                        _ => { let role = self.channels.remove(at).role; self.channel_failed(out, role); }
                    }
                }
                0x04 => {
                    let (local, _flags) = (r.u16(), r.u16());
                    let Some(at) = self.channels.iter().position(|c| c.local == local) else {
                        // Invalid CID in request.
                        let mut reject = vec![2, 0]; reject.extend_from_slice(&local.to_le_bytes()); reject.extend_from_slice(&[0, 0]);
                        out.signal(self.handle, 0x01, id, &reject);
                        continue;
                    };
                    let (mut options, mut refused) = (r.0, Vec::new());
                    while options.len() >= 2 {
                        let (ty, len) = (options[0], options[1] as usize);
                        let Some(value) = options.get(2..2 + len) else { break };
                        match (ty & 0x7f, len) {
                            (1, 2) => self.channels[at].mtu = (u16::from_le_bytes([value[0], value[1]]) as usize).max(48),
                            // Retransmission and flow control: basic mode only.
                            (4, 9) if value[0] != 0 => refused.extend_from_slice(&[4, 9, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
                            _ => {}
                        }
                        options = &options[2 + len..];
                    }
                    let remote = self.channels[at].remote;
                    let mut response = remote.to_le_bytes().to_vec();
                    response.extend_from_slice(&[0, 0, if refused.is_empty() { 0 } else { 1 }, 0]);
                    response.extend_from_slice(&refused);
                    out.signal(self.handle, 0x05, id, &response);
                    if refused.is_empty() { self.channels[at].theirs_done = true; self.opened(out, at); }
                }
                0x05 => {
                    let (local, _flags, result) = (r.u16(), r.u16(), r.u16());
                    let Some(at) = self.channels.iter().position(|c| c.local == local) else { continue };
                    if result == 0 { self.channels[at].ours_done = true; self.opened(out, at); } else { self.close_channel(out, at); }
                }
                0x06 => {
                    let (local, remote) = (r.u16(), r.u16());
                    let mut response = local.to_le_bytes().to_vec(); response.extend_from_slice(&remote.to_le_bytes());
                    out.signal(self.handle, 0x07, id, &response);
                    if let Some(at) = self.channels.iter().position(|c| c.local == local) { let role = self.channels.remove(at).role; self.channel_closed(out, role); }
                }
                0x07 => {
                    let (_remote, local) = (r.u16(), r.u16());
                    if let Some(at) = self.channels.iter().position(|c| c.local == local) { let role = self.channels.remove(at).role; self.channel_closed(out, role); }
                }
                0x08 => out.signal(self.handle, 0x09, id, data),
                0x0a => {
                    let ty = r.u16();
                    let mut response = ty.to_le_bytes().to_vec();
                    match ty {
                        // Extended features: fixed channels. Fixed channels: signalling only.
                        2 => response.extend_from_slice(&[0, 0, 0x80, 0, 0, 0]),
                        3 => response.extend_from_slice(&[0, 0, 0x02, 0, 0, 0, 0, 0, 0, 0]),
                        _ => response.extend_from_slice(&[1, 0]),
                    }
                    out.signal(self.handle, 0x0b, id, &response);
                }
                0x01 | 0x09 | 0x0b => {}
                // Command not understood.
                _ => out.signal(self.handle, 0x01, id, &[0, 0]),
            }
        }
    }

    fn close_channel(&mut self, out: &mut Out, at: usize) {
        let channel = &self.channels[at];
        let mut data = channel.remote.to_le_bytes().to_vec(); data.extend_from_slice(&channel.local.to_le_bytes());
        let id = self.next_id();
        out.signal(self.handle, 0x06, id, &data);
    }
    fn channel_failed(&mut self, out: &mut Out, role: Role) {
        match role {
            // Without stream signalling the link is of no use; nor without the sink check's channel.
            Role::Signal | Role::Media => out.command(DISCONNECT, &[self.handle as u8, (self.handle >> 8) as u8, 0x13]),
            Role::SdpClient if self.checking => {
                self.checking = false; self.check_at = None;
                out.command(DISCONNECT, &[self.handle as u8, (self.handle >> 8) as u8, 0x13]);
            }
            Role::SdpClient => { self.id_query = false; self.id_until = None; }
            Role::Sdp | Role::Control => {}
        }
    }
    fn channel_closed(&mut self, out: &mut Out, role: Role) {
        if role == Role::Media && self.upgrade.is_some() {
            // Their stream's media channel is gone: configure the better one (once Close was answered).
            if self.av == Av::Idle { let target = self.upgrade.take(); self.configure_stream(out, target); }
            return;
        }
        if matches!(role, Role::Signal | Role::Media) && self.stream.is_some() && self.av != Av::Idle {
            let was_open = !matches!(self.av, Av::Discover | Av::Capabilities | Av::Configure | Av::Opening | Av::Configured | Av::MediaWait | Av::Closing);
            if self.av == Av::Streaming { out.events.push_back(Event::Streaming(false)); }
            if was_open { out.events.push_back(Event::Stream(None)); }
            self.av = Av::Idle; self.stream = None; self.av_wait = None;
        }
    }

    fn opened(&mut self, out: &mut Out, at: usize) {
        let channel = &mut self.channels[at];
        if channel.open || !channel.ours_done || !channel.theirs_done { return }
        channel.open = true;
        match channel.role {
            Role::Signal => self.av_initiate_at = Some(out.now + if self.incoming { 1500 } else { 0 }),
            Role::Media => if self.av == Av::MediaWait {
                self.av = Av::Open;
                let current = self.stream.map(|s| s.kind());
                let better = out.codecs.iter().position(|k| Some(*k) == current).is_none_or(|rank| rank > 0);
                if self.remote_configured && !self.probed && better {
                    // The headphones chose: before audio starts, see whether they offer a codec we prefer.
                    self.probed = true;
                    self.probe = Some((Vec::new(), Vec::new(), 0));
                    self.av_command(out, 1, &[]);
                } else { self.announce(out); }
            },
            Role::SdpClient => {
                let remote = channel.remote;
                if self.checking { out.l2cap(self.handle, remote, &sdp::search_request(SINK_CHECK_TID, sdp::UUID_AUDIO_SINK), false); }
                else if self.id_query { out.l2cap(self.handle, remote, &sdp::device_id_request(DEVICE_ID_TID), false); }
            }
            // Ask which events the headphones report: absolute volume needs the volume event.
            Role::Control => { self.remote = Remote::default(); self.control_send(out, avrcp::get_events); }
            Role::Sdp => {}
        }
    }

    /// Sends a controller command to the headphones; it waits for its response.
    fn control_send(&mut self, out: &mut Out, make: impl FnOnce(u8) -> Vec<u8>) {
        let Some(remote) = self.channel(Role::Control).map(|c| c.remote) else { return };
        self.remote.label = (self.remote.label + 1) & 0xf;
        let packet = make(self.remote.label);
        self.remote.waiting = Some((self.remote.label, packet.get(9).copied().unwrap_or(0), out.now + AVRCP_WAIT_MS));
        out.l2cap(self.handle, remote, &packet, false);
    }
    /// Sends what waited for the last response: a renewed registration, then a new level.
    fn control_flush(&mut self, out: &mut Out) {
        if self.remote.waiting.is_some() || !self.remote.absolute { return }
        if std::mem::take(&mut self.remote.register) { return self.control_send(out, avrcp::register_volume) }
        let Some(level) = self.remote.wanted.take() else { return };
        if self.remote.level == Some(level) { return }
        self.control_send(out, |label| avrcp::set_volume(label, level));
    }
    /// A response from the headphones to one of our commands.
    fn control_reply(&mut self, out: &mut Out, packet: &[u8]) {
        let Some((label, reply)) = avrcp::reply(packet) else { return };
        let answered = self.remote.waiting.is_some_and(|w| w.0 == label);
        // Their own changes are reported; the echo of a level we are setting is not.
        let setting = self.remote.waiting.is_some_and(|w| w.1 == 0x50);
        if answered { self.remote.waiting = None; }
        match reply {
            avrcp::Reply::Events(events) => if answered && events.contains(&avrcp::VOLUME_CHANGED) {
                self.control_send(out, avrcp::register_volume);
            },
            avrcp::Reply::VolumeInterim(level) => {
                out.events.push_back(Event::AmplifierLevel(level));
                if !self.remote.absolute {
                    self.remote.absolute = true; self.remote.level = Some(level);
                    out.events.push_back(Event::AbsoluteVolume);
                    out.events.push_back(Event::Volume { level, initial: true });
                } else if self.remote.level != Some(level) {
                    self.remote.level = Some(level);
                    out.events.push_back(Event::Volume { level, initial: false });
                }
            }
            avrcp::Reply::VolumeChanged(level) => if self.remote.absolute {
                out.events.push_back(Event::AmplifierLevel(level));
                if self.remote.level != Some(level) && !setting { out.events.push_back(Event::Volume { level, initial: false }); }
                self.remote.level = Some(level);
                // A registration ends with its change: renew it.
                self.remote.register = true;
            },
            avrcp::Reply::VolumeSet(level) if answered && setting => {
                self.remote.level = Some(level);
                out.events.push_back(Event::AmplifierLevel(level));
            }
            avrcp::Reply::VolumeSet(_) => {}
            avrcp::Reply::Refused(_) => {}
        }
        self.control_flush(out);
    }

    fn av_send(&mut self, out: &mut Out, header: u8, signal: u8, body: &[u8]) {
        let Some(remote) = self.channel(Role::Signal).map(|c| c.remote) else { return };
        let mut packet = vec![header, signal]; packet.extend_from_slice(body);
        out.l2cap(self.handle, remote, &packet, false);
    }
    fn av_command(&mut self, out: &mut Out, signal: u8, body: &[u8]) {
        self.av_label = (self.av_label + 1) & 0xf;
        self.av_wait = Some((signal, out.now + 4000));
        self.av_send(out, self.av_label << 4, signal, body);
    }
    /// The stream is open: report it, open the control channel, start it if audio waits.
    fn announce(&mut self, out: &mut Out) {
        out.events.push_back(Event::Stream(self.stream_info()));
        if self.channels.iter().all(|c| c.role != Role::Control) { self.control_at = Some(out.now + 1000); }
        if self.id_at.is_none() && !self.id_query { self.id_at = Some(out.now + DEVICE_ID_AFTER_MS); }
        if self.want_stream { self.start(out); }
    }
    /// Configures `target` (their endpoint, the stream, their offer) as initiator.
    fn configure_stream(&mut self, out: &mut Out, target: Option<(u8, Codec, Vec<u8>)>) {
        let Some((seid, stream, offer)) = target else { return };
        self.remote_seid = seid; self.stream = Some(stream); self.av = Av::Configure; self.remote_configured = false;
        let element = codec::configuration(&stream, &offer);
        let mut body = vec![seid << 2, stream.kind().seid() << 2, 1, 0, 7, element.len() as u8];
        body.extend(element);
        self.av_command(out, 3, &body);
    }
    /// A reply during the check of the headphones' own choice.
    fn probe_reply(&mut self, out: &mut Out, accepted: bool, signal: u8, body: &[u8]) {
        let Some((unasked, offers, asked)) = &mut self.probe else { return };
        match (accepted, signal) {
            (true, 1) => *unasked = body.chunks_exact(2).filter(|sep| sep[1] >> 4 == 0 && sep[1] & 0x08 != 0).map(|sep| sep[0] >> 2).take(12).collect(),
            (true, 2) => { let seid = *asked; offers.extend(codec::elements(body).into_iter().map(|e| (seid, e.to_vec()))); }
            // One endpoint that will not describe itself does not end the check.
            (false, 2) => {}
            _ => { self.probe = None; return self.announce(out) }
        }
        if let Some(seid) = unasked.pop() { *asked = seid; return self.av_command(out, 2, &[seid << 2]) }
        let Some((_, offers, _)) = self.probe.take() else { return };
        let list: Vec<Vec<u8>> = offers.iter().map(|o| o.1.clone()).collect();
        let current = self.stream.map(|s| s.kind());
        let rank = |kind: Option<Kind>| kind.and_then(|k| out.codecs.iter().position(|c| *c == k)).unwrap_or(usize::MAX);
        match codec::best(&list, &out.codecs) {
            Some((at, best)) if rank(Some(best.kind())) < rank(current) => {
                // Close theirs; the better stream is configured once its media channel has closed.
                let theirs = list.iter().position(|o| codec::identify(o).map(|(k, _)| Some(k)) == Some(current))
                    .and_then(|i| self.stream.map(|s| (offers[i].0, s, list[i].clone())));
                self.fallback = theirs.filter(|t| t.0 == self.remote_seid);
                self.upgrade = Some((offers[at].0, best, list[at].clone()));
                self.origin = Origin::Upgraded { from: current.unwrap_or(Kind::Sbc) };
                self.av = Av::Closing;
                self.av_command(out, 8, &[self.remote_seid << 2]);
            }
            _ => { self.origin = Origin::Theirs { checked: true }; self.announce(out) }
        }
    }
    /// Asks the next endpoint for its capabilities, or configures the best codec offered.
    fn ask_next(&mut self, out: &mut Out) {
        if let Some(seid) = self.unasked.pop() {
            self.remote_seid = seid;
            self.av_command(out, 2, &[seid << 2]);
            return;
        }
        if out.bench_receiver {
            self.offers.retain(|(_, element)| element == &BENCH_SBC_OFFER);
            if self.offers.len() != 1 {
                self.av = Av::Idle;
                self.stream = None;
                self.offers.clear();
                out.events.push_back(Event::Stream(None));
                return;
            }
        }
        let offers: Vec<Vec<u8>> = self.offers.iter().map(|o| o.1.clone()).collect();
        match codec::best(&offers, &out.codecs) {
            Some((index, stream)) => {
                self.remote_seid = self.offers[index].0; self.stream = Some(stream); self.av = Av::Configure; self.origin = Origin::Ours;
                let element = codec::configuration(&stream, &offers[index]);
                let mut body = vec![self.remote_seid << 2, stream.kind().seid() << 2, 1, 0, 7, element.len() as u8];
                body.extend(element);
                self.av_command(out, 3, &body);
            }
            None => { self.av = Av::Idle; out.events.push_back(Event::Stream(None)); }
        }
        self.offers.clear();
    }
    fn start(&mut self, out: &mut Out) {
        if self.av == Av::Open { self.av = Av::Starting; self.av_command(out, 7, &[self.remote_seid << 2]); }
    }
    fn suspend(&mut self, out: &mut Out) {
        if self.av == Av::Streaming { self.av = Av::Suspending; self.av_command(out, 9, &[self.remote_seid << 2]); }
    }

    fn avdtp(&mut self, out: &mut Out, packet: &[u8]) {
        let Some(&[header, signal]) = packet.get(..2).map(|h| <&[u8; 2]>::try_from(h).unwrap()) else { return };
        // Fragmented signalling is not reassembled; nothing we exchange needs it at a 672-byte MTU.
        if header >> 2 & 3 != 0 { return }
        let (label, kind, signal, body) = (header >> 4, header & 3, signal & 0x3f, &packet[2..]);
        let accept = label << 4 | 2;
        let reject = label << 4 | 3;
        if kind == 0 {
            match signal {
                1 => {
                    // One endpoint per codec; the configured one is in use.
                    let seps: Vec<u8> = out.codecs.iter().flat_map(|kind| {
                        let in_use = u8::from(self.av != Av::Idle && self.stream.is_some_and(|s| s.kind() == *kind));
                        [kind.seid() << 2 | in_use << 1, 0x00]
                    }).collect();
                    self.av_send(out, accept, signal, &seps);
                }
                2 | 12 | 4 => {
                    let kind = body.first().and_then(|seid| Kind::of_seid(seid >> 2)).filter(|kind| out.codecs.contains(kind));
                    let element = match (signal, kind) {
                        (4, Some(kind)) => self.stream.filter(|s| s.kind() == kind).map(|s| codec::configuration(&s, &codec::capabilities(kind))),
                        (_, Some(kind)) => Some(codec::capabilities(kind)),
                        // Bad ACP SEID.
                        _ => None,
                    };
                    match element {
                        Some(element) => { let mut list = vec![1, 0, 7, element.len() as u8]; list.extend(element); self.av_send(out, accept, signal, &list) }
                        None => self.av_send(out, reject, signal, &[if kind.is_some() { 0x31 } else { 0x12 }]),
                    }
                }
                3 if out.bench_receiver => self.av_send(out, reject, signal, &[7, 0x29]),
                3 => {
                    let kind = body.first().and_then(|seid| Kind::of_seid(seid >> 2)).filter(|kind| out.codecs.contains(kind));
                    let element = body.get(2..).map(codec::elements).and_then(|list| list.first().copied());
                    let configured = body.get(1).map(|seid| seid >> 2).zip(element.and_then(|e| codec::choose(e, true))).filter(|(_, c)| Some(c.kind()) == kind);
                    match configured {
                        // SEP in use: we are configuring it, or it is configured.
                        _ if !matches!(self.av, Av::Idle | Av::Discover | Av::Capabilities) => self.av_send(out, reject, signal, &[7, 0x13]),
                        Some((seid, stream)) => {
                            self.remote_seid = seid; self.stream = Some(stream); self.av = Av::Configured; self.av_wait = None;
                            self.remote_configured = true; self.origin = Origin::Theirs { checked: false };
                            self.av_send(out, accept, signal, &[]);
                        }
                        // Unsupported configuration, reported against the codec category.
                        None => self.av_send(out, reject, signal, &[7, 0x29]),
                    }
                }
                6 if self.av == Av::Configured => { self.av = Av::MediaWait; self.av_send(out, accept, signal, &[]); }
                7 if out.bench_receiver && !self.want_stream => self.av_send(out, reject, signal,
                    &[body.first().copied().unwrap_or(0), 0x31]),
                7 if self.av == Av::Open && self.probe.is_some() => {
                    // They start before the check of their choice ends: keep their stream.
                    self.probe = None; self.av_wait = None;
                    self.announce(out);
                    self.av = Av::Streaming; self.av_send(out, accept, signal, &[]);
                    out.events.push_back(Event::Streaming(true));
                }
                7 if self.av == Av::Open => {
                    self.av = Av::Streaming; self.av_send(out, accept, signal, &[]);
                    out.events.push_back(Event::Streaming(true));
                }
                9 if self.av == Av::Streaming => {
                    self.av = Av::Open; self.av_send(out, accept, signal, &[]);
                    out.events.push_back(Event::Streaming(false));
                }
                8 | 10 => {
                    self.av_send(out, accept, signal, &[]);
                    self.channel_closed(out, Role::Signal);
                    self.av = Av::Idle; self.stream = None;
                }
                13 => self.av_send(out, accept, signal, &[]),
                // Bad state for 6, 7 and 9 (the first SEID is named where the reply carries one).
                6 => self.av_send(out, reject, signal, &[0x31]),
                7 | 9 => self.av_send(out, reject, signal, &[body.first().copied().unwrap_or(0), 0x31]),
                // General reject.
                _ => self.av_send(out, label << 4 | 1, signal, &[]),
            }
            return;
        }
        let Some((waiting, _)) = self.av_wait else { return };
        if waiting != signal || label != self.av_label { return }
        self.av_wait = None;
        if self.probe.is_some() { return self.probe_reply(out, kind == 2, signal, body) }
        if kind != 2 {
            // Rejected. A refused start or suspend leaves the stream open; anything else ends it.
            match self.av {
                // They will not close their stream: keep it.
                Av::Closing => { self.upgrade = None; self.fallback = None; self.av = Av::Open; self.origin = Origin::Theirs { checked: true }; self.announce(out); }
                // The better configuration was refused: configure theirs again.
                Av::Configure | Av::Opening if self.fallback.is_some() => {
                    let fallback = self.fallback.take();
                    self.origin = Origin::Theirs { checked: true };
                    self.configure_stream(out, fallback);
                }
                // One endpoint that will not describe itself does not end the search.
                Av::Capabilities => self.ask_next(out),
                Av::Starting => { self.av = Av::Open; out.events.push_back(Event::Streaming(false)); }
                Av::Suspending => self.av = Av::Streaming,
                _ => { self.av = Av::Idle; self.stream = None; out.events.push_back(Event::Stream(None)); }
            }
            return;
        }
        match (self.av, signal) {
            (Av::Discover, 1) => {
                // Audio sinks, free ones first: each is asked for its capabilities (one codec per endpoint is usual).
                let mut sinks: Vec<(bool, u8)> = body.chunks_exact(2).filter(|sep| sep[1] >> 4 == 0 && sep[1] & 0x08 != 0).map(|sep| (sep[0] & 2 != 0, sep[0] >> 2)).collect();
                sinks.sort();
                sinks.truncate(12);
                self.unasked = sinks.into_iter().rev().map(|(_, seid)| seid).collect();
                self.offers.clear();
                self.av = Av::Capabilities;
                self.ask_next(out);
            }
            (Av::Capabilities, 2) => {
                for element in codec::elements(body) { self.offers.push((self.remote_seid, element.to_vec())); }
                self.ask_next(out);
            }
            (Av::Configure, 3) => { self.av = Av::Opening; self.av_command(out, 6, &[self.remote_seid << 2]); }
            (Av::Opening, 6) => { self.av = Av::MediaWait; self.connect(out, PSM_AVDTP, Role::Media); }
            (Av::Starting, 7) => { self.av = Av::Streaming; out.events.push_back(Event::Streaming(true)); if !self.want_stream { self.suspend(out); } }
            (Av::Suspending, 9) => { self.av = Av::Open; out.events.push_back(Event::Streaming(false)); if self.want_stream { self.start(out); } }
            (Av::Closing, 8) => {
                self.av = Av::Idle; self.stream = None;
                // We close the transport channel; the better stream follows once it is gone.
                match self.channels.iter().position(|c| c.role == Role::Media && c.open) {
                    Some(at) => self.close_channel(out, at),
                    None => { let target = self.upgrade.take(); self.configure_stream(out, target); }
                }
            }
            _ => {}
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase { Sync, Version, Rom, Baud, Download(usize),
    /// Moving a controller found at `Settings::previous` to `Settings::baud`.
    Move,
    Init(usize), Ready, Failed }

/// A device found by the scan: address, class, RSSI, whether it was named, whether its EIR lists an A2DP sink.
type Seen = (Addr, u32, i8, bool, bool);
struct Scan { found: Vec<Seen>, names: VecDeque<Addr>, inquiring: bool, deadline: u64,
    /// The device whose name request is out.
    naming: Option<Addr>,
    /// Inquiries started.
    rounds: u8 }

pub struct Host {
    settings: Settings,
    h5: h5::H5,
    out: Out,
    phase: Phase,
    fast: bool,
    /// The sync's current UART setting: 115200, `Settings::baud`, or `Settings::previous`.
    sync_at: u8,
    sync_deadline: u64,
    sync_attempts: u8,
    pending: Option<(u16, u64)>,
    hold_until: u64,
    retune: bool,
    version: Version,
    image: Vec<u8>,
    acl_total: u16,
    acl_credits: u16,
    address: Addr,
    scan: Option<Scan>,
    /// The address being connected before its link exists, whether it connected to us,
    /// and whether it must pass the sink check (`ConnectSink`).
    connecting: Option<(Addr, bool, bool)>,
    link: Option<Link>,
    playing: bool,
    diagnostics: Diagnostics,
    raw: Vec<u16>,
    verify_patch: bool,
    stats: LinkStats,
    /// Since when ACL data has waited for a controller buffer.
    stall_since: Option<u64>,
    /// When to read RSSI and link quality next; whether the controller answers each.
    quality_at: u64,
    /// Whether the controller answers Read RSSI, Get Link Quality, Read Transmit Power Level and Read AFH Channel Map.
    quality_ok: [bool; 4],
    quality_round: u32,
    /// The profile report last sent: (link handle, streaming); None for no link.
    coex: Option<(u16, bool)>,
    coex_report: bool,
    /// `Command::Check` waits for its version reply until this time.
    check: Option<u64>,
    /// `connecting` gives up at this time ([`PAGE_LIMIT_MS`]).
    connect_by: Option<u64>,
    /// Names learned by earlier scans of this session, so a new scan need not ask again.
    names: Vec<(Addr, String)>,
    /// `CancelConnect` was asked for the page in `connecting`: should the page complete
    /// before the cancel (the controller answers 0x0b), the link is closed.
    cancel_asked: bool,
}

impl Host {
    /// The UART must be at 115200 baud, 8 data bits, even parity, no flow control.
    pub fn new(mut settings: Settings, now: u64) -> Self {
        // A previous setting equal to one the sync tries anyway is not tried twice.
        settings.previous = settings.previous.filter(|&p| p != (INITIAL_BAUD, false) && p != (settings.baud, settings.flow) && rtl::baud_code(p.0).is_some());
        let codecs = if settings.codecs.is_empty() { vec![Kind::Sbc] } else { settings.codecs.clone() };
        let bench_receiver = settings.bench_peer.is_some();
        Self { settings, h5: h5::H5::new(now), phase: Phase::Sync, fast: false, sync_at: 0, sync_deadline: now + 1500, sync_attempts: 0, pending: None,
            hold_until: 0, retune: false, version: Version::default(), image: Vec::new(), acl_total: 1, acl_credits: 1, address: [0; 6], scan: None,
            connecting: None, link: None, playing: false, diagnostics: Diagnostics::default(), raw: Vec::new(), verify_patch: false, check: None, connect_by: None, names: Vec::new(), cancel_asked: false,
            stats: LinkStats::default(), stall_since: None, quality_at: 0, quality_ok: [true; 4], quality_round: 0, coex: None, coex_report: true,
            out: Out { now, acl_mtu: 255, acl: VecDeque::new(), media: VecDeque::new(), media_dropped: 0, events: VecDeque::new(), commands: VecDeque::new(), codecs, bench_receiver, volume: 0x40, volume_limit: 127 } }
    }
    pub fn events(&mut self) -> impl Iterator<Item = Event> + '_ { self.out.events.drain(..) }
    /// Bound levels advertised, accepted, or sent on the control channel, including queued sets.
    pub fn set_volume_limit(&mut self, level: u8) {
        self.out.volume_limit = level.min(127);
        self.out.volume = self.out.volume.min(self.out.volume_limit);
        if let Some(pending) = self.link.as_mut().and_then(|link| link.remote.wanted.as_mut()) {
            *pending = (*pending).min(self.out.volume_limit);
        }
    }
    /// Changes the codecs offered and preferred; an open stream keeps its codec.
    pub fn set_codecs(&mut self, codecs: Vec<Kind>) { self.out.codecs = if codecs.is_empty() { vec![Kind::Sbc] } else { codecs }; }
    pub fn ready(&self) -> bool { self.phase == Phase::Ready }
    /// Ready with no link, connection attempt, scan or check open: `Command::Reinit` would run.
    pub fn idle(&self) -> bool { self.ready() && self.link.is_none() && self.connecting.is_none() && self.scan.is_none() && self.check.is_none() }
    pub fn scanning(&self) -> bool { self.scan.is_some() }
    /// The device of the encrypted link, if any.
    pub fn connected(&self) -> Option<Addr> { self.link.as_ref().filter(|l| l.secured).map(|l| l.addr) }
    /// A link exists, encrypted or not.
    pub fn linked(&self) -> bool { self.link.is_some() }
    /// `Command::Scan` would start a scan now: ready, none open, no connection attempt and
    /// no link still being authenticated.
    pub fn can_scan(&self) -> bool { self.ready() && self.scan.is_none() && self.connecting.is_none() && self.link.as_ref().is_none_or(|l| l.secured) }
    /// `Command::Connect` would start now: ready, no link and no connection attempt.
    pub fn can_connect(&self) -> bool { self.ready() && self.link.is_none() && self.connecting.is_none() }
    /// The device of an outgoing page in progress (`Command::CancelConnect` ends it).
    pub fn paging(&self) -> Option<Addr> { self.connecting.filter(|c| !c.1).map(|c| c.0) }
    pub fn stream(&self) -> Option<Stream> { self.link.as_ref().and_then(Link::stream_info) }
    /// How the open stream's codec was chosen.
    pub fn origin(&self) -> Option<Origin> { self.link.as_ref().filter(|l| l.stream_info().is_some()).map(|l| l.origin) }
    pub fn streaming(&self) -> bool { self.link.as_ref().is_some_and(|l| l.av == Av::Streaming) }
    /// Media packets dropped because the controller did not take them in time.
    pub fn media_dropped(&self) -> u32 { self.out.media_dropped }
    pub fn diagnostics(&self) -> Diagnostics {
        Diagnostics { h5_sleep: self.h5.sleep_messages, h5_retransmissions: self.h5.retransmissions, h5_rejected: self.h5.rejected, ..self.diagnostics.clone() }
    }
    pub fn transport_counters(&self) -> (u32, u32) { (self.h5.retransmissions, self.h5.rejected) }
    /// Audio-path counters; a credit stall in progress is included.
    pub fn link_stats(&self) -> LinkStats {
        let current = self.stall_since.map_or(0, |since| self.out.now.saturating_sub(since)) as u32;
        LinkStats {
            acl_buffers: self.acl_total, acl_in_use: self.acl_total.saturating_sub(self.acl_credits),
            credit_stall_ms: self.stats.credit_stall_ms.saturating_add(current), credit_stall_max_ms: self.stats.credit_stall_max_ms.max(current),
            h5_retransmissions: self.h5.retransmissions, h5_resends: self.h5.resends, h5_bad_frames: self.h5.bad_frames,
            h5_out_of_sequence: self.h5.out_of_sequence, ..self.stats
        }
    }
    /// Whether to send Realtek's profile report (on by default, as the stock stack does).
    pub fn set_coex_report(&mut self, on: bool) { self.coex_report = on; }
    /// ACL packets accepted and not yet sent by the controller over the air: queued
    /// here (media and other), in H5, and in the controller's buffers.
    pub fn acl_backlog(&self) -> usize {
        self.out.media.iter().map(Vec::len).sum::<usize>() + self.out.acl.len() + self.h5.backlog()
            + usize::from(self.acl_total.saturating_sub(self.acl_credits))
    }
    /// Tells status queries from the headphones whether music is playing.
    pub fn set_playing(&mut self, playing: bool) {
        if self.playing == playing { return }
        self.playing = playing;
        let Some(link) = &mut self.link else { return };
        if let (Some(label), Some(remote)) = (link.status_label.take(), link.channel(Role::Control).map(|c| c.remote)) {
            self.out.l2cap(link.handle, remote, &avrcp::status_changed(label, playing), false);
        }
    }

    fn fail(&mut self, message: impl Into<String>) {
        if self.phase == Phase::Failed { return }
        self.phase = Phase::Failed;
        self.out.events.push_back(Event::Failed(message.into()));
    }

    pub fn command(&mut self, command: Command) {
        if self.phase != Phase::Ready { return }
        match command {
            Command::Scan => if self.scan.is_none() && self.connecting.is_none() {
self.scan = Some(Scan { found: Vec::new(), names: VecDeque::new(), inquiring: false, deadline: 0, naming: None, rounds: 0 });
                self.inquire();
            },
            Command::StopScan => self.stop_scan(),
            Command::Connect(addr) | Command::ConnectSink(addr) => {
                if self.settings.bench_peer.is_some_and(|peer| peer != addr) { return }
                if self.link.is_some() || self.connecting.is_some() { return }
                self.stop_scan();
                self.connecting = Some((addr, false, matches!(command, Command::ConnectSink(_))));
                self.connect_by = Some(self.out.now + PAGE_LIMIT_MS);
                self.cancel_asked = false;
                self.out.events.push_back(Event::Connecting(addr));
                let mut p = addr.to_vec();
                // All ACL packet types, page scan mode R1, no clock offset, allow a role switch.
                p.extend_from_slice(&[0x18, 0xcc, 0x01, 0x00, 0x00, 0x00, 0x01]);
                self.out.command(CREATE_CONNECTION, &p);
            }
            Command::Disconnect => if let Some(link) = &self.link {
                self.out.command(DISCONNECT, &[link.handle as u8, (link.handle >> 8) as u8, 0x13]);
            },
            Command::Forget(addr) => {
                self.settings.keys.retain(|k| k.addr != addr);
                if let Some(link) = self.link.as_ref().filter(|l| l.addr == addr) {
                    self.out.command(DISCONNECT, &[link.handle as u8, (link.handle >> 8) as u8, 0x13]);
                }
            }
            Command::Raw(opcode, parameters) => { self.raw.push(opcode); self.out.command(opcode, &parameters); }
            Command::Check => if self.check.is_none() {
                self.check = Some(self.out.now + CHECK_MS);
                self.out.command(READ_LOCAL_VERSION, &[]);
            },
            Command::Reinit => if self.idle() {
                self.verify_patch = false;
                self.init();
            },
            Command::CancelConnect => if let Some(addr) = self.paging() { self.cancel_asked = true; self.out.command(CREATE_CONNECTION_CANCEL, &addr); },
            Command::Start => if let Some(link) = &mut self.link { link.want_stream = true; link.start(&mut self.out); },
            Command::Suspend => if let Some(link) = &mut self.link { link.want_stream = false; link.suspend(&mut self.out); },
            Command::SetVolume(level) => {
                let level = level.min(self.out.volume_limit);
                self.out.volume = level;
                let Some(link) = &mut self.link else { return };
                if link.remote.absolute { link.remote.wanted = Some(level); link.control_flush(&mut self.out); }
                if let (Some(label), Some(remote)) = (link.volume_label.take(), link.channel(Role::Control).map(|c| c.remote)) {
                    self.out.l2cap(link.handle, remote, &avrcp::volume_changed(label, level), false);
                }
            }
        }
    }
    /// Starts one general inquiry of 10.24 s with any number of responses. A scan
    /// runs a second one when the first found nothing or was never reported complete.
    fn inquire(&mut self) {
        let Some(scan) = &mut self.scan else { return };
        scan.inquiring = true; scan.rounds += 1;
        // Each name request later extends the deadline.
        scan.deadline = self.out.now + 14_000;
        self.out.command(INQUIRY, &[0x33, 0x8b, 0x9e, 8, 0]);
    }
    fn stop_scan(&mut self) {
        let Some(scan) = self.scan.take() else { return };
        if scan.inquiring { self.out.command(INQUIRY_CANCEL, &[]); }
        // A name request pages the device: end it, so a connection can page at once.
        if let Some(addr) = scan.naming { self.out.command(REMOTE_NAME_CANCEL, &addr); }
        self.out.events.push_back(Event::ScanFinished);
    }

    /// Whether another media packet would be queued without dropping one.
    pub fn media_room(&self) -> bool { self.out.media.len() < 4 }
    /// Sends `count` SBC frames that together hold `samples` PCM frames.
    pub fn send_media(&mut self, frames: &[u8], count: u8, samples: u32) {
        let mut payload = Vec::with_capacity(1 + frames.len());
        payload.push(count & 0x0f);
        payload.extend_from_slice(frames);
        self.send_media_payload(&payload, samples, false);
    }
    /// Sends one media packet: `payload` is everything after the RTP header (for aptX,
    /// which has none, the whole packet). `samples` advances the RTP timestamp;
    /// `marker` sets the RTP marker bit (the last fragment of an AAC access unit).
    pub fn send_media_payload(&mut self, payload: &[u8], samples: u32, marker: bool) {
        let Some(link) = &mut self.link else { return };
        let Some(remote) = link.channel(Role::Media).map(|c| c.remote) else { return };
        if link.av != Av::Streaming { return }
        let mut packet = Vec::with_capacity(12 + payload.len());
        if link.stream.is_none_or(|s| s.rtp()) {
            packet.extend_from_slice(&[0x80, 0x60 | if marker { 0x80 } else { 0 }]);
            packet.extend_from_slice(&link.sequence.to_be_bytes());
            packet.extend_from_slice(&link.timestamp.to_be_bytes());
            packet.extend_from_slice(&[0, 0, 0, 1]);
        }
        packet.extend_from_slice(payload);
        link.sequence = link.sequence.wrapping_add(1);
        link.timestamp = link.timestamp.wrapping_add(samples);
        self.out.l2cap(link.handle, remote, &packet, true);
    }

    /// Feeds bytes read from the UART.
    pub fn rx(&mut self, bytes: &[u8], now: u64) {
        self.out.now = now;
        let mut packets = Vec::new();
        self.h5.rx(bytes, now, &mut packets);
        for (ty, payload) in packets {
            match ty {
                h5::EVT if payload.len() >= 2 && payload.len() == 2 + payload[1] as usize => self.event(payload[0], &payload[2..]),
                h5::ACL if payload.len() >= 4 => {
                    let (word, len) = (u16::from_le_bytes([payload[0], payload[1]]), u16::from_le_bytes([payload[2], payload[3]]) as usize);
                    let playing = self.playing;
                    if let (Some(link), Some(data)) = (self.link.as_mut().filter(|l| l.handle == word & 0x0fff), payload.get(4..4 + len)) {
                        link.acl(&mut self.out, (word >> 12) as u8, data, playing, &mut self.diagnostics.sink_checks);
                    }
                }
                _ => self.diagnostics.unparsed += 1,
            }
        }
    }

    /// Appends the bytes to write to `wire` and returns when to call again at the latest.
    pub fn poll(&mut self, now: u64, wire: &mut Vec<u8>) -> Option<u64> {
        self.out.now = now;
        let mut deadlines = Vec::new();
        if self.phase == Phase::Sync {
            if self.h5.link() == h5::Link::Active {
                self.phase = Phase::Version;
                self.out.command(READ_LOCAL_VERSION, &[]);
            } else if now >= self.sync_deadline {
                // A controller that kept its patch across a player restart answers at the fast rate.
                self.sync_attempts += 1;
                if self.sync_attempts >= 6 { self.fail("The Bluetooth controller does not answer"); } else {
                    // 115200, the fast rate, and the previous setting if there is one, in turn.
                    self.sync_at = (self.sync_at + 1) % if self.settings.previous.is_some() { 3 } else { 2 };
                    self.fast = self.sync_at != 0;
                    self.h5.restart(now);
                    self.sync_deadline = now + 1500;
                    self.hold_until = now + 20;
                    let (baud, flow) = self.sync_setting();
                    self.out.events.push_back(Event::Uart { baud, flow });
                    return Some(self.hold_until);
                }
            }
            deadlines.push(self.sync_deadline);
        }
        if self.h5.peer_reset {
            self.h5.peer_reset = false;
            if self.phase == Phase::Ready { self.fail("The Bluetooth controller restarted"); }
        }
        if let Some(deadline) = self.check {
            if now >= deadline { self.check = None; self.out.events.push_back(Event::Lost); } else { deadlines.push(deadline); }
        }
        if let Some((opcode, deadline)) = self.pending {
            if now >= deadline {
                self.pending = None;
                self.diagnostics.command_timeouts += 1;
                // An inquiry or name request the controller never acknowledged must not leave the scan open.
                if matches!(opcode, INQUIRY | REMOTE_NAME) && self.scan.take().is_some() { self.out.events.push_back(Event::ScanFinished); }
                if self.phase != Phase::Ready { self.fail(format!("The Bluetooth controller did not answer command {opcode:#06x}")); }
            } else { deadlines.push(deadline); }
        }
        if let Some(deadline) = self.scan.as_ref().map(|scan| scan.deadline) {
            if now >= deadline {
                self.diagnostics.scan_timeouts += 1;
                // An inquiry the controller never reported complete: with nothing found, cancel it
                // and run one more; with devices found, cancel it and go on as if it had completed.
                let (inquiring, empty, again) = self.scan.as_ref().map_or((false, true, false), |scan| (scan.inquiring, scan.found.is_empty(), scan.rounds < 2));
                if inquiring && empty && again { self.out.command(INQUIRY_CANCEL, &[]); self.inquire(); }
                else if inquiring && !empty {
                    self.out.command(INQUIRY_CANCEL, &[]);
                    if let Some(scan) = &mut self.scan { scan.inquiring = false; }
                    self.inquiry_done();
                } else { self.stop_scan(); }
            } else { deadlines.push(deadline); }
        }
        if self.connecting.is_none() { self.connect_by = None; }
        if let Some(at) = self.connect_by {
            if now >= at {
                // No Connection Complete: stop the page and take the next attempt.
                self.connect_by = None;
                if let Some((addr, incoming, _)) = self.connecting.take() {
                    if !incoming { self.out.command(CREATE_CONNECTION_CANCEL, &addr); }
                    self.out.events.push_back(Event::TimedOut { addr, step: Step::Page });
                }
            } else { deadlines.push(at); }
        }
        if let Some(link) = &mut self.link { deadlines.extend(link.tick(&mut self.out, &mut self.diagnostics.sink_checks)); }
        if self.link.as_ref().is_some_and(|l| l.closing_at.is_some_and(|at| now >= at)) {
            // The controller did not confirm the disconnect: forget the link so a new attempt can start.
            if let Some(link) = self.link.take() { self.closed(link, 0x16); }
        }
        self.report_profile();
        if self.phase == Phase::Failed { self.out.commands.clear(); self.out.acl.clear(); self.out.media.clear(); return None }
        if now < self.hold_until { deadlines.push(self.hold_until); } else if self.h5.link() == h5::Link::Active {
            if self.pending.is_none() {
                if let Some((opcode, parameters)) = self.out.commands.pop_front() {
                    let mut packet = vec![opcode as u8, (opcode >> 8) as u8, parameters.len() as u8];
                    packet.extend(parameters);
                    self.h5.send(h5::CMD, packet);
                    self.pending = Some((opcode, now + 5000));
                    deadlines.push(now + 5000);
                }
            }
            while self.acl_credits > 0 && self.h5.backlog() < 6 {
                if self.out.acl.is_empty() { if let Some(packet) = self.out.media.pop_front() { self.out.acl.extend(packet); } }
                let Some(packet) = self.out.acl.pop_front() else { break };
                self.h5.send(h5::ACL, packet);
                self.acl_credits -= 1;
            }
            let waiting = !self.out.acl.is_empty() || !self.out.media.is_empty();
            if self.acl_credits == 0 && waiting { self.stall_since.get_or_insert(now); }
            else if let Some(since) = self.stall_since.take() { self.stalled(now.saturating_sub(since)); }
            if let Some(handle) = self.link.as_ref().filter(|l| l.secured && l.av == Av::Streaming).map(|l| l.handle) {
                if now >= self.quality_at && self.out.commands.is_empty() {
                    self.quality_at = now + QUALITY_MS;
                    let h = handle.to_le_bytes();
                    if self.quality_ok[0] { self.out.command(READ_RSSI, &h); }
                    if self.quality_ok[1] { self.out.command(GET_LINK_QUALITY, &h); }
                    // Transmit power and the hopping map change slowly: every fifth round.
                    if self.quality_round % 5 == 0 {
                        if self.quality_ok[2] { self.out.command(READ_TX_POWER, &[h[0], h[1], 0]); }
                        if self.quality_ok[3] { self.out.command(READ_AFH_MAP, &h); }
                    }
                    self.quality_round = self.quality_round.wrapping_add(1);
                }
                deadlines.push(self.quality_at);
            }
        }
        deadlines.extend(self.h5.poll(now, wire));
        if self.retune {
            self.retune = false;
            self.out.events.push_back(Event::Uart { baud: self.settings.baud, flow: self.settings.flow });
        }
        deadlines.into_iter().min()
    }

    /// The UART setting of the sync's current attempt.
    fn sync_setting(&self) -> (u32, bool) {
        match (self.sync_at, self.settings.previous) {
            (0, _) => (INITIAL_BAUD, false),
            (2, Some(previous)) => previous,
            _ => (self.settings.baud, self.settings.flow),
        }
    }

    /// Tells the controller when an A2DP stream starts and stops, so the Wi-Fi
    /// coexistence logic sees it (docs/bluetooth.md, coexistence).
    fn report_profile(&mut self) {
        if !self.coex_report || self.phase != Phase::Ready { return }
        let now = self.link.as_ref().filter(|l| l.secured).map(|l| (l.handle, l.av == Av::Streaming));
        if now == self.coex { return }
        let parameters = match now {
            Some((handle, streaming)) => vec![1, handle as u8, (handle >> 8) as u8, PROFILE_A2DP, if streaming { PROFILE_A2DP } else { 0 }],
            None => vec![0, 0],
        };
        self.coex = now;
        self.out.command(RTL_PROFILE_REPORT, &parameters);
    }

    fn stalled(&mut self, ms: u64) {
        let ms = ms.min(u64::from(u32::MAX)) as u32;
        self.stats.credit_stall_ms = self.stats.credit_stall_ms.saturating_add(ms);
        self.stats.credit_stall_max_ms = self.stats.credit_stall_max_ms.max(ms);
        if u64::from(ms) >= CREDIT_STALL_MS { self.stats.credit_stalls += 1; self.stats.credit_stall_last_ms = ms; }
    }

    fn complete(&mut self, opcode: u16, status: u8, mut r: Rd) {
        let now = self.out.now;
        match (self.phase, opcode) {
            // Unsupported (or refused): stop asking. The handle is not checked: there is one link.
            (_, READ_RSSI) => if status == 0 { r.u16(); self.stats.rssi = Some(r.u8() as i8); } else { self.quality_ok[0] = false; },
            (_, GET_LINK_QUALITY) => if status == 0 { r.u16(); self.stats.link_quality = Some(r.u8()); } else { self.quality_ok[1] = false; },
            (_, READ_TX_POWER) => if status == 0 { r.u16(); self.stats.tx_power = Some(r.u8() as i8); } else { self.quality_ok[2] = false; },
            (_, READ_AFH_MAP) => if status == 0 && r.0.len() >= 13 {
                r.u16(); r.u8();
                // 79 channels in the low bits of 10 bytes.
                self.stats.afh_channels = Some(r.0[..10].iter().map(|b| b.count_ones()).sum::<u32>().min(79) as u8);
            } else { self.quality_ok[3] = false; },
            (Phase::Version, READ_LOCAL_VERSION) => {
                if status != 0 { return self.fail("The Bluetooth controller refused the version query") }
                self.version = Version { hci_version: r.u8(), hci_revision: r.u16(), lmp_version: r.u8(), manufacturer: r.u16(), lmp_subversion: r.u16(),
                    rom_version: None, already_patched: self.fast };
                if !self.fast && self.settings.firmware.is_some() {
                    self.phase = Phase::Rom;
                    self.out.command(RTL_ROM_VERSION, &[]);
                } else if self.sync_at == 2 {
                    // Left at the previous setting by a failed bring-up: to this one's rate first.
                    let Some(code) = rtl::baud_code(self.settings.baud) else { return self.fail("Bluetooth needs a supported UART rate") };
                    self.phase = Phase::Move;
                    self.out.command(RTL_BAUD, &code.to_le_bytes());
                } else { self.init(); }
            }
            (Phase::Rom, RTL_ROM_VERSION) => {
                let rom = if status == 0 { r.u8() } else { 0 };
                self.version.rom_version = Some(rom);
                let firmware = self.settings.firmware.as_deref().unwrap_or(&[]);
                let image = rtl::select_patch(firmware, rom).and_then(|patch| {
                    let mut image = patch.data;
                    if let Some(config) = &self.settings.config { image.extend(rtl::with_uart(config, self.settings.baud, self.settings.flow)?); }
                    Ok(image)
                });
                let code = rtl::baud_code(self.settings.baud);
                match (image, code) {
                    (Ok(image), Some(code)) if self.settings.config.is_some() => {
                        self.image = image; self.phase = Phase::Baud;
                        self.out.command(RTL_BAUD, &code.to_le_bytes());
                    }
                    (Ok(_), _) => self.fail("Bluetooth firmware needs its configuration file and a supported UART rate"),
                    (Err(error), _) => self.fail(format!("Bluetooth firmware: {error}")),
                }
            }
            (Phase::Baud, RTL_BAUD) => {
                if status != 0 { return self.fail("The Bluetooth controller refused the UART rate") }
                // The acknowledgement leaves at the old rate; everything after it waits for the new one.
                self.retune = true; self.fast = true; self.hold_until = now + 100;
                let fragments: Vec<Vec<u8>> = rtl::fragments(&self.image).collect();
                self.phase = Phase::Download(fragments.len());
                for fragment in fragments { self.out.command(RTL_DOWNLOAD, &fragment); }
            }
            (Phase::Move, RTL_BAUD) => {
                if status != 0 { return self.fail("The Bluetooth controller refused the UART rate") }
                // As in `Phase::Baud`: the acknowledgement left at the previous setting.
                self.retune = true; self.sync_at = 1; self.hold_until = now + 100;
                self.init();
            }
            (Phase::Download(left), RTL_DOWNLOAD) => {
                if status != 0 { return self.fail(format!("Bluetooth firmware download failed with status {status:#04x}")) }
                if left > 1 { self.phase = Phase::Download(left - 1); } else {
                    // The patch starts now and applies the configuration.
                    self.image = Vec::new(); self.hold_until = now + 150;
                    self.init();
                }
            }
            (Phase::Init(left), _) => {
                match opcode {
                    READ_BUFFER_SIZE if status == 0 => {
                        self.out.acl_mtu = r.u16() as usize; r.u8();
                        self.acl_total = r.u16().max(1); self.acl_credits = self.acl_total;
                    }
                    READ_BD_ADDR if status == 0 => self.address = r.addr(),
                    RESET | READ_BUFFER_SIZE if status != 0 => return self.fail(format!("Bluetooth start-up command {opcode:#06x} failed with status {status:#04x}")),
                    _ => {}
                }
                if left > 1 { self.phase = Phase::Init(left - 1); } else {
                    self.phase = Phase::Ready;
                    if self.fast { self.h5.set_resend_ms(FAST_RESEND_MS); }
                    self.out.events.push_back(Event::Ready { addr: self.address });
                }
            }
            _ => {}
        }
    }

    fn init(&mut self) {
        self.out.events.push_back(Event::Controller(self.version));
        let mut name = [0u8; 248];
        let bytes = self.settings.name.as_bytes();
        let len = bytes.len().min(247);
        name[..len].copy_from_slice(&bytes[..len]);
        // After a download, the version is read again: a patch that took changes the revision and subversion.
        self.verify_patch = self.version.rom_version.is_some();
        let scan_enable = if self.settings.bench_peer.is_some() { [0x03] } else { [0x02] };
        let commands: [(u16, &[u8]); 12] = [
            (RESET, &[]), (READ_LOCAL_VERSION, &[]), (READ_BUFFER_SIZE, &[]), (READ_BD_ADDR, &[]),
            // The default events plus Secure Simple Pairing and extended inquiry results.
            (SET_EVENT_MASK, &[0xff, 0xff, 0xfb, 0xff, 0x07, 0xf8, 0xbf, 0x3d]),
            (WRITE_SSP_MODE, &[1]), (WRITE_CLASS, &CLASS_OF_DEVICE), (WRITE_LOCAL_NAME, &name), (WRITE_INQUIRY_MODE, &[2]),
            // Page for 5.12 s; allow role switch and sniff; answer pages from paired headphones, stay undiscoverable.
            (WRITE_PAGE_TIMEOUT, &[0x00, 0x20]), (WRITE_LINK_POLICY, &[0x05, 0x00]), (WRITE_SCAN_ENABLE, &scan_enable),
        ];
        self.phase = Phase::Init(commands.len());
        for (opcode, parameters) in commands { self.out.command(opcode, parameters); }
    }

    fn found(&mut self, addr: Addr, class: u32, rssi: i8, name: Option<String>, sink: bool) {
        if self.scan.is_none() { return }
        let name = match name {
            Some(name) => {
                self.names.retain(|n| n.0 != addr);
                if self.names.len() >= 32 { self.names.remove(0); }
                self.names.push((addr, name.clone()));
                Some(name)
            }
            None => self.names.iter().find(|n| n.0 == addr).map(|n| n.1.clone()),
        };
        let Some(scan) = &mut self.scan else { return };
        let sink = match scan.found.iter_mut().find(|f| f.0 == addr) {
            Some(known) if known.3 || name.is_none() => return,
            Some(known) => { known.3 = true; known.4 || sink }
            None => { scan.found.push((addr, class, rssi, name.is_some(), sink)); sink }
        };
        self.out.events.push_back(Event::Found { addr, class, rssi, name, sink });
    }
    /// The inquiry ended: ask for the names the responses did not carry, audio devices first.
    fn inquiry_done(&mut self) {
        let Some(scan) = &mut self.scan else { return };
        let mut unnamed: Vec<_> = scan.found.iter().filter(|f| !f.3).map(|f| (f.1 >> 8 & 0x1f != 4 && !f.4, f.0)).collect();
        unnamed.sort();
        scan.names = unnamed.into_iter().take(8).map(|(_, addr)| addr).collect();
        self.next_name();
    }
    fn next_name(&mut self) {
        let Some(scan) = &mut self.scan else { return };
        match scan.names.pop_front() {
            Some(addr) => { scan.deadline = self.out.now + 8_000; scan.naming = Some(addr); let mut p = addr.to_vec(); p.extend_from_slice(&[1, 0, 0, 0]); self.out.command(REMOTE_NAME, &p); }
            None => { self.scan = None; self.out.events.push_back(Event::ScanFinished); }
        }
    }
    fn key(&self, addr: Addr) -> Option<&Key> { self.settings.keys.iter().find(|k| k.addr == addr) }
    /// The link is gone: its stream ends, its queued packets are dropped.
    fn closed(&mut self, link: Link, reason: u8) {
        if link.av == Av::Streaming { self.out.events.push_back(Event::Streaming(false)); }
        if link.stream_info().is_some() { self.out.events.push_back(Event::Stream(None)); }
        self.out.acl.clear(); self.out.media.clear();
        self.acl_credits = self.acl_total;
        if let Some(since) = self.stall_since.take() { self.stalled(self.out.now.saturating_sub(since)); }
        self.stats.rssi = None; self.stats.link_quality = None; self.stats.tx_power = None; self.stats.afh_channels = None;
        self.out.events.push_back(if link.secured { Event::Disconnected { addr: link.addr, reason } } else { Event::ConnectFailed { addr: link.addr, status: reason } });
    }

    fn event(&mut self, code: u8, p: &[u8]) {
        let mut r = Rd(p);
        self.diagnostics.events += 1;
        match self.diagnostics.by_code.iter_mut().find(|c| c.0 == code) { Some(c) => c.1 += 1, None => self.diagnostics.by_code.push((code, 1)) }
        let (opcode, status) = match code {
            0x0e => (u16::from_le_bytes([p.get(1).copied().unwrap_or(0), p.get(2).copied().unwrap_or(0)]), p.get(3).copied().unwrap_or(0)),
            0x0f => (u16::from_le_bytes([p.get(2).copied().unwrap_or(0), p.get(3).copied().unwrap_or(0)]), p.first().copied().unwrap_or(0)),
            _ => (0, p.first().copied().unwrap_or(0)),
        };
        // Bring-up's download would fill the timeline: keep what follows it.
        if !matches!(opcode, RTL_DOWNLOAD | READ_RSSI | GET_LINK_QUALITY | READ_TX_POWER | READ_AFH_MAP) {
            if self.diagnostics.timeline.len() >= 40 { self.diagnostics.timeline.remove(0); }
            self.diagnostics.timeline.push((self.out.now, code, opcode, status));
        }
        if code == 0x0e {
            if let Some(at) = self.raw.iter().position(|&raw| raw == opcode) {
                self.raw.remove(at);
                if self.diagnostics.replies.len() < 16 { self.diagnostics.replies.push((opcode, p.get(3..).unwrap_or(&[]).to_vec())); }
            }
            if opcode == READ_LOCAL_VERSION && self.phase == Phase::Ready && self.check.take().is_some() {
                self.out.events.push_back(Event::Alive);
            }
            if opcode == READ_LOCAL_VERSION && self.verify_patch && p.len() >= 12 {
                self.verify_patch = false;
                self.diagnostics.patched_version = Some((u16::from_le_bytes([p[5], p[6]]), u16::from_le_bytes([p[10], p[11]])));
            }
        }
        match code {
            0x02 | 0x22 | 0x2f => self.diagnostics.inquiry_results += 1,
            0x01 => self.diagnostics.inquiry_completes += 1,
            0x07 => self.diagnostics.name_replies += 1,
            _ => {}
        }
        match code {
            0x0e => {
                r.u8();
                let opcode = r.u16();
                if self.pending.is_some_and(|(pending, _)| pending == opcode) { self.pending = None; }
                let status = r.u8();
                if status != 0 && opcode != 0 && self.diagnostics.command_errors.len() < 8 { self.diagnostics.command_errors.push((opcode, status)); }
                self.complete(opcode, status, r);
            }
            0x0f => {
                let status = r.u8(); r.u8();
                let opcode = r.u16();
                if self.pending.is_some_and(|(pending, _)| pending == opcode) { self.pending = None; }
                if status == 0 { return }
                if self.diagnostics.command_errors.len() < 8 { self.diagnostics.command_errors.push((opcode, status)); }
                match opcode {
                    CREATE_CONNECTION => if let Some((addr, ..)) = self.connecting.take() { self.out.events.push_back(Event::ConnectFailed { addr, status }); },
                    INQUIRY => if self.scan.take().is_some() { self.out.events.push_back(Event::ScanFinished); },
                    REMOTE_NAME => self.next_name(),
                    AUTHENTICATION_REQUESTED | SET_ENCRYPTION => if let Some(link) = &self.link {
                        self.out.command(DISCONNECT, &[link.handle as u8, (link.handle >> 8) as u8, 0x13]);
                    },
                    _ => {}
                }
            }
            // A completion after the host cancelled the inquiry (it overran) belongs to no running inquiry.
            0x01 => if let Some(scan) = self.scan.as_mut().filter(|scan| scan.inquiring) {
                scan.inquiring = false;
                if scan.found.is_empty() && scan.rounds < 2 { self.inquire(); return }
                self.inquiry_done();
            },
            0x02 | 0x22 => {
                let n = r.u8() as usize;
                let wide = usize::from(code == 0x02);
                if p.len() < 1 + n * 14 { return }
                for i in 0..n {
                    let addr: Addr = p[1 + 6 * i..7 + 6 * i].try_into().unwrap();
                    let class = &p[1 + n * (8 + wide) + 3 * i..];
                    let rssi = if code == 0x22 { p[1 + n * 13 + i] as i8 } else { 0 };
                    self.found(addr, u32::from(class[0]) | u32::from(class[1]) << 8 | u32::from(class[2]) << 16, rssi, None, false);
                }
            }
            0x2f => {
                r.u8();
                let addr = r.addr(); r.skip(2);
                let class = u32::from(r.u8()) | u32::from(r.u8()) << 8 | u32::from(r.u8()) << 16;
                r.skip(2);
                let rssi = r.u8() as i8;
                let (mut eir, mut name, mut sink) = (r.0, None, false);
                while eir.len() >= 2 && eir[0] != 0 {
                    let len = eir[0] as usize;
                    let Some(field) = eir.get(1..1 + len) else { break };
                    if matches!(field[0], 0x08 | 0x09) { name = Some(String::from_utf8_lossy(&field[1..]).into_owned()); }
                    sink |= sdp::eir_lists(field[0], &field[1..], sdp::UUID_AUDIO_SINK);
                    eir = &eir[1 + len..];
                }
                self.found(addr, class, rssi, name, sink);
            }
            0x07 => {
                let (status, addr) = (r.u8(), r.addr());
                if status == 0 {
                    let end = r.0.iter().position(|&b| b == 0).unwrap_or(r.0.len());
                    let name = String::from_utf8_lossy(&r.0[..end]).into_owned();
                    let known = self.scan.as_ref().and_then(|s| s.found.iter().find(|f| f.0 == addr).map(|f| (f.1, f.2, f.4)));
                    if let Some((class, rssi, sink)) = known { self.found(addr, class, rssi, Some(name), sink); }
                }
                self.next_name();
            }
            0x04 => {
                let addr = r.addr(); r.skip(3);
                let acl = r.u8() == 1;
                let allowed = self.settings.bench_peer.map_or_else(|| self.key(addr).is_some(), |peer| peer == addr);
                if acl && self.link.is_none() && self.connecting.is_none() && allowed {
                    self.stop_scan();
                    self.connecting = Some((addr, true, false));
                    self.connect_by = Some(self.out.now + PAGE_LIMIT_MS);
                    self.out.events.push_back(Event::Connecting(addr));
                    // Stay the peripheral of the link the headphones created.
                    let mut p = addr.to_vec(); p.push(0x01);
                    self.out.command(ACCEPT_CONNECTION, &p);
                } else {
                    // Limited resources.
                    let mut p = addr.to_vec(); p.push(0x0d);
                    self.out.command(REJECT_CONNECTION, &p);
                }
            }
            0x03 => {
                let (status, handle, addr) = (r.u8(), r.u16() & 0x0fff, r.addr());
                let Some((_, incoming, check)) = self.connecting.take_if(|c| c.0 == addr) else {
                    // A link the host gave up on before it completed: close it.
                    if status == 0 && self.link.as_ref().is_none_or(|l| l.handle != handle) { self.out.command(DISCONNECT, &[handle as u8, (handle >> 8) as u8, 0x13]); }
                    return
                };
                let cancelled = std::mem::take(&mut self.cancel_asked) && !incoming;
                if status != 0 { self.out.events.push_back(Event::ConnectFailed { addr, status }); return }
                if cancelled {
                    // The page won the race with its cancel: the link nobody wants any more is closed.
                    self.out.command(DISCONNECT, &[handle as u8, (handle >> 8) as u8, 0x13]);
                    self.out.events.push_back(Event::ConnectFailed { addr, status: 0x02 });
                    return
                }
                let mut link = Link::new(addr, handle, incoming, self.out.now);
                link.had_key = self.key(addr).is_some();
                if incoming { link.auth_at = Some(self.out.now + 500); }
                else if check { self.diagnostics.sink_checks.0 += 1; link.check_sink(&mut self.out); }
                else { link.authenticate(&mut self.out); }
                self.link = Some(link);
            }
            0x05 => {
                let (_status, handle, reason) = (r.u8(), r.u16() & 0x0fff, r.u8());
                let Some(link) = self.link.take_if(|l| l.handle == handle) else { return };
                self.closed(link, reason);
            }
            0x06 => {
                let (status, handle) = (r.u8(), r.u16() & 0x0fff);
                let Some(link) = self.link.as_mut().filter(|l| l.handle == handle) else { return };
                link.auth_sent = false;
                match status {
                    0 => { link.reach(&mut self.out, Step::Encryption); self.out.command(SET_ENCRYPTION, &[handle as u8, (handle >> 8) as u8, 1]) }
                    // The headphones started the same procedure; theirs completes it.
                    0x22 | 0x23 | 0x2a => link.auth_at = Some(self.out.now + 400),
                    _ => {
                        // Authentication failure or missing key: the stored key no longer matches.
                        let addr = link.addr;
                        if link.had_key && matches!(status, 0x05 | 0x06) {
                            self.settings.keys.retain(|k| k.addr != addr);
                            self.out.events.push_back(Event::KeyRejected(addr));
                        }
                        self.out.command(DISCONNECT, &[handle as u8, (handle >> 8) as u8, 0x05]);
                    }
                }
            }
            0x08 => {
                let (status, handle, enabled) = (r.u8(), r.u16() & 0x0fff, r.u8());
                let Some(link) = self.link.as_mut().filter(|l| l.handle == handle) else { return };
                if status == 0 && enabled != 0 { link.secured(&mut self.out); } else if !link.secured {
                    self.out.command(DISCONNECT, &[handle as u8, (handle >> 8) as u8, 0x05]);
                }
            }
            0x13 => {
                let n = r.u8();
                for _ in 0..n { r.u16(); self.acl_credits = self.acl_credits.saturating_add(r.u16()).min(self.acl_total); }
            }
            0x16 if self.settings.bench_peer.is_some() => self.out.command(PIN_NEGATIVE_REPLY, &r.addr()),
            0x16 => { let mut p = r.addr().to_vec(); p.push(4); p.extend_from_slice(b"0000"); p.extend_from_slice(&[0; 12]); self.out.command(PIN_REPLY, &p); }
            0x17 => {
                let addr = r.addr();
                if let Some(link) = self.link.as_mut().filter(|l| l.addr == addr) { link.reach(&mut self.out, Step::Authentication); }
                let mut p = addr.to_vec();
                match self.key(addr) {
                    Some(key) => { p.extend_from_slice(&key.key); self.out.command(LINK_KEY_REPLY, &p); }
                    None => self.out.command(LINK_KEY_NEGATIVE, &p),
                }
            }
            0x18 => {
                let addr = r.addr();
                if self.settings.bench_peer.is_some_and(|peer| peer != addr) { return }
                let mut key = [0u8; 16];
                for b in &mut key { *b = r.u8(); }
                let key = Key { addr, key, kind: r.u8() };
                self.settings.keys.retain(|k| k.addr != addr);
                self.settings.keys.push(key.clone());
                self.out.events.push_back(Event::LinkKey(key));
            }
            // IO capability request: no input, no output; no out-of-band data; general bonding without MITM protection.
            0x31 => {
                let addr = r.addr();
                let mut p = addr.to_vec();
                if self.settings.bench_peer.is_some_and(|peer| peer != addr) {
                    p.push(0x18);
                    self.out.command(IO_CAPABILITY_NEGATIVE_REPLY, &p);
                } else {
                    p.extend_from_slice(&[0x03, 0x00, 0x04]);
                    self.out.command(IO_CAPABILITY_REPLY, &p);
                }
            }
            // Numeric comparison with a device that cannot show or confirm a number: accept.
            0x33 => {
                let addr = r.addr();
                let opcode = if self.settings.bench_peer.is_some_and(|peer| peer != addr) {
                    USER_CONFIRMATION_NEGATIVE_REPLY
                } else {
                    USER_CONFIRMATION_REPLY
                };
                self.out.command(opcode, &addr);
            }
            // No keyboard and no out-of-band data: refuse at once rather than let the pairing stall.
            0x34 => self.out.command(USER_PASSKEY_NEGATIVE, &r.addr()),
            0x35 => self.out.command(REMOTE_OOB_NEGATIVE, &r.addr()),
            0x10 => self.fail(format!("Bluetooth controller hardware error {:#04x}", r.u8())),
            _ => {}
        }
    }
}
