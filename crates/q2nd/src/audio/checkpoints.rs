#![forbid(unsafe_code)]
//! One persistence owner, one coalesced pending periodic save, explicit barriers.
//! Periodic FULL-sync I/O must never execute on the PCM feeder. Pause/export/
//! shutdown still wait for durability, serialized after any in-flight save.
use super::player::TrackSource;
use std::sync::{Arc, Mutex, mpsc};
use std::{future::Future,pin::Pin};
type Shared=Arc<(Mutex<State>,tokio::sync::Notify)>;
type Work=Pin<Box<dyn Future<Output=()>+Send>>;
use subsonic::Song;

// Rust drops struct fields in declaration order. The entire coordinator
// future (including unpolled captures) must retire before its completion
// guard wakes the synchronous audio owner. Async capture drop order cannot
// provide this boundary: an unpolled future may drop the guard first.
struct OwnedWork { work:Work, _exit:WorkerExit }
impl Future for OwnedWork {
    type Output=();
    fn poll(self:Pin<&mut Self>,cx:&mut std::task::Context<'_>)->std::task::Poll<()> {
        self.get_mut().work.as_mut().poll(cx)
    }
}

struct Job {
    song: Song,
    position: u64,
    session: Option<super::listening::Session>,
    generation: u64,
    ack: Option<mpsc::Sender<Result<(), String>>>,
}
#[derive(Default)]
struct State {
    pending: Option<Job>,
    error: Option<(u64, String)>,
    stop: bool,
    closed: bool,
}
// A callback can panic independently of the queue lock. Closing the queue on
// every worker exit releases retained barrier senders and rejects future work.
struct WorkerExit {shared:Shared,done:Option<tokio::sync::oneshot::Sender<()>>}
impl Drop for WorkerExit {
    fn drop(&mut self) {
        let mut state = self.shared.0.lock().unwrap_or_else(|poison| poison.into_inner());
        state.closed = true;
        if let Some(job) = state.pending.take() {
            let error = "Listening location writer stopped before saving".to_string();
            if let Some(ack) = job.ack {
                let _ = ack.send(Err(error));
            } else {
                state.error = Some((job.generation, error));
            }
        }
        self.shared.1.notify_waiters();
        if let Some(done)=self.done.take() {let _=done.send(());}
    }
}
pub struct Writer {
    shared: Shared,
    source: Arc<dyn TrackSource>,
    session: Option<super::listening::Session>,
    task: Option<tokio::task::JoinHandle<()>>,
    finished: Option<tokio::sync::oneshot::Receiver<()>>,
}
impl Writer {
    pub fn new(source: Arc<dyn TrackSource>) -> Self {
        let shared = Arc::new((Mutex::new(State::default()), tokio::sync::Notify::new()));
        Self { shared, source, session: None, task: None, finished:None }
    }

    pub fn set_session(&mut self, session: Option<super::listening::Session>) { self.session = session; }

    pub fn is_prepared(&self) -> bool { self.task.is_some() }

    #[cfg(test)]
    pub(super) fn owner_id(&self) -> Option<tokio::task::Id> {
        self.task.as_ref().map(|task| task.id())
    }

    /// The audio owner admits one coordinator before playback. Submission
    /// only replaces the bounded pending snapshot; it never waits for storage.
    pub fn prepare_before_playback(&mut self) -> Result<(), String> {
        self.prepare_with(|work|Ok(crate::rt::io().spawn(work)))
    }

    fn prepare_with(&mut self,start:impl FnOnce(Work)->std::io::Result<tokio::task::JoinHandle<()>>)->Result<(),String> {
        if self.is_prepared() {return Ok(());}
        self.shared.0.lock().unwrap().closed=false;
        let worker=self.shared.clone();let source=self.source.clone();
        let (done,finished)=tokio::sync::oneshot::channel();
        // Preconstructed so panic, abort and unpolled destruction reject
        // pending barriers and release a synchronous owner joining shutdown.
        let exit=WorkerExit {shared:worker.clone(),done:Some(done)};
        let work=OwnedWork {work:Box::pin(run(worker,source)),_exit:exit};
        self.task=Some(start(Box::pin(work)).map_err(|_|"Unable to prepare listening location writer".to_string())?);
        self.finished=Some(finished);
        Ok(())
    }

    pub fn periodic(&self, song: &Song, position: u64, generation: u64) {
        let mut state = self.shared.0.lock().unwrap();
        if !self.is_prepared() || state.closed {
            state.error = Some((
                generation,
                "Listening location writer stopped before saving".into(),
            ));
            return;
        }
        // Only the audio owner submits/waits. A synchronous barrier cannot be
        // overwritten by a periodic request from a different producer.
        assert!(!state.pending.as_ref().is_some_and(|job| job.ack.is_some()));
        state.pending = Some(Job {
            song: song.clone(),
            position,
            session: self.session.clone(),
            generation,
            ack: None,
        });
        self.shared.1.notify_one();
    }

    pub fn checkpoint(&self, song: &Song, position: u64, generation: u64) -> Result<(), String> {
        let (ack, receiver) = mpsc::channel();
        {
            let mut state = self.shared.0.lock().unwrap();
            if !self.is_prepared() || state.closed {
                return Err("Listening location writer stopped before saving".into());
            }
            assert!(!state.pending.as_ref().is_some_and(|job| job.ack.is_some()));
            state.pending = Some(Job {
                song: song.clone(),
                position,
            session: self.session.clone(),
                generation,
                ack: Some(ack),
            });
            self.shared.1.notify_one();
        }
        receiver
            .recv()
            .unwrap_or_else(|_| Err("Listening location writer stopped before saving".into()))
    }

    pub fn error(&self, generation: u64) -> Option<String> {
        self.shared
            .0
            .lock()
            .unwrap()
            .error
            .take()
            .and_then(|(saved, error)| (saved == generation).then_some(error))
    }
}
async fn run(worker:Shared,source:Arc<dyn TrackSource>) {
    loop {
        let notified=worker.1.notified();
        let job={
            let mut state=worker.0.lock().unwrap();
            match state.pending.take() {
                Some(job)=>Some(job),
                None if state.stop=>return,
                None=>None,
            }
        };
        let Some(job)=job else {notified.await;continue;};
        // Production awaits the DB owner; no pool worker parks behind SQLite.
        // Keep the job/ack until the accepted durable write has completed.
        let result = source.clone().checkpoint_async(job.song.clone(), job.position, job.session.clone()).await;
        {
            let mut state=worker.0.lock().unwrap();
            match &result {
                Ok(()) if state.error.as_ref().is_some_and(|(generation,_)|*generation==job.generation)=>state.error=None,
                Err(error) if job.ack.is_none()=>state.error=Some((job.generation,error.clone())),
                _=>{},
            }
        }
        if let Some(ack)=job.ack {let _=ack.send(result);}
    }
}
impl Drop for Writer {
    fn drop(&mut self) {
        self.shared.0.lock().unwrap().stop=true;
        self.shared.1.notify_one();
        // The PCM owner is synchronous. Join the coordinator only after it
        // drains the in-flight write and final pending checkpoint. Never abort
        // a future which owns an unjoined durability job.
        if let Some(finished)=self.finished.take() {let _=finished.blocking_recv();}
        self.task.take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::player::OpenedTrack;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;
    struct Delayed {
        entered: mpsc::Sender<()>,
        release: Mutex<mpsc::Receiver<()>>,
        count: AtomicUsize,
        positions: Mutex<Vec<u64>>,
        fail: AtomicBool,
        panic: AtomicBool,
    }
    impl TrackSource for Delayed {
        fn open(&self, _blocking: &crate::rt::Blocking, _: &Song, _: &Arc<crate::audio::interrupt::Interrupt>) -> Result<OpenedTrack, String> {
            unreachable!()
        }
        fn upcoming(&self, _: &[Song]) {}
        fn now_playing(&self, _: &Song) {}
        fn played(&self, _: &Song, _: u64) {}
        fn save_queue(&self, _: &[Song], _: usize, _: u64) {}
        fn checkpoint(&self, _: &Song, position: u64) -> Result<(), String> {
            assert!(crate::rt::on_pool(),"storage runs off the I/O executor and PCM owner");
            if self.count.fetch_add(1, Ordering::Relaxed) == 0 {
                self.entered.send(()).unwrap();
                self.release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(3))
                    .map_err(|_| "Generated delayed save timed out")?;
            }
            assert!(
                !self.panic.load(Ordering::Relaxed),
                "Generated checkpoint worker panic"
            );
            self.positions.lock().unwrap().push(position);
            if self.fail.load(Ordering::Relaxed) {
                Err("Generated fsync failure".into())
            } else {
                Ok(())
            }
        }
    }
    fn delayed() -> (Arc<Delayed>, mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (entered, wait) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        (
            Arc::new(Delayed {
                entered,
                release: Mutex::new(gate),
                count: AtomicUsize::new(0),
                positions: Mutex::new(Vec::new()),
                fail: AtomicBool::new(false),
                panic: AtomicBool::new(false),
            }),
            wait,
            release,
        )
    }
    fn prepared(source: Arc<dyn TrackSource>) -> Writer {
        let mut writer = Writer::new(source);
        writer.prepare_before_playback().unwrap();
        writer
    }

    #[test]
    fn unused_owner_rejects_submissions_without_spawning_and_drops_cleanly() {
        let (source, _, _) = delayed();
        let writer = Writer::new(source.clone());
        assert!(!writer.is_prepared());
        writer.periodic(&Song::default(), 7, 1);
        assert!(writer.error(1).is_some());
        assert!(writer.checkpoint(&Song::default(), 8, 1).is_err());
        assert!(!writer.is_prepared());
        assert!(writer.shared.0.lock().unwrap().pending.is_none());
        drop(writer);
        assert_eq!(source.count.load(Ordering::Relaxed), 0);
        assert_eq!(Arc::strong_count(&source), 1);
    }

    #[test]
    fn completion_follows_capture_destruction_even_without_a_poll() {
        struct Probe {
            done:tokio::sync::oneshot::Receiver<()>,
            released:Arc<AtomicBool>,
        }
        impl Future for Probe {
            type Output=();
            fn poll(self:Pin<&mut Self>,_:&mut std::task::Context<'_>)->std::task::Poll<()> {
                std::task::Poll::Pending
            }
        }
        impl Drop for Probe {
            fn drop(&mut self) {
                assert!(matches!(self.done.try_recv(),Err(tokio::sync::oneshot::error::TryRecvError::Empty)),
                    "completion must not precede owned capture destruction");
                self.released.store(true,Ordering::Release);
            }
        }
        for poll in [false,true] {
            let shared=Arc::new((Mutex::new(State::default()),tokio::sync::Notify::new()));
            let (done,wait)=tokio::sync::oneshot::channel();
            let released=Arc::new(AtomicBool::new(false));
            let mut work=Box::pin(OwnedWork {
                work:Box::pin(Probe {done:wait,released:released.clone()}),
                _exit:WorkerExit {shared:shared.clone(),done:Some(done)},
            });
            if poll {
                let mut cx=std::task::Context::from_waker(std::task::Waker::noop());
                assert!(work.as_mut().poll(&mut cx).is_pending());
            }
            drop(work);
            assert!(released.load(Ordering::Acquire));
            assert!(shared.0.lock().unwrap().closed);
        }
    }

    #[test]
    fn unpolled_or_idle_aborted_coordinator_rejects_barriers_and_joins() {
        for unpolled in [false,true] {
            let (source,_,_)=delayed();
            let mut writer=Writer::new(source.clone());
            if unpolled {
                writer.prepare_with(|work|Ok(crate::rt::io().spawn(async move {drop(work);}))).unwrap();
            } else {
                writer.prepare_before_playback().unwrap();writer.task.as_ref().unwrap().abort();
            }
            let end=std::time::Instant::now()+Duration::from_secs(2);
            while !writer.shared.0.lock().unwrap().closed {
                assert!(std::time::Instant::now()<end);std::thread::yield_now();
            }
            assert!(writer.checkpoint(&Song::default(),1,1).is_err());
            drop(writer);
            assert_eq!(source.count.load(Ordering::Relaxed),0);
            assert_eq!(Arc::strong_count(&source),1);
        }
    }

    #[test]
    fn failed_preparation_can_retry_and_never_acknowledges_false_durability() {
        let (source, _entered, release) = delayed();
        let mut writer = Writer::new(source.clone());
        assert!(writer.prepare_with(|_| Err(std::io::Error::other("Injected spawn failure"))).is_err());
        assert!(!writer.is_prepared());
        assert!(writer.checkpoint(&Song::default(), 7, 1).is_err());
        writer.prepare_before_playback().unwrap();
        let id = writer.owner_id();
        writer.prepare_with(|_| panic!("already prepared owner must not spawn")).unwrap();
        assert_eq!(writer.owner_id(), id);
        release.send(()).unwrap();
        writer.checkpoint(&Song::default(), 8, 1).unwrap();
        assert_eq!(*source.positions.lock().unwrap(), vec![8]);
    }

    #[test]
    fn first_prepared_submit_does_not_wait_for_storage_and_drop_flushes_it() {
        let (source, entered, release) = delayed();
        let began = std::time::Instant::now();
        let writer = prepared(source.clone());
        let prepared_us = began.elapsed().as_micros();
        let submitted = std::time::Instant::now();
        writer.periodic(&Song::default(), 11, 1);
        let first_submit_us = submitted.elapsed().as_micros();
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        let (send, done) = mpsc::channel();
        let dropping = std::thread::spawn(move || { drop(writer); send.send(()).unwrap(); });
        assert!(done.recv_timeout(Duration::from_millis(20)).is_err());
        release.send(()).unwrap();
        done.recv_timeout(Duration::from_secs(2)).unwrap();
        dropping.join().unwrap();
        assert_eq!(*source.positions.lock().unwrap(), vec![11]);
        println!("Q2_CHECKPOINT_COST prepared_us={prepared_us} first_submit_us={first_submit_us}");
    }

    #[test]
    fn shutdown_requested_during_exclusive_preparation_joins_the_created_owner() {
        let (source, _, _) = delayed();
        let mut writer = Writer::new(source.clone());
        let (entered, begun) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let quit = Arc::new(AtomicBool::new(false));
        let stopping = quit.clone();
        let preparing = std::thread::spawn(move || {
            writer.prepare_with(|work| {
                entered.send(()).unwrap();
                gate.recv_timeout(Duration::from_secs(2)).unwrap();
                Ok(crate::rt::io().spawn(work))
            }).unwrap();
            assert!(stopping.load(Ordering::Acquire));
            // Exclusive preparation has returned: shutdown can now own/drop it.
            drop(writer);
        });
        begun.recv_timeout(Duration::from_secs(2)).unwrap();
        quit.store(true, Ordering::Release);
        release.send(()).unwrap();
        preparing.join().unwrap();
        assert_eq!(source.count.load(Ordering::Relaxed), 0);
        assert_eq!(Arc::strong_count(&source), 1);
    }

    #[test]
    fn delayed_periodic_fsync_does_not_block_feeding_and_barrier_wins() {
        let (source, entered, release) = delayed();
        let writer = Arc::new(prepared(source.clone()));
        let song = Song::default();
        writer.periodic(&song, 1, 4);
        entered.recv_timeout(Duration::from_secs(1)).unwrap();
        // The persistence owner is still blocked. Arbitrarily many PCM-side
        // submissions complete now, with exactly one retained pending snapshot.
        for position in 2..=10000 {
            writer.periodic(&song, position, 4);
        }
        assert_eq!(
            writer
                .shared
                .0
                .lock()
                .unwrap()
                .pending
                .as_ref()
                .unwrap()
                .position,
            10000
        );
        let (copy, song) = (writer.clone(), song.clone());
        let (done, result) = mpsc::channel();
        let barrier = std::thread::spawn(move || {
            done.send(copy.checkpoint(&song, 10001, 4)).unwrap();
        });
        for _ in 0..1000 {
            if writer
                .shared
                .0
                .lock()
                .unwrap()
                .pending
                .as_ref()
                .is_some_and(|job| job.ack.is_some())
            {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let queued = writer
            .shared
            .0
            .lock()
            .unwrap()
            .pending
            .as_ref()
            .is_some_and(|job| job.ack.is_some());
        let waiting = result.try_recv().is_err();
        release.send(()).unwrap();
        assert!(queued && waiting);
        assert_eq!(result.recv_timeout(Duration::from_secs(1)).unwrap(), Ok(()));
        barrier.join().unwrap();
        assert_eq!(*source.positions.lock().unwrap(), vec![1, 10001]);
        assert_eq!(writer.error(4), None);
    }
    #[test]
    #[cfg_attr(panic = "abort", ignore = "recovers from a panic; a panic = \"abort\" build, as on the Q2, aborts")]
    fn worker_panic_rejects_pending_and_future_barriers_instead_of_retaining_ack_forever() {
        let (source, entered, release) = delayed();
        source.panic.store(true, Ordering::Relaxed);
        let writer = Arc::new(prepared(source));
        let song = Song::default();
        writer.periodic(&song, 1, 4);
        entered.recv_timeout(Duration::from_secs(1)).unwrap();
        let copy = writer.clone();
        let (done, result) = mpsc::channel();
        let barrier = std::thread::spawn(move || {
            done.send(copy.checkpoint(&song, 2, 4)).unwrap();
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !writer
            .shared
            .0
            .lock()
            .unwrap()
            .pending
            .as_ref()
            .is_some_and(|job| job.ack.is_some())
        {
            assert!(std::time::Instant::now() < deadline);
            std::thread::yield_now();
        }
        release.send(()).unwrap();
        assert!(
            result
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap_err()
                .contains("writer stopped")
        );
        barrier.join().unwrap();
        assert!(
            writer
                .checkpoint(&Song::default(), 3, 4)
                .unwrap_err()
                .contains("writer stopped")
        );
        writer.periodic(&Song::default(), 4, 4);
        assert!(writer.error(4).unwrap().contains("writer stopped"));
    }

    #[test]
    fn later_durable_success_clears_an_earlier_background_error_in_the_same_generation() {
        let (source, entered, release) = delayed();
        source.fail.store(true, Ordering::Relaxed);
        let writer = prepared(source.clone());
        let song = Song::default();
        writer.periodic(&song, 1, 4);
        entered.recv_timeout(Duration::from_secs(1)).unwrap();
        release.send(()).unwrap();
        assert!(writer.checkpoint(&song, 2, 4).is_err());
        source.fail.store(false, Ordering::Relaxed);
        assert_eq!(writer.checkpoint(&song, 3, 4), Ok(()));
        assert_eq!(writer.error(4), None);
    }

    #[test]
    fn stale_background_failure_is_not_published_to_successor_and_sync_errors_propagate() {
        let (source, entered, release) = delayed();
        source.fail.store(true, Ordering::Relaxed);
        let writer = prepared(source.clone());
        let song = Song::default();
        writer.periodic(&song, 123, 9);
        entered.recv_timeout(Duration::from_secs(1)).unwrap();
        release.send(()).unwrap();
        assert!(
            writer
                .checkpoint(&song, 456, 10)
                .unwrap_err()
                .contains("fsync failure")
        );
        assert_eq!(writer.error(10), None); // old-generation periodic failure
        source.fail.store(false, Ordering::Relaxed);
        assert_eq!(writer.checkpoint(&song, 789, 10), Ok(()));
        assert_eq!(*source.positions.lock().unwrap(), vec![123, 456, 789]);
    }
}
