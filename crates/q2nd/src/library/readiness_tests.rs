//! Controlled readiness transitions with a stopped clock and observable wakes.
use super::*;
use std::future::Future;
use std::task::{Context, Poll, Wake, Waker};

#[derive(Default)]
pub(super) struct Wakes(AtomicU32);
impl Wake for Wakes {
    fn wake(self: Arc<Self>) { self.0.fetch_add(1, Ordering::SeqCst); }
    fn wake_by_ref(self: &Arc<Self>) { self.0.fetch_add(1, Ordering::SeqCst); }
}
impl Wakes {
    pub(super) fn take(&self) -> u32 { self.0.swap(0, Ordering::SeqCst) }
}

fn library(tag: &str) -> Arc<Library> {
    let (lib, _) = Library::create(Paths::new(generated_test_directory(tag)), &Config::default(), Arc::new(|_| {}));
    lib.online.store(true, Ordering::Relaxed);
    let progress = Progress::new();
    lib.inflight.lock().unwrap().insert(("generated".into(), "raw".into()), Transfer {
        generation: lib.client_generation(), part: lib.paths.stream_cache().join("generated.part"),
        progress, cancel: Arc::default(), trace_id: 0, journal_id: 0,
        users: 0, orphaned: true, playback: true,
    });
    lib
}

#[test]
fn priority_wait_has_no_periodic_wake_and_connectivity_changes_wake_it() {
    for transition in 0..4 {
        let lib = library(&format!("priority-event-{transition}"));
        let runtime = tokio::runtime::Builder::new_current_thread().enable_time().start_paused(true).build().unwrap();
        runtime.block_on(async {
            let (_sender, mut receiver) = tokio::sync::watch::channel(None);
            let wakes = Arc::new(Wakes::default());
            let waker = Waker::from(wakes.clone());
            let mut cx = Context::from_waker(&waker);
            let wait = lib.wait_playback_priority(&mut receiver);
            tokio::pin!(wait);
            assert!(wait.as_mut().poll(&mut cx).is_pending());
            tokio::time::advance(Duration::from_millis(500)).await;
            assert_eq!(wakes.take(), 0, "no 250 ms readiness poll");
            match transition {
                0 => lib.set_online(false),
                1 => lib.set_offline_mode(true),
                2 => lib.set_radio_doze(true),
                _ => { drop(lib.configure_async(&Config::default())); },
            }
            assert!(wakes.take() > 0, "the state transition wakes the waiter");
            assert_eq!(wait.as_mut().poll(&mut cx), Poll::Ready(PriorityWait::Offline));
        });
    }
}

#[test]
fn priority_wait_observes_cancel_queue_close_and_one_fixed_deadline() {
    let lib = library("priority-cancel-deadline");
    let runtime = tokio::runtime::Builder::new_current_thread().enable_time().start_paused(true).build().unwrap();
    runtime.block_on(async {
        let (sender, mut receiver) = tokio::sync::watch::channel(None);
        let wakes = Arc::new(Wakes::default());
        let waker = Waker::from(wakes.clone());
        let mut cx = Context::from_waker(&waker);
        {
            let wait = lib.wait_playback_priority(&mut receiver);
            tokio::pin!(wait);
            assert!(wait.as_mut().poll(&mut cx).is_pending());
            for _ in 0..2 {
                tokio::time::advance(PLAYBACK_PRIORITY_WAIT / 3).await;
                lib.background_changed();
                assert!(wait.as_mut().poll(&mut cx).is_pending());
            }
            // Extra readiness notifications never renew the deadline.
            tokio::time::advance(PLAYBACK_PRIORITY_WAIT / 3).await;
            assert_eq!(wait.as_mut().poll(&mut cx), Poll::Ready(PriorityWait::Timeout));
        }
        {
            let wait = lib.wait_playback_priority(&mut receiver);
            tokio::pin!(wait);
            assert!(wait.as_mut().poll(&mut cx).is_pending());
            wakes.take();
            assert_eq!(lib.cancel_orphans(|_| false), 1);
            assert!(wakes.take() > 0);
            assert_eq!(wait.as_mut().poll(&mut cx), Poll::Ready(PriorityWait::Clear));
        }
        // Even a clear registry must not start work for an obsolete queue.
        sender.send_replace(Some(Vec::new()));
        assert_eq!(lib.wait_playback_priority(&mut receiver).await, PriorityWait::Newer);
        receiver.borrow_and_update();
        drop(sender);
        assert_eq!(lib.wait_playback_priority(&mut receiver).await, PriorityWait::Newer);
    });
}

#[test]
fn priority_queue_update_wakes_pending_wait_and_retains_latest_value() {
    let lib = library("priority-queue-event");
    let runtime = tokio::runtime::Builder::new_current_thread().enable_time().start_paused(true).build().unwrap();
    runtime.block_on(async {
        let (sender, mut receiver) = tokio::sync::watch::channel(None);
        let wakes = Arc::new(Wakes::default());
        let waker = Waker::from(wakes.clone());
        let mut cx = Context::from_waker(&waker);
        {
            let wait = lib.wait_playback_priority(&mut receiver);
            tokio::pin!(wait);
            assert!(wait.as_mut().poll(&mut cx).is_pending());
            sender.send_replace(Some(vec![Song { id: "first".into(), ..Song::default() }]));
            sender.send_replace(Some(vec![Song { id: "latest".into(), ..Song::default() }]));
            assert!(wakes.take() > 0);
            assert_eq!(wait.as_mut().poll(&mut cx), Poll::Ready(PriorityWait::Newer));
        }
        assert_eq!(receiver.borrow_and_update().as_ref().unwrap()[0].id, "latest");
    });
}
