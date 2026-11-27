//! In-memory ordering of spoken Download/Remove actions, registered before a
//! background job is queued. The latest action survives until every older job
//! for that logical item has retired; completing a newer job cannot revive one.
use std::collections::HashMap;
use std::sync::{Arc,Mutex,Weak};
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Default)]
pub(super) struct Registry(Mutex<State>,tokio::sync::Notify);
#[derive(Default)]
struct State { active:usize, items:HashMap<String,Weak<AtomicBool>> }
pub struct Intent {
    registry:Arc<Registry>, current:Arc<AtomicBool>,
    pub(super) logical:String,
}
impl Registry {
    // A worker may claim only an idle identity. It must never supersede a
    // newer user Remove or Download that has registered ahead of its DB job.
    pub(super) fn claim(self:&Arc<Self>, logical:String)->Option<Intent> {
        let mut state=self.0.lock().unwrap();
        if state.active>=32 || state.items.get(&logical).and_then(Weak::upgrade).is_some() { return None; }
        let current=Arc::new(AtomicBool::new(true));
        state.items.insert(logical.clone(),Arc::downgrade(&current)); state.active+=1;
        Some(Intent { registry:self.clone(), current, logical })
    }
    pub(super) fn begin(self:&Arc<Self>,logical:String)->Result<Intent,String> {
        let mut state=self.0.lock().unwrap();
        state.items.retain(|_,version|version.strong_count()>0);
        if state.active>=32 {return Err("Too many pending download actions; try again shortly".into());}
        // A token only transitions true -> false. Retiring its successor can
        // never make an older job current again, even after the map is pruned.
        if let Some(previous)=state.items.get(&logical).and_then(Weak::upgrade) {
            previous.store(false,Ordering::Release);
        }
        let current=Arc::new(AtomicBool::new(true));
        state.items.insert(logical.clone(),Arc::downgrade(&current));state.active+=1;
        self.1.notify_waiters();
        Ok(Intent {registry:self.clone(),current,logical})
    }
}
impl Intent {
    /// Publication/removal checks this after taking offline_art_gate. Registering
    /// a new action never waits for card IO; the newer action will take that
    /// same gate before its final mutation.
    pub(super) fn current(&self)->bool {self.current.load(Ordering::Acquire)}
    pub(super) async fn superseded(&self) {
        loop {
            let changed=self.registry.1.notified();tokio::pin!(changed);changed.as_mut().enable();
            if !self.current() {return;}
            changed.await;
        }
    }
    pub(super) fn check(&self)->Result<(),String> {self.current().then_some(()).ok_or_else(||"Download action superseded".into())}
}
impl Drop for Intent {fn drop(&mut self) {self.registry.0.lock().unwrap().active-=1;}}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn latest_action_remains_authoritative_after_it_completes() {
        let registry=Arc::new(Registry::default());let old=registry.begin("book".into()).unwrap();
        {let newer=registry.begin("book".into()).unwrap();assert!(!old.current());assert!(newer.current());}
        assert!(!old.current());let newest=registry.begin("book".into()).unwrap();assert!(newest.current());assert!(!old.current());
    }
    #[test]
    fn superseded_wait_observes_changes_before_registration_and_after_poll() {
        for already_changed in [false,true] {
            let registry=Arc::new(Registry::default());let old=registry.begin("book".into()).unwrap();
            if already_changed {drop(registry.begin("book".into()).unwrap());}
            let (done,finished)=std::sync::mpsc::channel();
            let pending=crate::rt::io().spawn(async move {old.superseded().await;done.send(()).unwrap();});
            if !already_changed {
                let (pulse,pulsed)=std::sync::mpsc::channel();
                crate::rt::io().spawn(async move {pulse.send(()).unwrap();});
                pulsed.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
                assert!(!pending.is_finished());
                drop(registry.begin("book".into()).unwrap());
            }
            finished.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        }
    }

    #[test]
    fn bounded_jobs_prune_retired_items_and_never_revive_old_tokens() {
        let registry=Arc::new(Registry::default());let mut rng=fastrand::Rng::with_seed(0xD0_2026);
        let mut jobs:Vec<Intent>=Vec::new();
        for _ in 0..10000 {
            if !jobs.is_empty() && rng.bool() {jobs.swap_remove(rng.usize(..jobs.len()));}
            let id=format!("book-{}",rng.usize(..9));
            let result=registry.begin(id.clone());
            if jobs.len()==32 {assert!(result.is_err());} else {
                let job=result.unwrap();assert!(job.current());
                assert!(jobs.iter().filter(|job|job.logical==id).all(|job|!job.current()));jobs.push(job);
            }
            assert!(registry.0.lock().unwrap().items.len()<=32);
        }
        drop(jobs);drop(registry.begin("final".into()).unwrap());assert_eq!(registry.0.lock().unwrap().items.len(),1);
    }
}
