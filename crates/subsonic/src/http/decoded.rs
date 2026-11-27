//! Incremental decoded media. Empty input is WouldBlock until HTTP reaches EOF,
//! so a decoder job never waits for a socket or reads ahead of its caller.
use super::Body;
use hyper::body::Bytes;
use std::io::{self, BufRead, Read};

pub const CHUNK_BYTES: usize = 64 * 1024;

// Finite media inflation has independent capacity: queued document parsing,
// card work or DNS on a caller's general pool must not prevent an already
// buffered gzip stream from progressing.
// This runtime has no executor thread, and its sole worker starts on demand.
fn pool() -> &'static tokio::runtime::Runtime {
    static POOL: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    POOL.get_or_init(|| tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(1).thread_name("media-inflate")
        .thread_keep_alive(std::time::Duration::from_secs(10))
        .build().expect("media inflation pool"))
}

#[derive(Default)]
struct Input {
    bytes: Bytes,
    eof: bool,
}
impl BufRead for Input {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.bytes.is_empty() && !self.eof { Err(io::ErrorKind::WouldBlock.into()) }
        else { Ok(&self.bytes) }
    }
    fn consume(&mut self, count: usize) { self.bytes = self.bytes.slice(count..); }
}
impl Read for Input {
    fn read(&mut self, target: &mut [u8]) -> io::Result<usize> {
        if target.is_empty() { return Ok(0); }
        let source = self.fill_buf()?;
        let count = source.len().min(target.len());
        target[..count].copy_from_slice(&source[..count]);
        self.consume(count);
        Ok(count)
    }
}

type Decoder = flate2::bufread::MultiGzDecoder<Input>;
enum Step { Data(Vec<u8>), Input, End, Error(io::Error) }

fn inflate(mut decoder: Decoder) -> (Decoder, Step) {
    let mut output = vec![0; CHUNK_BYTES];
    let step = match decoder.read(&mut output) {
        Ok(0) => Step::End,
        Ok(count) => { output.truncate(count); Step::Data(output) },
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => Step::Input,
        Err(error) => Step::Error(error),
    };
    (decoder, step)
}

/// Owns one HTTP body and returns at most64KiB of decoded bytes per call. No
/// decompressor holds a pool worker while waiting for another network frame.
/// The in-flight decode handle is retained across cancellation of `chunk()`.
pub struct DecodedBody {
    body: Body,
    pending: Bytes,
    decoder: Option<Decoder>,
    inflating: Option<tokio::task::JoinHandle<(Decoder, Step)>>,
    ended: bool,
    wire_remaining: Option<usize>,
}
impl DecodedBody {
    pub(super) fn new(body: Body) -> Self {
        let decoder = body.gzip.then(|| Decoder::new(Input::default()));
        Self { body, pending: Bytes::new(), decoder, inflating: None, ended: false, wire_remaining: None }
    }

    /// Whole-body consumers retain their compressed-input cap as well as their
    /// decoded-output cap, without holding a second full compressed buffer.
    pub(super) fn with_wire_limit(mut self, limit: usize) -> Self {
        self.wire_remaining=Some(limit);
        self
    }

    async fn input(&mut self) -> Option<io::Result<Bytes>> {
        let chunk=self.body.chunk().await?;
        if let (Ok(bytes),Some(remaining))=(&chunk,&mut self.wire_remaining) {
            let Some(left)=remaining.checked_sub(bytes.len()) else {
                return Some(Err(io::Error::new(io::ErrorKind::InvalidData,"response too large")));
            };
            *remaining=left;
        }
        Some(chunk)
    }

    fn fail(&mut self, error: io::Error) -> Option<io::Result<Vec<u8>>> {
        self.ended = true;
        self.body.finish(false);
        Some(Err(error))
    }

    pub async fn chunk(&mut self) -> Option<io::Result<Vec<u8>>> {
        if self.ended { return None; }
        if !self.body.gzip {
            if self.pending.is_empty() {
                match self.input().await {
                    Some(Ok(bytes)) => self.pending = bytes,
                    Some(Err(error)) => return self.fail(error),
                    None => { self.ended = true; return None; },
                }
            }
            let count = self.pending.len().min(CHUNK_BYTES);
            return Some(Ok(self.pending.split_to(count).to_vec()));
        }
        loop {
            if self.inflating.is_none() {
                let decoder = self.decoder.take().expect("owned gzip decoder");
                self.inflating = Some(pool().spawn_blocking(move || inflate(decoder)));
            }
            let result = self.inflating.as_mut().expect("decode job").await;
            self.inflating = None;
            let (decoder, step) = match result {
                Ok(result) => result,
                Err(error) => return self.fail(io::Error::other(error)),
            };
            self.decoder = Some(decoder);
            match step {
                Step::Data(bytes) => return Some(Ok(bytes)),
                Step::End => { self.ended = true; return None; },
                Step::Error(error) => return self.fail(error),
                Step::Input => {
                    let bytes = match self.input().await {
                        Some(Ok(bytes)) => Some(bytes),
                        Some(Err(error)) => return self.fail(error),
                        None => None,
                    };
                    let input = self.decoder.as_mut().expect("returned decoder").get_mut();
                    debug_assert!(input.bytes.is_empty() && !input.eof);
                    if let Some(bytes) = bytes { input.bytes = bytes; }
                    else { input.eof = true; }
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Write};
    use std::net::TcpListener;
    use std::time::Duration;

    fn gzip(raw: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(raw).unwrap();
        encoder.finish().unwrap()
    }

    fn fragments(encoded: &[u8], width: usize) -> io::Result<Vec<u8>> {
        let mut decoder = Decoder::new(Input::default());
        let mut pieces = encoded.chunks(width);
        let mut output = Vec::new();
        loop {
            let (returned, step) = inflate(decoder);decoder = returned;
            match step {
                Step::Data(bytes) => { assert!(bytes.len() <= CHUNK_BYTES); output.extend(bytes); },
                Step::Input => {
                    assert!(decoder.get_ref().bytes.is_empty());
                    assert!(!decoder.get_ref().eof, "EOF must terminate or reject");
                    if let Some(piece) = pieces.next() { decoder.get_mut().bytes = Bytes::copy_from_slice(piece); }
                    else { decoder.get_mut().eof = true; }
                },
                Step::End => return Ok(output),
                Step::Error(error) => return Err(error),
            }
        }
    }

    #[test]
    fn incremental_gzip_preserves_fragmented_headers_trailers_and_members() {
        let first = vec![7; CHUNK_BYTES*3+17];
        let second: Vec<u8> = (0..19000).map(|index| (index%251) as u8).collect();
        let mut encoded = gzip(&first);encoded.extend(gzip(&second));
        let expected = [first, second].concat();
        for width in [1, 7, 64, CHUNK_BYTES] {
            assert_eq!(fragments(&encoded, width).unwrap(), expected);
        }
        assert_eq!(fragments(&gzip(&[]),1).unwrap(), b"");
    }

    #[test]
    fn incremental_gzip_rejects_truncation_crc_and_trailing_garbage() {
        let valid = gzip(&vec![3; CHUNK_BYTES+5]);
        for removed in [1, 4, 8, valid.len()-1] {
            assert!(fragments(&valid[..valid.len()-removed], 1).is_err());
        }
        let mut bad_crc=valid.clone();let index=bad_crc.len()-8;bad_crc[index]^=1;
        assert!(fragments(&bad_crc,1).is_err());
        let mut trailing=valid;trailing.extend(b"garbage");
        assert!(fragments(&trailing,1).is_err());
    }

    fn server(encoded: Vec<u8>, compressed: bool, stall: bool) -> (String, std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
        let listener=TcpListener::bind("127.0.0.1:0").unwrap();
        let url=format!("http://{}/generated",listener.local_addr().unwrap());
        let (release, wait)=std::sync::mpsc::channel();
        let worker=std::thread::spawn(move || {
            let (mut socket,_)=listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            socket.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut reader=BufReader::new(socket.try_clone().unwrap());
            loop { let mut line=String::new();assert!(reader.read_line(&mut line).unwrap()>0);if line=="\r\n" {break;} }
            write!(socket,"HTTP/1.1 200 OK\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n",encoded.len(),if compressed {"Content-Encoding: gzip\r\n"} else {""}).unwrap();
            if stall {
                socket.write_all(&encoded[..8]).unwrap();
                wait.recv_timeout(Duration::from_secs(5)).unwrap();
                socket.write_all(&encoded[8..]).unwrap();
            } else { socket.write_all(&encoded).unwrap(); }
        });
        (url,release,worker)
    }

    #[test]
    fn async_decoded_http_body_bounds_plain_and_compressed_chunks() {
        let runtime=tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let expected=vec![42;CHUNK_BYTES*5+11];
        for compressed in [false,true] {
            let encoded=if compressed {gzip(&expected)} else {expected.clone()};
            let (url,_release,worker)=server(encoded,compressed,false);
            runtime.block_on(async {
                let response=crate::http::AgentBuilder::new().build().get(&url).send(Default::default()).await.unwrap();
                let mut body=response.into_decoded();let mut actual=Vec::new();
                while let Some(chunk)=body.chunk().await {
                    let chunk=chunk.unwrap();assert!(!chunk.is_empty() && chunk.len()<=CHUNK_BYTES);actual.extend(chunk);
                }
                assert_eq!(actual,expected);
                assert!(body.chunk().await.is_none());
            });
            worker.join().unwrap();
        }
    }

    #[test]
    fn whole_body_limits_cover_wire_bytes_expansion_and_exact_plain_boundary() {
        let runtime=tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        for (raw,compressed,limit,passes) in [
            (vec![4;257],false,257,true),
            (vec![4;257],false,256,false),
            (vec![4;257],true,257,true),
            (vec![4;257],true,256,false),
            (vec![4],true,1,false),
            (Vec::new(),false,0,true),
        ] {
            let encoded=if compressed {gzip(&raw)} else {raw.clone()};
            let (url,_release,worker)=server(encoded,compressed,false);
            runtime.block_on(async {
                let response=crate::http::AgentBuilder::new().build().get(&url).send(Default::default()).await.unwrap();
                let result=response.bytes(limit).await;
                if passes {assert_eq!(result.unwrap(),raw);}
                else {assert_eq!(result.unwrap_err().kind(),io::ErrorKind::InvalidData);}
            });
            worker.join().unwrap();
        }
    }

    #[test]
    fn whole_body_gzip_waits_for_cpu_lane_without_running_inflation_on_executor() {
        let (entered,ready)=std::sync::mpsc::channel();
        let (release,wait)=std::sync::mpsc::channel();
        let held=pool().spawn_blocking(move || {entered.send(()).unwrap();let _=wait.recv_timeout(Duration::from_secs(5));});
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        let raw=vec![13;CHUNK_BYTES*2+7];let expected=raw.clone();
        let (url,_release,worker)=server(gzip(&raw),true,false);
        let runtime=tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let response=crate::http::AgentBuilder::new().build().get(&url).send(Default::default()).await.unwrap();
            let pending=tokio::spawn(async move {response.bytes(expected.len()).await});
            // The runtime can service unrelated work while inflation capacity
            // is held. Inline inflation would complete despite that hold.
            tokio::time::sleep(Duration::from_millis(20)).await;
            let waited=!pending.is_finished();
            release.send(()).unwrap();held.await.unwrap();
            assert!(waited,"gzip body must await its finite CPU job");
            assert_eq!(pending.await.unwrap().unwrap(),raw);
        });
        worker.join().unwrap();
    }

    #[test]
    fn cancelling_a_gzip_chunk_wait_preserves_decoder_and_connection() {
        let runtime=tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let expected=vec![17;CHUNK_BYTES*2+11];
        let (url,release,worker)=server(gzip(&expected),true,true);
        runtime.block_on(async {
            let response=crate::http::AgentBuilder::new().build().get(&url).send(Default::default()).await.unwrap();
            let mut body=response.into_decoded();
            assert!(tokio::time::timeout(Duration::from_millis(20),body.chunk()).await.is_err());
            release.send(()).unwrap();
            let mut actual=Vec::new();
            while let Some(chunk)=body.chunk().await {actual.extend(chunk.unwrap());}
            assert_eq!(actual,expected);
        });
        worker.join().unwrap();
    }

    #[test]
    fn held_document_decoders_cannot_starve_finite_media_inflation() {
        // Production document parsing uses the caller's finite Tokio pool.
        // Occupy all of it; media inflation owns independent bounded capacity.
        let runtime=tokio::runtime::Builder::new_current_thread().enable_all().max_blocking_threads(3).build().unwrap();
        let mut releases=Vec::new();let mut jobs=Vec::new();
        for _ in 0..3 {
            let (ready,entered)=std::sync::mpsc::channel();
            let (release,wait)=std::sync::mpsc::channel();
            jobs.push(runtime.spawn_blocking(move || {
                ready.send(()).unwrap();let _=wait.recv_timeout(Duration::from_secs(5));
            }));
            entered.recv_timeout(Duration::from_secs(5)).unwrap();releases.push(release);
        }
        let expected=vec![19;CHUNK_BYTES*2+11];
        let (url,_release,worker)=server(gzip(&expected),true,false);
        runtime.block_on(async {
            let actual=tokio::time::timeout(Duration::from_secs(2),async {
                let response=crate::http::AgentBuilder::new().build().get(&url).send(Default::default()).await.unwrap();
                let mut body=response.into_decoded();let mut actual=Vec::new();
                while let Some(chunk)=body.chunk().await {actual.extend(chunk.unwrap());}
                actual
            }).await.expect("gzip playback must progress while every document decoder is held");
            assert_eq!(actual,expected);
            // Raw image bodies also need no general worker. Complete both
            // media paths while every general worker is still held.
            let image=b"generated image bytes".to_vec();
            let (base,_release,image_server)=server(gzip(&image),true,false);
            let client=crate::Client::new(crate::Credentials {server:base.clone(),user:"generated".into(),auth:crate::Auth::ApiKey("synthetic".into())},"test");
            assert_eq!(tokio::time::timeout(Duration::from_secs(2),client.fetch_image_url_async(&base,1024))
                .await.expect("image inflation must not require a general worker").unwrap(),image);
            image_server.join().unwrap();
            assert!(jobs.iter().all(|job| !job.is_finished()));
            for release in releases { release.send(()).unwrap(); }
            for job in jobs { job.await.unwrap(); }
            // API JSON and cover XML/JSON error inspection intentionally use
            // finite general workers. Check their completed results with that
            // capacity admitted; separate stalled-body controls prove socket
            // waits never occupy it. The removed legacy pool is not recreated.
            for document in [false,true] {
                let bytes=if document {br#"{"subsonic-response":{"status":"ok","version":"1.16.1"}}"#.to_vec()} else {b"generated cover bytes".to_vec()};
                let (base,_release,server)=server(gzip(&bytes),true,false);
                let client=crate::Client::new(crate::Credentials {server:base,user:"generated".into(),auth:crate::Auth::ApiKey("synthetic".into())},"test");
                tokio::time::timeout(Duration::from_secs(2),async {
                    if document {client.fetch_async(&crate::calls::ping().request).await.unwrap();}
                    else {assert_eq!(client.cover_art_async("generated",50).await.unwrap(),Some(bytes));}
                }).await.expect("bounded cover/document path must complete with its admitted worker capacity");
                server.join().unwrap();
            }
        });
        worker.join().unwrap();
    }
}
