//! aptX and aptX-HD encoder for an A2DP source.
//!
//! Safe wrapper over the vendored libopenaptx 0.2.0 (LGPL-2.1-or-later,
//! `vendor/openaptx/`). This file holds all the `unsafe` of the crate.

use std::os::raw::{c_int, c_uchar};
use std::ptr::NonNull;

/// Opaque `struct aptx_context` of openaptx.h.
#[repr(C)]
struct AptxContext {
    _private: [u8; 0],
}

// SAFETY: these declarations match the prototypes in
// vendor/openaptx/openaptx.h (`size_t` is `usize` on every Rust target).
unsafe extern "C" {
    fn aptx_init(hd: c_int) -> *mut AptxContext;
    fn aptx_finish(ctx: *mut AptxContext);
    fn aptx_encode(
        ctx: *mut AptxContext,
        input: *const c_uchar,
        input_size: usize,
        output: *mut c_uchar,
        output_size: usize,
        written: *mut usize,
    ) -> usize;
}

/// PCM samples per codeword: 4 stereo frames, 2 channels.
const SAMPLES_PER_CODEWORD: usize = 2 * Encoder::FRAME_SAMPLES;
/// The C encoder reads 24-bit little-endian samples: 3 bytes each.
const INPUT_BYTES_PER_CODEWORD: usize = 3 * SAMPLES_PER_CODEWORD;
/// Codewords converted and encoded per C call; sizes the stack buffer (768 bytes).
const CHUNK_CODEWORDS: usize = 32;

/// One aptX or aptX-HD encoder stream.
pub struct Encoder {
    ctx: NonNull<AptxContext>,
    codeword_bytes: usize,
}

// SAFETY: the context is a single malloc'ed block that this value owns
// exclusively. libopenaptx keeps no global or thread-local state, and
// `aptx_encode`/`aptx_finish` only touch the context passed to them, so the
// owner may move to another thread. The type is not `Sync` (NonNull), and
// `encode` takes `&mut self`.
unsafe impl Send for Encoder {}

impl Encoder {
    /// Stereo PCM frames consumed per codeword: 4.
    pub const FRAME_SAMPLES: usize = 4;

    /// `hd` selects aptX-HD (24-bit, 6 bytes per 4 stereo samples) instead of
    /// aptX (4 bytes per 4 stereo samples).
    pub fn new(hd: bool) -> Result<Encoder, String> {
        // SAFETY: `aptx_init` takes a plain integer, allocates the context
        // with malloc and resets it, or returns NULL when malloc fails.
        let ctx = unsafe { aptx_init(c_int::from(hd)) };
        match NonNull::new(ctx) {
            Some(ctx) => Ok(Encoder {
                ctx,
                codeword_bytes: if hd { 6 } else { 4 },
            }),
            None => Err("aptx_init: out of memory".to_string()),
        }
    }

    /// Encodes interleaved stereo i16 PCM whose length is a multiple of 8
    /// samples (4 stereo frames); appends the encoded bytes to `out` and
    /// returns how many bytes were appended.
    /// (16-bit input is placed in the top bits of the codec's 24-bit input.)
    ///
    /// Panics if `pcm.len()` is not a multiple of 8.
    pub fn encode(&mut self, pcm: &[i16], out: &mut Vec<u8>) -> usize {
        assert!(
            pcm.len().is_multiple_of(SAMPLES_PER_CODEWORD),
            "aptX input must be a multiple of {SAMPLES_PER_CODEWORD} samples, got {}",
            pcm.len()
        );
        let start = out.len();
        let total = pcm.len() / SAMPLES_PER_CODEWORD * self.codeword_bytes;
        out.resize(start + total, 0);

        let mut input = [0u8; CHUNK_CODEWORDS * INPUT_BYTES_PER_CODEWORD];
        let pcm_chunks = pcm.chunks(CHUNK_CODEWORDS * SAMPLES_PER_CODEWORD);
        let out_chunks = out[start..].chunks_mut(CHUNK_CODEWORDS * self.codeword_bytes);
        for (src, dst) in pcm_chunks.zip(out_chunks) {
            let input = &mut input[..src.len() * 3];
            for (bytes, sample) in input.as_chunks_mut::<3>().0.iter_mut().zip(src) {
                // sample << 8 as 24-bit little-endian.
                let [lo, hi] = sample.to_le_bytes();
                bytes[0] = 0;
                bytes[1] = lo;
                bytes[2] = hi;
            }
            let mut written = 0usize;
            // SAFETY: `self.ctx` came from `aptx_init` and is freed only in
            // `drop`. `input` and `dst` are live, non-overlapping slices and
            // the lengths passed are their lengths; `aptx_encode` reads at
            // most `input_size` bytes, writes at most `output_size` bytes and
            // stores one `size_t` through `written`. `&mut self` rules out a
            // concurrent call on the same context.
            let consumed = unsafe {
                aptx_encode(
                    self.ctx.as_ptr(),
                    input.as_ptr(),
                    input.len(),
                    dst.as_mut_ptr(),
                    dst.len(),
                    &mut written,
                )
            };
            // `dst` holds exactly one codeword per 24 input bytes, so the C
            // loop consumes and fills both completely.
            assert!(consumed == input.len() && written == dst.len());
        }
        total
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: `self.ctx` came from `aptx_init`, has not been freed, and
        // is not used after this call. `aptx_finish` is `free`.
        unsafe { aptx_finish(self.ctx.as_ptr()) }
    }
}

#[cfg(test)]
mod tests {
    use super::Encoder;

    /// Deterministic stereo test signal using integer arithmetic only: two
    /// tones from "magic circle" oscillators (about 878 Hz and 4.8 kHz at
    /// 44.1 kHz, weighted differently per channel) plus LCG noise.
    fn signal(frames: usize) -> Vec<i16> {
        let (mut x1, mut y1) = (9000i32, 0i32);
        let (mut x2, mut y2) = (5000i32, 0i32);
        let mut lcg = 0x1234_5678u32;
        let mut pcm = Vec::with_capacity(frames * 2);
        for _ in 0..frames {
            x1 -= (y1 * 128) >> 10;
            y1 += (x1 * 128) >> 10;
            x2 -= (y2 * 700) >> 10;
            y2 += (x2 * 700) >> 10;
            let mut noise = || {
                lcg = lcg.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((lcg >> 23) as i32) - 256
            };
            let left = x1 + x2 / 2 + noise();
            let right = y1 / 2 + y2 + noise();
            pcm.push(left.clamp(-32768, 32767) as i16);
            pcm.push(right.clamp(-32768, 32767) as i16);
        }
        pcm
    }

    fn fnv1a(bytes: &[u8]) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for &b in bytes {
            hash ^= u64::from(b);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }

    fn s16le(pcm: &[i16]) -> Vec<u8> {
        pcm.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    fn encode_all(hd: bool, pcm: &[i16]) -> Vec<u8> {
        let mut enc = Encoder::new(hd).unwrap();
        let mut out = Vec::new();
        let n = enc.encode(pcm, &mut out);
        assert_eq!(n, out.len());
        out
    }

    #[test]
    fn encoder_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<Encoder>();
    }

    #[test]
    fn byte_rate() {
        let frames = 4 * 1000;
        let pcm = signal(frames);
        assert_eq!(encode_all(false, &pcm).len(), frames);
        assert_eq!(encode_all(true, &pcm).len(), frames * 3 / 2);
    }

    #[test]
    fn appends_and_returns_appended_length() {
        let pcm = signal(8);
        let mut enc = Encoder::new(false).unwrap();
        let mut out = vec![0xAA, 0xBB];
        assert_eq!(enc.encode(&pcm, &mut out), 8);
        assert_eq!(out.len(), 10);
        assert_eq!(&out[..2], &[0xAA, 0xBB]);
        assert_eq!(enc.encode(&[], &mut out), 0);
        assert_eq!(out.len(), 10);
    }

    #[test]
    #[should_panic(expected = "multiple of 8")]
    fn rejects_partial_codeword() {
        let mut enc = Encoder::new(false).unwrap();
        enc.encode(&[0i16; 6], &mut Vec::new());
    }

    /// Splitting the input across calls gives the same stream as one call.
    #[test]
    fn call_boundaries_do_not_change_the_stream() {
        let pcm = signal(4 * 301);
        for hd in [false, true] {
            let whole = encode_all(hd, &pcm);
            let mut enc = Encoder::new(hd).unwrap();
            let mut pieces = Vec::new();
            let mut rest = &pcm[..];
            let mut step = 1;
            while !rest.is_empty() {
                let take = (step * 8).min(rest.len());
                enc.encode(&rest[..take], &mut pieces);
                rest = &rest[take..];
                step = step % 70 + 1;
            }
            assert_eq!(whole, pieces);
        }
    }

    /// One second at 44.1 kHz.
    const VECTOR_FRAMES: usize = 44100;
    const VECTOR_INPUT_FNV1A: u64 = 0x2d99_d0bf_cc8a_0c00;
    const VECTOR_APTX_FNV1A: u64 = 0x02f7_a2b9_ede5_a1fe;
    const VECTOR_APTX_HD_FNV1A: u64 = 0x8edc_7038_7b19_d39c;

    #[test]
    fn encoder_vectors() {
        let pcm = signal(VECTOR_FRAMES);
        let got = (
            fnv1a(&s16le(&pcm)),
            fnv1a(&encode_all(false, &pcm)),
            fnv1a(&encode_all(true, &pcm)),
        );
        assert_eq!(
            got,
            (VECTOR_INPUT_FNV1A, VECTOR_APTX_FNV1A, VECTOR_APTX_HD_FNV1A),
            "got {:#018x} {:#018x} {:#018x}",
            got.0,
            got.1,
            got.2
        );
    }

    /// With `Q2_APTX_DUMP=<dir>` writes `signal.s16` (s16le stereo input),
    /// `signal.aptx` and `signal.aptxhd` for decoding with another
    /// implementation. Does nothing when the variable is unset.
    #[test]
    fn dump_for_external_decoder() {
        let Some(dir) = std::env::var_os("Q2_APTX_DUMP") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        std::fs::create_dir_all(&dir).unwrap();
        let pcm = signal(4 * 44100);
        std::fs::write(dir.join("signal.s16"), s16le(&pcm)).unwrap();
        std::fs::write(dir.join("signal.aptx"), encode_all(false, &pcm)).unwrap();
        std::fs::write(dir.join("signal.aptxhd"), encode_all(true, &pcm)).unwrap();
    }
}
