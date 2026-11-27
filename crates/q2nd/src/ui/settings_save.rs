//! UI observations of the ordered settings owner. A preference keeps its
//! candidate separate until durable success; unrelated edits remain live.
use super::{App, UiMsg, Request};
use crate::config::{Config, Save, ShelfLibraryPreference};
use std::{io::ErrorKind, sync::Arc, time::Instant};

#[derive(Clone, Copy)]
pub(super) enum Handoff { Usb, Power(bool), Restart }
#[derive(Default)]
pub(super) struct State {
    revision: u64,
    next: u64,
    pending: Option<Pending>,
    handoff: Option<Handoff>,
    preference: Option<Preference>,
    restarting: bool,
}
struct Pending {
    id: u64,
    revision: u64,
    config: Config,
    save: Save,
    preference: Option<Preference>,
}
pub struct Preference {
    pub screen: u64,
    pub token: u64,
    pub generation: Arc<()>,
    pub namespace: String,
    pub podcast: bool,
    pub value: ShelfLibraryPreference,
}
pub struct PreferenceSaved(pub Result<(), String>);

impl App {
    pub(super) fn settings_changed(&mut self) {
        self.settings_save.revision = self.settings_save.revision.checked_add(1).expect("settings revision exhausted");
        self.config_save = Some(Instant::now());
    }
    fn start_settings_write(&mut self, config: Config, preference: Option<Preference>) {
        assert!(self.settings_save.pending.is_none());
        self.settings_save.next = self.settings_save.next.checked_add(1).expect("settings write token exhausted");
        let id = self.settings_save.next;
        let save = self.paths.save_config_async(config.clone());
        self.settings_save.pending = Some(Pending { id, revision: self.settings_save.revision, config, save: save.clone(), preference });
        let tx = self.tx.clone();
        crate::rt::io().spawn(async move { let result = save.wait().await; let _ = tx.send(UiMsg::SettingsSaved { id, result }); });
    }
    pub(super) fn start_settings_save(&mut self) {
        if self.settings_save.pending.is_none() {
            if let Some(preference) = self.settings_save.preference.take() {
                self.start_preference_write(preference);
                if self.settings_save.pending.is_some() { return; }
            }
        }
        if self.settings_save.pending.is_none() && (self.config_save.is_some() || self.config_save_failed) {
            self.start_settings_write(self.config.clone(), None);
        }
    }
    pub(super) fn save_library_preference(&mut self, preference: Preference) {
        if self.settings_save.preference.is_some() || self.settings_save.pending.as_ref().is_some_and(|pending| pending.preference.is_some()) {
            self.preference_reply(preference, Err("Library selection is being saved; try again".into()));
            return;
        }
        if self.settings_save.pending.is_some() {
            self.settings_save.preference = Some(preference);
        } else {
            self.start_preference_write(preference);
        }
    }
    fn start_preference_write(&mut self, preference: Preference) {
        if !self.lib.shelf_generation_current(&preference.generation) || !self.lib.shelf_namespace_current(&preference.namespace) {
            self.preference_reply(preference, Err("Audiobookshelf changed; choose a library again".into()));
            return;
        }
        let mut config = self.config.clone();
        config.audiobookshelf.set_library_preference(preference.podcast, preference.value.clone());
        self.start_settings_write(config, Some(preference));
    }
    fn preference_reply(&mut self, preference: Preference, result: Result<(), String>) {
        // Durable mutation belongs to the app. Back removes only its navigation
        // observer, so a late reply cannot replace an unrelated top screen.
        if let Err(error) = &result { self.toast(error); }
        let action = self.with_screen(preference.screen, |screen, ctx| {
            screen.loaded(preference.token, Box::new(PreferenceSaved(result)), ctx);
            screen.completion()
        }).flatten();
        if self.top_id() == preference.screen {
            if let Some(action) = action { self.apply(action); }
        }
    }
    fn finish_settings_write(&mut self, id: u64, result: Result<(), ErrorKind>) -> bool {
        if !self.settings_save.pending.as_ref().is_some_and(|pending| pending.id == id) { return true; }
        let mut pending = self.settings_save.pending.take().unwrap();
        let mut preference_reply = None;
        if let Some(preference) = pending.preference.take() {
            let valid = self.lib.shelf_generation_current(&preference.generation)
                && self.lib.shelf_namespace_current(&preference.namespace);
            let reply = match result {
                Ok(()) if valid => {
                    let unchanged = self.settings_save.revision == pending.revision;
                    self.config.audiobookshelf.set_library_preference(preference.podcast, preference.value.clone());
                    self.config_changed();
                    if unchanged { pending.revision = self.settings_save.revision; }
                    Ok(())
                }
                Ok(()) => { self.settings_changed(); Err("Audiobookshelf changed; choose a library again".into()) }
                Err(_) => Err("Library selection not saved; check the card".into()),
            };
            preference_reply = Some((preference, reply));
        }
        match result {
            Ok(()) => {
                if self.settings_save.revision == pending.revision && self.config == pending.config {
                    self.config_save = None;
                    self.config_save_failed = false;
                } else {
                    // Successful older writes cannot acknowledge subsequent edits.
                    self.config_save.get_or_insert_with(Instant::now);
                }
            }
            Err(kind) => {
                self.config_save_failed = true;
                self.config_save = if matches!(kind, ErrorKind::PermissionDenied | ErrorKind::WouldBlock) { None } else { Some(Instant::now()) };
                self.toast("Settings not saved: check SD config");
            }
        }
        if let Some((preference, reply)) = preference_reply { self.preference_reply(preference, reply); }
        result.is_ok()
    }
    pub(super) fn settings_saved(&mut self, id: u64, result: Result<(), ErrorKind>) {
        if !self.finish_settings_write(id, result) { self.settings_save.handoff = None; }
        if self.settings_save.preference.is_some() { self.start_settings_save(); }
        self.continue_settings_handoff();
    }
    pub(super) fn request_settings_handoff(&mut self, handoff: Handoff) {
        if self.settings_save.handoff.is_some() || self.settings_save.restarting { return; }
        self.settings_save.handoff = Some(handoff);
        if matches!(handoff, Handoff::Restart) { self.settings_changed(); }
        self.continue_settings_handoff();
    }
    fn continue_settings_handoff(&mut self) {
        if self.settings_save.handoff.is_none() || self.settings_save.pending.is_some() { return; }
        if self.config_save.is_some() || self.config_save_failed {
            self.start_settings_save();
            return;
        }
        match self.settings_save.handoff.take().unwrap() {
            Handoff::Usb => self.open_usb_drive_saved(),
            Handoff::Power(off) => self.prepare_power_saved(off),
            Handoff::Restart => {
                self.settings_save.restarting = true;
                let revision = self.settings_save.revision;
                let player = self.player.clone(); let lib = self.lib.clone(); let tx = self.tx.clone();
                crate::rt::io().spawn(async move {
                    player.send(crate::audio::player::Cmd::Pause);
                    let result = lib.save_now_async().await;
                    let _ = tx.send(UiMsg::RestartPrepared { revision, result });
                });
            }
        }
    }
    pub(super) fn restart_prepared(&mut self, revision: u64, result: Result<(), String>) {
        self.settings_save.restarting = false;
        match result {
            Err(error) => self.toast(error),
            Ok(()) if revision == self.settings_save.revision && self.settings_save.pending.is_none() && self.config_save.is_none() && !self.config_save_failed => self.pending.push(Request::RestartReady),
            Ok(()) => self.request_settings_handoff(Handoff::Restart),
        }
    }
    pub fn begin_restart(&mut self) { self.request_settings_handoff(Handoff::Restart); }

    #[cfg(test)]
    pub(super) fn settings_pending(&self) -> bool { self.settings_save.pending.is_some() || self.settings_save.preference.is_some() }

    /// Map/ordinary fixtures settle only accepted work, not the debounce timer.
    #[cfg(test)]
    pub(super) fn settle_settings_fixture(&mut self) {
        assert!(crate::rt::may_wait_for_tasks());
        crate::rt::Ui::new().unwrap().block_on(async {
            tokio::time::timeout(std::time::Duration::from_secs(3), async {
                while let Some(pending) = &self.settings_save.pending {
                    let id = pending.id; let save = pending.save.clone();
                    let result = save.wait().await;
                    self.finish_settings_write(id, result);
                    if self.settings_save.preference.is_some() { self.start_settings_save(); }
                }
            }).await.expect("owned settings fixture completion");
        });
    }

    /// Final outside-loop barrier and test compatibility: finish any accepted
    /// preference, then persist the latest merged state, including newer edits.
    pub async fn save_settings_async(&mut self) -> bool {
        loop {
            self.start_settings_save();
            let Some(pending) = &self.settings_save.pending else { return true; };
            let id = pending.id; let save = pending.save.clone();
            let result = save.wait().await;
            if !self.finish_settings_write(id, result) && self.settings_save.preference.is_none() { return false; }
        }
    }
    #[cfg(test)]
    pub fn save_settings(&mut self) -> bool {
        assert!(crate::rt::may_wait_for_tasks());
        crate::rt::Ui::new().unwrap().block_on(self.save_settings_async())
    }
    #[cfg(test)]
    pub(crate) async fn prepare_restart(&mut self) -> Result<(), String> {
        self.settings_changed();
        if !self.save_settings_async().await { return Err("Settings could not be saved; restart cancelled".into()); }
        self.player.send(crate::audio::player::Cmd::Pause);
        self.lib.save_now_async().await
    }
}
