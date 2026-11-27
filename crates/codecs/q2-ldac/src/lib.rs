//! LDAC encoder for an A2DP source.
//!
//! A safe wrapper over the `ldacBT` API of AOSP libldac (Apache-2.0, vendored
//! under `vendor/libldac`; see `README.q2.md`). This is the only Rust file in
//! the crate with `unsafe`.
//!
//! libldac sizes packets for a 2-DH5 baseband packet whatever MTU it is
//! given: it rejects an MTU below 679 and never puts more than 660 bytes of
//! LDAC frames in a packet. Every quality therefore produces whole packets of
//! 660 bytes: 2 frames at `High`, 3 at `Standard`, 6 at `Mobile`.

use std::ffi::{c_int, c_uchar, c_void};
use std::ptr::NonNull;
use std::sync::RwLock;

/// The A2DP codec-specific information for LDAC, from `ldacBT.h`
/// ("LDAC Specification of Bluetooth A2DP Rev.2.0.1").
///
/// The media codec capability is 8 bytes after the media-type and codec-type
/// (0xFF, vendor specific) bytes: vendor id (4, little-endian), codec id
/// (2, little-endian), a sampling-frequency bit mask, a channel-mode bit mask.
pub mod a2dp {
    /// Sony's Bluetooth company id. On the wire: 2D 01 00 00.
    pub const VENDOR_ID: u32 = 0x0000_012D;
    /// Vendor-specific codec id for LDAC. On the wire: AA 00.
    pub const CODEC_ID: u16 = 0x00AA;
    /// Sampling-frequency bits (capability octet 6; bits 7 and 6 are zero).
    pub const FREQ_44100: u8 = 0x20;
    pub const FREQ_48000: u8 = 0x10;
    pub const FREQ_88200: u8 = 0x08;
    pub const FREQ_96000: u8 = 0x04;
    /// Defined by the specification; this encoder rejects them.
    pub const FREQ_176400: u8 = 0x02;
    pub const FREQ_192000: u8 = 0x01;
    /// Channel-mode bits (capability octet 7; bits 7 to 3 are zero).
    pub const CHANNEL_MONO: u8 = 0x04;
    pub const CHANNEL_DUAL: u8 = 0x02;
    pub const CHANNEL_STEREO: u8 = 0x01;
    /// The smallest media MTU an LDAC stream may use.
    pub const MTU_MIN: usize = 679;
    /// Most LDAC frames in one media packet (4-bit count in the payload header).
    pub const FRAMES_MAX: u8 = 15;
}

/// PCM frames `ldacBT_encode` takes per call (`LDACBT_ENC_LSU`).
const LSU: usize = 128;
/// Largest output of one `ldacBT_encode` call (`LDACBT_MAX_NBYTES`).
const MAX_NBYTES: usize = 1024;
/// `LDACBT_SMPL_FMT_S16`.
const SMPL_FMT_S16: c_int = 2;
/// `LDACBT_CHANNEL_MODE_STEREO`.
const CHANNEL_MODE_STEREO: c_int = 1;
/// Bytes libldac reserves for the RTP header, the media payload header and
/// content protection (`LDACBT_TX_HEADER_SIZE`).
const TX_HEADER_SIZE: usize = 18;

/// libldac's one piece of writable file-scope state: `ldaclib_init_encode`
/// stores pointers to the MDCT tables for the frame size in four global
/// arrays (`gaa_fwin_ldac` and friends, `set_mdct_table_ldac`), and encoding
/// reads them. The stored values are the same on every call. Initialisation
/// holds this lock for writing and encoding for reading, so encoders on
/// different threads never race on those arrays.
static MDCT_TABLES: RwLock<()> = RwLock::new(());

/// `struct _st_ldacbt_handle`; only ever used behind a pointer.
#[repr(C)]
struct Handle {
    _opaque: [u8; 0],
}

// SAFETY: these declarations match the prototypes in vendor/libldac/inc/ldacBT.h
// (`HANDLE_LDAC_BT` is a pointer to the opaque handle; `LDACBT_SMPL_FMT_T` is
// an enum passed as int). build.rs links the library that defines them.
unsafe extern "C" {
    fn ldacBT_get_handle() -> *mut Handle;
    fn ldacBT_free_handle(h: *mut Handle);
    fn ldacBT_init_handle_encode(
        h: *mut Handle,
        mtu: c_int,
        eqmid: c_int,
        cm: c_int,
        fmt: c_int,
        sf: c_int,
    ) -> c_int;
    fn ldacBT_encode(
        h: *mut Handle,
        p_pcm: *mut c_void,
        pcm_used: *mut c_int,
        p_stream: *mut c_uchar,
        stream_sz: *mut c_int,
        frame_num: *mut c_int,
    ) -> c_int;
    fn ldacBT_get_bitrate(h: *mut Handle) -> c_int;
    fn ldacBT_set_eqmid(h: *mut Handle, eqmid: c_int) -> c_int;
    fn ldacBT_get_eqmid(h: *mut Handle) -> c_int;
    fn ldacBT_alter_eqmid_priority(h: *mut Handle, priority: c_int) -> c_int;
    fn ldacBT_get_error_code(h: *mut Handle) -> c_int;
}

/// libldac's encode quality modes. The bit rates are for 48 or 96 kHz and
/// for 44.1 or 88.2 kHz.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Quality {
    /// 990 / 909 kbit/s.
    High,
    /// 660 / 606 kbit/s.
    Standard,
    /// 330 / 303 kbit/s.
    Mobile,
}

impl Quality {
    /// `LDACBT_EQMID_HQ`, `_SQ`, `_MQ`.
    fn eqmid(self) -> c_int {
        match self {
            Quality::High => 0,
            Quality::Standard => 1,
            Quality::Mobile => 2,
        }
    }
}

/// The bit-rate levels an adaptive encoder moves through, best first: libldac's
/// `tbl_ldacbt_eqmid_property` from HQ down to its limit for
/// `ldacBT_alter_eqmid_priority` (MQ). kbit/s at 48 and 44.1 kHz.
pub const LEVELS: [(&str, u32, u32); 5] = [("HQ", 990, 909), ("SQ", 660, 606), ("Q0", 492, 452), ("Q1", 396, 363), ("MQ", 330, 303)];
/// libldac's EQMID for each of [`LEVELS`] (`LDACBT_EQMID_HQ`, `_SQ`, `_Q0`, `_Q1`, `_MQ`).
const LEVEL_EQMID: [c_int; 5] = [0, 1, 3, 4, 2];

impl Quality {
    /// This quality's place in [`LEVELS`].
    pub fn level(self) -> u8 {
        match self {
            Quality::High => 0,
            Quality::Standard => 1,
            Quality::Mobile => 4,
        }
    }
}

/// A call that reads or changes the encode quality mode.
enum Eqmid {
    Get,
    Set(c_int),
    /// `LDACBT_EQMID_INC_CONNECTION` (-1): one step to a lower bit rate.
    Lower,
}

/// One media payload ready for an A2DP packet.
pub struct Packet {
    /// LDAC frames, WITHOUT the one-byte A2DP LDAC media payload header.
    pub data: Vec<u8>,
    /// Number of LDAC frames in `data` (1 to 15); the low four bits of the
    /// media payload header.
    pub frames: u8,
}

/// An LDAC encoder for 16-bit stereo PCM.
pub struct Encoder {
    handle: NonNull<Handle>,
    frame_samples: usize,
    payload_max: usize,
    out: Box<[u8; MAX_NBYTES]>,
}

// SAFETY: the handle is a malloc'd structure reachable only through this
// `Encoder`, which is not `Sync`, so one thread at a time uses it. libldac
// keeps no thread-local state. Its only writable file-scope data are the
// MDCT table pointers, guarded by `MDCT_TABLES`.
unsafe impl Send for Encoder {}

impl Encoder {
    /// `rate` is 44100, 48000, 88200 or 96000; stereo; `mtu` is the L2CAP
    /// media MTU (bytes available for RTP header + payload, e.g. 679 or 895).
    ///
    /// libldac rejects an MTU below 679 and gains nothing from a larger one:
    /// a payload is at most 660 bytes.
    pub fn new(rate: u32, quality: Quality, mtu: usize) -> Result<Encoder, String> {
        let frame_samples = match rate {
            44_100 | 48_000 => LSU,
            88_200 | 96_000 => 2 * LSU,
            _ => return Err(format!("ldac: unsupported sample rate {rate}")),
        };
        if mtu < a2dp::MTU_MIN {
            return Err(format!("ldac: mtu {mtu} is below {}", a2dp::MTU_MIN));
        }
        // Larger values change nothing in libldac; clamping keeps the cast exact.
        let mtu_c = mtu.min(0xFFFF) as c_int;

        // SAFETY: no preconditions; returns null on allocation failure.
        let raw = unsafe { ldacBT_get_handle() };
        let handle = NonNull::new(raw).ok_or("ldac: out of memory")?;
        // From here the handle is owned by `enc`, whose Drop frees it.
        let enc = Encoder {
            handle,
            frame_samples,
            payload_max: a2dp::MTU_MIN - TX_HEADER_SIZE,
            out: Box::new([0; MAX_NBYTES]),
        };
        let guard = MDCT_TABLES.write().unwrap_or_else(|e| e.into_inner());
        // SAFETY: the handle is live and came from ldacBT_get_handle; the
        // remaining arguments are plain integers that libldac validates.
        // The write lock excludes every other reader and writer of the
        // global MDCT table pointers this call stores.
        let rc = unsafe {
            ldacBT_init_handle_encode(
                enc.handle.as_ptr(),
                mtu_c,
                quality.eqmid(),
                CHANNEL_MODE_STEREO,
                SMPL_FMT_S16,
                rate as c_int,
            )
        };
        drop(guard);
        if rc != 0 {
            return Err(enc.error("init"));
        }
        Ok(enc)
    }

    /// Stereo PCM frames consumed per call to `encode`: 128 at 44.1/48 kHz,
    /// 256 at 88.2/96 kHz (what ldacBT requires).
    pub fn frame_samples(&self) -> usize {
        self.frame_samples
    }

    /// Encodes exactly `frame_samples()` interleaved stereo i16 frames.
    /// Returns a packet when libldac has filled one.
    ///
    /// After an error the encoder state is undefined by libldac's contract;
    /// make a new `Encoder`.
    pub fn encode(&mut self, pcm: &[i16]) -> Result<Option<Packet>, String> {
        if pcm.len() != self.frame_samples * 2 {
            return Err(format!(
                "ldac: encode needs {} samples, got {}",
                self.frame_samples * 2,
                pcm.len()
            ));
        }
        let mut packet: Option<Packet> = None;
        let _tables = MDCT_TABLES.read().unwrap_or_else(|e| e.into_inner());
        // ldacBT_encode takes 128 frames per call at every rate and encodes
        // once it holds one LDAC frame (two calls at 88.2/96 kHz).
        for chunk in pcm.as_chunks::<{ LSU * 2 }>().0 {
            let mut used: c_int = 0;
            let mut size: c_int = 0;
            let mut frames: c_int = 0;
            // SAFETY: the handle is live and initialised for encoding.
            // `chunk` is 128 stereo frames of native-endian i16, the amount
            // ldacBT_encode reads for LDACBT_SMPL_FMT_S16 stereo; it only
            // reads through `p_pcm` (the C prototype lacks `const`). `out`
            // is LDACBT_MAX_NBYTES long, the most one call writes. The three
            // out-pointers are valid for writes for the duration of the call.
            // The read lock keeps the global MDCT table pointers unchanged.
            let rc = unsafe {
                ldacBT_encode(
                    self.handle.as_ptr(),
                    chunk.as_ptr().cast_mut().cast::<c_void>(),
                    &mut used,
                    self.out.as_mut_ptr(),
                    &mut size,
                    &mut frames,
                )
            };
            if rc != 0 {
                return Err(self.error("encode"));
            }
            if used as usize != LSU * 4 {
                return Err(format!(
                    "ldac: encoder took {used} of {} PCM bytes",
                    LSU * 4
                ));
            }
            if size == 0 {
                continue;
            }
            let size = size as usize;
            if size > self.payload_max
                || frames < 1
                || frames > c_int::from(a2dp::FRAMES_MAX)
                || packet.is_some()
            {
                return Err(format!(
                    "ldac: unexpected packet ({size} bytes, {frames} frames)"
                ));
            }
            packet = Some(Packet {
                data: self.out[..size].to_vec(),
                frames: frames as u8,
            });
        }
        Ok(packet)
    }

    /// The stream's bit rate in kbit/s as libldac reports it, LDAC frame
    /// headers included: 909, 606 or 303 at 44.1/88.2 kHz and 990, 660 or
    /// 330 at 48/96 kHz.
    pub fn bitrate(&self) -> u32 {
        // SAFETY: the handle is live and initialised for encoding; the call
        // only reads it.
        let kbps = unsafe { ldacBT_get_bitrate(self.handle.as_ptr()) };
        kbps.max(0) as u32
    }

    /// The current place in [`LEVELS`] (0 is HQ).
    pub fn level(&mut self) -> u8 {
        let eqmid = self.eqmid(Eqmid::Get);
        LEVEL_EQMID.iter().position(|&e| e == eqmid).map_or(0, |i| i as u8)
    }

    /// Moves to `level` of [`LEVELS`] (an adaptive bit rate). libldac applies it
    /// from the next LDAC frame; packets keep their 660-byte payload and the
    /// frame count in the payload header follows the frame size.
    pub fn set_level(&mut self, level: u8) -> Result<(), String> {
        let level = usize::from(level).min(LEVELS.len() - 1);
        // ldacBT_set_eqmid takes HQ, SQ and MQ; Q0 and Q1 are reached from SQ one step at a time.
        let (named, lower) = match level { 0 => (0, 0), 1 => (1, 0), 2 => (1, 1), 3 => (1, 2), _ => (2, 0) };
        if self.eqmid(Eqmid::Set(named)) != 0 {
            return Err(self.error("set_eqmid"));
        }
        for _ in 0..lower {
            if self.eqmid(Eqmid::Lower) != 0 {
                return Err(self.error("alter_eqmid_priority"));
            }
        }
        Ok(())
    }

    fn eqmid(&mut self, call: Eqmid) -> c_int {
        let h = self.handle.as_ptr();
        // SAFETY: the handle is live and initialised for encoding (`new`).
        // The three calls read or update fields of the handle only (the
        // target EQMID, frame length and frames per packet); EQMID values
        // are checked by libldac (`ldacBT_assert_eqmid`, the table bounds of
        // `ldacBT_get_altered_eqmid`). `&mut self` excludes a concurrent encode.
        unsafe {
            match call {
                Eqmid::Get => ldacBT_get_eqmid(h),
                Eqmid::Set(eqmid) => ldacBT_set_eqmid(h, eqmid),
                Eqmid::Lower => ldacBT_alter_eqmid_priority(h, -1),
            }
        }
    }

    fn error(&self, what: &str) -> String {
        // SAFETY: the handle is live; the call reads the handle and stores
        // the combined code in it.
        let code = unsafe { ldacBT_get_error_code(self.handle.as_ptr()) };
        // LDACBT_API_ERR, LDACBT_HANDLE_ERR, LDACBT_BLOCK_ERR of ldacBT.h.
        format!(
            "ldac: {what} failed (api {}, handle {}, block {})",
            (code >> 20) & 0xFFF,
            (code >> 10) & 0x3FF,
            code & 0x3FF
        )
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: the handle came from ldacBT_get_handle and is freed exactly
        // once, here. ldacBT_free_handle also releases the encoder state
        // that ldacBT_close_handle would.
        unsafe { ldacBT_free_handle(self.handle.as_ptr()) }
    }
}
