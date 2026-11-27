//! Browser settings adapt the native player’s controls to tab-local device state.
use super::*;
use crate::config::{Accent, Look};
use crate::ui::screens::controls::{Choice, Confirm};

#[path = "../../../q2nd/src/ui/screens/settings/pages.rs"]
mod pages;
pub use pages::Page;

fn toggle(label: &'static str, cmd: u32, default: bool, ctx: &Ctx) -> Row {
    set(label, cmd).value(on_off(selected(ctx, label, usize::from(default)) != 0))
}

struct ChoiceOptions {
    labels: Vec<String>,
    default: usize,
}

fn option_labels<T: Copy + PartialEq>(
    values: &[T],
    default: T,
    label: impl Fn(T) -> String,
) -> ChoiceOptions {
    ChoiceOptions {
        labels: values.iter().copied().map(label).collect(),
        default: values
            .iter()
            .position(|&value| value == default)
            .expect("default offered in picker"),
    }
}

fn choice(page: &str) -> Option<ChoiceOptions> {
    use options::{HeadphonePower, Quality, ReplayGainMode};
    Some(match page {
        "Volume Normalization" => {
            option_labels(&ReplayGainMode::ALL, options::DEFAULT_REPLAY_GAIN, |mode| {
                mode.label().into()
            })
        }
        "Headphone Power" => {
            option_labels(&HeadphonePower::ALL, HeadphonePower::default(), |level| {
                level.label().into()
            })
        }
        "DAC Filter" => ChoiceOptions {
            labels: options::DAC_FILTERS.map(String::from).into(),
            default: usize::from(options::DEFAULT_DAC_FILTER),
        },
        "Song Ratings" => {
            option_labels(&RatingControls::ALL, RatingControls::default(), |rating| {
                rating.label().into()
            })
        }
        "Skip Forward" | "Skip Backward" => option_labels(
            &options::SKIP_SECONDS,
            if page == "Skip Forward" {
                options::DEFAULT_SKIP_FORWARD
            } else {
                options::DEFAULT_SKIP_BACKWARD
            },
            |seconds| format!("{seconds} Seconds"),
        ),
        "Streaming Quality" | "Download Quality" => option_labels(
            &Quality::ALL,
            if page == "Streaming Quality" {
                options::DEFAULT_STREAM_QUALITY
            } else {
                options::DEFAULT_DOWNLOAD_QUALITY
            },
            |quality| quality.label().into(),
        ),
        "Duration" => option_labels(
            &options::AUTO_SLEEP_DURATIONS,
            options::DEFAULT_AUTO_SLEEP_DURATION,
            |minutes| options::minutes_label(u32::from(minutes)),
        ),
        "Backlight Timeout" => option_labels(
            &options::BACKLIGHT_TIMEOUTS,
            options::DEFAULT_BACKLIGHT_TIMEOUT,
            options::backlight_label,
        ),
        "Mix Size" => option_labels(&(10..=90).step_by(5).collect::<Vec<u8>>(), 50,
            |percent| format!("{percent}% of card")),
        "Song Cache Limit" => option_labels(
            &options::CACHE_LIMITS,
            options::DEFAULT_CACHE_LIMIT,
            options::cache_limit_label,
        ),
        "Fetch Ahead" => option_labels(
            &options::FETCH_AHEAD,
            options::DEFAULT_FETCH_AHEAD,
            |songs| format!("{songs} Songs"),
        ),
        "Sleep When Idle" => option_labels(
            &options::IDLE_TIMEOUTS,
            options::DEFAULT_IDLE_TIMEOUT,
            options::minutes_label,
        ),
        "Auto Timer Starts" | "Auto Timer Ends" => {
            let default = if page == "Auto Timer Starts" {
                options::DEFAULT_AUTO_SLEEP_START
            } else {
                options::DEFAULT_AUTO_SLEEP_END
            };
            ChoiceOptions {
                labels: (0..48)
                    .map(|half_hours| options::clock_label(half_hours * 30))
                    .collect(),
                default: usize::from(default / 30),
            }
        }
        _ => return None,
    })
}

fn choice_value(page: &'static str, ctx: &Ctx) -> String {
    let choice = choice(page).expect("registered choice");
    choice.labels[selected(ctx, page, choice.default)].clone()
}

fn choice_link(label: &str, page: &'static str, ctx: &Ctx) -> Row {
    link(label, page).value(choice_value(page, ctx))
}

fn wifi_status(ctx: &Ctx) -> &'static str {
    if !ctx.demo.wireless {
        "Off"
    } else if ctx.demo.network.is_some() {
        "Connected"
    } else {
        "Not Connected"
    }
}

fn service_status(ctx: &Ctx, shelf: bool) -> &'static str {
    if !service_configured(ctx, shelf) {
        "Not Set"
    } else if ctx.demo.offline {
        "Offline"
    } else if wifi_status(ctx) == "Connected" {
        "Connected"
    } else {
        "Unavailable"
    }
}

fn hidden_field(ctx: &Ctx, id: &str) -> &'static str {
    if field(ctx, id, "demo").is_empty() {
        "Not set"
    } else {
        "••••••"
    }
}

pub(super) fn rows(page: Page, ctx: &Ctx) -> Vec<Row> {
    use Page::*;
    let settings = |label: &str, page| Row::link(label, Dest::SettingsPage(page));
    match page {
        Root => pages::root_rows(
            ctx.demo.eq.graphic_enabled,
            ctx.demo.brightness,
            wifi_status(ctx),
            if ctx.demo.bluetooth && ctx.demo.paired {
                "Connected"
            } else {
                on_off(ctx.demo.bluetooth)
            }
            .into(),
            Dest::Bluetooth,
        ),
        Sound => pages::sound_rows(
            ctx.demo.eq.summary(),
            options::ReplayGainMode::ALL[selected(ctx, "Volume Normalization", 0)],
            ctx.demo.volume_limit,
            headphone_power(ctx),
            selected(ctx, "DAC Filter", usize::from(options::DEFAULT_DAC_FILTER)) as u8,
        ),
        Playback => vec![
            settings("Auto Sleep Timer", AutoSleep).value(choice_value("Duration", ctx)),
            toggle("Gapless", 12, true, ctx),
            toggle("Pause on Unplug", 19, true, ctx),
            choice_link("Song Ratings", "Song Ratings", ctx),
            link("Default Spoken Speed", "Default Speed")
                .value(speed_label(ctx.demo.speed_percent))
                .separated(),
            set("Book Skip Buttons", 72).value(if selected(ctx, "Book Skip Buttons", 0) == 0 {
                "Time"
            } else {
                "One Chapter"
            }),
            choice_link("Skip Forward", "Skip Forward", ctx),
            choice_link("Skip Backward", "Skip Backward", ctx),
            choice_link("Streaming Quality", "Streaming Quality", ctx).separated(),
            choice_link("Download Quality", "Download Quality", ctx),
            toggle("Navidrome Scrobbles", 14, true, ctx).detail("Library music only"),
            settings("ListenBrainz", ListenBrainz).value(on_off(selected(ctx, "ListenBrainz", 0) != 0)),
            settings("Last.fm", LastFm).value(on_off(selected(ctx, "Last.fm", 0) != 0)),
            toggle("Sync Queue", 15, true, ctx),
        ],
        AutoSleep => vec![
            choice_link("Duration", "Duration", ctx).detail("Audiobooks and podcasts only"),
            choice_link("Auto Timer Starts", "Auto Timer Starts", ctx),
            choice_link("Auto Timer Ends", "Auto Timer Ends", ctx),
        ],
        Display => vec![
            settings("Brightness", Brightness).value(format!("{}%", ctx.demo.brightness)),
            choice_link("Backlight Timeout", "Backlight Timeout", ctx),
            set("Night Warmth", 36).value(if selected(ctx, "Night Warmth", 0) == 0 {
                "Off"
            } else {
                "21–07"
            }),
            set("Theme", 33).value(if *ctx.look == Look::Classic {
                "Classic"
            } else {
                "Dark"
            }),
            link("Selection Color", "Selection Color").value(ctx.accent.label()),
            set("Time in Title", 35).value(on_off(ctx.demo.clock_in_title)),
        ],
        Connections => vec![
            Row::link("Wi-Fi", Dest::Wifi).value(wifi_status(ctx)),
            settings("Navidrome Server", Server).value(service_status(ctx, false)),
            settings("Audiobookshelf", Audiobookshelf).value(service_status(ctx, true)),
            Row::link("Tailscale", Dest::Tailscale).value(if ctx.demo.tailscale {
                "Needs Sign In"
            } else {
                "Off"
            }),
            set("Offline Mode", 50).value(on_off(ctx.demo.offline)),
        ],
        Server => vec![
            link("Address", "Server Address").detail(field(
                ctx,
                "Server Address",
                "https://music.example.test",
            )),
            link("User", "User").detail(field(ctx, "User", "listener")),
            link("Password", "Password").value(hidden_field(ctx, "Password")),
            set("Test Connection", 4),
            set("Scan Library", 5),
            info("Or edit config.toml").detail("in Q2Navidrome on the card"),
        ],
        Audiobookshelf => vec![
            link("Address", "Audiobookshelf Address").detail(field(
                ctx,
                "Audiobookshelf Address",
                "https://books.example.test",
            )),
            link("API Token", "API Token").value(hidden_field(ctx, "API Token")),
            set("Test Connection", 74).value(service_status(ctx, true)),
            set("Sync Now", 81).detail("New and removed books and episodes"),
            set("Full Resync", 82).detail("Everything again; also daily while charging"),
            info("Create an API key in Audiobookshelf").detail("Settings → Users → API keys"),
        ],
        ListenBrainz => vec![
            set("Scrobbling", 91).value(on_off(selected(ctx, "ListenBrainz", 0) != 0)).detail("Music and radio"),
            link("API Token", "ListenBrainz API Token").detail(if field(ctx, "ListenBrainz API Token", "").is_empty() { "From listenbrainz.org" } else { "Saved" }),
            link("Client Contact URL", "Client Contact URL").detail("HTTPS maintainer contact"),
            info("Status").detail(if selected(ctx, "ListenBrainz", 0) == 0 { "Off" } else { "Simulated" }),
        ],
        LastFm => vec![
            set("Scrobbling", 94).value(on_off(selected(ctx, "Last.fm", 0) != 0)).detail("Music and radio"),
            info("Status").detail(if selected(ctx, "Last.fm", 0) == 0 { "Off" } else { "Setup Needed" }),
            info("Edit config.toml").detail("Add authorized Last.fm settings on the card"),
        ],
        Storage => {
            let mut rows = vec![
                Row::link("Storage Usage", Dest::StorageUsage),
                toggle("Offline Mix", 90, false, ctx).detail("Rotates music, audiobooks and podcasts"),
            ];
            if selected(ctx, "Offline Mix", 0) != 0 {
                rows.push(choice_link("Mix Size", "Mix Size", ctx));
            }
            rows.extend([
            choice_link("Song Cache Limit", "Song Cache Limit", ctx)
                .detail("Streamed songs only, not downloads"),
            choice_link("Fetch Ahead", "Fetch Ahead", ctx),
            link("Clear Song Cache…", "Clear Song Cache?"),
            info("Cover Art").detail("Kept on the card, with no time or size limit"),
            ]);
            rows
        }
        System => vec![
            toggle("Clicker", 41, true, ctx),
            choice_link("Sleep When Idle", "Sleep When Idle", ctx),
            settings("Date & Time", DateTime),
            settings("About", About).separated(),
        ],
        DateTime => vec![
            settings("Time Zone", TimeZone).detail(timezones::label(ctx.demo.timezone)),
            toggle("24-Hour Clock", 34, false, ctx),
        ],
        TimeZone => timezones::rows(ctx.demo.timezone),
        About => pages::about_rows(env!("Q2_NATIVE_APP_VERSION"), "Web Demo", None),
        Brightness | VolumeLimit | SystemInfo => vec![],
    }
}

fn headphone_power(ctx: &Ctx) -> options::HeadphonePower {
    let levels = options::HeadphonePower::ALL;
    let default = levels
        .iter()
        .position(|&level| level == options::HeadphonePower::default())
        .unwrap();
    levels[selected(ctx, "Headphone Power", default)]
}

fn flip(ctx: &mut Ctx, key: &'static str, default: bool) -> Action {
    let value = selected(ctx, key, usize::from(default));
    ctx.demo.values.insert(key, usize::from(value == 0));
    Action::None
}

pub(super) fn command(page: Page, cmd: u32, ctx: &mut Ctx) -> Action {
    match page {
        Page::ListenBrainz if cmd == 91 => flip(ctx, "ListenBrainz", false),
        Page::LastFm if cmd == 94 => flip(ctx, "Last.fm", false),
        Page::Storage if cmd == 90 => flip(ctx, "Offline Mix", false),
        Page::Sound => {
            let page = match cmd {
                pages::SOUND_CHECK => "Volume Normalization",
                pages::GAIN => "Headphone Power",
                pages::FILTER => "DAC Filter",
                _ => return Action::None,
            };
            Action::Push(crate::ui::screens::open(Dest::FixturePage(page), ctx))
        }
        Page::Playback => match cmd {
            12 => flip(ctx, "Gapless", true),
            19 => flip(ctx, "Pause on Unplug", true),
            72 => flip(ctx, "Book Skip Buttons", false),
            14 => flip(ctx, "Navidrome Scrobbles", true),
            15 => flip(ctx, "Sync Queue", true),
            _ => Action::None,
        },
        Page::Display => match cmd {
            33 => {
                *ctx.look = if *ctx.look == Look::Classic {
                    Look::Dark
                } else {
                    Look::Classic
                };
                Action::None
            }
            35 => {
                ctx.demo.clock_in_title = !ctx.demo.clock_in_title;
                Action::None
            }
            36 => flip(ctx, "Night Warmth", false),
            _ => Action::None,
        },
        Page::Connections if cmd == 50 => {
            ctx.demo.offline = !ctx.demo.offline;
            Action::None
        }
        Page::Server => service_command(cmd, false, ctx),
        Page::Audiobookshelf => service_command(cmd, true, ctx),
        Page::System if cmd == 41 => flip(ctx, "Clicker", true),
        Page::DateTime if cmd == 34 => flip(ctx, "24-Hour Clock", false),
        Page::TimeZone => timezones::change(cmd, ctx.demo.timezone, |ctx, id| {
            ctx.demo.timezone = id;
            Action::Restart
        }),
        _ => Action::None,
    }
}

fn service_command(cmd: u32, shelf: bool, ctx: &Ctx) -> Action {
    let message = if !ctx.demo.online() {
        if shelf {
            "Can't reach Audiobookshelf"
        } else {
            "Can't reach Navidrome"
        }
    } else if !service_configured(ctx, shelf) {
        if shelf {
            "Set the address and API token first"
        } else {
            "Set the address and user first"
        }
    } else {
        match cmd {
            4 | 74 => "Connection successful",
            5 => "Library scan requested",
            81 | 82 => "Library up to date",
            _ => return Action::None,
        }
    };
    Action::Toast(message.into())
}

pub(super) fn screen(dest: &Dest, ctx: &Ctx) -> Option<Box<dyn Screen>> {
    let slider = |kind, level| {
        Box::new(Slider {
            kind,
            level,
            bar: Rect::default(),
            confirm: Rect::default(),
        }) as Box<dyn Screen>
    };
    Some(match dest {
        Dest::SettingsPage(SettingsPage::Brightness) => {
            slider(Level::Brightness, u16::from(ctx.demo.brightness))
        }
        Dest::SettingsPage(SettingsPage::VolumeLimit) => {
            slider(Level::VolumeLimit, u16::from(ctx.demo.volume_limit))
        }
        Dest::FixturePage("Default Speed") => slider(Level::SpokenSpeed, ctx.demo.speed_percent),
        Dest::FixturePage("Clear Song Cache?") => Box::new(Confirm::clear_cache(|ctx| {
            ctx.demo.cache_cleared = true;
            pop_toast("Song cache cleared")
        })),
        Dest::FixturePage("Reset EQ Bands?") => Box::new(Confirm::reset_bands(|ctx| {
            ctx.demo.eq.bands = [0; 10];
            Action::Pop
        })),
        Dest::FixturePage("Forget Network?") => Box::new(Confirm::forget_network(|ctx| {
            if let Some(index) = ctx.demo.network.take() {
                ctx.demo.values.insert(NETWORKS[index].0, 0);
            }
            Action::Pop
        })),
        Dest::FixturePage("Forget This Device") => {
            Box::new(Confirm::forget_device(|_| Action::Pop))
        }
        Dest::FixturePage("EQ Presets") => {
            let current = eq::PRESETS
                .iter()
                .position(|preset| preset.bands == ctx.demo.eq.bands);
            let labels = eq::PRESETS
                .iter()
                .map(|preset| preset.name.into())
                .collect();
            Box::new(Choice::new(
                "EQ Presets",
                labels,
                current,
                Box::new(|i, ctx| {
                    ctx.demo.eq.set_preset(i);
                    Action::Pop
                }),
            ))
        }
        Dest::FixturePage("Selection Color") => {
            let current = Accent::ALL.iter().position(|accent| accent == ctx.accent);
            let labels = Accent::ALL.map(|accent| accent.label().into()).into();
            Box::new(Choice::new(
                "Selection Color",
                labels,
                current,
                Box::new(|index, ctx| {
                    *ctx.accent = Accent::ALL[index];
                    Action::Pop
                }),
            ))
        }
        Dest::FixturePage(page) => {
            let page = *page;
            if let Some(options) = choice(page) {
                let current = selected(ctx, page, options.default);
                let mut screen = Choice::new(
                    page,
                    options.labels,
                    Some(current),
                    Box::new(move |index, ctx| {
                        ctx.demo.values.insert(page, index);
                        Action::Pop
                    }),
                );
                if page == "Headphone Power" {
                    screen = screen.details(
                        options::HeadphonePower::ALL
                            .map(|level| level.detail().into())
                            .into(),
                    );
                }
                Box::new(screen)
            } else {
                return None;
            }
        }
        _ => return None,
    })
}

#[derive(Clone, Copy)]
enum Level {
    Brightness,
    VolumeLimit,
    SpokenSpeed,
}

struct Slider {
    kind: Level,
    level: u16,
    bar: Rect,
    confirm: Rect,
}

impl Slider {
    fn limits(&self) -> (i32, i32, i32) {
        match self.kind {
            Level::Brightness | Level::VolumeLimit => (1, 1, 100),
            Level::SpokenSpeed => {
                use crate::ui::screens::spoken_speed_view::{MAX, MIN, STEP};
                (i32::from(STEP), i32::from(MIN), i32::from(MAX))
            }
        }
    }

    fn set_level(&mut self, value: i32, ctx: &mut Ctx) -> Action {
        let (_, min, max) = self.limits();
        self.level = value.clamp(min, max) as u16;
        match self.kind {
            Level::Brightness => ctx.demo.brightness = self.level as u8,
            Level::VolumeLimit => {
                ctx.demo.volume_limit = self.level as u8;
                *ctx.volume = (*ctx.volume).min(ctx.demo.volume_limit);
            }
            Level::SpokenSpeed => ctx.demo.speed_percent = self.level,
        }
        Action::Stay
    }
}

impl Screen for Slider {
    fn clickable(&self, x: i32, y: i32) -> bool {
        hits(self.bar, x, y) || hits(self.confirm, x, y)
    }

    fn click(&mut self, x: i32, y: i32, ctx: &mut Ctx) -> Action {
        if hits(self.confirm, x, y) {
            return self.event(Event::Center, ctx);
        }
        if !hits(self.bar, x, y) {
            return Action::None;
        }
        let (step, min, max) = self.limits();
        let base = if matches!(self.kind, Level::SpokenSpeed) {
            min
        } else {
            0
        };
        let track_width = self.bar.w - 2;
        let offset = (x - self.bar.x - 1).clamp(0, track_width);
        let steps = (max - base) / step;
        let value = base + step * ((offset * steps + track_width / 2) / track_width);
        self.set_level(value, ctx)
    }

    fn title(&self) -> String {
        match self.kind {
            Level::Brightness => "Brightness",
            Level::VolumeLimit => "Volume Limit",
            Level::SpokenSpeed => "Default Speed",
        }
        .into()
    }

    fn event(&mut self, event: Event, ctx: &mut Ctx) -> Action {
        match event {
            Event::Center | Event::Back => Action::Pop,
            Event::Scroll(ticks) => {
                let (step, _, _) = self.limits();
                self.set_level(
                    i32::from(self.level).saturating_add(ticks.saturating_mul(step)),
                    ctx,
                )
            }
            _ => Action::None,
        }
    }

    fn draw(&mut self, d: &mut Draw, area: Rect) {
        let help_y = if matches!(self.kind, Level::SpokenSpeed) {
            use crate::ui::screens::spoken_speed_view as speed;
            speed::draw(
                d,
                area,
                self.level,
                speed::caption(true, false, false),
                true,
                None,
            );
            self.bar = speed::bar(area);
            186
        } else {
            self.bar = pages::draw_level(
                d,
                area,
                self.level as u8,
                matches!(self.kind, Level::VolumeLimit),
            );
            180
        };
        self.confirm = center_target(
            d,
            "Wheel: adjust · Center: done",
            area.x + area.w / 2,
            area.y + help_y,
        );
    }
}

pub fn sleep_timer_choice(ctx: &Ctx) -> Box<dyn Screen> {
    let period = if ctx.sleep_timer.is_none() {
        Some(Duration::ZERO)
    } else {
        ctx.demo.sleep_period
    };
    pages::sleep_timer_choice(period, |period, ctx| {
        *ctx.sleep_fading = false;
        ctx.demo.sleep_period = period;
        *ctx.sleep_timer = period.map(|period| ctx.now + period);
    })
}
