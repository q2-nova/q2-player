//! Mixed downloads ranked by on-card audio allocation.
use super::super::menu::{RowsTask, Source};
use super::super::spoken::{Page, SpokenPage, is_episode};
use crate::library::{LibEvent, Library, Policy};
use crate::ui::list::{Badge, Row, RowAction};
use crate::ui::{Action, Ctx, Screen};
use std::sync::Arc;
use subsonic::Song;

pub(super) struct DownloadedItems;

fn spoken_page(song: &Song) -> SpokenPage {
    SpokenPage(if is_episode(song) {
        Page::DownloadedPodcasts
    } else {
        Page::DownloadedBooks
    })
}

impl Source for DownloadedItems {
    fn title(&self) -> String {
        "Downloaded Items".into()
    }

    fn media(&self) -> bool {
        true
    }
    fn local(&self) -> bool {
        true
    }
    fn immediate(&self) -> bool {
        false
    }
    fn download_badges(&self) -> bool {
        false
    }

    fn empty(&self) -> &'static str {
        "No downloaded items"
    }

    fn load(self: Arc<Self>, lib: Arc<Library>, _: Policy) -> RowsTask {
        Box::pin(async move {
            let items = lib.storage_downloads().await?;
            Ok(super::super::menu::on_pool(lib, |_| items.into_iter().map(row).collect()).await)
        })
    }

    fn cmd(&self, cmd: u32, row: &Row, ctx: &mut Ctx) -> Action {
        row.song_item()
            .map_or(Action::None, |song| spoken_page(song).cmd(cmd, row, ctx))
    }

    fn menu(&self, row: &Row, ctx: &mut Ctx) -> Option<Box<dyn Screen>> {
        let song = row.song_item().filter(|song| song.spoken.is_some())?;
        spoken_page(song).menu(row, ctx)
    }

    fn reset_on(&self, event: &LibEvent) -> bool {
        matches!(event, LibEvent::SpokenChanged)
    }

    fn reload_on(&self, event: &LibEvent) -> bool {
        matches!(event, LibEvent::Downloads | LibEvent::Online(_)) || self.reset_on(event)
    }
}

fn row((song, bytes): (Song, u64)) -> Row {
    let kind = if is_episode(&song) {
        "Podcast episode"
    } else if song.spoken.is_some() {
        "Audiobook"
    } else {
        "Song"
    };
    let key = song.spoken.as_ref().map_or_else(
        || format!("song:{}", song.id),
        |spoken| format!("spoken:{}", spoken.id),
    );
    let action = if song.spoken.is_some() {
        RowAction::Cmd(1)
    } else {
        RowAction::Play {
            songs: Arc::new(vec![song.clone()]),
            index: 0,
        }
    };
    let row = Row::new(song.title.clone(), action)
        .key(key)
        .detail(format!("{} audio · {kind}", super::bytes(bytes)))
        .art(song.cover_art.clone())
        .badge(Badge::Offline);
    if song.spoken.is_some() {
        row.song(song)
    } else {
        row.shared_song()
    }
}
