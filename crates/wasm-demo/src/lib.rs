//! Runs the device's portable screens against in-memory playback and library
//! fixtures. The browser supplies input and time and renders the resulting
//! frames and audio transport.
#![allow(dead_code, unexpected_cfgs)]

extern crate self as subsonic;

mod artwork;
mod clock;
mod config;
mod destinations;
mod library;
pub mod playback;
mod session;
mod ui;
#[path = "../../shared/timezones.rs"]
mod timezones;

#[path = "../../subsonic/src/document.rs"]
mod document;
#[path = "../../subsonic/src/error.rs"]
mod error;
#[path = "../../q2nd/src/gfx/mod.rs"]
#[allow(deprecated)]
mod gfx;
#[path = "../../q2nd/src/gfx_msa.rs"]
mod gfx_msa;
#[path = "../../subsonic/src/models.rs"]
mod models;
#[path = "../../q2nd/src/resize_msa.rs"]
mod resize_msa;

pub use error::{Error, Result};
pub use models::*;

mod audio {
    pub use crate::playback as player;
}

mod input {
    #[derive(Clone, Copy)]
    pub enum Event {
        Scroll(i32),
        Center,
        CenterLong,
        Back,
        WheelEnd,
        BackLong,
    }
}

#[derive(Clone, Copy, Debug)]
pub enum AlbumListType {
    Random,
    Newest,
    Frequent,
    Recent,
    Starred,
    AlphabeticalByName,
    AlphabeticalByArtist,
    Highest,
    ByGenre,
    ByYear,
}

#[derive(Clone, Copy)]
pub enum ItemKind {
    Song,
    Album,
    Artist,
}

pub mod calls {
    use crate::AlbumListType;
    pub struct AlbumRequest {
        pub kind: AlbumListType,
        pub size: u32,
        pub offset: u32,
    }

    pub fn album_list(kind: AlbumListType, size: u32, offset: u32) -> AlbumRequest {
        AlbumRequest { kind, size, offset }
    }
}

mod ux {
    #[derive(Clone, Copy)]
    pub enum Code {
        Offline,
        NetworkUnreachable,
        NavidromeUnreachable,
        AbsUnreachable,
        Stuck,
    }

    impl Code {
        pub fn message(self) -> &'static str {
            match self {
                Self::Offline => "Offline",
                Self::NetworkUnreachable => "Network unavailable",
                Self::NavidromeUnreachable => "Navidrome unavailable",
                Self::AbsUnreachable => "Audiobookshelf unavailable",
                Self::Stuck => "Taking longer than expected",
            }
        }
    }

    #[derive(Clone, Copy)]
    pub enum Journey {
        Buffering,
        OpenPage,
    }
    pub fn begin(_: Journey) {}
    pub fn fail(_: Code, _: Journey, _: u32) {}
    pub fn pending_audio() -> Option<Journey> {
        None
    }
    pub fn present_in(text: &str, _: Journey) -> String {
        text.to_owned()
    }
}

// Compile-time adapters for native-only helpers in shared drawing modules.
// Wasm loads the primary fonts from bundled bytes and cannot open files.
mod rt {
    pub struct Blocking;
    pub struct Jobs;
    pub fn jobs() -> Jobs {
        Jobs
    }
    impl Jobs {
        pub fn run_isolated(&self, run: impl FnOnce(&Blocking)) {
            run(&Blocking);
        }
    }
}

mod resources {
    use std::{
        io,
        path::{Path, PathBuf},
    };
    pub fn directory(name: &str) -> PathBuf {
        name.into()
    }
    pub fn read(_: &crate::rt::Blocking, _: &Path, _: usize) -> io::Result<Vec<u8>> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

mod platform {
    pub mod resident {
        use std::{io, path::Path};
        pub fn map_file(_: &crate::rt::Blocking, _: &Path, _: usize) -> io::Result<&'static [u8]> {
            Err(io::ErrorKind::Unsupported.into())
        }
        pub fn trim_after_browsing() {}
    }
}

use session::Session;
use std::cell::RefCell;

thread_local! {
    static DEMO: RefCell<Option<Session>> = const { RefCell::new(None) };
}

#[unsafe(no_mangle)]
pub extern "C" fn demo_init() {
    clock::set_elapsed(0);
    DEMO.with_borrow_mut(|demo| *demo = Some(Session::new()));
}

#[unsafe(no_mangle)]
pub extern "C" fn demo_input(code: i32) {
    DEMO.with_borrow_mut(|demo| {
        if let Some(demo) = demo {
            demo.input(code);
        }
    });
}

/// Hit-test a point in logical framebuffer pixels against the topmost screen.
#[unsafe(no_mangle)]
pub extern "C" fn demo_clickable(x: i32, y: i32) -> u32 {
    read_demo(|demo| u32::from(demo.clickable(x, y)))
}

#[unsafe(no_mangle)]
pub extern "C" fn demo_click(x: i32, y: i32) -> u32 {
    DEMO.with_borrow_mut(|demo| u32::from(demo.as_mut().is_some_and(|session| session.click(x, y))))
}

/// Pointer to the current tightly packed 375×320 RGBA8 frame in Wasm memory.
#[unsafe(no_mangle)]
pub extern "C" fn demo_frame(elapsed_ms: u32) -> *const u8 {
    DEMO.with_borrow_mut(|demo| {
        demo.as_mut()
            .map_or(std::ptr::null(), |demo| demo.frame(elapsed_ms))
    })
}

/// An accessibility summary of the current screen and selection, in UTF-8.
#[unsafe(no_mangle)]
pub extern "C" fn demo_label() -> *const u8 {
    DEMO.with_borrow(|demo| {
        demo.as_ref()
            .map_or(std::ptr::null(), |demo| demo.label().as_ptr())
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn demo_label_len() -> usize {
    DEMO.with_borrow(|demo| demo.as_ref().map_or(0, |demo| demo.label().len()))
}

fn read_demo<T: Default>(read: impl FnOnce(&Session) -> T) -> T {
    DEMO.with_borrow(|demo| demo.as_ref().map(read).unwrap_or_default())
}

/// Pointer to the current fixture's UTF-8 Song ID. Read before the next input or frame.
#[unsafe(no_mangle)]
pub extern "C" fn demo_media_id() -> *const u8 {
    read_demo(|demo| {
        demo.status()
            .current()
            .map_or(std::ptr::null(), |song| song.id.as_ptr())
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn demo_media_id_len() -> usize {
    read_demo(|demo| demo.status().current().map_or(0, |song| song.id.len()))
}

/// Media kinds: 0 empty, 1 music, 2 radio, 3 books and podcasts.
#[unsafe(no_mangle)]
pub extern "C" fn demo_media_kind() -> u32 {
    read_demo(|demo| match demo.status().current() {
        None => 0,
        Some(song) if song.radio_url.is_some() => 2,
        Some(song) if song.spoken.is_some() => 3,
        Some(_) => 1,
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn demo_playing() -> u32 {
    read_demo(|demo| u32::from(demo.status().state == playback::PlayState::Playing))
}

#[unsafe(no_mangle)]
pub extern "C" fn demo_position_ms() -> u32 {
    read_demo(|demo| demo.status().position_ms as u32)
}

#[unsafe(no_mangle)]
pub extern "C" fn demo_duration_ms() -> u32 {
    read_demo(|demo| demo.status().duration_ms as u32)
}

/// Changes when playback selects a track or restarts the current one.
#[unsafe(no_mangle)]
pub extern "C" fn demo_playback_generation() -> u32 {
    read_demo(|demo| demo.status().generation as u32)
}

/// Changes on a committed scrub or a spoken-media skip within the track.
#[unsafe(no_mangle)]
pub extern "C" fn demo_seek_revision() -> u32 {
    read_demo(|demo| demo.status().seek_revision as u32)
}

#[unsafe(no_mangle)]
pub extern "C" fn demo_volume() -> u32 {
    read_demo(|demo| u32::from(demo.volume()))
}

#[unsafe(no_mangle)]
pub extern "C" fn demo_speed_percent() -> u32 {
    read_demo(|demo| u32::from(demo.speed_percent()))
}

#[unsafe(no_mangle)]
pub extern "C" fn demo_wheel_is_volume() -> u32 {
    read_demo(|demo| u32::from(demo.wheel_is_volume()))
}

#[unsafe(no_mangle)]
pub extern "C" fn demo_online() -> u32 {
    read_demo(|demo| u32::from(demo.online()))
}

#[unsafe(no_mangle)]
pub extern "C" fn demo_external_clock(enabled: u32) {
    DEMO.with_borrow_mut(|demo| {
        if let Some(demo) = demo {
            demo.set_external_clock(enabled != 0);
        }
    });
}

/// Configured time-zone identifier, in UTF-8, for the host's wall clock.
#[unsafe(no_mangle)]
pub extern "C" fn demo_timezone_ptr() -> *const u8 {
    read_demo(|demo| demo.timezone().as_ptr())
}

#[unsafe(no_mangle)]
pub extern "C" fn demo_timezone_len() -> usize {
    read_demo(|demo| demo.timezone().len())
}

/// Local wall-clock minutes since midnight. Values outside the day are unavailable.
#[unsafe(no_mangle)]
pub extern "C" fn demo_local_minutes(minutes: u32) {
    DEMO.with_borrow_mut(|demo| {
        if let Some(demo) = demo {
            demo.set_local_minutes(minutes);
        }
    });
}

/// Updates the playhead from browser media after the current seek has landed.
#[unsafe(no_mangle)]
pub extern "C" fn demo_media_progress(
    generation: u32,
    seek_revision: u32,
    position_ms: u32,
    duration_ms: u32,
) {
    DEMO.with_borrow_mut(|demo| {
        if let Some(demo) = demo {
            demo.media_progress(generation, seek_revision, position_ms, duration_ms);
        }
    });
}

#[unsafe(no_mangle)]
pub extern "C" fn demo_media_ended(generation: u32) {
    DEMO.with_borrow_mut(|demo| {
        if let Some(demo) = demo {
            demo.media_ended(generation);
        }
    });
}
