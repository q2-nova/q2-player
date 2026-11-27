#![forbid(unsafe_code)]
//! Volume and headphone EQ per output (BLUETOOTH-002, EQ-OUTPUT-001).
//!
//! The headphone jack keeps `[playback] volume` and the profile pinned in
//! `[playback.eq]`. Each Bluetooth device keeps its own volume and headphone
//! EQ in the paired-device store (`bluetooth::Saved`). This holds the device
//! audio goes to and decides what the player is told when the output, or a
//! setting of it, changes. The graphic EQ is the listener's taste and is the
//! same on every output.
use crate::audio::eq;
use crate::bluetooth::{DeviceEq, DeviceId, EqChoice};
use q2_bt::host::Addr;

/// A device's first connection, without a saved volume, starts no louder than this.
pub const FIRST_VOLUME_CAP: u8 = 60;
/// One press of a headset's volume key, in volume steps.
pub const KEY_STEP: i32 = 4;

/// The device's headphone profile.
#[derive(Clone, Debug, PartialEq)]
pub enum EqState {
    /// Looking for its model in the AutoEQ catalog.
    Matching,
    /// No catalog model matched its name or Device ID.
    NoMatch,
    /// The profile is being read from the card or downloaded.
    Loading,
    Ready(eq::Profile),
    /// The profile could not be read; the reason.
    Unavailable(String),
}

pub struct Bluetooth {
    pub addr: Addr,
    pub name: String,
    pub class: u32,
    pub id: Option<DeviceId>,
    pub volume: u8,
    /// `bluetooth::Output::volume_revision` as last followed.
    volume_revision: u32,
    pub absolute: bool,
    pub eq: Option<DeviceEq>,
    pub eq_state: EqState,
}

impl Bluetooth {
    /// Why this profile is in use, for the Equalizer page.
    pub fn eq_reason(&self) -> &'static str {
        match (&self.eq_state, &self.eq) {
            (EqState::Matching, _) => "Finding a match…",
            (EqState::NoMatch, _) => "No automatic match",
            (EqState::Loading, _) => "Loading…",
            (EqState::Unavailable(_), _) => "Not downloaded",
            (EqState::Ready(_), Some(DeviceEq { choice: EqChoice::Chosen, .. })) => "Chosen",
            (EqState::Ready(_), _) => "Matched automatically",
        }
    }
    pub fn enabled(&self) -> bool { self.eq.as_ref().is_none_or(|eq| eq.enabled) }
    /// The profile the player runs on this device.
    pub fn profile(&self) -> Option<&eq::Profile> {
        match &self.eq_state { EqState::Ready(profile) if self.enabled() => Some(profile), _ => None }
    }
}

/// What the player must be told after a change.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Changes {
    /// `Cmd::SetRemoteVolume`.
    pub remote: Option<bool>,
    /// `Cmd::SetVolume`.
    pub volume: Option<u8>,
    /// `Cmd::SetEq` with `Outputs::eq`.
    pub eq: bool,
    /// The headphones changed the volume: show it.
    pub show_volume: bool,
    /// The device's headphone profile must be found or read (`Outputs::plan`).
    pub resolve: bool,
}

/// How to find the device's profile.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// Match its model in the catalog, then read that profile.
    Match(crate::audio::eq_match::Hint),
    /// Read the saved choice's profile.
    Load { path: String },
}

#[derive(Default)]
pub struct Outputs {
    pub bluetooth: Option<Bluetooth>,
    /// Counts profile resolutions, so a late result of an earlier one is dropped.
    pub token: u64,
}

impl Outputs {
    /// Follows the Bluetooth thread's output (`State::output`). `wired_volume` is the jack's.
    pub fn sync(&mut self, output: Option<&crate::bluetooth::Output>, wired_volume: u8) -> Changes {
        let mut changes = Changes::default();
        match (output, &mut self.bluetooth) {
            (None, None) => {}
            (None, Some(_)) => {
                self.bluetooth = None;
                changes = Changes { remote: Some(false), volume: Some(wired_volume), eq: true, ..Changes::default() };
            }
            (Some(output), Some(bluetooth)) if bluetooth.addr == output.addr => {
                if output.absolute != bluetooth.absolute { bluetooth.absolute = output.absolute; changes.remote = Some(output.absolute); }
                if output.volume_revision != bluetooth.volume_revision {
                    bluetooth.volume_revision = output.volume_revision;
                    let volume = output.saved.volume.unwrap_or(wired_volume.min(FIRST_VOLUME_CAP));
                    if volume != bluetooth.volume {
                        bluetooth.volume = volume;
                        changes.volume = Some(volume);
                        changes.show_volume = true;
                    }
                }
                if output.saved.eq != bluetooth.eq { bluetooth.eq = output.saved.eq.clone(); changes.resolve = true; }
                if output.saved.id != bluetooth.id {
                    bluetooth.id = output.saved.id;
                    // A Device ID read after the stream opened can match what the name did not.
                    if bluetooth.eq.is_none() && bluetooth.eq_state == EqState::NoMatch { changes.resolve = true; }
                }
            }
            (Some(output), _) => {
                let volume = output.saved.volume.unwrap_or(wired_volume.min(FIRST_VOLUME_CAP));
                self.bluetooth = Some(Bluetooth { addr: output.addr, name: output.name.clone(), class: output.class, id: output.saved.id, volume,
                    volume_revision: output.volume_revision, absolute: output.absolute, eq: output.saved.eq.clone(), eq_state: EqState::Loading });
                changes = Changes { remote: Some(output.absolute), volume: Some(volume), eq: true, show_volume: false, resolve: true };
            }
        }
        changes
    }

    /// The volume of the output in use.
    pub fn volume(&self, wired: u8) -> u8 { self.bluetooth.as_ref().map_or(wired, |b| b.volume) }

    /// The EQ the player runs: the jack's settings, or on Bluetooth the same
    /// graphic EQ with the device's own headphone profile (never the jack's).
    pub fn eq(&self, wired: &eq::Settings) -> eq::Settings {
        let Some(bluetooth) = &self.bluetooth else { return wired.clone() };
        let profile = bluetooth.profile().cloned();
        eq::Settings { graphic_enabled: wired.graphic_enabled, bands: wired.bands, headphone_enabled: profile.is_some(), headphone: profile }
    }

    /// Starts resolving the device's profile: the token, the device and what to do;
    /// `None` when there is nothing to read.
    pub fn plan(&mut self) -> Option<(u64, Addr, Plan)> {
        let bluetooth = self.bluetooth.as_mut()?;
        self.token += 1;
        let plan = match &bluetooth.eq {
            Some(eq) if eq.path.is_empty() => { bluetooth.eq_state = EqState::NoMatch; return None }
            Some(eq) => { bluetooth.eq_state = EqState::Loading; Plan::Load { path: eq.path.clone() } }
            None => {
                bluetooth.eq_state = EqState::Matching;
                Plan::Match(crate::audio::eq_match::Hint { name: bluetooth.name.clone(), class: bluetooth.class, id: bluetooth.id })
            }
        };
        Some((self.token, bluetooth.addr, plan))
    }

    /// A resolution finished. `matched`: the catalog entry an automatic match found.
    /// Returns the choice to save for the device, if one was made, or `None` when the result is stale.
    pub fn resolved(&mut self, token: u64, addr: Addr, matched: Option<DeviceEq>, profile: Result<Option<eq::Profile>, String>) -> Option<Option<DeviceEq>> {
        if token != self.token { return None }
        let bluetooth = self.bluetooth.as_mut().filter(|b| b.addr == addr)?;
        if matched.is_some() { bluetooth.eq = matched.clone(); }
        bluetooth.eq_state = match profile {
            Ok(Some(profile)) => EqState::Ready(profile),
            Ok(None) => EqState::NoMatch,
            Err(error) => EqState::Unavailable(error),
        };
        Some(matched)
    }

    /// The listener picked a profile for the Bluetooth device: the choice to save.
    pub fn choose(&mut self, path: &str, profile: eq::Profile) -> Option<(Addr, DeviceEq)> {
        let bluetooth = self.bluetooth.as_mut()?;
        let eq = DeviceEq { choice: EqChoice::Chosen, enabled: true, path: path.into(), name: profile.name.clone() };
        bluetooth.eq = Some(eq.clone());
        bluetooth.eq_state = EqState::Ready(profile);
        self.token += 1;
        Some((bluetooth.addr, eq))
    }

    /// Turns the device's headphone EQ on or off: the choice to save.
    pub fn set_enabled(&mut self, enabled: bool) -> Option<(Addr, DeviceEq)> {
        let bluetooth = self.bluetooth.as_mut()?;
        let mut eq = bluetooth.eq.clone().unwrap_or(DeviceEq { choice: EqChoice::Automatic, enabled, path: String::new(), name: String::new() });
        eq.enabled = enabled;
        bluetooth.eq = Some(eq.clone());
        Some((bluetooth.addr, eq))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bluetooth::{Output, Saved};

    fn profile(name: &str) -> eq::Profile {
        eq::Profile { name: name.into(), source: format!("generated:{name}"), preamp: -3.0, filters: vec![eq::Filter { kind: eq::Kind::Peak, hz: 1000.0, gain: 3.0, q: 1.0 }] }
    }
    fn output(addr: u8, saved: Saved) -> Output {
        Output { addr: [addr; 6], name: "Generated Headphones".into(), class: 0x24_0418, saved, volume_revision: 0, absolute: false }
    }
    fn wired() -> eq::Settings {
        eq::Settings { graphic_enabled: true, bands: [2; 10], headphone_enabled: true, headphone: Some(profile("Wired Pin")) }
    }

    #[test]
    fn each_output_keeps_its_volume_and_a_new_device_starts_at_a_capped_level() {
        let mut outputs = Outputs::default();
        // A device seen for the first time: the jack's volume, capped.
        let changes = outputs.sync(Some(&output(1, Saved::default())), 85);
        assert_eq!(changes, Changes { remote: Some(false), volume: Some(FIRST_VOLUME_CAP), eq: true, show_volume: false, resolve: true });
        assert_eq!(outputs.volume(85), FIRST_VOLUME_CAP);
        assert_eq!(outputs.sync(Some(&output(1, Saved::default())), 30).volume, None, "the same device again changes nothing");
        // Back to the jack: its own volume, the device's gone.
        assert_eq!(outputs.sync(None, 85), Changes { remote: Some(false), volume: Some(85), eq: true, ..Changes::default() });
        assert_eq!(outputs.volume(85), 85);
        // A device with a saved volume gets it, whatever the jack's.
        let saved = Saved { volume: Some(72), ..Saved::default() };
        assert_eq!(outputs.sync(Some(&output(2, saved.clone())), 20).volume, Some(72));
        // Another device replaces it directly.
        assert_eq!(outputs.sync(Some(&output(3, Saved { volume: Some(15), ..Saved::default() })), 20).volume, Some(15));
        assert_eq!(outputs.sync(Some(&output(2, saved)), 20).volume, Some(72));
    }

    #[test]
    fn headphone_volume_changes_are_followed_once_and_absolute_volume_switches_the_player() {
        let mut outputs = Outputs::default();
        let mut o = output(1, Saved { volume: Some(50), ..Saved::default() });
        outputs.sync(Some(&o), 40);
        // Absolute volume comes up after the stream opens.
        o.absolute = true;
        assert_eq!(outputs.sync(Some(&o), 40), Changes { remote: Some(true), ..Changes::default() });
        // The headphones' buttons: followed and shown, once.
        o.saved.volume = Some(58); o.volume_revision = 1;
        assert_eq!(outputs.sync(Some(&o), 40), Changes { volume: Some(58), show_volume: true, ..Changes::default() });
        assert_eq!(outputs.sync(Some(&o), 40), Changes::default());
        // A snapshot with the same revision does not override an optimistic wheel change.
        o.saved.volume = Some(61);
        assert_eq!(outputs.sync(Some(&o), 40), Changes::default());
        assert_eq!(outputs.volume(40), 58);
    }

    #[test]
    fn bluetooth_runs_its_own_profile_never_the_jacks_and_keeps_the_graphic_eq() {
        let mut outputs = Outputs::default();
        assert_eq!(outputs.eq(&wired()), wired());
        outputs.sync(Some(&output(1, Saved::default())), 50);
        // Before its profile is found: the graphic EQ only.
        let bluetooth = outputs.eq(&wired());
        assert_eq!((bluetooth.graphic_enabled, bluetooth.bands, bluetooth.headphone_enabled, bluetooth.headphone), (true, [2; 10], false, None));
        // An automatic match: saved as automatic, applied.
        let (token, addr, plan) = outputs.plan().unwrap();
        assert!(matches!(plan, Plan::Match(ref hint) if hint.name == "Generated Headphones" && hint.class == 0x24_0418));
        let matched = DeviceEq { choice: EqChoice::Automatic, enabled: true, path: "generated/over-ear/Generated%20Model".into(), name: "Generated Model".into() };
        assert_eq!(outputs.resolved(token, addr, Some(matched.clone()), Ok(Some(profile("Generated Model")))), Some(Some(matched)));
        assert_eq!(outputs.eq(&wired()).headphone.unwrap().name, "Generated Model");
        assert_eq!(outputs.bluetooth.as_ref().unwrap().eq_reason(), "Matched automatically");
        // A stale result does nothing.
        assert_eq!(outputs.resolved(token - 1, addr, None, Ok(None)), None);
        // Chosen by the listener; then off; the jack's profile never appears.
        let (_, chosen) = outputs.choose("generated/in-ear/Other", profile("Other")).unwrap();
        assert_eq!((chosen.choice, outputs.bluetooth.as_ref().unwrap().eq_reason()), (EqChoice::Chosen, "Chosen"));
        assert_eq!(outputs.eq(&wired()).headphone.unwrap().name, "Other");
        let (_, off) = outputs.set_enabled(false).unwrap();
        assert!(!off.enabled && off.path == "generated/in-ear/Other");
        assert_eq!((outputs.eq(&wired()).headphone_enabled, outputs.eq(&wired()).headphone), (false, None));
        // Disconnect: the jack's pinned profile again.
        outputs.sync(None, 50);
        assert_eq!(outputs.eq(&wired()), wired());
    }

    #[test]
    fn saved_choices_load_and_a_late_device_id_retries_a_failed_match() {
        let mut outputs = Outputs::default();
        let chosen = DeviceEq { choice: EqChoice::Chosen, enabled: true, path: "generated/x".into(), name: "X".into() };
        let mut o = output(4, Saved { eq: Some(chosen.clone()), ..Saved::default() });
        assert!(outputs.sync(Some(&o), 50).resolve);
        assert_eq!(outputs.plan().map(|p| p.2), Some(Plan::Load { path: "generated/x".into() }));
        assert_eq!(outputs.bluetooth.as_ref().unwrap().eq_reason(), "Loading…");
        // Offline and not on the card.
        let token = outputs.token;
        outputs.resolved(token, [4; 6], None, Err("Profile not saved; connect to download".into()));
        assert_eq!(outputs.bluetooth.as_ref().unwrap().eq_reason(), "Not downloaded");
        // Another device without a choice and no match; its Device ID arrives later.
        o = output(5, Saved::default());
        outputs.sync(Some(&o), 50);
        let (token, addr, _) = outputs.plan().unwrap();
        outputs.resolved(token, addr, None, Ok(None));
        assert_eq!(outputs.bluetooth.as_ref().unwrap().eq_reason(), "No automatic match");
        o.saved.id = Some(DeviceId { source: 1, vendor: 0x004c, product: 0x200e, version: 0 });
        assert!(outputs.sync(Some(&o), 50).resolve);
        assert!(matches!(outputs.plan().map(|p| p.2), Some(Plan::Match(hint)) if hint.id.is_some()));
        // Headphone EQ switched off before any match: nothing to read.
        o.saved.eq = Some(DeviceEq { choice: EqChoice::Automatic, enabled: false, path: String::new(), name: String::new() });
        assert!(outputs.sync(Some(&o), 50).resolve);
        assert!(outputs.plan().is_none());
    }
}

#[cfg(test)]
mod app_tests {
    use super::*;
    use crate::audio::player::{Cmd, PlayerHandle, Status};
    use crate::bluetooth::{Output, Saved};
    use crate::ui::UiMsg;

    fn drain(commands: &std::sync::mpsc::Receiver<Cmd>) -> Vec<Cmd> { commands.try_iter().collect() }
    fn volumes(commands: &[Cmd]) -> Vec<u8> { commands.iter().filter_map(|c| if let Cmd::SetVolume(v) = c { Some(*v) } else { None }).collect() }
    fn remote(commands: &[Cmd]) -> Vec<bool> { commands.iter().filter_map(|c| if let Cmd::SetRemoteVolume(v) = c { Some(*v) } else { None }).collect() }
    fn eqs(commands: &[Cmd]) -> Vec<eq::Settings> { commands.iter().filter_map(|c| if let Cmd::SetEq(s) = c { Some(s.clone()) } else { None }).collect() }
    fn profile(name: &str) -> eq::Profile {
        eq::Profile { name: name.into(), source: format!("generated:{name}"), preamp: -2.0, filters: vec![eq::Filter { kind: eq::Kind::Peak, hz: 500.0, gain: 2.0, q: 1.0 }] }
    }

    #[test]
    fn volume_limit_slider_only_turns_down_the_active_output_when_needed() {
        use crate::ui::screens::settings::{Page, Settings};
        for (wired, headphones, sent) in [(90, 12, vec![]), (12, 90, vec![60])] {
            let mut app = crate::ui::settings_tests::fixture();
            app.player.shutdown();
            let (player, commands) = PlayerHandle::fixture(Status::default());
            app.player = player;
            app.config.playback.volume = wired;
            app.config.playback.max_volume = 100;
            let output = Output { addr: [7; 6], name: "Fixture".into(), class: 0,
                saved: Saved { volume: Some(headphones), ..Saved::default() }, volume_revision: 0, absolute: false };
            app.outputs.sync(Some(&output), wired);
            app.push(Box::new(Settings::new(Page::VolumeLimit)));
            let mut platform = crate::platform::Headless { last: None };
            app.event(crate::ui::Event::Scroll(-40), &mut platform);
            assert_eq!(volumes(&drain(&commands)), sent);
            assert_eq!(app.config.playback.volume, wired.min(60));
            assert_eq!(app.outputs.volume(wired), headphones.min(60));
            app.event(crate::ui::Event::Scroll(30), &mut platform);
            assert!(volumes(&drain(&commands)).is_empty());
            let jack = app.config.playback.volume;
            assert_eq!(app.outputs.sync(None, jack).volume, Some(wired.min(60)));
        }
    }

    #[test]
    fn one_volume_per_output_follows_the_headphones_and_switches_the_eq_with_the_route() {
        let mut app = crate::ui::settings_tests::fixture();
        app.player.shutdown();
        let (player, commands) = PlayerHandle::fixture(Status::default());
        app.player = player;
        app.background = false;
        app.config.playback.volume = 40;
        app.config.playback.eq = eq::Settings { graphic_enabled: true, bands: [1; 10], headphone_enabled: true, headphone: Some(profile("Jack Pin")) };
        let mut platform = crate::platform::Headless { last: None };
        // Headphones with absolute volume and a saved level connect.
        let mut output = Output { addr: [7; 6], name: "Generated Headphones".into(), class: 0x24_0418,
            saved: Saved { volume: Some(55), ..Saved::default() }, volume_revision: 0, absolute: true };
        let changes = app.outputs.sync(Some(&output), 40);
        app.apply_output_changes(changes);
        let sent = drain(&commands);
        assert_eq!((remote(&sent), volumes(&sent)), (vec![true], vec![55]));
        let bluetooth_eq = eqs(&sent).pop().unwrap();
        assert_eq!((bluetooth_eq.graphic_enabled, bluetooth_eq.bands, bluetooth_eq.headphone), (true, [1; 10], None), "the jack's profile is not used on Bluetooth");
        // The wheel changes the device's volume, not the jack's.
        app.change_volume(3);
        assert_eq!((volumes(&drain(&commands)), app.output_volume(), app.config.playback.volume), (vec![58], 58, 40));
        // The headphones' buttons: the player follows and shows it.
        output.saved.volume = Some(70); output.volume_revision = 1;
        app.volume_shown = None;
        let changes = app.outputs.sync(Some(&output), 40);
        app.apply_output_changes(changes);
        assert_eq!((volumes(&drain(&commands)), app.output_volume(), app.volume_shown.is_some()), (vec![70], 70, true));
        // A headset volume key steps the same volume.
        app.handle(UiMsg::Input(crate::input::Raw::Media(crate::input::Media::VolumeDown)), &mut platform);
        assert_eq!((volumes(&drain(&commands)), app.config.playback.volume), (vec![70 - KEY_STEP as u8], 40));
        // The device's profile, matched automatically, is applied.
        let token = app.outputs.token;
        let matched = DeviceEq { choice: EqChoice::Automatic, enabled: true, path: "generated/over-ear/Model".into(), name: "Generated Model".into() };
        app.handle(UiMsg::OutputEq { token, addr: [7; 6], matched: Some(matched), profile: Ok(Some(profile("Generated Model"))) }, &mut platform);
        let applied = eqs(&drain(&commands)).pop().unwrap();
        assert_eq!(applied.headphone.map(|p| p.name), Some("Generated Model".into()));
        assert_eq!(app.outputs.bluetooth.as_ref().unwrap().eq_reason(), "Matched automatically");
        // Disconnected: the jack's volume, software volume, and its pinned profile.
        let changes = app.outputs.sync(None, 40);
        app.apply_output_changes(changes);
        let sent = drain(&commands);
        assert_eq!((remote(&sent), volumes(&sent)), (vec![false], vec![40]));
        assert_eq!(eqs(&sent).pop().unwrap(), app.config.playback.eq);
        // A headset key with no Bluetooth output (a wired remote): the jack's volume.
        app.handle(UiMsg::Input(crate::input::Raw::Media(crate::input::Media::VolumeUp)), &mut platform);
        assert_eq!((volumes(&drain(&commands)), app.config.playback.volume), (vec![40 + KEY_STEP as u8], 40 + KEY_STEP as u8));
    }
}
