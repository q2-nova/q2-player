//! LATM packing: round trip and one hand-built element.

use q2_aac::{latm, unlatm};

#[test]
fn hand_built_element() {
    // useSameStreamMux 0, audioMuxVersion 0, allStreamsSameTimeFraming 1,
    // numSubFrames 000000, numProgram 0000, numLayer 000        -> 0x20 0x00
    // AudioSpecificConfig 00010 0100 0010 000 (LC, 44100, 2 ch)  -> 0x12 0x10
    // frameLengthType 000, latmBufferFullness 11111111,
    // otherDataPresent 0, crcCheckPresent 0, length 00000010,
    // payload 0xDE 0xAD, three zero bits:
    // 00011111 11100000 00010110 11110101 01101000               -> 1F E0 16 F5 68
    let element = latm(&[0xDE, 0xAD], 44100);
    assert_eq!(
        element,
        [0x20, 0x00, 0x12, 0x10, 0x1F, 0xE0, 0x16, 0xF5, 0x68]
    );
    assert_eq!(unlatm(&element).as_deref(), Some(&[0xDE, 0xAD][..]));
    // 48000 Hz changes only the sampling frequency index: 00010 0011 0010 000.
    assert_eq!(latm(&[0xDE, 0xAD], 48000)[..4], [0x20, 0x00, 0x11, 0x90]);
}

#[test]
fn round_trip_lengths() {
    // Lengths around the 255-byte steps of PayloadLengthInfo, and the largest stereo
    // access unit (1536 bytes).
    for length in [
        0usize, 1, 2, 254, 255, 256, 509, 510, 511, 743, 765, 1024, 1536,
    ] {
        let unit: Vec<u8> = (0..length).map(|i| (i * 31 + length) as u8).collect();
        for rate in [44100, 48000] {
            let element = latm(&unit, rate);
            assert_eq!(element.len(), length + 7 + length / 255, "length {length}");
            assert_eq!(
                unlatm(&element).as_deref(),
                Some(&unit[..]),
                "length {length}"
            );
        }
    }
}

#[test]
fn unlatm_is_strict() {
    let element = latm(&[1, 2, 3, 4, 5], 44100);
    assert!(unlatm(&element).is_some());
    // Truncated and extended elements.
    assert!(unlatm(&element[..element.len() - 1]).is_none());
    let mut longer = element.clone();
    longer.push(0);
    assert!(unlatm(&longer).is_none());
    assert!(unlatm(&[]).is_none());
    // Non-zero padding.
    let mut padded = element.clone();
    *padded.last_mut().unwrap() |= 1;
    assert!(unlatm(&padded).is_none());
    // Flipping any of the 45 header bits fails, except the four sampling frequency
    // index bits, which can select another valid rate.
    for bit in 0..45 {
        if (21..25).contains(&bit) {
            continue;
        }
        let mut changed = element.clone();
        changed[bit / 8] ^= 0x80 >> (bit % 8);
        assert!(unlatm(&changed).is_none(), "bit {bit}");
    }
}
