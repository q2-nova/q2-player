//! Public radio transport: never attach Subsonic credentials to station URLs.
pub mod france;
pub mod kcrw;
use std::io::{self, Read};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn fail(message: &str) -> io::Error {
    io::Error::other(message)
}
fn http(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

/// Resolve small M3U/PLS playlists, then return an unbounded live transport reader.
/// HLS requires a segment player and is deliberately rejected.
#[cfg(test)]
pub struct Stream {
    /// Raw transport bytes, including ICY blocks. Buffer before `audio_reader`
    /// so metadata follows consumption rather than network read-ahead.
    pub reader: Box<dyn Read + Send>,
    pub bitrate_kbps: Option<u32>,
    pub metadata_interval: Option<usize>,
}

/// Latest title for one connection only; never persisted as station metadata.
pub type Metadata = Arc<Mutex<Option<String>>>;

pub fn audio_reader(reader: Box<dyn Read + Send>, interval: Option<usize>, title: Metadata) -> Box<dyn Read + Send> {
    match interval.filter(|n| *n > 0) {
        Some(n) => Box::new(Icy { reader, interval: n, left: n, title }),
        None => reader,
    }
}

/// Async transport owns its HTTP response until the radio producer begins
/// bounded decoded reads. ICY bytes still remain in consumption order.
pub struct AsyncStream {
    response: crate::http::Response,
    pub bitrate_kbps: Option<u32>,
    pub metadata_interval: Option<usize>,
}
impl AsyncStream {
    pub fn into_decoded(self) -> crate::http::DecodedBody {
        self.response.into_decoded()
    }
}
/// Compatibility entry point; request/playlist handling has one async core.
#[cfg(test)]
pub fn open(url: &str, proxy: Option<&str>) -> io::Result<Stream> {
    let url = url.to_owned();
    let proxy = proxy.map(str::to_owned);
    let stream = crate::http::block_on(async move { open_async(&url, proxy.as_deref()).await })
        .ok_or_else(|| fail("Radio stream disconnected"))??;
    Ok(Stream {
        reader: stream.response.into_reader(),
        bitrate_kbps: stream.bitrate_kbps,
        metadata_interval: stream.metadata_interval,
    })
}

pub async fn open_async(url: &str, proxy: Option<&str>) -> io::Result<AsyncStream> {
    let mut b = crate::http::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(5))
        .timeout_read(Duration::from_secs(15));
    if let Some(p) = proxy {
        b = b.proxy(crate::http::Proxy::new(p).map_err(|_| fail("Invalid radio proxy"))?);
    }
    let agent = b.build();
    let mut url = url.to_string();
    for _ in 0..4 {
        if !http(&url) {
            return Err(fail("Radio requires an HTTP or HTTPS stream"));
        }
        let response = agent
            .get(&url)
            .set("Icy-MetaData", "1")
            .send(hyper::body::Bytes::new())
            .await
            .map_err(|error| match error {
                crate::http::Error::Status(code, _) => {
                    fail(&format!("Radio server returned HTTP {code}"))
                }
                crate::http::Error::Transport(error) => {
                    fail(&format!("Radio connection failed ({:?})", error.kind()))
                }
            })?;
        let mime = response
            .header("Content-Type")
            .unwrap_or("")
            .to_ascii_lowercase();
        let path = url.split('?').next().unwrap_or(&url).to_ascii_lowercase();
        let playlist = path.ends_with(".pls")
            || path.ends_with(".m3u")
            || path.ends_with(".m3u8")
            || mime.contains("mpegurl")
            || mime.contains("scpls");
        if playlist {
            let base = response.get_url().to_string();
            let mut body = response.into_decoded();
            let mut bytes = Vec::new();
            while let Some(chunk) = body.chunk().await {
                let chunk = chunk.map_err(|_| fail("Could not read radio playlist"))?;
                if chunk.len() > 65536usize.saturating_sub(bytes.len()) {
                    return Err(fail("Radio playlist is too large"));
                }
                bytes.extend_from_slice(&chunk);
            }
            url = tokio::task::spawn_blocking(move || {
                playlist_url(&String::from_utf8_lossy(&bytes), &base)
            })
            .await
            .map_err(|_| fail("Could not read radio playlist"))??;
            continue;
        }
        let interval = response
            .header("icy-metaint")
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|n| *n > 0);
        let bitrate_kbps = response
            .header("icy-br")
            .and_then(|s| s.split(',').next())
            .and_then(|s| s.trim().parse::<u32>().ok())
            .filter(|n| *n > 0);
        return Ok(AsyncStream {
            response,
            bitrate_kbps,
            metadata_interval: interval,
        });
    }
    Err(fail("Radio playlist nesting is too deep"))
}

fn playlist_url(text: &str, base: &str) -> io::Result<String> {
    if text.lines().any(|l| l.trim().starts_with("#EXT-X-")) {
        return Err(fail(
            "HLS radio is not supported; choose an MP3 or AAC stream",
        ));
    }
    let pls = text
        .lines()
        .any(|l| l.trim().eq_ignore_ascii_case("[playlist]"));
    let line = text
        .lines()
        .map(str::trim)
        .find_map(|l| {
            if pls {
                let (k, v) = l.split_once('=')?;
                k.eq_ignore_ascii_case("file1").then_some(v.trim())
            } else {
                (!l.is_empty() && !l.starts_with('#')).then_some(l)
            }
        })
        .ok_or_else(|| fail("Empty radio playlist"))?;
    let base = url::Url::parse(base).map_err(|_| fail("Invalid radio URL"))?;
    let next = base
        .join(line)
        .map_err(|_| fail("Invalid playlist URL"))?
        .to_string();
    if !http(&next) {
        return Err(fail("Unsupported playlist URL scheme"));
    }
    Ok(next)
}
struct Icy {
    reader: Box<dyn Read + Send>,
    interval: usize,
    left: usize,
    title: Metadata,
}

/// Missing or malformed fields leave the previous title alone; an explicit
/// empty StreamTitle clears it. Accept UTF-8 and legacy Latin-1 station text.
fn stream_title(bytes: &[u8]) -> Option<String> {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    let bytes = &bytes[..end];
    let text = std::str::from_utf8(bytes).map(str::to_owned)
        .unwrap_or_else(|_| bytes.iter().map(|b| char::from(*b)).collect());
    let mut fields = text.as_str();
    while let Some((key, rest)) = fields.split_once('=') {
        let rest = rest.trim_start();
        let quote = rest.chars().next()?;
        if quote != '\'' && quote != '"' { return None; }
        let rest = &rest[1..];
        let delimiter = format!("{quote};");
        let end = rest.find(&delimiter).or_else(|| rest.strip_suffix(quote).map(str::len))?;
        if key.trim().eq_ignore_ascii_case("StreamTitle") {
            let cleaned: String = rest[..end].chars()
                .map(|c| if c.is_control() { ' ' } else { c }).take(512).collect();
            return Some(cleaned.split_whitespace().collect::<Vec<_>>().join(" "));
        }
        fields = rest.get(end + 2..).unwrap_or("");
    }
    None
}
impl Read for Icy {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        if self.left == 0 {
            let mut length = [0];
            self.reader.read_exact(&mut length)?;
            let mut metadata = [0; 4080];
            self.reader
                .read_exact(&mut metadata[..length[0] as usize * 16])?;
            if let Some(title) = stream_title(&metadata[..length[0] as usize * 16]) {
                *self.title.lock().unwrap() = (!title.is_empty()).then_some(title);
            }
            self.left = self.interval;
        }
        let len = buf.len().min(self.left);
        let n = self.reader.read(&mut buf[..len])?;
        self.left -= n;
        Ok(n)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn block(text: &[u8]) -> Vec<u8> {
        let len = text.len().div_ceil(16);
        let mut out = vec![len as u8];
        out.extend_from_slice(text);
        out.resize(1 + len * 16, 0);
        out
    }
    #[test]
    fn titles_update_on_consumption_and_empty_blocks_preserve_them() {
        let mut data = b"abc".to_vec();
        for metadata in [b"StreamTitle='Artist - First';".as_slice(), b"", b"StreamTitle='Second';", b"StreamTitle='';"] {
            data.extend(block(metadata));
            data.extend(b"abc");
        }
        let title = Metadata::default();
        let mut reader = audio_reader(Box::new(io::Cursor::new(data)), Some(3), title.clone());
        for expected in [None, Some("Artist - First"), Some("Artist - First"), Some("Second"), None] {
            let mut audio = [0; 3];
            reader.read_exact(&mut audio).unwrap();
            assert_eq!(&audio, b"abc");
            assert_eq!(title.lock().unwrap().as_deref(), expected);
        }
    }
    /// Delivers its bytes in the given piece sizes, as a socket does.
    struct Pieces(io::Cursor<Vec<u8>>, Vec<usize>, usize);
    impl Read for Pieces {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let piece = self.1[self.2 % self.1.len()].max(1);
            self.2 += 1;
            let len = out.len().min(piece);
            self.0.read(&mut out[..len])
        }
    }
    fn drain(reader: &mut dyn Read, step: usize) -> (Vec<u8>, bool) {
        let (mut audio, mut buffer) = (Vec::new(), vec![0; step.max(1)]);
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => return (audio, true),
                Ok(n) => audio.extend_from_slice(&buffer[..n]),
                Err(_) => return (audio, false),
            }
        }
    }
    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(256))]
        /// Station text is arbitrary bytes: any block yields no title or one
        /// of at most 512 characters without control characters.
        #[test]
        fn any_metadata_block_yields_no_title_or_a_bounded_clean_one(
            bytes in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..4081),
            prefix in proptest::sample::select(vec![&b""[..], b"StreamTitle='", b"StreamUrl='x';StreamTitle=\"", b"streamtitle = '", b"=';=\";StreamTitle='"]),
        ) {
            let block = [prefix, &bytes[..]].concat();
            if let Some(title) = stream_title(&block) {
                proptest::prop_assert!(title.chars().count() <= 512);
                proptest::prop_assert!(!title.chars().any(char::is_control));
                proptest::prop_assert!(!title.starts_with(' ') && !title.ends_with(' '));
            }
        }

        /// Whatever the interval, the block lengths, the block contents and
        /// the way the socket splits the bytes, the decoder receives exactly
        /// the audio bytes in order, and a stream cut inside a block is an
        /// error and never audio.
        #[test]
        fn interleaved_metadata_never_reaches_the_audio_however_the_stream_is_split(
            interval in 1usize..700,
            audio in proptest::collection::vec(proptest::prelude::any::<u8>(), 0..3000),
            blocks in proptest::collection::vec((0u8..=255, proptest::prelude::any::<u8>()), 0..8),
            pieces in proptest::collection::vec(1usize..600, 1..6),
            step in 1usize..900,
            cut in proptest::prelude::any::<proptest::sample::Index>(),
        ) {
            let mut wire = Vec::new();
            for (index, chunk) in audio.chunks(interval).enumerate() {
                wire.extend_from_slice(chunk);
                if chunk.len() == interval {
                    let (length, fill) = blocks.get(index % blocks.len().max(1)).copied().unwrap_or((0, 0));
                    wire.push(length);
                    wire.extend(std::iter::repeat_n(fill, length as usize * 16));
                }
            }
            let title = Metadata::default();
            let mut reader = audio_reader(Box::new(Pieces(io::Cursor::new(wire.clone()), pieces.clone(), 0)), Some(interval), title.clone());
            let (heard, clean) = drain(&mut reader, step);
            proptest::prop_assert!(clean);
            proptest::prop_assert_eq!(&heard, &audio);
            proptest::prop_assert!(title.lock().unwrap().as_ref().is_none_or(|title| title.chars().count() <= 512));
            // Cut anywhere: a prefix of the audio, never a metadata byte.
            let cut = cut.index(wire.len() + 1);
            let mut reader = audio_reader(Box::new(Pieces(io::Cursor::new(wire[..cut].to_vec()), pieces, 0)), Some(interval), Metadata::default());
            let (heard, _) = drain(&mut reader, step);
            proptest::prop_assert!(audio.starts_with(&heard), "{} bytes heard of a stream cut at {cut}", heard.len());
        }

        /// A playlist body is whatever a station's server sends: the result
        /// is an error or an http(s) URL.
        #[test]
        fn any_playlist_text_resolves_to_an_http_url_or_an_error(
            lines in proptest::collection::vec(proptest::sample::select(vec!["", "#EXTM3U", "#EXTINF:-1,Station", "[playlist]", "[PLAYLIST]", "File1=http://radio.example/live", "file1 = stream.mp3",
                "NumberOfEntries=1", "http://radio.example/a.mp3", "https://radio.example/b", "//other.example/c", "/absolute", "relative.aac", "ftp://radio.example/x", "file:///etc/passwd",
                "javascript:alert(1)", "#EXT-X-TARGETDURATION:6", "File1=", "=", "\u{feff}http://bom.example/", "http://[::1", "\0", "caf\u{e9}://x", "data:audio/mpeg;base64,AAAA"]), 0..8),
            junk in ".{0,40}",
            crlf in proptest::prelude::any::<bool>(),
        ) {
            let mut lines: Vec<String> = lines.into_iter().map(str::to_owned).collect();
            lines.push(junk);
            let text = lines.join(if crlf { "\r\n" } else { "\n" });
            for base in ["http://radio.example/dir/list.m3u", "https://radio.example", "not a url"] {
                if let Ok(next) = playlist_url(&text, base) {
                    proptest::prop_assert!(http(&next), "{next}");
                    proptest::prop_assert!(url::Url::parse(&next).is_ok());
                }
            }
        }
    }

    #[test]
    fn metadata_text_handles_unicode_apostrophes_and_malformed_fields() {
        assert_eq!(stream_title("StreamTitle='Björk - It's Oh So Quiet';StreamUrl='ignored';".as_bytes()).as_deref(), Some("Björk - It's Oh So Quiet"));
        assert_eq!(stream_title(b"StreamUrl='ignored';StreamTitle='Caf\xe9';").as_deref(), Some("Café"));
        assert_eq!(stream_title(b"StreamTitle='A\nB\tC';").as_deref(), Some("A B C"));
        assert_eq!(stream_title(b"StreamTitle='unfinished"), None);
        assert_eq!(stream_title(b"StreamUrl='ignored';"), None);
        assert_eq!(stream_title(format!("StreamTitle='{}';", "x".repeat(4000)).as_bytes()).unwrap().len(), 512);
    }
    #[test]
    fn fragmented_reads_strip_metadata_and_truncation_is_an_error() {
        struct Fragmented(io::Cursor<Vec<u8>>);
        impl Read for Fragmented {
            fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
                let len = out.len().min(1);
                self.0.read(&mut out[..len])
            }
        }
        let mut data = b"abc".to_vec();
        data.extend(block(b"StreamTitle='Test';"));
        data.extend(b"def\x02short");
        let title = Metadata::default();
        let mut reader = audio_reader(Box::new(Fragmented(io::Cursor::new(data))), Some(3), title.clone());
        let mut audio = [0; 6];
        reader.read_exact(&mut audio).unwrap();
        assert_eq!(&audio, b"abcdef");
        assert_eq!(title.lock().unwrap().as_deref(), Some("Test"));
        assert_eq!(reader.read(&mut audio).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(title.lock().unwrap().as_deref(), Some("Test"));
    }
    #[test]
    fn playlist_formats_and_limits() {
        assert_eq!(
            playlist_url(
                "[playlist]\nFile1=https://a.example/live\nFile2=https://b.example",
                "https://x.example/list.pls"
            )
            .unwrap(),
            "https://a.example/live"
        );
        assert_eq!(
            playlist_url("#EXTM3U\n../live.mp3", "https://x.example/lists/a.m3u").unwrap(),
            "https://x.example/live.mp3"
        );
        assert!(
            playlist_url(
                "#EXTM3U\n#EXT-X-TARGETDURATION:10\na.ts",
                "https://x.example/"
            )
            .is_err()
        );
        assert!(playlist_url("file:///etc/passwd", "https://x.example/").is_err());
    }
    #[test]
    fn icy_metadata_never_reaches_decoder() {
        let mut data = b"abc\x01".to_vec();
        data.extend_from_slice(&[b'x'; 16]);
        data.extend_from_slice(b"def\x00ghi");
        let mut r = Icy {
            reader: Box::new(io::Cursor::new(data)),
            interval: 3,
            left: 3,
            title: Metadata::default(),
        };
        let mut out = [0; 9];
        r.read_exact(&mut out).unwrap();
        assert_eq!(&out, b"abcdefghi");
    }
}

#[cfg(test)]
mod http_tests {
    use super::*;
    use std::io::Write;
    #[test]
    fn socks_radio_uses_remote_dns_and_origin_form_through_playlists() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy = format!("socks5://{}", listener.local_addr().unwrap());
        let worker = std::thread::spawn(move || {
            for (path, body) in [("/station.pls", "[playlist]\nFile1=/live.mp3"), ("/live.mp3", "audio bytes")] {
                let (mut socket, _) = listener.accept().unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
                let mut greeting = [0; 3];
                socket.read_exact(&mut greeting).unwrap();
                assert_eq!(greeting, [5, 1, 0]);
                socket.write_all(&[5, 0]).unwrap();
                let mut connect = [0; 5];
                socket.read_exact(&mut connect).unwrap();
                assert_eq!(&connect[..4], &[5, 1, 0, 3]); // domain, resolved by proxy
                let mut host = vec![0; connect[4] as usize];
                socket.read_exact(&mut host).unwrap();
                assert_eq!(host, b"station.invalid");
                let mut port = [0; 2];
                socket.read_exact(&mut port).unwrap();
                assert_eq!(u16::from_be_bytes(port), 80);
                socket.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 80]).unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    socket.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap();
                assert!(request.starts_with(&format!("GET {path} HTTP/1.1\r\n")), "{request}");
                assert!(!request.to_lowercase().contains("authorization:"));
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
        });
        let mut body = String::new();
        open("http://station.invalid/station.pls", Some(&proxy)).unwrap().reader.read_to_string(&mut body).unwrap();
        assert_eq!(body, "audio bytes");
        worker.join().unwrap();
    }

    #[test]
    fn radio_http_failure_keeps_status_without_private_url() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/live?token=private-token", listener.local_addr().unwrap());
        let worker = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0]; socket.read_exact(&mut byte).unwrap(); request.push(byte[0]);
            }
            socket.write_all(b"HTTP/1.1 503 Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
        });
        let error = open(&url, None).err().unwrap().to_string();
        assert_eq!(error, "Radio server returned HTTP 503");
        worker.join().unwrap();
    }

    #[test]
    fn unavailable_socks_proxy_does_not_connect_directly() {
        let origin = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        origin.set_nonblocking(true).unwrap();
        let proxy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = proxy.local_addr().unwrap();
        drop(proxy);
        assert!(open(&format!("http://{}/live", origin.local_addr().unwrap()), Some(&format!("socks5://{address}"))).is_err());
        assert_eq!(origin.accept().unwrap_err().kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn resolves_http_playlist_without_api_auth() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/station.pls", listener.local_addr().unwrap());
        let worker = std::thread::spawn(move || {
            for body in ["[playlist]\nFile1=/live.mp3", "audio bytes"] {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut request = Vec::new();
                loop {
                    let mut b = [0];
                    socket.read_exact(&mut b).unwrap();
                    request.push(b[0]);
                    if request.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let text = String::from_utf8(request).unwrap().to_lowercase();
                assert!(!text.contains("authorization:"));
                assert!(text.contains("icy-metadata: 1"));
                assert!(!text.contains("?u="));
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .unwrap();
            }
        });
        let mut r = open(&url, None).unwrap().reader;
        let mut body = String::new();
        r.read_to_string(&mut body).unwrap();
        assert_eq!(body, "audio bytes");
        worker.join().unwrap();
    }
}

#[cfg(test)]
#[path = "radio_async_tests.rs"]
mod async_tests;
