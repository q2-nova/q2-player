//! Prefer a saved network's 5 GHz BSS. Associated playback measured 498–526 mW
//! on 5 GHz against 545–575 mW on 2437 MHz, and the supplicant's own band
//! choice does not prefer either.
//!
//! Once per associated BSS: if it is 2.4 GHz and the supplicant's scan lists
//! the same SSID on 5 GHz at `MIN_DBM` or better, roam to the strongest such
//! BSS. The supplicant forgets BSSs about 3 min after it last saw them (other
//! than the current one), so an old association has none: then one scan of
//! the 5 GHz channels is requested, at most once per `RETRY`, and the decision
//! waits `SCAN_WAIT` for its results. A target is not tried again for `RETRY`,
//! so a 5 GHz BSS the supplicant cannot hold does not cause a roam loop.

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Weakest 5 GHz signal worth moving to. band-steer-04: at -71 dBm the
/// 5 GHz link held for 20 min and drew 511-526 mW against 564-567 mW on
/// 2437 MHz (band-steer-03, where -70 kept the Q2 on 2.4 GHz). -72..-75 is
/// untested; a link that fails falls back through the supplicant, and RETRY
/// bounds repeat attempts.
pub const MIN_DBM: i32 = -75;
pub const RETRY: Duration = Duration::from_secs(30 * 60);
pub const SCAN_WAIT: Duration = Duration::from_secs(15);
/// The supplicant's `SCAN` with the 5 GHz channels only.
pub const SCAN_5GHZ: &str = "SCAN freq=4900-5900";

#[derive(Debug, PartialEq)]
pub enum Action {
    /// Nothing to decide yet (5 GHz already, decided, or a scan pending).
    None,
    /// Request `SCAN_5GHZ`.
    Scan,
    /// `ROAM` to this BSS.
    Roam(String),
    /// On 2.4 GHz and staying: the network's strongest 5 GHz signal, if seen.
    Stay(Option<i32>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Bss {
    pub bssid: String,
    pub freq: u32,
    /// dBm.
    pub signal: i32,
    pub ssid: String,
}

/// Every BSS in `SCAN_RESULTS` (bssid, frequency, signal, flags, ssid);
/// hidden networks are left out.
pub fn parse_bss(text: &str) -> Vec<Bss> {
    text.lines().skip(1).filter_map(|line| {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 5 || f[4].is_empty() { return None; }
        Some(Bss { bssid: f[0].to_string(), freq: f[1].parse().ok()?, signal: f[2].parse().ok()?, ssid: super::unescape(f[4]) })
    }).collect()
}

#[derive(Default)]
pub struct Steer {
    /// The associated BSS already decided on.
    checked: Option<String>,
    tried: HashMap<String, Instant>,
    /// When the last 5 GHz scan was requested.
    scanned: Option<Instant>,
}

impl Steer {
    /// Whether this association still needs a decision (reads the scan only then).
    pub fn wants(&self, bssid: &str) -> bool {
        self.checked.as_deref() != Some(bssid)
    }

    /// One decision per associated BSS, after at most one 5 GHz scan.
    pub fn check(&mut self, now: Instant, ssid: &str, bssid: &str, freq: u32, scan: &[Bss]) -> Action {
        if !self.wants(bssid) { return Action::None; }
        if freq >= 5000 {
            self.checked = Some(bssid.to_string());
            return Action::None;
        }
        let since_scan = self.scanned.map(|t| now.saturating_duration_since(t));
        if since_scan.is_some_and(|d| d < SCAN_WAIT) { return Action::None; }
        let five: Vec<&Bss> = scan.iter().filter(|b| b.ssid == ssid && b.freq >= 5000).collect();
        if five.is_empty() && since_scan.is_none_or(|d| d >= RETRY) {
            self.scanned = Some(now);
            return Action::Scan;
        }
        self.checked = Some(bssid.to_string());
        self.tried.retain(|_, t| now.saturating_duration_since(*t) < RETRY);
        let strongest = five.iter().map(|b| b.signal).max();
        match five.into_iter().filter(|b| b.signal >= MIN_DBM && !self.tried.contains_key(&b.bssid)).max_by_key(|b| b.signal) {
            Some(best) => {
                self.tried.insert(best.bssid.clone(), now);
                Action::Roam(best.bssid.clone())
            }
            None => Action::Stay(strongest),
        }
    }

    /// Not associated: the next association gets a fresh decision.
    pub fn reset(&mut self) {
        self.checked = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCAN: &str = "bssid / frequency / signal level / flags / ssid\n\
        aa:bb:cc:dd:ee:01\t2437\t-48\t[WPA2-PSK-CCMP][ESS]\tHome\n\
        aa:bb:cc:dd:ee:02\t5500\t-66\t[WPA2-PSK-CCMP][ESS]\tHome\n\
        aa:bb:cc:dd:ee:03\t5180\t-61\t[WPA2-PSK-CCMP][ESS]\tHome\n\
        aa:bb:cc:dd:ee:04\t5745\t-40\t[WPA2-PSK-CCMP][ESS]\tNeighbour\n\
        aa:bb:cc:dd:ee:05\t5200\t-30\t[ESS]\t\n";

    #[test]
    fn parses_every_named_bss() {
        let b = parse_bss(SCAN);
        assert_eq!(b.len(), 4, "the hidden BSS is left out");
        assert_eq!(b[1], Bss { bssid: "aa:bb:cc:dd:ee:02".into(), freq: 5500, signal: -66, ssid: "Home".into() });
    }

    #[test]
    fn moves_to_the_strongest_5ghz_bss_of_the_same_network() {
        let mut s = Steer::default();
        let t = Instant::now();
        assert_eq!(s.check(t, "Home", "aa:bb:cc:dd:ee:01", 2437, &parse_bss(SCAN)), Action::Roam("aa:bb:cc:dd:ee:03".into()));
        // One decision per associated BSS.
        assert!(!s.wants("aa:bb:cc:dd:ee:01"));
        assert_eq!(s.check(t, "Home", "aa:bb:cc:dd:ee:01", 2437, &parse_bss(SCAN)), Action::None);
    }

    #[test]
    fn stays_on_5ghz_and_ignores_weak_or_foreign_bss() {
        let mut s = Steer::default();
        let t = Instant::now();
        assert_eq!(s.check(t, "Home", "aa:bb:cc:dd:ee:03", 5180, &parse_bss(SCAN)), Action::None, "already 5 GHz");
        let weak = "h\nx1\t2412\t-50\t[ESS]\tCafe\nx2\t5180\t-79\t[ESS]\tCafe\nx3\t5745\t-40\t[ESS]\tOther\n";
        assert_eq!(s.check(t, "Cafe", "x1", 2412, &parse_bss(weak)), Action::Stay(Some(-79)), "below MIN_DBM, or another SSID");
    }

    #[test]
    fn a_failed_target_waits_out_the_retry_interval() {
        let mut s = Steer::default();
        let t = Instant::now();
        let scan = parse_bss(SCAN);
        assert_eq!(s.check(t, "Home", "aa:bb:cc:dd:ee:01", 2437, &scan), Action::Roam("aa:bb:cc:dd:ee:03".into()));
        // The roam failed and the supplicant came back to 2.4 GHz.
        s.reset();
        assert_eq!(s.check(t, "Home", "aa:bb:cc:dd:ee:01", 2437, &scan), Action::Roam("aa:bb:cc:dd:ee:02".into()), "next-best target");
        s.reset();
        assert_eq!(s.check(t + Duration::from_secs(60), "Home", "aa:bb:cc:dd:ee:01", 2437, &scan), Action::Stay(Some(-61)), "both tried recently");
        s.reset();
        assert_eq!(s.check(t + RETRY, "Home", "aa:bb:cc:dd:ee:01", 2437, &scan), Action::Roam("aa:bb:cc:dd:ee:03".into()));
    }

    #[test]
    fn an_old_association_scans_the_5ghz_channels_once() {
        let mut s = Steer::default();
        let t = Instant::now();
        // The supplicant has forgotten everything but the current BSS.
        let only_current = parse_bss("h\naa:bb:cc:dd:ee:01\t2437\t-48\t[ESS]\tHome\n");
        assert_eq!(s.check(t, "Home", "aa:bb:cc:dd:ee:01", 2437, &only_current), Action::Scan);
        assert!(s.wants("aa:bb:cc:dd:ee:01"), "undecided while the scan runs");
        assert_eq!(s.check(t + Duration::from_secs(3), "Home", "aa:bb:cc:dd:ee:01", 2437, &only_current), Action::None, "waits for results");
        assert_eq!(s.check(t + SCAN_WAIT, "Home", "aa:bb:cc:dd:ee:01", 2437, &parse_bss(SCAN)), Action::Roam("aa:bb:cc:dd:ee:03".into()));
        // No 5 GHz BSS found: decided, and no second scan within RETRY.
        let mut s = Steer::default();
        assert_eq!(s.check(t, "Home", "aa:bb:cc:dd:ee:01", 2437, &only_current), Action::Scan);
        assert_eq!(s.check(t + SCAN_WAIT, "Home", "aa:bb:cc:dd:ee:01", 2437, &only_current), Action::Stay(None));
        s.reset();
        assert_eq!(s.check(t + Duration::from_secs(600), "Home", "aa:bb:cc:dd:ee:01", 2437, &only_current), Action::Stay(None), "one scan per RETRY");
        s.reset();
        assert_eq!(s.check(t + RETRY, "Home", "aa:bb:cc:dd:ee:01", 2437, &only_current), Action::Scan);
    }
}
