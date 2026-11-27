use super::*;
use tokio::io::{AsyncReadExt,AsyncWriteExt};

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
}
async fn request_head(socket:&mut tokio::net::TcpStream) -> String {
    let mut head=Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        assert!(head.len()<8192);
        let mut byte=[0];socket.read_exact(&mut byte).await.unwrap();head.push(byte[0]);
    }
    String::from_utf8(head).unwrap()
}
async fn server(bytes:Vec<u8>,status:u16,headers:&str,truncated:bool) -> (String,tokio::task::JoinHandle<String>) {
    let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint=format!("http://{}/feed",listener.local_addr().unwrap());
    let headers=headers.to_owned();
    let task=tokio::spawn(async move {
        let (mut socket,_)=tokio::time::timeout(Duration::from_secs(5),listener.accept()).await.unwrap().unwrap();
        let head=request_head(&mut socket).await;
        let response=format!("HTTP/1.1 {status} Generated\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n",bytes.len()+usize::from(truncated));
        socket.write_all(response.as_bytes()).await.unwrap();
        // A bounded oversized-body decoder may legitimately close early.
        let _=socket.write_all(&bytes).await;
        head
    });
    (endpoint,task)
}
fn document() -> Vec<u8> {
    let now=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    serde_json::to_vec(&serde_json::json!({"stationId":7,"steps":{"current":{"stationId":7,"embedType":"song","title":"Generated","authors":"Fixture","start":now-1,"end":now+60}}})).unwrap()
}
fn gzip(bytes:&[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder=flate2::write::GzEncoder::new(Vec::new(),flate2::Compression::fast());
    encoder.write_all(bytes).unwrap();encoder.finish().unwrap()
}

#[test]
fn async_generated_feed_preserves_proxy_gzip_limit_and_error_boundaries() {
    let decoder=Arc::new(Decoder::new());
    runtime().block_on(async {
        for compressed in [false,true] {
            let body=document();let body=if compressed {gzip(&body)}else{body};
            let (url,task)=server(body,200,if compressed {"Content-Encoding: gzip\r\n"}else{""},false).await;
            // The generated proxy must receive the remote absolute URI; no
            // connection or DNS request is made to the synthetic remote host.
            let update=fetch_with("http://generated.invalid/live",Some(&url),&decoder).await.unwrap();
            assert_eq!(update.title.as_deref(),Some("Fixture - Generated"));
            assert!((5..=60).contains(&update.refresh.as_secs()));
            assert!(task.await.unwrap().starts_with("GET http://generated.invalid/live HTTP/1.1\r\n"));
            let oversized=vec![b' ';MAX_RESPONSE as usize+1];
            let body=if compressed {gzip(&oversized)} else {oversized};
            let (url,task)=server(body,200,if compressed {"Content-Encoding: gzip\r\n"}else{""},false).await;
            assert_eq!(fetch_with(&url,None,&decoder).await.err().unwrap().to_string(),"Radio metadata too large");
            task.await.unwrap();
        }
        let (url,task)=server(b"private generated body".to_vec(),401,"",false).await;
        assert_eq!(fetch_with(&url,None,&decoder).await.err().unwrap().to_string(),"Radio metadata HTTP 401");task.await.unwrap();
        let (url,task)=server(document(),200,"",true).await;
        assert!(fetch_with(&url,None,&decoder).await.is_err());task.await.unwrap();
        assert_eq!(fetch_with("http://generated.invalid",Some("broken secret proxy"),&decoder).await.err().unwrap().to_string(),"Invalid radio proxy");
    });
}

#[test]
fn metadata_stalled_body_keeps_eight_second_transport_deadline() {
    let decoder=Arc::new(Decoder::new());
    runtime().block_on(async {
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint=format!("http://{}/feed",listener.local_addr().unwrap());
        let task=tokio::spawn(async move {
            let (mut socket,_)=listener.accept().await.unwrap();request_head(&mut socket).await;
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n{").await.unwrap();
            let mut byte=[0];
            let closed=tokio::time::timeout(Duration::from_secs(10),socket.read(&mut byte)).await.unwrap();
            assert!(matches!(closed,Ok(0)) || closed.is_err());
        });
        let start=std::time::Instant::now();
        let result=tokio::time::timeout(Duration::from_secs(10),fetch_with(&endpoint,None,&decoder)).await.expect("the8s production deadline must win");
        assert_eq!(result.err().unwrap().kind(),io::ErrorKind::TimedOut);
        assert!(start.elapsed()>=Duration::from_secs(7));
        task.await.unwrap();
    });
}

#[test]
fn cancellation_closes_actual_metadata_socket_without_waiting_for_timeout() {
    let decoder=Arc::new(Decoder::new());
    runtime().block_on(async {
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint=format!("http://{}/feed",listener.local_addr().unwrap());
        let (ready,started)=tokio::sync::oneshot::channel();
        let server=tokio::spawn(async move {
            let (mut socket,_)=listener.accept().await.unwrap();request_head(&mut socket).await;
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n{").await.unwrap();
            ready.send(()).unwrap();
            let mut byte=[0];
            let result=tokio::time::timeout(Duration::from_secs(3),socket.read(&mut byte)).await.expect("cancel must close promptly");
            assert!(matches!(result,Ok(0)) || result.is_err());
        });
        let selected=decoder.clone();
        let fetch=tokio::spawn(async move {fetch_with(&endpoint,None,&selected).await});
        started.await.unwrap();
        // Neither the I/O executor nor the finite parser lane waits for this
        // admitted body. The permit still prevents accumulating more bodies.
        assert_eq!(tokio::spawn(async {7}).await.unwrap(),7);
        tokio::time::timeout(Duration::from_secs(2),async {
            while decoder.admission.available_permits()!=0 {tokio::task::yield_now().await;}
        }).await.unwrap();
        assert_eq!(tokio::time::timeout(Duration::from_secs(2),decoder.runtime.spawn_blocking(||7)).await.expect("stalled HTTP must leave parser capacity free").unwrap(),7);
        assert_eq!(decoder.started.load(std::sync::atomic::Ordering::SeqCst),0);
        fetch.abort();assert!(fetch.await.err().unwrap().is_cancelled());
        server.await.unwrap();
    });
}

#[test]
fn metadata_has_independent_capacity_when_shared_http_decoders_are_busy() {
    const CHILD:&str="Q2_FIP_DECODE_ISOLATED_TEST";
    if std::env::var(CHILD).as_deref()!=Ok("1") {
        // Saturating the shared decoder pool must not contaminate parallel
        // tests. Target producers run this exact case in its own process with
        // the same explicit marker (QEMU need not exec a nested MIPS ELF).
        let result=std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact","radio::france::wire_tests::metadata_has_independent_capacity_when_shared_http_decoders_are_busy","--nocapture"])
            .env(CHILD,"1").output().unwrap();
        assert!(result.status.success(),"{}",String::from_utf8_lossy(&result.stdout));
        return;
    }
    runtime().block_on(async {
        struct Release(Arc<(std::sync::Mutex<bool>,std::sync::Condvar)>);
        impl Drop for Release {fn drop(&mut self){*self.0.0.lock().unwrap()=true;self.0.1.notify_all();}}
        let gate=Arc::new((std::sync::Mutex::new(false),std::sync::Condvar::new()));
        let release=Release(gate.clone());let mut tasks=Vec::new();let mut entered=Vec::new();
        for _ in 0..3 {
            let gate=gate.clone();let (send,receive)=tokio::sync::oneshot::channel();entered.push(receive);
            tasks.push(tokio::spawn(crate::http::Response::synthetic(200).read_with(move |_| {
                send.send(()).unwrap();let mut open=gate.0.lock().unwrap();while !*open {open=gate.1.wait(open).unwrap();}
            })));
        }
        for entered in entered {tokio::time::timeout(Duration::from_secs(3),entered).await.unwrap().unwrap();}
        let (url,server)=server(document(),200,"",false).await;
        let result=tokio::time::timeout(Duration::from_secs(2),fetch_from_async(&url,None)).await;
        drop(release);
        for task in tasks {task.await.unwrap().unwrap();}
        server.await.unwrap();
        assert_eq!(result.expect("unrelated decoder stalls cannot delay metadata").unwrap().title.as_deref(),Some("Fixture - Generated"));
    });
}

#[test]
fn cancelled_admission_closes_bodies_and_never_accumulates_decoder_jobs() {
    let decoder=Arc::new(Decoder::new());
    runtime().block_on(async {
        use std::sync::atomic::Ordering::SeqCst;
        let permit=decoder.admission.clone().acquire_owned().await.unwrap();
        for ordinal in 1..=3 {
            let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint=format!("http://{}/feed",listener.local_addr().unwrap());
            let server=tokio::spawn(async move {
                let (mut socket,_)=listener.accept().await.unwrap();request_head(&mut socket).await;
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n{").await.unwrap();
                let mut byte=[0];let closed=tokio::time::timeout(Duration::from_secs(10),socket.read(&mut byte)).await.unwrap();
                assert!(matches!(closed,Ok(0)) || closed.is_err());
            });
            let selected=decoder.clone();
            let fetch=tokio::spawn(async move {fetch_with(&endpoint,None,&selected).await});
            tokio::time::timeout(Duration::from_secs(2),async {
                while decoder.entered.load(SeqCst)<ordinal {tokio::task::yield_now().await;}
            }).await.unwrap();
            assert_eq!(decoder.started.load(SeqCst),0);
            if ordinal==3 {
                let result=tokio::time::timeout(Duration::from_secs(9),fetch).await.expect("outer8s deadline includes admission").unwrap();
                assert_eq!(result.err().unwrap().kind(),io::ErrorKind::TimedOut);
            } else {fetch.abort();assert!(fetch.await.err().unwrap().is_cancelled());}
            server.await.unwrap();
        }
        assert_eq!(decoder.started.load(SeqCst),0);
        drop(permit);
        let (url,server)=server(document(),200,"",false).await;
        assert_eq!(fetch_with(&url,None,&decoder).await.unwrap().title.as_deref(),Some("Fixture - Generated"));
        server.await.unwrap();
        let done=decoder.admission.clone().acquire_owned().await.unwrap();
        assert_eq!(decoder.started.load(SeqCst),1);drop(done);
    });
}
