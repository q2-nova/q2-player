// [q2 patch] Huffman decoding of the spectral samples for the Q2
// (README.q2.md). Upstream's `requantize::read_huffman_samples` reads through
// symphonia-core's `BitReaderLtr`: a 64-bit bit cache (several instructions
// per shift on a 32-bit MIPS), a refill check and an `io::Result` on every
// read. This version reads the same bits with a 32-bit window computed from
// the bit position, and decodes with tables built from the same code lists.
//
// It returns what upstream returns for every input, including truncated and
// malformed ones: the same samples, the same rzero, and an error in the same
// cases (a read that needs more bits than the main data holds). The tests
// compare the two on random bitstreams; upstream's function is the reference
// in q2ref/requantize.rs.

use symphonia_core::errors::{Error, Result};

use super::codebooks::{self, MPEG_QUADS_TABLES, MPEG_TABLES};
use super::GranuleChannel;

use std::cmp::min;
use std::convert::TryInto;

use lazy_static::lazy_static;
use log::debug;

/// A decode table entry. Leaf: the value in bits 0..16 and the number of
/// bits the code uses at this level in bits 16..24. Jump (bit 31 set): the
/// index of the next level in bits 0..16 and its width in bits 16..24.
const JUMP: u32 = 1 << 31;

/// Bits decoded per table level, as upstream's codebooks.
const LEVEL_BITS: u32 = 8;

struct Book {
    table: Vec<u32>,
    /// Width of the first level; 0 for an empty book.
    root_bits: u32,
}

/// Fills `table[at..at + (1 << width)]` for the codes in `codes` (code bits
/// left-aligned in a u32 with the `used` bits of earlier levels shifted out,
/// remaining length, value) and appends deeper levels.
fn build_level(table: &mut Vec<u32>, at: usize, width: u32, codes: &[(u32, u32, u16)]) {
    let mut deeper: Vec<Vec<(u32, u32, u16)>> = vec![Vec::new(); 1 << width];
    for &(bits, len, value) in codes {
        let index = (bits >> (32 - width)) as usize;
        if len <= width {
            for slot in 0..(1usize << (width - len)) {
                table[at + index + slot] = (len << 16) | u32::from(value);
            }
        }
        else {
            deeper[index].push((bits << width, len - width, value));
        }
    }
    for (index, codes) in deeper.iter().enumerate() {
        if codes.is_empty() {
            continue;
        }
        let longest = codes.iter().map(|c| c.1).max().unwrap();
        let next_width = min(longest, LEVEL_BITS);
        let next_at = table.len();
        assert!(next_at < 1 << 16);
        table.resize(next_at + (1 << next_width), 0);
        table[at + index] = JUMP | (next_width << 16) | next_at as u32;
        build_level(table, next_at, next_width, codes);
    }
}

fn build_book(codes: &[u32], lens: &[u8], wrap: u16) -> Book {
    if codes.is_empty() {
        return Book { table: Vec::new(), root_bits: 0 };
    }
    let list: Vec<(u32, u32, u16)> = codes
        .iter()
        .zip(lens)
        .enumerate()
        .map(|(i, (&code, &len))| {
            let i = i as u16;
            (code << (32 - u32::from(len)), u32::from(len), ((i / wrap) << 4) | (i % wrap))
        })
        .collect();
    let longest = list.iter().map(|c| c.1).max().unwrap();
    let root_bits = min(longest, LEVEL_BITS);
    let mut table = vec![0u32; 1 << root_bits];
    build_level(&mut table, 0, root_bits, &list);
    Book { table, root_bits }
}

lazy_static! {
    static ref BOOKS: [Book; 18] = {
        let mut books: Vec<Book> = Vec::new();
        for table in MPEG_TABLES.iter() {
            books.push(build_book(table.codes, table.lens, table.wrap));
        }
        match books.try_into() {
            Ok(books) => books,
            Err(_) => unreachable!(),
        }
    };
    static ref QUADS_BOOKS: [Book; 2] = {
        let mut books: Vec<Book> = Vec::new();
        for table in MPEG_QUADS_TABLES.iter() {
            books.push(build_book(table.codes, table.lens, table.wrap));
        }
        match books.try_into() {
            Ok(books) => books,
            Err(_) => unreachable!(),
        }
    };
    /// REQUANTIZE_POW43 as bit patterns: the sign bit is ORed in.
    static ref POW43_BITS: [u32; 8207] = {
        let mut pow43 = [0u32; 8207];
        for (i, pow43) in pow43.iter_mut().enumerate() {
            *pow43 = f32::powf(i as f32, 4.0 / 3.0).to_bits();
        }
        pow43
    };
}

/// The main data from a bit position on, read most-significant bit first.
struct Bits<'a> {
    buf: &'a [u8],
    /// Position of the next bit.
    pos: usize,
    /// Number of bits in `buf`.
    end: usize,
}

fn overrun<T>() -> Result<T> {
    // What symphonia-core's bit reader returns at the end of its buffer; the
    // caller turns it into the "huffman decode overrun" decode error.
    Err(Error::IoError(std::io::Error::new(std::io::ErrorKind::Other, "unexpected end of bitstream")))
}

impl Bits<'_> {
    /// The next 32 bits from the byte holding `pos`, shifted so the bit at
    /// `pos` is bit 31: at least 25 valid bits, zeros past the end.
    #[inline(always)]
    fn window(&self) -> u32 {
        let byte = self.pos >> 3;
        let word = match self.buf.get(byte..byte + 4) {
            Some(four) => u32::from_be_bytes([four[0], four[1], four[2], four[3]]),
            None => self.window_at_end(byte),
        };
        word << (self.pos & 7)
    }

    #[cold]
    fn window_at_end(&self, byte: usize) -> u32 {
        let mut four = [0u8; 4];
        if byte < self.buf.len() {
            let rest = &self.buf[byte..];
            four[..rest.len()].copy_from_slice(rest);
        }
        u32::from_be_bytes(four)
    }

    #[inline(always)]
    fn left(&self) -> usize {
        self.end - self.pos
    }

    /// Reads `n` bits, 0 to 25.
    #[inline(always)]
    fn take(&mut self, n: u32) -> Result<u32> {
        if n as usize > self.left() {
            return overrun();
        }
        // Two shifts, so n = 0 gives 0.
        let bits = (self.window() >> 1) >> (31 - n);
        self.pos += n as usize;
        Ok(bits)
    }

    /// Decodes one code: its value and length.
    #[inline(always)]
    fn code(&mut self, book: &Book) -> Result<(u32, u32)> {
        let mut window = self.window();
        let mut width = book.root_bits;
        let mut entry = book.table[(window >> (32 - width)) as usize];
        let mut len = 0;
        while entry & JUMP != 0 {
            window <<= width;
            len += width;
            width = (entry >> 16) & 0xff;
            entry = book.table[(entry & 0xffff) as usize + (window >> (32 - width)) as usize];
        }
        len += entry >> 16;
        if len as usize > self.left() {
            return overrun();
        }
        self.pos += len as usize;
        Ok((entry & 0xffff, len))
    }
}

/// Reads the Huffman coded spectral samples for a given channel in a granule
/// from the main data, starting at bit `start`, into a provided sample
/// buffer. Returns the number of decoded samples (the starting index of the
/// rzero partition).
///
/// Note, each spectral sample is raised to the (4/3)-rd power. This is not
/// actually part of the Huffman decoding process, but, by converting the
/// integer sample to floating point here we don't need to do pointless
/// casting or use an extra buffer.
pub(super) fn read_huffman_samples(
    main_data: &[u8],
    start: usize,
    channel: &GranuleChannel,
    part3_bits: u32,
    buf: &mut [f32; 576],
) -> Result<usize> {
    // If there are no Huffman code bits, zero all samples and return immediately.
    if part3_bits == 0 {
        buf.fill(0.0);
        return Ok(0);
    }

    let pow43: &[u32; 8207] = &POW43_BITS;
    let end = 8 * main_data.len();
    let mut bs = Bits { buf: main_data, pos: min(start, end), end };

    let mut bits_read = 0;
    let mut i = 0;

    // There are two samples per big_value, therefore multiply big_values by 2 to get number of
    // samples in the big_value partition.
    let big_values_len = 2 * channel.big_values as usize;

    // There are up-to 3 regions in the big_value partition. Determine the sample index denoting the
    // end of each region (non-inclusive). Clamp to the end of the big_values partition.
    let regions: [usize; 3] = [
        min(channel.region1_start, big_values_len),
        min(channel.region2_start, big_values_len),
        min(576, big_values_len),
    ];

    // Iterate over each region in big_values.
    for (region_idx, region_end) in regions.iter().enumerate() {
        // Select the Huffman table based on the region's table select value.
        let table_select = channel.table_select[region_idx] as usize;

        // Tables 0..16 are all unique, while tables 16..24 and 24..32 each use one table but
        // differ in the number of linbits to use.
        let book = match table_select {
            0..=15 => &BOOKS[table_select],
            16..=23 => &BOOKS[16],
            24..=31 => &BOOKS[17],
            _ => unreachable!(),
        };

        let linbits = codebooks::CODEBOOK_LINBITS[table_select];

        // If the table for a region is empty, fill the region with zeros and move on to the next
        // region.
        if book.root_bits == 0 {
            while i < *region_end {
                buf[i] = 0.0;
                i += 1;
                buf[i] = 0.0;
                i += 1;
            }
            continue;
        }

        // Otherwise, read the big_values.
        while i < *region_end && bits_read < part3_bits {
            // Decode the next Huffman code.
            let (value, code_len) = bs.code(book)?;
            bits_read += code_len;

            // In the big_values partition, each Huffman code decodes to two sample, x and y. Each
            // sample can have a value between 0..=15: x in the top nibble, y in the bottom.
            let mut x = (value >> 4) as usize;
            let mut y = (value & 0xf) as usize;

            // If the first sample, x, is not 0, further process it.
            if x > 0 {
                // If x is saturated (it is at the maximum possible value), and the table specifies
                // linbits, then read linbits more bits and add it to the sample.
                if x == 15 && linbits > 0 {
                    x += bs.take(linbits)? as usize;
                    bits_read += linbits;
                }

                // The next bit is the sign bit. The value of the sample is raised to the (4/3)
                // power. Upstream multiplies the power by 1.0 or -1.0: the power with the sign
                // bit set.
                buf[i] = f32::from_bits(pow43[x] | (bs.take(1)? << 31));
                bits_read += 1;
            }
            else {
                buf[i] = 0.0;
            }

            i += 1;

            // Likewise, repeat the previous two steps for the second sample, y.
            if y > 0 {
                if y == 15 && linbits > 0 {
                    y += bs.take(linbits)? as usize;
                    bits_read += linbits;
                }

                buf[i] = f32::from_bits(pow43[y] | (bs.take(1)? << 31));
                bits_read += 1;
            }
            else {
                buf[i] = 0.0;
            }

            i += 1;
        }
    }

    let count1_book = &QUADS_BOOKS[usize::from(channel.count1table_select)];

    // Read the count1 partition.
    while i <= 572 && bits_read < part3_bits {
        // In the count1 partition, each Huffman code decodes to 4 samples: v, w, x, and y.
        // Each sample is 1-bit long (1 or 0).
        //
        // For each 1-bit sample, if it is 0, then the dequantized sample value is 0 as well. If
        // the 1-bit sample is 1, then read the sign bit (the next bit). The dequantized sample is
        // then either +/-1.0 depending on the sign bit.

        // Decode the next Huffman code.
        let (value, code_len) = bs.code(count1_book)?;
        bits_read += code_len;

        // The first 4 bits indicate if a sample if 0 or 1. Count the number of samples with a
        // non-zero value.
        let num_ones = (value & 0xf).count_ones();

        // Read the sign bits for each non-zero sample in a single read.
        let mut signs = bs.take(num_ones)?;
        bits_read += num_ones;

        // Unpack the samples.
        const ONE: u32 = 0x3f80_0000;
        if value & 0x1 != 0 {
            buf[i + 3] = f32::from_bits(ONE | ((signs & 1) << 31));
            signs >>= 1;
        }
        else {
            buf[i + 3] = 0.0;
        }

        if value & 0x2 != 0 {
            buf[i + 2] = f32::from_bits(ONE | ((signs & 1) << 31));
            signs >>= 1;
        }
        else {
            buf[i + 2] = 0.0;
        }

        if value & 0x4 != 0 {
            buf[i + 1] = f32::from_bits(ONE | ((signs & 1) << 31));
            signs >>= 1;
        }
        else {
            buf[i + 1] = 0.0;
        }

        if value & 0x8 != 0 {
            buf[i + 0] = f32::from_bits(ONE | ((signs & 1) << 31));
        }
        else {
            buf[i + 0] = 0.0;
        }

        i += 4;
    }

    // Ignore any extra "stuffing" bits.
    if bits_read < part3_bits {
        if (part3_bits - bits_read) as usize > bs.left() {
            return overrun();
        }
    }
    // Word on the street is that some encoders are poor at "stuffing" bits, resulting in part3_len
    // being ever so slightly too large. This causes the Huffman decode loop to decode the next few
    // bits as spectral samples. However, these bits are actually random data and are not real
    // samples, therefore, undo them! The caller will be reponsible for re-aligning the bitstream
    // reader. Candy Pop confirms this.
    else if bits_read > part3_bits && i > big_values_len {
        debug!("count1 overrun, malformed bitstream");
        i -= 4;
    }
    else if bits_read > part3_bits {
        debug!("big_values overrun, malformed bitstream");

        // It's possible that the big_values partition is overrun. In this case, do nothing.
    }

    // The final partition after the count1 partition is the rzero partition. Samples in this
    // partition are all 0.
    buf[i..].fill(0.0);

    Ok(i)
}

#[cfg(test)]
mod tests {
    use super::super::common::BlockType;
    use super::super::q2ref_requantize as upstream;
    use super::super::GranuleChannel;
    use super::{BOOKS, MPEG_QUADS_TABLES, MPEG_TABLES, QUADS_BOOKS};
    use symphonia_core::io::{BitReaderLtr, ReadBitsLtr};

    /// Every code list is a complete prefix code (Kraft sum 1), so every bit
    /// pattern decodes and the tables have no unfilled entry.
    #[test]
    fn q2_code_lists_are_complete() {
        let lists = MPEG_TABLES
            .iter()
            .map(|t| t.lens)
            .chain(MPEG_QUADS_TABLES.iter().map(|t| t.lens))
            .filter(|lens| !lens.is_empty());
        for lens in lists {
            let kraft: u64 = lens.iter().map(|&len| 1u64 << (32 - u32::from(len))).sum();
            assert_eq!(kraft, 1 << 32);
        }
        for book in BOOKS.iter().chain(QUADS_BOOKS.iter()) {
            assert!(book.table.iter().all(|&entry| entry != 0));
        }
    }

    /// Same samples, same rzero and an error in the same cases as upstream,
    /// on random bits: every table, linbits, region layouts, part3 lengths
    /// that end inside codes, and main data that ends early.
    #[test]
    fn q2_huffman_matches_upstream() {
        let mut rng = crate::q2simd::tests::Rng(0x1357_9bdf);
        let mut errors = 0;
        let mut overruns = 0;
        for case in 0..20_000 {
            let len = match case % 4 {
                0 => (rng.next() % 40) as usize,
                _ => 40 + (rng.next() % 1200) as usize,
            };
            // Random bits; some cases mostly zero bits or mostly one bits so
            // short codes, long codes and escapes all occur.
            let bias = rng.next() % 5;
            let data: Vec<u8> = (0..len)
                .map(|_| match bias {
                    0 => (rng.next() & rng.next() & rng.next()) as u8,
                    1 => (rng.next() | rng.next() | rng.next()) as u8,
                    _ => rng.next() as u8,
                })
                .collect();
            let start = if len == 0 { 0 } else { (rng.next() as usize) % (8 * len) };
            let big_values = (rng.next() % 289) as u16;
            let mut starts = [(rng.next() % 289) as usize * 2, (rng.next() % 289) as usize * 2];
            starts.sort();
            let channel = GranuleChannel {
                big_values,
                table_select: [(rng.next() % 32) as u8, (rng.next() % 32) as u8, (rng.next() % 32) as u8],
                region1_start: starts[0],
                region2_start: if rng.next() % 3 == 0 { 576 } else { starts[1] },
                count1table_select: (rng.next() % 2) as u8,
                block_type: BlockType::Long,
                ..Default::default()
            };
            let available = (8 * len - start) as u32;
            let part3_bits = match rng.next() % 6 {
                0 => 0,
                1 => available,
                2 => available + 1 + rng.next() % 64,
                _ => rng.next() % (available + 1),
            };

            let mut ours = [3f32; 576];
            let ours_result = super::read_huffman_samples(&data, start, &channel, part3_bits, &mut ours);

            let mut theirs = [3f32; 576];
            let theirs_result = if start >> 3 < data.len() {
                let mut bs = BitReaderLtr::new(&data[start >> 3..]);
                bs.ignore_bits((start & 7) as u32).unwrap();
                upstream::read_huffman_samples(&mut bs, &channel, part3_bits, &mut theirs)
            }
            else {
                // The caller rejects a start beyond the main data.
                continue;
            };

            match (ours_result, theirs_result) {
                (Ok(a), Ok(b)) => {
                    assert_eq!(a, b, "case {}", case);
                    assert_eq!(ours.map(f32::to_bits), theirs.map(f32::to_bits), "case {}", case);
                    if a > 2 * usize::from(big_values) {
                        overruns += 1;
                    }
                }
                (Err(_), Err(_)) => errors += 1,
                (a, b) => panic!("case {}: ours {:?}, upstream {:?}", case, a.is_ok(), b.is_ok()),
            }
        }
        // The cases cover both outcomes.
        assert!(errors > 500 && overruns > 500, "{} errors, {} count1 cases", errors, overruns);
    }
}
