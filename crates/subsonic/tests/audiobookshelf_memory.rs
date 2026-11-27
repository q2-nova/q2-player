//! Heap bound of an Audiobookshelf JSON body read whole, measured with dhat
//! (its own test binary: dhat replaces the global allocator).
//!
//! Finding 23 of the code review, round 2: a body is read up to 8 MiB and
//! was parsed into a `serde_json::Value` whatever it held. `[0,0,...]` at
//! that size is 4.2 million values.
use std::io::{Read, Write};
use std::net::TcpListener;

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

#[test]
fn a_body_of_millions_of_values_is_refused_within_its_own_size() {
    let mut body = Vec::with_capacity(8 * 1024 * 1024);
    body.push(b'[');
    while body.len() < 8 * 1024 * 1024 - 4 { body.extend_from_slice(b"0,"); }
    body.extend_from_slice(b"0]");
    let size = body.len();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/abs", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        while !request.ends_with(b"\r\n\r\n") { socket.read_exact(&mut byte).unwrap(); request.push(byte[0]); }
        let head = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
        socket.write_all(head.as_bytes()).unwrap();
        socket.write_all(&body).unwrap();
    });
    let client = subsonic::audiobookshelf::Client::new(&base, "generated-fixture-token", None).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    let profiler = dhat::Profiler::builder().testing().build();
    let result = runtime.block_on(client.get_async("api/libraries"));
    let peak = dhat::HeapStats::get().max_bytes;
    drop(profiler);
    server.join().unwrap();
    eprintln!("body {size} bytes; peak {peak} bytes; refused: {}", result.is_err());
    assert!(matches!(result, Err(subsonic::audiobookshelf::Error::Protocol)), "the document is refused");
    // The body itself, the read's buffers and the client: not its values.
    assert!(peak < 3 * size, "peak {peak} bytes for a {size}-byte body");
}
