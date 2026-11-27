//! Async lifecycle controls use only generated processes and LocalAPI sockets.
use super::*;
use super::fixture_tests::Fixture;

async fn until(mut predicate:impl FnMut()->bool) {
    tokio::time::timeout(Duration::from_secs(4),async {
        while !predicate(){tokio::time::sleep(Duration::from_millis(1)).await;}
    }).await.expect("async Tailscale lifecycle deadline");
}
fn joined(pid:i32) {
    assert_eq!(crate::platform::process::generated::wait_nonblocking(pid).unwrap_err().raw_os_error(),Some(libc::ECHILD));
}
#[tokio::test]
async fn cancelled_stop_wait_retains_join_until_children_exit() {
    let fixture=Fixture::new();let mut owner=fixture.owner();
    until(||fixture.pids("cli-pids").len()==1).await;
    let pids=[fixture.pids("daemon-pids")[0],fixture.pids("cli-pids")[0]];
    {
        let stop=owner.stop_async();tokio::pin!(stop);
        assert!(tokio::time::timeout(Duration::from_millis(40),&mut stop).await.is_err());
    }
    until(||fixture.root.join("terminating").exists()).await;
    assert!(owner.worker.inner.lock().unwrap().start.is_none());
    assert!(!owner.worker.inner.lock().unwrap().completion.as_ref().unwrap().done.load(std::sync::atomic::Ordering::Acquire));
    fixture.release();
    tokio::time::timeout(Duration::from_secs(2),owner.stop_async()).await.unwrap();
    for pid in pids {joined(pid);}
    owner.set_enabled(true);assert!(owner.worker.inner.lock().unwrap().thread.is_none());
}
#[tokio::test]
async fn dropping_owner_requests_joined_cleanup_without_blocking_runtime() {
    let fixture=Fixture::new();let owner=fixture.owner();
    until(||fixture.pids("cli-pids").len()==1).await;
    let completion=owner.worker.inner.lock().unwrap().completion.clone().unwrap();
    let pids=[fixture.pids("daemon-pids")[0],fixture.pids("cli-pids")[0]];
    drop(owner);
    until(||fixture.root.join("terminating").exists()).await;
    assert!(!completion.done.load(std::sync::atomic::Ordering::Acquire));
    fixture.release();tokio::time::timeout(Duration::from_secs(2),completion.wait()).await.unwrap();
    for pid in pids {joined(pid);}
}
#[tokio::test]
async fn daemon_and_cli_belong_to_persistent_spawn_parent_thread() {
    let fixture=Fixture::new();let mut owner=fixture.owner();
    until(||fixture.pids("cli-pids").len()==1).await;
    let tid=crate::rt::io().spawn(async {crate::platform::process::generated::current_thread_id()}).await.unwrap();
    // Host-only task-parent introspection: Q2 lacks CHECKPOINT_RESTORE and
    // therefore this /proc children interface. PPid alone cannot prove it.
    let pids=[fixture.pids("daemon-pids")[0],fixture.pids("cli-pids")[0]];
    let parents:Vec<_>=std::fs::read_dir("/proc/self/task").unwrap().filter_map(|entry| {
        let entry=entry.ok()?;
        let text=std::fs::read_to_string(entry.path().join("children")).ok()?;
        let children:Vec<i32>=text.split_whitespace().map(|v|v.parse().unwrap()).collect();
        pids.iter().all(|pid|children.contains(pid)).then_some(entry.path())
    }).collect();
    assert_eq!(parents.len(),1);
    assert_ne!(parents[0].file_name().unwrap().to_str().unwrap(),tid.to_string());
    assert_eq!(std::fs::read_to_string(parents[0].join("comm")).unwrap().trim(),"ts-spawn");
    let parent=super::fixture_tests::owned_task_sample(parents[0].file_name().unwrap().to_str().unwrap().parse().unwrap());
    fixture.release();owner.stop_async().await;
    for pid in pids {joined(pid);}
    // pthread join and procfs disappearance are different endpoints. Reuse
    // the retained identity-bound observer; it records the immediate sample,
    // requires strict absence and refuses a replacement task incarnation.
    let observation=super::fixture_tests::disappearance(parent,Duration::from_secs(1));
    eprintln!("generated_spawn_parent_retirement={observation:?}");
    assert!(observation.passed,"joined spawn parent must disappear within the bounded observation");
}
#[tokio::test]
async fn stalled_localapi_uses_no_blocking_worker_and_cancellation_closes_socket() {
    let root=tests::fixture();
    let mut clients=Vec::new();let mut sockets=Vec::new();
    for n in 0..4 {
        let path=root.join(format!("async-{n}.sock"));
        let listener=tokio::net::UnixListener::bind(&path).unwrap();
        clients.push(tokio::spawn(async move {observed_status_async(&path,Duration::from_secs(30),None).await}));
        let (mut socket,_)=tokio::time::timeout(Duration::from_secs(2),listener.accept()).await.unwrap().unwrap();
        let mut request=Vec::new();
        while !request.ends_with(b"\r\n\r\n") {request.push(socket.read_u8().await.unwrap());}
        sockets.push(socket);
    }
    assert!(tokio::time::timeout(Duration::from_secs(1),crate::rt::spawn_blocking(|_|true)).await.unwrap().unwrap());
    for task in clients {task.abort();assert!(task.await.unwrap_err().is_cancelled());}
    for mut socket in sockets {
        let result=tokio::time::timeout(Duration::from_secs(1),socket.read_u8()).await.unwrap();
        assert_eq!(result.unwrap_err().kind(),io::ErrorKind::UnexpectedEof);
    }
    std::fs::remove_dir_all(root).unwrap();
}
#[tokio::test]
async fn supervisor_panic_joins_children_before_restart_and_preserves_stop() {
    let fixture=Fixture::new();let mut owner=fixture.owner();
    until(||fixture.pids("cli-pids").len()==1).await;
    let original=[fixture.pids("daemon-pids")[0],fixture.pids("cli-pids")[0]];
    owner.request(Request::Panic);
    until(||fixture.root.join("terminating").exists()).await;
    assert_ne!(owner.state().health,"Tailscale supervisor restarted");
    fixture.release();until(||owner.state().health=="Tailscale supervisor restarted").await;
    for pid in original {joined(pid);}
    owner.set_enabled(true);until(||fixture.pids("cli-pids").len()==2).await;
    assert!(!fixture.root.join("overlap").exists());owner.stop_async().await;
}

#[tokio::test]
async fn interrupted_fragmented_status_preserves_bytes_and_probe_order() {
    struct Reader {bytes:std::io::Cursor<Vec<u8>>,interrupt:bool}
    impl tokio::io::AsyncRead for Reader {
        fn poll_read(mut self:std::pin::Pin<&mut Self>,_:&mut std::task::Context<'_>,out:&mut tokio::io::ReadBuf<'_>)->std::task::Poll<io::Result<()>> {
            if self.interrupt {self.interrupt=false;return std::task::Poll::Ready(Err(io::ErrorKind::Interrupted.into()));}
            self.interrupt=true;
            let count=out.remaining().min(3);let bytes=out.initialize_unfilled();
            let count=std::io::Read::read(&mut self.bytes,&mut bytes[..count])?;
            out.advance(count);std::task::Poll::Ready(Ok(()))
        }
    }
    let mut reader=Reader {bytes:std::io::Cursor::new(b"HTTP/1.0 200 OK\r\n\r\n{\"BackendState\":\"Running\"}".to_vec()),interrupt:true};
    let mut phases=Vec::new();
    let bytes=status_body(&mut reader,&mut Some(&mut |phase|phases.push(phase))).await.unwrap();
    assert_eq!(parse_status(&bytes).unwrap().status,"Connected");
    assert_eq!(phases,[ProbePhase::FirstResponse,ProbePhase::HeaderReceived,ProbePhase::ResponseEnd]);
}
