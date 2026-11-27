//! One event-driven doze lookup. Stale replies never authorize radio sleep;
//! input keeps running while the DB owner or card is busy.
use super::{UiMsg, UiTx};
use crate::{audio::player::Status, library::{LibError, Library}};
use std::{sync::Arc, time::Instant};
use subsonic::Song;

struct Stamp {
    queue: Arc<Vec<Song>>,
    index: usize,
    player: u64,
    seek: u64,
    music: u32,
    state: crate::audio::player::PlayState,
    position: u64,
    at: Instant,
    revision: u64,
}
impl Stamp {
    fn current(&self, status: &Status, music: u32, position: u64, revision: u64) -> bool {
        Arc::ptr_eq(&self.queue, &status.queue) && self.index == status.index
            && self.player == status.generation && self.seek == status.seek_revision
            && self.music == music && self.state == status.state && self.revision == revision
            && position >= self.position
    }
}
struct Pending {
    token: u64,
    stamp: Stamp,
    result: tokio::sync::oneshot::Receiver<Result<u64, LibError>>,
}
#[derive(Default)]
pub(super) struct Cache {
    pending: Option<Pending>,
    ready: Option<(Stamp, u64)>,
    token: u64,
    revision: u64,
}
impl Cache {
    pub(super) fn pending(&self) -> bool { self.pending.is_some() }
    pub(super) fn has_token(&self, token: u64) -> bool { self.pending.as_ref().is_some_and(|pending| pending.token == token) }
    pub(super) fn invalidate(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.ready = None;
    }
    pub(super) fn request(&mut self, lib: Arc<Library>, tx: UiTx, status: &Status, position: u64, now: Instant) {
        if self.pending.is_some() { return; }
        self.token = self.token.wrapping_add(1);
        let token = self.token;
        let stamp = Stamp { queue: status.queue.clone(), index: status.index, player: status.generation,
            seek: status.seek_revision, music: lib.client_generation(), state: status.state,
            position, at: now, revision: self.revision };
        let (send, result) = tokio::sync::oneshot::channel();
        let queue = stamp.queue.clone(); let index = stamp.index;
        self.pending = Some(Pending { token, stamp, result });
        crate::rt::io().spawn(async move {
            let ahead = lib.local_ahead_ms_async(queue, index, position, super::doze::LOOKAHEAD_SONGS).await;
            // Result precedes wake, so handling this message never awaits storage.
            let _ = send.send(ahead);
            let _ = tx.send(UiMsg::DozeCacheReady(token));
        });
    }
    fn accept(&mut self, stamp: Stamp, result: Result<u64, LibError>, status: &Status, music: u32, position: u64) -> Option<(Instant, usize, usize)> {
        if !stamp.current(status, music, position, self.revision) { return None; }
        let checked = (stamp.at, Arc::as_ptr(&stamp.queue) as usize, stamp.index);
        // Failed reads are a conservative zero, never proof of cached content.
        self.ready = Some((stamp, result.unwrap_or(0)));
        Some(checked)
    }
    pub(super) fn receive(&mut self, token: u64, status: &Status, music: u32, position: u64) -> Option<(Instant, usize, usize)> {
        if self.pending.as_ref().is_none_or(|pending| pending.token != token) { return None; }
        let mut pending = self.pending.take().unwrap();
        match pending.result.try_recv() {
            Ok(result) => self.accept(pending.stamp, result, status, music, position),
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => self.accept(pending.stamp, Err(LibError::Offline), status, music, position),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => { self.pending = Some(pending); None }
        }
    }
    pub(super) fn ahead(&self, status: &Status, music: u32, position: u64) -> Option<u64> {
        self.ready.as_ref().filter(|(stamp, _)| stamp.current(status, music, position, self.revision))
            .map(|(stamp, ahead)| ahead.saturating_sub(position.saturating_sub(stamp.position)))
    }
    /// A conservative lower bound while the radio is already asleep. Normal
    /// track advance must not force a reconnect just because its fresh lookup
    /// is pending. The retained immutable queue proves intervening content;
    /// subtract it, never add new availability or admit a new sleep from it.
    pub(super) fn carried_ahead(&self, status: &Status, music: u32, position: u64) -> Option<u64> {
        let (stamp, ahead) = self.ready.as_ref()?;
        if !Arc::ptr_eq(&stamp.queue, &status.queue) || stamp.music != music || stamp.revision != self.revision
            || status.index < stamp.index || status.index - stamp.index > super::doze::LOOKAHEAD_SONGS { return None; }
        let skipped: u64 = stamp.queue.iter().skip(stamp.index).take(status.index - stamp.index)
            .map(|song| u64::from(song.duration) * 1000).sum();
        Some(ahead.saturating_sub(skipped.saturating_add(position).saturating_sub(stamp.position)))
    }
    /// Fixture settling joins the real snapshot, then feeds the same acceptance
    /// rule. Production never uses this blocking-at-the-test-boundary path.
    #[cfg(test)]
    pub(super) async fn settle(&mut self, status: &Status, music: u32, position: u64) -> Option<(Instant, usize, usize)> {
        let pending = self.pending.take()?;
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), pending.result).await.expect("doze cache fixture deadline")
            .unwrap_or(Err(LibError::Offline));
        self.accept(pending.stamp, result, status, music, position)
    }
}
