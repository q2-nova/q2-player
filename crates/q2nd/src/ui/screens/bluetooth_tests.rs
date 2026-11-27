//! The Bluetooth pages' rows for each controller, link and pairing state.
use super::*;
use q2_bt::codec::Codec;
use q2_bt::host::Stream;

fn titles(rows: &[Row]) -> Vec<&str> { rows.iter().map(|r| r.title.as_str()).collect() }
fn row<'a>(rows: &'a [Row], title: &str) -> &'a Row { rows.iter().find(|r| r.title == title).unwrap_or_else(|| panic!("no {title} row in {:?}", titles(rows))) }

const NOW: u64 = 1_795_000_000;
const HEADPHONES: Addr = [1; 6];
const SPEAKER: Addr = [2; 6];

fn device(addr: Addr, name: &str, class: u32, rssi: i8, sink: bool) -> Device { Device { addr, name: name.into(), class, rssi, sink } }

fn ready() -> State {
    State { phase: Phase::Ready, paired: vec![device(HEADPHONES, "Generated Headphones", 0x24_0418, 0, false), device(SPEAKER, "Generated Speaker", 0x24_0414, 0, false)],
        seen: vec![(SPEAKER, NOW - 3 * 3600)], ldac: Some(crate::bluetooth::coder::LdacQuality::Standard), ..State::default() }
}

#[test]
fn the_bluetooth_page_shows_the_controller_then_paired_devices_and_their_state() {
    let mut state = State::default();
    assert_eq!(titles(&Bluetooth::rows(false, "auto", &state, NOW)), ["Bluetooth"]);
    // Starting: the switch says so; paired devices and pairing are offered, their requests wait.
    state.phase = Phase::Starting;
    let rows = Bluetooth::rows(true, "auto", &state, NOW);
    assert_eq!((titles(&rows), rows[0].detail.as_deref()), (vec!["Bluetooth", "Pair New Device", "Preferred Codec"], Some("Starting…")));
    // A failed start: the reason in words, and Try Again.
    state.phase = Phase::Failed("The Bluetooth controller does not answer".into());
    let rows = Bluetooth::rows(true, "auto", &state, NOW);
    assert_eq!((titles(&rows), rows[0].detail.as_deref()), (vec!["Bluetooth", "Try Again"], Some("The Bluetooth controller does not answer")));
    assert!(matches!(rows[1].action, RowAction::Cmd(RETRY_START)));

    let mut state = ready();
    let rows = Bluetooth::rows(true, "auto", &state, NOW);
    assert_eq!(titles(&rows), ["Bluetooth", "Generated Headphones", "Generated Speaker", "Pair New Device", "Preferred Codec"]);
    assert_eq!(rows[0].detail.as_deref(), Some("Ready"));
    assert_eq!((rows[1].value.as_deref(), rows[1].detail.as_deref()), (None, Some("Not connected yet")));
    assert_eq!((rows[2].value.as_deref(), rows[2].detail.as_deref()), (None, Some("Last connected 3 hours ago")));
    assert!(rows[1].separator && rows[3].separator && rows[1].chevron && rows[3].chevron);
    assert_eq!(row(&rows, "Preferred Codec").value.as_deref(), Some("Automatic"));

    // Connecting, step by step, then playing with the codec and its bit rate.
    state.link = Link::Connecting(HEADPHONES);
    for (step, words) in [(None, "Connecting…"), (Some(Step::Authentication), "Connecting…"), (Some(Step::Encryption), "Securing the connection…")] {
        state.step = step;
        assert_eq!(Bluetooth::rows(true, "auto", &state, NOW)[1].detail.as_deref(), Some(words), "{step:?}");
    }
    state.link = Link::Connected(HEADPHONES);
    state.step = None;
    assert_eq!(Bluetooth::rows(true, "auto", &state, NOW)[1].detail.as_deref(), Some("Setting up audio…"));
    state.link = Link::Playing(HEADPHONES);
    state.stream = Some(Stream { codec: Codec::Ldac { rate: 44100 }, mtu: 895 });
    let rows = Bluetooth::rows(true, "ldac", &state, NOW);
    assert_eq!((rows[1].value.as_deref(), rows[1].detail.as_deref()), (Some("Playing"), Some("LDAC · 606 kbit/s (Standard)")));
    assert_eq!(row(&rows, "Preferred Codec").value.as_deref(), Some("LDAC"));

    // A failed attempt: the notice in words, and Try Again names the device.
    let mut state = ready();
    state.notice = "Headphones not found. Turn them on, or put them in pairing mode.".into();
    state.attempt = Some((SPEAKER, false));
    let rows = Bluetooth::rows(true, "auto", &state, NOW);
    assert_eq!(titles(&rows)[..3], ["Bluetooth", "Last Problem", "Try Again"]);
    assert_eq!((rows[1].detail.as_deref(), rows[2].detail.as_deref()), (Some(state.notice.as_str()), Some("Connect to Generated Speaker")));
    assert!(matches!(rows[1].action, RowAction::None) && matches!(rows[2].action, RowAction::Cmd(RETRY_ATTEMPT)));
    // Not while another attempt runs.
    state.link = Link::Connecting(HEADPHONES);
    assert!(!titles(&Bluetooth::rows(true, "auto", &state, NOW)).contains(&"Try Again"));
}

#[test]
fn pair_new_device_lists_what_it_finds_and_shows_each_step_and_the_outcome() {
    let mut state = State { phase: Phase::Starting, discovering: true, ..State::default() };
    let rows = PairNew::rows(&state, None);
    assert_eq!((titles(&rows), rows[0].detail.as_deref()), (vec!["Starting Bluetooth…"], Some("The search begins when it is ready")));

    state.phase = Phase::Ready;
    state.paired = vec![device(HEADPHONES, "Generated Paired", 0x24_0418, 0, false)];
    state.found = vec![
        device([3; 6], "Generated Keyboard", 0x00_0540, -50, false),
        device([4; 6], "Generated Computer", 0x7c_0104, -58, true),
        device(HEADPHONES, "Generated Paired", 0x24_0418, -40, false),
        device([5; 6], "Generated Far Headphones", 0x24_0418, -80, false),
        device([6; 6], "Generated Speaker", 0x24_0414, -66, false),
    ];
    let rows = PairNew::rows(&state, None);
    assert_eq!(titles(&rows), ["Searching…", "Generated Speaker", "Generated Far Headphones", "Generated Computer", "Generated Keyboard"],
        "audio devices first, stronger first; paired ones are not offered");
    assert_eq!(rows[0].detail.as_deref(), Some("Put the headphones in pairing mode"));
    let shown: Vec<_> = rows[1..].iter().map(|r| (r.value.as_deref().unwrap(), r.detail.as_deref().unwrap())).collect();
    assert_eq!(shown, [("Good", "Speaker"), ("Weak", "Headphones"), ("Strong", "Takes Bluetooth audio"), ("Strong", "Other device")]);
    assert!(rows[1..].iter().all(|r| matches!(r.action, RowAction::Cmd(n) if n >= FOUND)) && matches!(rows[0].action, RowAction::None));

    // The chosen device's progress, in the status row and on its own row.
    let chosen = [5; 6];
    state.link = Link::Connecting(chosen);
    for (step, words) in [(Some(Step::Page), "Connecting…"), (Some(Step::Authentication), "Pairing…"), (Some(Step::Encryption), "Securing the connection…")] {
        state.step = step;
        let rows = PairNew::rows(&state, Some(chosen));
        assert_eq!((rows[0].title.as_str(), rows[0].detail.as_deref()), ("Pairing with Generated Far Headphones", Some(words)));
        assert_eq!(row(&rows, "Generated Far Headphones").value.as_deref(), Some("Pairing…"));
    }
    state.step = Some(Step::SinkCheck);
    state.link = Link::Connecting([4; 6]);
    assert_eq!(PairNew::rows(&state, Some([4; 6]))[0].detail.as_deref(), Some("Checking for Bluetooth audio…"));

    // A failure: what went wrong, and Try Again; the devices stay listed.
    state.link = Link::Idle;
    state.step = None;
    state.notice = "Pairing did not finish in time. Put the headphones in pairing mode and try again.".into();
    state.attempt = Some((chosen, false));
    let rows = PairNew::rows(&state, Some(chosen));
    assert_eq!(titles(&rows)[..2], ["Pairing Did Not Work", "Try Again"]);
    assert_eq!(rows[0].detail.as_deref(), Some(state.notice.as_str()));
    assert_eq!(rows.len(), 6);

    // The search over: Search Again.
    state.notice.clear();
    state.attempt = None;
    state.discovering = false;
    let rows = PairNew::rows(&state, None);
    assert_eq!((titles(&rows)[..2].to_vec(), rows[0].detail.as_deref()), (vec!["Search Finished", "Search Again"], None));
    state.found.clear();
    assert_eq!(PairNew::rows(&state, None)[0].detail.as_deref(), Some("No devices in pairing mode were found"));
    state.phase = Phase::Failed("Turn Wi-Fi on to use Bluetooth".into());
    assert_eq!(titles(&PairNew::rows(&state, None)), ["Bluetooth Is Not Available", "Try Again"]);
}

#[test]
fn a_paired_device_page_connects_disconnects_and_forgets() {
    let mut state = ready();
    let rows = DeviceInfo::rows(&state, SPEAKER, NOW);
    assert_eq!(titles(&rows), ["Connect", "Status", "Last Connected", "Type", "Forget This Device…"]);
    let values: Vec<_> = rows[1..4].iter().map(|r| r.value.as_deref().unwrap()).collect();
    assert_eq!(values, ["Not connected", "3 hours ago", "Speaker"]);
    assert!(matches!(rows[0].action, RowAction::Cmd(CONNECT)) && rows[4].chevron && rows[4].separator);
    state.phase = Phase::Starting;
    assert_eq!(DeviceInfo::rows(&state, SPEAKER, NOW)[0].detail.as_deref(), Some("Connects when Bluetooth is ready"));
    state.phase = Phase::Ready;
    state.link = Link::Connecting(SPEAKER);
    let rows = DeviceInfo::rows(&state, SPEAKER, NOW);
    assert_eq!((rows[0].title.as_str(), rows[0].detail.as_deref(), rows[1].value.as_deref()), ("Stop Connecting", Some("Connecting…"), Some("Connecting…")));
    state.link = Link::Connected(SPEAKER);
    state.stream = Some(Stream { codec: Codec::Aac { rate: 48000, bitrate: 256_000 }, mtu: 895 });
    let rows = DeviceInfo::rows(&state, SPEAKER, NOW);
    assert_eq!(titles(&rows), ["Disconnect", "Status", "Codec", "Type", "Forget This Device…"]);
    assert_eq!((rows[1].value.as_deref(), rows[2].value.as_deref(), rows[2].detail.as_deref()), (Some("Connected"), Some("48 kHz"), Some("AAC · 256 kbit/s")));
    state.codec_reason = "Changed from SBC, which the headphones chose".into();
    let rows = DeviceInfo::rows(&state, SPEAKER, NOW);
    assert_eq!((rows[3].title.as_str(), rows[3].detail.as_deref()), ("Codec Choice", Some("Changed from SBC, which the headphones chose")));
    // Another device's link does not show on this page.
    assert_eq!(DeviceInfo::rows(&state, HEADPHONES, NOW)[0].title, "Connect");
    state.paired.retain(|d| d.addr != SPEAKER);
    assert_eq!(titles(&DeviceInfo::rows(&state, SPEAKER, NOW)), ["No Longer Paired"]);
}

#[test]
fn bluetooth_details_show_state_and_counters_without_addresses_or_names() {
    let mut state = ready();
    state.controller = Some(q2_bt::host::Version { hci_version: 0x0b, hci_revision: 0x000f, lmp_version: 0x0b, manufacturer: 0x005d, lmp_subversion: 0x8723, rom_version: Some(2), already_patched: false });
    state.diagnostics.patched_version = Some((0xd5c8, 0x1143));
    // The transport and link readings come from the copy telemetry publishes.
    state.link_stats.h5_retransmissions = 3;
    state.link_stats.h5_resends = 2;
    state.link_stats.h5_bad_frames = 1;
    state.link_stats.credit_stalls = 4;
    state.link_stats.rssi = Some(-6);
    state.link_stats.link_quality = Some(250);
    state.counters = (5, 0, 7);
    state.link = Link::Connecting(HEADPHONES);
    state.step = Some(Step::Authentication);
    state.last_error = "Pairing did not finish in time. Put the headphones in pairing mode and try again.".into();
    state.waiting = 1;
    let rows = Details::rows(&state);
    assert_eq!(titles(&rows), ["Controller", "Firmware", "Version", "Link", "Codec", "Last Error", "Transport", "Commands", "Scans", "Audio", "Wi-Fi Power-Ups", "Waiting Requests"]);
    assert!(rows.iter().all(|r| matches!(r.action, RowAction::None)), "a reading page");
    assert_eq!((row(&rows, "Firmware").value.as_deref(), row(&rows, "Version").detail.as_deref()), (Some("Patch loaded"), Some("ROM 2 · revision 0xd5c8 · subversion 0x1143")));
    assert_eq!(row(&rows, "Link").value.as_deref(), Some("Authenticating"));
    assert_eq!(row(&rows, "Last Error").detail.as_deref(), Some(state.last_error.as_str()));
    assert_eq!(row(&rows, "Transport").detail.as_deref(), Some("H5 resent 3 · timeouts 2 · bad 1 · rejected 0"));
    assert_eq!(row(&rows, "Link").detail.as_deref(), Some("RSSI -6 · quality 250"));
    assert_eq!(row(&rows, "Audio").detail.as_deref(), Some("Dropped packets 5 · underruns 7 · stalls 4"));
    assert_eq!(row(&rows, "Waiting Requests").value.as_deref(), Some("1"));
    let text: String = rows.iter().flat_map(|r| [r.title.clone(), r.value.clone().unwrap_or_default(), r.detail.clone().unwrap_or_default()]).collect::<Vec<_>>().join("\n");
    for device in &state.paired {
        assert!(!text.contains(&device.name), "no device names");
        let hex: String = device.addr.iter().rev().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":");
        assert!(!text.to_lowercase().contains(&hex), "no addresses");
    }
}

#[test]
fn last_connected_reads_as_elapsed_time() {
    assert_eq!([0, 119, 120, 3599, 3600, 7200, 86_399, 86_400, 172_800, 900_000].map(|ago_s| ago(NOW, NOW - ago_s)),
        ["Just now", "Just now", "2 minutes ago", "59 minutes ago", "1 hour ago", "2 hours ago", "23 hours ago", "Yesterday", "2 days ago", "10 days ago"]);
    assert_eq!(ago(NOW, NOW + 60), "Just now", "a clock set back since");
}
