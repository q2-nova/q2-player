use crate::audio::interrupt::Interrupt;
use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

async fn bounded<T>(future: impl std::future::Future<Output=T>)->T {
    tokio::time::timeout(Duration::from_secs(3),future).await.expect("generated range deadline")
}
async fn request(listener:&TcpListener)->TcpStream {
    let (mut socket,_)=bounded(listener.accept()).await.unwrap();
    let mut header=Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        header.push(bounded(socket.read_u8()).await.unwrap());
        assert!(header.len()<8192);
    }
    assert!(String::from_utf8(header).unwrap().to_lowercase().contains("range: bytes="));
    socket
}
async fn closed(socket:&mut TcpStream) {
    assert_eq!(bounded(socket.read_u8()).await.unwrap_err().kind(),io::ErrorKind::UnexpectedEof);
}
fn music(listener:&TcpListener)->RangeFetch {
    let client=subsonic::Client::new(subsonic::Credentials {
        server:format!("http://{}",listener.local_addr().unwrap()),user:"generated".into(),
        auth:subsonic::Auth::Password("synthetic".into()),
    },"range-fixture");
    let request=client.stream_request("fixture",&subsonic::StreamOptions::default());
    Arc::new(move |offset,length| {
        let response=client.read_range_async(&request,offset,length);
        Box::pin(async move {response.await.map_err(|_|io::Error::other("generated range failed"))})
    })
}
fn spoken(listener:&TcpListener,total:u64)->RangeFetch {
    let client=subsonic::audiobookshelf::Client::new(&format!("http://{}",listener.local_addr().unwrap()),"synthetic",None).unwrap();
    Arc::new(move |offset,length| {
        let response=client.range_async("/fixture",offset,length,total);
        Box::pin(async move {response.await.map(Some).map_err(|_|io::Error::other("generated range failed"))})
    })
}

#[tokio::test]
async fn music_cut_closes_stalled_headers_and_body_then_retries() {
    for body in [false,true] {
        let listener=TcpListener::bind("127.0.0.1:0").await.unwrap();
        let interrupt=Arc::new(Interrupt::new(false));
        let flag=interrupt.clone();
        let fetch=music(&listener);
        let decoder=tokio::task::spawn_blocking(move || crate::audio::callback::test_scope(|| {
            let mut ranges=super::super::Ranges {fetch,start:0,bytes:Vec::new(),unsupported:false};
            assert!(super::super::is_cut(&ranges.read(100,&mut [0;4],&interrupt).unwrap_err()));
            ranges
        }));
        let mut old=request(&listener).await;
        if body {old.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 100-103/1000\r\nContent-Length: 4\r\nConnection: close\r\n\r\n").await.unwrap();}
        flag.store(true,Ordering::Relaxed);
        let mut ranges=bounded(decoder).await.unwrap();
        closed(&mut old).await;
        flag.store(false,Ordering::Relaxed);
        let decoder=tokio::task::spawn_blocking(move || crate::audio::callback::test_scope(|| {
            let mut bytes=[0;4];
            assert_eq!(ranges.read(100,&mut bytes,&flag).unwrap(),Some(4));
            assert_eq!(&bytes,b"data");
        }));
        let mut socket=request(&listener).await;
        socket.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 100-103/1000\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndata").await.unwrap();
        bounded(decoder).await.unwrap();
    }
}

#[tokio::test]
async fn audiobook_new_seek_and_drop_close_obsolete_transport() {
    for body in [false,true] {
        let listener=TcpListener::bind("127.0.0.1:0").await.unwrap();
        let worker=Worker::new(spoken(&listener,1000),1000,1);
        worker.request(Request{id:1,offset:0,length:4});
        let mut old=request(&listener).await;
        if body {old.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-3/1000\r\nContent-Length: 4\r\nConnection: close\r\n\r\n").await.unwrap();}
        worker.request(Request{id:2,offset:900,length:100});
        closed(&mut old).await;
        let mut current=request(&listener).await;
        current.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 900-999/1000\r\nContent-Length: 100\r\nConnection: close\r\n\r\n").await.unwrap();
        let shared=worker.shared.clone();
        drop(worker);
        closed(&mut current).await;
        assert!(shared.state.lock().unwrap().closed);
        assert!(shared.state.lock().unwrap().results.is_empty());
    }
}

#[tokio::test]
async fn audiobook_pause_preserves_pending_response_and_bounded_prefetch() {
    let listener=TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut reader=RangeReader::new_async(8,Arc::new(Interrupt::new(false)),spoken(&listener,8));
    reader.request(0).unwrap();
    let mut socket=request(&listener).await;
    reader.interrupt.store(true,Ordering::Relaxed);
    let (returned, ())=crate::audio::callback::test_job(reader, |reader| {
        assert!(super::super::is_cut(&reader.read(&mut [0;8]).unwrap_err()));
    }).await; reader=returned;
    socket.write_all(b"HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-7/8\r\nContent-Length: 8\r\nConnection: close\r\n\r\nabcdefgh").await.unwrap();
    bounded(async {
        while reader.worker.as_ref().unwrap().shared.state.lock().unwrap().results.is_empty() {tokio::task::yield_now().await;}
    }).await;
    assert!(reader.pending.is_some());
    reader.interrupt.store(false,Ordering::Relaxed);
    crate::audio::callback::test_job(reader, |reader| {
        let mut bytes=[0;8]; reader.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes,b"abcdefgh");
        assert_eq!(reader.read(&mut bytes).unwrap(),0);
    }).await;
}

#[tokio::test]
#[cfg_attr(panic = "abort", ignore = "recovers from a panic; a panic = \"abort\" build, as on the Q2, aborts")]
async fn audiobook_async_panic_and_abort_publish_closed() {
    for panic in [false,true] {
        let worker=Worker::new(Arc::new(move |_,_|Box::pin(async move {
            if panic {panic!("generated range future panic");}
            std::future::pending::<RangeResult>().await
        })),4,1);
        worker.request(Request{id:1,offset:0,length:4});
        if !panic {worker.task.abort();}
        bounded(async {
            while !worker.shared.state.lock().unwrap().closed {tokio::task::yield_now().await;}
        }).await;
        assert!(worker.receive(1,0,&Interrupt::new(false)).unwrap_err().to_string().contains("worker stopped"));
    }
    // Exercise pre-poll destruction independently of scheduler timing.
    let shared=Arc::new(Shared {state:Mutex::new(State::default()),ready:Arc::default(),changed:tokio::sync::Notify::new()});
    let exit=WorkerExit(shared.clone());
    let unpolled=async move {let _exit=exit; std::future::pending::<()>().await;};
    drop(unpolled);
    assert!(shared.state.lock().unwrap().closed);
}


#[derive(Debug)]
struct HeldRangeErrorDrop {
    entered: Option<tokio::sync::oneshot::Sender<()>>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
    completed: Arc<Interrupt>,
}
impl std::fmt::Display for HeldRangeErrorDrop {
    fn fmt(&self, f:&mut std::fmt::Formatter<'_>)->std::fmt::Result {
        f.write_str("generated held range error destructor")
    }
}
impl std::error::Error for HeldRangeErrorDrop {}
impl Drop for HeldRangeErrorDrop {
    fn drop(&mut self) {
        if let Some(entered)=self.entered.take() {let _=entered.send(());}
        // Bound fixture teardown even when an assertion fails. The payload's
        // real destructor, rather than a production hook, owns this hold.
        let _=self.release.lock().unwrap().recv_timeout(Duration::from_secs(3));
        self.completed.store(true,Ordering::Release);
    }
}
struct ReleaseRangeDrop(Option<std::sync::mpsc::Sender<()>>);
impl Drop for ReleaseRangeDrop {
    fn drop(&mut self) {if let Some(release)=self.0.take(){let _=release.send(());}}
}

#[tokio::test]
async fn retired_range_payloads_leave_io_state_available_during_seek_and_close() {
    for close in [false,true] {
        let (entered, waiting)=tokio::sync::oneshot::channel();
        let (release, released)=std::sync::mpsc::channel();
        let release=ReleaseRangeDrop(Some(release));
        let completed=Arc::new(Interrupt::new(false));
        let error=io::Error::other(HeldRangeErrorDrop {entered:Some(entered),release:Mutex::new(released),completed:completed.clone()});
        let first=Arc::new(Mutex::new(Some(Err(error))));
        let fetch:RangeFetch=Arc::new(move |_,_| {
            let result=first.lock().unwrap().take();
            Box::pin(async move {
                match result {Some(result)=>result,None=>std::future::pending::<RangeResult>().await}
            })
        });
        let worker=Worker::new(fetch,4,1);
        worker.request(Request{id:1,offset:0,length:4});
        let shared=worker.shared.clone();
        bounded(async {
            while shared.state.lock().unwrap().results.is_empty() {tokio::task::yield_now().await;}
        }).await;
        let capacity=shared.state.lock().unwrap().results.capacity();
        // The real seek/close path destroys an actual queued io::Error. Its
        // destructor remains held while the I/O owner observes current state.
        let action=tokio::task::spawn_blocking(move || {
            if close {drop(worker);None}
            else {worker.request(Request{id:2,offset:0,length:4});Some(worker)}
        });
        bounded(waiting).await.unwrap();
        let observed=bounded(crate::rt::io().spawn(async move {
            // Do not poison the shared runtime on an old-source failure: a
            // held mutex is already the failing observation, not a reason to
            // synchronously park this test's I/O poll.
            shared.state.try_lock().ok().map(|state| {
                (state.generation,state.closed,state.results.is_empty(),state.results.capacity(),completed.load(Ordering::Acquire))
            })
        })).await.unwrap();
        drop(release);
        let returned=bounded(action).await.unwrap();
        assert_eq!(observed,Some((if close {1} else {2},close,true,capacity,false)),
                   "range destruction must not hold the producer's state lock");
        drop(returned);
    }
}
