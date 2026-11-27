//! Account-bound direct listens. SQLite owns pending submissions and their
//! acknowledgements. Only one async owner sends them.
use super::Library;
use crate::config::Config;
use serde::{Deserialize, Serialize};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use subsonic::{
    lastfm,
    listenbrainz::{self as service, Error, Submission},
};

#[derive(Serialize, Deserialize)]
pub(super) struct Listen {
    pub artist: String,
    pub title: String,
    pub album: Option<String>,
    pub duration_ms: Option<u64>,
    pub started_at: u64,
    pub station: Option<String>,
}

impl Listen {
    fn track(&self) -> service::Track<'_> {
        service::Track {
            artist: &self.artist,
            title: &self.title,
            album: self.album.as_deref(),
            duration_ms: self.duration_ms,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum Destination {
    ListenBrainz,
    LastFm,
}

enum Client {
    ListenBrainz(service::Client),
    LastFm(lastfm::Client),
}

struct Route {
    scope: Vec<u8>,
    client: Client,
}

#[derive(Clone, PartialEq, Eq)]
struct Input {
    listenbrainz: crate::config::ListenBrainz,
    lastfm: crate::config::LastFm,
    proxy: Option<&'static str>,
    music_account: (String, String),
}

#[derive(Default)]
struct Configured {
    input: Option<Input>,
    generation: u64,
    routes: [Option<Arc<Route>>; 2],
    statuses: [&'static str; 2],
}

#[derive(Default)]
pub(super) struct Direct {
    configured: Mutex<Configured>,
    budget: service::Budget,
    wake: [Arc<tokio::sync::Notify>; 2],
    cancel: tokio::sync::Notify,
}

fn account_scope(dest: Destination, parts: &[&str]) -> Vec<u8> {
    let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
    hash.update(&[dest as u8]);
    for part in parts {
        hash.update(&(part.len() as u64).to_le_bytes());
        hash.update(part.as_bytes());
    }
    hash.finish().as_ref().to_vec()
}

impl Direct {
    pub(super) fn configure(&self, config: &Config) {
        let mut state = self.configured.lock().unwrap();
        let input = Input {
            listenbrainz: config.listenbrainz.clone(),
            lastfm: config.lastfm.clone(),
            proxy: config.http_proxy(),
            music_account: (config.server.url.clone(), config.server.user.clone()),
        };
        if state.input.as_ref() == Some(&input) {
            return;
        }
        state.routes = Default::default();
        state.statuses = ["Off"; 2];
        let lb = &input.listenbrainz;
        if lb.enabled {
            let client = service::Client::with_budget(
                &lb.token,
                input.proxy,
                &lb.contact_url,
                self.budget.clone(),
            );
            state.statuses[Destination::ListenBrainz as usize] = "Setup Needed";
            if let Ok(client) = client {
                state.statuses[Destination::ListenBrainz as usize] = "Ready";
                state.routes[Destination::ListenBrainz as usize] = Some(Arc::new(Route {
                    scope: account_scope(Destination::ListenBrainz, &[&lb.token]),
                    client: Client::ListenBrainz(client),
                }));
            }
        }
        let lf = &input.lastfm;
        if lf.enabled {
            let credentials = lastfm::Credentials {
                api_key: &lf.api_key,
                shared_secret: &lf.shared_secret,
                session: &lf.session,
            };
            state.statuses[Destination::LastFm as usize] = "Setup Needed";
            if let Ok(client) = lastfm::Client::new(&credentials, input.proxy, &lf.contact_url) {
                state.statuses[Destination::LastFm as usize] = "Ready";
                state.routes[Destination::LastFm as usize] = Some(Arc::new(Route {
                    scope: account_scope(Destination::LastFm, &[&lf.api_key, &lf.session]),
                    client: Client::LastFm(client),
                }));
            }
        }
        state.input = Some(input);
        state.generation = state.generation.checked_add(1).unwrap();
        self.cancel_network();
        self.wake();
    }

    pub(super) fn admission(&self) -> Option<u64> {
        let state = self.configured.lock().unwrap();
        state
            .routes
            .iter()
            .any(Option::is_some)
            .then_some(state.generation)
    }

    pub(super) fn wake(&self) {
        for wake in &self.wake {
            wake.notify_one();
        }
    }

    pub(super) fn cancel_network(&self) {
        self.cancel.notify_waiters();
    }

    fn route(&self, generation: u64, dest: Destination) -> Option<Arc<Route>> {
        let state = self.configured.lock().unwrap();
        if state.generation != generation {
            return None;
        }
        state.routes[dest as usize].clone()
    }

    fn status(&self, generation: u64, dest: Destination, value: &'static str) {
        let mut state = self.configured.lock().unwrap();
        if state.generation == generation {
            state.statuses[dest as usize] = value;
        }
    }
}

impl Library {
    pub fn listenbrainz_status(&self) -> &'static str {
        self.direct_status(Destination::ListenBrainz)
    }

    pub fn lastfm_status(&self) -> &'static str {
        self.direct_status(Destination::LastFm)
    }

    fn direct_status(&self, dest: Destination) -> &'static str {
        self.direct_listens.configured.lock().unwrap().statuses[dest as usize]
    }

    pub(super) fn submit_direct(&self, generation: u64, dest: Destination, listen: Listen) {
        if listen.started_at < 1_033_410_600
            || listen.artist.trim().is_empty()
            || listen.title.trim().is_empty()
        {
            return;
        }
        let Some(route) = self.direct_listens.route(generation, dest) else {
            return;
        };
        let weak = self.me.clone();
        self.db_post(move |blocking, db| {
            let Some(lib) = weak.upgrade() else { return };
            if lib.direct_listens.route(generation, dest).is_none() {
                return;
            }
            if db
                .queue_direct_listen(blocking, &route.scope, listen)
                .is_err()
            {
                lib.direct_listens
                    .status(generation, dest, "Could Not Save Listen");
            }
            lib.direct_listens.wake();
        });
    }

    pub(super) fn start_direct_listens(lib: &Arc<Self>) {
        for dest in [Destination::ListenBrainz, Destination::LastFm] {
            Self::start_direct_worker(lib, dest);
        }
    }

    fn start_direct_worker(lib: &Arc<Self>, dest: Destination) {
        let weak = Arc::downgrade(lib);
        let wake = lib.direct_listens.wake[dest as usize].clone();
        crate::rt::io().spawn(async move {
            let mut wait = None;
            let mut failures = 0u32;
            let mut previous: Option<u64> = None;
            loop {
                // On an outage, frequent new songs must not bypass backoff.
                let deadline = wait.map(|delay| tokio::time::Instant::now() + delay);
                loop {
                    let Some(lib) = weak.upgrade() else { return };
                    let generation = lib.direct_listens.configured.lock().unwrap().generation;
                    drop(lib);
                    if previous != Some(generation) {
                        failures = 0;
                        previous = Some(generation);
                        break;
                    }
                    if let Some(deadline) = deadline {
                        tokio::select! {
                            () = wake.notified() => {},
                            () = tokio::time::sleep_until(deadline) => break,
                        }
                    } else {
                        wake.notified().await;
                        break;
                    }
                }
                let Some(lib) = weak.upgrade() else { return };
                wait = lib
                    .flush_direct_listen(previous.unwrap(), dest, &mut failures)
                    .await;
            }
        });
    }

    async fn flush_direct_listen(
        &self,
        generation: u64,
        dest: Destination,
        failures: &mut u32,
    ) -> Option<Duration> {
        if !self.maintenance_link_ready() {
            return None;
        }
        let route = self.direct_listens.route(generation, dest)?;
        if self.direct_status(dest) == "Token Rejected" {
            return None;
        }
        let scope = route.scope.clone();
        let (id, listen) = match self
            .db_run(move |b, db| db.next_direct_listen(b, &scope))
            .await
        {
            Ok(item) => item?,
            Err(_) => {
                self.direct_listens
                    .status(generation, dest, "Could Not Read Listens");
                return Some(Duration::from_secs(60));
            }
        };
        let changed = self.direct_listens.cancel.notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if self.direct_listens.route(generation, dest).is_none()
            || !self.maintenance_link_ready()
        {
            return None;
        }
        let result = tokio::select! {
            biased;
            () = &mut changed => return None,
            result = route.submit(&listen) => result,
        };
        match result {
            Ok(()) | Err(Error::InvalidListen) => {
                let status = if result.is_ok() {
                    "Ready"
                } else {
                    "Listen Rejected"
                };
                let scope = route.scope.clone();
                if self
                    .db_run(move |b, db| db.finish_direct_listen(b, &scope, id))
                    .await
                    .is_err()
                {
                    self.direct_listens
                        .status(generation, dest, "Could Not Save Receipt");
                    return Some(Duration::from_secs(60));
                }
                *failures = 0;
                self.direct_listens.status(generation, dest, status);
                Some(Duration::from_secs(1))
            }
            Err(Error::Authentication | Error::Configuration) => {
                self.direct_listens
                    .status(generation, dest, "Token Rejected");
                None
            }
            Err(Error::RateLimited {
                retry_after_seconds,
            }) => {
                self.direct_listens.status(generation, dest, "Retrying");
                Some(Duration::from_secs(
                    retry_after_seconds.unwrap_or(30).max(1),
                ))
            }
            Err(Error::Unavailable | Error::Protocol) => {
                self.direct_listens.status(generation, dest, "Retrying");
                *failures = (*failures + 1).min(6);
                Some(Duration::from_secs(5 << *failures))
            }
        }
    }
}

impl Route {
    async fn submit(&self, listen: &Listen) -> Result<(), Error> {
        match &self.client {
            Client::ListenBrainz(client) => {
                client
                    .submit_async(
                        &listen.track(),
                        Submission::Cached {
                            started_at: listen.started_at,
                        },
                    )
                    .await
            }
            Client::LastFm(client) => {
                let result = client
                    .scrobble_async(&[lastfm::Listen {
                        is_broadcast: listen.station.is_some(),
                        artist: &listen.artist,
                        title: &listen.title,
                        album: listen.album.as_deref(),
                        duration_seconds: listen.duration_ms.map(|ms| ms / 1000),
                        started_at: listen.started_at,
                    }])
                    .await;
                match result {
                    Ok(outcomes) if outcomes == [lastfm::Outcome::Accepted] => Ok(()),
                    Ok(_) | Err(lastfm::Error::Input(_) | lastfm::Error::Rejected(_)) => {
                        Err(Error::InvalidListen)
                    }
                    Err(lastfm::Error::Authentication | lastfm::Error::Configuration) => {
                        Err(Error::Authentication)
                    }
                    Err(lastfm::Error::RateLimited {
                        retry_after_seconds,
                    }) => Err(Error::RateLimited {
                        retry_after_seconds,
                    }),
                    Err(lastfm::Error::Unavailable) => Err(Error::Unavailable),
                    Err(lastfm::Error::Protocol) => Err(Error::Protocol),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::player::{RadioListen, TrackSource};
    use crate::{config::Paths, library::Tracks};
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{Ipv4Addr, SocketAddr, TcpListener};

    fn fixture(name: &str) -> (Arc<Library>, Config, std::path::PathBuf) {
        let base = std::env::var_os("Q2_GENERATED_TEST_ROOT")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| {
                std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../build/perf/tests")
            });
        let root = base.join(format!("{name}-{}", fastrand::u64(..)));
        let config = Config::default();
        let (lib, _) = Library::create(Paths::new(root.clone()), &config, Arc::new(|_| {}));
        (lib, config, root)
    }

    fn generated_station() -> subsonic::Song {
        subsonic::Song {
            id: "station".into(),
            title: "Generated Station".into(),
            radio_url: Some("https://station.invalid/stream".into()),
            ..Default::default()
        }
    }

    fn generated_accounts(config: &mut Config) {
        config.listenbrainz.token = "generated-listenbrainz".into();
        config.listenbrainz.contact_url = "https://contact.invalid/q2".into();
        config.lastfm.api_key = "generated-key".into();
        config.lastfm.shared_secret = "generated-secret".into();
        config.lastfm.session = "generated-session".into();
        config.lastfm.contact_url = config.listenbrainz.contact_url.clone();
    }

    #[test]
    fn library_and_radio_respect_each_service_switch_independently() {
        let music: subsonic::Song = serde_json::from_str(
            r#"{"id":"music","title":"Song","album":"Album","artist":"Artist","duration":120}"#,
        )
        .unwrap();
        let radio = generated_station();
        let blocking = crate::rt::Blocking::dedicated_thread();
        for navidrome in [false, true] {
            for (lb, lf) in [(false, false), (false, true), (true, false), (true, true)] {
                let (lib, mut config, root) = fixture("service-switches");
                generated_accounts(&mut config);
                config.playback.scrobble = navidrome;
                config.listenbrainz.enabled = lb;
                config.lastfm.enabled = lf;
                lib.configure(&config);
                let tracks = Tracks(lib.clone());
                let start = 1_900_000_000_000;
                tracks.played(&music, start);
                tracks.played(&radio, start);
                let generation = tracks.direct_listen_generation();
                assert_eq!(generation.is_some(), lb || lf);
                if let Some(generation) = generation {
                    tracks.direct_song_played(generation, &music, start);
                    tracks.direct_radio_played(
                        generation,
                        &radio,
                        "Radio artist",
                        "Radio song",
                        start,
                        RadioListen::ListenBrainz,
                    );
                    tracks.direct_radio_played(
                        generation,
                        &radio,
                        "Radio artist",
                        "Radio song",
                        start,
                        RadioListen::LastFm { duration_ms: None },
                    );
                }
                lib.db_worker.drain();
                {
                    let db = lib.db();
                    let nav = db.pending(&blocking, 10);
                    assert_eq!(nav.len(), usize::from(navidrome));
                    if navidrome {
                        assert_eq!(
                            nav[0].1,
                            super::super::PendingCall::Scrobble {
                                id: music.id.clone(),
                                time_ms: start
                            }
                        );
                    }
                    let lb_scope =
                        account_scope(Destination::ListenBrainz, &[&config.listenbrainz.token]);
                    let lf_scope = account_scope(
                        Destination::LastFm,
                        &[&config.lastfm.api_key, &config.lastfm.session],
                    );
                    for (enabled, scope) in [(lb, lb_scope), (lf, lf_scope)] {
                        if enabled {
                            let (id, listen) =
                                db.next_direct_listen(&blocking, &scope).unwrap().unwrap();
                            assert_eq!(
                                (listen.title, listen.album, listen.station),
                                (music.title.clone(), Some(music.album.clone()), None)
                            );
                            db.finish_direct_listen(&blocking, &scope, id).unwrap();
                            let (id, listen) =
                                db.next_direct_listen(&blocking, &scope).unwrap().unwrap();
                            assert_eq!(
                                (listen.title.as_str(), listen.station.as_deref()),
                                ("Radio song", Some("station"))
                            );
                            db.finish_direct_listen(&blocking, &scope, id).unwrap();
                        }
                        assert!(db.next_direct_listen(&blocking, &scope).unwrap().is_none());
                    }
                }
                drop(tracks);
                drop(lib);
                std::fs::remove_dir_all(root).unwrap();
            }
        }
    }

    #[test]
    fn radio_played_dispatches_to_distinct_service_accounts_and_only_acks_success() {
        let (lib, mut config, root) = fixture("direct-service");
        let tracks = Tracks(lib.clone());
        generated_accounts(&mut config);
        config.playback.scrobble = false;
        config.listenbrainz.enabled = true;
        config.lastfm.enabled = true;
        lib.direct_listens.configure(&config);

        let lb_wire = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let lf_wire = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let endpoint = |wire: &TcpListener| match wire.local_addr().unwrap() {
            SocketAddr::V4(ip) => ip,
            _ => unreachable!(),
        };
        let creds = lastfm::Credentials {
            api_key: &config.lastfm.api_key,
            shared_secret: &config.lastfm.shared_secret,
            session: &config.lastfm.session,
        };
        let lb = Client::ListenBrainz(service::Client::generated_loopback(
            &config.listenbrainz.token,
            endpoint(&lb_wire),
        ));
        let lf = Client::LastFm(lastfm::Client::generated_loopback(
            &creds,
            endpoint(&lf_wire),
        ));
        for (dest, client) in [(Destination::ListenBrainz, lb), (Destination::LastFm, lf)] {
            let mut state = lib.direct_listens.configured.lock().unwrap();
            let slot = &mut state.routes[dest as usize];
            let scope = slot.as_ref().unwrap().scope.clone();
            *slot = Some(Arc::new(Route { scope, client }));
        }
        let generation = tracks.direct_listen_generation().unwrap();
        let radio = generated_station();
        tracks.direct_radio_played(
            generation,
            &radio,
            "Generated Artist",
            "Generated Song",
            1_900_000_000_000,
            RadioListen::ListenBrainz,
        );
        tracks.direct_radio_played(
            generation,
            &radio,
            "Generated Artist",
            "Generated Song",
            1_900_000_000_000,
            RadioListen::LastFm {
                duration_ms: Some(90_000),
            },
        );
        lib.db_worker.drain();

        fn reply(
            listener: TcpListener,
            status: &'static str,
            response: &'static str,
        ) -> std::thread::JoinHandle<(String, String)> {
            std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut reader = BufReader::new(&mut socket);
                let mut headers = String::new();
                loop {
                    let mut line = String::new();
                    assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                    headers.push_str(&line);
                    if line == "\r\n" {
                        break;
                    }
                }
                let n: usize = headers
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse().unwrap())
                    })
                    .unwrap();
                let mut body = vec![0; n];
                reader.read_exact(&mut body).unwrap();
                write!(socket, "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}", response.len()).unwrap();
                (headers, String::from_utf8(body).unwrap())
            })
        }
        let lb_test = reply(lb_wire, "200 OK", r#"{"status":"ok"}"#);
        let lf_test = reply(lf_wire, "503 Unavailable", "");
        let runtime = crate::rt::Ui::new().unwrap();
        runtime.block_on(async {
            let mut failures = 0;
            assert_eq!(
                lib.flush_direct_listen(generation, Destination::ListenBrainz, &mut failures)
                    .await,
                Some(Duration::from_secs(1))
            );
            assert_eq!(
                lib.flush_direct_listen(generation, Destination::LastFm, &mut failures)
                    .await,
                Some(Duration::from_secs(10))
            );
        });
        let (lb_request, listen) = lb_test.join().unwrap();
        let (lf_request, form) = lf_test.join().unwrap();
        assert!(
            lb_request
                .to_ascii_lowercase()
                .contains("authorization: token generated-listenbrainz")
        );
        let listen: serde_json::Value = serde_json::from_str(&listen).unwrap();
        assert_eq!(
            listen["payload"][0]["track_metadata"]["track_name"],
            "Generated Song"
        );
        assert_eq!(listen["payload"][0]["listened_at"], 1_900_000_000);
        assert!(!lf_request.to_ascii_lowercase().contains("authorization:"));
        assert!(
            form.contains("chosenByUser=0")
                && form.contains("duration=90")
                && form.contains("api_sig=")
        );
        assert!(!form.contains("generated-secret"));
        assert_eq!(lib.lastfm_status(), "Retrying");
        let db = lib.db();
        for (dest, pending) in [
            (Destination::ListenBrainz, false),
            (Destination::LastFm, true),
        ] {
            let scope = lib
                .direct_listens
                .route(generation, dest)
                .unwrap()
                .scope
                .clone();
            assert_eq!(
                db.next_direct_listen(&crate::rt::Blocking::dedicated_thread(), &scope)
                    .unwrap()
                    .is_some(),
                pending
            );
        }
        drop(db);
        config.lastfm.session = "different-generated-session".into();
        lib.direct_listens.configure(&config);
        assert!(
            lib.direct_listens
                .route(generation, Destination::LastFm)
                .is_none()
        );
        let updated = lib
            .direct_listens
            .route(
                tracks.direct_listen_generation().unwrap(),
                Destination::LastFm,
            )
            .unwrap();
        assert!(
            lib.db()
                .next_direct_listen(&crate::rt::Blocking::dedicated_thread(), &updated.scope,)
                .unwrap()
                .is_none()
        );
        drop(tracks);
        drop(lib);
        std::fs::remove_dir_all(root).unwrap();
    }
}
