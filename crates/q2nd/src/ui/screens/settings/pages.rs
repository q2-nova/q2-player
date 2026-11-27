//! Settings destinations and their top-level grouping on all platforms.
use crate::config::options;
use crate::ui::{
    Action, Ctx, Dest, Screen,
    list::{Row, RowAction},
    screens::controls::Choice,
};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Root,
    About,
    SystemInfo,
    Server,
    Audiobookshelf,
    Playback,
    AutoSleep,
    ListenBrainz,
    LastFm,
    Storage,
    Sound,
    Display,
    Connections,
    System,
    VolumeLimit,
    DateTime,
    TimeZone,
    Brightness,
}

impl Page {
    pub fn tall_rows(self) -> bool {
        matches!(
            self,
            Self::Server
                | Self::About
                | Self::SystemInfo
                | Self::Sound
                | Self::DateTime
                | Self::AutoSleep
        )
    }

    pub fn title(self) -> &'static str {
        match self {
            Page::Root => "Settings",
            Page::About => "About",
            Page::SystemInfo => "System Information",
            Page::Server => "Navidrome Server",
            Page::Audiobookshelf => "Audiobookshelf",
            Page::Playback => "Playback",
            Page::ListenBrainz => "ListenBrainz",
            Page::LastFm => "Last.fm",
            Page::AutoSleep => "Auto Sleep Timer",
            Page::Storage => "Storage",
            Page::Sound => "Sound",
            Page::Display => "Display",
            Page::Connections => "Connections",
            Page::System => "System",
            Page::VolumeLimit => "Volume Limit",
            Page::DateTime => "Date & Time",
            Page::TimeZone => "Time Zone",
            Page::Brightness => "Brightness",
        }
    }
}

pub fn root_rows(
    eq_on: bool,
    brightness: u8,
    wifi: &str,
    bluetooth: String,
    bt_dest: Dest,
) -> Vec<Row> {
    let link = |page: Page| Row::link(page.title(), Dest::SettingsPage(page));
    vec![
        link(Page::Sound).value(if eq_on { "EQ On" } else { "EQ Off" }),
        link(Page::Playback),
        link(Page::Display).value(format!("{brightness}%")),
        link(Page::Connections).value(wifi),
        Row::link("Bluetooth", bt_dest).value(bluetooth),
        link(Page::Storage),
        link(Page::System),
    ]
}

pub fn about_rows(app: &str, os: &str, report_command: Option<u32>) -> Vec<Row> {
    let (system, report) = match report_command {
        Some(cmd) => (
            Row::link("System Information", Dest::SettingsPage(Page::SystemInfo)),
            Row::new("Report a Problem", RowAction::Cmd(cmd)),
        ),
        None => (
            Row::new("System Information", RowAction::None).dim(true),
            Row::new("Report a Problem", RowAction::None).dim(true),
        ),
    };
    vec![
        Row::new("Q2 Navidrome", RowAction::None).value(app),
        Row::new("OS Version", RowAction::None).value(os),
        system.separated(),
        report.key("report-problem"),
    ]
}

/// An inactive timer checks Off. An active timer with an unknown period has no check.
pub fn sleep_timer_choice(
    period: Option<Duration>,
    apply: impl Fn(Option<Duration>, &mut Ctx) + 'static,
) -> Box<dyn Screen> {
    let minutes = options::AUTO_SLEEP_DURATIONS;
    let current = period.and_then(|period| {
        minutes
            .iter()
            .position(|&m| u64::from(m) * 60 == period.as_secs())
    });
    let labels = minutes.map(|m| options::minutes_label(u32::from(m))).into();
    Box::new(Choice::new(
        "Sleep Timer",
        labels,
        current,
        Box::new(move |i, ctx| {
            let m = minutes[i];
            let period = (m != 0).then(|| Duration::from_secs(u64::from(m) * 60));
            apply(period, ctx);
            let message = if m == 0 {
                "Sleep timer off".into()
            } else {
                format!("Pausing in {m} minutes")
            };
            Action::Many(vec![Action::Pop, Action::Toast(message)])
        }),
    ))
}

pub const SOUND_CHECK: u32 = 13;
pub const GAIN: u32 = 17;
pub const FILTER: u32 = 18;

pub fn volume_limit(value: u8) -> String {
    if value >= 100 {
        "Off".into()
    } else {
        format!("{value}%")
    }
}

pub fn sound_rows(
    eq: String,
    normalization: options::ReplayGainMode,
    limit: u8,
    power: options::HeadphonePower,
    filter: u8,
) -> Vec<Row> {
    vec![
        Row::link("Equalizer", Dest::Equalizer).detail(eq),
        Row::screen("Volume Normalization", SOUND_CHECK)
            .key("Volume Normalization")
            .value(normalization.label()),
        Row::link("Volume Limit", Dest::SettingsPage(Page::VolumeLimit)).value(volume_limit(limit)),
        Row::screen("Headphone Power", GAIN)
            .key("Headphone Power")
            .value(power.label())
            .detail(power.detail()),
        Row::screen("DAC Filter", FILTER)
            .key("DAC Filter")
            .value(options::DAC_FILTERS[usize::from(filter.min(3))]),
    ]
}

/// Shared geometry and presentation of the two percentage sliders.
pub fn draw_level(
    d: &mut crate::ui::Draw,
    area: crate::gfx::Rect,
    value: u8,
    limit: bool,
) -> crate::gfx::Rect {
    use crate::gfx::{Rect, Weight};
    let cx = area.x + area.w / 2;
    let label = if limit {
        volume_limit(value)
    } else {
        format!("{value}%")
    };
    d.fonts
        .draw_centered(d.c, &label, cx, area.y + 78, 30, Weight::Bold, d.theme.text);
    let bar = Rect::new(45, area.y + 137, area.w - 90, 18);
    crate::ui::progress_bar(d.c, &d.theme, bar, u64::from(value), 100);
    d.fonts.draw_centered(
        d.c,
        "Wheel: adjust · Center: done",
        cx,
        area.y + 180,
        16,
        Weight::Regular,
        d.theme.muted,
    );
    bar
}
