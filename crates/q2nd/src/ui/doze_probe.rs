//! One-shot generated observations. This owner never waits or starts a worker.
use crate::{audio::player::Cmd, numeric_probe::{DozeBoundary, MusicCursor}};
use std::sync::{Arc, OnceLock};
use tokio::sync::oneshot::Sender;
use std::time::{Duration, Instant};

struct Pending {
    deadline: Instant,
    context: Arc<OnceLock<DozeBoundary>>,
    reply: Sender<Result<MusicCursor, &'static str>>,
}

#[derive(Default)]
pub(super) struct DozeProbe {
    ordinal: u64,
    pending: Option<Pending>,
}

impl DozeProbe {
    pub(super) fn armed(&self) -> bool { self.pending.is_some() }

    pub(super) fn arm(&mut self, now: Instant, deadline: Instant,
        context: Arc<OnceLock<DozeBoundary>>, reply: Sender<Result<MusicCursor, &'static str>>) {
        self.expire(now);
        let error = if deadline <= now { Some("doze observation arm expired") }
            else if self.pending.is_some() { Some("doze observation already armed") }
            else if context.get().is_some() { Some("doze observation context already used") }
            else { None };
        if let Some(error) = error { let _ = reply.send(Err(error)); }
        else { self.pending = Some(Pending { deadline, context, reply }); }
    }

    pub(super) fn expire(&mut self, now: Instant) {
        if self.pending.as_ref().is_some_and(|pending| now >= pending.deadline) {
            let _ = self.pending.take().unwrap().reply.send(Err("doze observation arm expired"));
        }
    }

    /// Call only for a confirmed false-to-true automatic *intent* transition.
    /// Every transition advances the ordinal, including unobserved ones.
    pub(super) fn entered(&mut self, now: Instant, requested_us: u64,
        confirmed_us: u64, estimated_ahead_ms: u64) -> Option<Cmd> {
        self.expire(now);
        let ordinal = self.ordinal.checked_add(1);
        self.ordinal = ordinal.unwrap_or(u64::MAX);
        let pending = self.pending.take()?;
        let Some(ordinal) = ordinal else {
            let _ = pending.reply.send(Err("doze observation ordinal exhausted"));
            return None;
        };
        if requested_us == 0 || confirmed_us < requested_us {
            let _ = pending.reply.send(Err("doze observation intent clock refused"));
            return None;
        }
        let boundary = DozeBoundary { ordinal, requested_us, confirmed_us, estimated_ahead_ms };
        if pending.context.set(boundary).is_err() {
            let _ = pending.reply.send(Err("doze observation context already used"));
            return None;
        }
        Some(Cmd::ObserveMusicCursor {
            deadline: pending.deadline.min(now + Duration::from_secs(2)), reply: pending.reply,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    #[test]
    fn doze_probe_preserves_missing_edges_and_single_owner_deadline() {
        let now = Instant::now();
        let mut probe = DozeProbe::default();
        assert!(probe.entered(now, 0, 0, 100).is_none());
        let context = Arc::new(OnceLock::new());
        let (reply, mut received) = oneshot::channel();
        probe.arm(now, now + Duration::from_secs(10), context.clone(), reply);
        let Some(Cmd::ObserveMusicCursor { deadline, reply }) = probe.entered(now, 100, 150, 999) else { panic!("owner request missing") };
        assert_eq!(deadline, now + Duration::from_secs(2));
        assert_eq!(context.get().unwrap(), &DozeBoundary { ordinal:2, requested_us:100, confirmed_us:150, estimated_ahead_ms:999 });
        assert!(!probe.armed());
        assert!(probe.entered(now, 200, 210, 998).is_none());
        drop(reply);
        assert!(matches!(received.try_recv(), Err(oneshot::error::TryRecvError::Closed)));
    }

    #[test]
    fn doze_probe_refuses_duplicate_expired_and_reused_arms() {
        let now = Instant::now();
        let end = now + Duration::from_secs(1);
        let mut probe = DozeProbe::default();
        let context = Arc::new(OnceLock::new());
        let (reply, mut first) = oneshot::channel();
        probe.arm(now, end, context.clone(), reply);
        let (reply, mut duplicate) = oneshot::channel();
        probe.arm(now, end, Arc::new(OnceLock::new()), reply);
        assert_eq!(duplicate.try_recv().unwrap(), Err("doze observation already armed"));
        assert!(probe.entered(end, 100, 150, 999).is_none());
        assert_eq!(first.try_recv().unwrap(), Err("doze observation arm expired"));
        assert!(context.get().is_none());
        let (reply, mut expired) = oneshot::channel();
        probe.arm(end, end, context.clone(), reply);
        assert_eq!(expired.try_recv().unwrap(), Err("doze observation arm expired"));
        context.set(DozeBoundary {ordinal:1,requested_us:1,confirmed_us:2,estimated_ahead_ms:3}).unwrap();
        let (reply, mut reused) = oneshot::channel();
        probe.arm(now, end, context, reply);
        assert_eq!(reused.try_recv().unwrap(), Err("doze observation context already used"));
    }

    #[test]
    fn doze_probe_refuses_clock_and_counter_failures_and_drops_waiter() {
        let now = Instant::now();
        for (ordinal, request, confirm) in [(0,0,1),(0,2,1),(u64::MAX,1,2)] {
            let mut probe = DozeProbe {ordinal,pending:None};
            let context = Arc::new(OnceLock::new());
            let (reply, mut received) = oneshot::channel();
            probe.arm(now, now + Duration::from_secs(1), context.clone(), reply);
            assert!(probe.entered(now, request, confirm, 0).is_none());
            assert!(received.try_recv().unwrap().is_err());
            assert!(context.get().is_none());
        }
        let mut probe = DozeProbe::default();
        let (reply, mut received) = oneshot::channel();
        probe.arm(now, now + Duration::from_secs(1), Arc::new(OnceLock::new()), reply);
        drop(probe);
        assert!(matches!(received.try_recv(), Err(oneshot::error::TryRecvError::Closed)));
    }
}
