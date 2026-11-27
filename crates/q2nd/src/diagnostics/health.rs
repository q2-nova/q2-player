//! Explicit numeric observations only. No log strings, addresses or media data.
//! Admission captures context in diagnostics before delayed finite persistence.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum Metric {
    AppStart,
    FirstFrame,
    ScreenWakeRequested,
    InputToPresent,
    ResumeReturnToPresent,
    WifiAssociatedObserved,
    TailscaleReadyObserved,
    BadgeSelected,
    BadgeSelectedLimit,
    AudioError,
    AudioRecoverySucceeded,
    AudioRecoveryExhausted,
    ShuffleToAudio,
    ShuffleAlbumsToAudio,
    MusicPlayToAudio,
    SpokenPlayToAudio,
    ResumeToAudio,
    SkipToAudio,
    PageOpen,
    InfoOpen,
    /// KiB, at screen-off before the heap trim.
    MemoryResidentKib,
    MemoryAvailableKib,
    HeapFreeKib,
    HeapReleasedKib,
}
impl Metric {
    pub(crate) fn valid(self, value: u64) -> bool {
        match self {
            Self::AppStart | Self::FirstFrame | Self::ScreenWakeRequested => value == 0,
            // Preserve numeric microseconds; an out-of-domain observation is
            // rejected, never clamped into a plausible duration.
            Self::InputToPresent | Self::ResumeReturnToPresent | Self::ShuffleToAudio | Self::ShuffleAlbumsToAudio
            | Self::MusicPlayToAudio | Self::SpokenPlayToAudio | Self::ResumeToAudio | Self::SkipToAudio
            | Self::PageOpen | Self::InfoOpen |
            Self::WifiAssociatedObserved | Self::TailscaleReadyObserved => value <= 86_400_000_000,
            // 8 input bits, 4 provenance bits, badge enum 0..4. Selection is
            // before presentation; doze bit records intent, not RF residency.
            Self::BadgeSelected => value <= 0x4fff,
            Self::BadgeSelectedLimit => value == 1024,
            Self::AudioError | Self::AudioRecoverySucceeded | Self::AudioRecoveryExhausted => (1..=4095).contains(&value),
            Self::MemoryResidentKib | Self::MemoryAvailableKib | Self::HeapFreeKib | Self::HeapReleasedKib => value <= 1 << 20,
        }
    }
    fn from_metric(event: crate::metrics::Event) -> Option<Self> {
        use crate::metrics::Event as E;
        Some(match event {
            E::AppStart => Self::AppStart,
            E::FirstFrame => Self::FirstFrame,
            E::ScreenWakeRequested => Self::ScreenWakeRequested,
            E::ScreenWake => Self::InputToPresent,
            E::ResumePresent => Self::ResumeReturnToPresent,
            E::WifiAssociated => Self::WifiAssociatedObserved,
            E::TailscaleReady => Self::TailscaleReadyObserved,
            E::BadgeSelected => Self::BadgeSelected,
            E::BadgeSelectedLimit => Self::BadgeSelectedLimit,
            E::NumericOwner | E::TailscaleProbe(_) | E::LoggerShutdown(_) => return None,
        })
    }
}

pub(crate) fn observe(event: crate::metrics::Event, value: u64, time: u64) {
    if let Some(metric) = Metric::from_metric(event) {
        // Root admission owns validity, opt-in, context, 64-entry/64-per-minute
        // bounds and explicit loss accounting. Never perform I/O or wait here.
        let _ = super::admit_health(metric, value, time);
    }
}

impl Metric {
    /// Latency distributions, coalesced in the outbox.
    pub(crate) fn distribution(self) -> bool {
        matches!(self, Self::InputToPresent | Self::ResumeReturnToPresent | Self::ShuffleToAudio | Self::ShuffleAlbumsToAudio
            | Self::MusicPlayToAudio | Self::SpokenPlayToAudio | Self::ResumeToAudio | Self::SkipToAudio | Self::PageOpen | Self::InfoOpen)
    }
}

/// A successful UX journey's latency, as a mergeable distribution.
pub(crate) fn journey(metric: Metric, latency_us: u64) {
    let _ = super::admit_health(metric, latency_us, crate::playtrace::boottime_us());
}

/// The process and system memory at a screen-off heap trim.
pub(crate) fn memory(trim: &crate::platform::resident::Trim) {
    let time = crate::playtrace::boottime_us();
    for (metric, value) in [(Metric::MemoryResidentKib, trim.resident_kib), (Metric::MemoryAvailableKib, trim.available_kib),
                            (Metric::HeapFreeKib, trim.heap_free_kib), (Metric::HeapReleasedKib, trim.released_kib)] {
        let _ = super::admit_health(metric, value.min(1 << 20), time);
    }
}

/// Fixed numeric errno only, through the existing bounded opt-in outbox.
pub(crate) fn audio_error(code: i32) {
    // An underrun also goes to the UX flight recorder, next to the track
    // opens, stream waits and fetch-ahead that explain it.
    if code.unsigned_abs() == libc::EPIPE.unsigned_abs() { crate::ux::underrun(libc::EPIPE); }
    let _ = super::admit_health(Metric::AudioError, u64::from(code.unsigned_abs()), crate::playtrace::boottime_us());
}

/// Successful accepted PCM or bounded recovery exhaustion; never error strings.
pub(crate) fn audio_recovery(code: i32, recovered: bool) {
    let metric = if recovered { Metric::AudioRecoverySucceeded } else { Metric::AudioRecoveryExhausted };
    let _ = super::admit_health(metric, u64::from(code.unsigned_abs()), crate::playtrace::boottime_us());
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn health_allowlist_excludes_logs_probes_and_arbitrary_strings() {
        use crate::metrics::{Event as E, LoggerField, ProbePhase};
        for event in [E::NumericOwner, E::TailscaleProbe(ProbePhase::Error), E::LoggerShutdown(LoggerField::Outcome)] {
            assert_eq!(Metric::from_metric(event), None);
        }
        for name in ["ssid", "server-url", "song-title", "tailscale-probe-error", "logger-shutdown-outcome"] {
            assert!(serde_json::from_str::<Metric>(&format!("\"{name}\"")).is_err());
        }
        assert_eq!(serde_json::to_string(&Metric::FirstFrame).unwrap(), "\"first-frame\"");
        assert_eq!(Metric::from_metric(E::ScreenWake), Some(Metric::InputToPresent));
    }
    #[test]
    fn health_bounds_preserve_units_and_badge_domain() {
        for metric in [Metric::AppStart, Metric::FirstFrame, Metric::ScreenWakeRequested] {
            assert!(metric.valid(0)); assert!(!metric.valid(1));
        }
        for metric in [Metric::InputToPresent, Metric::ResumeReturnToPresent,
                       Metric::WifiAssociatedObserved, Metric::TailscaleReadyObserved] {
            assert!(metric.valid(86_400_000_000)); assert!(!metric.valid(86_400_000_001));
            assert!(!metric.valid(u64::MAX));
        }
        assert!(Metric::AudioError.valid(77)); assert!(!Metric::AudioError.valid(0)); assert!(!Metric::AudioError.valid(4096));
        assert!(Metric::BadgeSelected.valid(0x4fff)); assert!(!Metric::BadgeSelected.valid(0x5000));
        assert!(Metric::BadgeSelectedLimit.valid(1024)); assert!(!Metric::BadgeSelectedLimit.valid(1023));
        assert!(Metric::HeapFreeKib.valid(1 << 20)); assert!(!Metric::HeapFreeKib.valid((1 << 20) + 1));
        assert_eq!(serde_json::to_string(&Metric::HeapReleasedKib).unwrap(), "\"heap-released-kib\"");
    }
}
