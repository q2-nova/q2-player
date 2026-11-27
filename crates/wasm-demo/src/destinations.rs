//! Synthetic device state and the demo's supplemental menu routes.
use crate::clock::Instant;
use crate::config::{RatingControls, options};
use crate::gfx::{Rect, Weight};
use crate::input::Event;
use crate::timezones;
use crate::ui::list::{Badge, Row, RowAction};
use crate::ui::screens::text::{Kind, TextEntry};
use crate::ui::{Action, Ctx, Dest, Draw, Screen};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;
use subsonic::{Chapter, Song, Spoken, SpokenSource};

mod settings;
pub use settings::{Page as SettingsPage, sleep_timer_choice};
#[path = "../../q2nd/src/audio/eq.rs"]
mod eq;
#[path = "../../q2nd/src/ui/info_text.rs"]
mod info_text;
use info_text::InfoText;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpokenPage {
    Books,
    Podcasts,
    Section {
        podcast: bool,
        title: &'static str,
    },
    Local,
    DownloadedBooks,
    DownloadedPodcasts,
    Context {
        podcast: bool,
        title: &'static str,
        item: Option<usize>,
    },
    Info {
        item: usize,
        preview: bool,
    },
    Show(usize),
}

pub struct State {
    pub brightness: u8,
    volume_limit: u8,
    eq: eq::Settings,
    sleep_period: Option<Duration>,
    pub speed_percent: u16,
    pub clock_in_title: bool,
    pub timezone: &'static str,
    wireless: bool,
    network: Option<usize>,
    scanned: bool,
    offline: bool,
    bluetooth: bool,
    paired: bool,
    tailscale: bool,
    cache_cleared: bool,
    downloads: BTreeSet<usize>,
    positions: BTreeMap<usize, u64>,
    queued_downloads: BTreeSet<usize>,
    active_download: Option<(usize, Instant, u8)>,
    download_progress: Option<(usize, u8)>,
    downloads_paused: bool,
    saved_radios: BTreeSet<usize>,
    values: BTreeMap<&'static str, usize>,
    fields: BTreeMap<&'static str, String>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            brightness: 50,
            volume_limit: 100,
            eq: eq::Settings::default(),
            sleep_period: None,
            speed_percent: options::DEFAULT_SPOKEN_SPEED,
            clock_in_title: false,
            timezone: timezones::UTC,
            wireless: true,
            network: Some(0),
            scanned: false,
            offline: false,
            bluetooth: false,
            paired: false,
            tailscale: false,
            cache_cleared: false,
            downloads: BTreeSet::from([1, 3]),
            positions: BTreeMap::from([(0, 812_000), (2, 443_000)]),
            queued_downloads: BTreeSet::new(),
            active_download: None,
            download_progress: None,
            downloads_paused: false,
            saved_radios: BTreeSet::new(),
            values: BTreeMap::new(),
            fields: BTreeMap::new(),
        }
    }
}

const NETWORKS: [(&str, &str); 3] = [
    ("Listening Room", "Strong"),
    ("Guest Network", "Good"),
    ("Station Café", "Weak"),
];
const STATIONS: [&str; 7] = [
    "FIP",
    "KEXP",
    "SomaFM · Groove Salad",
    "SomaFM · Drone Zone",
    "SomaFM · Secret Agent",
    "FIP Jazz",
    "FIP Groove",
];

pub fn title(dest: &Dest) -> Option<&str> {
    match dest {
        Dest::Settings => Some("Settings"),
        Dest::SettingsPage(page) => Some(page.title()),
        Dest::Wifi => Some("Wi-Fi"),
        Dest::Bluetooth => Some("Bluetooth"),
        Dest::Tailscale => Some("Tailscale"),
        Dest::StorageUsage => Some("Storage Usage"),
        Dest::FixturePage("Audiobookshelf Address") => Some("Address"),
        Dest::Spoken(page) => Some(match page {
            SpokenPage::Books => "Books",
            SpokenPage::Podcasts => "Podcasts",
            SpokenPage::Local => "On This Card",
            SpokenPage::DownloadedBooks => "Downloaded Books",
            SpokenPage::DownloadedPodcasts => "Downloaded Episodes",
            SpokenPage::Section { title, .. } | SpokenPage::Context { title, .. } => title,
            SpokenPage::Info { item, .. } => {
                if ITEMS[*item].podcast {
                    "Episode Info"
                } else {
                    "Book Info"
                }
            }
            SpokenPage::Show(item) => ITEMS[*item].album,
        }),
        Dest::Equalizer => Some("Equalizer"),
        Dest::RadioStations { featured } => Some(if *featured {
            "Featured Stations"
        } else {
            "Radios"
        }),
        Dest::Downloads => Some("Downloads"),
        Dest::FixturePage(title) => Some(title),
        _ => None,
    }
}

fn link(label: &str, page: &'static str) -> Row {
    Row::link(label, Dest::FixturePage(page))
}

fn set(label: &str, cmd: u32) -> Row {
    Row::new(label, RowAction::Cmd(cmd))
}

fn info(label: &str) -> Row {
    Row::new(label, RowAction::None)
}

fn on_off(enabled: bool) -> &'static str {
    if enabled { "On" } else { "Off" }
}

fn selected(ctx: &Ctx, page: &'static str, default: usize) -> usize {
    ctx.demo.values.get(page).copied().unwrap_or(default)
}

fn field<'a>(ctx: &'a Ctx, id: &str, default: &'a str) -> &'a str {
    ctx.demo
        .fields
        .get(id)
        .map(String::as_str)
        .unwrap_or(default)
}

fn speed_label(percent: u16) -> String {
    crate::ui::screens::spoken_speed_view::label(percent)
}

fn fixture_rows(page: &str, ctx: &Ctx) -> Option<Vec<Row>> {
    Some(match page {
        "Wi-Fi" => {
            let mut rows = vec![set("Wi-Fi", 1).value(on_off(ctx.demo.wireless))];
            if ctx.demo.wireless {
                let detail = ctx.demo.network.map_or_else(
                    || "Not connected".into(),
                    |index| format!("{} · 192.0.2.21", NETWORKS[index].0),
                );
                rows.push(info("Connected To").detail(detail));
                if ctx.demo.network.is_some() {
                    rows.push(link("Forget This Network…", "Forget Network?"));
                }
                rows.push(set("Scan", 2));
                if ctx.demo.scanned {
                    rows.extend(NETWORKS.iter().enumerate().map(
                        |(index, &(network, strength))| {
                            let saved = selected(ctx, network, usize::from(index == 0)) != 0;
                            let row = if index == 2 || saved {
                                set(network, 100 + index as u32)
                            } else {
                                link(network, network_password(index))
                            };
                            row.value(strength).detail(if index == 2 {
                                "Open"
                            } else if saved {
                                "Saved · Secured"
                            } else {
                                "Secured"
                            })
                        },
                    ));
                }
            }
            rows
        }
        "Tailscale" => {
            let mut rows = vec![set("Tailscale", 1).value(on_off(ctx.demo.tailscale))];
            if ctx.demo.tailscale {
                rows.extend([
                    info("Status").detail("Needs Sign In"),
                    link("Sign In", "Sign In"),
                    link("Forget This Device", "Forget This Device"),
                ]);
            }
            rows
        }
        "Sign In" => vec![info("Sign in on your Q2").detail("Tailscale sign-in requires a device")],
        "Bluetooth" => {
            let mut rows = vec![set("Bluetooth", 1).value(on_off(ctx.demo.bluetooth))];
            if ctx.demo.bluetooth {
                rows.push(set("Scan", 2));
                if selected(ctx, "Bluetooth Scan", 0) != 0 || ctx.demo.paired {
                    rows.push(set("Studio Headphones", 3).detail(if ctx.demo.paired {
                        "Connected"
                    } else {
                        "Available"
                    }));
                }
            }
            rows
        }
        "Storage Usage" => vec![
            info("Card Capacity").value("32.0 GB"),
            info("Available").value(if ctx.demo.cache_cleared {
                "27.4 GB"
            } else {
                "26.2 GB"
            }),
            info("Downloads").value("740 MB"),
            info("Song Cache").value(if ctx.demo.cache_cleared {
                "0 MB"
            } else {
                "1.2 GB"
            }),
            info("Cover Art").value("188 MB"),
        ],
        _ => return None,
    })
}

fn network_password(index: usize) -> &'static str {
    ["Password for Listening Room", "Password for Guest Network"][index]
}

fn equalizer(ctx: &Ctx) -> Vec<Row> {
    vec![
        set("Graphic EQ", 0).value(on_off(ctx.demo.eq.graphic_enabled)),
        link("Presets", "EQ Presets").value(ctx.demo.eq.preset_name()),
        link("Adjust Bands", "Graphic EQ"),
        link("Reset Bands…", "Reset EQ Bands?"),
        info("Output")
            .value(if ctx.demo.paired && ctx.demo.bluetooth {
                "Studio Headphones"
            } else {
                "Headphone Jack"
            })
            .separated(),
        info("Headphone EQ").value("Off"),
        info("No headphones selected"),
    ]
}

#[derive(Clone, Copy)]
struct Item {
    title: &'static str,
    artist: &'static str,
    album: &'static str,
    detail: &'static str,
    summary: &'static str,
    seconds: u32,
    podcast: bool,
    chapters: &'static [(u32, &'static str)],
}

const ITEMS: [Item; 6] = [
    Item {
        title: "The Wonderful Wizard of Oz",
        artist: "L. Frank Baum",
        album: "Oz",
        detail: "W. Blaine Dowler",
        summary: "A cyclone carries Dorothy and Toto to Oz, where they follow the yellow brick road in search of a way home. Read for LibriVox.",
        seconds: 9_673,
        podcast: false,
        chapters: &[
            (0, "Chapters 1–2"),
            (838, "Chapters 3–4"),
            (1_683, "Chapters 5–6"),
            (2_558, "Chapters 7–8"),
            (3_465, "Chapters 9–10"),
            (4_285, "Chapters 11–12"),
            (6_066, "Chapters 13–14"),
            (6_814, "Chapters 15–16"),
            (7_693, "Chapters 17–18"),
            (8_259, "Chapters 19–20"),
            (8_894, "Chapters 21–24"),
        ],
    },
    Item {
        title: "Alice's Adventures in Wonderland (Abridged)",
        artist: "Lewis Carroll",
        album: "",
        detail: "Phil Chenevert",
        summary: "Alice follows a white rabbit into a world of peculiar creatures. Abridged and read for LibriVox.",
        seconds: 4_087,
        podcast: false,
        chapters: &[
            (0, "Down the Rabbit Hole"),
            (547, "The Pool of Tears"),
            (1_021, "A Caucus Race and a Long Tale"),
            (1_477, "The Rabbit Sends In a Little Bill"),
            (2_011, "Advice from a Caterpillar"),
            (2_469, "Pig and Pepper"),
            (2_854, "A Mad Tea-Party"),
            (3_096, "The Queen's Croquet Ground"),
            (3_578, "Who Stole the Tarts?"),
            (3_828, "Alice's Evidence"),
        ],
    },
    Item {
        title: "Encore: A Year In Mars Dune Alpha",
        artist: "NASA's Curious Universe",
        album: "NASA's Curious Universe",
        detail: "20 October 2026",
        summary: "NASA revisits an Earth-based mission that simulated a year of life on Mars.",
        seconds: 1_607,
        podcast: true,
        chapters: &[],
    },
    Item {
        title: "Encore: Welcome to the Dark Side",
        artist: "NASA's Curious Universe",
        album: "NASA's Curious Universe",
        detail: "22 September 2026",
        summary: "NASA's Roman Space Telescope will help scientists investigate the mystery of dark energy.",
        seconds: 1_554,
        podcast: true,
        chapters: &[],
    },
    Item {
        title: "Capturing Artemis Return",
        artist: "Houston We Have a Podcast",
        album: "Houston We Have a Podcast",
        detail: "20 November 2026",
        summary: "NASA photographers James Blair and Josh Valcarcel discuss capturing the Artemis II crew's splashdown and recovery.",
        seconds: 3_586,
        podcast: true,
        chapters: &[],
    },
    Item {
        title: "The Time Machine",
        artist: "H. G. Wells",
        album: "",
        detail: "LibriVox Volunteers",
        summary: "A Victorian inventor tells his dinner guests what he discovered on a journey into the distant future.",
        seconds: 13_649,
        podcast: false,
        chapters: &[
            (0, "Chapter I"),
            (1_290, "Chapter II"),
            (2_129, "Chapter III"),
            (3_055, "Chapter IV"),
            (4_725, "Chapter V"),
            (7_304, "Chapter VI"),
            (8_170, "Chapter VII"),
            (9_270, "Chapter VIII"),
            (10_367, "Chapter IX"),
            (11_416, "Chapter X"),
            (11_879, "Chapter XI"),
            (12_682, "Chapter XII"),
            (13_482, "Epilogue"),
        ],
    },
];

pub(crate) fn podcast_series(song: &Song) -> Option<&str> {
    let item = ITEMS.get(State::item_index(song)?)?;
    item.podcast.then_some(item.album)
}

fn item_song(index: usize, local: bool) -> Song {
    let item = ITEMS[index];
    let id = format!("demo-spoken-{index}");
    let duration_ms = u64::from(item.seconds) * 1000;
    Song {
        id: id.clone(),
        title: item.title.into(),
        artist: item.artist.into(),
        album: if item.album.is_empty() {
            item.title
        } else {
            item.album
        }
        .into(),
        duration: item.seconds,
        suffix: Some(if item.podcast { "mp3" } else { "m4b" }.into()),
        bit_rate: Some(64),
        spoken: Some(Box::new(Spoken {
            id,
            offset_ms: 0,
            duration_ms,
            chapters: item
                .chapters
                .iter()
                .map(|&(seconds, title)| Chapter {
                    start_ms: u64::from(seconds) * 1_000,
                    title: title.into(),
                })
                .collect(),
            source: if local {
                SpokenSource::Local {
                    relative_path: format!("demo-{index}.m4b"),
                }
            } else {
                SpokenSource::Audiobookshelf {
                    namespace: "demo".into(),
                    item: format!("demo-item-{index}"),
                    episode: item.podcast.then(|| format!("episode-{index}")),
                    content_url: String::new(),
                }
            },
        })),
        ..Song::default()
    }
}

fn item_row(index: usize, ctx: &Ctx, local: bool) -> Row {
    let item = ITEMS[index];
    let downloaded = ctx.demo.downloads.contains(&index);
    let (detail, badge) = if downloaded {
        ("Downloaded".into(), Badge::Offline)
    } else if let Some((_, _, percent)) = ctx.demo.active_download.filter(|entry| entry.0 == index)
    {
        (
            format!("Downloading · {percent}%"),
            Badge::Downloading(percent),
        )
    } else if ctx.demo.queued_downloads.contains(&index) {
        (
            if ctx.demo.downloads_paused {
                "Paused"
            } else {
                "Queued"
            }
            .into(),
            Badge::Queued,
        )
    } else {
        let detail = if let Some(position) = ctx
            .demo
            .positions
            .get(&index)
            .copied()
            .filter(|position| *position > 0)
        {
            format!(
                "{} · Resume at {}",
                item.artist,
                crate::ui::duration(position / 1000)
            )
        } else {
            item.artist.into()
        };
        (detail, Badge::None)
    };
    set(item.title, 1000 + index as u32)
        .key(format!("item-{index}"))
        .song(item_song(index, local))
        .detail(detail)
        .badge(badge)
}

const BOOK_SECTIONS: &[&str] = &[
    "Continue Listening",
    "Continue Series",
    "Recently Added",
    "Recent Series",
    "Discover",
    "Books",
    "Genres",
    "Series",
    "Collections",
    "Playlists",
    "Authors",
    "Narrators",
    "Search Books",
];
const PODCAST_SECTIONS: &[&str] = &[
    "Continue Listening",
    "Recently Added",
    "Listen Again",
    "Library",
    "Latest Episodes",
    "Playlists",
];

fn section_link(label: &'static str, podcast: bool) -> Row {
    Row::link(
        label,
        Dest::Spoken(SpokenPage::Section {
            podcast,
            title: label,
        }),
    )
}

fn spoken_rows(page: SpokenPage, ctx: &Ctx) -> Vec<Row> {
    use SpokenPage::*;
    match page {
        Books | Podcasts => {
            let podcast = page == Podcasts;
            let sections = if podcast {
                PODCAST_SECTIONS
            } else {
                BOOK_SECTIONS
            };
            let mut rows: Vec<_> = sections
                .iter()
                .map(|label| section_link(label, podcast))
                .collect();
            if !podcast {
                rows[5].separator = true;
                rows.push(Row::link("On This Card", Dest::Spoken(Local)));
            }
            rows.push(Row::link(
                "Downloads",
                Dest::Spoken(if podcast {
                    DownloadedPodcasts
                } else {
                    DownloadedBooks
                }),
            ));
            rows
        }
        Local => vec![item_row(5, ctx, true)],
        DownloadedBooks | DownloadedPodcasts => {
            let podcast = page == DownloadedPodcasts;
            let rows: Vec<_> = ctx
                .demo
                .downloads
                .union(&ctx.demo.queued_downloads)
                .copied()
                .filter(|index| ITEMS[*index].podcast == podcast)
                .map(|index| item_row(index, ctx, false))
                .collect();
            if rows.is_empty() {
                vec![info("No books or downloaded episodes")]
            } else {
                rows
            }
        }
        Section { podcast, title } => section_rows(podcast, title, ctx),
        Show(index) => show_rows(index, ctx),
        Context {
            podcast,
            title,
            item,
        } => {
            let mut rows = Vec::new();
            if let Some(item) = item {
                if matches!(
                    title,
                    "On This Card" | "Downloaded Books" | "Downloaded Episodes"
                ) {
                    rows.push(set("Resume", 1000 + item as u32));
                }
                rows.push(Row::screen(
                    if podcast { "Episode Info" } else { "Book Info" },
                    1200 + item as u32,
                ));
                if title != "On This Card" {
                    rows.push(set(
                        if ctx.demo.downloads.contains(&item) {
                            "Remove Download"
                        } else if ctx.demo.queued_downloads.contains(&item) {
                            "Cancel Download"
                        } else {
                            "Download for Offline"
                        },
                        1100 + item as u32,
                    ));
                }
            }
            if !matches!(
                title,
                "On This Card" | "Downloaded Books" | "Downloaded Episodes"
            ) {
                if title != "Books" && title != "Podcasts" || item.is_some() {
                    rows.push(set("Refresh", 6));
                }
                rows.push(Row::screen("Switch Library", 7));
            }
            rows
        }
        Info { .. } => Vec::new(),
    }
}

fn show_rows(index: usize, ctx: &Ctx) -> Vec<Row> {
    let show = ITEMS[index].album;
    ITEMS
        .iter()
        .enumerate()
        .filter(|(_, item)| item.podcast && item.album == show)
        .map(|(index, _)| item_row(index, ctx, false))
        .collect()
}

fn section_rows(podcast: bool, title: &'static str, ctx: &Ctx) -> Vec<Row> {
    let items = |indexes: &[usize]| {
        indexes
            .iter()
            .map(|&index| item_row(index, ctx, false))
            .collect()
    };
    let show = |index: usize| Row::link(ITEMS[index].album, Dest::Spoken(SpokenPage::Show(index)));
    if podcast {
        match title {
            "Choose Library" => vec![set("Podcasts", 8).badge(Badge::Check)],
            "Library" | "Recently Added" => vec![show(2), show(4)],
            "Continue Listening" => items(&[2]),
            "Listen Again" => items(&[3]),
            "Latest Episodes" => items(&[4, 2, 3]),
            "Playlists" => vec![section_link("Evening Listening", true)],
            "Evening Listening" => items(&[2, 4]),
            _ => Vec::new(),
        }
    } else {
        match title {
            "Choose Library" => vec![set("Audiobooks", 8).badge(Badge::Check)],
            "Continue Listening" | "Continue Series" | "Recent Series" => items(&[0]),
            "Recently Added" | "Discover" => items(&[1, 5, 0]),
            "Books" => items(&[1, 5, 0]),
            "Genres" => vec![
                section_link("Fiction", false),
                section_link("Science Fiction", false),
            ],
            "Series" => vec![section_link("Oz", false)],
            "Collections" => vec![section_link("After Dark", false)],
            "Playlists" => vec![section_link("Evening Listening", false)],
            "Authors" => vec![
                section_link("H. G. Wells", false),
                section_link("L. Frank Baum", false),
                section_link("Lewis Carroll", false),
            ],
            "Narrators" => vec![
                section_link("LibriVox Volunteers", false),
                section_link("Phil Chenevert", false),
                section_link("W. Blaine Dowler", false),
            ],
            "Fiction" => items(&[0, 1]),
            "Lewis Carroll" | "Phil Chenevert" => items(&[1]),
            "Science Fiction" | "H. G. Wells" | "LibriVox Volunteers" => items(&[5]),
            "Oz" | "L. Frank Baum" | "W. Blaine Dowler" => items(&[0]),
            "After Dark" | "Evening Listening" => items(&[1, 0]),
            "Search Results" => {
                let query = field(ctx, "Book Search", "").to_lowercase();
                let found: Vec<_> = ITEMS
                    .iter()
                    .enumerate()
                    .filter(|(_, item)| {
                        !item.podcast
                            && [item.title, item.artist, item.album, item.detail]
                                .iter()
                                .any(|text| text.to_lowercase().contains(&query))
                    })
                    .map(|(index, _)| item_row(index, ctx, false))
                    .collect();
                if found.is_empty() {
                    vec![info("No books found")]
                } else {
                    found
                }
            }
            _ => Vec::new(),
        }
    }
}

impl State {
    pub fn online(&self) -> bool {
        self.wireless && self.network.is_some() && !self.offline
    }

    pub fn connection_badge(&self) -> crate::ui::connectivity::Badge {
        crate::ui::connectivity::select(
            self.offline,
            self.wireless && self.network.is_some(),
            self.tailscale,
            false,
            self.navidrome_configured(),
        )
    }

    fn navidrome_configured(&self) -> bool {
        ["Server Address", "User"]
            .into_iter()
            .all(|field| self.fields.get(field).is_none_or(|value| !value.is_empty()))
    }

    pub fn spoken_downloaded(&self, song: &Song) -> bool {
        Self::item_index(song).is_some_and(|index| self.downloads.contains(&index))
    }

    pub fn resume_position(&self, song: &Song) -> u64 {
        Self::item_index(song)
            .and_then(|index| self.positions.get(&index).copied())
            .unwrap_or_default()
    }

    pub fn save_position(&mut self, song: &Song, position_ms: u64) {
        if let Some(index) = Self::item_index(song) {
            let length = u64::from(song.duration) * 1000;
            self.positions.insert(
                index,
                if position_ms >= length.saturating_sub(1000) {
                    0
                } else {
                    position_ms
                },
            );
        }
    }

    pub(crate) fn item_index(song: &Song) -> Option<usize> {
        song.id.strip_prefix("demo-spoken-")?.parse().ok()
    }

    pub fn volume_limit(&self) -> u8 {
        self.volume_limit
    }

    pub fn rating_controls(&self) -> RatingControls {
        self.values.get("Song Ratings").map_or_else(RatingControls::default, |&index| RatingControls::ALL[index])
    }

    pub fn skip_seconds(&self, forward: bool) -> u64 {
        let (name, default) = if forward {
            ("Skip Forward", options::DEFAULT_SKIP_FORWARD)
        } else {
            ("Skip Backward", options::DEFAULT_SKIP_BACKWARD)
        };
        u64::from(self.values.get(name).map_or(default, |&index| options::SKIP_SECONDS[index]))
    }

    pub fn skip_by_chapter(&self) -> bool {
        self.values.get("Book Skip Buttons").copied() == Some(1)
    }
}

pub fn refresh(ctx: &mut Ctx) -> bool {
    let demo = &mut *ctx.demo;
    if demo.downloads_paused || !demo.online() {
        if let Some((item, _, percent)) = demo.active_download.take() {
            demo.download_progress = Some((item, percent));
            return true;
        }
        return false;
    }
    if let Some((item, start, percent)) = demo.active_download {
        let elapsed = ctx.now.saturating_duration_since(start).as_millis();
        let next = (elapsed / 80).min(100) as u8;
        if next == 100 {
            demo.queued_downloads.remove(&item);
            demo.downloads.insert(item);
            demo.active_download = None;
            demo.download_progress = None;
            return true;
        }
        if next / 5 != percent / 5 {
            demo.active_download = Some((item, start, next));
            return true;
        }
    } else if let Some((item, percent)) = demo
        .download_progress
        .or_else(|| demo.queued_downloads.first().map(|&item| (item, 0)))
    {
        let started = ctx.now - Duration::from_millis(u64::from(percent) * 80);
        demo.active_download = Some((item, started, percent));
        return true;
    }
    false
}

fn download_rows(ctx: &Ctx) -> Vec<Row> {
    let mut rows = Vec::new();
    if !ctx.demo.queued_downloads.is_empty() {
        let count = ctx.demo.queued_downloads.len();
        let detail = if let Some((index, _, percent)) = ctx.demo.active_download {
            format!("{} · {}%", ITEMS[index].title, percent)
        } else if ctx.demo.downloads_paused {
            "Paused".into()
        } else if !ctx.demo.online() {
            "Waiting for the server".into()
        } else {
            "Waiting to start".into()
        };
        rows.push(
            Row::new(format!("Audiobooks · {count} to download"), RowAction::None)
                .key("spoken-download-status")
                .detail(detail),
        );
        rows.push(
            set(
                if ctx.demo.downloads_paused {
                    "Resume Downloads"
                } else {
                    "Pause Downloads"
                },
                20,
            )
            .key("download-pause-toggle"),
        );
    }
    rows.extend(
        ctx.lib
            .albums
            .iter()
            .filter(|album| ctx.lib.album_downloaded(&album.id))
            .map(|album| {
                crate::ui::music_rows::album_row(album, "Album · Downloaded".into())
                    .badge(Badge::Offline)
            }),
    );
    rows
}

fn radio_rows(featured: bool, ctx: &Ctx) -> Vec<Row> {
    let row = |index: usize| set(STATIONS[index], 2000 + index as u32);
    if featured {
        (0..STATIONS.len()).map(row).collect()
    } else {
        let mut rows = vec![Row::link(
            "Featured Stations",
            Dest::RadioStations { featured: true },
        )];
        rows.extend(ctx.demo.saved_radios.iter().copied().map(row));
        if !ctx.demo.online() {
            rows.push(info("Server stations unavailable"));
        }
        rows
    }
}

/// Settings use the shared list's passive information row style.
pub fn clock_24h(state: &State) -> bool {
    state.values.get("24-Hour Clock").copied().unwrap_or_default() != 0
}

pub fn menu_tall(dest: &Dest) -> Option<bool> {
    match dest {
        Dest::Settings | Dest::Equalizer => Some(false),
        Dest::SettingsPage(page) => Some(page.tall_rows()),
        Dest::Wifi | Dest::Bluetooth | Dest::Tailscale | Dest::StorageUsage => Some(true),
        _ => None,
    }
}

pub fn rows(dest: &Dest, ctx: &Ctx) -> Option<Vec<Row>> {
    match dest {
        Dest::Settings => Some(settings::rows(SettingsPage::Root, ctx)),
        Dest::SettingsPage(page) => Some(settings::rows(*page, ctx)),
        Dest::Spoken(page) => Some(spoken_rows(*page, ctx)),
        Dest::Equalizer => Some(equalizer(ctx)),
        Dest::RadioStations { featured } => Some(radio_rows(*featured, ctx)),
        Dest::Downloads => Some(download_rows(ctx)),
        Dest::Wifi => fixture_rows("Wi-Fi", ctx),
        Dest::Bluetooth => fixture_rows("Bluetooth", ctx),
        Dest::Tailscale => fixture_rows("Tailscale", ctx),
        Dest::StorageUsage => fixture_rows("Storage Usage", ctx),
        Dest::FixturePage(page) => {
            if let Some(index) = STATIONS.iter().position(|station| station == page) {
                return Some(vec![
                    set("Play Station", 2000 + index as u32),
                    set(
                        if ctx.demo.saved_radios.contains(&index) {
                            "Remove from Navidrome"
                        } else {
                            "Add to Navidrome"
                        },
                        2100 + index as u32,
                    ),
                ]);
            }
            fixture_rows(page, ctx)
        }
        _ => None,
    }
}

pub fn context(dest: &Dest, row: &Row) -> Option<Dest> {
    use SpokenPage::*;
    match dest {
        Dest::Spoken(page) => {
            let item = row
                .key
                .strip_prefix("item-")
                .and_then(|id| id.parse::<usize>().ok());
            let (podcast, title) = match page {
                Books => (false, "Books"),
                Podcasts => (true, "Podcasts"),
                Local => (false, "On This Card"),
                DownloadedBooks => (false, "Downloaded Books"),
                DownloadedPodcasts => (true, "Downloaded Episodes"),
                Section {
                    title: "Choose Library",
                    ..
                } => return None,
                Section { podcast, title } => (*podcast, *title),
                Show(index) => (true, ITEMS[*index].album),
                Context { .. } | Info { .. } => return None,
            };
            Some(Dest::Spoken(Context {
                podcast,
                item,
                title,
            }))
        }
        Dest::RadioStations { .. } => STATIONS
            .iter()
            .copied()
            .find(|station| *station == row.title)
            .map(Dest::FixturePage),
        _ => None,
    }
}

fn play(song: Song) -> Action {
    Action::Play {
        songs: Arc::new(vec![song]),
        index: 0,
        shuffle: false,
    }
}

fn pop_toast(message: impl Into<String>) -> Action {
    Action::Many(vec![Action::Pop, Action::Toast(message.into())])
}

fn playable_spoken(ctx: &Ctx, index: usize, local: bool) -> bool {
    local
        || ctx.demo.downloads.contains(&index)
        || (ctx.demo.online() && service_configured(ctx, true))
}

fn play_spoken(ctx: &Ctx, index: usize, local: bool) -> Action {
    if !playable_spoken(ctx, index, local) {
        return Action::Toast("Not downloaded".into());
    }
    play(item_song(index, local))
}

pub fn command(dest: &Dest, cmd: u32, ctx: &mut Ctx) -> Action {
    match dest {
        Dest::SettingsPage(page) => settings::command(*page, cmd, ctx),
        Dest::Equalizer if cmd == 0 => {
            ctx.demo.eq.graphic_enabled = !ctx.demo.eq.graphic_enabled;
            Action::None
        }
        Dest::Downloads if cmd == 20 => {
            ctx.demo.downloads_paused = !ctx.demo.downloads_paused;
            Action::None
        }
        Dest::Spoken(page) => spoken_command(*page, cmd, ctx),
        Dest::RadioStations { .. } if (2000..2000 + STATIONS.len() as u32).contains(&cmd) => {
            play_radio((cmd - 2000) as usize, ctx)
        }
        Dest::Wifi => fixture_command("Wi-Fi", cmd, ctx),
        Dest::Bluetooth => fixture_command("Bluetooth", cmd, ctx),
        Dest::Tailscale => fixture_command("Tailscale", cmd, ctx),
        Dest::FixturePage(page) => fixture_command(page, cmd, ctx),
        _ => Action::None,
    }
}

fn service_configured(ctx: &Ctx, shelf: bool) -> bool {
    if shelf {
        !field(ctx, "Audiobookshelf Address", "demo").is_empty()
            && !field(ctx, "API Token", "demo").is_empty()
    } else {
        ctx.demo.navidrome_configured()
    }
}

fn spoken_command(page: SpokenPage, cmd: u32, ctx: &mut Ctx) -> Action {
    use SpokenPage::*;
    match cmd {
        6 => pop_toast(if ctx.demo.online() {
            "Library up to date"
        } else {
            "Can't reach Audiobookshelf"
        }),
        7 => {
            let Context { podcast, .. } = page else {
                return Action::None;
            };
            let choice = crate::ui::screens::open(
                Dest::Spoken(Section {
                    podcast,
                    title: "Choose Library",
                }),
                ctx,
            );
            Action::Replace(choice)
        }
        8 => Action::Pop,
        value @ 1000..=1005 => {
            let index = (value - 1000) as usize;
            if matches!(page, DownloadedBooks | DownloadedPodcasts)
                && !ctx.demo.downloads.contains(&index)
            {
                return Action::Toast("Waiting to download".into());
            }
            let local = page == Local
                || matches!(
                    page,
                    Context {
                        title: "On This Card",
                        ..
                    }
                );
            if !ITEMS[index].podcast && !matches!(page, Context { .. }) {
                return Action::Push(Box::new(ItemInfo::new(index, true, local)));
            }
            if matches!(page, Context { .. }) && playable_spoken(ctx, index, local) {
                Action::Many(vec![Action::Pop, play_spoken(ctx, index, local)])
            } else {
                play_spoken(ctx, index, local)
            }
        }
        value @ 1200..=1205 => {
            let local = matches!(
                page,
                Context {
                    title: "On This Card",
                    ..
                }
            );
            Action::Replace(Box::new(ItemInfo::new(
                (value - 1200) as usize,
                false,
                local,
            )))
        }
        value @ 1100..=1105 => {
            let index = (value - 1100) as usize;
            if ctx.demo.downloads.remove(&index) {
                pop_toast("Download removed")
            } else if ctx.demo.queued_downloads.remove(&index) {
                if ctx
                    .demo
                    .active_download
                    .is_some_and(|entry| entry.0 == index)
                {
                    ctx.demo.active_download = None;
                }
                if ctx
                    .demo
                    .download_progress
                    .is_some_and(|entry| entry.0 == index)
                {
                    ctx.demo.download_progress = None;
                }
                pop_toast("Download removed")
            } else {
                ctx.demo.queued_downloads.insert(index);
                pop_toast(if ITEMS[index].podcast {
                    "Episode queued"
                } else {
                    "Audiobook queued"
                })
            }
        }
        _ => Action::None,
    }
}

fn play_radio(index: usize, ctx: &Ctx) -> Action {
    if !ctx.demo.online() {
        return Action::Toast("Radio needs a connection".into());
    }
    play(Song {
        id: format!("demo-radio-{index}"),
        title: STATIONS[index].into(),
        artist: match index {
            1 => "KEXP",
            2..=4 => "SomaFM",
            _ => "Radio France",
        }
        .into(),
        radio_url: Some(format!("https://radio.example.test/{index}")),
        ..Song::default()
    })
}

fn fixture_command(page: &'static str, cmd: u32, ctx: &mut Ctx) -> Action {
    match (page, cmd) {
        ("Wi-Fi", 1) => {
            ctx.demo.wireless = !ctx.demo.wireless;
            if ctx.demo.wireless {
                ctx.demo.scanned = true;
            }
            Action::None
        }
        ("Wi-Fi", 2) => {
            ctx.demo.scanned = true;
            Action::None
        }
        ("Wi-Fi", index @ 100..=102) => {
            let index = (index - 100) as usize;
            ctx.demo.network = Some(index);
            Action::Toast(format!("Connected to {}", NETWORKS[index].0))
        }
        ("Tailscale", 1) => {
            ctx.demo.tailscale = !ctx.demo.tailscale;
            Action::None
        }
        ("Bluetooth", 1) => {
            ctx.demo.bluetooth = !ctx.demo.bluetooth;
            Action::None
        }
        ("Bluetooth", 2) => {
            ctx.demo.values.insert("Bluetooth Scan", 1);
            Action::None
        }
        ("Bluetooth", 3) => {
            ctx.demo.paired = !ctx.demo.paired;
            Action::None
        }
        (station, cmd) if STATIONS.contains(&station) => {
            if !ctx.demo.online() {
                return pop_toast("Radio needs a connection");
            }
            if (2000..2000 + STATIONS.len() as u32).contains(&cmd) {
                return Action::Many(vec![Action::Pop, play_radio((cmd - 2000) as usize, ctx)]);
            }
            if !(2100..2100 + STATIONS.len() as u32).contains(&cmd) {
                return Action::None;
            }
            let index = (cmd - 2100) as usize;
            if ctx.demo.saved_radios.remove(&index) {
                pop_toast("Station removed")
            } else {
                ctx.demo.saved_radios.insert(index);
                pop_toast("Station saved")
            }
        }
        _ => Action::None,
    }
}

pub fn special(dest: &Dest, ctx: &Ctx) -> Option<Box<dyn Screen>> {
    Some(match dest {
        Dest::FixturePage("Graphic EQ") => Box::new(GraphicEq {
            selected: 0,
            editing: false,
            settings: ctx.demo.eq.clone(),
            bands: [Rect::default(); 10],
            confirm: Rect::default(),
        }),
        Dest::Spoken(SpokenPage::Info { item, preview }) => {
            Box::new(ItemInfo::new(*item, *preview, false))
        }
        Dest::Spoken(SpokenPage::Section {
            title: "Search Books",
            ..
        }) => Box::new(TextEntry::new(
            "Search Books",
            "",
            Kind::Full,
            false,
            Box::new(|text, ctx| {
                if text.is_empty() {
                    return Action::None;
                }
                ctx.demo.fields.insert("Book Search", text);
                Action::Push(crate::ui::screens::open(
                    Dest::Spoken(SpokenPage::Section {
                        podcast: false,
                        title: "Search Results",
                    }),
                    ctx,
                ))
            }),
        )),
        Dest::FixturePage("Sign In") => Box::new(InfoPage {
            text: InfoText::default(),
            back_target: None,
        }),
        dest => settings::screen(dest, ctx).or_else(|| match dest {
            Dest::FixturePage(page) => text_editor(page, ctx),
            _ => None,
        })?,
    })
}

fn text_editor(id: &'static str, ctx: &Ctx) -> Option<Box<dyn Screen>> {
    let (title, default, kind, secret) = match id {
        "Server Address" => (id, "https://music.example.test", Kind::Url, false),
        "User" => (id, "listener", Kind::Full, false),
        "Password" => (id, "", Kind::Full, true),
        "ListenBrainz API Token" => (id, "", Kind::Full, true),
        "Client Contact URL" => (id, "", Kind::Url, false),
        "API Token" => ("Audiobookshelf API Token", "", Kind::Full, true),
        "Audiobookshelf Address" => ("Audiobookshelf Address", "https://books.example.test", Kind::Url, false),
        "Password for Listening Room" | "Password for Guest Network" => (id, "", Kind::Full, true),
        _ => return None,
    };
    let entry = TextEntry::new(
        title,
        if secret { "" } else { field(ctx, id, default) },
        kind,
        secret,
        Box::new(move |text, ctx| {
            if id == "Password for Listening Room" || id == "Password for Guest Network" {
                if text.len() < 8 {
                    return Action::Toast("Enter at least 8 characters".into());
                }
                let index = usize::from(id == "Password for Guest Network");
                ctx.demo.network = Some(index);
                ctx.demo.values.insert(NETWORKS[index].0, 1);
                pop_toast(format!("Connected to {}", NETWORKS[index].0))
            } else {
                ctx.demo.fields.insert(id, text);
                Action::Pop
            }
        }),
    );
    Some(Box::new(entry))
}

pub(crate) fn hits(rect: Rect, x: i32, y: i32) -> bool {
    (rect.x..rect.right()).contains(&x) && (rect.y..rect.bottom()).contains(&y)
}

pub(crate) fn center_target(d: &mut Draw, help: &str, cx: i32, y: i32) -> Rect {
    let (prefix, _) = help.split_once("Center:").expect("adjustment hint has a Center action");
    let width = d.fonts.measure(help, 16, Weight::Regular);
    let start = d.fonts.measure(prefix, 16, Weight::Regular);
    Rect::new(cx - width / 2 + start, y, width - start, d.fonts.line_height(16))
}

struct GraphicEq {
    selected: usize,
    editing: bool,
    settings: eq::Settings,
    bands: [Rect; 10],
    confirm: Rect,
}

impl Screen for GraphicEq {
    fn clickable(&self, x: i32, y: i32) -> bool {
        hits(self.confirm, x, y) || self.bands.iter().any(|&band| hits(band, x, y))
    }

    fn click(&mut self, x: i32, y: i32, ctx: &mut Ctx) -> Action {
        if hits(self.confirm, x, y) {
            return self.event(Event::Center, ctx);
        }
        let Some(index) = self.bands.iter().position(|&band| hits(band, x, y)) else {
            return Action::None;
        };
        self.selected = index;
        self.editing = true;
        let band = self.bands[index];
        let mid = band.y + band.h / 2;
        let level = ((mid - y) as f32 / 5.0).round() as i32;
        let delta = level.clamp(-12, 12) - i32::from(self.settings.bands[index]);
        if delta == 0 {
            Action::Stay
        } else {
            self.event(Event::Scroll(delta), ctx)
        }
    }

    fn title(&self) -> String {
        "Graphic EQ".into()
    }

    fn event(&mut self, event: Event, ctx: &mut Ctx) -> Action {
        match event {
            Event::Scroll(ticks) if self.editing => {
                let band = &mut self.settings.bands[self.selected];
                *band = i32::from(*band).saturating_add(ticks).clamp(-12, 12) as i8;
                self.settings.graphic_enabled = true;
                ctx.demo.eq = self.settings.clone();
            }
            Event::Scroll(ticks) => {
                self.selected = (self.selected as i32).saturating_add(ticks).clamp(0, 9) as usize
            }
            Event::Center => self.editing = !self.editing,
            Event::Back if self.editing => self.editing = false,
            Event::Back => return Action::Pop,
            _ => return Action::None,
        }
        Action::Stay
    }

    fn draw(&mut self, d: &mut Draw, area: Rect) {
        let cx = area.x + area.w / 2;
        let top = area.y + 67;
        let bottom = top + 120;
        let mid = (top + bottom) / 2;
        let frequency = eq::FREQUENCIES[self.selected].round() as u32;
        d.fonts.draw_centered(
            d.c,
            &format!(
                "{frequency} Hz   {:+} dB",
                self.settings.bands[self.selected]
            ),
            cx,
            area.y + 18,
            22,
            Weight::Bold,
            d.theme.text,
        );
        d.c.fill_rect(Rect::new(area.x + 35, mid, area.w - 70, 1), d.theme.faint);
        for (index, (label, band)) in eq::LABELS.iter().zip(self.settings.bands).enumerate() {
            let x = area.x + 48 + index as i32 * 31;
            self.bands[index] = Rect::new(x - 11, top - 4, 24, bottom - top + 9);
            let color = if index == self.selected {
                d.theme.sel_bottom
            } else {
                d.theme.muted
            };
            d.c.fill_rect(Rect::new(x, top, 2, bottom - top), d.theme.track);
            let y = mid - i32::from(band) * 5;
            d.c.fill_round_rect(Rect::new(x - 7, y - 4, 16, 9), 3, color);
            if index == self.selected && self.editing {
                d.c.fill_rect(Rect::new(x - 10, bottom + 25, 22, 3), color);
            }
            d.fonts
                .draw_centered(d.c, label, x + 1, bottom + 9, 12, Weight::Regular, color);
        }
        let help = if self.editing {
            "Wheel: level · Center: done"
        } else {
            "Wheel: band · Center: adjust"
        };
        d.fonts.draw_centered(d.c, help, cx, area.y + 230, 16, Weight::Regular, d.theme.muted);
        self.confirm = center_target(d, help, cx, area.y + 230);
        let status = if self.settings.graphic_enabled {
            "±12 dB · Automatic headroom"
        } else {
            "Off · Adjust a band to enable"
        };
        d.fonts.draw_centered(
            d.c,
            status,
            cx,
            area.y + 253,
            14,
            Weight::Regular,
            d.theme.muted,
        );
    }
}

struct ItemInfo {
    index: usize,
    preview: bool,
    local: bool,
    text: InfoText,
    center_target: Option<Rect>,
    back_target: Option<Rect>,
}

impl ItemInfo {
    fn new(index: usize, preview: bool, local: bool) -> Self {
        Self {
            index,
            preview,
            local,
            text: InfoText::default(),
            center_target: None,
            back_target: None,
        }
    }
}

impl Screen for ItemInfo {
    fn clickable(&self, x: i32, y: i32) -> bool {
        self.center_target.is_some_and(|r| hits(r, x, y)) || self.back_target.is_some_and(|r| hits(r, x, y))
    }

    fn click(&mut self, x: i32, y: i32, ctx: &mut Ctx) -> Action {
        if self.center_target.is_some_and(|r| hits(r, x, y)) {
            self.event(Event::Center, ctx)
        } else if self.back_target.is_some_and(|r| hits(r, x, y)) {
            self.event(Event::Back, ctx)
        } else {
            Action::None
        }
    }

    fn title(&self) -> String {
        if ITEMS[self.index].podcast {
            "Episode Info"
        } else {
            "Book Info"
        }
        .into()
    }

    fn event(&mut self, event: Event, ctx: &mut Ctx) -> Action {
        match event {
            Event::Scroll(ticks) => {
                self.text.scroll_by(ticks);
                Action::Stay
            }
            Event::Center if self.preview && playable_spoken(ctx, self.index, self.local) => {
                Action::Many(vec![Action::Pop, play_spoken(ctx, self.index, self.local)])
            }
            Event::Center if self.preview => Action::Toast("Not downloaded".into()),
            Event::Center => Action::Stay,
            Event::Back => Action::Pop,
            _ => Action::None,
        }
    }

    fn draw(&mut self, d: &mut Draw, area: Rect) {
        let item = ITEMS[self.index];
        let current = d
            .status
            .current()
            .is_some_and(|song| song.title == item.title);
        let (footer, center_label) =
            if self.preview && current && d.status.state == crate::playback::PlayState::Paused {
                ("Wheel: read · Center: resume · Return: back", "Center: resume")
            } else if self.preview {
                ("Wheel: read · Center: play · Return: back", "Center: play")
            } else {
                ("Wheel: read · Center: refresh · Return: back", "Center: refresh")
            };
        self.center_target = InfoText::footer_target(d.fonts, area, footer, center_label);
        self.back_target = InfoText::footer_target(d.fonts, area, footer, "Return: back");
        self.text.draw(d, area, footer, || {
            let mut lines = vec![(item.title.into(), true)];
            if item.podcast {
                lines.extend([(item.album.into(), false), (item.detail.into(), false)]);
            } else {
                lines.push((format!("Author: {}", item.artist), false));
                lines.push((format!("Narrator: {}", item.detail), false));
                if !item.album.is_empty() {
                    lines.push((format!("Series: {} #1", item.album), false));
                }
            }
            lines.extend([
                (
                    format!("Length: {}", crate::ui::duration(u64::from(item.seconds))),
                    false,
                ),
                (String::new(), false),
                ("Summary".into(), true),
                (item.summary.into(), false),
            ]);
            lines
        });
    }
}

struct InfoPage {
    text: InfoText,
    back_target: Option<Rect>,
}

impl Screen for InfoPage {
    fn clickable(&self, x: i32, y: i32) -> bool {
        self.back_target.is_some_and(|r| hits(r, x, y))
    }

    fn click(&mut self, x: i32, y: i32, ctx: &mut Ctx) -> Action {
        if self.clickable(x, y) { self.event(Event::Back, ctx) } else { Action::None }
    }

    fn title(&self) -> String {
        "Sign In".into()
    }

    fn event(&mut self, event: Event, _: &mut Ctx) -> Action {
        match event {
            Event::Back => Action::Pop,
            Event::Scroll(ticks) => {
                self.text.scroll_by(ticks);
                Action::Stay
            }
            _ => Action::None,
        }
    }

    fn draw(&mut self, d: &mut Draw, area: Rect) {
        self.back_target = InfoText::footer_target(d.fonts, area, "Return: back", "Return: back");
        self.text.draw(d, area, "Return: back", || {
            vec![
                ("Sign in on your Q2".into(), true),
                ("Tailscale sign-in requires a device".into(), false),
            ]
        });
    }
}
