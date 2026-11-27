//! Implementation-derived UI evidence. This module exists only in test builds.
//!
//! Screens expose their actual controls; the walker replays real App events.
//! Fixture data, traversal bounds and omissions are part of the output contract.
use super::*;
use super::list::{ListView, Row, RowAction};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone)]
pub(crate) struct Surface {
    kind: &'static str,
    state: String,
    rows: Vec<Row>,
    selected: usize,
    controls: Vec<usize>,
    contextual: bool,
}
impl Surface {
    pub(crate) fn rows(&self) -> &[Row] { &self.rows }
    pub(crate) fn selected(&self) -> usize { self.selected }
    pub(crate) fn controls(&self) -> &[usize] { &self.controls }
    pub(crate) fn state(&self) -> &str { &self.state }
    pub(crate) fn with_state(mut self, state: String) -> Self { self.state = state; self }
    pub(crate) fn contextual(mut self) -> Self { self.contextual = true; self }
    pub(crate) fn list(list: &ListView) -> Self {
        Self { kind: "list", state: String::new(), rows: list.rows.clone(), selected: list.selected, controls: list.control_indices(), contextual: false }
    }
    pub(crate) fn widget(kind: &'static str, state: String) -> Self {
        Self { kind, state, rows: Vec::new(), selected: 0, controls: Vec::new(), contextual: false }
    }
    fn signature(&self) -> String {
        format!("{}:{}:{}", self.kind, self.state,
            self.rows.iter().map(|r| format!("{}:{}:{}", r.title, target(&r.action), r.separator)).collect::<Vec<_>>().join("|"))
    }
}

fn target(action: &RowAction) -> String {
    match action {
        RowAction::None => "information".into(),
        RowAction::Open(Dest::BookInfo { target, title }) => format!("BookInfo {{ target: {}, title: {title:?} }}", info_target(target)),
        RowAction::Open(dest) => format!("{dest:?}"),
        RowAction::Cmd(cmd) => format!("command {cmd}"),
        RowAction::Play { songs, index } => format!("play item {} of {}", index + 1, songs.len()),
        RowAction::Shuffle(songs) => format!("shuffle {} items", songs.len()),
    }
}

/// Generated accounts use an ephemeral server port; map display omits only the
/// account namespace. Duplicate checks still retain its full scoped identity.
pub(crate) fn info_target(target: &crate::library::book_info::BookInfoTarget) -> String {
    use crate::library::book_info::BookInfoTarget;
    match target {
        BookInfoTarget::Local { relative_path } => format!("local:{relative_path}"),
        BookInfoTarget::Shelf { library, item, episode, .. } => format!("shelf:{library:?}:{item}:{episode:?}"),
    }
}

fn effects(app: &App) -> String {
    let mut observed = Vec::new();
    if let Some((message, _)) = &app.toast { observed.push(format!("toast {message:?}")); }
    for request in &app.pending {
        observed.push(match request {
            Request::EnterPower { off } => format!("enter power off={off}"),
            Request::Sleep => "request Sleep".into(), Request::PowerOff => "request PowerOff".into(),
            Request::Restart => "request Restart".into(),
            Request::RestartReady => "restart storage complete".into(),
            Request::UsbDrive => "request UsbDrive".into(), Request::SetBrightness(value) => format!("request Brightness {value}"),
            Request::Wifi(value) => format!("request Wi-Fi {value}"),
            Request::OfflineMode { offline, .. } => format!("request Offline Mode {offline}"),
            Request::Toast(message) => format!("request toast {message:?}"),
        });
    }
    if observed.is_empty() { String::new() } else { format!("; {}", observed.join("; ")) }
}

fn destination_key(dest: &Dest) -> String {
    // Display names do not make a distinct destination. Keep this exhaustive:
    // a new destination must have an explicit identity for duplicate checks.
    let mut identity = dest.clone();
    match &mut identity {
        Dest::Settings | Dest::SettingsPage(screens::settings::Page::Root) => return "settings".into(),
        Dest::Album { name, .. } | Dest::Artist { name, .. } | Dest::Playlist { name, .. } |
        Dest::OfflineSet { name, .. } => name.clear(),
        Dest::Albums { title, .. } | Dest::Similar { title, .. } | Dest::BookInfo { title, .. } => title.clear(),
        Dest::Spoken(screens::spoken::Page::Libraries { title, .. } | screens::spoken::Page::Browse { title, .. }) => title.clear(),
        Dest::Music | Dest::RadioStations { .. } | Dest::CoverFlow | Dest::Artists | Dest::Playlists | Dest::Genres |
        Dest::Genre { .. } | Dest::Favorites | Dest::StarredSongs | Dest::StarredAlbums | Dest::StarredArtists |
        Dest::TopSongs { .. } | Dest::Search | Dest::SearchResults { .. } | Dest::Downloads | Dest::StorageUsage |
        Dest::Queue | Dest::Equalizer | Dest::SettingsPage(_) | Dest::Wifi | Dest::Tailscale | Dest::Bluetooth |
        Dest::Spoken(_) => {},
    }
    // Preserve every non-display field, including fields added to nested pages.
    format!("{identity:?}")
}

/// The duplicate check is deliberately independent of snapshot approval.
/// Same-page aliases need an explicit reason here, alongside their journey test.
fn validate_rows(title: &str, surface: &Surface) -> Result<(), String> {
    let mut keys = BTreeSet::new();
    let mut destinations = BTreeMap::new();
    for row in &surface.rows {
        if !keys.insert(&row.key) { return Err(format!("{title}: repeated stable row key {:?}", row.key)); }
        if let RowAction::Open(dest) = &row.action {
            let dest = destination_key(dest);
            if let Some(previous) = destinations.insert(dest.clone(), row.title.clone()) {
                return Err(format!("{title}: unexplained duplicate destination {dest}: {previous} / {}", row.title));
            }
        }
    }
    Ok(())
}

#[derive(Clone)]
struct Scenario {
    name: &'static str,
    configured: bool,
    installed: bool,
    shelf: Option<bool>,
    song: Option<subsonic::Song>,
    ratings: crate::config::RatingControls,
}

fn generated_song(spoken: Option<bool>, radio: bool) -> subsonic::Song {
    subsonic::Song {
        id: "fixture-song".into(), title: "Generated Title".into(),
        artist: "Generated Artist".into(), artist_id: Some("fixture-artist".into()),
        album: "Generated Album".into(), album_id: Some("fixture-album".into()),
        duration: 240, cover_art: Some("fixture-cover".into()),
        spoken: spoken.map(|podcast| subsonic::Spoken {
            id: "fixture-logical".into(), duration_ms: 240_000,
            source: subsonic::SpokenSource::Audiobookshelf {
                namespace: "fixture-account".into(), item: "fixture-book-or-series".into(),
                episode: podcast.then(|| "fixture-episode".into()), content_url: "generated.flac".into(),
            }, ..Default::default()
        }).map(Box::new),
        radio_url: radio.then(|| "https://radio.invalid/generated".into()),
        ..Default::default()
    }
}

fn seed_music(lib: &Library) {
    use serde_json::json;
    use subsonic::{calls, AlbumListType};
    let song = generated_song(None, false);
    let artist = json!({"id":"fixture-artist", "name":"Generated Artist", "albumCount":1});
    let album = json!({"id":"fixture-album", "name":"Generated Album", "artist":"Generated Artist", "artistId":"fixture-artist", "songCount":1, "coverArt":"fixture-cover"});
    {
        let blocking = crate::rt::Blocking::dedicated_thread(); let mut db = lib.db();
        let build = db.catalog_begin(&blocking).unwrap();
        let summary: subsonic::Album = serde_json::from_value(album.clone()).unwrap();
        let label = crate::ui::list::index_letter(&summary.name);
        db.catalog_append(&blocking, &build, 0, &[(summary, label)]).unwrap();
        db.catalog_publish(&blocking, &build).unwrap();
    }
    let playlist = json!({"id":"fixture-playlist", "name":"Generated Playlist", "owner":"generated-user", "songCount":1});
    let put = |key: String, body: serde_json::Value| lib.db().put_response(&crate::rt::Blocking::dedicated_thread(), &key, &body.to_string());
    put(calls::artists().request.cache_key(), json!({"artists":{"index":[{"name":"G","artist":[artist.clone()]}]}}));
    let mut artist_page = artist.clone(); artist_page["album"] = json!([album.clone()]);
    put(calls::artist("fixture-artist").request.cache_key(), json!({"artist":artist_page}));
    let mut album_page = album.clone(); album_page["song"] = json!([song.clone()]);
    put(calls::album("fixture-album").request.cache_key(), json!({"album":album_page}));
    put(calls::playlists().request.cache_key(), json!({"playlists":{"playlist":[playlist.clone()]}}));
    let mut playlist_page = playlist.clone(); playlist_page["entry"] = json!([song.clone()]);
    put(calls::playlist("fixture-playlist").request.cache_key(), json!({"playlist":playlist_page}));
    put(calls::genres().request.cache_key(), json!({"genres":{"genre":[{"value":"Generated Genre", "songCount":1,"albumCount":1}]}}));
    put(calls::starred().request.cache_key(), json!({"starred2":{"artist":[artist], "album":[album.clone()], "song":[song.clone()]}}));
    for kind in [AlbumListType::AlphabeticalByName, AlbumListType::AlphabeticalByArtist, AlbumListType::Newest,
                 AlbumListType::Recent, AlbumListType::Frequent, AlbumListType::Highest, AlbumListType::Random] {
        for size in [30, 100, 500] {
            put(calls::album_list(kind, size, 0).request.cache_key(), json!({"albumList2":{"album":[album.clone()]}}));
        }
    }
    put(calls::top_songs("Generated Artist", 50).request.cache_key(), json!({"topSongs":{"song":[song.clone()]}}));
    put(calls::similar_songs("fixture-song", 50).request.cache_key(), json!({"similarSongs2":{"song":[song.clone()]}}));
    put(calls::songs_by_genre("Generated Genre", 500, 0).request.cache_key(), json!({"songsByGenre":{"song":[song]}}));
}

fn scenarios() -> Vec<Scenario> {
    use crate::config::RatingControls::{Both, Hearts, Stars};
    vec![
        Scenario { name: "unconfigured / empty queue", configured: false, shelf: None, installed: false, song: None, ratings: Stars },
        Scenario { name: "configured offline / music stars", configured: true, shelf: None, installed: false, song: Some(generated_song(None, false)), ratings: Stars },
        Scenario { name: "configured offline / music hearts", configured: true, shelf: None, installed: false, song: Some(generated_song(None, false)), ratings: Hearts },
        Scenario { name: "configured offline / music both", configured: true, shelf: None, installed: false, song: Some(generated_song(None, false)), ratings: Both },
        Scenario { name: "configured offline / book", configured: true, shelf: None, installed: false, song: Some(generated_song(Some(false), false)), ratings: Both },
        Scenario { name: "configured offline / podcast", configured: true, shelf: None, installed: false, song: Some(generated_song(Some(true), false)), ratings: Both },
        Scenario { name: "configured offline / radio", configured: true, shelf: None, installed: false, song: Some(generated_song(None, true)), ratings: Both },
        Scenario { name: "ABS generated cache / library chooser", configured: true, shelf: Some(false), installed: false, song: None, ratings: Both },
        Scenario { name: "ABS generated cache / separate defaults", configured: true, shelf: Some(true), installed: false, song: None, ratings: Both },
        Scenario { name: "mainline / installed storage", configured: true, shelf: None, installed: true, song: Some(generated_song(None, false)), ratings: Stars },
    ]
}

fn surface(app: &App) -> Surface { app.stack.last().unwrap().1.audit() }
fn top(app: &App) -> String { app.stack.last().unwrap().1.title() }
fn step(app: &mut App, event: Event) {
    // One intentional detent at a time. End a spin so host speed cannot engage
    // acceleration and make the generated path skip controls.
    app.event(event, &mut crate::platform::Headless { last: None });
    app.settle_settings_fixture();
    // The actual UI awaits these after each event. Do not carry an old
    // Offline request into unrelated descendant rows in the map.
    crate::rt::Ui::new().unwrap().block_on(app.apply_offline_requests());
    let id = app.top_id();
    app.with_screen(id, |screen, ctx| screen.audit_settle(ctx));
    if surface(app).kind == "BookInfoScreen" { app.render(); }
}

fn portal_fixtures(app: &mut App, output: &mut String) {
    use super::portal::State;

    fn choose(app: &mut App, label: &str) {
        let current = surface(app);
        let row = current.rows.iter().position(|row| row.title == label).expect("portal row present");
        let control = current.controls.iter().position(|&index| index == row).expect("portal row actionable");
        step(app, Event::Scroll(i32::MIN));
        step(app, Event::WheelEnd);
        if control > 0 { step(app, Event::Scroll(control as i32)); }
        assert_eq!(surface(app).rows[surface(app).selected].title, label);
        step(app, Event::Center);
    }

    fn publish(app: &mut App, state: State) {
        app.portal.state = state;
        let id = app.top_id();
        app.with_screen(id, |screen, ctx| screen.portal_changed(ctx));
    }

    output.push_str("\n## Wi-Fi sign-in and Delta account preparation\n\nGenerated lease and portal states drive the actual Wi-Fi screen and wheel controls. These probes cover the conditional controls, their immediate actions and Back. They do not connect a phone, reach a live provider or check media throughput.\n\n| Flow | Observed control | Wheel, Center or Back result |\n| --- | --- | --- |\n");
    app.net = Net::new(app.paths.root.join("generated-wifi"), true, Arc::new(|| {}));
    app.net.simulate_address(None);
    app.push(Box::new(screens::wifi::Wifi::new()));
    assert!(!surface(app).rows.iter().any(|row| row.title == "Wi-Fi Sign-In"));
    choose(app, "Prepare Delta Sign-In");
    assert_eq!(top(app), "Prepare Delta Sign-In");
    assert_eq!(surface(app).rows[0].detail.as_deref(), Some("Join the player's private Wi-Fi on your phone"));
    choose(app, "Start Phone Setup");
    publish(app, State::Pairing {
        ap: "WIFI:T:WPA;S:Generated-WiFi;P:fixture-only;;".into(),
        page: "http://192.0.2.1:8765/#fixture-only".into(),
    });
    let wifi_code = surface(app).state;
    assert_eq!(wifi_code, "Wi-Fi QR");
    step(app, Event::Center);
    let page_code = surface(app).state;
    assert_eq!(page_code, "Setup QR");
    step(app, Event::Center);
    assert_eq!(surface(app).state, wifi_code);
    step(app, Event::Back);
    assert_eq!(top(app), "Wi-Fi");
    assert_eq!(surface(app).rows[surface(app).selected].title, "Prepare Delta Sign-In");
    output.push_str(&format!("| Delta account prep without an address | Start Phone Setup → {wifi_code} | Center shows {page_code}; Center returns to the Wi-Fi QR; Back cancels and returns to {} |\n", top(app)));

    choose(app, "Prepare Delta Sign-In");
    choose(app, "Start Phone Setup");
    app.portal.prepare_fixture();
    publish(app, State::Prepared);
    assert_eq!(surface(app).rows[0].title, "Account Ready");
    choose(app, "Done");
    assert!(app.portal.has_prepared());
    output.push_str(&format!("| Account received | Account Ready | Done returns to {} and retains the prepared account |\n", top(app)));

    app.net.simulate_address(Some("192.0.2.10"));
    app.portal.observe(&app.net.state());
    let id = app.top_id();
    app.with_screen(id, |screen, ctx| screen.network_changed(ctx));
    choose(app, "Wi-Fi Sign-In");
    assert!(matches!(app.portal.state, State::Checking));
    publish(app, State::Guest);
    assert!(matches!(app.portal.state, State::Working));
    publish(app, State::Terms);
    let prompt = surface(app);
    assert_eq!(prompt.rows.iter().map(|row| row.title.as_str()).collect::<Vec<_>>(),
        ["Terms & Conditions apply", "Agree"]);
    assert_eq!(prompt.rows[prompt.selected].title, "Agree");
    assert!(app.stack.last().unwrap().1.script_ready());
    step(app, Event::CenterLong);
    assert!(matches!(app.portal.state, State::Terms));
    step(app, Event::Back);
    assert_eq!(top(app), "Wi-Fi");
    assert_eq!(surface(app).rows[surface(app).selected].title, "Wi-Fi Sign-In");
    assert!(!matches!(app.portal.state, State::Terms));
    output.push_str(&format!("| Guest agreement | Wi-Fi Sign-In → Terms & Conditions apply / Agree | Agree is selected immediately. Long Center leaves the prompt open. Back cancels and returns to {} |\n", top(app)));

    choose(app, "Wi-Fi Sign-In");
    publish(app, State::Guest);
    assert!(matches!(app.portal.state, State::Working));
    publish(app, State::Terms);
    step(app, Event::Center);
    assert!(matches!(app.portal.state, State::Working));
    publish(app, State::Guest);
    assert!(matches!(app.portal.state, State::Guest));
    assert_eq!(surface(app).state, "Retry");
    step(app, Event::Center);
    assert!(matches!(app.portal.state, State::Checking));
    publish(app, State::Online);
    assert_eq!(surface(app).rows[0].title, "Internet Available");
    output.push_str("| Guest agreement acceptance | Agree | Unverified access offers Retry. Verified access shows Internet Available |\n");

    publish(app, State::Delta);
    assert_eq!(surface(app).rows[0].title, "Use Prepared Account");
    app.portal.forget_prepared();
    publish(app, State::Delta);
    assert_eq!(surface(app).rows[0].title, "Enter Details on Phone");
    choose(app, "Enter Details on Phone");
    choose(app, "Start Phone Setup");
    step(app, Event::Back);
    assert!(matches!(app.portal.state, State::Checking));
    publish(app, State::Delta);
    assert_eq!(top(app), "Delta Wi-Fi");
    assert!(!app.portal.has_prepared());
    output.push_str("| Delta phone entry | Enter Details on Phone / Enter SkyMiles Details | Back cancels setup. A fresh check restores the Delta choices on the same network |\n");

    choose(app, "Enter Details on Phone");
    choose(app, "Start Phone Setup");
    app.net.simulate_address(Some("192.0.2.11"));
    app.portal.observe(&app.net.state());
    app.portal.prepare_fixture();
    publish(app, State::Prepared);
    choose(app, "Done");
    assert!(matches!(app.portal.state, State::Checking));
    assert!(surface(app).rows.is_empty());
    publish(app, State::Online);
    assert_eq!(surface(app).rows[0].title, "Internet Available");
    assert!(!surface(app).rows.iter().any(|row| row.title == "Use Prepared Account"));
    assert!(app.portal.has_prepared());
    publish(app, State::Delta);
    assert_eq!(surface(app).rows[0].title, "Use Prepared Account");
    output.push_str("| Account received during Delta sign-in | Done | Rechecks the current link. The prepared account stays available, and its sign-in action appears only after a Delta check |\n");

    choose(app, "Enter SkyMiles Details");
    assert_eq!(surface(app).state, "Loading");
    step(app, Event::Center);
    assert_eq!(surface(app).state, "Loading");
    publish(app, State::Online);
    assert_eq!(surface(app).rows[0].title, "Internet Available");
    publish(app, State::Delta);
    choose(app, "Enter SkyMiles Details");
    assert_eq!(surface(app).state, "Loading");
    publish(app, State::Failed("Finish sign-in in your browser".into()));
    assert_eq!(surface(app).state, "Retry");
    step(app, Event::Back);
    assert!(matches!(app.portal.state, State::Checking));
    publish(app, State::Delta);

    choose(app, "Enter SkyMiles Details");
    assert_eq!(surface(app).state, "Loading");
    publish(app, State::Credentials);
    let account = top(app);
    app.handle(UiMsg::ScriptRecordText, &mut crate::platform::Headless { last: None });
    for _ in 0..4 { step(app, Event::Center); }
    app.render();
    let recording = app.script_text().unwrap();
    assert!(recording["current"].as_array().unwrap().iter().any(|text| text == "••••"));
    assert!(!recording.to_string().contains("aaaa"));
    step(app, Event::CenterLong);
    let password = top(app);
    assert_eq!(password, "SkyMiles Password");
    app.text_record = None;
    step(app, Event::Back);
    assert!(matches!(app.portal.state, State::Checking));
    publish(app, State::Delta);
    assert_eq!(top(app), "Delta Wi-Fi");
    output.push_str(&format!("| Delta direct sign-in | {account} → {password} | The helper checks first. Only its credentials request opens the wheel editor. Return cancels and rechecks {} |\n", top(app)));

    choose(app, "Enter SkyMiles Details");
    publish(app, State::Credentials);
    step(app, Event::Center);
    step(app, Event::CenterLong);
    assert_eq!(top(app), "SkyMiles Password");
    app.net.simulate_address(Some("192.0.2.12"));
    step(app, Event::CenterLong);
    assert!(matches!(app.portal.state, State::Checking));
    assert_eq!(surface(app).state, "Loading");
    publish(app, State::Delta);
    choose(app, "Enter SkyMiles Details");
    publish(app, State::Credentials);
    for _ in 0..2 {
        step(app, Event::Center);
        step(app, Event::CenterLong);
    }
    assert!(matches!(app.portal.state, State::Working));
    publish(app, State::Delta);
    app.net.simulate_address(Some("192.0.2.13"));
    // The new lease is already in the owner snapshot. Its UiMsg::Net is pending.
    choose(app, "Use Prepared Account");
    assert!(matches!(app.portal.state, State::Checking));
    assert!(app.portal.has_prepared());
    publish(app, State::Delta);
    choose(app, "Use Prepared Account");
    assert!(matches!(app.portal.state, State::Working));
    step(app, Event::Back);
    assert_eq!(top(app), "Wi-Fi");
    assert!(app.portal.has_prepared());
    output.push_str("| Delta prepared-account sign-in | Use Prepared Account | Center rechecks a newly published lease without consuming the account. After Delta is verified, Center begins sign-in. Back before the password prompt returns to Wi-Fi and keeps the account ready |\n");
}
fn reset(app: &mut App, scenario: &Scenario, path: &[Event]) {
    let root = app.paths.root.clone();
    app.paths = if !scenario.installed { Paths::new(root.clone()) } else { Paths::installed(root.clone(), root.join("generated-card"), root.join("generated-run"), true, None) };
    app.config = Config::default();
    app.config.display.rating_controls = scenario.ratings;
    if scenario.configured {
        app.config.server.url = "http://127.0.0.1:9".into();
        app.config.server.user = "generated-user".into();
        app.config.server.password = "generated-password".into();
    }
    app.lib.configure(&app.config);
    app.lib.set_offline_mode(true);
    app.lib.reset_ui_fixture();
    if scenario.configured { seed_music(&app.lib); }
    if let Some(defaults) = scenario.shelf {
        crate::library::shelf_browse::seed_hierarchy(&app.lib, &mut app.config).unwrap();
        if defaults {
            use subsonic::audiobookshelf_browse::Media;
            app.config.audiobookshelf.default_books = app.lib.shelf_library_choices(Media::Book, crate::library::Policy::CacheOnly).unwrap().preference("a");
            app.config.audiobookshelf.default_podcasts = app.lib.shelf_library_choices(Media::Podcast, crate::library::Policy::CacheOnly).unwrap().preference("p");
        }
    }
    app.spoken_speed = std::rc::Rc::new(std::cell::RefCell::new(screens::spoken::PlaybackSpeed::current()));
    app.pending.clear();
    app.toast = None;
    app.status = Status::default();
    app.status.queue = Arc::new(scenario.song.clone().into_iter().collect());
    app.status.index = 0;
    app.status.generation = 1;
    app.status.position_ms = 12_000;
    app.status.duration_ms = 240_000;
    app.status.speed_percent = 100;
    app.status.shuffle = false;
    app.status.repeat = crate::audio::player::Repeat::Off;
    app.player.fixture_status(app.status.clone());
    app.stack.clear();
    app.push(Box::new(screens::home::Home::new()));
    let home = app.top_id();
    app.with_screen(home, |screen, ctx| screen.audit_settle(ctx));
    for &event in path { step(app, event); }
}

fn selected_path(app: &mut App, scenario: &Scenario, path: &[Event], wanted: usize) -> Vec<Event> {
    let mut next = path.to_vec();
    reset(app, scenario, path);
    let current = surface(app);
    next.push(Event::WheelEnd);
    if let Some(to) = current.controls.iter().position(|&i| i == wanted) {
        next.push(Event::Scroll(i32::MIN));
        next.push(Event::WheelEnd);
        next.push(Event::Scroll(to as i32));
    }
    next.push(Event::WheelEnd);
    next
}

fn escape(text: &str) -> String { text.replace('|', "\\|").replace('\n', " / ") }

fn journey(app: &mut App, scenario: &Scenario, labels: &[&str]) -> Vec<Event> {
    let mut path = Vec::new();
    for label in labels {
        reset(app, scenario, &path);
        let control = surface(app);
        let index = control.rows.iter().position(|row| row.title == *label)
            .unwrap_or_else(|| panic!("missing critical-journey menu {label:?} after {:?}", path));
        assert!(control.controls.contains(&index), "critical-journey row is actionable");
        path = selected_path(app, scenario, &path, index);
        path.push(Event::Center);
    }
    reset(app, scenario, &path);
    path
}

fn walk(app: &mut App, scenario: &Scenario, path: Vec<Event>, route: String,
        seen: &mut BTreeSet<String>, output: &mut String, depth: usize) {
    reset(app, scenario, &path);
    let title = top(app);
    let current = surface(app);
    validate_rows(&title, &current).unwrap();
    let identity = format!("{title}:{:?}:{}:{}:{}", (scenario.configured, scenario.shelf), scenario.installed, current.signature(), if current.contextual { scenario.name } else { "common" });
    if !seen.insert(identity) { return; }
    output.push_str(&format!("\n### {}\n\n", escape(&route)));
    output.push_str(&format!("Runtime screen: `{}`; control: `{}`; script control settled: `{}`.\n\n", escape(&title), current.kind, app.stack.last().unwrap().1.script_ready()));
    if !current.state.is_empty() { output.push_str(&format!("State: `{}`.\n\n", escape(&current.state))); }

    reset(app, scenario, &path);
    step(app, Event::Back);
    output.push_str(&format!("Back → `{}`{}; ", escape(&top(app)),
        if top(app) == title && app.stack.len() == 1 { " (root stays)" } else { "" }));
    reset(app, scenario, &path);
    let before = app.stack.len();
    step(app, Event::CenterLong);
    let held = app.stack.len() != before || top(app) != title;
    output.push_str(&format!("long Center → `{}`.\n", if held { escape(&top(app)) } else { "same screen".into() }));
    let mut children = Vec::new();
    if held && depth < 6 {
        let mut held_path = path.clone(); held_path.push(Event::CenterLong);
        children.push((held_path, format!("{route} → long Center")));
    }
    if !current.rows.is_empty() {
        output.push_str("\n| Row | Value | Actual row action | Tap result | Long Center | Back after tap |\n| --- | --- | --- | --- | --- | --- |\n");
        if current.rows.len() > 96 {
            use md5::{Digest, Md5};
            let labels = current.rows.iter().map(|r| format!("{}:{}", r.key, r.title)).collect::<Vec<_>>().join("\n");
            output.push_str(&format!("\nLarge data catalog: {} rows, full label/key MD5 `{:x}`; only the first two and last row are expanded below. These are data, not an exhaustive interaction traversal.\n\n", current.rows.len(), Md5::digest(labels)));
        }
        for (i, row) in current.rows.iter().enumerate().filter(|(i, _)| current.rows.len() <= 96 || *i < 2 || *i == current.rows.len() - 1) {
            let chosen = selected_path(app, scenario, &path, i);
            reset(app, scenario, &chosen);
            // Passive information rows are skipped by mixed settings lists.
            let reachable = current.controls.contains(&i) && surface(app).selected == i;
            let mut tap = chosen.clone(); tap.push(Event::Center);
            let (result, back) = if reachable {
                reset(app, scenario, &tap);
                let next = surface(app);
                let destination = top(app);
                let mut result = if destination == title {
                    match next.rows.iter().find(|next| next.key == row.key).and_then(|next| next.value.as_ref()) {
                        Some(value) if row.value.as_ref() != Some(value) => format!("{destination} ({} → {value})", row.title),
                        _ => destination.clone(),
                    }
                } else { destination.clone() };
                result.push_str(&effects(app));
                let changed = next.signature() != current.signature() || destination != title;
                if changed && depth < 6 && !matches!(row.action, RowAction::None) {
                    children.push((tap.clone(), format!("{route} → {}", row.title)));
                }
                step(app, Event::Back);
                (result, top(app))
            } else { ("passive row".into(), "—".into()) };
            reset(app, scenario, &chosen);
            let old_depth = app.stack.len();
            step(app, Event::CenterLong);
            let hold = if reachable && (app.stack.len() != old_depth || top(app) != title) {
                let mut p = chosen; p.push(Event::CenterLong);
                if depth < 6 { children.push((p, format!("{route} → hold {}", row.title))); }
                top(app)
            } else { "—".into() };
            let value = if title == "Storage Usage" { "filesystem-dependent" } else { row.value.as_deref().unwrap_or("") };
            output.push_str(&format!("| {}{} | {} | `{}` | {} | {} | {} |\n",
                if row.separator { "— " } else { "" }, escape(&format!("{}{}", row.title, if row.badge == list::Badge::Check { " ✓" } else { "" })), escape(value), escape(&target(&row.action)),
                escape(&result), escape(&hold), escape(&back)));
        }
    } else if current.kind == "list" {
        output.push_str("\nNo fixture rows: remote, unavailable, loading or empty state. This is **not coverage of its populated menu**.\n");
    } else if current.kind == "NowPlaying" {
        let mut taps = path.clone();
        let mut states = vec![current.state.clone()];
        let mut controls = Vec::new();
        for _ in 0..6 {
            taps.push(Event::Center); reset(app, scenario, &taps);
            let state = surface(app).state;
            states.push(state.clone());
            step(app, Event::Scroll(1));
            let wheel = surface(app).state;
            reset(app, scenario, &taps);
            step(app, Event::Back);
            let back = format!("{} / {}", surface(app).state, top(app));
            reset(app, scenario, &taps);
            step(app, Event::CenterLong);
            controls.push(format!("| `{}` | `{}` | `{}` | `{}` |", state, wheel, back, top(app)));
            if state == current.state { break; }
        }
        output.push_str(&format!("\nObserved Center-tap cycle: `{}`.\n", states.join(" → ")));
        output.push_str("\n| Mode after Center | Wheel +1 state | Back state / destination | Long Center destination |\n| --- | --- | --- | --- |\n");
        output.push_str(&controls.join("\n"));
        output.push('\n');
    }
    if current.rows.is_empty() && current.kind != "list" && current.kind != "NowPlaying" {
        output.push_str("\n| Actual widget event | Result | Observed control state | Back after event |\n| --- | --- | --- | --- |\n");
        for (label, event) in [("Center", Event::Center), ("Long Center", Event::CenterLong), ("Wheel +1", Event::Scroll(1)), ("Wheel −1", Event::Scroll(-1))] {
            let mut probe = path.clone(); probe.push(event);
            reset(app, scenario, &probe);
            let next = surface(app);
            let result = format!("{}{}", top(app), effects(app));
            if event == Event::Center && top(app) != title && depth < 6 {
                children.push((probe, format!("{route} → Center")));
            }
            step(app, Event::Back);
            output.push_str(&format!("| {label} | {} | `{}` | {} |\n", escape(&result), escape(&next.state), escape(&top(app))));
        }
    }
    if depth == 6 { output.push_str("\nTraversal bound reached; deeper paths are not covered by this fixture.\n"); }
    for (path, route) in children { walk(app, scenario, path, route, seen, output, depth + 1); }
}

#[test]
fn implementation_hierarchy_matches_reviewed_map() {
    let mut app = super::settings_tests::startup_fixture();
    app.hardware.touchscreen = true;
    app.player.shutdown();
    let (player, _commands) = PlayerHandle::fixture(Status::default());
    app.player = player;
    // Drop background jobs. The map settles generic menus synchronously through
    // their real CacheOnly Source::load. ABS seed generation contacts only its
    // temporary synthetic local server, which stops before UI traversal.
    app.background = false;
    let mut output = String::from("# Generated UI hierarchy and interactions\n\n\
Generated by `ui::hierarchy::implementation_hierarchy_matches_reviewed_map` from actual Screen controls and App events. Do not edit this file by hand.\n\n\
This is a bounded generated-fixture map, not proof of exhaustive UI or good UX. Every Screen implementation must expose its actual control in test builds. Toasts and pending app requests are observed, widget events use actual handlers, and Book Info metadata comes from its rendered lines. Generated account namespace hashes are omitted only from display; duplicate checks retain complete scoped identities. Same-page duplicate destinations and stable row keys are checked independently of snapshot approval.\n\n\
Coverage: generated ABS production-client/cache roots with chooser and separate book/podcast defaults; unconfigured/configured-offline roots, music stars/hearts/both, book, podcast, live radio and unavailable mainline stock handover; Home and local menus, generated music cache album/artist/playlist/genre routes, paused queue, Settings choices/confirmations/editors reachable within six edges, actual long-Center menus and Now Playing Center cycles, and observed Back targets. Repeated identical non-contextual surfaces are emitted once per capability context; context menus, playback modes, speed and info controls remain separate for every media/rating fixture. Script control readiness means an initial control is settled (including stable errors), not that a transport is connected. Large data catalogs retain their row count and complete label/key fingerprint, with three representative rows expanded.\n\n\
Limits: live online transport/catalog pagination, network/USB hotplug, signed-in Tailscale/Wi-Fi scan states, actual system information, stored/offline media and error/retry interleavings are not enumerated here. Generated cached ABS lists and metadata are populated; genuinely empty lists are explicitly marked. Background page jobs are disabled: download taps expose their immediate loading/queueing state, while asynchronous completion/removal states require separate fixtures. Accepted library-choice writes settle through the real ordered settings owner before mapping their durable destination; pending storage, newer edits and Back during a save have separate controls. Timing, audio correctness, physical input feel, visuals and discoverability need separate tests and UX review.\n\n\
Source includes Center-speed controls.\n");
    output.push_str("\n## Actual unconfigured startup\n\n");
    assert_eq!(top(&app), "Navidrome Server", "first-run setup redirect remains covered");
    output.push_str(&format!("App::new initially opens `{}` with actual rows: {}. ", top(&app), surface(&app).rows.iter().map(|row| row.title.as_str()).collect::<Vec<_>>().join(" / ")));
    step(&mut app, Event::Back);
    assert_eq!(top(&app), "Q2");
    output.push_str("Back → `Q2`; the Home traces below begin after this deliberate return.\n");
    let mut seen = BTreeSet::new();
    for scenario in scenarios() {
        output.push_str(&format!("\n## Fixture: {}\n", scenario.name));
        walk(&mut app, &scenario, Vec::new(), "Q2".into(), &mut seen, &mut output, 0);
        if scenario.song.is_some() {
            walk(&mut app, &scenario, vec![Event::BackLong], "Now Playing shortcut".into(), &mut seen, &mut output, 0);
        }
    }
    output.push_str("\n## Conditional Home spoken availability\n\nGenerated worker completion values are delivered through the production Home completion handler; configured offline setup is independent of reachability. Actual generated M4B/download discovery, missing-file/catalog negatives and Home/Back selection transitions have separate App worker tests. Unknown card/index reads retain known rows. These probes cover Home visibility only, not populated local-content playback.\n\n| ABS setup | Disk worker evidence | Actual Home rows |\n| --- | --- | --- |\n");
    for (name, configured, books, podcasts) in [
        ("neither", false, false, false), ("books only", false, true, false),
        ("podcasts only", false, false, true), ("both", false, true, true),
        ("neither / offline", true, false, false),
    ] {
        reset(&mut app, &scenarios()[0], &[]);
        if configured {
            app.config.audiobookshelf.url = "http://127.0.0.1:9".into();
            app.config.audiobookshelf.token = "generated-token".into();
            app.lib.configure(&app.config);
        }
        let home = app.top_id();
        app.handle(UiMsg::Loaded { screen: home, token: 1, data: Box::new(crate::library::spoken::Availability {
            books: Some(books), podcasts: Some(podcasts),
        }) }, &mut crate::platform::Headless { last: None });
        let rows = surface(&app).rows.iter().map(|row| row.title.clone()).collect::<Vec<_>>();
        assert_eq!(rows.contains(&"Books".into()), configured || books);
        assert_eq!(rows.contains(&"Podcasts".into()), configured || podcasts);
        output.push_str(&format!("| {} | {name} | {} |\n", if configured { "configured" } else { "unconfigured" }, rows.join(" / ")));
    }
    output.push_str("\n## Generated storage accounting contexts\n\nThese fixtures deliver generated counts through the actual asynchronous completion handler, then use App wheel/Back events. Filesystem allocation/inode accounting has independent generated-file tests.\n");
    for (name, usage) in screens::storage::generated_usage_cases() {
        reset(&mut app, &scenarios()[0], &[]);
        app.push(Box::new(screens::settings::Settings::new(screens::settings::Page::Storage)));
        step(&mut app, Event::Center);
        let screen = app.top_id();
        app.handle(UiMsg::Loaded { screen, token: 1, data: Box::new(usage) }, &mut crate::platform::Headless { last: None });
        output.push_str(&format!("\n### Storage: {name}\n\n"));
        let first = surface(&app).state;
        let mut states = vec![first.clone()];
        for _ in 0..5 {
            step(&mut app, Event::Scroll(1));
            let state = surface(&app).state;
            states.push(state.clone());
            if state == first { break; }
        }
        output.push_str(&format!("Clockwise detents: `{}`.\n\n", states.join(" → ")));
        if name.starts_with("mixed ") {
            let mut routes = Vec::new();
            for _ in 0..3 {
                let wedge = surface(&app).state;
                step(&mut app, Event::Center);
                let target = top(&app);
                step(&mut app, Event::Back);
                assert_eq!(surface(&app).state, wedge, "Back keeps the selected wedge");
                routes.push(target);
                step(&mut app, Event::Scroll(1));
            }
            output.push_str(&format!("Center on Downloads / Song Cache / Other Files opens `{}`. Back keeps each wedge selected. Populated rows and long-Center actions have separate fixture tests.\n\n", routes.join(" / ")));
        }
        step(&mut app, Event::Back);
        output.push_str(&format!("Back → `{}`; ", top(&app)));
        step(&mut app, Event::Center);
        output.push_str(&format!("Center reopens `{}` at the retained parent row.\n", top(&app)));
    }
    output.push_str("\n## Required critical journeys\n\nThese labels are requirement-oriented fixture inputs; targets and action counts come from actual runtime rows and events. Wheel repositioning is fixture setup, not an estimate of human wheel travel.\n\n| Journey from Q2 | Center choices | Actual target | Back | Center after Back |\n| --- | --- | --- | --- | --- |\n");
    let context = &scenarios()[1];
    for (labels, target) in [
        (vec!["Settings", "Playback", "Default Spoken Speed"], "Default Speed"),
        (vec!["Settings", "Playback", "Skip Forward"], "Skip Forward"),
        (vec!["Settings", "Playback", "Skip Backward"], "Skip Backward"),
        (vec!["Settings", "Storage", "Storage Usage"], "Storage Usage"),
    ] {
        let path = journey(&mut app, context, &labels);
        assert_eq!(top(&app), target, "critical journey target");
        let choices = path.iter().filter(|&&event| event == Event::Center).count();
        assert_eq!(choices, 3, "defaults and storage stay three choices from Home");
        step(&mut app, Event::Back);
        let back = top(&app);
        step(&mut app, Event::Center);
        assert_eq!(top(&app), target, "Back preserved the critical-journey parent row");
        output.push_str(&format!("| {} | {choices} | {target} | {back} | {} |\n", labels.join(" → "), top(&app)));
    }
    journey(&mut app, context, &["Settings", "Storage"]);
    assert!(!surface(&app).rows.iter().any(|row| row.title == "Mix Size"));
    journey(&mut app, context, &["Settings", "Storage", "Offline Mix", "Mix Size"]);
    let picker = top(&app);
    let options = surface(&app);
    let selected = &options.rows[options.selected].title;
    assert_eq!(picker, "Mix Size");
    assert_eq!(selected, "50% of card");
    step(&mut app, Event::Back);
    let storage = surface(&app);
    assert_eq!(storage.rows[storage.selected].title, picker);
    output.push_str(&format!("\nWith Offline Mix enabled, Storage shows **{picker}**. Its initial selection is **{selected}**. Back returns to the same Storage row.\n"));
    output.push_str("\n## Required spoken selection journeys\n\nGenerated ABS records are parsed by the production client and cache. These offline map probes disable workers: Opening is a pending operation, not proof of a playback session. Async success, cancellation and retained resume have separate production-handler tests.\n\n| Home journey | Center choices to initial target | Initial control | One more Center | Back | Retained row |\n| --- | --- | --- | --- | --- | --- |\n");
    let contexts = scenarios();
    let context = contexts.iter().find(|scenario| scenario.shelf == Some(true)).unwrap();
    for (labels, preview) in [
        (vec!["Books", "All Books", "Generated Book"], true),
        (vec!["Podcasts", "Latest Episodes", "Generated Episode"], false),
    ] {
        let path = journey(&mut app, context, &labels);
        let first = surface(&app);
        let initial = top(&app);
        assert_eq!(path.iter().filter(|&&event| event == Event::Center).count(), 3);
        if preview {
            assert_eq!(first.kind, "BookInfoScreen", "book selection previews rather than starts playback");
            for required in ["Generated Author", "Generated Narrator", "Generated Series", "Year: 2026", "Length: 4:00", "Generated offline summary."] {
                assert!(first.state.contains(required), "actual laid-out metadata missing {required}");
            }
            assert!(first.state.contains("play=true") && first.state.contains("error=false"));
            step(&mut app, Event::Center);
        }
        assert_eq!(surface(&app).kind, "Opening", "book explicit play and episode direct play enter actual pending operation");
        assert!(!app.stack.last().unwrap().1.script_ready(), "pending Opening must not falsely satisfy a readiness barrier");
        let next = top(&app);
        step(&mut app, Event::Back);
        let back = top(&app);
        let list = surface(&app);
        assert_eq!(list.rows[list.selected].title, *labels.last().unwrap(), "Back retains selected content row");
        output.push_str(&format!("| {} | 3 | {initial} / {} | {} | {back} | {} |\n", labels.join(" → "), first.kind,
            if preview { format!("{next} / Opening (pending)") } else { "Already Opening (direct episode play)".into() }, list.rows[list.selected].title));
        assert_eq!(list.rows[list.selected].spoken_progress, if preview { 35 } else { 100 });
        step(&mut app, Event::CenterLong);
        let menu = surface(&app);
        let label = if preview { "Mark Played" } else { "Mark Unplayed" };
        assert!(menu.rows.iter().any(|row| row.title == label));
        step(&mut app, Event::Back);
        let returned = surface(&app);
        assert_eq!(returned.rows[returned.selected].title, *labels.last().unwrap());

    }
    reset(&mut app, &scenarios()[0], &[]);
    portal_fixtures(&mut app, &mut output);
    app.player.shutdown();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let generated = root.join("build/ui/hierarchy.md");
    std::fs::create_dir_all(generated.parent().unwrap()).unwrap();
    std::fs::write(&generated, &output).unwrap();
    let reviewed = root.join("tests/ui/ui-hierarchy.md");
    if std::env::var_os("Q2_UI_MAP_UPDATE").is_some() {
        std::fs::write(reviewed, &output).unwrap();
    } else {
        assert_eq!(std::fs::read_to_string(reviewed).unwrap_or_default(), output,
            "UI hierarchy drift: inspect build/ui/hierarchy.md and its diff, review UX, then regenerate with tests/ui/map.py --write");
    }
}

#[test]
fn unexplained_duplicate_destinations_and_keys_fail_without_snapshot_approval() {
    let mut list = ListView::new(false);
    list.set_rows(vec![Row::link("First", Dest::Music), Row::link("Other label", Dest::Music)]);
    assert!(validate_rows("Generated duplicate", &Surface::list(&list)).unwrap_err().contains("duplicate destination"));
    list.set_rows(vec![Row::link("First", Dest::Music).key("same"), Row::link("Second", Dest::Settings).key("same")]);
    assert!(validate_rows("Generated duplicate", &Surface::list(&list)).unwrap_err().contains("stable row key"));
    list.set_rows(vec![Row::link("First name", Dest::Album { id: "same-album".into(), name: "First".into() }),
        Row::link("Second name", Dest::Album { id: "same-album".into(), name: "Renamed".into() })]);
    assert!(validate_rows("Renamed alias", &Surface::list(&list)).unwrap_err().contains("duplicate destination"));
}
