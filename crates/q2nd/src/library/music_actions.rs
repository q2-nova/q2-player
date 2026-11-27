//! Ordered local music actions. Input only posts work; SQLite never runs on UI.
use super::{Library, PendingCall};
use std::{
    collections::HashMap,
    future::Future,
    sync::{atomic::Ordering, Arc},
};
use subsonic::{ItemKind, Song};

#[derive(Default)]
struct Value {
    committed: Option<(u64, u8)>,
    latest: Option<(u64, u8)>,
    outstanding: usize,
}
#[derive(Default)]
pub(super) struct Feedback {
    epoch: u32,
    next: u64,
    values: HashMap<String, [Value; 2]>,
}
impl Feedback {
    fn account(&mut self, epoch: u32) {
        if self.epoch != epoch {
            self.values.clear();
            self.epoch = epoch;
        }
    }
    fn get(&mut self, epoch: u32, rating: bool, id: &str) -> Option<u8> {
        self.account(epoch);
        self.values.get(id).and_then(|values| {
            let value = &values[usize::from(rating)];
            value.latest.or(value.committed).map(|(_, value)| value)
        })
    }
    fn begin(&mut self, epoch: u32, key: (bool, String), value: u8) -> u64 {
        self.account(epoch);
        if self.values.len() >= 512 && !self.values.contains_key(&key.1) {
            self.values
                .retain(|_, values| values.iter().any(|value| value.outstanding != 0));
        }
        self.next = self
            .next
            .checked_add(1)
            .expect("music action revision exhausted");
        let entry = &mut self.values.entry(key.1).or_default()[usize::from(key.0)];
        entry.latest = Some((self.next, value));
        entry.outstanding += 1;
        self.next
    }
    fn finish(&mut self, epoch: u32, key: &(bool, String), revision: u64, value: u8, saved: bool) {
        if self.epoch != epoch {
            return;
        }
        let Some(values) = self.values.get_mut(&key.1) else {
            return;
        };
        let entry = &mut values[usize::from(key.0)];
        if saved && entry.committed.is_none_or(|(old, _)| old < revision) {
            entry.committed = Some((revision, value));
        }
        if entry.latest.is_some_and(|(latest, _)| latest == revision) {
            entry.latest = None;
        }
        entry.outstanding -= 1;
    }
}

impl Library {
    /// Capture transport admission before the runtime can defer the first poll.
    pub(super) fn scrobble_now_playing(self: &Arc<Self>, song: &Song)
        -> Option<impl Future<Output = ()> + Send + 'static + use<>> {
        if song.radio_url.is_some() || song.spoken.is_some()
            || !self.settings.read().unwrap().scrobble || !self.maintenance_link_ready() || !self.is_online() { return None; }
        let generation = self.client_generation();
        let id = song.id.clone();
        let lib = self.clone();
        Some(async move {
            if !lib.maintenance_link_ready() { return; }
            let _ = lib.fetch_net_generation(&subsonic::calls::scrobble(&id, None, false).request, generation, true).await;
        })
    }

    pub(super) fn scrobble_played(&self, song: &Song, started_ms: u64) {
        if song.radio_url.is_some() || song.spoken.is_some() || !self.settings.read().unwrap().scrobble { return; }
        let epoch = self.music_epoch.load(Ordering::SeqCst);
        let call = PendingCall::Scrobble { id: song.id.clone(), time_ms: started_ms };
        // Eager, ordered DB admission survives the dropped observer. Preserve
        // batching with the next track start (or existing sync cadence): this
        // callback must not add a sync wake, network request or resident owner.
        drop(self.persist_music_action(epoch, call));
    }

    pub fn loved(&self, song: &Song) -> bool {
        self.music_feedback
            .lock()
            .unwrap()
            .get(self.music_epoch.load(Ordering::SeqCst), false, &song.id)
            .map(|value| value != 0)
            .unwrap_or(song.starred.is_some())
    }
    pub fn rating(&self, song: &Song) -> u8 {
        self.music_feedback
            .lock()
            .unwrap()
            .get(self.music_epoch.load(Ordering::SeqCst), true, &song.id)
            .unwrap_or(song.user_rating.unwrap_or(0))
    }
    pub fn star(
        self: &Arc<Self>,
        kind: ItemKind,
        id: &str,
        on: bool,
    ) -> impl Future<Output = Result<(), String>> + Send + 'static {
        let kind_s = match kind {
            ItemKind::Song => "song",
            ItemKind::Album => "album",
            ItemKind::Artist => "artist",
        };
        self.send_or_queue(
            PendingCall::Star {
                kind: kind_s.into(),
                id: id.into(),
                on,
            },
            (kind == ItemKind::Song).then(|| ((false, id.to_owned()), u8::from(on))),
        )
    }
    pub fn rate(
        self: &Arc<Self>,
        id: &str,
        rating: u8,
    ) -> impl Future<Output = Result<(), String>> + Send + 'static {
        self.send_or_queue(
            PendingCall::Rating {
                id: id.into(),
                rating,
            },
            Some(((true, id.to_owned()), rating)),
        )
    }
    fn send_or_queue(
        self: &Arc<Self>,
        call: PendingCall,
        value: Option<((bool, String), u8)>,
    ) -> impl Future<Output = Result<(), String>> + Send + 'static {
        // Posting happens at the call, not when the returned future is polled.
        // The feedback guard also orders concurrent callers' revisions/posts.
        let mut feedback = self.music_feedback.lock().unwrap();
        let epoch = self.music_epoch.load(Ordering::SeqCst);
        let revision = value
            .as_ref()
            .map(|(key, value)| feedback.begin(epoch, key.clone(), *value));
        let stored = self.persist_music_action(epoch, call);
        drop(feedback);
        let lib = self.clone();
        // An abandoned UI waiter must not abandon the durable job or feedback.
        let completion = crate::rt::io().spawn(async move {
            let mut result = stored.await;
            if lib.music_epoch.load(Ordering::SeqCst) != epoch {
                result = Err("Music account changed; action cancelled".into());
            }
            if let Some(((key, value), revision)) = value.zip(revision) {
                lib.music_feedback.lock().unwrap().finish(
                    epoch,
                    &key,
                    revision,
                    value,
                    result.is_ok(),
                );
            }
            if result.is_ok() {
                lib.wake_sync();
            }
            result
        });
        async move {
            completion
                .await
                .map_err(|_| "Music action completion failed".to_string())?
        }
    }

    /// Shared account admission for UI actions and completed-song scrobbles.
    /// Submission happens now; the DB owner checks the captured account before
    /// SQL and records storage failure even when nobody awaits the result.
    fn persist_music_action(&self, epoch: u32, call: PendingCall)
        -> impl Future<Output = Result<(), String>> + Send + 'static {
        let current = self.music_epoch.clone();
        let bound = self.bound_music_epoch.clone();
        let failed = self.music_write_failed.clone();
        self.db_worker.run(move |blocking, db| {
            if current.load(Ordering::SeqCst) != epoch {
                return Err("Music account changed; action cancelled".to_string());
            }
            if bound.load(Ordering::SeqCst) != epoch {
                return Err("Music settings are being applied; retry the action".into());
            }
            let stored = if matches!(call, PendingCall::Scrobble { .. }) {
                db.push_pending_deferred(blocking, &call)
            } else {
                db.push_pending_checked(blocking, &call)
            };
            stored.map_err(|_| {
                failed.store(true, Ordering::SeqCst);
                "Could not save music action".to_string()
            })
        })
    }
    pub(super) fn check_music_writes(&self) -> Result<(), String> {
        if self.music_write_failed.swap(false, Ordering::SeqCst) {
            Err("Music action storage failed; transition cancelled".into())
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests;
