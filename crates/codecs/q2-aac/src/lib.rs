//! AAC-LC encoder for the Q2's A2DP source.
//!
//! The encoder is the vendored VisualOn vo-aacenc (`vendor/vo-aacenc`, Apache-2.0,
//! fixed-point C). `latm`/`unlatm` pack one access unit as the RFC 3016 LATM
//! AudioMuxElement that A2DP carries as the MPEG-2,4 AAC media payload.
//! README.q2.md records the licence decision, the vendored files and measurements.

use std::ffi::{c_int, c_long, c_short, c_ulong, c_void};
use std::ptr;

/// `VO_CODECBUFFER` (vendor/vo-aacenc/common/include/voType.h).
#[repr(C)]
struct VoCodecBuffer {
    buffer: *mut u8,
    length: c_ulong,
    time: i64,
}

/// `AACENC_PARAM` (vendor/vo-aacenc/common/include/voAAC.h).
#[repr(C)]
struct AacencParam {
    sample_rate: c_int,
    bit_rate: c_int,
    n_channels: c_short,
    adts_used: c_short,
}

/// `VO_AUDIO_CodingAAC` (voAudio.h).
const VO_AUDIO_CODING_AAC: c_int = 8;
/// `VO_PID_AAC_ENCPARAM` (voAAC.h).
const VO_PID_AAC_ENCPARAM: c_long = 0x4221_1040;
/// `VO_ERR_NONE`.
const VO_ERR_NONE: c_ulong = 0;

// SAFETY: these match the definitions in vendor/vo-aacenc/aacenc/src/aacenc.c. `VO_U32` is `unsigned long`,
// `VO_S32` is `signed long`, `VO_HANDLE` and `VO_PTR` are `void *`, the enum is `int`.
unsafe extern "C" {
    fn voAACEncInit(handle: *mut *mut c_void, coding: c_int, user_data: *mut c_void) -> c_ulong;
    fn voAACEncSetParam(handle: *mut c_void, id: c_long, data: *mut c_void) -> c_ulong;
    fn voAACEncSetInputData(handle: *mut c_void, input: *mut VoCodecBuffer) -> c_ulong;
    fn voAACEncGetOutputData(
        handle: *mut c_void,
        output: *mut VoCodecBuffer,
        info: *mut c_void,
    ) -> c_ulong;
    fn voAACEncUninit(handle: *mut c_void) -> c_ulong;
}

/// Largest access unit of a stereo AAC stream: 6144 bits per channel.
const MAX_ACCESS_UNIT: usize = 1536;
/// Output space handed to the encoder for one access unit.
const OUT_SPACE: usize = 2048;

/// MPEG-4 samplingFrequencyIndex table (ISO/IEC 14496-3 table 1.18).
const SAMPLE_RATES: [u32; 13] = [
    96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350,
];

fn rate_index(rate: u32) -> Option<u8> {
    SAMPLE_RATES
        .iter()
        .position(|&r| r == rate)
        .map(|i| i as u8)
}

/// AudioSpecificConfig: audioObjectType 2 (AAC-LC, 5 bits), samplingFrequencyIndex
/// (4 bits), channelConfiguration 2 (4 bits), GASpecificConfig 000 (1024-sample frames,
/// no core coder, no extension).
fn asc(index: u8) -> [u8; 2] {
    let bits: u16 = (2 << 11) | (u16::from(index) << 7) | (2 << 3);
    bits.to_be_bytes()
}

/// AAC-LC stereo encoder. One instance encodes one stream.
pub struct Encoder {
    handle: *mut c_void,
    rate: u32,
}

// SAFETY: the handle owns heap memory that only this value references, and the C
// library has no writable globals (its objects have no .data or .bss symbols; all
// tables are const), so the encoder may move to and be used from another thread.
unsafe impl Send for Encoder {}

impl Encoder {
    /// Stereo PCM frames per access unit: 1024.
    pub const FRAME_SAMPLES: usize = 1024;

    /// AAC-LC, stereo, `rate` 44100 or 48000, `bitrate` in bit/s (for example 256000; A2DP sinks commonly accept up to 320000).
    ///
    /// `bitrate` must be 8000 to 320000. The C library replaces a rate outside that
    /// range with its default without reporting it, so it is refused here.
    pub fn new(rate: u32, bitrate: u32) -> Result<Encoder, String> {
        if rate != 44100 && rate != 48000 {
            return Err(format!("q2-aac: unsupported sample rate {rate}"));
        }
        if !(8000..=320_000).contains(&bitrate) {
            return Err(format!("q2-aac: unsupported bitrate {bitrate}"));
        }
        let mut handle: *mut c_void = ptr::null_mut();
        // SAFETY: `handle` is a valid place for the returned pointer. A null user-data
        // pointer selects the library's own malloc/free operators (USE_DEAULT_MEM is
        // defined in build.rs).
        let status = unsafe { voAACEncInit(&mut handle, VO_AUDIO_CODING_AAC, ptr::null_mut()) };
        if status != VO_ERR_NONE || handle.is_null() {
            return Err(format!("q2-aac: voAACEncInit failed ({status:#x})"));
        }
        // From here `Drop` frees the handle on every path.
        let encoder = Encoder { handle, rate };
        let mut param = AacencParam {
            sample_rate: rate as c_int,
            bit_rate: bitrate as c_int,
            n_channels: 2,
            adts_used: 0,
        };
        // SAFETY: the handle is live; for VO_PID_AAC_ENCPARAM the library reads one
        // AACENC_PARAM through the pointer during the call and does not keep it.
        let status = unsafe {
            voAACEncSetParam(encoder.handle, VO_PID_AAC_ENCPARAM, (&raw mut param).cast())
        };
        if status != VO_ERR_NONE {
            return Err(format!("q2-aac: voAACEncSetParam failed ({status:#x})"));
        }
        Ok(encoder)
    }

    /// Encodes exactly 1024 interleaved stereo i16 frames; appends one RAW AAC access unit (no ADTS header) to `out`; returns its length.
    pub fn encode(&mut self, pcm: &[i16], out: &mut Vec<u8>) -> Result<usize, String> {
        if pcm.len() != Self::FRAME_SAMPLES * 2 {
            return Err(format!(
                "q2-aac: encode needs {} samples, got {}",
                Self::FRAME_SAMPLES * 2,
                pcm.len()
            ));
        }
        let start = out.len();
        out.resize(start + OUT_SPACE, 0);
        let mut input = VoCodecBuffer {
            // The library only reads the input; its buffer type is not const.
            buffer: pcm.as_ptr().cast::<u8>().cast_mut(),
            length: size_of_val(pcm) as c_ulong,
            time: 0,
        };
        let mut output = VoCodecBuffer {
            buffer: out[start..].as_mut_ptr(),
            length: OUT_SPACE as c_ulong,
            time: 0,
        };
        // SAFETY: the handle is live and `&mut self` excludes concurrent use. `input`
        // describes the 2048 readable samples of `pcm`, which is exactly one block, so
        // the GetOutputData call below consumes all of it and the library does not read
        // the stored pointer after this function returns (the next SetInputData replaces
        // it). `output` describes OUT_SPACE writable bytes of `out`; an access unit is at
        // most MAX_ACCESS_UNIT bytes. The output-info pointer may be null.
        let status = unsafe {
            let status = voAACEncSetInputData(self.handle, &mut input);
            if status == VO_ERR_NONE {
                voAACEncGetOutputData(self.handle, &mut output, ptr::null_mut())
            } else {
                status
            }
        };
        let length = output.length as usize;
        if status != VO_ERR_NONE || length == 0 || length > MAX_ACCESS_UNIT {
            out.truncate(start);
            return Err(format!(
                "q2-aac: encode failed ({status:#x}, {length} bytes)"
            ));
        }
        out.truncate(start + length);
        Ok(length)
    }

    /// The 2-byte MPEG-4 AudioSpecificConfig for this stream (AAC-LC, the rate's index, 2 channels).
    pub fn audio_specific_config(&self) -> [u8; 2] {
        asc(if self.rate == 48000 { 3 } else { 4 })
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: the handle came from voAACEncInit and is freed exactly once, here.
        unsafe {
            voAACEncUninit(self.handle);
        }
    }
}

/// MSB-first bit writer.
struct BitWriter {
    bytes: Vec<u8>,
    /// Bits used in the last byte, 0 when byte aligned.
    used: u32,
}

impl BitWriter {
    fn put(&mut self, value: u32, bits: u32) {
        for i in (0..bits).rev() {
            if self.used == 0 {
                self.bytes.push(0);
            }
            let bit = ((value >> i) & 1) as u8;
            *self.bytes.last_mut().expect("a byte was pushed") |= bit << (7 - self.used);
            self.used = (self.used + 1) % 8;
        }
    }
}

/// Wraps one raw access unit as an RFC 3016 LATM AudioMuxElement with muxConfigPresent=1 (StreamMuxConfig carrying the
/// AudioSpecificConfig, one program, one layer, allStreamsSameTimeFraming=1, frameLengthType 0, then PayloadLengthInfo
/// and the payload), which is the A2DP MPEG-2,4 AAC media payload format (A2DP 1.3 section 4.5; RFC 3016).
///
/// The element is 7 bytes longer than the access unit, plus one byte per full 255 bytes of it.
///
/// Panics if `rate` has no MPEG-4 sampling frequency index (`Encoder` only produces
/// 44100 and 48000).
pub fn latm(access_unit: &[u8], rate: u32) -> Vec<u8> {
    let index = rate_index(rate).expect("q2-aac: latm: rate has no sampling frequency index");
    let config = asc(index);
    let mut w = BitWriter {
        bytes: Vec::with_capacity(access_unit.len() + access_unit.len() / 255 + 7),
        used: 0,
    };
    w.put(0, 1); // useSameStreamMux
    // StreamMuxConfig
    w.put(0, 1); // audioMuxVersion
    w.put(1, 1); // allStreamsSameTimeFraming
    w.put(0, 6); // numSubFrames: one PayloadMux per element
    w.put(0, 4); // numProgram: one program
    w.put(0, 3); // numLayer: one layer
    w.put(u32::from(u16::from_be_bytes(config)), 16); // AudioSpecificConfig
    w.put(0, 3); // frameLengthType
    w.put(0xFF, 8); // latmBufferFullness
    w.put(0, 1); // otherDataPresent
    w.put(0, 1); // crcCheckPresent
    // PayloadLengthInfo: 255 for each full 255 bytes, then the remainder (0 to 254).
    let mut left = access_unit.len();
    while left >= 255 {
        w.put(255, 8);
        left -= 255;
    }
    w.put(left as u32, 8);
    // PayloadMux, then zero bits to the byte boundary.
    for &byte in access_unit {
        w.put(u32::from(byte), 8);
    }
    w.bytes
}

/// MSB-first bit reader.
struct BitReader<'a> {
    bytes: &'a [u8],
    position: usize,
}

impl BitReader<'_> {
    fn get(&mut self, bits: u32) -> Option<u32> {
        let mut value = 0;
        for _ in 0..bits {
            let byte = *self.bytes.get(self.position / 8)?;
            value = (value << 1) | u32::from((byte >> (7 - self.position % 8)) & 1);
            self.position += 1;
        }
        Some(value)
    }
}

/// Strict inverse of `latm` for tests: returns the raw access unit.
///
/// `None` for anything `latm` would not have produced, including an element with
/// trailing bytes or non-zero padding.
pub fn unlatm(element: &[u8]) -> Option<Vec<u8>> {
    let mut r = BitReader {
        bytes: element,
        position: 0,
    };
    let fixed = r.get(16)?; // useSameStreamMux .. numLayer
    let config = r.get(16)?;
    let tail = r.get(13)?; // frameLengthType .. crcCheckPresent
    if fixed != 0x2000 || tail != 0x3FC {
        return None;
    }
    let index = (config >> 7) & 0xF;
    let rate = *SAMPLE_RATES.get(index as usize)?;
    let mut length = 0usize;
    loop {
        let byte = r.get(8)?;
        length += byte as usize;
        if byte != 255 {
            break;
        }
    }
    let mut access_unit = Vec::with_capacity(length);
    for _ in 0..length {
        access_unit.push(r.get(8)? as u8);
    }
    // Rebuilding checks the AudioSpecificConfig, the padding and the total length.
    (latm(&access_unit, rate) == element).then_some(access_unit)
}
