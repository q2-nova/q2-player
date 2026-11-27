//! Coalesce routine queue saves without delaying a newly acquired network.
use tokio::{sync::Notify, time::{Instant, sleep_until}};

pub(super) async fn coalesce(wake: &Notify, urgent: impl Fn() -> Option<bool>) -> Option<bool> {
    let deadline = Instant::now() + super::SYNC_COALESCE;
    loop {
        if urgent()? { return Some(true); }
        tokio::select! {
            // Routine kicks do not extend the original batching deadline.
            () = sleep_until(deadline) => return urgent(),
            () = wake.notified() => {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::{Arc, atomic::{AtomicBool, Ordering}}, time::Duration};

    #[tokio::test(start_paused = true)]
    async fn a_network_kick_interrupts_an_existing_coalescing_window() {
        let wake = Arc::new(Notify::new());
        let urgent = Arc::new(AtomicBool::new(false));
        let (task_wake, task_urgent) = (wake.clone(), urgent.clone());
        let task = tokio::spawn(async move {
            coalesce(&task_wake, || Some(task_urgent.swap(false, Ordering::Relaxed))).await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        let at = Instant::now();
        urgent.store(true, Ordering::Relaxed);
        wake.notify_one();
        assert_eq!(task.await.unwrap(), Some(true));
        assert_eq!(Instant::now(), at, "network readiness does not wait for the old three-second timer");
    }

    #[tokio::test(start_paused = true)]
    async fn ordinary_kicks_keep_one_fixed_deadline_and_initial_urgency_is_retained() {
        let wake = Arc::new(Notify::new());
        let task_wake = wake.clone();
        let start = Instant::now();
        let task = tokio::spawn(async move { coalesce(&task_wake, || Some(false)).await });
        tokio::task::yield_now().await;
        for _ in 0..2 {
            tokio::time::advance(Duration::from_secs(1)).await;
            wake.notify_one();
            tokio::task::yield_now().await;
            assert!(!task.is_finished());
        }
        assert_eq!(task.await.unwrap(), Some(false));
        assert_eq!(Instant::now() - start, super::super::SYNC_COALESCE);
        let now = Instant::now();
        assert_eq!(coalesce(&wake, || Some(true)).await, Some(true));
        assert_eq!(Instant::now(), now);
    }

    #[tokio::test(start_paused = true)]
    async fn retiring_the_owner_ends_the_coalescing_wait() {
        let wake = Arc::new(Notify::new());
        let alive = Arc::new(AtomicBool::new(true));
        let (task_wake, task_alive) = (wake.clone(), alive.clone());
        let task = tokio::spawn(async move {
            coalesce(&task_wake, || task_alive.load(Ordering::Relaxed).then_some(false)).await
        });
        tokio::task::yield_now().await;
        let now = Instant::now();
        alive.store(false, Ordering::Relaxed);
        wake.notify_one();
        assert_eq!(task.await.unwrap(), None);
        assert_eq!(Instant::now(), now);
    }
}
