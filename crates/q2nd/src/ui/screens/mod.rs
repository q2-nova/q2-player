//! The screens, and where each destination leads.

pub mod spoken;
pub mod book_info;
mod spoken_browse;
pub mod coverflow;
pub(crate) mod controls;
mod music_sync;
pub mod home;
pub mod menu;
mod music_menu;
pub mod now_playing;
pub mod popup;
pub mod settings;
pub mod storage;
pub mod equalizer;
mod eq_curves;
pub mod sources;
pub mod text;
pub mod wifi;
pub(super) mod portal;
pub mod tailscale;
pub mod bluetooth;

use super::{Dest, Screen};
use menu::Menu;
use sources::*;
use subsonic::calls;

pub fn open(dest: Dest, ctx: &super::Ctx) -> Box<dyn Screen> {
    match dest {
        Dest::BookInfo { target, .. } => Box::new(book_info::BookInfoScreen::new(target)),
        Dest::Spoken(page) => spoken_browse::open(page),
        Dest::RadioStations { featured } => Menu::boxed(Radios { featured }),
        Dest::Music => Menu::boxed(MusicMenu),
        Dest::CoverFlow => Box::new(coverflow::CoverFlow::new()),
        Dest::Artists => Menu::boxed(Artists),
        Dest::Artist { id, name } => Menu::boxed(Artist { id, name }),
        Dest::Albums { kind, title } => Menu::boxed(Albums::new(kind, title)),
        Dest::Album { id, name } => Menu::boxed(AlbumPage::new(id, name)),
        Dest::Playlists => Menu::boxed(Playlists),
        Dest::Playlist { id, name } => Menu::boxed(PlaylistPage::new(id, name)),
        Dest::Genres => Menu::boxed(Genres),
        Dest::Genre { name } => Menu::boxed(GenrePage { name }),
        Dest::Favorites => Menu::boxed(Favorites),
        Dest::StarredSongs => Menu::boxed(Starred(StarredKind::Songs)),
        Dest::StarredAlbums => Menu::boxed(Starred(StarredKind::Albums)),
        Dest::StarredArtists => Menu::boxed(Starred(StarredKind::Artists)),
        Dest::TopSongs { artist } => Menu::boxed(SongsPage {
            title: format!("Top Songs: {artist}"),
            call: calls::top_songs(&artist, 50),
            empty: "No top songs\nThe server needs Last.fm for these",
        }),
        Dest::Similar { id, title } => Menu::boxed(SongsPage {
            title: format!("Like {title}"),
            call: calls::similar_songs(&id, 50),
            empty: "No similar songs\nThe server needs Last.fm for these",
        }),
        Dest::Search => text::search(),
        Dest::SearchResults { query } => Menu::boxed(SearchResults { query }),
        Dest::Downloads => Menu::boxed(Downloads),
        Dest::StorageUsage => Box::new(storage::Storage::new()),
        Dest::OfflineSet { kind, id, name } => Menu::boxed(OfflinePage { kind, id, name }),
        Dest::Queue => Menu::boxed(Queue { player: ctx.player.clone() }),
        Dest::Equalizer => Box::new(equalizer::Equalizer::new()),
        Dest::Settings => Box::new(settings::Settings::new(settings::Page::Root)),
        Dest::SettingsPage(p) => Box::new(settings::Settings::new(p)),
        Dest::Wifi => Box::new(wifi::Wifi::new()),
        Dest::Tailscale => Box::new(tailscale::Tailscale::new()),
        Dest::Bluetooth => Box::new(bluetooth::Bluetooth::new()),
    }
}

fn hits(rect: crate::gfx::Rect, x: i32, y: i32) -> bool {
    (rect.x..rect.right()).contains(&x) && (rect.y..rect.bottom()).contains(&y)
}

fn center_target(d: &mut super::Draw, help: &str, cx: i32, y: i32) -> crate::gfx::Rect {
    use crate::gfx::{Rect, Weight};
    let (prefix, _) = help.split_once("Center:").expect("adjustment hint has a Center action");
    let width = d.fonts.measure(help, 16, Weight::Regular);
    let start = d.fonts.measure(prefix, 16, Weight::Regular);
    Rect::new(cx - width / 2 + start, y, width - start, d.fonts.line_height(16))
}

fn slider_step(bar: crate::gfx::Rect, x: i32, steps: i32) -> i32 {
    let width = bar.w - 2;
    let offset = (x - bar.x - 1).clamp(0, width);
    (offset * steps + width / 2) / width
}

#[cfg(test)]
mod touch_tests {
    use super::*;
    use crate::gfx::{HEIGHT, WIDTH};
    use crate::ui::{Action, settings_tests::fixture, theme::{ROW_H, STATUS_H}};

    #[test]
    fn popup_tap_chooses_the_rendered_row_and_ignores_outside() {
        let mut app = fixture();
        app.push(Box::new(popup::Popup::new("Two choices")
            .add("First", |_| Action::Toast("First chosen".into()))
            .add("Second", |_| Action::Toast("Second chosen".into()))));
        let id = app.top_id();
        let x = WIDTH / 2;
        let popup_top = STATUS_H + (HEIGHT - STATUS_H - 34 - 2 * ROW_H - 8) / 2;
        let y = popup_top + 34 + ROW_H + ROW_H / 2;
        assert_eq!(app.with_screen(id, |screen, _| screen.clickable(x, y)), Some(false));
        app.render();
        app.with_screen(id, |screen, ctx| {
            assert!(!screen.clickable(x, popup_top + 12), "title is passive");
            assert!(!screen.clickable(16, y), "background lies outside the dialog");
            assert!(screen.clickable(x, y));
            let Action::Many(actions) = screen.click(x, y, ctx) else {
                panic!("the tap must run the selected popup handler");
            };
            assert!(matches!(actions.as_slice(), [Action::Pop, Action::Toast(text)] if text == "Second chosen"));
        }).unwrap();
    }

    #[test]
    fn search_cells_enter_a_character_and_confirm_the_query() {
        let mut app = fixture();
        app.push(Box::new(text::TextEntry::new("Search", "", text::Kind::Search, false,
            Box::new(|query, _| Action::Toast(query)))));
        let id = app.top_id();
        let y = HEIGHT - 80;
        app.render();
        app.with_screen(id, |screen, ctx| {
            let first_letter = WIDTH / 2;
            assert!(screen.clickable(first_letter, y));
            assert!(matches!(screen.click(first_letter, y, ctx), Action::None));
        }).unwrap();
        app.render();
        app.with_screen(id, |screen, ctx| {
            let confirm = WIDTH / 2 - 3 * 34;
            assert!(matches!(screen.click(confirm, y, ctx), Action::Toast(query) if query == "a"));
        }).unwrap();
    }

    #[test]
    fn brightness_tap_uses_the_settings_update_path() {
        let mut app = fixture();
        app.push(Box::new(settings::Settings::new(settings::Page::Brightness)));
        let id = app.top_id();
        app.render();
        app.with_screen(id, |screen, ctx| {
            let (x, y) = (45, STATUS_H + 137);
            assert!(screen.clickable(x, y));
            assert!(matches!(screen.click(x, y, ctx), Action::Stay));
            assert_eq!(ctx.config.display.brightness, 1);
        }).unwrap();
    }
}
