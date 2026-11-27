#![forbid(unsafe_code)]
//! Command interruption and producer progress share one event at each reader.
//! Register before inspecting the predicate; a generation retained under the
//! event lock closes the predicate-to-sleep race without a polling timeout.
use std::sync::{
    Arc, Condvar, Mutex, Weak,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

#[derive(Debug, Default)]
pub struct Interrupt {
    set: AtomicBool,
    events: Mutex<Vec<Weak<Event>>>,
}
impl Interrupt {
    pub fn new(set: bool) -> Self {
        Self {
            set: AtomicBool::new(set),
            events: Mutex::new(Vec::new()),
        }
    }
    pub fn load(&self, ordering: Ordering) -> bool {
        self.set.load(ordering)
    }
    pub fn store(&self, set: bool, ordering: Ordering) {
        self.set.store(set, ordering);
        if set {
            // Event never calls back into Interrupt while holding its lock.
            // Keep only live weak registrations; old songs retain no resources.
            self.events.lock().unwrap().retain(|event| {
                if let Some(event) = event.upgrade() {
                    event.notify();
                    true
                } else {
                    false
                }
            });
        }
    }
    /// Generated ready-reader control: hold the two locks a lazy ready read
    /// must not acquire. The closure must return ownership before reader Drop.
    #[cfg(test)]
    pub(super) fn with_generated_locks<R>(&self, event: &Event, f: impl FnOnce() -> R) -> R {
        let _registry = self.events.lock().unwrap();
        let _generation = event.generation.lock().unwrap();
        f()
    }

    pub(super) fn register(&self, event: &Arc<Event>) {
        let mut events = self.events.lock().unwrap();
        events.retain(|event| event.strong_count() != 0);
        let target = Arc::downgrade(event);
        if !events.iter().any(|old| old.ptr_eq(&target)) {
            events.push(target);
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct Event {
    generation: Mutex<u64>,
    ready: Condvar,
}
impl Event {
    pub(super) fn version(&self) -> u64 {
        *self.generation.lock().unwrap()
    }
    pub(super) fn notify(&self) {
        let mut generation = self.generation.lock().unwrap();
        *generation = generation.wrapping_add(1);
        self.ready.notify_all();
    }
    /// The caller must release its data lock before waiting. The generation was
    /// sampled before inspecting data/cancellation, so either change wakes it.
    pub(super) fn wait_after(&self, version: u64, limit: Option<Duration>) -> std::io::Result<()> {
        super::source::wait_context()?;
        let generation = self.generation.lock().unwrap();
        if *generation != version {
            return Ok(());
        }
        match limit {
            Some(limit) => {
                drop(self.ready.wait_timeout(generation, limit).unwrap());
            }
            None => {
                drop(self.ready.wait(generation).unwrap());
            }
        }
        Ok(())
    }
}

/// A wait registration is lazy: ready-buffer reads acquire no registry or
/// generation lock. Its first wait only registers and samples a generation,
/// then returns so the caller rechecks its predicate before actually sleeping.
pub(super) struct Waiter<'a> {
    interrupt: &'a Interrupt,
    event: &'a Arc<Event>,
    version: Option<u64>,
}
impl<'a> Waiter<'a> {
    pub(super) fn new(interrupt: &'a Interrupt, event: &'a Arc<Event>) -> Self {
        Self {
            interrupt,
            event,
            version: None,
        }
    }
    pub(super) fn wait(&mut self, limit: Option<Duration>) -> std::io::Result<()> {
        super::source::wait_context()?;
        if let Some(version) = self.version {
            self.event.wait_after(version, limit)?;
        } else {
            self.interrupt.register(self.event);
        }
        self.version = Some(self.event.version());
        Ok(())
    }
}

/// Only the registered owner has a health deadline. An ordinary unregistered
/// decoder has no periodic cancellation poll; a truly stalled read must still
/// stop pulsing according to the reader's existing watchdog policy.
pub(super) fn health_interval() -> Option<Duration> {
    let interval = crate::health::wait(Duration::MAX);
    (interval != Duration::MAX).then_some(interval)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn lazy_waiter_rechecks_registration_and_retains_predicate_to_park_changes() {
        for cancellation in [false, true] {
            for phase in 0..3 {
                let (done, complete) = mpsc::channel();
                let worker = std::thread::spawn(move || {
                    let blocking = crate::rt::Blocking::dedicated_thread();
                    super::super::callback::with(&blocking, || {
                        let interrupt = Interrupt::new(false);
                        let event = Arc::new(Event::default());
                        let produced = AtomicBool::new(false);
                        let publish = || {
                            if cancellation {
                                interrupt.store(true, Ordering::Relaxed);
                            } else {
                                produced.store(true, Ordering::Relaxed);
                                event.notify();
                            }
                        };
                        let available = || {
                            interrupt.load(Ordering::Relaxed) || produced.load(Ordering::Relaxed)
                        };
                        // A ready read creates its waiter without either lock.
                        let registry = interrupt.events.lock().unwrap();
                        let generation = event.generation.lock().unwrap();
                        let mut wait = Waiter::new(&interrupt, &event);
                        assert!(wait.version.is_none());
                        drop(generation);
                        drop(registry);
                        assert_eq!(health_interval(), None);
                        if phase == 0 {
                            publish();
                        }
                        if !available() {
                            // First wait only registers. It must return even
                            // though neither data nor cancellation is ready.
                            wait.wait(None).unwrap();
                            assert_eq!(interrupt.events.lock().unwrap().len(), 1);
                            if phase == 1 {
                                publish();
                            }
                            if !available() {
                                assert_eq!(phase, 2);
                                publish(); // Change after predicate, before park.
                                wait.wait(None).unwrap();
                            }
                        }
                        assert!(available());
                    });
                    done.send(()).unwrap();
                });
                complete.recv_timeout(Duration::from_secs(3)).unwrap();
                worker.join().unwrap();
            }
        }
    }

    #[test]
    fn cancellation_before_registration_and_between_predicate_and_wait_is_retained() {
        let interrupt = Interrupt::new(true);
        let event = Arc::new(Event::default());
        interrupt.register(&event);
        assert!(interrupt.load(Ordering::Relaxed));
        interrupt.store(false, Ordering::Relaxed);
        let version = event.version();
        assert!(!interrupt.load(Ordering::Relaxed));
        interrupt.store(true, Ordering::Relaxed);
        let blocking = crate::rt::Blocking::dedicated_thread();
        super::super::callback::with(&blocking, || event.wait_after(version, None)).unwrap();
        assert!(interrupt.load(Ordering::Relaxed));
    }

    #[test]
    fn cancellation_wakes_a_reader_and_registrations_neither_duplicate_nor_retain_old_songs() {
        let interrupt = Arc::new(Interrupt::new(false));
        let event = Arc::new(Event::default());
        interrupt.register(&event);
        interrupt.register(&event);
        assert_eq!(interrupt.events.lock().unwrap().len(), 1);
        let (ready, waiting) = mpsc::channel();
        let (done, complete) = mpsc::channel();
        let reader_event = event.clone();
        let reader_interrupt = interrupt.clone();
        let reader = std::thread::spawn(move || {
            let blocking = crate::rt::Blocking::dedicated_thread();
            super::super::callback::with(&blocking, || {
                let mut version = reader_event.version();
                assert!(!reader_interrupt.load(Ordering::Relaxed));
                ready.send(()).unwrap();
                while !reader_interrupt.load(Ordering::Relaxed) {
                    reader_event.wait_after(version, None).unwrap();
                    version = reader_event.version();
                }
            });
            done.send(()).unwrap();
        });
        waiting.recv_timeout(Duration::from_secs(3)).unwrap();
        interrupt.store(true, Ordering::Relaxed);
        complete.recv_timeout(Duration::from_secs(3)).unwrap();
        reader.join().unwrap();
        let weak = Arc::downgrade(&event);
        drop(event);
        assert!(weak.upgrade().is_none());
        interrupt.store(true, Ordering::Relaxed);
        assert!(interrupt.events.lock().unwrap().is_empty());
    }
}
