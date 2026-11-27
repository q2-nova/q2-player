use super::*;
use crate::config::{Config, Paths};
use crate::ui::Screen;
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Generated server with a finite lifetime and joined cleanup, including panic paths.
struct Server {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Server {
    fn new() -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let (done, observed) = (stop.clone(), requests.clone());
        let thread = std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(15);
            while !done.load(Ordering::SeqCst) && Instant::now() < deadline {
                let Ok((mut socket, _)) = listener.accept() else { std::thread::sleep(Duration::from_millis(2)); continue; };
                socket.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
                socket.set_write_timeout(Some(Duration::from_secs(1))).unwrap();
                let mut reader = BufReader::new(socket.try_clone().unwrap());
                let mut request = String::new();
                if reader.read_line(&mut request).is_err() { continue; }
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_err() || line == "\r\n" || line.is_empty() { break; }
                }
                let body = if request.contains("/rest/ping") {
                    serde_json::json!({"status":"ok"})
                } else if request.contains("/rest/getAlbumList2") {
                    observed.lock().unwrap().push(request);
                    // Duplicates and an unavailable candidate precede two
                    // whole albums that cross the 150-song queue boundary.
                    serde_json::json!({"status":"ok", "albumList2":{"album": [
                        {"id":"missing","name":"Missing"},
                        {"id":"a","name":"A"}, {"id":"a","name":"A"},
                        {"id":"b","name":"B"}, {"id":"c","name":"C"}
                    ]}})
                } else {
                    observed.lock().unwrap().push(request);
                    serde_json::json!({"status":"failed","error":{"code":70,"message":"Generated missing album"}})
                };
                let body = serde_json::json!({"subsonic-response":body}).to_string();
                let _ = write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        });
        Self { url, requests, stop, thread: Some(thread) }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}

#[test]
fn online_shuffle_samples_catalog_without_browsing_and_preserves_album_boundaries() {
    let server = Server::new();
    let mut config = Config::default();
    config.server.url = server.url.clone();
    config.server.user = "generated".into(); config.server.password = "synthetic-only".into();
    let root = crate::library::generated_test_directory("shuffle-catalog");
    let lib = Library::open(Paths::new(root), &config, Arc::new(|_| {}));
    for id in ["a", "b"] {
        let songs: Vec<_> = (0..80).map(|track| serde_json::json!({
            "id":format!("{id}-{track}"), "title":"Generated", "albumId":id,
            "discNumber":track / 40 + 1, "track":track % 40 + 1
        })).collect();
        lib.db().put_response(&crate::rt::Blocking::dedicated_thread(), &format!("getAlbum?id={id}"),
            &serde_json::json!({"album":{"id":id,"name":"Generated","song":songs}}).to_string());
    }
    let songs = crate::rt::Ui::new().unwrap().block_on(async {
        tokio::time::timeout(Duration::from_secs(5), shuffled_albums_online(lib)).await.unwrap().unwrap()
    });
    assert_eq!(songs.len(), 160, "finish the album crossing 150; never repeat the duplicate album");
    assert_eq!(songs.iter().map(|s| s.id.clone()).collect::<Vec<_>>(),
        ["a", "b"].into_iter().flat_map(|id| (0..80).map(move |t| format!("{id}-{t}"))).collect::<Vec<_>>());
    let requests = server.requests.lock().unwrap();
    assert_eq!(requests.len(), 2, "only the random draw and missing album need the server");
    assert!(requests[0].contains("type=random") && requests[0].contains("size=500") && requests[0].contains("offset=0"));
    assert!(requests[1].contains("id=missing"));
}

#[test]
fn shuffle_preparation_back_rejects_late_playback_and_account_change_preserves_context() {
    let mut app = crate::ui::settings_tests::fixture(); app.player.shutdown();
    // Generated completion messages make Back/completion ordering deterministic.
    app.background = false;
    let mut platform = crate::platform::Headless { last: None };
    let parent = app.top_id();
    app.push(Box::new(ShufflePreparation::albums(true)));
    let screen = app.top_id();
    assert_eq!(app.stack.last().unwrap().1.title(), "Shuffle Albums");
    app.event(crate::input::Event::Back, &mut platform);
    assert_eq!(app.top_id(), parent);
    let result = || {
        let song = Song { id: "generated".into(), ..Default::default() };
        Box::new(Ok::<_, String>(ShuffleSelection::Queue(vec![song]))) as Box<dyn std::any::Any + Send>
    };
    app.handle(crate::ui::UiMsg::Loaded { screen, token:1, data:result() }, &mut platform);
    assert_eq!(app.top_id(), parent, "late completion cannot navigate after Back");

    app.push(Box::new(ShufflePreparation::albums(true)));
    let screen = app.top_id();
    app.config.server.user = "different-generated-account".into();
    app.with_screen(screen, |screen, ctx| {
        screen.loaded(1, result(), ctx);
        let Some(Action::Many(actions)) = screen.completion() else { panic!("completion required") };
        assert!(matches!(actions.as_slice(), [Action::Dismiss(_), Action::Toast(message)] if message.contains("Playback changed")));
    });
    assert_eq!(app.top_id(), screen, "completion remains owned by its preparation screen");
}

#[test]
fn shuffle_empty_and_failure_return_to_the_menu_without_opening_now_playing() {
    let mut app = crate::ui::settings_tests::fixture(); app.player.shutdown(); app.background = false;
    let parent = app.top_id();
    for result in [Ok(ShuffleSelection::Queue(Vec::new())), Err("Generated unavailable server".to_string())] {
        let mut preparation = ShufflePreparation::albums(true);
        app.with_screen(parent, |_, ctx| {
            preparation.enter(ctx);
            preparation.loaded(1, Box::new(result), ctx);
            let Some(Action::Many(actions)) = preparation.completion() else { panic!("completion required") };
            assert!(matches!(actions.as_slice(), [Action::Dismiss(_), Action::Toast(_)]));
        });
    }
}

#[test]
fn online_song_shuffle_replaces_spoken_only_after_success_and_surfaces_failure() {
    for success in [true, false] {
        let mut app = crate::ui::settings_tests::fixture();
        app.player.shutdown(); app.background = false;
        let book = Song { id: "generated-book".into(), spoken: Some(Default::default()), ..Default::default() };
        let mut status = crate::audio::player::Status::default();
        status.queue = Arc::new(vec![book]); status.generation = 17;
        let (player, commands) = crate::audio::player::PlayerHandle::fixture(status);
        app.player = player;
        let parent = app.top_id();
        app.push(Box::new(ShufflePreparation { online: true, ..ShufflePreparation::new(None) }));
        let preparation = app.top_id();
        assert!(!app.stack.last().unwrap().1.is_now_playing());
        assert!(commands.try_recv().is_err(), "existing book keeps playing during fetch");
        let result: Result<ShuffleSelection, String> = if success {
            Ok(ShuffleSelection::Queue(vec![Song { id: "generated-song".into(), ..Default::default() }]))
        } else { Err("Generated music server failure".into()) };
        app.handle(crate::ui::UiMsg::Loaded {screen: preparation, token: 1, data: Box::new(result)}, &mut crate::platform::Headless {last: None});
        if success {
            assert!(app.stack.last().unwrap().1.is_now_playing());
            match commands.try_recv().unwrap() {
                Cmd::Play {songs, shuffle, ..} => {
                    assert_eq!(songs[0].id, "generated-song"); assert!(songs[0].spoken.is_none());
                    assert!(!shuffle, "server random order is already randomized");
                }
                _ => panic!("expected music playback admission"),
            }
        } else {
            assert_eq!(app.top_id(), parent);
            assert!(commands.try_recv().is_err());
            assert!(app.toast.as_ref().unwrap().0.contains("Generated music server failure"));
        }
    }
}
