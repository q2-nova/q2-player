//! Async network ownership with one bounded card write in flight. Pool workers
//! only write/close the file; none waits for HTTP, including compressed media.
use crate::audio::source::Progress;
use super::mix_space::Ticket;
use crate::playtrace::Trace;
use std::io::{self, Write};
use std::sync::{Arc, atomic::{AtomicU8, Ordering}};

#[derive(Default)]
pub(super) struct Cancel {
    // 0 active, 1 cancelled, 2 publication claimed. The last transition
    // linearizes cache completion against a concurrent obsolete-transfer sweep.
    state: AtomicU8,
    wake: tokio::sync::Notify,
}
impl Cancel {
    pub fn is_cancelled(&self) -> bool { self.state.load(Ordering::Acquire) == 1 }
    pub fn cancel(&self) -> bool {
        if self.state.compare_exchange(0,1,Ordering::AcqRel,Ordering::Acquire).is_err() { return false; }
        self.wake.notify_waiters();
        true
    }
    pub fn begin_publication(&self) -> bool {
        self.state.compare_exchange(0,2,Ordering::AcqRel,Ordering::Acquire).is_ok()
    }
    pub async fn cancelled(&self) {
        loop {
            let wake = self.wake.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            if self.is_cancelled() { return; }
            wake.await;
        }
    }
}

#[derive(Debug)]
pub(super) enum Failure { Cancelled, Length, Network(io::Error), Card(io::Error), Sync(io::Error) }

/// File ownership that can cross async request/reply boundaries. Construct it
/// in the creating card job, before any future or task can capture the file.
/// Graceful transfer completion still awaits close; this is the abandonment
/// fallback, not acknowledgement of publication or durability.
pub(super) struct FileOutput(Option<std::fs::File>);
impl FileOutput {
    pub(super) fn new(file: std::fs::File) -> Self { Self(Some(file)) }
}
impl Drop for FileOutput {
    fn drop(&mut self) { crate::rt::drop_off_executor(self.0.take()); }
}

struct Writer<W: Send + 'static> {
    output: Option<W>,
    written: u64,
    progress: Arc<Progress>,
    trace: Option<Arc<Trace>>,
}
impl<W: Send + 'static> Drop for Writer<W> {
    fn drop(&mut self) { crate::rt::drop_off_executor(self.output.take()); }
}
impl<W: Write + Send + 'static> Writer<W> {
    fn append(mut self, bytes: Vec<u8>, second: Option<Vec<u8>>, blocking: &crate::rt::Blocking) -> (Self, io::Result<()>) {
        let started = self.trace.as_ref().map_or(0, |trace| trace.now());
        let length = bytes.len() + second.as_ref().map_or(0, Vec::len);
        let output = self.output.as_mut().expect("owned transfer writer");
        let result = if let Some(second) = second {
            write_pair(output, &bytes, &second)
        } else {
            output.write_all(&bytes)
        };
        if result.is_ok() {
            self.written += length as u64;
            if let Some(trace) = self.trace.as_ref() { trace.transfer_wrote(blocking, started, length, self.written); }
            self.progress.advance(self.written);
        }
        (self, result)
    }
}

fn write_pair(output: &mut impl Write, first: &[u8], second: &[u8]) -> io::Result<()> {
    let mut buffers = [io::IoSlice::new(first), io::IoSlice::new(second)];
    let mut pending = &mut buffers[..];
    while !pending.is_empty() {
        match output.write_vectored(pending) {
            Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
            Ok(written) => io::IoSlice::advance_slices(&mut pending, written),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub(super) fn sync_file(_blocking: &crate::rt::Blocking, file: &mut std::fs::File) -> io::Result<()> {
    file.sync_all()
}

async fn close<W: Send + 'static>(mut writer: Writer<W>, sync: Option<fn(&crate::rt::Blocking, &mut W)->io::Result<()>>) -> Result<(), Failure> {
    crate::rt::spawn_blocking(move |blocking| {
        let result=sync.map_or(Ok(()),|sync|sync(blocking, writer.output.as_mut().expect("owned transfer writer")));
        drop(writer);
        result
    }).await.map_err(|error| Failure::Card(io::Error::other(error)))?
        .map_err(Failure::Sync)
}

pub(super) type Priority = Arc<dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output=()> + Send>> + Send + Sync>;

/// Wait for player transfers before reading another bounded download chunk.
/// Register the event before inspecting the active state to avoid lost wakes.
pub(super) fn playback_priority(lib: Arc<super::Library>) -> Priority {
    Arc::new(move || {
        let lib=lib.clone();
        Box::pin(async move {
            loop {
                let changed=lib.priority_changed.notified();
                tokio::pin!(changed);changed.as_mut().enable();
                if !lib.playback_transfer_running() && !lib.spoken_playback_active() && !lib.background_should_yield() {break;}
                changed.await;
            }
        })
    })
}

/// Fetch-ahead between chunks: waits while a listener waits on a Navidrome
/// request or the health probe runs (library/health.rs), and never while a
/// player stream is downloading (the player may have joined this transfer).
pub(super) fn fetch_ahead_priority(lib: Arc<super::Library>) -> Priority {
    Arc::new(move || {
        let lib=lib.clone();
        Box::pin(async move {
            loop {
                let changed=lib.priority_changed.notified();
                tokio::pin!(changed);changed.as_mut().enable();
                if !lib.background_should_yield() || lib.playback_transfer_running() {break;}
                changed.await;
            }
        })
    })
}

pub(super) fn copy_file_prioritized(
    body: subsonic::http::DecodedBody, mut output: FileOutput, progress: Arc<Progress>, cancel: Arc<Cancel>,
    trace: Option<Arc<Trace>>, sync: Option<fn(&crate::rt::Blocking, &mut std::fs::File)->io::Result<()>>,
    exact_length: Option<u64>, priority: Priority, storage: Option<Arc<Ticket>>,
) -> impl std::future::Future<Output=Result<u64,Failure>> + Send {
    copy_inner(body,output.0.take().expect("owned transfer file"),progress,cancel,trace,sync,exact_length,Some(priority),0,None,storage)
}

/// Continue only after the caller has validated the representation and
/// positioned its owned file at `initial`.
pub(super) fn copy_file_resume(
    body: subsonic::http::DecodedBody, mut output:FileOutput, progress:Arc<Progress>, cancel:Arc<Cancel>,
    total:Option<u64>, initial:u64, lib:Arc<super::Library>, storage:Arc<Ticket>,
) -> impl std::future::Future<Output=Result<u64,Failure>> + Send {
    progress.set_total(total);progress.advance(initial);
    let priority = playback_priority(lib.clone());
    let overlap = Overlap {
        allowed: Box::new(move || !lib.radio_dozing() && !lib.offline_mode() && !lib.fetching()
            && lib.prefetch_ready.load(Ordering::Acquire) && !lib.playback_transfer_running()
            && !lib.spoken_playback_active() && !lib.background_should_yield()),
        #[cfg(test)]
        received: None,
    };
    copy_inner(body,output.0.take().expect("owned transfer file"),progress,cancel,None,Some(sync_file),total,Some(priority),initial,Some(overlap),Some(storage))
}

/// Move the already guarded file directly into the copy future's guard.
/// No raw file is exposed to the caller or held across an async boundary.
pub(super) fn copy_file(
    body: subsonic::http::DecodedBody,
    mut output: FileOutput,
    progress: Arc<Progress>,
    cancel: Arc<Cancel>,
    trace: Option<Arc<Trace>>,
    sync: Option<fn(&crate::rt::Blocking, &mut std::fs::File)->io::Result<()>>,
    exact_length: Option<u64>,
    priority: Option<Priority>,
) -> impl std::future::Future<Output = Result<u64, Failure>> + Send {
    copy_inner(body, output.0.take().expect("owned transfer file"), progress, cancel, trace, sync, exact_length, priority, 0, None, None)
}

/// Cancellation drops the HTTP body immediately, but joins a card write already
/// in progress before returning. Callers may then publish/retire the file safely.
/// The owner must await this function, not abort its task while a write runs.
pub(super) fn copy<W: Write + Send + 'static>(
    body: subsonic::http::DecodedBody,
    output: W,
    progress: Arc<Progress>,
    cancel: Arc<Cancel>,
    trace: Option<Arc<Trace>>,
    sync: Option<fn(&crate::rt::Blocking, &mut W)->io::Result<()>>,
    exact_length: Option<u64>,
) -> impl std::future::Future<Output = Result<u64, Failure>> + Send {
    copy_inner(body,output,progress,cancel,trace,sync,exact_length,None,0,None,None)
}
struct Overlap {
    allowed: Box<dyn Fn() -> bool + Send + Sync>,
    #[cfg(test)]
    received: Option<std::sync::mpsc::Sender<()>>,
}

fn copy_inner<W: Write + Send + 'static>(
    mut body: subsonic::http::DecodedBody, output: W, progress: Arc<Progress>, cancel: Arc<Cancel>,
    trace: Option<Arc<Trace>>, sync: Option<fn(&crate::rt::Blocking,&mut W)->io::Result<()>>,
    exact_length: Option<u64>, priority: Option<Priority>, initial:u64, overlap: Option<Overlap>, storage: Option<Arc<Ticket>>,
) -> impl std::future::Future<Output=Result<u64,Failure>> + Send {
    // Wrap before the first poll. An unpolled future or an abandoned job reply
    // must retire its descriptor off the executor just like an active transfer.
    let mut writer = Writer { output: Some(output), written: initial, progress, trace };
    async move {
    let mut buffered = None;
    let result = loop {
        if let Some(priority)=&priority {
            tokio::select! { biased; ()=cancel.cancelled()=>break Err(Failure::Cancelled), ()=priority()=>{} }
        }
        let chunk = match buffered.take() {
            Some(chunk) => chunk,
            None => tokio::select! {
                biased;
                () = cancel.cancelled() => break Err(Failure::Cancelled),
                chunk = body.chunk() => chunk,
            },
        };
        let bytes = match chunk {
            Some(Ok(bytes)) => bytes,
            Some(Err(error)) => break Err(Failure::Network(error)),
            None => break if exact_length.is_some_and(|length|length!=writer.written) {Err(Failure::Length)} else {Ok(writer.written)},
        };
        if cancel.is_cancelled() { break Err(Failure::Cancelled); }
        if exact_length.is_some_and(|length|writer.written.saturating_add(bytes.len() as u64)>length) {break Err(Failure::Length);}
        let ahead = overlap.as_ref().is_some_and(|window| (window.allowed)());
        let mut second = None;
        if ahead {
            // Pair two already available chunks in a single card job without
            // delaying the first chunk or copying either buffer.
            let next = tokio::select! {
                biased;
                () = cancel.cancelled() => break Err(Failure::Cancelled),
                chunk = body.chunk() => Some(chunk),
                () = std::future::ready(()) => None,
            };
            if let Some(next) = next {
                match next {
                    Some(Ok(chunk)) if exact_length.is_none_or(|length|
                        writer.written.saturating_add((bytes.len() + chunk.len()) as u64) <= length) => {
                        second = Some(chunk);
                    }
                    other => buffered = Some(other),
                }
            }
        }
        // A new foreground request can arrive while the next chunk is read.
        // Check again before enqueueing a combined write.
        if second.is_some() {
            if let Some(priority) = &priority {
                tokio::select! { biased; () = cancel.cancelled() => break Err(Failure::Cancelled), () = priority() => {} }
            }
        }
        let length = (bytes.len() + second.as_ref().map_or(0, Vec::len)) as u64;
        if let Some(storage) = &storage {
            match storage.ensure(length, 1).await {
                Ok(()) => {},
                Err(error) if error.kind() == io::ErrorKind::Interrupted => break Err(Failure::Cancelled),
                Err(error) => break Err(Failure::Card(error)),
            }
        }
        let read_ahead = ahead && second.is_none() && buffered.is_none();
        let credit = storage.clone();
        let mut work = crate::rt::spawn_blocking(move |blocking| {
            let result = writer.append(bytes, second, blocking);
            if result.1.is_ok() {
                if let Some(credit) = credit { credit.consumed(length); }
            }
            result
        });
        let written = loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    drop(body);
                    let (returned, _) = work.await.map_err(|error| Failure::Card(io::Error::other(error)))?;
                    close(returned, None).await?;
                    return Err(Failure::Cancelled);
                },
                written = &mut work => break written,
                chunk = body.chunk(), if read_ahead && buffered.is_none() => {
                    buffered = Some(chunk);
                    #[cfg(test)]
                    if let Some(reply) = overlap.as_ref().and_then(|window| window.received.as_ref()) {
                        let _ = reply.send(());
                    }
                },
            }
        };
        match written {
            Ok((returned, write)) => {
                writer = returned;
                if let Err(error) = write { break Err(Failure::Card(error)); }
            },
            Err(error) => return Err(Failure::Card(io::Error::other(error))),
        }
    };
    drop(body);
    // Close descriptors on the pool too, after the final write has joined.
    close(writer,if result.is_ok() {sync} else {None}).await?;
    if cancel.is_cancelled() { Err(Failure::Cancelled) } else { result }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;
    use std::sync::{Mutex, mpsc};
    use std::time::Duration;

    fn server() -> (String, std::thread::JoinHandle<()>) {
        let listener=TcpListener::bind("127.0.0.1:0").unwrap();
        let url=format!("http://{}/generated",listener.local_addr().unwrap());
        let worker=std::thread::spawn(move || {
            let (mut socket,_)=listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            socket.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut reader=BufReader::new(socket.try_clone().unwrap());
            loop {let mut line=String::new();assert!(reader.read_line(&mut line).unwrap()>0);if line=="\r\n" {break;}}
            write!(socket,"HTTP/1.1 200 OK\r\nContent-Length: 196608\r\nConnection: close\r\n\r\n").unwrap();
            // Cancellation may close the socket before its fixture body ends.
            let _=socket.write_all(&vec![7;196608]);
        });
        (url,worker)
    }

    struct HeldWrite {
        entered: mpsc::Sender<()>,
        release: mpsc::Receiver<()>,
        state: Arc<Mutex<(usize,usize,bool)>>,
    }
    impl Write for HeldWrite {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.entered.send(()).unwrap();
            self.release.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
            let mut state=self.state.lock().unwrap();state.0+=1;state.1+=bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {Ok(())}
    }
    impl Drop for HeldWrite {
        fn drop(&mut self) {self.state.lock().unwrap().2=true;}
    }

    #[test]
    fn priority_gate_holds_download_and_cancellation_remains_prompt() {
        for cancel_while_held in [false,true] {
            let (url,server)=server();let cancel=Arc::new(Cancel::default());let worker_cancel=cancel.clone();
            let (entered,waiting)=mpsc::channel();let release=Arc::new(tokio::sync::Notify::new());
            let gate=release.clone();let ready=Arc::new(std::sync::atomic::AtomicBool::new(false));let flag=ready.clone();
            let priority:Priority=Arc::new(move || {
                let gate=gate.clone();let entered=entered.clone();let flag=flag.clone();
                Box::pin(async move {
                    loop {
                        let notified=gate.notified();tokio::pin!(notified);notified.as_mut().enable();
                        if flag.load(std::sync::atomic::Ordering::Acquire) {break;}
                        let _=entered.send(());notified.await;
                    }
                })
            });
            let progress=Progress::new();let observed=progress.clone();let (done,finished)=mpsc::channel();
            crate::rt::io().spawn(async move {
                let response=subsonic::http::AgentBuilder::new().build().get(&url).send(Default::default()).await.unwrap();
                done.send(copy_inner(response.into_decoded(),Vec::new(),progress,worker_cancel,None,None,None,Some(priority),0,None,None).await).unwrap();
            });
            waiting.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
            assert_eq!(observed.snapshot().written,0);
            if cancel_while_held {cancel.cancel();} else {ready.store(true,std::sync::atomic::Ordering::Release);release.notify_waiters();}
            let result=finished.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
            if cancel_while_held {assert!(matches!(result,Err(Failure::Cancelled)));} else {assert_eq!(result.unwrap(),196608);}
            server.join().unwrap();
        }
    }

    #[test]
    fn cancelled_transfer_joins_pending_write_without_blocking_executor_or_queueing_more_writes() {
        let (url,server)=server();
        let cancel=Arc::new(Cancel::default());let task_cancel=cancel.clone();
        let (entered,writing)=mpsc::channel();let (release,wait)=mpsc::channel();
        let state=Arc::new(Mutex::new((0,0,false)));
        let output=HeldWrite {entered,release:wait,state:state.clone()};
        let (done,finished)=mpsc::channel();
        crate::rt::io().spawn(async move {
            let response=subsonic::http::AgentBuilder::new().build().get(&url).send(Default::default()).await.unwrap();
            done.send(copy(response.into_decoded(),output,Progress::new(),task_cancel,None,None,None).await).unwrap();
        });
        writing.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        cancel.cancel();
        let (pulse,pulsed)=mpsc::channel();
        crate::rt::io().spawn(async move {pulse.send(()).unwrap();});
        pulsed.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        assert!(finished.try_recv().is_err(), "terminal must wait for the active card write");
        assert_eq!(*state.lock().unwrap(),(0,0,false));
        release.send(()).unwrap();
        assert!(matches!(finished.recv_timeout(crate::library::TEST_LIVENESS).unwrap(),Err(Failure::Cancelled)));
        let state=state.lock().unwrap();assert_eq!(state.0,1);assert!(state.1<=65536);assert!(state.2);
        server.join().unwrap();
    }

    #[test]
    fn cancellation_after_read_ahead_closes_network_before_held_card_work_joins() {
        use std::io::Read;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/generated", listener.local_addr().unwrap());
        let (send_more, allow_more) = mpsc::channel();
        let (closed, network_closed) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut request = BufReader::new(socket.try_clone().unwrap());
            loop {
                let mut line = String::new();
                assert!(request.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" { break; }
            }
            write!(socket, "HTTP/1.1 200 OK\r\nContent-Length: 8193\r\nConnection: close\r\n\r\n").unwrap();
            socket.write_all(&[7]).unwrap();
            allow_more.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
            socket.write_all(&[8; 4096]).unwrap();
            assert_eq!(request.read(&mut [0]).unwrap(), 0, "HTTP body closed while card write remains held");
            closed.send(()).unwrap();
        });

        let (entered, writing) = mpsc::channel();
        let (release, hold) = mpsc::channel();
        let (received, read_ahead) = mpsc::channel();
        let (done, finished) = mpsc::channel();
        let state = Arc::new(Mutex::new((0, 0, false)));
        let output = HeldWrite { entered, release: hold, state: state.clone() };
        let cancel = Arc::new(Cancel::default());
        let task_cancel = cancel.clone();
        crate::rt::io().spawn(async move {
            let response = subsonic::http::AgentBuilder::new().build().get(&url).send(Default::default()).await.unwrap();
            let window = Overlap { allowed: Box::new(|| true), received: Some(received) };
            let result = copy_inner(response.into_decoded(), output, Progress::new(), task_cancel, None, None, Some(8193), None, 0, Some(window), None).await;
            done.send(result).unwrap();
        });
        writing.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        send_more.send(()).unwrap();
        read_ahead.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        assert!(cancel.cancel());
        network_closed.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        assert!(finished.try_recv().is_err());
        release.send(()).unwrap();
        assert!(matches!(finished.recv_timeout(crate::library::TEST_LIVENESS).unwrap(), Err(Failure::Cancelled)));
        assert_eq!(*state.lock().unwrap(), (1, 1, true));
        server.join().unwrap();
    }

    #[test]
    fn paired_card_write_handles_interrupts_and_partial_vectored_writes() {
        struct ShortWriter {
            bytes: Vec<u8>,
            attempts: usize,
        }
        impl Write for ShortWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.attempts += 1;
                if self.attempts == 2 { return Err(io::ErrorKind::Interrupted.into()); }
                let count = bytes.len().min(3);
                self.bytes.extend_from_slice(&bytes[..count]);
                Ok(count)
            }
            fn flush(&mut self) -> io::Result<()> { Ok(()) }
        }
        let mut writer = ShortWriter { bytes: Vec::new(), attempts: 0 };
        write_pair(&mut writer, b"1234", b"abcdefg").unwrap();
        assert_eq!(&writer.bytes, b"1234abcdefg");
        assert_eq!(writer.attempts, 6);
    }

    #[test]
    fn a_body_without_an_admitted_length_cannot_consume_the_card_reserve() {
        let (url, server) = server();
        let mut config = crate::config::Config::default();
        config.storage.reserve_mb = u64::MAX;
        let root = crate::library::generated_test_directory("mix-unknown-length");
        let lib = crate::library::Library::create(crate::config::Paths::new(root), &config, Arc::new(|_| {})).0;
        let path = lib.paths.offline().join("generated-unknown.wav");
        let output = std::fs::File::create(&path).unwrap();
        let progress = Progress::new();
        let observed = progress.clone();
        let cancel = Arc::new(Cancel::default());
        let credit = Ticket::new(lib, cancel.clone(), true, "unknown-length".into());
        let (done, result) = mpsc::channel();
        crate::rt::io().spawn(async move {
            let response = subsonic::http::AgentBuilder::new().build().get(&url).send(Default::default()).await.unwrap();
            // Exercise the writer without an authoritative response length or
            // an up-front allocation credit from the admitting request.
            done.send(copy_inner(response.into_decoded(), output, progress, cancel, None, None, None, None, 0, None, Some(credit)).await).unwrap();
        });
        assert!(matches!(result.recv_timeout(crate::library::TEST_LIVENESS).unwrap(),
            Err(Failure::Card(error)) if error.raw_os_error() == Some(libc::ENOSPC)));
        assert_eq!(observed.snapshot().written, 0);
        assert_eq!(path.metadata().unwrap().len(), 0);
        server.join().unwrap();
    }

    #[test]
    fn card_write_failure_is_distinct_from_network_failure_and_does_not_advance_progress() {
        let (url,server)=server();let (done,finished)=mpsc::channel();
        let output=std::fs::OpenOptions::new().write(true).open("/dev/full").unwrap();
        let progress=Progress::new();let observed=progress.clone();
        crate::rt::io().spawn(async move {
            let response=subsonic::http::AgentBuilder::new().build().get(&url).send(Default::default()).await.unwrap();
            done.send(copy(response.into_decoded(),output,progress,Arc::new(Cancel::default()),None,None,None).await).unwrap();
        });
        assert!(matches!(finished.recv_timeout(crate::library::TEST_LIVENESS).unwrap(),Err(Failure::Card(error)) if error.raw_os_error()==Some(libc::ENOSPC)));
        assert_eq!(observed.snapshot().written,0);
        server.join().unwrap();
    }

    struct HeldClose { entered: mpsc::Sender<()>, release: mpsc::Receiver<()> }
    impl Write for HeldClose {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {Ok(bytes.len())}
        fn flush(&mut self) -> io::Result<()> {Ok(())}
    }
    impl Drop for HeldClose {
        fn drop(&mut self) {self.entered.send(()).unwrap();self.release.recv_timeout(crate::library::TEST_LIVENESS).unwrap();}
    }

    #[test]
    fn unpolled_and_network_waiting_transfers_close_off_executor() {
        struct CloseDomain(mpsc::Sender<bool>);
        impl Write for CloseDomain {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> { Ok(bytes.len()) }
            fn flush(&mut self) -> io::Result<()> { Ok(()) }
        }
        impl Drop for CloseDomain {
            fn drop(&mut self) { self.0.send(crate::rt::on_executor()).unwrap(); }
        }
        for poll in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let url = format!("http://{}/generated", listener.local_addr().unwrap());
            let (release, held) = mpsc::channel();
            let server = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                socket.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut reader = BufReader::new(socket.try_clone().unwrap());
                loop {
                    let mut line = String::new();
                    assert!(reader.read_line(&mut line).unwrap() > 0);
                    if line == "\r\n" { break; }
                }
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Length: 1\r\nConnection: close\r\n\r\n").unwrap();
                held.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
            });
            let (closed, retired) = mpsc::channel();
            crate::rt::Ui::new().unwrap().block_on(async {
                let response = subsonic::http::AgentBuilder::new().build().get(&url)
                    .send(Default::default()).await.unwrap();
                let mut pending = Box::pin(copy(response.into_decoded(), CloseDomain(closed),
                    Progress::new(), Arc::new(Cancel::default()), None, None, None));
                if poll {
                    std::future::poll_fn(|cx| {
                        assert!(std::future::Future::poll(pending.as_mut(), cx).is_pending(),
                            "fixture withholds the body while the transfer owns its writer");
                        std::task::Poll::Ready(())
                    }).await;
                }
                drop(pending);
            });
            assert!(!retired.recv_timeout(crate::library::TEST_LIVENESS).unwrap());
            release.send(()).unwrap();
            server.join().unwrap();
        }
    }

    #[test]
    fn cancellation_during_final_close_does_not_return_success_for_publication() {
        let (url,server)=server();let (done,finished)=mpsc::channel();
        let (entered,closing)=mpsc::channel();let (release,wait)=mpsc::channel();
        let cancel=Arc::new(Cancel::default());let task_cancel=cancel.clone();
        crate::rt::io().spawn(async move {
            let response=subsonic::http::AgentBuilder::new().build().get(&url).send(Default::default()).await.unwrap();
            done.send(copy(response.into_decoded(),HeldClose {entered,release:wait},Progress::new(),task_cancel,None,None,None).await).unwrap();
        });
        closing.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        assert!(cancel.cancel());
        assert!(finished.try_recv().is_err());
        release.send(()).unwrap();
        assert!(matches!(finished.recv_timeout(crate::library::TEST_LIVENESS).unwrap(),Err(Failure::Cancelled)));
        assert!(!cancel.begin_publication());
        server.join().unwrap();
    }

    #[test]
    fn exact_length_rejects_short_and_excess_audio_before_durable_sync() {
        for length in [0,300_000] {
            let (url,server)=server();let (done,finished)=mpsc::channel();
            let progress=Progress::new();let observed=progress.clone();
            crate::rt::io().spawn(async move {
                let response=subsonic::http::AgentBuilder::new().build().get(&url).send(Default::default()).await.unwrap();
                done.send(copy(response.into_decoded(),Vec::new(),progress,Arc::new(Cancel::default()),None,Some(|_, _|panic!("invalid length must not sync")),Some(length)).await).unwrap();
            });
            assert!(matches!(finished.recv_timeout(crate::library::TEST_LIVENESS).unwrap(),Err(Failure::Length)));
            assert_eq!(observed.snapshot().written,if length==0 {0} else {196608});
            server.join().unwrap();
        }
    }

    #[test]
    fn publication_claim_and_late_cancellation_have_one_winner() {
        let cancelled=Cancel::default();
        assert!(cancelled.cancel());assert!(!cancelled.begin_publication());assert!(!cancelled.cancel());
        let completing=Cancel::default();
        assert!(completing.begin_publication());assert!(!completing.cancel());
        assert!(!completing.is_cancelled());assert!(!completing.begin_publication());
    }
}
