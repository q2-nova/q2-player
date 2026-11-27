//! SDP server with three fixed records: A2DP source, AVRCP target and AVRCP
//! controller (1.4, for absolute volume). Headphones read these to learn the
//! profile versions; nothing is registered at run time. The client side is a
//! service search (whether a device has an A2DP sink) and the Device ID
//! record's vendor and product.

const UUID_L2CAP: u16 = 0x0100;
const UUID_AVCTP: u16 = 0x0017;
const UUID_AVDTP: u16 = 0x0019;
const UUID_BROWSE_ROOT: u16 = 0x1002;
pub const UUID_AUDIO_SOURCE: u16 = 0x110a;
pub const UUID_AUDIO_SINK: u16 = 0x110b;
pub const UUID_AV_REMOTE_TARGET: u16 = 0x110c;
const UUID_A2DP: u16 = 0x110d;
const UUID_AVRCP: u16 = 0x110e;
const UUID_AV_REMOTE_CONTROLLER: u16 = 0x110f;
const UUID_DEVICE_ID: u16 = 0x1200;

fn u16e(v: u16) -> Vec<u8> { vec![0x09, (v >> 8) as u8, v as u8] }
fn uuid(v: u16) -> Vec<u8> { vec![0x19, (v >> 8) as u8, v as u8] }
fn seq(items: &[Vec<u8>]) -> Vec<u8> {
    let body: Vec<u8> = items.concat();
    let mut out = if body.len() < 256 { vec![0x35, body.len() as u8] } else { vec![0x36, (body.len() >> 8) as u8, body.len() as u8] };
    out.extend(body);
    out
}

struct Record { handle: u32, uuids: &'static [u16], attributes: Vec<(u16, Vec<u8>)> }

fn records() -> [Record; 3] {
    let record = |handle: u32, classes: &[u16], psm: u16, protocol: u16, protocol_version: u16, profile: u16, profile_version: u16, features: u16, uuids| Record {
        handle, uuids,
        attributes: vec![
            (0x0000, vec![0x0a, (handle >> 24) as u8, (handle >> 16) as u8, (handle >> 8) as u8, handle as u8]),
            (0x0001, seq(&classes.iter().map(|c| uuid(*c)).collect::<Vec<_>>())),
            (0x0004, seq(&[seq(&[uuid(UUID_L2CAP), u16e(psm)]), seq(&[uuid(protocol), u16e(protocol_version)])])),
            (0x0005, seq(&[uuid(UUID_BROWSE_ROOT)])),
            (0x0009, seq(&[seq(&[uuid(profile), u16e(profile_version)])])),
            (0x0311, u16e(features)),
        ],
    };
    [
        // A2DP 1.3 source; supported features: player.
        record(0x0001_0001, &[UUID_AUDIO_SOURCE], 0x0019, UUID_AVDTP, 0x0103, UUID_A2DP, 0x0103, 0x0001,
            &[UUID_AUDIO_SOURCE, UUID_L2CAP, UUID_AVDTP, UUID_BROWSE_ROOT, UUID_A2DP]),
        // AVRCP 1.4 target over AVCTP 1.3; category 1 (player); no browsing.
        record(0x0001_0002, &[UUID_AV_REMOTE_TARGET], 0x0017, UUID_AVCTP, 0x0103, UUID_AVRCP, 0x0104, 0x0001,
            &[UUID_AV_REMOTE_TARGET, UUID_L2CAP, UUID_AVCTP, UUID_BROWSE_ROOT, UUID_AVRCP]),
        // AVRCP 1.4 controller; category 2 (amplifier): the player sets the headphones' absolute volume.
        record(0x0001_0003, &[UUID_AVRCP, UUID_AV_REMOTE_CONTROLLER], 0x0017, UUID_AVCTP, 0x0103, UUID_AVRCP, 0x0104, 0x0002,
            &[UUID_AVRCP, UUID_AV_REMOTE_CONTROLLER, UUID_L2CAP, UUID_AVCTP, UUID_BROWSE_ROOT]),
    ]
}

/// One data element: its type, its value bytes and the bytes it occupies.
fn element(b: &[u8]) -> Option<(u8, &[u8], usize)> {
    let descriptor = *b.first()?;
    let (ty, size) = (descriptor >> 3, descriptor & 7);
    let (header, len): (usize, usize) = match size {
        0..=4 => (1, if ty == 0 { 0 } else { 1usize << size }),
        5 => (2, *b.get(1)? as usize),
        6 => (3, u16::from_be_bytes([*b.get(1)?, *b.get(2)?]) as usize),
        _ => (5, u32::from_be_bytes(b.get(1..5)?.try_into().ok()?) as usize),
    };
    Some((ty, b.get(header..header.checked_add(len)?)?, header + len))
}

/// Splits a sequence's value into its elements.
fn items(mut body: &[u8]) -> Option<Vec<(u8, &[u8])>> {
    let mut out = Vec::new();
    while !body.is_empty() { let (ty, value, used) = element(body)?; out.push((ty, value)); body = &body[used..]; }
    Some(out)
}

fn short_uuid(value: &[u8]) -> Option<u16> {
    const BASE: [u8; 12] = [0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0x80, 0x5f, 0x9b, 0x34, 0xfb];
    match value.len() {
        2 => Some(u16::from_be_bytes([value[0], value[1]])),
        4 if value[..2] == [0, 0] => Some(u16::from_be_bytes([value[2], value[3]])),
        16 if value[..2] == [0, 0] && value[4..] == BASE => Some(u16::from_be_bytes([value[2], value[3]])),
        _ => None,
    }
}

/// Parses a service search pattern; `None` in the list is a UUID no record has.
fn pattern(b: &[u8]) -> Option<(Vec<Option<u16>>, usize)> {
    let (ty, body, used) = element(b)?;
    if ty != 6 { return None }
    let list = items(body)?;
    if list.is_empty() || list.len() > 12 || list.iter().any(|(ty, _)| *ty != 3) { return None }
    Some((list.into_iter().map(|(_, value)| short_uuid(value)).collect(), used))
}

fn ranges(b: &[u8]) -> Option<(Vec<(u16, u16)>, usize)> {
    let (ty, body, used) = element(b)?;
    if ty != 6 { return None }
    let mut out = Vec::new();
    for (ty, value) in items(body)? {
        match (ty, value.len()) {
            (1, 2) => { let id = u16::from_be_bytes([value[0], value[1]]); out.push((id, id)); }
            (1, 4) => out.push((u16::from_be_bytes([value[0], value[1]]), u16::from_be_bytes([value[2], value[3]]))),
            _ => return None,
        }
    }
    Some((out, used))
}

fn matches(record: &Record, pattern: &[Option<u16>]) -> bool { pattern.iter().all(|u| u.is_some_and(|u| record.uuids.contains(&u))) }
fn attribute_list(record: &Record, ranges: &[(u16, u16)]) -> Vec<u8> {
    let chosen: Vec<Vec<u8>> = record.attributes.iter().filter(|(id, _)| ranges.iter().any(|(a, b)| a <= id && id <= b))
        .flat_map(|(id, value)| [u16e(*id), value.clone()]).collect();
    seq(&chosen)
}

/// Byte count, a slice of `full` and the continuation state, as the two attribute responses carry them.
fn fragment(full: &[u8], rest: &[u8], max: usize, mtu: usize) -> Option<Vec<u8>> {
    let offset = match rest { [0] => 0, [2, a, b] => u16::from_be_bytes([*a, *b]) as usize, _ => return None };
    if offset > full.len() || max < 7 { return None }
    let take = (full.len() - offset).min(max).min(mtu.saturating_sub(5 + 2 + 3));
    let mut out = vec![(take >> 8) as u8, take as u8];
    out.extend_from_slice(&full[offset..offset + take]);
    let next = offset + take;
    if next < full.len() { out.extend_from_slice(&[2, (next >> 8) as u8, next as u8]); } else { out.push(0); }
    Some(out)
}

fn answer(pdu: u8, p: &[u8], mtu: usize) -> Option<(u8, Vec<u8>)> {
    let all = records();
    match pdu {
        0x02 => {
            let (pattern, used) = pattern(p)?;
            if p.get(used + 2..)? != [0] { return None }
            let handles: Vec<u32> = all.iter().filter(|r| matches(r, &pattern)).map(|r| r.handle).collect();
            let n = handles.len() as u16;
            let mut out = vec![0, n as u8, 0, n as u8];
            for h in handles { out.extend_from_slice(&h.to_be_bytes()); }
            out.push(0);
            Some((0x03, out))
        }
        0x04 => {
            let handle = u32::from_be_bytes(p.get(..4)?.try_into().ok()?);
            let max = u16::from_be_bytes(p.get(4..6)?.try_into().ok()?) as usize;
            let (ranges, used) = ranges(p.get(6..)?)?;
            let record = all.iter().find(|r| r.handle == handle)?;
            Some((0x05, fragment(&attribute_list(record, &ranges), p.get(6 + used..)?, max, mtu)?))
        }
        0x06 => {
            let (pattern, used) = pattern(p)?;
            let max = u16::from_be_bytes(p.get(used..used + 2)?.try_into().ok()?) as usize;
            let (ranges, used2) = ranges(p.get(used + 2..)?)?;
            let lists: Vec<Vec<u8>> = all.iter().filter(|r| matches(r, &pattern)).map(|r| attribute_list(r, &ranges)).collect();
            Some((0x07, fragment(&seq(&lists), p.get(used + 2 + used2..)?, max, mtu)?))
        }
        _ => None,
    }
}

/// A ServiceSearchRequest for records with `uuid`, at most eight handles.
pub fn search_request(tid: u16, uuid16: u16) -> Vec<u8> {
    let mut p = seq(&[uuid(uuid16)]);
    p.extend_from_slice(&[0, 8, 0]);
    let mut request = vec![0x02, (tid >> 8) as u8, tid as u8, (p.len() >> 8) as u8, p.len() as u8];
    request.extend(p);
    request
}

/// The total record count of a ServiceSearchResponse to transaction `tid`; `None` for an
/// error response, another transaction or a malformed PDU.
pub fn search_count(response: &[u8], tid: u16) -> Option<u16> {
    let (pdu, id, len) = (*response.first()?, u16::from_be_bytes(response.get(1..3)?.try_into().ok()?), u16::from_be_bytes(response.get(3..5)?.try_into().ok()?) as usize);
    if pdu != 0x03 || id != tid || response.len() != 5 + len || len < 5 { return None }
    Some(u16::from_be_bytes([response[5], response[6]]))
}

/// A device's Device ID record (SDP 0x1200).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DeviceId {
    /// 1: the Bluetooth SIG assigned `vendor`; 2: the USB Implementers Forum did.
    pub source: u16,
    pub vendor: u16,
    pub product: u16,
    pub version: u16,
}

/// A ServiceSearchAttributeRequest for the Device ID record's attributes 0x0200-0x0205.
pub fn device_id_request(tid: u16) -> Vec<u8> {
    let mut p = seq(&[uuid(UUID_DEVICE_ID)]);
    p.extend_from_slice(&[0x01, 0x00]);
    p.extend(seq(&[vec![0x0a, 0x02, 0x00, 0x02, 0x05]]));
    p.push(0);
    let mut request = vec![0x06, (tid >> 8) as u8, tid as u8, (p.len() >> 8) as u8, p.len() as u8];
    request.extend(p);
    request
}

/// The first Device ID record that names a vendor in a ServiceSearchAttributeResponse to
/// transaction `tid`; `None` for none, a continued or malformed response, or another transaction.
pub fn device_id(response: &[u8], tid: u16) -> Option<DeviceId> {
    let (pdu, id, len) = (*response.first()?, u16::from_be_bytes(response.get(1..3)?.try_into().ok()?), u16::from_be_bytes(response.get(3..5)?.try_into().ok()?) as usize);
    if pdu != 0x07 || id != tid || response.len() != 5 + len { return None }
    let count = u16::from_be_bytes(response.get(5..7)?.try_into().ok()?) as usize;
    if response.get(7 + count..)? != [0] { return None }
    let (ty, body, _) = element(response.get(7..7 + count)?)?;
    if ty != 6 { return None }
    items(body)?.into_iter().filter(|(ty, _)| *ty == 6).find_map(|(_, record)| {
        let mut found = DeviceId::default();
        let mut vendor = false;
        for pair in items(record)?.chunks_exact(2) {
            let ((id_type, id), (value_type, value)) = (pair[0], pair[1]);
            if id_type != 1 || id.len() != 2 || value_type != 1 || value.len() != 2 { continue }
            let value = u16::from_be_bytes([value[0], value[1]]);
            match u16::from_be_bytes([id[0], id[1]]) {
                0x0201 => { found.vendor = value; vendor = true; }
                0x0202 => found.product = value,
                0x0203 => found.version = value,
                0x0205 => found.source = value,
                _ => {}
            }
        }
        vendor.then_some(found)
    })
}

/// A Device ID record's attribute list as a device's SDP server sends it (the fake headphones).
#[cfg(any(test, feature = "fake"))]
pub fn device_id_attributes(id: DeviceId) -> Vec<u8> {
    let pair = |attribute: u16, value: u16| [u16e(attribute), u16e(value)].concat();
    seq(&[pair(0x0200, 0x0103), pair(0x0201, id.vendor), pair(0x0202, id.product), pair(0x0203, id.version), pair(0x0204, 0x0101), pair(0x0205, id.source)])
}
/// A ServiceSearchAttributeResponse holding `lists` (each an attribute list), not continued.
#[cfg(any(test, feature = "fake"))]
pub fn attribute_response(tid: u16, lists: &[Vec<u8>]) -> Vec<u8> {
    let all = seq(lists);
    let mut body = (all.len() as u16).to_be_bytes().to_vec();
    body.extend(all);
    body.push(0);
    let mut out = vec![0x07, (tid >> 8) as u8, tid as u8, (body.len() >> 8) as u8, body.len() as u8];
    out.extend(body);
    out
}

/// Whether a 16-bit or 128-bit UUID list of an extended inquiry response (data types
/// 0x02/0x03 and 0x06/0x07) names `uuid16`.
pub fn eir_lists(kind: u8, data: &[u8], uuid16: u16) -> bool {
    match kind {
        0x02 | 0x03 => data.chunks_exact(2).any(|u| u16::from_le_bytes([u[0], u[1]]) == uuid16),
        // Little-endian 128-bit UUIDs: the base UUID with the 16-bit value in bytes 12 and 13.
        0x06 | 0x07 => data.chunks_exact(16).any(|u| {
            let mut be: Vec<u8> = u.to_vec(); be.reverse();
            short_uuid(&be) == Some(uuid16)
        }),
        _ => false,
    }
}

/// The response PDU to one request PDU; `None` for something that is not a request.
pub fn respond(request: &[u8], mtu: usize) -> Option<Vec<u8>> {
    let (pdu, tid, len) = (*request.first()?, request.get(1..3)?, u16::from_be_bytes(request.get(3..5)?.try_into().ok()?) as usize);
    let (code, body) = match request.get(5..5 + len).filter(|_| request.len() == 5 + len).and_then(|p| answer(pdu, p, mtu)) {
        Some(answer) => answer,
        // Invalid request syntax; an unknown record handle is reported the same way.
        None => (0x01, vec![0x00, 0x03]),
    };
    let mut out = vec![code, tid[0], tid[1], (body.len() >> 8) as u8, body.len() as u8];
    out.extend(body);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn search_attributes(uuids: &[Vec<u8>], max: u16, continuation: &[u8]) -> Vec<u8> {
        let mut p = seq(uuids);
        p.extend_from_slice(&max.to_be_bytes());
        p.extend(seq(&[vec![0x0a, 0x00, 0x00, 0xff, 0xff]]));
        p.extend_from_slice(continuation);
        let mut request = vec![0x06, 0x12, 0x34, (p.len() >> 8) as u8, p.len() as u8];
        request.extend(p);
        request
    }

    #[test]
    fn audio_source_record_carries_avdtp_psm_and_versions() {
        let response = respond(&search_attributes(&[uuid(UUID_AUDIO_SOURCE)], 512, &[0]), 672).unwrap();
        assert_eq!(response[..3], [0x07, 0x12, 0x34]);
        let count = u16::from_be_bytes([response[5], response[6]]) as usize;
        let lists = &response[7..7 + count];
        assert_eq!(response[7 + count..], [0]);
        let (ty, body, used) = element(lists).unwrap();
        assert_eq!((ty, used), (6, count));
        let records = items(body).unwrap();
        assert_eq!(records.len(), 1);
        let attributes = items(records[0].1).unwrap();
        assert_eq!(attributes.len(), 12);
        // ProtocolDescriptorList: L2CAP PSM 0x19, AVDTP 1.3.
        let protocols = attributes.iter().position(|(_, v)| *v == [0x00, 0x04]).unwrap();
        assert_eq!(attributes[protocols + 1].1, [0x35, 6, 0x19, 0x01, 0x00, 0x09, 0x00, 0x19, 0x35, 6, 0x19, 0x00, 0x19, 0x09, 0x01, 0x03]);
    }

    #[test]
    fn searches_match_all_uuids_including_128_bit_forms_and_the_browse_root() {
        let count = |uuids: &[Vec<u8>]| {
            let mut p = seq(uuids);
            p.extend_from_slice(&[0, 10, 0]);
            let mut request = vec![0x02, 0, 1, 0, p.len() as u8];
            request.extend(p);
            let response = respond(&request, 672).unwrap();
            assert_eq!(response[0], 0x03);
            response[8] as usize
        };
        assert_eq!(count(&[uuid(UUID_BROWSE_ROOT)]), 3);
        assert_eq!(count(&[uuid(UUID_L2CAP), uuid(UUID_AVCTP)]), 2);
        assert_eq!(count(&[uuid(UUID_AV_REMOTE_CONTROLLER)]), 1);
        assert_eq!(count(&[uuid(UUID_AUDIO_SOURCE), uuid(UUID_AV_REMOTE_TARGET)]), 0);
        assert_eq!(count(&[uuid(0x1108)]), 0);
        let mut long = vec![0x1c, 0, 0, 0x11, 0x0c];
        long.extend_from_slice(&[0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0x80, 0x5f, 0x9b, 0x34, 0xfb]);
        assert_eq!(count(&[long.clone()]), 1);
        long[5] = 1;
        assert_eq!(count(&[long]), 0);
    }

    #[test]
    fn continuation_reassembles_to_the_single_response() {
        let whole = respond(&search_attributes(&[uuid(UUID_L2CAP)], 1024, &[0]), 672).unwrap();
        let count = u16::from_be_bytes([whole[5], whole[6]]) as usize;
        let expected = whole[7..7 + count].to_vec();
        let (mut got, mut continuation) = (Vec::new(), vec![0u8]);
        for _ in 0..40 {
            let part = respond(&search_attributes(&[uuid(UUID_L2CAP)], 16, &continuation), 48).unwrap();
            let count = u16::from_be_bytes([part[5], part[6]]) as usize;
            assert!(count <= 16 && part.len() <= 48);
            got.extend_from_slice(&part[7..7 + count]);
            continuation = part[7 + count..].to_vec();
            if continuation == [0] { break }
        }
        assert_eq!(got, expected);
    }

    #[test]
    fn avrcp_records_are_target_and_controller_1_4_with_their_categories() {
        let attributes = |class: u16| {
            let response = respond(&search_attributes(&[uuid(class)], 512, &[0]), 672).unwrap();
            let count = u16::from_be_bytes([response[5], response[6]]) as usize;
            let (_, body, _) = element(&response[7..7 + count]).unwrap();
            let records = items(body).unwrap();
            assert_eq!(records.len(), 1, "{class:#06x}");
            items(records[0].1).unwrap().into_iter().map(|(_, v)| v.to_vec()).collect::<Vec<_>>()
        };
        for (class, classes, features) in [(UUID_AV_REMOTE_TARGET, vec![0x19, 0x11, 0x0c], 0x0001u16),
            (UUID_AV_REMOTE_CONTROLLER, vec![0x19, 0x11, 0x0e, 0x19, 0x11, 0x0f], 0x0002)] {
            let a = attributes(class);
            let value = |id: u16| a.iter().position(|v| *v == id.to_be_bytes()).map(|i| a[i + 1].clone()).unwrap();
            assert_eq!(value(0x0001), classes);
            // AVCTP 1.3 on PSM 0x17; AVRCP 1.4; category 1 (target) or 2 (controller).
            assert_eq!(value(0x0004), [0x35, 6, 0x19, 0x01, 0x00, 0x09, 0x00, 0x17, 0x35, 6, 0x19, 0x00, 0x17, 0x09, 0x01, 0x03]);
            assert_eq!(value(0x0009), [0x35, 6, 0x19, 0x11, 0x0e, 0x09, 0x01, 0x04]);
            assert_eq!(value(0x0311), features.to_be_bytes());
        }
    }

    #[test]
    fn device_id_request_and_response_round_trip() {
        let request = device_id_request(0x0d1d);
        assert_eq!(request, [0x06, 0x0d, 0x1d, 0, 15, 0x35, 3, 0x19, 0x12, 0x00, 0x01, 0x00, 0x35, 5, 0x0a, 0x02, 0x00, 0x02, 0x05, 0]);
        let id = DeviceId { source: 1, vendor: 0x1234, product: 0x5678, version: 0x0100 };
        let response = attribute_response(0x0d1d, &[device_id_attributes(id)]);
        assert_eq!(device_id(&response, 0x0d1d), Some(id));
        assert_eq!(device_id(&response, 0x0d1e), None, "another transaction");
        assert_eq!(device_id(&attribute_response(0x0d1d, &[]), 0x0d1d), None, "no record");
        let mut continued = response.clone();
        *continued.last_mut().unwrap() = 2;
        assert_eq!(device_id(&continued, 0x0d1d), None);
        // Our own server has no Device ID record: an empty answer.
        assert_eq!(device_id(&respond(&request, 672).unwrap(), 0x0d1d), None);
        let mut seed = 5u32;
        for _ in 0..3000 {
            let mut bad = response.clone();
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let at = (seed >> 8) as usize % bad.len();
            bad[at] = (seed >> 20) as u8;
            if seed & 1 == 1 { bad.truncate(at); }
            let _ = device_id(&bad, 0x0d1d);
        }
    }

    #[test]
    fn the_client_search_is_answered_by_the_server_and_parsed() {
        let response = respond(&search_request(0x0102, UUID_AUDIO_SOURCE), 672).unwrap();
        assert_eq!(search_count(&response, 0x0102), Some(1));
        assert_eq!(search_count(&respond(&search_request(7, UUID_AUDIO_SINK), 672).unwrap(), 7), Some(0));
        assert_eq!(search_count(&response, 0x0103), None, "another transaction");
        assert_eq!(search_count(&[0x01, 1, 2, 0, 2, 0, 3], 0x0102), None, "an error response");
        assert!(eir_lists(0x03, &[0x0a, 0x11, 0x0b, 0x11], UUID_AUDIO_SINK) && !eir_lists(0x03, &[0x0a, 0x11], UUID_AUDIO_SINK));
        let mut long = [0xfb, 0x34, 0x9b, 0x5f, 0x80, 0x00, 0x00, 0x80, 0x00, 0x10, 0x00, 0x00, 0x0b, 0x11, 0x00, 0x00];
        assert!(eir_lists(0x07, &long, UUID_AUDIO_SINK));
        long[0] = 0;
        assert!(!eir_lists(0x07, &long, UUID_AUDIO_SINK));
    }

    #[test]
    fn malformed_requests_get_an_error_response_and_never_panic() {
        let good = search_attributes(&[uuid(UUID_AUDIO_SOURCE)], 512, &[0]);
        let mut seed = 3u32;
        for _ in 0..5000 {
            let mut bad = good.clone();
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let at = (seed >> 8) as usize % bad.len();
            bad[at] = (seed >> 20) as u8;
            if seed & 1 == 1 { bad.truncate(at.max(1)); }
            if let Some(response) = respond(&bad, 672) { assert!(matches!(response[0], 0x01 | 0x07)); }
        }
        assert_eq!(respond(&[0x06, 0, 9, 0, 0], 672).unwrap(), [0x01, 0, 9, 0, 2, 0, 3]);
    }
}
