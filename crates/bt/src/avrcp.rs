//! AVRCP over AVCTP, both roles on the one control channel.
//!
//! Target (category 1): the headphones' buttons, including passthrough
//! volume up and down, and their status queries. A headphone that registers
//! for volume changes here, or sets an absolute volume here, is answered as
//! well, although the specification gives those to the rendering side.
//!
//! Controller (category 2, AVRCP 1.4 absolute volume): the player asks which
//! events the headphones report, registers for their volume changes and sets
//! their amplifier with SetAbsoluteVolume. Levels are 0..=127 (127 = 100 %).

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button { Play, Pause, Stop, Next, Previous, VolumeUp, VolumeDown }

const PID: [u8; 2] = [0x11, 0x0e];
const COMPANY: [u8; 3] = [0x00, 0x19, 0x58];
// Command types.
const CONTROL: u8 = 0x0;
const STATUS: u8 = 0x1;
const NOTIFY: u8 = 0x3;
// Response codes.
const NOT_IMPLEMENTED: u8 = 0x8;
const ACCEPTED: u8 = 0x9;
const REJECTED: u8 = 0xa;
const STABLE: u8 = 0xc;
const CHANGED: u8 = 0xd;
const INTERIM: u8 = 0xf;
// PDUs.
const GET_CAPABILITIES: u8 = 0x10;
const REGISTER_NOTIFICATION: u8 = 0x31;
const SET_ABSOLUTE_VOLUME: u8 = 0x50;
// Events.
const PLAYBACK_STATUS: u8 = 0x01;
pub const VOLUME_CHANGED: u8 = 0x0d;

fn vendor(ctype: u8, pdu: u8, parameters: &[u8]) -> Vec<u8> {
    let mut out = vec![ctype, 0x48, 0x00, COMPANY[0], COMPANY[1], COMPANY[2], pdu, 0, (parameters.len() >> 8) as u8, parameters.len() as u8];
    out.extend_from_slice(parameters);
    out
}

/// What a command from the headphones asks of the player, besides its response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    Button(Button),
    /// SetAbsoluteVolume sent to the player: the headphones' new level.
    Volume(u8),
    /// A registration for playback status changes, with its transaction label.
    StatusRegistration(u8),
    /// A registration for volume changes, with its transaction label.
    VolumeRegistration(u8),
}

/// Handles one AVCTP command packet from the headphones. `volume` is the
/// player's level (0..=127) for volume registrations. Returns the response
/// to send and what was asked.
pub fn handle(packet: &[u8], playing: bool, volume: u8) -> (Option<Vec<u8>>, Option<Request>) {
    handle_limited(packet, playing, volume, 127)
}

pub(crate) fn handle_limited(packet: &[u8], playing: bool, volume: u8, limit: u8) -> (Option<Vec<u8>>, Option<Request>) {
    let Some(&[header, pid0, pid1]) = packet.get(..3).map(|h| <&[u8; 3]>::try_from(h).unwrap()) else { return (None, None) };
    // Single packets only; responses and fragments are not ours to answer.
    if header & 0x0e != 0 { return (None, None) }
    let label = header >> 4;
    let reply = |frame: Vec<u8>| { let mut out = vec![header & 0xf0 | 0x02, pid0, pid1]; out.extend(frame); Some(out) };
    if [pid0, pid1] != PID { return (Some(vec![header & 0xf0 | 0x03, pid0, pid1]), None) }
    let frame = &packet[3..];
    let Some(&[ctype, subunit, opcode]) = frame.get(..3).map(|h| <&[u8; 3]>::try_from(h).unwrap()) else { return (None, None) };
    let echo = |code: u8| { let mut out = frame.to_vec(); out[0] = code; out };
    match opcode {
        // Pass through.
        0x7c if subunit == 0x48 && frame.len() >= 5 => {
            let (released, id) = (frame[3] & 0x80 != 0, frame[3] & 0x7f);
            let button = match id { 0x44 => Some(Button::Play), 0x46 => Some(Button::Pause), 0x45 => Some(Button::Stop),
                0x4b => Some(Button::Next), 0x4c => Some(Button::Previous), 0x41 => Some(Button::VolumeUp), 0x42 => Some(Button::VolumeDown), _ => None };
            match button {
                Some(button) => (reply(echo(ACCEPTED)), (!released).then_some(Request::Button(button))),
                None => (reply(echo(NOT_IMPLEMENTED)), None),
            }
        }
        // Unit info and subunit info.
        0x30 => (reply(vec![STABLE, 0xff, 0x30, 0x07, 0x48, COMPANY[0], COMPANY[1], COMPANY[2]]), None),
        0x31 => (reply(vec![STABLE, 0xff, 0x31, 0x07, 0x48, 0xff, 0xff, 0xff]), None),
        // Vendor dependent: Bluetooth SIG metadata transfer.
        0x00 if frame.len() >= 10 && frame[3..6] == COMPANY => {
            let (pdu, parameters) = (frame[6], &frame[10..]);
            let status = if playing { 1 } else { 2 };
            let (response, request) = match (pdu, parameters.first().copied()) {
                (GET_CAPABILITIES, Some(2)) if ctype == STATUS => (vendor(STABLE, pdu, &[2, 1, COMPANY[0], COMPANY[1], COMPANY[2]]), None),
                // Only playback status is offered; volume belongs to the headphones' side.
                (GET_CAPABILITIES, Some(3)) if ctype == STATUS => (vendor(STABLE, pdu, &[3, 1, PLAYBACK_STATUS]), None),
                (0x30, _) if ctype == STATUS => (vendor(STABLE, pdu, &[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, status]), None),
                (0x20, _) if ctype == STATUS => (vendor(STABLE, pdu, &[0]), None),
                (REGISTER_NOTIFICATION, Some(PLAYBACK_STATUS)) if ctype == NOTIFY => (vendor(INTERIM, pdu, &[PLAYBACK_STATUS, status]), Some(Request::StatusRegistration(label))),
                (REGISTER_NOTIFICATION, Some(VOLUME_CHANGED)) if ctype == NOTIFY => (vendor(INTERIM, pdu, &[VOLUME_CHANGED, volume & 0x7f]), Some(Request::VolumeRegistration(label))),
                (SET_ABSOLUTE_VOLUME, Some(level)) if ctype == CONTROL => {
                    let accepted = (level & 0x7f).min(limit);
                    (vendor(ACCEPTED, pdu, &[accepted]), Some(Request::Volume(accepted)))
                }
                // Invalid parameter for a known PDU, invalid command otherwise.
                (GET_CAPABILITIES | REGISTER_NOTIFICATION | SET_ABSOLUTE_VOLUME, _) => (vendor(REJECTED, pdu, &[0x01]), None),
                _ => (vendor(REJECTED, pdu, &[0x00]), None),
            };
            (reply(response), request)
        }
        _ => (reply(echo(NOT_IMPLEMENTED)), None),
    }
}

/// A response from the headphones to one of the player's commands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reply {
    /// GetCapabilities: the event identifiers the headphones report.
    Events(Vec<u8>),
    /// The interim response to a volume registration: the current level.
    VolumeInterim(u8),
    /// The volume changed on the headphones; the registration has ended.
    VolumeChanged(u8),
    /// SetAbsoluteVolume accepted, with the level the headphones set.
    VolumeSet(u8),
    /// The headphones refused or do not implement the PDU.
    Refused(u8),
}

/// Whether `packet` is an AVCTP response (the C/R bit) rather than a command.
pub fn is_response(packet: &[u8]) -> bool { packet.first().is_some_and(|h| h & 0x02 != 0) }

/// Parses a response to one of the player's vendor-dependent commands: its label and content.
pub fn reply(packet: &[u8]) -> Option<(u8, Reply)> {
    let header = *packet.first()?;
    if header & 0x0e != 0x02 || packet.get(1..3)? != PID { return None }
    let frame = &packet[3..];
    if frame.len() < 10 || frame[1] != 0x48 || frame[2] != 0x00 || frame[3..6] != COMPANY { return None }
    let (code, pdu) = (frame[0], frame[6]);
    let len = usize::from(u16::from_be_bytes([frame[8], frame[9]]));
    let parameters = frame.get(10..10 + len).unwrap_or(&frame[10..]);
    let reply = match (pdu, code, parameters) {
        (GET_CAPABILITIES, STABLE, [3, count, events @ ..]) => Reply::Events(events.iter().take(usize::from(*count)).copied().collect()),
        (REGISTER_NOTIFICATION, INTERIM, [VOLUME_CHANGED, level, ..]) => Reply::VolumeInterim(level & 0x7f),
        (REGISTER_NOTIFICATION, CHANGED, [VOLUME_CHANGED, level, ..]) => Reply::VolumeChanged(level & 0x7f),
        (SET_ABSOLUTE_VOLUME, ACCEPTED, [level, ..]) => Reply::VolumeSet(level & 0x7f),
        (_, NOT_IMPLEMENTED | REJECTED, _) => Reply::Refused(pdu),
        _ => return None,
    };
    Some((header >> 4, reply))
}

fn command(label: u8, ctype: u8, pdu: u8, parameters: &[u8]) -> Vec<u8> {
    let mut out = vec![(label & 0xf) << 4, PID[0], PID[1]];
    out.extend(vendor(ctype, pdu, parameters));
    out
}
/// GetCapabilities for the events the headphones support.
pub fn get_events(label: u8) -> Vec<u8> { command(label, STATUS, GET_CAPABILITIES, &[3]) }
/// RegisterNotification for the headphones' volume.
pub fn register_volume(label: u8) -> Vec<u8> { command(label, NOTIFY, REGISTER_NOTIFICATION, &[VOLUME_CHANGED, 0, 0, 0, 0]) }
/// SetAbsoluteVolume, `level` 0..=127.
pub fn set_volume(label: u8, level: u8) -> Vec<u8> { command(label, CONTROL, SET_ABSOLUTE_VOLUME, &[level & 0x7f]) }

/// The CHANGED notification for playback status, answering a registration made with `label`.
pub fn status_changed(label: u8, playing: bool) -> Vec<u8> {
    let mut out = vec![label << 4 | 0x02, PID[0], PID[1]];
    out.extend(vendor(CHANGED, REGISTER_NOTIFICATION, &[PLAYBACK_STATUS, if playing { 1 } else { 2 }]));
    out
}
/// The CHANGED notification for the player's volume, answering the headphones' registration made with `label`.
pub fn volume_changed(label: u8, level: u8) -> Vec<u8> {
    let mut out = vec![label << 4 | 0x02, PID[0], PID[1]];
    out.extend(vendor(CHANGED, REGISTER_NOTIFICATION, &[VOLUME_CHANGED, level & 0x7f]));
    out
}

/// The fake headphones' side: a response frame to a command with `label`.
#[cfg(any(test, feature = "fake"))]
pub fn response(label: u8, code: u8, pdu: u8, parameters: &[u8]) -> Vec<u8> {
    let mut out = vec![(label & 0xf) << 4 | 0x02, PID[0], PID[1]];
    out.extend(vendor(code, pdu, parameters));
    out
}
/// The fake headphones' side: a command's label, command type, PDU and parameters.
#[cfg(any(test, feature = "fake"))]
pub fn parse_command(packet: &[u8]) -> Option<(u8, u8, u8, Vec<u8>)> {
    if packet.len() < 13 || packet[0] & 0x0e != 0 || packet[1..3] != PID || packet[4] != 0x48 || packet[5] != 0x00 || packet[6..9] != COMPANY { return None }
    let len = usize::from(u16::from_be_bytes([packet[11], packet[12]]));
    Some((packet[0] >> 4, packet[3], packet[9], packet.get(13..13 + len)?.to_vec()))
}
#[cfg(any(test, feature = "fake"))]
pub const CODES: (u8, u8, u8, u8) = (ACCEPTED, STABLE, CHANGED, INTERIM);

#[cfg(any(test, feature = "fake"))]
pub fn pass_through(label: u8, id: u8, released: bool) -> Vec<u8> {
    vec![label << 4, PID[0], PID[1], 0x00, 0x48, 0x7c, id | if released { 0x80 } else { 0 }, 0x00]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_are_accepted_once_per_press_including_volume() {
        for (id, button) in [(0x44, Button::Play), (0x46, Button::Pause), (0x4b, Button::Next), (0x4c, Button::Previous), (0x45, Button::Stop),
            (0x41, Button::VolumeUp), (0x42, Button::VolumeDown)] {
            let (response, pressed) = handle(&pass_through(5, id, false), false, 0);
            assert_eq!(pressed, Some(Request::Button(button)));
            assert_eq!(response.unwrap(), [0x52, 0x11, 0x0e, 0x09, 0x48, 0x7c, id, 0x00]);
            let (response, released) = handle(&pass_through(6, id, true), false, 0);
            assert_eq!((response.unwrap()[3], released), (0x09, None));
        }
        // Mute is not one of ours.
        let (response, pressed) = handle(&pass_through(1, 0x43, false), false, 0);
        assert_eq!((response.unwrap()[3], pressed), (0x08, None));
    }

    #[test]
    fn capabilities_offer_playback_status_and_volume_requests_from_the_headphones_are_served() {
        let request = |ctype: u8, pdu: u8, parameters: &[u8]| { let mut p = vec![0x30, 0x11, 0x0e]; p.extend(vendor(ctype, pdu, parameters)); p };
        let (response, _) = handle(&request(1, 0x10, &[3]), true, 0);
        assert_eq!(response.unwrap()[3..], [0x0c, 0x48, 0x00, 0x00, 0x19, 0x58, 0x10, 0, 0, 3, 3, 1, 1]);
        let (response, asked) = handle(&request(3, 0x31, &[1, 0, 0, 0, 0]), true, 0);
        assert_eq!(response.unwrap()[3..], [0x0f, 0x48, 0x00, 0x00, 0x19, 0x58, 0x31, 0, 0, 2, 1, 1]);
        assert_eq!(asked, Some(Request::StatusRegistration(3)));
        assert_eq!(status_changed(3, false)[..4], [0x32, 0x11, 0x0e, 0x0d]);
        // Volume registration: interim with the player's level, then changed.
        let (response, asked) = handle(&request(3, 0x31, &[0x0d, 0, 0, 0, 0]), true, 0x40);
        assert_eq!(response.unwrap()[3..], [0x0f, 0x48, 0x00, 0x00, 0x19, 0x58, 0x31, 0, 0, 2, 0x0d, 0x40]);
        assert_eq!(asked, Some(Request::VolumeRegistration(3)));
        assert_eq!(volume_changed(3, 0x7f)[3..], [0x0d, 0x48, 0x00, 0x00, 0x19, 0x58, 0x31, 0, 0, 2, 0x0d, 0x7f]);
        // SetAbsoluteVolume from the headphones is accepted at the level asked, reserved bit cleared.
        let (response, asked) = handle(&request(0, 0x50, &[0xc0]), true, 0);
        assert_eq!(response.unwrap()[3..], [0x09, 0x48, 0x00, 0x00, 0x19, 0x58, 0x50, 0, 0, 1, 0x40]);
        assert_eq!(asked, Some(Request::Volume(0x40)));
        let (response, asked) = handle_limited(&request(0, 0x50, &[0x7f]), true, 0, 31);
        assert_eq!(response.unwrap().last(), Some(&31));
        assert_eq!(asked, Some(Request::Volume(31)));
        // Wrong command types and unknown PDUs are rejected.
        assert_eq!(handle(&request(1, 0x50, &[0x40]), true, 0).0.unwrap()[3], 0x0a);
        assert_eq!(handle(&request(3, 0x31, &[0x05, 0, 0, 0, 0]), true, 0).0.unwrap()[3], 0x0a);
        assert_eq!(handle(&request(1, 0x60, &[]), true, 0).0.unwrap()[3], 0x0a);
    }

    #[test]
    fn controller_commands_are_well_formed_and_replies_parse() {
        assert_eq!(get_events(2), [0x20, 0x11, 0x0e, 0x01, 0x48, 0x00, 0x00, 0x19, 0x58, 0x10, 0, 0, 1, 3]);
        assert_eq!(register_volume(4), [0x40, 0x11, 0x0e, 0x03, 0x48, 0x00, 0x00, 0x19, 0x58, 0x31, 0, 0, 5, 0x0d, 0, 0, 0, 0]);
        assert_eq!(set_volume(5, 0xff), [0x50, 0x11, 0x0e, 0x00, 0x48, 0x00, 0x00, 0x19, 0x58, 0x50, 0, 0, 1, 0x7f]);
        assert_eq!(parse_command(&set_volume(5, 0x20)), Some((5, 0, 0x50, vec![0x20])));
        assert!(!is_response(&set_volume(1, 1)) && is_response(&response(1, 0x9, 0x50, &[1])));
        assert_eq!(reply(&response(2, 0xc, 0x10, &[3, 2, 0x01, 0x0d])), Some((2, Reply::Events(vec![1, 0x0d]))));
        assert_eq!(reply(&response(4, 0xf, 0x31, &[0x0d, 0x33])), Some((4, Reply::VolumeInterim(0x33))));
        assert_eq!(reply(&response(4, 0xd, 0x31, &[0x0d, 0x90])), Some((4, Reply::VolumeChanged(0x10))));
        assert_eq!(reply(&response(5, 0x9, 0x50, &[0x41])), Some((5, Reply::VolumeSet(0x41))));
        assert_eq!(reply(&response(6, 0x8, 0x10, &[3])), Some((6, Reply::Refused(0x10))));
        assert_eq!(reply(&response(6, 0xa, 0x31, &[1])), Some((6, Reply::Refused(0x31))));
        // A command, a passthrough response and a playback-status notification are not replies to volume commands.
        assert_eq!(reply(&set_volume(1, 1)), None);
        assert_eq!(reply(&[0x12, 0x11, 0x0e, 0x09, 0x48, 0x7c, 0x44, 0x00]), None);
        assert_eq!(reply(&response(1, 0xd, 0x31, &[1, 1])), None);
    }

    #[test]
    fn other_profiles_and_garbage_are_answered_or_ignored_without_panicking() {
        assert_eq!(handle(&[0x40, 0x11, 0x0f, 1, 2, 3], false, 0).0.unwrap(), [0x43, 0x11, 0x0f]);
        let mut seed = 11u32;
        for _ in 0..5000 {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let len = (seed >> 27) as usize;
            let packet: Vec<u8> = (0..len).map(|i| { seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223); if i < 3 { [0x10 | ((seed >> 30) as u8 & 2), 0x11, 0x0e][i] } else { (seed >> 16) as u8 } }).collect();
            let _ = handle(&packet, false, 0x7f);
            let _ = reply(&packet);
            let _ = parse_command(&packet);
        }
    }
}
