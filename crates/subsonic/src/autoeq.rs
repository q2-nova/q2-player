//! Public AutoEQ downloads, entirely separate from Navidrome authentication.
use std::time::Duration;

pub async fn fetch_async(url: &str, proxy: Option<&str>) -> Result<String, String> {
    if !url.starts_with("https://raw.githubusercontent.com/jaakkopasanen/AutoEq/") {
        return Err("Invalid AutoEQ URL".into());
    }
    let mut builder = crate::http::AgentBuilder::new().timeout_connect(Duration::from_secs(5)).timeout(Duration::from_secs(15));
    if let Some(proxy) = proxy { builder = builder.proxy(crate::http::Proxy::new(proxy).map_err(|_|"Invalid AutoEQ proxy")?); }
    let response = builder.build().get(url).send(Vec::new().into()).await.map_err(|e| match e {
        crate::http::Error::Status(code,_) => format!("AutoEQ download: HTTP {code}"),
        _ => "AutoEQ unavailable; check connection".into(),
    })?;
    text(response).await
}

async fn text(response: crate::http::Response) -> Result<String, String> {
    let mut body = response.into_decoded();
    let mut bytes = Vec::new();
    while let Some(chunk) = body.chunk().await {
        let chunk = chunk.map_err(|_| "AutoEQ download interrupted")?;
        if chunk.len() > 16384usize.saturating_sub(bytes.len()) {
            return Err("AutoEQ profile too large".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    String::from_utf8(bytes).map_err(|_|"Invalid AutoEQ text".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    #[test]
    fn async_body_keeps_decoded_limit_and_text_errors() {
        for (bytes, gzip, expected) in [
            (vec![b'x'; 16384], false, None),
            (vec![b'x'; 16385], false, Some("AutoEQ profile too large")),
            (vec![b'x'; 16385], true, Some("AutoEQ profile too large")),
            (vec![0xff], false, Some("Invalid AutoEQ text")),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/generated", listener.local_addr().unwrap());
            let server = std::thread::spawn(move || {
                listener.set_nonblocking(true).unwrap();
                let deadline=std::time::Instant::now()+Duration::from_secs(5);
                let mut socket=loop {
                    match listener.accept() {
                        Ok((socket,_)) => break socket,
                        Err(error) if error.kind()==std::io::ErrorKind::WouldBlock && std::time::Instant::now()<deadline => std::thread::sleep(Duration::from_millis(1)),
                        Err(error) => panic!("generated AutoEQ accept: {error}"),
                    }
                };
                socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                // Read the whole request head: a request split across reads and left
                // unread at close makes the kernel reset the connection, which failed
                // this test under load (2026-11-22, V0.135 build-test).
                let mut request = Vec::new(); let mut chunk = [0; 1024];
                while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = socket.read(&mut chunk).unwrap();
                    if n == 0 { break; }
                    request.extend_from_slice(&chunk[..n]);
                }
                let wire = if gzip {
                    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
                    encoder.write_all(&bytes).unwrap(); encoder.finish().unwrap()
                } else { bytes };
                let encoding = if gzip { "Content-Encoding: gzip\r\n" } else { "" };
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n{encoding}Connection: close\r\n\r\n", wire.len()).unwrap();
                let _ = socket.write_all(&wire);
            });
            let result = crate::http::block_on(async move {
                let response = crate::http::AgentBuilder::new().build().get(&url).send(Vec::new().into()).await.unwrap();
                text(response).await
            }).unwrap();
            if let Some(error) = expected { assert_eq!(result.unwrap_err(), error); }
            else { assert_eq!(result.unwrap().len(), 16384); }
            server.join().unwrap();
        }
    }

    #[test]
    fn invalid_public_endpoint_is_refused_before_transport() {
        assert_eq!(crate::http::block_on(async { fetch_async("http://127.0.0.1/private", None).await }).unwrap().unwrap_err(), "Invalid AutoEQ URL");
    }
}
