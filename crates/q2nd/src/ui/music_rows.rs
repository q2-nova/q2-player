//! Album, artist and track rows shared by the player and browser demo.
use super::{Dest, list::{Row, RowAction}};
use std::sync::Arc;
use subsonic::{Album, Song};

pub fn album_row(a: &Album, detail: String) -> Row {
    Row::link(a.name.clone(), Dest::Album { id: a.id.clone(), name: a.name.clone() })
        .key(a.id.clone())
        .detail(detail)
        .art(a.cover_art.clone())
}

pub fn artist_row(a: &subsonic::Artist) -> Row {
    Row::link(a.name.clone(), Dest::Artist { id: a.id.clone(), name: a.name.clone() })
        .key(a.id.clone()).detail(if a.album_count == 1 { "1 album".to_string() } else { format!("{} albums", a.album_count) }).artist_art(a)
}

pub fn track_row(songs: &Arc<Vec<Song>>, index: usize, art: bool, track_numbers: bool) -> Row {
    let song = &songs[index];
    let title = match (track_numbers, song.track) {
        (true, Some(n)) => format!("{n}. {}", song.title),
        _ => song.title.clone(),
    };
    let row = Row::new(title, RowAction::Play { songs: songs.clone(), index })
        .key(format!("{}#{index}", song.id)).shared_song();
    if art {
        row.art(song.cover_art.clone()).detail(song.artist.clone())
    } else {
        row.value(crate::ui::duration(u64::from(song.duration)))
    }
}
