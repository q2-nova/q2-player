//! QuickTime chapter text tracks referenced by `tref/chap` (common M4B files).
//! Metadata/sample counts and title bytes are bounded before allocation or I/O.
use super::{bad, be32, be64};
use std::io::{self, Read, Seek, SeekFrom};
use subsonic::Chapter;
pub(super) const MAX_CHAPTERS: usize = 16384;
/// A chapter title is one list row.
pub(super) const MAX_TITLE_CHARS: usize = 256;
/// The sample's length field, a byte-order mark, and four bytes a character
/// (UTF-8's longest, and a UTF-16 surrogate pair).
const TITLE_READ: usize = 2 + 2 + 4 * MAX_TITLE_CHARS;
fn atoms(mut data: &[u8]) -> io::Result<Vec<(&[u8], &[u8])>> {
    let mut result = Vec::new();
    while !data.is_empty() {
        if data.len() < 8 {
            return Err(bad());
        }
        let mut size = be32(data, 0)? as usize;
        let mut head = 8;
        if size == 1 {
            size = usize::try_from(be64(data, 8)?).map_err(|_| bad())?;
            head = 16;
        }
        if size == 0 {
            size = data.len();
        }
        if size < head || size > data.len() {
            return Err(bad());
        }
        if result.len() >= 65536 {
            return Err(bad());
        }
        result.push((&data[4..8], &data[head..size]));
        data = &data[size..];
    }
    Ok(result)
}
fn child<'a>(data: &'a [u8], kind: &[u8]) -> io::Result<Option<&'a [u8]>> {
    Ok(atoms(data)?
        .into_iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, v)| v))
}
fn required<'a>(data: &'a [u8], kind: &[u8]) -> io::Result<&'a [u8]> {
    child(data, kind)?.ok_or_else(bad)
}
fn count(data: &[u8], width: usize, limit: usize) -> io::Result<usize> {
    let count = be32(data, 4)? as usize;
    if count > limit || data.len() < 8 + count * width {
        return Err(bad());
    }
    Ok(count)
}
pub(super) fn read(
    _blocking: &crate::rt::Blocking,
    file: &mut std::fs::File,
    moov: &[u8],
    duration: u64,
) -> io::Result<Vec<Chapter>> {
    let tracks = atoms(moov)?
        .into_iter()
        .filter(|(k, _)| *k == b"trak")
        .map(|(_, v)| v)
        .collect::<Vec<_>>();
    let mut ids = Vec::new();
    for track in &tracks {
        if let Some(tref) = child(track, b"tref")? {
            if let Some(chap) = child(tref, b"chap")? {
                if chap.len() % 4 != 0 || chap.len() > 4 * MAX_CHAPTERS {
                    return Err(bad());
                }
                for i in (0..chap.len()).step_by(4) {
                    ids.push(be32(chap, i)?);
                }
            }
        }
    }
    for track in tracks {
        let tkhd = required(track, b"tkhd")?;
        let id = be32(tkhd, if tkhd.first() == Some(&1) { 20 } else { 12 })?;
        if !ids.contains(&id) {
            continue;
        }
        let mdia = required(track, b"mdia")?;
        let mdhd = required(mdia, b"mdhd")?;
        let scale = be32(mdhd, if mdhd.first() == Some(&1) { 20 } else { 12 })? as u64;
        if scale == 0 {
            return Err(bad());
        }
        let minf = required(mdia, b"minf")?;
        let stbl = required(minf, b"stbl")?;
        let stsz = required(stbl, b"stsz")?;
        let sample_count = be32(stsz, 8)? as usize;
        if sample_count > MAX_CHAPTERS {
            return Err(bad());
        }
        let fixed_size = be32(stsz, 4)? as usize;
        let sizes = (0..sample_count)
            .map(|i| {
                if fixed_size > 0 {
                    Ok(fixed_size)
                } else {
                    be32(stsz, 12 + i * 4).map(|n| n as usize)
                }
            })
            .collect::<io::Result<Vec<_>>>()?;
        if sizes.iter().any(|&n| n < 2 || n > 65538)
            || sizes.iter().sum::<usize>() > 16 * 1024 * 1024
        {
            return Err(bad());
        }
        let stts = required(stbl, b"stts")?;
        let timing_count = count(stts, 8, MAX_CHAPTERS)?;
        let mut starts = Vec::new();
        let mut ticks = 0u64;
        for i in 0..timing_count {
            let n = be32(stts, 8 + i * 8)? as usize;
            let delta = be32(stts, 12 + i * 8)? as u64;
            if n > sample_count.saturating_sub(starts.len()) {
                return Err(bad());
            }
            for _ in 0..n {
                starts.push(ticks.saturating_mul(1000) / scale);
                ticks = ticks.checked_add(delta).ok_or_else(bad)?;
            }
        }
        if starts.len() != sample_count {
            return Err(bad());
        }
        let stsc = required(stbl, b"stsc")?;
        let map_count = count(stsc, 12, MAX_CHAPTERS)?;
        let mut maps = Vec::new();
        for i in 0..map_count {
            let first = be32(stsc, 8 + i * 12)? as usize;
            let n = be32(stsc, 12 + i * 12)? as usize;
            if first == 0
                || n == 0
                || n > sample_count
                || maps.last().is_some_and(|&(p, _)| p >= first)
            {
                return Err(bad());
            }
            maps.push((first, n));
        }
        if maps.first().is_none_or(|&(p, _)| p != 1) {
            return Err(bad());
        }
        let (offsets, wide) = if let Some(data) = child(stbl, b"stco")? {
            (data, false)
        } else {
            (required(stbl, b"co64")?, true)
        };
        let chunks = count(offsets, if wide { 8 } else { 4 }, MAX_CHAPTERS)?;
        let file_len = file.metadata()?.len();
        let mut sample = 0;
        let mut map_index = 0;
        let mut chapters = Vec::new();
        let mut bytes = Vec::new();
        for chunk in 1..=chunks {
            while map_index + 1 < maps.len() && maps[map_index + 1].0 <= chunk {
                map_index += 1;
            }
            let mut offset = if wide {
                be64(offsets, 8 + (chunk - 1) * 8)?
            } else {
                be32(offsets, 8 + (chunk - 1) * 4)? as u64
            };
            for _ in 0..maps[map_index].1 {
                let size = *sizes.get(sample).ok_or_else(bad)?;
                if offset
                    .checked_add(size as u64)
                    .is_none_or(|end| end > file_len)
                {
                    return Err(bad());
                }
                // Only the title's first row of text is read and kept: a
                // sample may be 64 KiB, and chunks may share one sample.
                let len = size.min(TITLE_READ);
                bytes.resize(len, 0);
                file.seek(SeekFrom::Start(offset))?;
                file.read_exact(&mut bytes)?;
                let declared = u16::from_be_bytes(bytes[..2].try_into().unwrap()) as usize;
                if 2 + declared > size {
                    return Err(bad());
                }
                let title_bytes = &bytes[2..(2 + declared).min(len)];
                let title: String = if title_bytes.starts_with(&[0xFE, 0xFF]) {
                    char::decode_utf16(title_bytes[2..].chunks_exact(2).map(|b| u16::from_be_bytes([b[0], b[1]])))
                        .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
                        .take(MAX_TITLE_CHARS)
                        .collect()
                } else {
                    String::from_utf8_lossy(title_bytes).chars().take(MAX_TITLE_CHARS).collect()
                };
                if starts[sample] < duration {
                    chapters.push(Chapter {
                        start_ms: starts[sample],
                        title,
                    });
                }
                sample += 1;
                offset += size as u64;
            }
        }
        if sample != sample_count {
            return Err(bad());
        }
        return Ok(chapters);
    }
    Ok(Vec::new())
}

#[derive(Default)]
pub(super) struct Metadata {
    pub title: Option<String>,
    pub artist: Option<String>,
    pub description: Option<String>,
    pub narrator: Option<String>,
    pub series: Option<String>,
    pub cover: Option<Vec<u8>>,
}
fn tag_text(bytes: &[u8], maximum: usize) -> String {
    String::from_utf8_lossy(&bytes[..bytes.len().min(maximum.saturating_mul(4))])
        .trim_matches('\0')
        .chars()
        .take(maximum)
        .collect()
}
pub(super) fn metadata(moov: &[u8]) -> io::Result<Metadata> {
    let mut result = Metadata::default();
    let Some(udta) = child(moov, b"udta")? else {
        return Ok(result);
    };
    let Some(meta) = child(udta, b"meta")? else {
        return Ok(result);
    };
    let Some(ilst) = child(meta.get(4..).ok_or_else(bad)?, b"ilst")? else {
        return Ok(result);
    };
    for (key, entry) in atoms(ilst)? {
        let Some(data) = child(entry, b"data")? else {
            continue;
        };
        let bytes = data.get(8..).ok_or_else(bad)?;
        match key {
            b"\xA9nam" => result.title = Some(tag_text(bytes, 4096)),
            b"\xA9ART" => result.artist = Some(tag_text(bytes, 4096)),
            b"ldes" => result.description = Some(tag_text(bytes, 65536)),
            b"desc" | b"\xA9cmt" if result.description.is_none() => {
                result.description = Some(tag_text(bytes, 65536))
            }
            b"----" => {
                // Explicit freeform tags only. Composer/album tags are not
                // silently reinterpreted as a narrator or series.
                let name = child(entry, b"name")?
                    .and_then(|name| name.get(4..))
                    .map(|name| tag_text(name, 64).to_ascii_lowercase());
                let text = || tag_text(bytes, 4096);
                match name.as_deref() {
                    Some("narrator" | "narrated_by" | "narratedby") => {
                        result.narrator = Some(text())
                    }
                    Some("series") => result.series = Some(text()),
                    _ => {}
                }
            }
            b"covr" if bytes.len() <= 4 * 1024 * 1024 => result.cover = Some(bytes.to_vec()),
            _ => {}
        }
    }
    Ok(result)
}
