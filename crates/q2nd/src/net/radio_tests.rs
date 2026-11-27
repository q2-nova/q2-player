use super::radio::apply_radio;
use super::*;

/// A radio with its own published copy, as the monitor or a simulated
/// handle owns one.
fn new_radio(requested: bool) -> Radio { Radio::new(Shared::new(requested, WifiState::default())) }

#[test]
fn reconnect_between_status_polls_still_requests_dhcp_once() {
    let mut radio = new_radio(true);
    let mut observed = None;
    let initial = radio.intent();
    assert!(association_changed(&mut observed, initial.generation, true));
    assert!(!association_changed(&mut observed, initial.generation, true));
    // Neither the Off request nor the disconnected state is polled.
    radio.request(true, true);
    let resumed = radio.request(true, false);
    assert!(association_changed(&mut observed, resumed.generation, true));
    assert!(!association_changed(&mut observed, resumed.generation, true));
    assert!(!association_changed(&mut observed, resumed.generation, false));
    assert!(association_changed(&mut observed, resumed.generation, true));
}

#[test]
fn offline_enter_leave_preserves_requested_preference_and_restart_is_online() {
    for requested in [false, true] {
        let mut radio = new_radio(requested);
        let mut calls = Vec::new();
        let enter = radio.request(requested, true);
        apply_radio(&mut radio, enter, |on| { calls.push(on); true }, || panic!("no profile writes offline"));
        assert!(!radio.intent().enabled());
        assert_eq!(radio.intent().requested, requested);
        let leave = radio.request(requested, false);
        apply_radio(&mut radio, leave, |on| { calls.push(on); true }, || {});
        assert_eq!(calls, [false, requested]);
        assert_eq!(radio.intent().enabled(), requested);
        assert_eq!(new_radio(requested).intent().enabled(), requested, "runtime suppression never persists");
    }
}

#[test]
fn request_permutations_reject_stale_helpers_publication_and_faults() {
    let orders = [[0,1,2], [0,2,1], [1,0,2], [1,2,0], [2,0,1], [2,1,0]];
    // All 64 three-request histories, each with all six queued worker orders.
    for history in 0..64 {
        for order in orders {
            let mut radio = new_radio(false);
            let intents: Vec<_> = (0..3).map(|n| {
                let bits = (history >> (n * 2)) & 3;
                radio.request(bits & 1 != 0, bits & 2 != 0)
            }).collect();
            let mut calls = Vec::new();
            let mut configured = 0;
            for n in order {
                apply_radio(&mut radio, intents[n], |on| {calls.push(on); true}, || configured += 1);
            }
            assert_eq!(calls, [intents[2].enabled()]);
            assert_eq!(configured, usize::from(intents[2].enabled()));
            assert!(radio.failed(intents[0].generation).is_none());
            assert!(radio.failed(intents[1].generation).is_none());
            assert!(radio.take_fault().is_none());
            let mut published = false;
            assert!(!radio.publish(intents[0].generation, || published = true));
            assert!(!published);
            assert_eq!(radio.publish(intents[2].generation, || published = true), intents[2].enabled());
            assert_eq!(published, intents[2].enabled());
        }
    }
}

#[test]
fn failed_down_rolls_back_offline_and_restores_only_requested_wifi() {
    for requested in [false, true] {
        for restore_ok in [false, true] {
            let mut radio = new_radio(requested);
            let enter = radio.request(requested, true);
            let mut calls = Vec::new();
            apply_radio(&mut radio, enter, |on| { calls.push(on); on && restore_ok }, || {});
            assert_eq!(calls, if requested { vec![false, true] } else { vec![false] });
            assert!(!radio.intent().offline);
            assert_eq!(radio.intent().requested, requested);
            let (offline, message) = radio.take_fault().unwrap();
            assert!(!offline);
            assert_eq!(message.contains("restore failed"), requested && !restore_ok);
            assert!(radio.take_fault().is_none());
            assert!(radio.intent().error.is_some(), "persistent row fault survives toast consumption");
        }
    }
}

#[test]
fn actual_net_suppression_clears_stale_status_and_blocks_scan_until_explicit_enable() {
    let net = Net::new(PathBuf::from("unused-generated-helper"), true, Arc::new(|| {}));
    net.scan(); assert!(!net.state().scan.is_empty());
    net.set_offline_mode(true, true, &[]);
    net.scan();
    let state = net.state();
    assert!(!state.enabled); assert!(state.scan.is_empty()); assert!(state.ssid.is_none());
    assert_eq!(net.signal_bars(), 0);
    net.set_enabled(true, &[]);
    assert!(net.state().enabled); assert!(!net.shared.intent().offline);
}

#[test]
fn newer_request_clears_old_rollback_notice() {
    let mut radio = new_radio(true);
    let enter = radio.request(true, true);
    apply_radio(&mut radio, enter, |_| false, || {});
    radio.request(true, true);
    assert!(radio.take_fault().is_none());
    assert!(radio.intent().offline);
}

#[test]
fn doze_is_temporary_ends_on_any_explicit_choice_and_never_starts_from_off_or_offline() {
    let mut radio = new_radio(true);
    let mut calls = Vec::new();
    let doze = radio.doze(true).expect("saved-on radio dozes");
    assert!(radio.doze(true).is_none(), "already dozing: no new operation");
    apply_radio(&mut radio, doze, |on| { calls.push(on); true }, || panic!("no profile writes while dozing"));
    assert!(!radio.intent().enabled());
    assert!(radio.intent().requested, "the saved preference is untouched");
    let wake = radio.doze(false).unwrap();
    apply_radio(&mut radio, wake, |on| { calls.push(on); true }, || {});
    assert_eq!(calls, [false, true]);
    assert!(radio.intent().enabled());
    // Explicit Wi-Fi and Offline choices clear a doze.
    radio.doze(true).unwrap();
    assert!(!radio.request(true, true).doze);
    assert!(radio.doze(true).is_none(), "Offline never dozes");
    radio.request(false, false);
    assert!(radio.doze(true).is_none(), "Wi-Fi Off never dozes");
    assert!(!new_radio(true).intent().doze, "doze never persists");
}

#[test]
fn failed_doze_down_rolls_back_silently_and_restores_the_link() {
    let mut radio = new_radio(true);
    let doze = radio.doze(true).unwrap();
    let mut calls = Vec::new();
    apply_radio(&mut radio, doze, |on| { calls.push(on); on }, || {});
    assert_eq!(calls, [false, true], "down fails, then the link is brought back");
    assert!(!radio.intent().doze);
    assert!(radio.intent().enabled());
    assert!(radio.take_fault().is_none(), "nothing the owner asked for failed");
    // A stale doze completion cannot undo a newer explicit choice.
    let stale = radio.doze(true).unwrap();
    radio.request(true, false);
    apply_radio(&mut radio, stale, |_| panic!("stale doze must not run"), || {});
    assert!(radio.intent().enabled());
}

#[test]
fn boot_marker_follows_the_latest_request_and_not_doze() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build/test-wifi-autostart").join(std::process::id().to_string());
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let marker = dir.join("autostart");
    let net = Net::new(PathBuf::from("unused-generated-helper"), true, Arc::new(|| {})).with_autostart(marker.clone());
    assert!(!marker.exists());
    net.set_enabled(true, &[]);
    assert!(marker.exists(), "Wi-Fi On: init may start the supplicant at boot");
    net.set_doze(true);
    assert!(marker.exists(), "doze is temporary and leaves the owner's choice");
    net.set_offline_mode(true, true, &[]);
    assert!(!marker.exists(), "Offline Mode: no radio at boot");
    net.set_offline_mode(false, true, &[]);
    assert!(marker.exists());
    net.set_enabled(false, &[]);
    assert!(!marker.exists(), "Wi-Fi Off: no radio at boot");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn profile_digest_changes_with_any_sent_setting() {
    use crate::config::{WifiNetwork, WifiSecurity};
    let n = |psk: &str, security| WifiNetwork { ssid: "generated-ssid".into(), psk: psk.into(), security };
    let base = super::profile_digest(&[n("generated-pass-1", WifiSecurity::default())]);
    assert_eq!(base, super::profile_digest(&[n("generated-pass-1", WifiSecurity::default())]));
    assert_ne!(base, super::profile_digest(&[n("generated-pass-2", WifiSecurity::default())]));
    assert_ne!(base, super::profile_digest(&[]));
}
