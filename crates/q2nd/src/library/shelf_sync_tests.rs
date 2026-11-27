//! The Audiobookshelf library sync against a model server: the marker round
//! of an unchanged library, full resyncs, group pages derived from the book
//! rows (and equal to the server's), incremental changes, new podcast
//! episodes, covers the server lacks, a change during a sync, an interrupted
//! sync, stale pages, and generated mutation sequences. The model answers as
//! Audiobookshelf does: minified items name their series, authors and
//! narrators without ids, and every JSON document has an ETag.
use super::*;
use crate::config::{Config, Paths};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::sync::{Mutex, atomic::{AtomicBool, Ordering}};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub(crate) struct Book { pub id: String, pub title: String, pub series: Option<(u32, u32)>, pub authors: Vec<u32>, pub narrators: Vec<u32>, pub genres: Vec<u32>, pub added: u64 }
#[derive(Clone, Debug)]
pub(crate) struct Show { pub id: String, pub title: String, pub added: u64, pub episodes: Vec<(String, String, u64)> }
#[derive(Clone, Debug)]
pub(crate) struct Served { pub path: String, pub filter: String, pub status: u16, pub bytes: usize }

type Hook = Box<dyn FnMut(&mut Abs, &str) + Send>;

#[derive(Default)]
pub(crate) struct Abs {
    pub books: Vec<Book>,
    pub shows: Vec<Show>,
    /// Send ETags and answer If-None-Match (Audiobookshelf does).
    pub etags: bool,
    /// Items whose cover the server lacks (404).
    pub no_cover: BTreeSet<String>,
    pub served: Vec<Served>,
    /// Run after each answer, with its path.
    pub after: Option<Hook>,
    serial: u64,
}
fn author(n: u32) -> String { format!("Author {n}") }
fn narrator(n: u32) -> String { format!("Narrator {n}") }
fn genre(n: u32) -> String { format!("Genre {n}") }
/// Series 90 has ", " in its name, as a real series may.
fn series(n: u32) -> String { if n == 90 { "Tales, Collected".into() } else { format!("Series {n}") } }
impl Abs {
    pub fn with(books: usize, shows: usize, episodes: usize) -> Self {
        let mut abs = Abs { etags: true, ..Abs::default() };
        for _ in 0..books { abs.add_book(None); }
        for _ in 0..shows { abs.add_show(episodes); }
        abs
    }
    fn next(&mut self) -> u64 { self.serial += 1; self.serial }
    pub fn add_book(&mut self, title: Option<&str>) {
        let n = self.next();
        let title = title.map_or_else(|| format!("Book {:05}", (n * 7919) % 100_000), str::to_owned);
        let k = n as u32;
        self.books.push(Book { id: format!("b-{n}"), title, series: (n % 3 == 0).then_some((k % 4, k)),
            authors: if n % 5 == 0 { vec![k % 6, (k + 1) % 6] } else { vec![k % 6] }, narrators: (n % 2 == 0).then_some(k % 3).into_iter().collect(),
            genres: vec![k % 4], added: 1_000 + n });
    }
    pub fn add_show(&mut self, episodes: usize) {
        let n = self.next();
        let mut list = Vec::new();
        for e in 0..episodes { let at = self.next(); list.push((format!("e-{n}-{e}"), format!("Episode {e}"), 1_000_000 + at)); }
        self.shows.push(Show { id: format!("p-{n}"), title: format!("Show {n:04}"), added: 1_000 + n, episodes: list });
    }
    /// A new episode, published now.
    pub fn add_episode(&mut self, show: usize) {
        let at = self.next();
        let s = &mut self.shows[show];
        let e = s.episodes.len();
        s.episodes.push((format!("{}-new-{e}", s.id), format!("Episode new {e}"), 1_000_000 + at));
    }
    fn books_by_title(&self) -> Vec<&Book> {
        let mut books: Vec<&Book> = self.books.iter().collect();
        books.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()).then_with(|| a.id.cmp(&b.id)));
        books
    }
    fn by_added(&self) -> Vec<&Book> {
        let mut books: Vec<&Book> = self.books.iter().collect();
        books.sort_by_key(|b| b.added);
        books
    }
    fn names(&self, kind: char) -> Vec<(String, String, usize)> {
        let mut out: BTreeMap<String, (String, usize)> = BTreeMap::new();
        for b in &self.books {
            let ids: Vec<u32> = match kind { 's' => b.series.iter().map(|s| s.0).collect(), 'a' => b.authors.clone(), 'n' => b.narrators.clone(), _ => b.genres.clone() };
            for id in ids {
                let (key, name) = match kind { 's' => (format!("s-{id}"), series(id)), 'a' => (format!("a-{id}"), author(id)), 'n' => (format!("n-{id}"), narrator(id)), _ => (format!("g-{id}"), genre(id)) };
                out.entry(name).or_insert((key, 0)).1 += 1;
            }
        }
        out.into_iter().map(|(name, (id, count))| (id, name, count)).collect()
    }
    fn book_json(book: &Book) -> serde_json::Value {
        let series_name = book.series.map(|(s, seq)| format!("{} #{seq}", series(s)));
        let authors = book.authors.iter().map(|a| author(*a)).collect::<Vec<_>>().join(", ");
        let narrators = book.narrators.iter().map(|n| narrator(*n)).collect::<Vec<_>>().join(", ");
        serde_json::json!({"id":book.id,"libraryId":"lb","mediaType":"book","addedAt":book.added,
            "media":{"metadata":{"title":book.title,"authorName":authors,"narratorName":narrators,"seriesName":series_name,
                "genres":book.genres.iter().map(|g| genre(*g)).collect::<Vec<_>>()},"duration":240,"numTracks":1}})
    }
    fn show_json(show: &Show, episodes: bool) -> serde_json::Value {
        let mut media = serde_json::json!({"metadata":{"title":show.title,"author":"Generated Host"},"numEpisodes":show.episodes.len()});
        if episodes {
            media["episodes"] = show.episodes.iter().map(|(id, title, at)| serde_json::json!({"id":id,"title":title,"publishedAt":at,"audioFile":{"duration":12}})).collect();
        }
        serde_json::json!({"id":show.id,"libraryId":"lp","mediaType":"podcast","addedAt":show.added,"media":media})
    }
    fn answer(&self, path: &str, query: &BTreeMap<String, String>) -> Option<serde_json::Value> {
        let limit: usize = query.get("limit").and_then(|v| v.parse().ok()).unwrap_or(50);
        let page: usize = query.get("page").and_then(|v| v.parse().ok()).unwrap_or(0);
        let paged = |rows: Vec<serde_json::Value>| {
            let total = rows.len();
            serde_json::json!({"page":page,"limit":limit,"total":total,"results":rows.into_iter().skip(page * limit).take(limit).collect::<Vec<_>>()})
        };
        let desc = query.get("desc").is_some_and(|d| d == "1");
        let sort = query.get("sort").map(String::as_str).unwrap_or("");
        Some(match path {
            "/api/me" => serde_json::json!({"id":"user-model"}),
            "/api/libraries" => serde_json::json!({"libraries":[{"id":"lb","name":"Books","mediaType":"book"},{"id":"lp","name":"Shows","mediaType":"podcast"}]}),
            "/api/libraries/lb/items" => {
                let filter = query.get("filter").map(String::as_str).unwrap_or("");
                let mut books: Vec<&Book> = if sort == "addedAt" || filter == "recent" { self.by_added() } else { self.books_by_title() };
                if desc || filter == "recent" { books.reverse(); }
                let member = |b: &&Book, kind: &str, value: &str| match kind {
                    "series" => b.series.is_some_and(|(s, _)| base64(&format!("s-{s}")) == value),
                    "authors" => b.authors.iter().any(|a| base64(&format!("a-{a}")) == value),
                    "narrators" => b.narrators.iter().any(|n| base64(&narrator(*n)) == value),
                    "genres" => b.genres.iter().any(|g| base64(&genre(*g)) == value),
                    _ => false,
                };
                if let Some((kind, value)) = filter.split_once('.') {
                    books.retain(|b| member(b, kind, value));
                    if kind == "series" { books.sort_by_key(|b| (b.series.map(|s| s.1), b.added)); }
                } else if !filter.is_empty() && filter != "recent" { books.clear(); }
                paged(books.into_iter().map(Self::book_json).collect())
            }
            "/api/libraries/lp/items" => {
                let mut shows: Vec<&Show> = self.shows.iter().collect();
                if query.get("filter").is_some_and(|f| f != "recent") { shows.clear(); }
                if sort == "addedAt" || query.get("filter").is_some() { shows.sort_by_key(|s| s.added); if desc || query.get("filter").is_some() { shows.reverse(); } }
                else { shows.sort_by(|a, b| a.title.cmp(&b.title)); }
                paged(shows.into_iter().map(|s| Self::show_json(s, false)).collect())
            }
            "/api/libraries/lb/series" => paged(self.names('s').into_iter().map(|(id, name, count)| serde_json::json!({"id":id,"name":name,
                "books":(0..count).map(|_| serde_json::json!({})).collect::<Vec<_>>()})).collect()),
            "/api/libraries/lb/authors" => paged(self.names('a').into_iter().map(|(id, name, count)| serde_json::json!({"id":id,"name":name,"numBooks":count})).collect()),
            "/api/libraries/lb/narrators" => serde_json::json!({"narrators":self.names('n').into_iter().map(|(id, name, count)| serde_json::json!({"id":id,"name":name,"numBooks":count})).collect::<Vec<_>>()}),
            "/api/libraries/lb/filterdata" => serde_json::json!({"genres":self.names('g').into_iter().map(|(_, name, _)| name).collect::<Vec<_>>()}),
            p if p.ends_with("/collections") || p.ends_with("/playlists") || p.ends_with("/authors") || p.ends_with("/series") => paged(Vec::new()),
            p if p.ends_with("/filterdata") => serde_json::json!({"genres":[]}),
            p if p.ends_with("/narrators") => serde_json::json!({"narrators":[]}),
            p if p.ends_with("/recent-episodes") => {
                let mut episodes: Vec<(&Show, &(String, String, u64))> = self.shows.iter().flat_map(|s| s.episodes.iter().map(move |e| (s, e))).collect();
                episodes.sort_by(|a, b| b.1.2.cmp(&a.1.2).then_with(|| a.1.0.cmp(&b.1.0)));
                let rows: Vec<_> = episodes.into_iter().skip(page * limit).take(limit).map(|(s, (id, title, at))| serde_json::json!({
                    "id":id,"libraryItemId":s.id,"libraryId":"lp","title":title,"publishedAt":at,"audioFile":{"duration":12},
                    "podcast":{"metadata":{"title":s.title}}})).collect();
                serde_json::json!({"page":page,"limit":limit,"episodes":rows})
            }
            p if p.ends_with("/personalized") => {
                let first: Vec<_> = if p.contains("/lb/") { self.books_by_title().into_iter().take(2).map(Self::book_json).collect() } else { Vec::new() };
                let shelf = |id: &str, entities: Vec<serde_json::Value>| serde_json::json!({"id":id,"total":entities.len(),"entities":entities});
                serde_json::json!([shelf("continue-listening", first), shelf("discover", Vec::new()), shelf("listen-again", Vec::new()),
                    shelf("newest-episodes", Vec::new()), shelf("continue-series", Vec::new()), shelf("recent-series", Vec::new())])
            }
            p if p.starts_with("/api/items/") => {
                let id = p.trim_start_matches("/api/items/");
                Self::show_json(self.shows.iter().find(|s| s.id == id)?, true)
            }
            _ => return None,
        })
    }
    pub fn take(&mut self) -> Vec<Served> { std::mem::take(&mut self.served) }
}
fn base64(text: &str) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in text.as_bytes().chunks(3) {
        let x = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
        out.push(T[(x >> 18) as usize] as char);
        out.push(T[((x >> 12) & 63) as usize] as char);
        out.push(if c.len() > 1 { T[((x >> 6) & 63) as usize] as char } else { '=' });
        out.push(if c.len() > 2 { T[(x & 63) as usize] as char } else { '=' });
    }
    out
}
fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => { out.push(u8::from_str_radix(&value[i + 1..i + 3], 16).unwrap_or(b'?')); i += 3; }
            b'+' => { out.push(b' '); i += 1; }
            b => { out.push(b); i += 1; }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub(crate) struct AbsServer {
    pub url: String,
    pub model: Arc<Mutex<Abs>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Drop for AbsServer {
    fn drop(&mut self) { self.stop.store(true, Ordering::SeqCst); self.thread.take().unwrap().join().unwrap(); }
}
impl AbsServer {
    pub fn new(model: Abs) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let model = Arc::new(Mutex::new(model));
        let stop = Arc::new(AtomicBool::new(false));
        let (shared, done) = (model.clone(), stop.clone());
        let png = crate::gfx::Image::placeholder(64).png().unwrap();
        let thread = std::thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                let Ok((mut socket, _)) = listener.accept() else { std::thread::sleep(Duration::from_millis(1)); continue; };
                let (shared, png) = (shared.clone(), png.clone());
                std::thread::spawn(move || {
                    socket.set_nonblocking(false).unwrap();
                    socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                    let mut head = Vec::new();
                    while !head.ends_with(b"\r\n\r\n") {
                        let mut b = [0; 1];
                        if socket.read(&mut b).unwrap_or(0) == 0 { return; }
                        head.push(b[0]);
                    }
                    let head = String::from_utf8_lossy(&head).into_owned();
                    let target = head.split_whitespace().nth(1).unwrap_or("").to_owned();
                    let condition = head.lines().find_map(|l| l.strip_prefix("If-None-Match: ").or_else(|| l.strip_prefix("if-none-match: "))).map(|v| v.trim().to_owned());
                    let (path, query_text) = target.split_once('?').unwrap_or((&target, ""));
                    let query: BTreeMap<String, String> = query_text.split('&').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_owned(), decode(v))).collect();
                    let mut model = shared.lock().unwrap();
                    let cover_of = path.strip_prefix("/api/items/").and_then(|p| p.strip_suffix("/cover"));
                    let (status, headers, body) = if let Some(item) = cover_of {
                        if model.no_cover.contains(item) { (404, "Content-Type: application/json\r\n".to_owned(), b"{}".to_vec()) }
                        else { (200, "Content-Type: image/png\r\n".to_owned(), png) }
                    } else if let Some(value) = model.answer(path, &query) {
                        let body = value.to_string().into_bytes();
                        let tag = format!("W/\"{:x}\"", <md5::Md5 as md5::Digest>::digest(&body));
                        if model.etags && condition.as_deref() == Some(tag.as_str()) { (304, format!("ETag: {tag}\r\n"), Vec::new()) }
                        else if model.etags { (200, format!("Content-Type: application/json\r\nETag: {tag}\r\n"), body) }
                        else { (200, "Content-Type: application/json\r\n".to_owned(), body) }
                    } else { (404, "Content-Type: application/json\r\n".to_owned(), b"{}".to_vec()) };
                    let filter = query.get("filter").map(|f| f.split('.').next().unwrap_or("").to_owned()).unwrap_or_default();
                    model.served.push(Served { path: path.to_owned(), filter, status, bytes: body.len() });
                    if let Some(mut hook) = model.after.take() { hook(&mut model, path); model.after = Some(hook); }
                    drop(model);
                    let reason = match status { 200 => "OK", 304 => "Not Modified", _ => "Not Found" };
                    let _ = write!(socket, "HTTP/1.1 {status} {reason}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                    let _ = socket.write_all(&body);
                });
            }
        });
        Self { url, model, stop, thread: Some(thread) }
    }
    pub fn take(&self) -> Vec<Served> { self.model.lock().unwrap().take() }
}

pub(crate) struct Shelf { pub lib: Arc<Library>, root: std::path::PathBuf }
impl Drop for Shelf {
    fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.root); }
}
pub(crate) fn shelf(server: &AbsServer, label: &str) -> Shelf {
    let root = crate::library::generated_test_directory(label);
    let paths = Paths::new(root.clone());
    paths.create_all(&crate::rt::Blocking::dedicated_thread()).unwrap();
    let mut config = Config::default();
    config.storage.reserve_mb = 2;
    config.audiobookshelf.url = server.url.clone();
    config.audiobookshelf.token = "generated-model".into();
    let lib = Library::open_with_auto_sync(paths, &config, Arc::new(|_| {}), false);
    lib.db().hold_checkpoints();
    Shelf { lib, root }
}
fn wait<T>(future: impl std::future::Future<Output = T>) -> T {
    crate::rt::Ui::new().unwrap().block_on(async { tokio::time::timeout(Duration::from_secs(60), future).await.expect("bounded sync") })
}
/// One round of both libraries, as the automatic worker runs it.
pub(crate) fn sync(lib: &Arc<Library>, round: &SyncRound) -> Result<(), LibError> {
    for (id, media) in [("lb", Media::Book), ("lp", Media::Podcast)] {
        wait(lib.sync_shelf_library_from(id.into(), media, round, |_| {}))?;
    }
    Ok(())
}
fn full(lib: &Arc<Library>) { sync(lib, &SyncRound::full()).unwrap(); }
fn markers(lib: &Arc<Library>) { sync(lib, &SyncRound::default()).unwrap(); }
fn writes(lib: &Arc<Library>) -> (u64, u64) { let db = lib.db(); (db.total_changes(), db.wal_bytes()) }

/// What the card holds, read back as the offline browser reads it.
#[derive(Debug, PartialEq, Default)]
pub(crate) struct View {
    books: Vec<(String, String)>,
    series: Vec<(String, Vec<String>)>,
    authors: Vec<(String, Vec<String>)>,
    shows: Vec<(String, Vec<String>)>,
    latest: Vec<String>,
}
fn all(lib: &Arc<Library>, library: &str, media: Media, section: &Section) -> Option<Vec<Entry>> {
    let mut out = Vec::new();
    for page in 0.. {
        let rows = wait(lib.shelf_browse_async(library, media, section, page, 50, Policy::CacheOnly)).ok()?;
        out.extend(rows.entries);
        if !rows.has_more { break; }
    }
    Some(out)
}
fn groups_view(lib: &Arc<Library>, list: Section) -> Vec<(String, Vec<String>)> {
    all(lib, "lb", Media::Book, &list).unwrap_or_default().into_iter().filter_map(|e| match e.kind {
        EntryKind::Group { section } => Some((e.title, all(lib, "lb", Media::Book, &section).unwrap_or_default().into_iter().map(|e| e.title).collect())),
        _ => None,
    }).collect()
}
pub(crate) fn stored_view(lib: &Arc<Library>) -> View {
    let books = all(lib, "lb", Media::Book, &Section::Books).unwrap_or_default().into_iter()
        .filter_map(|e| match e.kind { EntryKind::Book { item } => Some((item, e.title)), _ => None }).collect();
    let shows = all(lib, "lp", Media::Podcast, &Section::Books).unwrap_or_default().into_iter().filter_map(|e| match e.kind {
        EntryKind::Podcast { item } => Some((e.title, all(lib, "lp", Media::Podcast, &Section::Episodes(item)).unwrap_or_default().into_iter().map(|e| e.title).collect())),
        _ => None,
    }).collect();
    let latest = all(lib, "lp", Media::Podcast, &Section::LatestEpisodes).unwrap_or_default().into_iter().map(|e| e.id).collect();
    View { books, series: groups_view(lib, Section::Series), authors: groups_view(lib, Section::Authors), shows, latest }
}
pub(crate) fn server_view(abs: &Abs) -> View {
    let books = abs.books_by_title().into_iter().map(|b| (b.id.clone(), b.title.clone())).collect();
    let series = abs.names('s').into_iter().map(|(id, name, _)| {
        let mut members: Vec<&Book> = abs.books_by_title().into_iter().filter(|b| b.series.is_some_and(|s| format!("s-{}", s.0) == id)).collect();
        members.sort_by_key(|b| (b.series.map(|s| s.1), b.added));
        (name, members.into_iter().map(|b| b.title.clone()).collect())
    }).collect();
    let authors = abs.names('a').into_iter().map(|(id, name, _)| (name,
        abs.by_added().into_iter().filter(|b| b.authors.iter().any(|a| format!("a-{a}") == id)).map(|b| b.title.clone()).collect())).collect();
    let mut shows: Vec<&Show> = abs.shows.iter().collect();
    shows.sort_by(|a, b| a.title.cmp(&b.title));
    let shows = shows.into_iter().map(|s| (s.title.clone(), s.episodes.iter().map(|e| e.1.clone()).collect())).collect();
    let mut episodes: Vec<&(String, String, u64)> = abs.shows.iter().flat_map(|s| s.episodes.iter()).collect();
    episodes.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    View { books, series, authors, shows, latest: episodes.into_iter().map(|e| e.0.clone()).collect() }
}
fn pages(served: &[Served]) -> (usize, usize, usize) {
    // The account check runs once per client, not per round.
    let pages: Vec<&Served> = served.iter().filter(|s| !s.path.contains("/cover") && s.path != "/api/me").collect();
    (pages.len(), pages.iter().filter(|s| s.status == 200).count(), pages.iter().map(|s| s.bytes).sum())
}
fn group_requests(served: &[Served]) -> usize { served.iter().filter(|s| matches!(s.filter.as_str(), "series" | "authors" | "narrators" | "genres")).count() }

#[test]
fn unchanged_library_round_is_a_few_marker_requests_and_no_page_writes() {
    let server = AbsServer::new(Abs::with(130, 3, 70));
    let shelf = shelf(&server, "abs-budget");
    full(&shelf.lib);
    assert_eq!(stored_view(&shelf.lib), server_view(&server.model.lock().unwrap()));
    let first = server.take();
    assert_eq!(first.iter().filter(|s| s.path.ends_with("/personalized")).count(), 2, "one personalized document per library");
    let before = writes(&shelf.lib);
    markers(&shelf.lib);
    let again = server.take();
    let (requests, _, _) = pages(&again);
    // Books: newest item, personalized. Podcasts: newest show, newest
    // episode, personalized.
    assert_eq!(requests, 5, "an unchanged round: {:?}", again.iter().map(|s| (&s.path, s.status)).collect::<Vec<_>>());
    assert!(again.iter().all(|s| !s.path.contains("/cover")), "no cover is asked for");
    let after = writes(&shelf.lib);
    assert_eq!(after.0 - before.0, 0, "rows written by an unchanged round");
    assert!(after.1 - before.1 <= 2 * 4120, "WAL bytes written by an unchanged round: {}", after.1 - before.1);
    assert_eq!(stored_view(&shelf.lib), server_view(&server.model.lock().unwrap()));
}

#[test]
fn a_full_resync_revalidates_every_page_with_304s() {
    let server = AbsServer::new(Abs::with(130, 3, 70));
    let shelf = shelf(&server, "abs-full-304");
    full(&shelf.lib);
    server.take();
    let before = writes(&shelf.lib);
    full(&shelf.lib);
    let again = server.take();
    let changed: Vec<&str> = again.iter().filter(|s| s.status == 200 && !s.path.contains("/cover") && s.path != "/api/me").map(|s| s.path.as_str()).collect();
    // The markers (no validator kept) and the shows' recent-episode marker.
    assert!(changed.iter().all(|p| p.ends_with("/items") || p.ends_with("/recent-episodes")) && changed.len() <= 3, "fetched whole: {changed:?}");
    assert!(again.iter().all(|s| !s.path.contains("/cover")), "no stored cover is fetched again");
    assert!(writes(&shelf.lib).0 - before.0 <= 4, "rows: the completion records");
}

#[test]
fn group_pages_are_derived_from_the_books_and_equal_the_servers() {
    let server = AbsServer::new(Abs::with(140, 0, 0));
    let shelf = shelf(&server, "abs-derived");
    full(&shelf.lib);
    let served = server.take();
    assert_eq!(group_requests(&served), 0, "no series, author, narrator or genre page is fetched");
    // Each derived page against the page the server sends for it.
    let client = wait(shelf.lib.shelf_client_async()).unwrap();
    let namespace = shelf.lib.shelf_namespace();
    let generation = shelf.lib.shelf_generation();
    let mut compared = 0;
    for list in [Section::Series, Section::Authors, Section::Narrators, Section::Genres] {
        let groups = wait(shelf.lib.stored_rows(&namespace, "lb", Media::Book, &list, &generation)).unwrap();
        assert!(!groups.is_empty(), "{list:?} has groups");
        for (entry, _) in groups {
            let EntryKind::Group { section } = entry.kind else { continue };
            for page in 0.. {
                let stored = wait(shelf.lib.stored_page(&namespace, "lb", Media::Book, &section, page, &generation)).unwrap().expect("a derived page");
                let fetched = match wait(client.browse_page_validated_async("lb", Media::Book, &section, page, 50, None)).unwrap() {
                    subsonic::audiobookshelf_browse::Validated::Changed(page, _) => page,
                    subsonic::audiobookshelf_browse::Validated::Unchanged => unreachable!(),
                };
                assert_eq!(stored, fetched, "{section:?} page {page}");
                compared += 1;
                if !stored.has_more { break; }
            }
        }
    }
    assert!(compared >= 15, "pages compared: {compared}");
    assert_eq!(stored_view(&shelf.lib), server_view(&server.model.lock().unwrap()));
}

#[test]
fn a_group_whose_name_is_ambiguous_is_fetched_instead() {
    let mut abs = Abs::with(40, 0, 0);
    // A series named "Tales, Collected": its books' seriesName cannot be
    // split back into it, so its page is fetched.
    for b in abs.books.iter_mut().filter(|b| b.series.is_some()).take(4) { b.series = Some((90, b.added as u32 % 3)); }
    let server = AbsServer::new(abs);
    let shelf = shelf(&server, "abs-derived-fallback");
    full(&shelf.lib);
    let served = server.take();
    assert_eq!(group_requests(&served), 1, "only the ambiguous series is fetched");
    assert_eq!(stored_view(&shelf.lib), server_view(&server.model.lock().unwrap()));
}

#[test]
fn a_series_whose_sequences_repeat_is_fetched_instead() {
    let mut abs = Abs::with(60, 0, 0);
    // The server's order among equal sequences is not in the rows.
    for b in abs.books.iter_mut().filter(|b| b.series.is_some_and(|s| s.0 == 1)) { b.series = Some((1, 1)); }
    let server = AbsServer::new(abs);
    let shelf = shelf(&server, "abs-derived-ties");
    full(&shelf.lib);
    let served = server.take();
    assert_eq!(served.iter().filter(|s| s.filter == "series").count(), 1, "only the series with repeated sequences is fetched");
    assert_eq!(stored_view(&shelf.lib), server_view(&server.model.lock().unwrap()));
}

#[test]
fn without_etags_a_full_resync_refetches_every_page_but_writes_none() {
    let mut abs = Abs::with(130, 3, 70);
    abs.etags = false;
    let server = AbsServer::new(abs);
    let shelf = shelf(&server, "abs-no-etag");
    full(&shelf.lib);
    let (requests, _, full_bytes) = pages(&server.take());
    let before = writes(&shelf.lib);
    full(&shelf.lib);
    let (again, changed, bytes) = pages(&server.take());
    assert_eq!((again, changed, bytes), (requests, requests, full_bytes));
    assert!(writes(&shelf.lib).0 - before.0 <= 4, "an unchanged page body is not written again");
}

#[test]
fn added_and_removed_books_and_shows_are_seen_by_the_marker_round() {
    let server = AbsServer::new(Abs::with(120, 2, 10));
    let shelf = shelf(&server, "abs-incremental");
    full(&shelf.lib);
    server.take();
    let changes: [fn(&mut Abs); 4] = [
        |m| m.add_book(Some("Aardvark Added")),
        |m| { m.books.remove(5); },
        |m| m.add_show(4),
        |m| { m.shows.remove(0); },
    ];
    for change in changes {
        change(&mut server.model.lock().unwrap());
        markers(&shelf.lib);
        assert_eq!(stored_view(&shelf.lib), server_view(&server.model.lock().unwrap()));
        let served = server.take();
        assert_eq!(group_requests(&served), 0);
    }
    // A rename moves no marker: the marker round keeps the old title, a
    // full resync (daily while charging, or asked for) brings it.
    {
        let mut m = server.model.lock().unwrap();
        let id = m.books_by_title()[7].id.clone();
        m.books.iter_mut().find(|b| b.id == id).unwrap().title = "Zz Renamed".into();
    }
    markers(&shelf.lib);
    assert_ne!(stored_view(&shelf.lib), server_view(&server.model.lock().unwrap()), "markers do not see an edit");
    full(&shelf.lib);
    assert_eq!(stored_view(&shelf.lib), server_view(&server.model.lock().unwrap()));
}

#[test]
fn a_new_episode_refetches_only_its_show() {
    let server = AbsServer::new(Abs::with(0, 5, 30));
    let shelf = shelf(&server, "abs-new-episode");
    full(&shelf.lib);
    server.take();
    server.model.lock().unwrap().add_episode(2);
    markers(&shelf.lib);
    let served = server.take();
    let items: Vec<&str> = served.iter().filter(|s| s.path.starts_with("/api/items/") && !s.path.ends_with("/cover")).map(|s| s.path.as_str()).collect();
    let changed_show = server.model.lock().unwrap().shows[2].id.clone();
    assert_eq!(items, [format!("/api/items/{changed_show}").as_str()], "only the show with a new episode");
    assert_eq!(stored_view(&shelf.lib), server_view(&server.model.lock().unwrap()));
}

#[test]
fn first_marker_round_caches_latest_episodes_through_the_terminal_page() {
    for count in [199, 200, 201] {
        let server = AbsServer::new(Abs::with(0, 1, count));
        let shelf = shelf(&server, "abs-latest-first");
        markers(&shelf.lib);
        let expected = server_view(&server.model.lock().unwrap()).latest;
        let cached = all(&shelf.lib, "lp", Media::Podcast, &Section::LatestEpisodes)
            .unwrap_or_else(|| panic!("{count} episodes: continuation page missing"));
        assert_eq!(cached.into_iter().map(|entry| entry.id).collect::<Vec<_>>(), expected);
    }
}

#[test]
fn a_marker_round_updates_the_latest_episodes_tail_after_offsets_shift() {
    let server = AbsServer::new(Abs::with(0, 35, 30));
    let shelf = shelf(&server, "abs-latest-tail");
    full(&shelf.lib);
    server.take();
    server.model.lock().unwrap().add_episode(0);
    markers(&shelf.lib);
    let changed = server.take();
    assert_eq!(stored_view(&shelf.lib), server_view(&server.model.lock().unwrap()));
    let latest_requests = |served: &[Served]| served.iter()
        .filter(|row| row.path.ends_with("/recent-episodes")).count();
    // The probe and show discovery take one request each. 1,051 episodes
    // need 22 cached pages; offsets of all previous pages moved.
    assert_eq!(latest_requests(&changed), 24);
    assert_eq!(changed.iter().filter(|row| row.path.starts_with("/api/items/")
        && !row.path.ends_with("/cover")).count(), 1);
    markers(&shelf.lib);
    let unchanged = server.take();
    assert_eq!(latest_requests(&unchanged), 1);
    assert_eq!(pages(&unchanged).0, 5);
}

#[test]
fn unchanged_legacy_markers_repair_a_missing_latest_episodes_continuation() {
    let server = AbsServer::new(Abs::with(0, 1, 200));
    let shelf = shelf(&server, "abs-latest-legacy");
    let lib = &shelf.lib;
    let client = wait(lib.shelf_client_async()).unwrap();
    let current = wait(lib.shelf_markers(&client, "lp", Media::Podcast)).unwrap();
    for page in 0..4 {
        wait(lib.shelf_browse_async("lp", Media::Podcast, &Section::LatestEpisodes,
            page, SIZE, Policy::NetworkOnly)).unwrap();
    }
    let scope = manifest_key(&lib.shelf_namespace(), "lp", Media::Podcast);
    let legacy = serde_json::json!({
        "newest": current.newest, "total": current.total, "episode": current.episode,
    }).to_string();
    wait(lib.shelf_db_run(lib.shelf_generation(), move |blocking, db| {
        db.put_spoken_response(blocking, &markers_key(&scope), &legacy).map_err(fail)?;
        db.put_spoken_response_durable(blocking, &scope, r#"{"pages":1}"#).map_err(fail)
    })).unwrap();
    assert!(all(lib, "lp", Media::Podcast, &Section::LatestEpisodes).is_none());

    markers(lib);
    assert_eq!(stored_view(lib), server_view(&server.model.lock().unwrap()));
    server.take();
    markers(lib);
    assert_eq!(pages(&server.take()).0, 5);
}

#[test]
fn covers_the_server_lacks_are_asked_for_again_only_by_a_full_resync() {
    let mut abs = Abs::with(60, 0, 0);
    abs.no_cover = abs.books.iter().take(7).map(|b| b.id.clone()).collect();
    let server = AbsServer::new(abs);
    let shelf = shelf(&server, "abs-missing-covers");
    full(&shelf.lib);
    let covers = |served: &[Served]| served.iter().filter(|s| s.path.ends_with("/cover")).count();
    assert!(covers(&server.take()) >= 60);
    server.model.lock().unwrap().add_book(Some("Aaa New"));
    markers(&shelf.lib);
    assert_eq!(covers(&server.take()), 1, "the new book's cover only, not the 7 the server lacks");
    full(&shelf.lib);
    assert_eq!(covers(&server.take()), 7, "a full resync asks again");
}

#[test]
fn a_book_added_between_pages_is_not_missed_or_listed_twice() {
    let mut abs = Abs::with(140, 0, 0);
    let mut fired = false;
    abs.after = Some(Box::new(move |m, path| {
        if path == "/api/libraries/lb/items" && !fired { fired = true; m.add_book(Some("Aaa First")); }
    }));
    let server = AbsServer::new(abs);
    let shelf = shelf(&server, "abs-shift");
    full(&shelf.lib);
    let view = stored_view(&shelf.lib);
    let mut ids: Vec<&String> = view.books.iter().map(|b| &b.0).collect();
    ids.sort(); ids.dedup();
    assert_eq!(ids.len(), view.books.len(), "no book is listed twice");
    assert_eq!(view, server_view(&server.model.lock().unwrap()));
}

#[test]
fn an_interrupted_sync_resumes_to_the_state_of_an_uninterrupted_one() {
    let server = AbsServer::new(Abs::with(110, 3, 60));
    let reference = shelf(&server, "abs-reference");
    full(&reference.lib);
    let full_requests = server.take().len();
    let expected = stored_view(&reference.lib);
    for stop_at in [3, full_requests / 3, full_requests / 2, full_requests - 3] {
        let shelf = shelf(&server, "abs-interrupted");
        let round = SyncRound::full();
        let (lib, partial) = (shelf.lib.clone(), round.clone());
        server.take();
        let task = crate::rt::io().spawn(async move { let _ = lib.sync_shelf_library_from("lb".into(), Media::Book, &partial, |_| {}).await;
            let _ = lib.sync_shelf_library_from("lp".into(), Media::Podcast, &partial, |_| {}).await; });
        let end = Instant::now() + Duration::from_secs(30);
        while server.model.lock().unwrap().served.len() < stop_at && Instant::now() < end && !task.is_finished() { std::thread::sleep(Duration::from_millis(1)); }
        task.abort();
        let _ = wait(task);
        sync(&shelf.lib, &round).unwrap();
        assert_eq!(stored_view(&shelf.lib), expected, "interrupted after {stop_at} requests");
        let resumed = server.take().len();
        assert!(resumed < full_requests + 8, "the resumed sync does not start again: {resumed} requests against {full_requests}");
    }
}

#[test]
fn pages_a_full_resync_no_longer_reaches_are_removed() {
    let server = AbsServer::new(Abs::with(110, 3, 120));
    let shelf = shelf(&server, "abs-prune");
    full(&shelf.lib);
    let removed = {
        let mut m = server.model.lock().unwrap();
        let show = m.shows.remove(1).id;
        m.books.truncate(40);
        show
    };
    let before = shelf.lib.db().audit_spoken_responses(&crate::rt::Blocking::dedicated_thread()).unwrap().len();
    full(&shelf.lib);
    assert!(all(&shelf.lib, "lp", Media::Podcast, &Section::Episodes(removed.clone())).is_none(), "a removed show's episodes are gone");
    assert!(wait(shelf.lib.shelf_browse_cached("lb", Media::Book, &Section::Books, 2, 50, Policy::CacheOnly, false)).is_err(), "a page past the shortened list is gone");
    let after = shelf.lib.db().audit_spoken_responses(&crate::rt::Blocking::dedicated_thread()).unwrap().len();
    assert!(after < before, "stored pages {before} -> {after}");
    assert_eq!(stored_view(&shelf.lib), server_view(&server.model.lock().unwrap()));
}

mod generated {
    use super::*;
    use proptest::prelude::*;

    #[derive(Clone, Debug)]
    enum Change { AddBook(Option<u16>), RemoveBook(u16), RenameBook(u16, u16), Series(u16, u8), AddShow(u8), RemoveShow(u8), AddEpisode(u8) }
    impl Change {
        /// Seen by the marker round (an item or episode added or removed).
        fn marked(&self) -> bool { !matches!(self, Change::RenameBook(..) | Change::Series(..)) }
    }
    fn apply(m: &mut Abs, change: &Change) {
        match *change {
            Change::AddBook(title) => m.add_book(title.map(|t| format!("Gen {t:05}")).as_deref()),
            Change::RemoveBook(at) if !m.books.is_empty() => { let n = m.books.len(); m.books.remove(at as usize % n); }
            Change::RenameBook(at, title) if !m.books.is_empty() => { let n = m.books.len(); m.books[at as usize % n].title = format!("Ren {title:05}"); }
            Change::Series(at, s) if !m.books.is_empty() => { let n = m.books.len(); m.books[at as usize % n].series = (s % 5 != 0).then_some((u32::from(s % 5), u32::from(s % 3))); }
            Change::AddShow(episodes) => m.add_show(usize::from(episodes % 60)),
            Change::RemoveShow(at) if !m.shows.is_empty() => { let n = m.shows.len(); m.shows.remove(at as usize % n); }
            Change::AddEpisode(at) if !m.shows.is_empty() => { let n = m.shows.len(); m.add_episode(at as usize % n); }
            _ => {}
        }
    }
    fn change() -> impl Strategy<Value = Change> {
        prop_oneof![
            proptest::option::of(any::<u16>()).prop_map(Change::AddBook), any::<u16>().prop_map(Change::RemoveBook),
            (any::<u16>(), any::<u16>()).prop_map(|(a, b)| Change::RenameBook(a, b)), (any::<u16>(), any::<u8>()).prop_map(|(a, b)| Change::Series(a, b)),
            any::<u8>().prop_map(Change::AddShow), any::<u8>().prop_map(Change::RemoveShow), any::<u8>().prop_map(Change::AddEpisode),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 6, failure_persistence: None, ..ProptestConfig::default() })]
        /// The library changes between rounds. After a marker round the card
        /// holds the server's library when every change moved a marker;
        /// after a full resync it always does.
        #[test]
        fn generated_mutations_leave_the_card_equal_to_the_server(
            books in 0usize..110, shows in 0usize..3,
            steps in proptest::collection::vec(proptest::collection::vec(change(), 0..6), 1..4),
        ) {
            let server = AbsServer::new(Abs::with(books, shows, 55));
            let shelf = shelf(&server, "abs-generated");
            full(&shelf.lib);
            for between in steps {
                { let mut m = server.model.lock().unwrap(); for change in &between { apply(&mut m, change); } }
                markers(&shelf.lib);
                if between.iter().all(Change::marked) {
                    prop_assert_eq!(stored_view(&shelf.lib), server_view(&server.model.lock().unwrap()));
                }
                full(&shelf.lib);
                prop_assert_eq!(stored_view(&shelf.lib), server_view(&server.model.lock().unwrap()));
            }
        }

        /// One change lands while the Books list is being paged: every book
        /// there before and after it is listed once, nothing is listed that
        /// was never there, and the next full resync holds the server's library.
        #[test]
        fn a_change_during_paging_never_drops_or_repeats_a_book(
            books in 51usize..120, change in change(), at in 1usize..4,
        ) {
            let mut abs = Abs::with(books, 0, 0);
            let (mut pending, mut seen) = (Some(change), 0usize);
            abs.after = Some(Box::new(move |m, path| if path == "/api/libraries/lb/items" {
                seen += 1;
                if seen == at { if let Some(c) = pending.take() { apply(m, &c); } }
            }));
            let server = AbsServer::new(abs);
            let before: std::collections::HashSet<String> = server.model.lock().unwrap().books.iter().map(|b| b.id.clone()).collect();
            let shelf = shelf(&server, "abs-generated-paging");
            full(&shelf.lib);
            let after: std::collections::HashSet<String> = server.model.lock().unwrap().books.iter().map(|b| b.id.clone()).collect();
            let listed: Vec<String> = stored_view(&shelf.lib).books.into_iter().map(|b| b.0).collect();
            let unique: std::collections::HashSet<String> = listed.iter().cloned().collect();
            prop_assert_eq!(unique.len(), listed.len(), "a book is listed twice");
            for id in before.intersection(&after) { prop_assert!(unique.contains(id), "{} was missed", id); }
            for id in &unique { prop_assert!(before.contains(id) || after.contains(id)); }
            server.model.lock().unwrap().after = None;
            full(&shelf.lib);
            prop_assert_eq!(stored_view(&shelf.lib), server_view(&server.model.lock().unwrap()));
        }
    }
}
