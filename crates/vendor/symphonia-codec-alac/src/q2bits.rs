// [q2 patch] Bit reader of the restructured element decoder (README.q2.md).
//
// symphonia-core's `BitReaderLtr` keeps a 64-bit cache and returns an
// `io::Result` from every read; on the 32-bit Q2 each 64-bit shift is a
// multi-instruction sequence. This reader keeps only a bit position and
// builds a 32-bit window from four bytes at that position. Reads past the end
// of the buffer return zero bits and are found afterwards with `overrun()`:
// the caller then abandons the packet and decodes it again with upstream's
// reader, which reports the error.

pub(crate) struct Bits<'a> {
    pub(crate) buf: &'a [u8],
    /// Bits consumed.
    pub(crate) pos: usize,
}

/// The four bytes at `i`, big-endian, zero where the buffer has ended.
#[cold]
#[inline(never)]
pub(crate) fn word_at_end(buf: &[u8], i: usize) -> u32 {
    let mut w = 0;
    for k in 0..4 {
        w = (w << 8) | u32::from(i.checked_add(k).and_then(|i| buf.get(i)).copied().unwrap_or(0));
    }
    w
}

/// The 32 bits at bit position `pos`, first bit in bit 31. At least 25 are
/// stream bits; the low `pos & 7` are zero.
#[inline(always)]
pub(crate) fn window(buf: &[u8], pos: usize) -> u32 {
    let i = pos >> 3;
    let w = match buf.get(i..i.wrapping_add(4)) {
        Some(b) => u32::from_be_bytes([b[0], b[1], b[2], b[3]]),
        None => word_at_end(buf, i),
    };
    w << (pos & 7)
}

impl<'a> Bits<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self {
        Bits { buf, pos: 0 }
    }

    /// The next `n` bits (0..=32) as an unsigned number.
    #[inline(always)]
    pub(crate) fn bits(&mut self, n: u32) -> u32 {
        debug_assert!(n <= 32);
        if n <= 25 {
            let w = window(self.buf, self.pos);
            self.pos += n as usize;
            // Two shifts: n = 0 gives 0.
            (w >> 1) >> (31 - n)
        }
        else {
            let high = self.bits(16);
            let low = self.bits(n - 16);
            (high << (n - 16)) | low
        }
    }

    /// The next `n` bits (1..=32) as a two's complement number of `n` bits.
    #[inline(always)]
    pub(crate) fn signed(&mut self, n: u32) -> i32 {
        debug_assert!(n >= 1 && n <= 32);
        let v = self.bits(n);
        ((v << (32 - n)) as i32) >> (32 - n)
    }

    #[inline(always)]
    pub(crate) fn bit(&mut self) -> bool {
        self.bits(1) != 0
    }

    /// Whether a read went past the end of the buffer.
    #[inline]
    pub(crate) fn overrun(&self) -> bool {
        self.pos > self.buf.len().saturating_mul(8)
    }
}
