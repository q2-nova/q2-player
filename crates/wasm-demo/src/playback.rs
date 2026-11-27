use crate::Song;
use std::{cell::RefCell, sync::Arc};

pub use q2_machines::queue::{Play as PlayState, Repeat};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    Offline,
    Streaming,
}

#[derive(Default)]
pub struct Status {
    pub queue: Arc<Vec<Song>>,
    pub index: usize,
    pub state: PlayState,
    pub position_ms: u64,
    pub duration_ms: u64,
    pub generation: u64,
    pub seek_revision: u64,
    pub speed_override: Option<u16>,
    pub shuffle: bool,
    pub repeat: Repeat,
    pub error: Option<String>,
    pub origin: Option<Origin>,
    pub format: Option<String>,
    pub radio_title: Option<String>,
    pub sample_rate: u32,
}

impl Status {
    pub fn current(&self) -> Option<&Song> {
        self.queue.get(self.index)
    }

    pub fn select(&mut self, index: usize) {
        self.index = index;
        self.position_ms = 0;
        self.generation = self.generation.wrapping_add(1);
        self.duration_ms = self.current().map_or(0, |s| u64::from(s.duration) * 1000);
        self.sample_rate = self
            .current()
            .and_then(|song| song.sampling_rate)
            .unwrap_or(0);
        self.format = self
            .current()
            .map(Song::quality_label)
            .filter(|f| !f.is_empty());
        self.origin =
            self.current().map(
                |song| match song.spoken.as_ref().map(|spoken| &spoken.source) {
                    Some(crate::SpokenSource::Local { .. }) => Origin::Offline,
                    _ => Origin::Streaming,
                },
            );
    }
}

pub fn seek_position(position: u64, duration: u64) -> u64 {
    if duration == 0 {
        position
    } else {
        position.min(duration.saturating_sub(1000))
    }
}

pub enum Cmd {
    Scrub {
        generation: u64,
        position: Option<u64>,
        active: bool,
    },
    SetShuffle(bool),
    SetSpeed {
        generation: u64,
        percent: Option<u16>,
    },
    SetRepeat(Repeat),
    Enqueue {
        songs: Arc<Vec<Song>>,
        next: bool,
    },
}

#[derive(Default)]
pub struct Player {
    pending: RefCell<Vec<Cmd>>,
}

impl Player {
    pub fn send(&self, cmd: Cmd) {
        self.pending.borrow_mut().push(cmd);
    }
    pub fn take(&self) -> Vec<Cmd> {
        self.pending.take()
    }
}
