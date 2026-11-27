//! Navidrome reachability: the shell around `q2_machines::server`
//! (docs/server-status.md).
//!
//! Requests report their outcome with the priority of whoever waits for them
//! ([`with_priority`]; a request outside a scope is foreground). The UI loop
//! reports the transport ([`Library::set_transport`]). One serialized probe
//! (`ping`) settles a failure. Only the machine changes the published status;
//! this file carries out its effects.
//!
//! The machine's state lives in one atomic word with the reset generation, so
//! no lock is taken: each event is one compare-and-swap of `transition`. Two
//! outcomes finishing at the same moment can publish their effects in either
//! order.
use super::{set_offline_cause, cause_of, Library, LibEvent};
use q2_machines::server::{self, Event, Priority, Status};
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// One ping attempt, all of it. Normal pings through Tailscale take well
/// under a second; this allows a loaded proxy several seconds.
#[cfg(not(test))]
pub(super) const PROBE_ATTEMPT: Duration = Duration::from_secs(8);
#[cfg(test)]
pub(super) const PROBE_ATTEMPT: Duration = Duration::from_millis(800);
/// Pause before the second attempt, so one lost ping is not taken as an
/// unreachable server.
#[cfg(not(test))]
const PROBE_RETRY: Duration = Duration::from_secs(1);
#[cfg(test)]
const PROBE_RETRY: Duration = Duration::from_millis(50);
/// Attempts per probe.
const PROBE_ATTEMPTS: u32 = 2;

tokio::task_local! {
    static PRIORITY: Priority;
}

/// Runs `future` with its Navidrome requests reported at `priority`.
pub(crate) fn with_priority<F: Future>(priority: Priority, future: F) -> impl Future<Output = F::Output> {
    PRIORITY.scope(priority, future)
}

/// The priority of the request being made: foreground unless a scope says otherwise.
pub(super) fn current_priority() -> Priority {
    PRIORITY.try_with(|priority| *priority).unwrap_or(Priority::Foreground)
}

/// The machine's state and the reset generation in one word: status in
/// bits 0–1, failures 2–3, probing 4, transport down 5, generation 8–31.
fn encode(state: server::Health, generation: u32) -> u32 {
    let status = match state.status { Status::Online => 0, Status::Checking => 1, Status::Offline => 2 };
    status | u32::from(state.failures.min(3)) << 2 | u32::from(state.probing) << 4 | u32::from(state.transport_down) << 5
        | (generation & 0x00ff_ffff) << 8
}

fn decode(word: u32) -> (server::Health, u32) {
    let status = match word & 3 { 0 => Status::Online, 1 => Status::Checking, _ => Status::Offline };
    (server::Health { status, failures: (word >> 2 & 3) as u8, probing: word >> 4 & 1 == 1, transport_down: word >> 5 & 1 == 1 }, word >> 8)
}

pub(super) struct Health {
    /// `encode`d machine state and reset generation; a probe's result counts
    /// only for its generation.
    state: AtomicU32,
    /// Foreground requests in flight; background transfers yield to them.
    foreground: AtomicU32,
    /// The transport last reported: 0 unknown, 1 up, 2 down.
    transport: AtomicU8,
    /// Resume maintenance when a recovered transport has a server response.
    resume_sync: AtomicBool,
}

impl Default for Health {
    fn default() -> Self {
        Health { state: AtomicU32::new(encode(server::Health::INITIAL, 0)), foreground: AtomicU32::new(0), transport: AtomicU8::new(0), resume_sync: AtomicBool::new(false) }
    }
}

impl Health {
    pub(super) fn transport_down(&self) -> bool { self.get().0.transport_down }

    /// Applies `event` atomically; with `generation`, only if no reset
    /// happened since. Returns the state before, the effect and the
    /// generation it applied in.
    fn step(&self, event: Event, generation: Option<u32>) -> Option<(server::Health, server::Effect, u32)> {
        let mut word = self.state.load(Ordering::Acquire);
        loop {
            let (before, current) = decode(word);
            if generation.is_some_and(|wanted| wanted != current) { return None; }
            let (after, effect) = server::transition(before, event);
            let next = if event == Event::Reset { current.wrapping_add(1) & 0x00ff_ffff } else { current };
            match self.state.compare_exchange_weak(word, encode(after, next), Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return Some((before, effect, next)),
                Err(actual) => word = actual,
            }
        }
    }
    fn get(&self) -> (server::Health, u32) { decode(self.state.load(Ordering::Acquire)) }
}

/// Held while a foreground request is in flight.
pub(super) struct Foreground<'a>(&'a Library);
impl Drop for Foreground<'_> {
    fn drop(&mut self) {
        if self.0.health.foreground.fetch_sub(1, Ordering::AcqRel) == 1 { self.0.priority_released(); }
    }
}

impl Library {
    /// Counts a foreground request in flight until the guard drops.
    pub(super) fn foreground_request(&self, priority: Priority) -> Option<Foreground<'_>> {
        (priority == Priority::Foreground).then(|| {
            self.health.foreground.fetch_add(1, Ordering::AcqRel);
            Foreground(self)
        })
    }

    /// Background transfers wait while a listener waits on the server or the
    /// probe runs, so neither queues behind the player's own bulk bytes.
    pub(super) fn background_should_yield(&self) -> bool {
        self.health.foreground.load(Ordering::Acquire) > 0 || self.health.get().0.probing
    }

    /// What Settings shows (Offline Mode and doze are the caller's).
    pub fn server_status(&self) -> Status {
        if !self.online.load(Ordering::Relaxed) { Status::Offline }
        else if self.health.get().0.status == Status::Checking { Status::Checking }
        else { Status::Online }
    }

    /// A response arrived from the current server.
    pub(super) fn observe_success(&self) {
        set_offline_cause(0);
        self.health_event(Event::Success);
    }

    /// A request to the current server failed. Errors that are not a
    /// connection or server failure (auth, a Subsonic error, a bad body) say
    /// nothing about reachability.
    pub(super) fn observe_failure(&self, error: &subsonic::Error, priority: Priority) {
        let Some(class) = error.failure() else { return };
        if priority != Priority::Low {
            crate::ux::record(crate::ux::Mark::ServerFailure, (priority as u32) << 8 | class as u32);
            set_offline_cause(cause_of(error));
        }
        self.health_event(Event::Failure { priority });
    }

    /// Both of the above for a finished request.
    pub(super) fn observe<T>(&self, result: &subsonic::Result<T>, priority: Priority) {
        match result {
            Ok(_) => self.observe_success(),
            Err(error) if error.is_offline() => self.observe_failure(error, priority),
            Err(_) => {}
        }
    }

    /// The link to the server: Wi-Fi with an address and, when enabled,
    /// Tailscale running. Only changes reach the machine.
    pub fn set_transport(&self, up: bool) {
        let code = if up { 1 } else { 2 };
        let previous = self.health.transport.swap(code, Ordering::Relaxed);
        if previous != code {
            log::info!("server transport {}", if up { "up" } else { "down" });
            if !up {
                // A foreground stream may fail fast once the transport is known down.
                self.link_pending.store(false, Ordering::Release);
                self.health.resume_sync.store(false, Ordering::Release);
                self.priority_released();
            } else if previous == 2 {
                self.health.resume_sync.store(true, Ordering::Release);
            }
            self.health_event(Event::Transport { up });
            // An earlier response can already have restored server health.
            if up && self.is_online() { self.wake_recovered_sync(); }
        }
    }

    /// The server or account changed: nothing is known about the new one.
    pub(super) fn reset_health(&self) {
        let _ = self.health.step(Event::Reset, None);
        self.online.store(false, Ordering::Relaxed);
    }

    /// The listener opened the server's status: probe now, unless a probe
    /// runs or there is no transport. A status shown from before the server
    /// failed reads Checking until the probe ends (docs/server-status.md).
    pub fn recheck_server(&self) {
        if !self.is_configured() || self.offline_mode() || self.radio_dozing() { return; }
        self.health_event(Event::Recheck);
    }

    /// The offline cadence (the sync worker): probes when the machine has no
    /// probe running and a transport, and waits for the result.
    pub(super) async fn probe_if_due(self: &Arc<Self>) {
        let Some((_, effect, generation)) = self.health.step(Event::ProbeDue, None) else { return };
        if effect.probe { self.clone().probe(generation).await; }
    }

    fn health_event(&self, event: Event) {
        let Some((before, effect, generation)) = self.health.step(event, None) else { return };
        if let Some(status) = effect.publish { self.publish_status(before.status, status); }
        if effect.probe {
            if let Some(lib) = self.me.upgrade() { crate::rt::io().spawn(lib.probe(generation)); }
        }
    }

    fn wake_recovered_sync(&self) {
        if !self.health.transport_down() && self.health.resume_sync.swap(false, Ordering::AcqRel) {
            self.sync_urgent.store(true, Ordering::Release);
            self.wake_sync();
        }
    }

    fn publish_status(&self, before: Status, status: Status) {
        crate::ux::set_navidrome(match status { Status::Online => 1, Status::Offline => 2, Status::Checking => 3 });
        log::info!("server {}", match status { Status::Online => "reachable", Status::Checking => "checking", Status::Offline => "unreachable" });
        let online = status != Status::Offline;
        // Checking is a sub-state of online: requests, downloads and artwork
        // carry on, and nothing visible changes but Settings' status.
        if (before != Status::Offline) == online { return; }
        self.online.store(online, Ordering::Relaxed);
        self.background_changed();
        self.downloads.interrupt();
        if online {
            self.wake_recovered_sync();
            set_offline_cause(0);
            if self.downloads.has_saved() { self.downloads.wake(); }
            self.art.retry_missing();
        }
        (self.notify)(LibEvent::Online(online));
    }

    /// One probe: up to [`PROBE_ATTEMPTS`] pings, again while the machine
    /// asks for another. Waits for the restored link after Offline Mode or
    /// an automatic radio doze. Its result counts
    /// only if no reset happened meanwhile.
    async fn probe(self: Arc<Self>, generation: u32) {
        // Wakes waiting background transfers once the probe ends.
        struct Done<'a>(&'a Library);
        impl Drop for Done<'_> { fn drop(&mut self) { self.0.priority_released(); } }
        let _done = Done(&self);
        loop {
            loop {
                let changed = self.priority_changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                if self.health.get().1 != generation { return; }
                if self.maintenance_link_ready() { break; }
                changed.await;
            }
            let ok = self.ping(generation).await;
            if ok { set_offline_cause(0); }
            let Some((before, effect, _)) = self.health.step(Event::ProbeDone { ok }, Some(generation)) else { return };
            if let Some(status) = effect.publish { self.publish_status(before.status, status); }
            if !effect.probe { return; }
        }
    }

    /// Up to [`PROBE_ATTEMPTS`] pings, each within [`PROBE_ATTEMPT`]; true
    /// once the server answers. An HTTP/1.1 connection carries one exchange
    /// at a time, so a ping takes the idle pooled connection or a new one and
    /// never waits behind another response.
    async fn ping(self: &Arc<Self>, generation: u32) -> bool {
        for attempt in 0..PROBE_ATTEMPTS {
            if attempt > 0 { tokio::time::sleep(PROBE_RETRY).await; }
            let owner = self.clone();
            let client = crate::rt::spawn_blocking(move |_| {
                (owner.health.get().1 == generation).then(|| owner.client().ok()).flatten()
            }).await.ok().flatten();
            let Some(client) = client else { return false };
            let request = async { client.fetch_async(&subsonic::calls::ping().request).await };
            let result = match tokio::time::timeout(PROBE_ATTEMPT, request).await {
                Ok(result) => result.map(drop),
                Err(_) => Err(subsonic::Error::Network("probe timed out".into())),
            };
            let class = result.as_ref().err().and_then(subsonic::Error::failure).map_or(0, |class| class as u32);
            crate::ux::record(crate::ux::Mark::ServerProbe, u32::from(result.is_ok()) | (attempt + 1) << 4 | class << 8);
            match result {
                Ok(()) => return true,
                Err(error) if error.is_offline() => set_offline_cause(cause_of(&error)),
                // Auth and Subsonic errors are answers: the server is there.
                Err(_) => return true,
            }
        }
        false
    }

    /// Tests: sets the published status directly (Online or Offline).
    #[cfg(test)]
    pub(crate) fn set_online(&self, on: bool) {
        // Tests may also store `online` directly: compare what is published.
        let before = if self.online.load(Ordering::Relaxed) { Status::Online } else { Status::Offline };
        let status = if on { Status::Online } else { Status::Offline };
        let (state, generation) = self.health.get();
        self.health.state.store(encode(server::Health { status, failures: 0, ..state }, generation), Ordering::Release);
        self.publish_status(before, status);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_state_round_trips_through_its_word() {
        for status in [Status::Online, Status::Checking, Status::Offline] {
            for failures in 0..=server::OFFLINE_AFTER {
                for probing in [false, true] {
                    for transport_down in [false, true] {
                        let state = server::Health { status, failures, probing, transport_down };
                        assert_eq!(decode(encode(state, 0x00ab_cdef)), (state, 0x00ab_cdef));
                    }
                }
            }
        }
    }
}
