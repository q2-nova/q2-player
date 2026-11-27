//! Native player setting values and labels shared with the browser demo.
use serde::{Deserialize, Serialize};

/// Stream and download quality.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    /// The original file, untouched.
    #[default]
    Original,
    /// MP3 at this many kbit/s, transcoded by the server.
    #[serde(rename = "mp3-320")]
    Mp3_320,
    #[serde(rename = "mp3-192")]
    Mp3_192,
    #[serde(rename = "mp3-128")]
    Mp3_128,
}

impl Quality {
    pub const ALL: [Quality; 4] = [
        Quality::Original,
        Quality::Mp3_320,
        Quality::Mp3_192,
        Quality::Mp3_128,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Quality::Original => "Original",
            Quality::Mp3_320 => "MP3 320",
            Quality::Mp3_192 => "MP3 192",
            Quality::Mp3_128 => "MP3 128",
        }
    }

    pub fn bit_rate(self) -> Option<u32> {
        match self {
            Quality::Original => None,
            Quality::Mp3_320 => Some(320),
            Quality::Mp3_192 => Some(192),
            Quality::Mp3_128 => Some(128),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ReplayGainMode {
    #[default]
    Off,
    Track,
    Album,
}

/// The headphone amplifiers' operating point: the CS43131s' full-scale
/// output and class-H rail mode (kernel patch 0187; q2-os docs/hardware.md,
/// "Audio"). A higher level drives demanding
/// headphones louder and costs more power. Efficient plays each volume
/// step as Standard up to its own maximum; High and Maximum are 4.8 and
/// 6 dB louder at every step.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum HeadphonePower {
    /// 0.5 Vrms full scale on fixed ±VCP (1.8 V) rails: the amplifiers never
    /// switch to the higher-voltage rail.
    #[default]
    Efficient,
    /// 1 Vrms, class H adapting to the signal (2.6 V upper rail): the
    /// driver's low-impedance level, the default from V0.47.
    Standard,
    /// 1.73 Vrms with the 3.0 V upper rail: the codecs' reset level, the
    /// earlier High gain.
    High,
    /// 2 Vrms (+1 dB on High): the stock firmware's level.
    Maximum,
}

impl HeadphonePower {
    pub const ALL: [HeadphonePower; 4] = [
        HeadphonePower::Efficient,
        HeadphonePower::Standard,
        HeadphonePower::High,
        HeadphonePower::Maximum,
    ];

    pub fn label(self) -> &'static str {
        match self {
            HeadphonePower::Efficient => "Efficient",
            HeadphonePower::Standard => "Standard",
            HeadphonePower::High => "High",
            HeadphonePower::Maximum => "Maximum",
        }
    }

    /// The trade, one line.
    pub fn detail(self) -> &'static str {
        match self {
            HeadphonePower::Efficient => "Least power; up to 0.5 V, loudest from volume 86",
            HeadphonePower::Standard => "Up to 1 V, for most headphones",
            HeadphonePower::High => "Up to 1.7 V, for demanding headphones",
            HeadphonePower::Maximum => "Up to 2 V, as stock; most power",
        }
    }
}

impl ReplayGainMode {
    pub const ALL: [Self; 3] = [Self::Off, Self::Track, Self::Album];

    pub fn label(self) -> &'static str {
        match self {
            Self::Off => "Off",
            Self::Track => "Track",
            Self::Album => "Album",
        }
    }
}

pub const DEFAULT_STREAM_QUALITY: Quality = Quality::Mp3_320;
pub const DEFAULT_DOWNLOAD_QUALITY: Quality = Quality::Original;
pub const DEFAULT_REPLAY_GAIN: ReplayGainMode = ReplayGainMode::Off;
pub const DAC_FILTERS: [&str; 4] = ["Sharp", "Slow", "Short Sharp", "Short Slow"];
pub const DEFAULT_DAC_FILTER: u8 = 3;

pub const SKIP_SECONDS: [u32; 9] = [5, 10, 15, 30, 45, 60, 90, 120, 300];
pub const DEFAULT_SKIP_FORWARD: u32 = 30;
pub const DEFAULT_SKIP_BACKWARD: u32 = 15;
pub const DEFAULT_SPOKEN_SPEED: u16 = 100;
pub const AUTO_SLEEP_DURATIONS: [u16; 9] = [0, 5, 10, 15, 30, 45, 60, 90, 120];
pub const DEFAULT_AUTO_SLEEP_DURATION: u16 = 0;
pub const DEFAULT_AUTO_SLEEP_START: u16 = 22 * 60;
pub const DEFAULT_AUTO_SLEEP_END: u16 = 7 * 60;
pub const CACHE_LIMITS: [u64; 6] = [1024, 2048, 4096, 8192, 16384, 32768];
pub const DEFAULT_CACHE_LIMIT: u64 = 4096;
pub const FETCH_AHEAD: [u32; 5] = [0, 1, 2, 3, 5];
pub const DEFAULT_FETCH_AHEAD: u32 = 2;
pub const BACKLIGHT_TIMEOUTS: [u32; 7] = [10, 20, 30, 60, 120, 300, 0];
pub const DEFAULT_BACKLIGHT_TIMEOUT: u32 = 30;
pub const IDLE_TIMEOUTS: [u32; 5] = [5, 10, 30, 60, 0];
pub const DEFAULT_IDLE_TIMEOUT: u32 = 10;

pub fn backlight_label(seconds: u32) -> String {
    match seconds {
        0 => "Always On".into(),
        seconds if seconds < 60 => format!("{seconds} Seconds"),
        60 => "1 Minute".into(),
        seconds => format!("{} Minutes", seconds / 60),
    }
}

pub fn cache_limit_label(mb: u64) -> String {
    if mb >= 1024 {
        format!("{:.1} GB", mb as f64 / 1024.0)
    } else {
        format!("{mb} MB")
    }
}

pub fn clock_label(minutes: u16) -> String {
    format!("{:02}:{:02}", (minutes / 60) % 24, minutes % 60)
}

pub fn minutes_label(minutes: u32) -> String {
    if minutes == 0 {
        "Off".into()
    } else {
        format!("{minutes} Minutes")
    }
}
