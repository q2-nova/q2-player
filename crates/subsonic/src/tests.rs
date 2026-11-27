use super::*;
use crate::client::{encode, parse_envelope};
use md5::{Digest, Md5};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::mpsc;

/// An HTTP proxy serving requests for an intentionally unresolvable name.
/// Covers the API agent and the separate media agent (including Range).
#[test]
fn proxy_carries_api_art_streams_and_downloads() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let worker = std::thread::spawn(move || {
        for endpoint in ["ping", "getCoverArt", "stream", "download"] {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(std::time::Duration::from_secs(3))).unwrap();
            let mut reader = BufReader::new(socket.try_clone().unwrap());
            let request = read_headers(&mut reader);
            assert!(request.starts_with(&format!("GET http://music.tailnet.invalid:4533/rest/{endpoint}?")), "{request}");
            let (body, typ, code) = if endpoint == "ping" {
                (br#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#.as_slice(), "application/json", "200 OK")
            } else {
                if endpoint == "stream" { assert!(request.to_lowercase().contains("range: bytes=12-\r\n")); }
                (b"media".as_slice(), "application/octet-stream", if endpoint == "stream" { "206 Partial Content" } else { "200 OK" })
            };
            write!(socket, "HTTP/1.1 {code}\r\nContent-Length: {}\r\nContent-Type: {typ}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
            socket.write_all(body).unwrap();
        }
    });
    let client = Client::with_proxy(Credentials {
        server: "http://music.tailnet.invalid:4533".into(), user: "me".into(), auth: Auth::Password("pw".into()),
    }, "test", Some(&proxy)).unwrap();
    client.get(&calls::ping()).unwrap();
    assert_eq!(client.fetch_bytes(&client.cover_art_request("art", None), 100).unwrap(), b"media");
    let mut response = client.open(&client.stream_request("song", &StreamOptions::default()), 12).unwrap();
    assert!(response.partial);
    let mut bytes = Vec::new();
    response.reader.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"media");
    assert_eq!(client.fetch_bytes(&client.download_request("song"), 100).unwrap(), b"media");
    worker.join().unwrap();
}

fn read_headers(reader: &mut impl BufRead) -> String {
    let mut head = String::new();
    loop {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).unwrap() > 0);
        if line == "\r\n" { return head; }
        head.push_str(&line);
    }
}

#[test]
fn offline_cover_fetch_distinguishes_missing_auth_network_and_bounded_binary() {
    use std::time::{Duration,Instant};
    let listener=TcpListener::bind("127.0.0.1:0").unwrap();listener.set_nonblocking(true).unwrap();
    let proxy=format!("http://{}",listener.local_addr().unwrap());
    let worker=std::thread::spawn(move|| {
        for (status,body) in [
            ("200 OK",b"generated-image-bytes".to_vec()),("404 Not Found",Vec::new()),
            ("200 OK",br#"<subsonic-response status="failed"><error code="70"/></subsonic-response>"#.to_vec()),
            ("200 OK",br#"{"subsonic-response":{"status":"failed","error":{"code":70}}}"#.to_vec()),
            ("200 OK",br#"<subsonic-response status="failed"><error code="40" message="generated-secret"/></subsonic-response>"#.to_vec()),
            ("404 Not Found",br#"{"subsonic-response":{"status":"failed","error":{"code":40,"message":"generated-auth-secret"}}}"#.to_vec()),
            ("503 Service Unavailable",Vec::new()),("200 OK",vec![b'x';4*1024*1024+1]),
        ] {
            let deadline=Instant::now()+Duration::from_secs(5);
            let mut socket=loop {match listener.accept() {
                Ok((socket,_))=>break socket,
                Err(error) if error.kind()==std::io::ErrorKind::WouldBlock && Instant::now()<deadline=>std::thread::sleep(Duration::from_millis(1)),
                Err(error)=>panic!("generated cover fixture accept: {error}"),
            }};
            socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();socket.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
            let request=read_headers(&mut BufReader::new(socket.try_clone().unwrap()));
            assert!(request.starts_with("GET http://generated-cover.fixture.invalid/rest/getCoverArt?"));assert!(request.contains("size=320"));
            let content_type=if body.starts_with(b"<") {"text/xml"} else if body.starts_with(b"{") {"application/json"} else {"image/png"};
            write!(socket,"HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).unwrap();let _=socket.write_all(&body);
        }
    });
    let client=Client::with_proxy(Credentials {server:"http://generated-cover.fixture.invalid".into(),user:"synthetic".into(),auth:Auth::ApiKey("generated-key".into())},"generated-test",Some(&proxy)).unwrap();
    assert_eq!(client.cover_art("cover",320).unwrap(),Some(b"generated-image-bytes".to_vec()));
    for _ in 0..3 {assert!(client.cover_art("cover",320).unwrap().is_none());}
    let auth=client.cover_art("cover",320).unwrap_err();assert!(auth.is_auth());assert!(!auth.to_string().contains("generated-secret"));
    let contradictory=client.cover_art("cover",320).unwrap_err();assert!(contradictory.is_auth());assert!(!contradictory.to_string().contains("generated-auth-secret"));
    assert!(client.cover_art("cover",320).unwrap_err().is_offline());assert!(matches!(client.cover_art("cover",320),Err(Error::Json(_))));
    worker.join().unwrap();
}

#[test]
fn refused_cover_connection_never_exposes_authentication_url_fields() {
    let listener=TcpListener::bind("127.0.0.1:0").unwrap();let server=format!("http://{}/generated-private-path",listener.local_addr().unwrap());drop(listener);
    for auth in [Auth::Password("generated-password".into()),Auth::ApiKey("generated-api-key".into())] {
        let client=Client::new(Credentials {server:server.clone(),user:"generated-user".into(),auth},"generated-test");
        let error=client.cover_art("cover",320).unwrap_err().to_string();
        for secret in [server.as_str(),"generated-password","generated-api-key","generated-user","u=","t=","s="] {assert!(!error.contains(secret),"cover failure must be sanitized");}
    }
}

#[test]
fn https_proxy_still_requires_tls() {
    use std::io::Read;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy = format!("http://{}", listener.local_addr().unwrap());
    let worker = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        socket.set_read_timeout(Some(std::time::Duration::from_secs(3))).unwrap();
        let mut reader = BufReader::new(socket.try_clone().unwrap());
        assert!(read_headers(&mut reader).starts_with("CONNECT music.tailnet.invalid:443 HTTP/1.1"));
        socket.write_all(b"HTTP/1.1 200 Connection established\r\n\r\n").unwrap();
        let mut header = [0; 3];
        reader.read_exact(&mut header).unwrap();
        assert_eq!(header, [0x16, 0x03, 0x01], "TLS ClientHello, never plaintext HTTP");
    });
    let client = Client::with_proxy(Credentials {
        server: "https://music.tailnet.invalid".into(), user: "me".into(), auth: Auth::Password("pw".into()),
    }, "test", Some(&proxy)).unwrap();
    assert!(client.get(&calls::ping()).is_err());
    worker.join().unwrap();
}

#[test]
fn unavailable_proxy_does_not_connect_directly() {
    let target = TcpListener::bind("127.0.0.1:0").unwrap();
    target.set_nonblocking(true).unwrap();
    let unused = TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy = format!("http://{}", unused.local_addr().unwrap());
    drop(unused);
    let client = Client::with_proxy(Credentials {
        server: format!("http://{}", target.local_addr().unwrap()), user: "me".into(), auth: Auth::Password("pw".into()),
    }, "test", Some(&proxy)).unwrap();
    assert!(client.get(&calls::ping()).is_err());
    assert_eq!(target.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
}

const ALBUM: &str = r#"{"subsonic-response":{"status":"ok","version":"1.16.1","type":"navidrome",
 "serverVersion":"0.53.3","openSubsonic":true,"album":{"id":"al-1","name":"Kid A","artist":"Radiohead",
 "artistId":"ar-1","coverArt":"al-1_0","songCount":2,"duration":600,"year":2000,"genre":"Electronic",
 "created":"2024-01-01T00:00:00Z","song":[
  {"id":"s1","parent":"al-1","title":"Everything in Its Right Place","album":"Kid A","artist":"Radiohead",
   "track":1,"discNumber":1,"year":2000,"coverArt":"al-1_0","size":30000000,"contentType":"audio/flac",
   "suffix":"flac","duration":251,"bitRate":950,"samplingRate":44100,"bitDepth":16,"channelCount":2,
   "albumId":"al-1","artistId":"ar-1","replayGain":{"trackGain":-6.5,"albumGain":-7.1,"trackPeak":0.98}},
  {"id":"s2","title":"Kid A","album":"Kid A","artist":"Radiohead","track":2,"duration":284,
   "suffix":"mp3","bitRate":320,"starred":"2024-02-02T00:00:00Z"}]}}}"#;

#[test]
fn parses_album_with_songs() {
    let body = parse_envelope(ALBUM).unwrap();
    let album = calls::album("al-1").parse(&body).unwrap();
    assert_eq!(album.album.name, "Kid A");
    assert_eq!(album.album.year, Some(2000));
    assert_eq!(album.song.len(), 2);
    assert_eq!(album.song[0].replay_gain.as_ref().unwrap().track_gain, Some(-6.5));
    assert_eq!(album.song[0].quality_label(), "FLAC 16/44.1");
    assert_eq!(album.song[1].quality_label(), "MP3 320");
    assert!(album.song[1].starred.is_some());
}

#[test]
fn missing_lists_parse_empty() {
    let body = parse_envelope(r#"{"subsonic-response":{"status":"ok","version":"1.16.1","starred2":{}}}"#).unwrap();
    let s = calls::starred().parse(&body).unwrap();
    assert!(s.song.is_empty() && s.album.is_empty());
    let body = parse_envelope(r#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#).unwrap();
    assert!(calls::playlists().parse(&body).unwrap().playlist.is_empty());
}

#[test]
fn api_errors() {
    let e = parse_envelope(
        r#"{"subsonic-response":{"status":"failed","version":"1.16.1","error":{"code":40,"message":"Wrong username or password"}}}"#,
    )
    .unwrap_err();
    assert!(e.is_auth());
    assert!(!e.is_offline());
    assert!(matches!(e, Error::Api { code: 40, .. }));
    assert!(parse_envelope("<html>").is_err());
}

#[test]
fn cache_keys_are_stable() {
    let r = calls::album_list(AlbumListType::Newest, 50, 100).request;
    assert_eq!(r.cache_key(), "getAlbumList2?type=newest&size=50&offset=100");
    assert_eq!(calls::artists().request.cache_key(), "getArtists");
}

#[test]
fn encodes_queries() {
    assert_eq!(encode("AC/DC & Co"), "AC%2FDC%20%26%20Co");
    assert_eq!(encode("Sigur Rós"), "Sigur%20R%C3%B3s");
}

#[test]
fn url_carries_salted_token() {
    let c = Client::new(
        Credentials { server: "http://h:4533/".into(), user: "me".into(), auth: Auth::Password("pw".into()) },
        "q2nd",
    );
    let url = c.url(&calls::ping().request);
    assert!(url.starts_with("http://h:4533/rest/ping?"));
    let q: Vec<(&str, &str)> = url.split_once('?').unwrap().1.split('&').map(|kv| kv.split_once('=').unwrap()).collect();
    let get = |k| q.iter().find(|(kk, _)| *kk == k).unwrap().1;
    let want: String = Md5::digest(format!("pw{}", get("s"))).iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(get("t"), want);
    assert_eq!(get("u"), "me");
    assert!(!url.contains("pw&"));

    let k = Client::new(
        Credentials { server: "http://h".into(), user: String::new(), auth: Auth::ApiKey("key".into()) },
        "q2nd",
    );
    let url = k.url(&calls::ping().request);
    assert!(url.contains("apiKey=key") && !url.contains("u="));
}

/// A one-shot HTTP server answering a single request; returns the request
/// line and headers it saw.
fn serve_once(status: &str, content_type: &str, body: &'static [u8]) -> (String, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = mpsc::channel();
    let status = status.to_string();
    let content_type = content_type.to_string();
    std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut head = String::new();
        let mut r = BufReader::new(s.try_clone().unwrap());
        loop {
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
            head.push_str(&line);
        }
        let _ = tx.send(head);
        write!(s, "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
        s.write_all(body).unwrap();
    });
    (format!("http://{addr}"), rx)
}

fn client(server: String) -> Client {
    Client::new(Credentials { server, user: "u".into(), auth: Auth::Password("p".into()) }, "q2nd-test")
}

#[test]
fn artist_image_urls_preserve_signatures_without_api_credentials() {
    for proxied in [false, true] {
        let (server, seen) = serve_once("200 OK", "image/webp", b"image");
        let c = Client::with_proxy(Credentials {
            server: "http://music.invalid".into(), user: "private-user".into(), auth: Auth::ApiKey("private-key".into()),
        }, "test", proxied.then_some(server.as_str())).unwrap();
        let host = if proxied { "http://images.invalid" } else { &server };
        let url = format!("{host}/artist.webp?signature=a%2Bb%26c");
        assert_eq!(c.fetch_image_url(&url, 5).unwrap(), b"image");
        let head = seen.recv().unwrap();
        assert!(head.contains("/artist.webp?signature=a%2Bb%26c HTTP/1.1"));
        assert!(!head.contains("private-") && !head.contains("apiKey") && !head.to_lowercase().contains("authorization:"));
    }
}

#[test]
fn image_url_size_and_scheme_are_bounded() {
    let (server, _) = serve_once("200 OK", "image/png", b"too large");
    let c = client(server.clone());
    assert!(c.fetch_image_url(&server, 4).is_err());
    assert!(c.fetch_image_url("file:///tmp/image.png", 100).is_err());
}

#[test]
fn artist_info_prefers_small_images_and_tolerates_missing_fields() {
    let call = calls::artist_info("artist");
    assert_eq!(call.request.cache_key(), "getArtistInfo2?id=artist&count=0");
    let info = call.parse(&serde_json::json!({"artistInfo2": {
        "smallImageUrl": "small", "mediumImageUrl": "", "largeImageUrl": "large"
    }})).unwrap();
    assert_eq!(info.image_urls().collect::<Vec<_>>(), vec!["small", "large"]);
    assert_eq!(call.parse(&serde_json::json!({"artistInfo2":{}})).unwrap().image_urls().count(), 0);
}

#[test]
fn fetch_over_http() {
    let (url, seen) = serve_once("200 OK", "application/json", ALBUM.as_bytes());
    let album = client(url).get(&calls::album("al-1")).unwrap();
    assert_eq!(album.song.len(), 2);
    let head = seen.recv().unwrap();
    assert!(head.starts_with("GET /rest/getAlbum?"), "{head}");
    assert!(head.contains("&id=al-1"));
}

#[test]
fn stream_with_range() {
    let (url, seen) = serve_once("206 Partial Content", "audio/flac", b"fLaC....");
    let c = client(url);
    let req = c.stream_request("s1", &StreamOptions { format: Some("raw".into()), max_bit_rate: Some(0) });
    let mut resp = c.open(&req, 1000).unwrap();
    assert!(resp.partial);
    assert_eq!(resp.content_length, Some(8));
    let mut b = Vec::new();
    resp.reader.read_to_end(&mut b).unwrap();
    assert_eq!(&b, b"fLaC....");
    let head = seen.recv().unwrap();
    assert!(head.contains("format=raw") && !head.contains("maxBitRate"));
    assert!(head.to_ascii_lowercase().contains("range: bytes=1000-"));
}

#[test]
fn bounded_ranges_validate_offsets_and_ignore_full_responses() {
    for (code, range, body, valid) in [
        ("206 Partial Content", "bytes 100-103/1000", "data", true),
        ("206 Partial Content", "bytes 0-3/1000", "data", false),
        ("206 Partial Content", "bytes 100-104/1000", "data!", false),
        ("206 Partial Content", "bytes 100-103/1000", "bad", false),
        ("206 Partial Content", "bytes 100-103/103", "data", false),
        ("200 OK", "", "full", true),
    ] {
        let server = TcpListener::bind("127.0.0.1:0").unwrap();
        let c = client(format!("http://{}", server.local_addr().unwrap()));
        let worker = std::thread::spawn(move || {
            let (mut socket, _) = server.accept().unwrap();
            socket.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
            let request = read_headers(&mut BufReader::new(socket.try_clone().unwrap()));
            assert!(request.to_ascii_lowercase().contains("range: bytes=100-103\r\n"));
            write!(socket, "HTTP/1.1 {code}\r\nContent-Range: {range}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        });
        let result = c.read_range(&c.stream_request("fixture", &StreamOptions::default()), 100, 4);
        assert_eq!(result.is_ok(), valid);
        if valid { assert_eq!(result.unwrap(), if code == "200 OK" { None } else { Some(b"data".to_vec()) }); }
        worker.join().unwrap();
    }
    // Preserve the old decoded-byte contract when a server ignores our
    // identity preference: gzip framing can exceed a tiny valid range.
    for body in [b"data".as_slice(),b"extra",b"bad"] {
        let server=TcpListener::bind("127.0.0.1:0").unwrap();
        let c=client(format!("http://{}",server.local_addr().unwrap()));
        let mut gzip=flate2::write::GzEncoder::new(Vec::new(),flate2::Compression::fast());
        gzip.write_all(body).unwrap(); let payload=gzip.finish().unwrap();
        let worker=std::thread::spawn(move || {
            let (mut socket,_)=server.accept().unwrap();
            socket.set_read_timeout(Some(std::time::Duration::from_secs(2))).unwrap();
            read_headers(&mut BufReader::new(socket.try_clone().unwrap()));
            write!(socket,"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 100-103/1000\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",payload.len()).unwrap();
            socket.write_all(&payload).unwrap();
        });
        let result=c.read_range(&c.stream_request("fixture",&StreamOptions::default()),100,4);
        if body==b"data" {assert_eq!(result.unwrap(),Some(body.to_vec()));} else {assert!(result.is_err());}
        worker.join().unwrap();
    }
}

#[test]
fn stream_error_document() {
    let (url, _seen) = serve_once(
        "200 OK",
        "application/json",
        br#"{"subsonic-response":{"status":"failed","error":{"code":70,"message":"not found"}}}"#,
    );
    let c = client(url);
    let err = c.open(&c.stream_request("x", &StreamOptions::default()), 0).err().unwrap();
    assert!(matches!(err, Error::Api { code: 70, .. }));
}

#[test]
fn unreachable_is_offline() {
    let c = client("http://127.0.0.1:1".into());
    assert!(c.fetch(&calls::ping().request).unwrap_err().is_offline());
}

#[test]
fn radio_station_api_shapes() {
    let stations=calls::internet_radio_stations().parse(&serde_json::json!({"internetRadioStations":{"internetRadioStation":[{"id":"1","name":"Test Radio","streamUrl":"https://example.test/live.mp3"}]}})).unwrap();
    assert_eq!(stations.internet_radio_station.len(),1);
    assert_eq!(stations.internet_radio_station[0].name,"Test Radio");
    let call=calls::create_radio_station("A & B","https://example.test/live?a=1&b=2");
    assert_eq!(call.request.endpoint,"createInternetRadioStation");
    assert_eq!(call.request.params[0],("name","A & B".into()));
    assert_eq!(call.request.params[1],("streamUrl","https://example.test/live?a=1&b=2".into()));
}

#[test]
fn async_art_and_document_cancellation_closes_stalled_bodies_without_blocking_runtime() {
    use std::io::Read;
    use std::time::Duration;
    for mode in 0..4 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (entered, seen) = tokio::sync::oneshot::channel();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            read_headers(&mut BufReader::new(socket.try_clone().unwrap()));
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 100\r\nConnection: close\r\n\r\nx").unwrap();
            entered.send(()).unwrap();
            match socket.read(&mut [0]) {
                Ok(0) => {},
                Err(error) if matches!(error.kind(),std::io::ErrorKind::ConnectionReset|std::io::ErrorKind::ConnectionAborted) => {},
                result => panic!("cancelled body retained its connection: {result:?}"),
            }
        });
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().max_blocking_threads(1).build().unwrap();
        runtime.block_on(async move {
            let client = client(base.clone());
            let request = client.cover_art_request("generated", Some(50));
            let pending = tokio::spawn(async move {
                match mode {
                    0 => client.fetch_bytes_async(&request, 1024).await.map(|_| ()),
                    1 => client.cover_art_async("generated", 50).await.map(|_| ()),
                    2 => client.fetch_image_url_async(&base, 1024).await.map(|_| ()),
                    _ => client.fetch_async(&request).await.map(|_| ()),
                }
            });
            tokio::time::timeout(Duration::from_secs(2), seen).await.unwrap().unwrap();
            tokio::time::timeout(Duration::from_secs(1), async {
                tokio::time::sleep(Duration::from_millis(10)).await;
                assert_eq!(tokio::task::spawn_blocking(|| 42).await.unwrap(), 42);
            }).await.expect("body read must not occupy the executor or its blocking pool");
            pending.abort();
            assert!(pending.await.unwrap_err().is_cancelled());
        });
        server.join().unwrap();
    }
}

#[test]
fn async_art_limits_apply_after_gzip_expansion_and_to_undeclared_bodies() {
    use std::time::Duration;
    for (mode, gzip) in [(0, true), (1, true), (2, true), (0, false), (2, false)] {
        let limit = if mode == 1 {4*1024*1024} else {32};
        let expanded = vec![b'x'; limit+1];
        let payload = if gzip {
            let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            encoder.write_all(&expanded).unwrap();encoder.finish().unwrap()
        } else {expanded};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
            read_headers(&mut BufReader::new(socket.try_clone().unwrap()));
            let encoding = if gzip {"Content-Encoding: gzip\r\n"} else {""};
            write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\n{encoding}Connection: close\r\n\r\n").unwrap();
            let _ = socket.write_all(&payload);
        });
        let client = client(base.clone());
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let result = runtime.block_on(async {
            match mode {
                0 => client.fetch_bytes_async(&client.cover_art_request("generated", None),limit).await.map(|_| ()),
                1 => client.cover_art_async("generated",50).await.map(|_| ()),
                _ => client.fetch_image_url_async(&base,limit).await.map(|_| ()),
            }
        });
        assert!(matches!(result, Err(Error::Json(_))), "decoded body limit was not enforced: {result:?}");
        server.join().unwrap();
    }
}
