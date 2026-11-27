//! A2DP media codec capabilities: what this source offers for each codec,
//! what it chooses from a sink's offer, and the configuration it sends.
//! An "element" here is the value of a Media Codec service capability:
//! media type, codec type, codec information (A2DP 1.3 section 4; the vendor
//! formats are those of Linux's and Android's A2DP implementations).
use crate::sbc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind { Sbc, Aac, AptX, AptXHd, Ldac }

impl Kind {
    /// Every codec, by nominal quality.
    pub const BY_QUALITY: [Kind; 5] = [Kind::Ldac, Kind::AptXHd, Kind::AptX, Kind::Aac, Kind::Sbc];
    /// What "Automatic" prefers: the order of nominal quality. Product feedback, 2026-11-22 (V0.133): "Auto
    /// connection mode should select the highest quality option by default ... I had to select AAC
    /// and reconnect to get AAC on my airpod pro 2s." This replaces the earlier agent choice of SBC
    /// before AAC (the vendored AAC encoder measured below SBC at bitpool 53,
    /// crates/codecs/q2-aac/README.q2.md).
    pub const AUTOMATIC: [Kind; 5] = Kind::BY_QUALITY;
    pub fn name(self) -> &'static str { match self { Kind::Sbc => "SBC", Kind::Aac => "AAC", Kind::AptX => "aptX", Kind::AptXHd => "aptX HD", Kind::Ldac => "LDAC" } }
    /// Our stream endpoint for this codec.
    pub fn seid(self) -> u8 { match self { Kind::Sbc => 1, Kind::Aac => 2, Kind::AptX => 3, Kind::AptXHd => 4, Kind::Ldac => 5 } }
    pub fn of_seid(seid: u8) -> Option<Kind> { Kind::BY_QUALITY.into_iter().find(|k| k.seid() == seid) }
}

/// A negotiated stream. Always two channels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Sbc(sbc::Params),
    /// AAC-LC in LATM, constant bit rate.
    Aac { rate: u32, bitrate: u32 },
    AptX { rate: u32 },
    AptXHd { rate: u32 },
    Ldac { rate: u32 },
}

impl Codec {
    pub fn kind(&self) -> Kind { match self { Codec::Sbc(_) => Kind::Sbc, Codec::Aac { .. } => Kind::Aac, Codec::AptX { .. } => Kind::AptX, Codec::AptXHd { .. } => Kind::AptXHd, Codec::Ldac { .. } => Kind::Ldac } }
    pub fn rate(&self) -> u32 { match *self { Codec::Sbc(p) => p.rate, Codec::Aac { rate, .. } | Codec::AptX { rate } | Codec::AptXHd { rate } | Codec::Ldac { rate } => rate } }
    /// Whether media packets carry an RTP header. aptX is the one codec sent bare.
    pub fn rtp(&self) -> bool { self.kind() != Kind::AptX }
}

const SBC: [u8; 4] = [0x33, 0xf7, 2, 53];
const APTX_VENDOR: [u8; 6] = [0x4f, 0x00, 0x00, 0x00, 0x01, 0x00];
const APTX_HD_VENDOR: [u8; 6] = [0xd7, 0x00, 0x00, 0x00, 0x24, 0x00];
const LDAC_VENDOR: [u8; 6] = [0x2d, 0x01, 0x00, 0x00, 0xaa, 0x00];
/// The most this source sends as AAC, and its default.
pub const AAC_MAX_BITRATE: u32 = 320_000;
pub const AAC_BITRATE: u32 = 256_000;

/// What this source offers for `kind`.
pub fn capabilities(kind: Kind) -> Vec<u8> {
    match kind {
        // 44.1 and 48 kHz, stereo and joint stereo, 4-16 blocks, 8 subbands, both allocations, bitpool 2-53.
        Kind::Sbc => [&[0x00, 0x00][..], &SBC].concat(),
        // MPEG-2 and MPEG-4 AAC LC, 44.1 and 48 kHz, two channels, constant rate up to 320 kbit/s.
        Kind::Aac => vec![0x00, 0x02, 0xc0, 0x01, 0x84, (AAC_MAX_BITRATE >> 16) as u8, (AAC_MAX_BITRATE >> 8) as u8, AAC_MAX_BITRATE as u8],
        // 44.1 and 48 kHz, stereo.
        Kind::AptX => [&[0x00, 0xff][..], &APTX_VENDOR, &[0x32]].concat(),
        Kind::AptXHd => [&[0x00, 0xff][..], &APTX_HD_VENDOR, &[0x32, 0, 0, 0, 0]].concat(),
        // 44.1 and 48 kHz, stereo.
        Kind::Ldac => [&[0x00, 0xff][..], &LDAC_VENDOR, &[0x30, 0x01]].concat(),
    }
}

/// The codec an element is for, with its codec information bytes.
pub fn identify(element: &[u8]) -> Option<(Kind, &[u8])> {
    if element.len() < 2 || element[0] >> 4 != 0 { return None }
    match (element[1], element.len()) {
        (0x00, 6) => Some((Kind::Sbc, &element[2..])),
        (0x02, 8) => Some((Kind::Aac, &element[2..])),
        (0xff, 9) if element[2..8] == APTX_VENDOR => Some((Kind::AptX, &element[8..])),
        (0xff, 13) if element[2..8] == APTX_HD_VENDOR => Some((Kind::AptXHd, &element[8..])),
        (0xff, 10) if element[2..8] == LDAC_VENDOR => Some((Kind::Ldac, &element[8..])),
        _ => None,
    }
}

/// The Media Codec elements of a capability list (service category 7).
pub fn elements(mut capabilities: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    while capabilities.len() >= 2 {
        let (category, len) = (capabilities[0], capabilities[1] as usize);
        let Some(value) = capabilities.get(2..2 + len) else { break };
        if category == 7 { out.push(value); }
        capabilities = &capabilities[2 + len..];
    }
    out
}

fn one_bit(value: u8) -> bool { value.count_ones() == 1 }
/// 44.1 kHz when offered, else 48 kHz: most music is 44.1 kHz and is then sent unresampled.
fn rate(bits_44: bool, bits_48: bool) -> Option<u32> { if bits_44 { Some(44100) } else if bits_48 { Some(48000) } else { None } }

fn choose_sbc(c: &[u8], exact: bool) -> Option<sbc::Params> {
    let offered = [c[0] & SBC[0], c[1] & SBC[1]];
    if exact && !(one_bit(c[0] >> 4) && one_bit(c[0] & 0xf) && one_bit(c[1] >> 4) && one_bit(c[1] >> 2 & 3) && one_bit(c[1] & 3) && offered == [c[0], c[1]]) { return None }
    let rate = rate(offered[0] & 0x20 != 0, offered[0] & 0x10 != 0)?;
    let joint = if offered[0] & 0x01 != 0 { true } else if offered[0] & 0x02 != 0 { false } else { return None };
    let blocks = [(0x10, 16), (0x20, 12), (0x40, 8), (0x80, 4)].into_iter().find(|(bit, _)| offered[1] & bit != 0)?.1;
    if offered[1] & 0x04 == 0 { return None }
    let snr = if offered[1] & 0x01 != 0 { false } else if offered[1] & 0x02 != 0 { true } else { return None };
    // The A2DP high-quality bitpools for joint stereo, within what the sink accepts.
    let preferred = if rate == 44100 { 53 } else { 51 };
    let (low, high) = (c[2].max(2), c[3].min(SBC[3]));
    if high < low { return None }
    Some(sbc::Params { rate, blocks, joint, snr, bitpool: preferred.clamp(low, high) })
}

/// The stream for a sink's offer (`exact` false: we choose within it) or for
/// a configuration the sink sets on our endpoint (`exact`: one value per field, all within ours).
pub fn choose(element: &[u8], exact: bool) -> Option<Codec> {
    let (kind, c) = identify(element)?;
    match kind {
        Kind::Sbc => choose_sbc(c, exact).map(Codec::Sbc),
        Kind::Aac => {
            // Object type: MPEG-2 or MPEG-4 AAC LC. Rates: 44.1 kHz is bit 0 of the second byte, 48 kHz bit 7 of the third.
            let (objects, r44, r48, stereo) = (c[0] & 0xc0, c[1] & 0x01 != 0, c[2] & 0x80 != 0, c[2] & 0x04 != 0);
            if objects == 0 || !stereo { return None }
            if exact && !(one_bit(c[0]) && (c[1].count_ones() + (c[2] >> 4).count_ones()) == 1 && one_bit(c[2] & 0x0c)) { return None }
            let offered = u32::from(c[3] & 0x7f) << 16 | u32::from(c[4]) << 8 | u32::from(c[5]);
            // Zero means the sink does not say.
            let ceiling = if offered == 0 { AAC_MAX_BITRATE } else { offered.min(AAC_MAX_BITRATE) };
            if ceiling < 64_000 { return None }
            Some(Codec::Aac { rate: rate(r44, r48)?, bitrate: if exact { ceiling } else { ceiling.min(AAC_BITRATE) } })
        }
        Kind::AptX | Kind::AptXHd => {
            if c[0] & 0x02 == 0 || (exact && !(one_bit(c[0] >> 4) && one_bit(c[0] & 0x0f))) { return None }
            let rate = rate(c[0] & 0x20 != 0, c[0] & 0x10 != 0)?;
            Some(if kind == Kind::AptX { Codec::AptX { rate } } else { Codec::AptXHd { rate } })
        }
        Kind::Ldac => {
            if c[1] & 0x01 == 0 || (exact && !(one_bit(c[0]) && one_bit(c[1]))) { return None }
            Some(Codec::Ldac { rate: rate(c[0] & 0x20 != 0, c[0] & 0x10 != 0)? })
        }
    }
}

/// The element that configures `codec` on a sink that offered `offer`.
pub fn configuration(codec: &Codec, offer: &[u8]) -> Vec<u8> {
    let info = identify(offer).map(|(_, info)| info).unwrap_or(&[]);
    match *codec {
        Codec::Sbc(p) => vec![0x00, 0x00, (if p.rate == 44100 { 0x20 } else { 0x10 }) | if p.joint { 0x01 } else { 0x02 },
            (0x80 >> (p.blocks / 4 - 1)) | 0x04 | if p.snr { 0x02 } else { 0x01 }, info.get(2).copied().unwrap_or(2).max(2), p.bitpool],
        Codec::Aac { rate, bitrate } => {
            // MPEG-2 AAC LC where the sink has it (every AAC sink must), else MPEG-4 AAC LC.
            let object = if info.first().is_none_or(|o| o & 0x80 != 0) { 0x80 } else { 0x40 };
            vec![0x00, 0x02, object, u8::from(rate == 44100), (if rate == 48000 { 0x80 } else { 0 }) | 0x04, (bitrate >> 16) as u8 & 0x7f, (bitrate >> 8) as u8, bitrate as u8]
        }
        Codec::AptX { rate } => [&[0x00, 0xff][..], &APTX_VENDOR, &[(if rate == 44100 { 0x20 } else { 0x10 }) | 0x02]].concat(),
        Codec::AptXHd { rate } => [&[0x00, 0xff][..], &APTX_HD_VENDOR, &[(if rate == 44100 { 0x20 } else { 0x10 }) | 0x02, 0, 0, 0, 0]].concat(),
        Codec::Ldac { rate } => [&[0x00, 0xff][..], &LDAC_VENDOR, &[if rate == 44100 { 0x20 } else { 0x10 }, 0x01]].concat(),
    }
}

/// The best stream among a sink's offers, by `preference` (first wins); the offer's index comes with it.
pub fn best(offers: &[Vec<u8>], preference: &[Kind]) -> Option<(usize, Codec)> {
    preference.iter().find_map(|kind| offers.iter().enumerate().find_map(|(i, offer)| {
        identify(offer).filter(|(k, _)| k == kind).and_then(|_| choose(offer, false)).map(|codec| (i, codec))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_offer_of_ours_is_identified_configurable_and_accepted_back() {
        for kind in Kind::BY_QUALITY {
            let ours = capabilities(kind);
            assert_eq!(identify(&ours).unwrap().0, kind);
            assert_eq!(Kind::of_seid(kind.seid()), Some(kind));
            let chosen = choose(&ours, false).unwrap();
            assert_eq!((chosen.kind(), chosen.rate()), (kind, 44100));
            let configured = configuration(&chosen, &ours);
            assert_eq!(configured.len(), ours.len());
            // A configuration is exact: the acceptor's check takes ours and reads the same stream back.
            let accepted = choose(&configured, true).unwrap();
            assert_eq!((accepted.kind(), accepted.rate()), (kind, 44100), "{configured:02x?}");
            // An offer is not a configuration.
            assert_eq!(choose(&ours, true), None, "{kind:?}");
        }
        assert_eq!(choose(&capabilities(Kind::Sbc), false), Some(Codec::Sbc(sbc::Params { rate: 44100, blocks: 16, joint: true, snr: false, bitpool: 53 })));
        assert_eq!(choose(&capabilities(Kind::Aac), false), Some(Codec::Aac { rate: 44100, bitrate: 256_000 }));
        assert!(!Codec::AptX { rate: 44100 }.rtp() && Codec::AptXHd { rate: 44100 }.rtp());
    }

    #[test]
    fn real_world_offers_are_read_as_their_implementations_write_them() {
        // AAC: MPEG-2 LC only, 44.1 kHz, 2 channels, VBR flag, 320 kbit/s: 256 kbit/s is chosen, MPEG-2 LC configured.
        let aac = [0x00, 0x02, 0x80, 0x01, 0x04, 0x84, 0xe2, 0x00];
        let chosen = choose(&aac, false).unwrap();
        assert_eq!(chosen, Codec::Aac { rate: 44100, bitrate: 256_000 });
        assert_eq!(configuration(&chosen, &aac), [0x00, 0x02, 0x80, 0x01, 0x04, 0x03, 0xe8, 0x00]);
        // AAC with a low ceiling keeps to it; MPEG-4 only is configured as MPEG-4; 48 kHz only is taken.
        let low = [0x00, 0x02, 0x40, 0x00, 0x84, 0x01, 0xf4, 0x00];
        assert_eq!(choose(&low, false), Some(Codec::Aac { rate: 48000, bitrate: 128_000 }));
        assert_eq!(configuration(&choose(&low, false).unwrap(), &low)[2..5], [0x40, 0x00, 0x84]);
        // aptX at 48 kHz only; aptX HD; LDAC with all rates and modes.
        assert_eq!(choose(&[0x00, 0xff, 0x4f, 0, 0, 0, 0x01, 0, 0x12], false), Some(Codec::AptX { rate: 48000 }));
        assert_eq!(choose(&[0x00, 0xff, 0xd7, 0, 0, 0, 0x24, 0, 0x32, 0, 0, 0, 0], false), Some(Codec::AptXHd { rate: 44100 }));
        assert_eq!(choose(&[0x00, 0xff, 0x2d, 0x01, 0, 0, 0xaa, 0, 0x3c, 0x07], false), Some(Codec::Ldac { rate: 44100 }));
        // Refused: mono-only aptX, LDAC at 96 kHz only, another vendor's codec, a video codec, truncated elements.
        assert_eq!(choose(&[0x00, 0xff, 0x4f, 0, 0, 0, 0x01, 0, 0x31], false), None);
        assert_eq!(choose(&[0x00, 0xff, 0x2d, 0x01, 0, 0, 0xaa, 0, 0x04, 0x01], false), None);
        assert_eq!(identify(&[0x00, 0xff, 0x0a, 0, 0, 0, 0x01, 0, 0x32]), None);
        assert_eq!(identify(&[0x10, 0x00, 0x33, 0xf7, 2, 53]), None);
        for cut in 0..9 { let _ = choose(&capabilities(Kind::AptX)[..cut], false); }
    }

    #[test]
    fn the_best_offer_follows_the_preference_and_falls_back_to_sbc() {
        let offers: Vec<Vec<u8>> = vec![capabilities(Kind::Sbc), capabilities(Kind::Aac), capabilities(Kind::AptX), vec![0x00, 0xff, 0x2d, 0x01, 0, 0, 0xaa, 0, 0x04, 0x01]];
        assert_eq!(best(&offers, &Kind::BY_QUALITY).unwrap(), (2, Codec::AptX { rate: 44100 }), "LDAC at 96 kHz only is passed over");
        assert_eq!(best(&offers, &[Kind::Aac, Kind::Sbc]).unwrap().0, 1);
        assert_eq!(best(&offers, &[Kind::Ldac, Kind::AptXHd, Kind::Sbc]).unwrap().0, 0);
        assert_eq!(best(&offers[1..], &[Kind::Sbc]), None);
        let list = [1, 0, 7, 6, 0, 0, 0x3f, 0xff, 2, 53, 8, 0, 7, 9, 0, 0xff, 0x4f, 0, 0, 0, 1, 0, 0x32, 7, 200];
        assert_eq!(elements(&list).len(), 2);
    }
}
