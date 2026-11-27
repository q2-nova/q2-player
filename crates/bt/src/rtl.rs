//! Realtek controller patch ("epatch" version 1) and configuration blobs:
//! the format `rtk_hciattach` and Linux's btrtl download over HCI.

/// Device baud-rate codes of the vendor command (btrtl's table).
pub fn baud_code(baud: u32) -> Option<u32> {
    Some(match baud { 115_200 => 0x0252_c014, 230_400 => 0x0252_c00a, 921_600 => 0x05f7_5004, 1_000_000 => 0x0000_5004,
        1_500_000 => 0x0492_8002, 2_000_000 => 0x0000_5002, 3_000_000 => 0x0492_8001, 4_000_000 => 0x0000_5001, _ => return None })
}
pub fn baud_of(code: u32) -> Option<u32> {
    [115_200, 230_400, 921_600, 1_000_000, 1_500_000, 2_000_000, 3_000_000, 4_000_000].into_iter().find(|&b| baud_code(b) == Some(code))
}

#[derive(Debug, PartialEq, Eq)]
pub struct Patch { pub fw_version: u32, pub project_id: Option<u8>, pub data: Vec<u8> }

fn u16le(b: &[u8], at: usize) -> Option<usize> { Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?) as usize) }
fn u32le(b: &[u8], at: usize) -> Option<u32> { Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?)) }

/// Selects the patch for a ROM version from a "Realtech" firmware file.
pub fn select_patch(fw: &[u8], rom_version: u8) -> Result<Patch, &'static str> {
    if fw.get(..8) != Some(b"Realtech") { return Err("not a Realtek patch file"); }
    if fw.len() < 18 || fw[fw.len() - 4..] != [0x51, 0x04, 0xfd, 0x77] { return Err("patch file has no extension signature"); }
    // Extension records precede the signature, read backwards: opcode, length, data.
    let mut project_id = None;
    let mut at = fw.len() - 4;
    while at >= 16 {
        let (opcode, len) = (fw[at - 1], fw[at - 2] as usize);
        if opcode == 0xff { break }
        if at < 2 + len { return Err("patch extension is truncated") }
        if opcode == 0 && len == 1 { project_id = Some(fw[at - 3]); }
        at -= 2 + len;
    }
    let fw_version = u32le(fw, 8).ok_or("patch header is truncated")?;
    let count = u16le(fw, 12).ok_or("patch header is truncated")?;
    for i in 0..count {
        let chip = u16le(fw, 14 + 2 * i).ok_or("patch table is truncated")?;
        if chip != rom_version as usize + 1 { continue }
        let len = u16le(fw, 14 + 2 * count + 2 * i).ok_or("patch table is truncated")?;
        let offset = u32le(fw, 14 + 4 * count + 4 * i).ok_or("patch table is truncated")? as usize;
        let mut data = fw.get(offset..offset + len).ok_or("patch data is truncated")?.to_vec();
        if len < 4 { return Err("patch is too short") }
        // The last four bytes of the patch carry the file's version.
        data[len - 4..].copy_from_slice(&fw_version.to_le_bytes());
        return Ok(Patch { fw_version, project_id, data });
    }
    Err("no patch for this ROM version")
}

#[derive(Debug, PartialEq, Eq)]
pub struct Uart { pub baud_code: u32, pub flow: bool }

fn entries(config: &[u8]) -> Result<Vec<(u16, std::ops::Range<usize>)>, &'static str> {
    if config.get(..4) != Some(&[0x55, 0xab, 0x23, 0x87]) { return Err("not a Realtek configuration file"); }
    if config.len() < 6 || u16le(config, 4) != Some(config.len() - 6) { return Err("configuration length does not match") }
    let (mut at, mut out) = (6, Vec::new());
    while at < config.len() {
        let (offset, len) = (u16le(config, at).ok_or("configuration entry is truncated")? as u16, *config.get(at + 2).ok_or("configuration entry is truncated")? as usize);
        if at + 3 + len > config.len() { return Err("configuration entry is truncated") }
        out.push((offset, at + 3..at + 3 + len));
        at += 3 + len;
    }
    Ok(out)
}

/// The UART entry (offset 0x000c) of a configuration file.
pub fn uart(config: &[u8]) -> Result<Option<Uart>, &'static str> {
    Ok(entries(config)?.into_iter().find(|(offset, range)| *offset == 0x000c && range.len() >= 13).map(|(_, range)| {
        let data = &config[range];
        Uart { baud_code: u32::from_le_bytes(data[..4].try_into().unwrap()), flow: data[12] & 4 != 0 }
    }))
}

/// A copy of `config` whose UART entry asks for `baud` and `flow`. The
/// controller applies it when the patch starts, so it must match the host.
pub fn with_uart(config: &[u8], baud: u32, flow: bool) -> Result<Vec<u8>, &'static str> {
    let code = baud_code(baud).ok_or("no Realtek code for this baud rate")?;
    let (_, range) = entries(config)?.into_iter().find(|(offset, range)| *offset == 0x000c && range.len() >= 13)
        .ok_or("configuration has no UART entry")?;
    let mut out = config.to_vec();
    out[range.start..range.start + 4].copy_from_slice(&code.to_le_bytes());
    out[range.start + 12] = out[range.start + 12] & !4 | if flow { 4 } else { 0 };
    Ok(out)
}

/// The vendor download command's parameters for each fragment, in order.
pub fn fragments(image: &[u8]) -> impl Iterator<Item = Vec<u8>> + '_ {
    let count = image.len().div_ceil(252);
    image.chunks(252).enumerate().map(move |(i, chunk)| {
        let mut index = if i <= 0x7f { i as u8 } else { ((i - 0x80) % 0x7f) as u8 + 1 };
        if i + 1 == count { index |= 0x80; }
        let mut out = Vec::with_capacity(1 + chunk.len());
        out.push(index); out.extend_from_slice(chunk);
        out
    })
}

#[cfg(any(test, feature = "fake"))]
pub mod fixture {
    /// A generated patch file with one patch per ROM version in `roms`.
    pub fn firmware(roms: &[u8], patch_len: usize) -> Vec<u8> {
        let n = roms.len();
        let mut fw = b"Realtech".to_vec();
        fw.extend_from_slice(&0xd5c8_1143u32.to_le_bytes());
        fw.extend_from_slice(&(n as u16).to_le_bytes());
        for rom in roms { fw.extend_from_slice(&(*rom as u16 + 1).to_le_bytes()); }
        for _ in roms { fw.extend_from_slice(&(patch_len as u16).to_le_bytes()); }
        let base = 14 + 8 * n;
        for i in 0..n { fw.extend_from_slice(&((base + i * patch_len) as u32).to_le_bytes()); }
        for (i, rom) in roms.iter().enumerate() { fw.extend((0..patch_len).map(|b| (b as u8).wrapping_mul(31).wrapping_add(*rom).wrapping_add(i as u8))); }
        fw.extend_from_slice(&[0xff, 0x13, 0x01, 0x00, 0x51, 0x04, 0xfd, 0x77]);
        fw
    }
    /// A configuration with the stock file's layout: a 16-byte UART entry and two one-byte entries.
    pub fn config() -> Vec<u8> {
        let mut c = vec![0x55, 0xab, 0x23, 0x87, 0x1b, 0x00, 0x0c, 0x00, 0x10, 0x01, 0x80, 0x92, 0x04];
        c.extend_from_slice(&[0x50, 0xc5, 0xea, 0x19, 0xe1, 0x1b, 0xfd, 0xaf, 0x5f, 0x01, 0xa4, 0x0b]);
        c.extend_from_slice(&[0x8d, 0x00, 0x01, 0xfa, 0x8f, 0x00, 0x01, 0xbf]);
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_is_selected_by_rom_version_and_stamped_with_the_file_version() {
        let fw = fixture::firmware(&[0, 2], 300);
        let patch = select_patch(&fw, 2).unwrap();
        assert_eq!((patch.fw_version, patch.project_id, patch.data.len()), (0xd5c8_1143, Some(0x13), 300));
        assert_eq!(patch.data[296..], 0xd5c8_1143u32.to_le_bytes());
        assert_eq!(patch.data[0], 2u8.wrapping_add(1));
        assert_ne!(select_patch(&fw, 0).unwrap().data[..296], patch.data[..296]);
        assert_eq!(select_patch(&fw, 1), Err("no patch for this ROM version"));
    }

    #[test]
    fn malformed_files_are_refused_without_panicking() {
        let fw = fixture::firmware(&[0, 2], 300);
        for cut in 0..fw.len() { let _ = select_patch(&fw[..cut], 0); }
        let mut seed = 5u32;
        for _ in 0..3000 {
            let mut bad = fw.clone();
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let at = (seed >> 8) as usize % 40;
            bad[at] ^= (seed >> 3) as u8 | 1;
            let _ = select_patch(&bad, (seed >> 29) as u8);
        }
        let config = fixture::config();
        for cut in 0..config.len() { assert!(uart(&config[..cut]).is_err() || cut == config.len()); }
        assert!(select_patch(b"Realtech", 0).is_err() && uart(&[]).is_err());
    }

    #[test]
    fn stock_layout_configuration_reads_3_mbaud_with_flow_control_and_is_rewritten_in_place() {
        let config = fixture::config();
        assert_eq!(uart(&config).unwrap(), Some(Uart { baud_code: 0x0492_8001, flow: true }));
        assert_eq!(baud_of(0x0492_8001), Some(3_000_000));
        let ours = with_uart(&config, 1_500_000, false).unwrap();
        assert_eq!(uart(&ours).unwrap(), Some(Uart { baud_code: 0x0492_8002, flow: false }));
        assert_eq!(ours.len(), config.len());
        assert_eq!(ours.iter().zip(&config).filter(|(a, b)| a != b).count(), 2);
        assert!(with_uart(&config, 123, false).is_err());
    }

    #[test]
    fn fragment_indices_wrap_as_btrtl_and_mark_the_last() {
        let image = vec![0u8; 252 * 226 + 5];
        let indices: Vec<u8> = fragments(&image).map(|f| f[0]).collect();
        assert_eq!(indices.len(), 227);
        assert_eq!(&indices[..3], [0, 1, 2]);
        assert_eq!(&indices[0x7f..0x82], [0x7f, 1, 2]);
        assert_eq!(indices[226], (226 - 0x80 + 1) as u8 | 0x80);
        assert_eq!(fragments(&image).last().unwrap().len(), 6);
        assert_eq!(fragments(&image).map(|f| f.len() - 1).sum::<usize>(), image.len());
    }

    /// The stock firmware's files, when a developer points at them:
    /// `Q2_BT_FIRMWARE_DIR=.../firmware/rtlbt cargo test -p q2-bt stock_files -- --nocapture`.
    #[test]
    fn stock_files_parse_when_provided() {
        let Ok(dir) = std::env::var("Q2_BT_FIRMWARE_DIR") else { return };
        let fw = std::fs::read(format!("{dir}/rtl8733bs_fw")).unwrap();
        let config = std::fs::read(format!("{dir}/rtl8733bs_config")).unwrap();
        for rom in 0..4u8 {
            match select_patch(&fw, rom) {
                Ok(patch) => println!("rom {rom}: patch {} bytes, version {:#010x}, project {:?}, {} fragments with config",
                    patch.data.len(), patch.fw_version, patch.project_id, fragments(&[patch.data.clone(), config.clone()].concat()).count()),
                Err(error) => println!("rom {rom}: {error}"),
            }
        }
        println!("config: {:?}", uart(&config).unwrap());
        let ours = with_uart(&config, 1_500_000, false).unwrap();
        assert_eq!(uart(&ours).unwrap(), Some(Uart { baud_code: 0x0492_8002, flow: false }));
    }
}
