use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn bounded<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(3), future)
        .await
        .expect("generated radio fixture deadline")
}
async fn request(listener: &TcpListener) -> TcpStream {
    let (mut socket, _) = bounded(listener.accept()).await.unwrap();
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        bytes.push(bounded(socket.read_u8()).await.unwrap());
        assert!(bytes.len() < 8192);
    }
    let text = String::from_utf8(bytes).unwrap().to_lowercase();
    assert!(text.contains("icy-metadata: 1"));
    assert!(!text.contains("authorization:"));
    socket
}
async fn until(mut predicate: impl FnMut() -> bool) {
    bounded(async {
        while !predicate() {
            tokio::task::yield_now().await;
        }
    })
    .await;
}
fn empty() -> Buffered {
    Buffered::empty(Arc::new(Interrupt::new(false)))
}

#[tokio::test]
async fn cancelled_header_playlist_and_live_body_close_socket_and_both_owners() {
    for phase in 0..3 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let path = if phase == 1 { "station.m3u" } else { "live" };
        let url = format!("http://{}/{path}", listener.local_addr().unwrap());
        let owner = empty();
        let shared = owner.shared.clone();
        let producer = tokio::spawn(producer(
            shared.clone(),
            url,
            None,
            Default::default(),
            None,
        ));
        let metadata = tokio::spawn({
            let shared = shared.clone();
            async move { metadata_stopped(&shared).await }
        });
        let mut socket = request(&listener).await;
        if phase != 0 {
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
        }
        if phase == 2 {
            until(|| shared.buffer.lock().unwrap().ready).await;
        }
        // A retained notification is required even if either waiter has not
        // yet registered. Header/playlist/body stalls all own real sockets.
        drop(owner);
        bounded(producer).await.unwrap();
        bounded(metadata).await.unwrap();
        assert!(shared.buffer.lock().unwrap().done);
        assert_eq!(
            bounded(socket.read_u8()).await.unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
}

#[tokio::test]
async fn full_buffer_stops_publication_then_resumes_exact_bytes_and_eof() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/live", listener.local_addr().unwrap());
    let expected: Vec<u8> = (0..200_000).map(|n| (n % 251) as u8).collect();
    let payload = expected.clone();
    let server = tokio::spawn(async move {
        let mut socket = request(&listener).await;
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nicy-br: 1\r\nicy-metaint: 17\r\nConnection: close\r\n\r\n", payload.len()).as_bytes()).await.unwrap();
        socket.write_all(&payload).await.unwrap();
    });
    // Exercise the actual runtime entry point. Decoder-facing reads stay
    // synchronous, but this test only reads after bytes/done are observed.
    let mut owner = Buffered::open(
        url,
        None,
        Arc::new(Interrupt::new(false)),
        Default::default(),
        None,
    );
    let shared = owner.shared.clone();
    until(|| shared.buffer.lock().unwrap().bytes.len() == 64 * 1024).await;
    for _ in 0..20 {
        tokio::task::yield_now().await;
    }
    {
        let buffer = shared.buffer.lock().unwrap();
        assert_eq!(buffer.bytes.len(), buffer.capacity);
        assert_eq!(buffer.metadata_interval, Some(17));
        assert!(!buffer.done);
    }
    let mut received = Vec::new();
    loop {
        until(|| {
            let b = shared.buffer.lock().unwrap();
            !b.bytes.is_empty() || b.done
        })
        .await;
        let (returned, (bytes, count)) = crate::audio::callback::test_job(owner, |owner| {
            let mut bytes=[0;7001]; let count=owner.read(&mut bytes).unwrap(); (bytes,count)
        }).await; owner=returned;
        if count == 0 {
            break;
        }
        received.extend_from_slice(&bytes[..count]);
        assert!(shared.buffer.lock().unwrap().bytes.len() <= 64 * 1024);
    }
    assert_eq!(received, expected);
    bounded(server).await.unwrap();
}

#[tokio::test]
async fn cancellation_of_full_buffer_does_not_require_decoder_progress() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/live", listener.local_addr().unwrap());
    let owner = empty();
    let shared = owner.shared.clone();
    let task = tokio::spawn(producer(
        shared.clone(),
        url,
        None,
        Default::default(),
        None,
    ));
    let mut socket = request(&listener).await;
    socket
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Length: 200000\r\nicy-br: 1\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
    socket.write_all(&vec![42; 100000]).await.unwrap();
    until(|| shared.buffer.lock().unwrap().bytes.len() == 65536).await;
    drop(owner);
    bounded(task).await.unwrap();
    let buffer = shared.buffer.lock().unwrap();
    assert!(buffer.done && buffer.cancelled);
    assert_eq!(buffer.bytes.len(), 65536);
    drop(buffer);
    assert_eq!(
        bounded(socket.read_u8()).await.unwrap_err().kind(),
        io::ErrorKind::UnexpectedEof
    );
}

#[tokio::test]
async fn truncated_body_drains_published_audio_before_sanitized_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}/live?synthetic-secret",
        listener.local_addr().unwrap()
    );
    let owner = empty();
    let shared = owner.shared.clone();
    let task = tokio::spawn(producer(
        shared.clone(),
        url,
        None,
        Default::default(),
        None,
    ));
    let mut socket = request(&listener).await;
    socket
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\nConnection: close\r\n\r\nabcdefghi")
        .await
        .unwrap();
    until(|| shared.buffer.lock().unwrap().bytes.len() == 9).await;
    drop(socket);
    bounded(task).await.unwrap();
    crate::audio::callback::test_job(owner, |owner| {
        let mut bytes=[0;9]; owner.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes,b"abcdefghi");
        assert_eq!(owner.read(&mut bytes).unwrap_err().to_string(),"Radio stream disconnected");
    }).await;
}

#[tokio::test]
async fn unpolled_and_aborted_producers_complete_reader_lifetime() {
    let owner = empty();
    let future = producer(
        owner.shared.clone(),
        "http://generated.invalid/".into(),
        None,
        Default::default(),
        None,
    );
    drop(future);
    assert!(owner.shared.buffer.lock().unwrap().done);
    crate::audio::callback::test_job(owner, |owner| {
        assert!(owner.read(&mut [0]).is_err());
    }).await;

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let owner = empty();
    let task = tokio::spawn(producer(
        owner.shared.clone(),
        format!("http://{}/", listener.local_addr().unwrap()),
        None,
        Default::default(),
        None,
    ));
    let mut socket = request(&listener).await;
    task.abort();
    assert!(bounded(task).await.unwrap_err().is_cancelled());
    assert!(owner.shared.buffer.lock().unwrap().done);
    crate::audio::callback::test_job(owner, |owner| {
        assert!(owner.read(&mut [0]).is_err());
    }).await;
    assert_eq!(
        bounded(socket.read_u8()).await.unwrap_err().kind(),
        io::ErrorKind::UnexpectedEof
    );
}
