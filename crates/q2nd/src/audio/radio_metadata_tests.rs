use super::*;
use subsonic::radio::france::Update;
use tokio::sync::{mpsc, oneshot};

type Reply = oneshot::Sender<io::Result<Update>>;
struct Rig {
    owner: Buffered,
    metadata: subsonic::radio::Metadata,
    requests: mpsc::UnboundedReceiver<Reply>,
    task: tokio::task::JoinHandle<()>,
}
impl Rig {
    fn new() -> Self {
        let owner = Buffered::empty(Arc::new(Interrupt::new(false)));
        let metadata = subsonic::radio::Metadata::default();
        let (send, requests) = mpsc::unbounded_channel();
        let start = tokio::time::Instant::now();
        let task = tokio::spawn(poll_station_metadata(owner.shared.clone(),metadata.clone(),move || {
            let (reply, receive) = oneshot::channel();
            send.send(reply).unwrap();
            async move { receive.await.unwrap() }
        },move || 1000+start.elapsed().as_secs()));
        Self {owner,metadata,requests,task}
    }
    async fn request(&mut self) -> Reply { self.requests.recv().await.unwrap() }
    fn title(&self) -> Option<String> { self.metadata.lock().unwrap().clone() }
}
async fn settle() { for _ in 0..20 { tokio::task::yield_now().await; } }
fn failed(reply: Reply) { assert!(reply.send(Err(io::Error::other("generated metadata failure"))).is_ok()); }
fn success(reply: Reply,expires:u64,refresh:u64) {
    assert!(reply.send(Ok(Update {title:Some("Generated title".into()),expires,refresh:Duration::from_secs(refresh)})).is_ok());
}

#[tokio::test(start_paused = true)]
async fn retry_deadlines_are_15_30_60_capped_and_success_resets_them() {
    let mut rig = Rig::new();
    let mut request = rig.request().await;
    for delay in [15,30,60,60] {
        failed(request);settle().await;
        tokio::time::advance(Duration::from_secs(delay-1)).await;settle().await;
        assert!(rig.requests.try_recv().is_err());
        tokio::time::advance(Duration::from_secs(1)).await;
        request=rig.request().await;
    }
    success(request,1300,5);settle().await;
    assert_eq!(rig.title().as_deref(),Some("Generated title"));
    tokio::time::advance(Duration::from_secs(5)).await;
    failed(rig.request().await);settle().await;
    tokio::time::advance(Duration::from_secs(14)).await;settle().await;
    assert!(rig.requests.try_recv().is_err());
    tokio::time::advance(Duration::from_secs(1)).await;
    let request=rig.request().await;
    drop(rig.owner);rig.task.await.unwrap();
    assert!(request.is_closed());
}

#[tokio::test(start_paused = true)]
async fn title_expiry_is_preserved_across_a_stalled_failure() {
    let mut rig=Rig::new();
    success(rig.request().await,1007,5);settle().await;
    tokio::time::advance(Duration::from_secs(5)).await;
    let request=rig.request().await;
    assert_eq!(rig.title().as_deref(),Some("Generated title"));
    tokio::time::advance(Duration::from_secs(3)).await;
    failed(request);settle().await;
    assert_eq!(rig.title(),None);
    drop(rig.owner);rig.task.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn close_cancels_stalled_request_and_retry_wait_without_late_publish() {
    for during_request in [false,true] {
        let mut rig=Rig::new();
        let request=rig.request().await;
        let pending=if during_request {Some(request)} else {success(request,1060,60);None};
        settle().await;
        let title=rig.title();
        drop(rig.owner);
        rig.task.await.unwrap();
        if let Some(request)=pending {assert!(request.is_closed());}
        assert_eq!(*rig.metadata.lock().unwrap(),title);
    }
}

#[tokio::test(start_paused = true)]
async fn completion_that_closes_audio_before_returning_cannot_publish() {
    let owner=Buffered::empty(Arc::new(Interrupt::new(false)));
    let shared=owner.shared.clone();
    let metadata=subsonic::radio::Metadata::default();
    poll_station_metadata(shared.clone(),metadata.clone(),move || {
        // Ready after the cancellation arm was polled: publication must still
        // check under the close lock, without relying on select branch order.
        shared.buffer.lock().unwrap().done=true;
        std::future::ready(Ok(Update {title:Some("Late".into()),expires:1200,refresh:Duration::from_secs(60)}))
    },||1000).await;
    assert_eq!(*metadata.lock().unwrap(),None);
}

#[tokio::test(start_paused = true)]
async fn stalled_metadata_does_not_hold_audio_buffer_or_io_executor() {
    let mut rig=Rig::new();
    let request=rig.request().await;
    {
        let mut buffer=rig.owner.shared.buffer.lock().unwrap();
        buffer.bytes.extend(b"generated audio");
        buffer.ready=true;
    }
    let (owner, bytes) = crate::audio::callback::test_job(rig.owner, |owner| {
        let mut bytes=[0;15]; owner.read_exact(&mut bytes).unwrap(); bytes
    }).await;
    rig.owner=owner;
    assert_eq!(&bytes,b"generated audio");
    let (send,receive)=oneshot::channel();
    tokio::spawn(async move {send.send(()).unwrap();});
    receive.await.unwrap();
    drop(rig.owner);rig.task.await.unwrap();assert!(request.is_closed());
}
