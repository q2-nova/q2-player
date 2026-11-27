use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn serve(listener: TcpListener, responses: Vec<(String, Vec<u8>)>) {
    for (path, response) in responses {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            request.push(socket.read_u8().await.unwrap());
        }
        let request = String::from_utf8(request).unwrap().to_lowercase();
        assert!(request.starts_with(&format!("get {path} http/1.1\r\n")));
        assert!(request.contains("icy-metadata: 1"));
        assert!(!request.contains("authorization:"));
        socket.write_all(&response).await.unwrap();
    }
}
fn reply(body: &[u8], headers: &str) -> Vec<u8> {
    let mut reply = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
        body.len()
    )
    .into_bytes();
    reply.extend_from_slice(body);
    reply
}
async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(3), future)
        .await
        .expect("generated radio transport deadline")
}

#[tokio::test]
async fn redirects_resolve_relative_playlist_and_preserve_raw_icy_headers() {
    bounded(async {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/start.m3u", listener.local_addr().unwrap());
        let server = tokio::spawn(serve(listener, vec![
            ("/start.m3u".into(), b"HTTP/1.1 302 Found\r\nLocation: /nested/station.m3u\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec()),
            ("/nested/station.m3u".into(), reply(b"#EXTM3U\nlive.mp3", "Content-Type: audio/x-mpegurl\r\n")),
            ("/nested/live.mp3".into(), reply(b"abc\x00def", "icy-br: 128, 256\r\nicy-metaint: 3\r\n")),
        ]));
        let stream = open_async(&url, None).await.unwrap();
        assert_eq!(stream.bitrate_kbps, Some(128));
        assert_eq!(stream.metadata_interval, Some(3));
        let mut body = stream.into_decoded();
        let mut bytes = Vec::new();
        while let Some(chunk) = body.chunk().await { bytes.extend(chunk.unwrap()); }
        assert_eq!(bytes, b"abc\x00def");
        server.await.unwrap();
    }).await;
}

#[tokio::test]
async fn playlist_exact_limit_accepted_next_byte_rejected() {
    bounded(async {
        for length in [65536, 65537] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/station.m3u", listener.local_addr().unwrap());
            let mut playlist = vec![b'#'];
            playlist.resize(length - 6, b'x');
            playlist.extend_from_slice(b"\n/live");
            assert_eq!(playlist.len(), length);
            let mut responses = vec![("/station.m3u".into(), reply(&playlist, ""))];
            if length == 65536 {
                responses.push(("/live".into(), reply(b"audio", "")));
            }
            let server = tokio::spawn(serve(listener, responses));
            let result = open_async(&url, None).await;
            if length == 65536 {
                let mut body = result.unwrap().into_decoded();
                assert_eq!(body.chunk().await.unwrap().unwrap(), b"audio");
                assert!(body.chunk().await.is_none());
            } else {
                assert_eq!(
                    result.err().unwrap().to_string(),
                    "Radio playlist is too large"
                );
            }
            server.await.unwrap();
        }
    })
    .await;
}

#[tokio::test]
async fn playlist_hls_and_four_hop_exhaustion_remain_explicit_errors() {
    bounded(async {
        for hls in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/station.m3u", listener.local_addr().unwrap());
            let body = if hls {
                b"#EXT-X-TARGETDURATION:10\na.ts".as_slice()
            } else {
                b"/station.m3u".as_slice()
            };
            let server = tokio::spawn(serve(
                listener,
                (0..if hls { 1 } else { 4 })
                    .map(|_| ("/station.m3u".into(), reply(body, "")))
                    .collect(),
            ));
            let error = open_async(&url, None).await.err().unwrap().to_string();
            assert_eq!(
                error,
                if hls {
                    "HLS radio is not supported; choose an MP3 or AAC stream"
                } else {
                    "Radio playlist nesting is too deep"
                }
            );
            server.await.unwrap();
        }
    })
    .await;
}
