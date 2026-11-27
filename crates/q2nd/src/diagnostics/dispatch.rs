//! Bounded, event-driven persistence on the existing finite worker pool.
use super::{Blocking, FAILURES, Report, State, WAKE, retain};
use std::{collections::VecDeque, sync::{Arc, atomic::Ordering}};

const LIMIT: usize = 64;
const WINDOW_US: u64 = 60_000_000;
#[derive(Default)]
pub(super) struct Pending {
    queue: VecDeque<Report>,
    running: bool,
    window: u64,
    admitted: usize,
}
impl Pending {
    /// Preserve a scheduled worker's ownership and the process-wide rate cap.
    pub(super) fn clear(&mut self) { self.queue.clear(); }
    fn admit(&mut self, report: Report) -> bool {
        let time = report.monotonic_us;
        if time.saturating_sub(self.window) >= WINDOW_US {
            self.window = time;
            self.admitted = 0;
        }
        if self.queue.len() >= LIMIT || self.admitted >= LIMIT { return false; }
        self.admitted += 1;
        // Coalesce only adjacent distributions captured in exactly the same
        // context. Never relabel an old observation after configuration changes.
        if let Some(last) = self.queue.back_mut() {
            if last.context == report.context && last.process == report.process
                && last.build == report.build && last.boot == report.boot
                && report.monotonic_us >= last.monotonic_us {
                if let (Some(super::crash::Event::Distribution {metric, sketch, last_us}),
                        Some(super::crash::Event::Distribution {metric: next, sketch: incoming, last_us: end}))
                    = (&mut last.event, &report.event) {
                    if metric == next && *end >= *last_us && sketch.merge(incoming) {
                        *last_us = *end;
                        last.dropped_reports = last.dropped_reports.max(report.dropped_reports);
                        return true;
                    }
                }
            }
        }
        self.queue.push_back(report);
        true
    }
}

/// Caller holds the capture boundary lock until this admission returns.
pub(super) fn submit(state: &Arc<State>, report: Report) -> bool {
    let Ok(mut pending) = state.health.try_lock() else {
        FAILURES.fetch_add(1, Ordering::Relaxed); return false;
    };
    if !pending.admit(report) { FAILURES.fetch_add(1, Ordering::Relaxed); return false; }
    if !pending.running {
        pending.running = true;
        let state = state.clone();
        crate::rt::jobs().run_isolated(move |blocking| drain(blocking, &state));
    }
    true
}

fn drain(blocking: &Blocking, state: &Arc<State>) {
    // At most one finite batch per scheduled job. Capture-before-queue order
    // matches admission and reconfiguration; no executor waits for these writes.
    for _ in 0..LIMIT {
        let capture = state.capture.lock().unwrap();
        let report = {
            let mut pending = state.health.lock().unwrap();
            if !capture.enabled { pending.clear(); }
            match pending.queue.pop_front() {
                Some(report) => report,
                None => { pending.running = false; return; }
            }
        };
        // Keep the boundary lock through persistence: disable cannot acknowledge
        // until a previous write has finished and the optional outbox is purged.
        if retain(blocking, &state.directory, &report).is_err() {
            FAILURES.fetch_add(1, Ordering::Relaxed);
        } else { WAKE.notify_one(); }
        drop(capture);
    }
    let mut pending = state.health.lock().unwrap();
    if pending.queue.is_empty() { pending.running = false; }
    else {
        let state = state.clone();
        crate::rt::jobs().run_isolated(move |blocking| drain(blocking, &state));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::{Context, ProcessIdentity, Stream};
    fn report(time: u64) -> Report {
        Report {id: super::super::uuid(),build:"a".repeat(64),boot:"b".repeat(36),
            runtime:"process".into(),kind:"health".into(),thread:0,task:0,poll:0,
            monotonic_us:time,elapsed_us:0,dropped_reports:0,frames:vec![],stack_status:"not_requested".into(),
            context:Context {stream:Stream::AutomatedTest,labels:vec!["automated-test".into()],..Context::default()},
            process:ProcessIdentity {pid:1,start_ticks:1},event:None}
    }
    #[test]
    fn distributions_coalesce_only_within_capture_context_and_keep_count_and_tail() {
        fn wake(time:u64,value:u64)->Report {
            let mut row=report(time);
            row.event=Some(super::super::crash::Event::Distribution {
                metric:super::super::health::Metric::InputToPresent,
                sketch:super::super::ddsketch::Sketch::sample(value).unwrap(),last_us:time});
            row
        }
        let mut pending=Pending::default();
        assert!(pending.admit(wake(1,100)));
        assert!(pending.admit(wake(2,9_000_000)));
        assert_eq!(pending.queue.len(),1);
        if let Some(super::super::crash::Event::Distribution {sketch,last_us,..})=&pending.queue[0].event {
            assert_eq!((sketch.count,sketch.max_us,*last_us),(2,9_000_000,2));
        } else {panic!("distribution missing");}
        let mut other=wake(3,200);other.context.stream=Stream::Production;other.context.labels.clear();
        assert!(pending.admit(other));assert_eq!(pending.queue.len(),2);
    }

    #[test]
    fn rate_and_capacity_remain_bounded_across_clears_and_time_regressions() {
        let mut p=Pending::default();
        for _ in 0..LIMIT { assert!(p.admit(report(1))); }
        assert!(!p.admit(report(2)));
        p.clear();
        assert!(!p.admit(report(0)), "clear cannot reset the rate budget");
        assert!(p.admit(report(WINDOW_US)));
        assert_eq!(p.queue.len(),1);
        assert_eq!(p.queue[0].context.stream,Stream::AutomatedTest);
        for _ in 1..LIMIT {assert!(p.admit(report(WINDOW_US)));}
        assert!(!p.admit(report(2*WINDOW_US)), "new time window cannot bypass queue capacity");
    }
}
