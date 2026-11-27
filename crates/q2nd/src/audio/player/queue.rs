//! Queue state-machine transitions and selection continuity.
//! The parent dispatches commands; these methods preserve the single player
//! thread owner and call output/decoder operations at the original boundaries.

use super::*;

impl Engine<'_> {
    /// The queue machine's view of the engine (docs/state-machines.md).
    pub(super) fn cursor(&self) -> qfsm::Queue {
        qfsm::Queue { len: self.queue.len(), index: self.index, play: self.state, failures: self.failures.min(qfsm::MAX_FAILURES - 1) }
    }

    /// The kind of the selected entry for the queue machine.
    pub(super) fn track_kind(&self) -> qfsm::Track {
        match self.queue.get(self.index) {
            Some(song) if song.spoken.is_some() => qfsm::Track::Book { continues: self.queue.get(self.index + 1)
                .and_then(|next| next.spoken.as_ref())
                .is_some_and(|next| next.id == song.spoken.as_ref().unwrap().id) },
            Some(song) if song.radio_url.is_some() => qfsm::Track::Radio,
            _ => qfsm::Track::Music,
        }
    }

    /// Carries out a command's queue transition.
    pub(super) fn run_queue(&mut self, step: qfsm::Step) {
        self.index = step.queue.index;
        match step.effect {
            qfsm::Effect::None => {}
            qfsm::Effect::Start => self.start_song(0),
            // The seek decides the play state (Event::Seek in seek_at).
            qfsm::Effect::Restart => { self.new_generation(); self.seek(0); return; }
            qfsm::Effect::Pause => self.pause(),
            qfsm::Effect::Resume => self.resume(),
            other => unreachable!("not a command effect: {other:?}"),
        }
        self.conforms(step.queue.play);
    }

    /// Debug builds check that the imperative shell reached the play state
    /// the queue machine decided on. Opening may already have resolved
    /// `Buffering`, a seek decides the state of a restart, and an output or
    /// storage fault may hold playback paused.
    pub(super) fn conforms(&self, play: PlayState) {
        debug_assert!(self.state == play || self.state == PlayState::Paused
            || (play != PlayState::Stopped && matches!(self.state, PlayState::Playing | PlayState::Buffering))
            || (play == PlayState::Stopped && self.queue.is_empty()),
            "player state {:?} does not refine the queue machine's {play:?}", self.state);
        debug_assert!(self.index < self.queue.len().max(1), "selection outside the queue");
    }

    pub(super) fn shuffle(&mut self, on: bool, keep_current: bool) {
        if on {
            if self.queue.len() < 2 {
                return;
            }
            if self.original.is_none() {
                self.original = Some(self.queue.clone());
            }
            // The shuffled order is the one copy; the original stays shared.
            let queue = Arc::make_mut(&mut self.queue);
            let current = queue.remove(self.index);
            fastrand::shuffle(queue);
            if keep_current {
                queue.insert(0, current);
                self.index = 0;
            } else {
                // A fresh shuffle starts anywhere.
                queue.push(current);
                fastrand::shuffle(queue);
                self.index = 0;
            }
        } else if let Some(orig) = self.original.take() {
            let id = self.queue.get(self.index).map(|s| s.id.clone());
            self.queue = orig;
            self.index = id.and_then(|id| self.queue.iter().position(|s| s.id == id)).unwrap_or(0);
        }
        self.save();
    }

    pub(super) fn skip_spoken(&mut self, forward: bool) -> bool {
        let Some(spoken) = self.queue.get(self.index).and_then(|s| s.spoken.as_ref()) else {
            return false;
        };
        let mut settings = self.source.spoken_skip();
        // Chapter preference is a book control. Podcasts always jump time
        // within the selected episode, including downloaded episodes.
        if matches!(&spoken.source, subsonic::SpokenSource::Audiobookshelf { episode: Some(_), .. }) {
            settings.skip_chapter = false;
        }
        if settings.skip_chapter && spoken.chapters.is_empty() {
            self.error = Some("This book has no chapters; choose Time in Settings → Playback → Book Skip Buttons".into());
            return true;
        }
        let absolute = self.logical_position_ms();
        let target = crate::library::spoken::skip_target(spoken, absolute, forward, settings);
        self.seek_spoken_absolute(target);
        self.save();
        true
    }

    pub(super) fn seek_spoken_absolute(&mut self, target: u64) {
        let (index, position) = crate::library::spoken::selection(&self.queue, self.index, target);
        if index == self.index {
            self.seek(position);
        } else {
            if !self.checkpoint_current() {
                self.pause();
                return;
            }
            let paused = self.state == PlayState::Paused;
            let generation = self.generation;
            let scrub_until = self.scrub_until;
            self.index = index;
            self.start_song(position);
            self.generation = generation;
            self.scrub_until = scrub_until;
            if paused {
                self.state = PlayState::Paused;
            }
        }
    }

    pub(super) fn new_generation(&mut self) {
        self.pause_admission_fault=None;
        self.generation = self.generation.wrapping_add(1);
        self.scrub_until = None;
        self.scrub_ended = false;
        self.seek_complete = false;
    }

    pub(super) fn start_song(&mut self, at_ms: u64) {
        self.new_generation();
        self.close_track();
        if let Some(s) = &mut self.sink {
            s.flush(self.blocking);
        }
        if self.queue.is_empty() {
            self.state = PlayState::Stopped;
            return;
        }
        self.resume_at = at_ms;
        self.base_ms = at_ms;
        self.frames = 0;
        self.state = PlayState::Buffering;
        self.error = None;
        self.load_speed();
    }

    pub(super) fn ended(&mut self) {
        let _capture = crate::diagnostics::PollScope::enter(201);
        self.end_trace(1);
        let scrubbing = self.scrub_until.is_some_and(|t| t > Instant::now());
        let step = qfsm::transition(self.cursor(), qfsm::Event::Ended { repeat: self.repeat, track: self.track_kind(), scrubbing });
        let previous_book = self.queue.get(self.index).and_then(|song| song.spoken.as_ref()).map(|book| book.id.clone());
        match step.effect {
            qfsm::Effect::FinishBook => {
                let current=self.queue[self.index].spoken.as_ref().unwrap();
                let duration=current.duration_ms.saturating_sub(current.offset_ms);
                if let Some(sink)=&mut self.sink{sink.drain(self.blocking);}
                self.observe_listening();
                if let Err(error)=self.checkpoints.checkpoint(&self.queue[self.index],duration,self.generation){self.error=Some(error);}
                self.close_track();self.release_output();self.base_ms=duration;self.frames=0;self.resume_at=duration;
                self.state=step.queue.play;self.source.save_queue(&self.queue,self.index,duration);
            }
            qfsm::Effect::RadioEnded => { self.error = Some("Radio stream ended; press Play to reconnect".into()); self.pause(); }
            qfsm::Effect::HoldScrub => self.scrub_ended = true,
            qfsm::Effect::StopAtEnd => {
                if previous_book.is_none(){self.new_generation();}
                if self.gapless { self.dec=None; } else { self.close_track(); }
                self.eq.reset();
                if let Some(s) = &mut self.sink {
                    s.drain(self.blocking);
                }
                self.state = step.queue.play;
                self.idle_since = Instant::now();
                self.base_ms = 0;
                self.frames = 0;
                self.save();
                // The publication below is forced (the machine proves it for
                // every entry into Stopped): a status sent under 200 ms
                // earlier would otherwise hold Stopped back until the next
                // command or the idle release, and the UI (and doze's
                // queue-end wake) would see Playing for up to 20 s.
            }
            qfsm::Effect::Advance => {
                if previous_book.is_none(){self.new_generation();}
                // Continuous albums must also keep continuous filter history. A new
                // sample format will reset it in Equalizer::prepare.
                if self.gapless { self.dec=None; } else { self.close_track(); }
                self.index = step.queue.index;
                if !self.gapless {
                    if let Some(s) = &mut self.sink {
                        s.drain(self.blocking);
                    }
                }
                // A logical book split into files keeps the live speed. Other
                // queue transitions reload the configured spoken preference.
                let same_book = previous_book.as_ref().is_some_and(|id| self.queue.get(self.index)
                    .and_then(|song| song.spoken.as_ref()).is_some_and(|book| &book.id == id));
                // The next song's frames follow the last one's in the PCM.
                self.base_ms = 0;
                self.frames = 0;
                self.resume_at = 0;
                self.state = step.queue.play;
                if !same_book { self.load_speed(); }
            }
            other => unreachable!("not an end-of-song effect: {other:?}"),
        }
        if step.publish { self.publish(true); }
    }

    pub(super) fn failed(&mut self, e: String) {
        let play = matches!(self.state, PlayState::Playing | PlayState::Buffering);
        if self.recover_descriptor(&e, play) || self.interrupt_stream(&e, play) { return; }
        let track = self.track_kind();
        let step = qfsm::transition(self.cursor(), qfsm::Event::Failed { track });
        match step.effect {
            qfsm::Effect::PauseWithError => {
                if track == qfsm::Track::Radio { log::warn!("radio playback: {e}"); }
                self.error = Some(e);
                self.pause();
            }
            qfsm::Effect::StopFailed => {
                log::warn!("playback: {e}");
                self.close_track();
                self.error = Some(e);
                self.state = step.queue.play;
                self.failures = step.queue.failures;
                // The output's idle release counts from the stop, as after
                // a pause or the queue's end.
                self.idle_since = Instant::now();
            }
            qfsm::Effect::SkipFailed => {
                log::warn!("playback: {e}");
                self.close_track();
                self.error = Some(e);
                self.failures = step.queue.failures;
                // Skip the song that won't play.
                self.index = step.queue.index;
                self.new_generation();
                self.base_ms = 0;
                self.frames = 0;
                self.resume_at = 0;
                self.publish(true);
                // Return to command dispatch before opening the next song.
                // The queue machine bounds consecutive failures; no retry
                // delay should hold up Pause, Next or shutdown here.
                return;
            }
            other => unreachable!("not a failure effect: {other:?}"),
        }
        self.publish(true);
    }
}
