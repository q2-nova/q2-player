//! The Music menu shared by the device and the browser simulator.
use crate::ui::{Dest, list::Row};
use subsonic::AlbumListType;

pub fn rows() -> Vec<Row> {
    let list = |kind, title: &str| Row::link(title, Dest::Albums { kind, title: title.into() });
    vec![
        Row::link("Cover Flow", Dest::CoverFlow),
        Row::link("Playlists", Dest::Playlists),
        Row::link("Artists", Dest::Artists),
        list(AlbumListType::AlphabeticalByName, "Albums"),
        Row::link("Genres", Dest::Genres),
        Row::link("Favorites", Dest::Favorites),
        list(AlbumListType::Newest, "Recently Added"),
        list(AlbumListType::Recent, "Recently Played"),
        list(AlbumListType::Frequent, "Most Played"),
        list(AlbumListType::Highest, "Top Rated"),
        list(AlbumListType::Random, "Random Albums"),
        Row::link("Search", Dest::Search),
    ]
}
