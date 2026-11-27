//! The play queue's cursor and play state (`audio/player.rs`, `Engine`).
//!
//! The state is the queue's length, the selected index, the play state and
//! the count of consecutive songs that failed to open or decode. The songs
//! themselves, positions, generations and the decoder stay in the player;
//! this machine decides which entry is selected, whether playback continues,
//! and when a forced status publication is owed.
//!
//! # Invariants
//!
//! For every state `q` with [`Queue::valid`] and every event `e`,
//! `transition(q, e)` returns a state that is again valid:
//!
//! * `index < len`, or `len == 0 && index == 0`;
//! * an empty queue is never `Playing` or `Buffering`;
//! * `failures < 3` ([`MAX_FAILURES`]).
//!
//! and:
//!
//! * a transition that enters `Stopped` from another state sets
//!   [`Step::publish`] (the UI and the Wi-Fi doze policy must see it at once);
//! * the natural end of the last song with repeat off, outside a scrub
//!   gesture, always ends in `Stopped` with `publish` set;
//! * a command never selects an entry outside the queue, whatever index or
//!   count it carries.

/// What the player is doing with the selected entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum Play {
    /// Nothing is selected for playback (empty queue, or the queue ended).
    #[default]
    Stopped,
    /// Audio is being decoded and written.
    Playing,
    /// Held by the owner or by an output or storage fault.
    Paused,
    /// Playing, waiting for the song to open or for a stream's bytes.
    Buffering,
}

impl Play {
    /// `Playing` or `Buffering`: the player thread decodes rather than waits.
    pub fn active(self) -> bool { matches!(self, Play::Playing | Play::Buffering) }
}

/// What follows the end of a song.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum Repeat {
    /// Stop after the last song.
    #[default]
    Off,
    /// Wrap from the last song to the first.
    All,
    /// Play the same song again.
    One,
}

/// The kind of the selected entry, as far as queue transitions care.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum Track {
    /// An ordinary song.
    Music,
    /// A live station: it never advances, repeats or is skipped on failure.
    Radio,
    /// One file of a book or podcast episode. `continues`: the next queue
    /// entry is a later file of the same book.
    Book {
        /// The next entry belongs to the same book.
        continues: bool,
    },
}

/// The queue cursor and play state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub struct Queue {
    /// Number of entries.
    pub len: usize,
    /// The selected entry; 0 when the queue is empty.
    pub index: usize,
    /// The play state.
    pub play: Play,
    /// Consecutive songs that failed since the last one that opened.
    pub failures: usize,
}

impl Queue {
    /// The invariant every transition preserves (module docs).
    pub fn valid(self) -> bool {
        self.failures < MAX_FAILURES
            && if self.len == 0 { self.index == 0 && !self.play.active() } else { self.index < self.len }
    }
}

/// Consecutive unplayable songs skipped before playback stops.
pub const MAX_FAILURES: usize = 3;

/// Something that happened to the queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum Event {
    /// The queue was replaced (`Cmd::Play` with `autoplay`, `Cmd::Restore`
    /// without). `index` is the caller's, unvalidated.
    Load {
        /// Length of the new queue.
        len: usize,
        /// Requested entry.
        index: usize,
        /// Start playing (else stay paused at the restored position).
        autoplay: bool,
    },
    /// The owner's play/pause key.
    PlayPause,
    /// Pause if playing.
    Pause,
    /// Resume the paused instance; `current` is false when the request was
    /// made for an earlier playback instance and must be ignored.
    Resume {
        /// The request names the current playback instance.
        current: bool,
    },
    /// Skip forward.
    Next {
        /// The repeat mode in force.
        repeat: Repeat,
    },
    /// Skip back; `restart` when the song is more than three seconds in.
    Prev {
        /// Restart the current song rather than select the previous one.
        restart: bool,
    },
    /// Select an entry by index (unvalidated).
    Jump(usize),
    /// `count` entries are being inserted after the current one (`next`) or
    /// at the end.
    Insert {
        /// How many entries.
        count: usize,
        /// After the current entry rather than at the end.
        next: bool,
    },
    /// Remove an entry by index (unvalidated).
    Remove(usize),
    /// The decoder reached the end of the selected entry.
    Ended {
        /// The repeat mode in force (ignored for books).
        repeat: Repeat,
        /// The entry's kind.
        track: Track,
        /// A scrub gesture is in progress: hold at the end instead.
        scrubbing: bool,
    },
    /// The selected entry could not be opened or decoded.
    Failed {
        /// The entry's kind.
        track: Track,
    },
    /// The selected entry opened and its first packet is about to play.
    Opened,
    /// The selected entry is being opened (or reopened after a release).
    Opening,
    /// The output accepted audio.
    Accepted,
    /// A seek within the selected entry (which a station ignores).
    Seek {
        /// The selected entry is a live station.
        radio: bool,
    },
    /// Playback was held from outside the queue: the sleep barrier, an
    /// output park, an output fault or a failed position save.
    Held,
}

/// What the player must do after a transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Effect {
    /// Nothing.
    None,
    /// Begin a new playback instance of the selected entry from its start
    /// (`Engine::start_song`).
    Start,
    /// Begin a new playback instance by seeking the open entry to zero.
    Restart,
    /// Pause the output and save the position.
    Pause,
    /// Resume the paused output.
    Resume,
    /// Splice the new entries in at `at`; `arm` when the queue was empty and
    /// the first new entry becomes the paused selection.
    Insert {
        /// Where the entries go.
        at: usize,
        /// The queue was empty: select entry 0, paused, at position 0.
        arm: bool,
    },
    /// Remove the entry, then act on the selection as told.
    Remove(AfterRemove),
    /// The book ended: checkpoint its end, release the output, stop.
    FinishBook,
    /// The station's stream ended: pause with the reconnect notice.
    RadioEnded,
    /// The end was reached under a scrub gesture: wait for the gesture.
    HoldScrub,
    /// The queue ended: drain the output and stop.
    StopAtEnd,
    /// Continue with the selected entry (same PCM when gapless).
    Advance,
    /// A book or station failed: pause and show the error.
    PauseWithError,
    /// Too many failures, or the last entry failed: stop and show the error.
    StopFailed,
    /// Skip the entry that failed and try the selected one.
    SkipFailed,
}

/// What happens to the selection once an entry has been removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AfterRemove {
    /// The selected entry is unaffected (its index may have shifted).
    Keep,
    /// The selected entry was the last: close it and stop at index 0.
    Stop,
    /// The selected entry was playing: start its successor.
    Start,
    /// The selected entry was paused or stopped: its successor takes its
    /// place, at position 0, in the same play state.
    Reload,
}

/// The result of a transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Step {
    /// The next state.
    pub queue: Queue,
    /// What the player must do to realise it.
    pub effect: Effect,
    /// The status must be published now, not at the next periodic slot.
    pub publish: bool,
}

/// `index` if it names an entry of a queue of `len`, else the last entry (0
/// for an empty queue).
pub fn clamp_index(len: usize, index: usize) -> usize { index.min(len.saturating_sub(1)) }

/// The queue's response to `event`. Total: every state and event has a
/// result, and an event that does not apply leaves the state unchanged with
/// [`Effect::None`].
pub fn transition(q: Queue, event: Event) -> Step {
    let stay = Step { queue: q, effect: Effect::None, publish: true };
    let go = |queue: Queue, effect: Effect| Step { queue, effect, publish: true };
    match event {
        Event::Load { len, index, autoplay } => {
            let play = if len == 0 { Play::Stopped } else if autoplay { Play::Buffering } else { Play::Paused };
            go(Queue { len, index: clamp_index(len, index), play, failures: q.failures },
                if autoplay { Effect::Start } else { Effect::None })
        }
        Event::PlayPause => match q.play {
            Play::Playing | Play::Buffering => go(Queue { play: Play::Paused, ..q }, Effect::Pause),
            Play::Paused if q.len > 0 => go(Queue { play: Play::Buffering, ..q }, Effect::Resume),
            Play::Stopped if q.len > 0 => go(Queue { play: Play::Buffering, ..q }, Effect::Start),
            Play::Paused | Play::Stopped => stay,
        },
        Event::Pause => if q.play.active() { go(Queue { play: Play::Paused, ..q }, Effect::Pause) } else { stay },
        Event::Resume { current } => {
            if current && q.play == Play::Paused && q.len > 0 {
                go(Queue { play: Play::Buffering, ..q }, Effect::Resume)
            } else { stay }
        }
        Event::Next { repeat } => {
            if q.len > 0 && q.index + 1 < q.len {
                go(Queue { index: q.index + 1, play: Play::Buffering, ..q }, Effect::Start)
            } else if repeat == Repeat::All && q.len > 0 {
                go(Queue { index: 0, play: Play::Buffering, ..q }, Effect::Start)
            } else { stay }
        }
        Event::Prev { restart } => {
            if q.len == 0 { stay }
            else if restart || q.index == 0 { go(q, Effect::Restart) }
            else { go(Queue { index: q.index - 1, play: Play::Buffering, ..q }, Effect::Start) }
        }
        Event::Jump(i) => {
            if i < q.len { go(Queue { index: i, play: Play::Buffering, ..q }, Effect::Start) } else { stay }
        }
        Event::Insert { count, next } => {
            let Some(len) = q.len.checked_add(count) else { return stay };
            if q.len == 0 {
                go(Queue { len, index: 0, play: Play::Paused, failures: q.failures }, Effect::Insert { at: 0, arm: true })
            } else {
                let at = if next { q.index + 1 } else { q.len };
                go(Queue { len, ..q }, Effect::Insert { at, arm: false })
            }
        }
        Event::Remove(i) => {
            if i >= q.len { return stay; }
            let len = q.len - 1;
            let failures = q.failures;
            if i < q.index {
                go(Queue { len, index: q.index - 1, failures, ..q }, Effect::Remove(AfterRemove::Keep))
            } else if i > q.index {
                go(Queue { len, failures, ..q }, Effect::Remove(AfterRemove::Keep))
            } else if q.index >= len {
                go(Queue { len, index: 0, play: Play::Stopped, failures }, Effect::Remove(AfterRemove::Stop))
            } else if q.play.active() {
                go(Queue { len, play: Play::Buffering, failures, ..q }, Effect::Remove(AfterRemove::Start))
            } else {
                go(Queue { len, failures, ..q }, Effect::Remove(AfterRemove::Reload))
            }
        }
        Event::Ended { repeat, track, scrubbing } => {
            if q.len == 0 { return go(Queue { play: Play::Stopped, ..q }, Effect::StopAtEnd); }
            match track {
                Track::Book { continues: false } if !scrubbing =>
                    return go(Queue { play: Play::Stopped, ..q }, Effect::FinishBook),
                Track::Radio => return go(Queue { play: Play::Paused, ..q }, Effect::RadioEnded),
                _ => {}
            }
            if scrubbing { return go(q, Effect::HoldScrub); }
            let last = q.index + 1 >= q.len;
            let repeat = if matches!(track, Track::Book { .. }) { Repeat::Off } else { repeat };
            match repeat {
                Repeat::One => Step { queue: Queue { play: Play::Buffering, ..q }, effect: Effect::Advance, publish: false },
                Repeat::All if last => Step { queue: Queue { index: 0, play: Play::Buffering, ..q }, effect: Effect::Advance, publish: false },
                Repeat::Off if last => go(Queue { play: Play::Stopped, ..q }, Effect::StopAtEnd),
                _ => Step { queue: Queue { index: q.index + 1, play: Play::Buffering, ..q }, effect: Effect::Advance, publish: false },
            }
        }
        Event::Failed { track } => {
            if q.len == 0 { return go(Queue { play: Play::Stopped, failures: 0, ..q }, Effect::StopFailed); }
            if !matches!(track, Track::Music) {
                return go(Queue { play: Play::Paused, ..q }, Effect::PauseWithError);
            }
            let failures = q.failures + 1;
            if failures >= q.len.min(MAX_FAILURES) || q.index + 1 >= q.len {
                go(Queue { play: Play::Stopped, failures: 0, ..q }, Effect::StopFailed)
            } else {
                go(Queue { index: q.index + 1, failures, ..q }, Effect::SkipFailed)
            }
        }
        Event::Opened => Step { queue: Queue { failures: 0, ..q }, effect: Effect::None, publish: false },
        Event::Accepted => {
            let play = if q.play == Play::Buffering { Play::Playing } else { q.play };
            Step { queue: Queue { play, ..q }, effect: Effect::None, publish: false }
        }
        Event::Opening => {
            let play = if q.play.active() { Play::Buffering } else { q.play };
            Step { queue: Queue { play, ..q }, effect: Effect::None, publish: true }
        }
        Event::Seek { radio } => {
            // A seek from Stopped plays from the new position; a paused seek
            // stays paused. An empty queue has nothing to seek in.
            let play = if radio || q.len == 0 || q.play == Play::Paused { q.play } else { Play::Buffering };
            go(Queue { play, ..q }, Effect::None)
        }
        Event::Held => go(Queue { play: Play::Paused, ..q }, Effect::None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::explore;

    const REPEATS: [Repeat; 3] = [Repeat::Off, Repeat::All, Repeat::One];
    const TRACKS: [Track; 4] = [Track::Music, Track::Radio, Track::Book { continues: false }, Track::Book { continues: true }];

    /// Every event over a small domain of indices and counts.
    fn events(bound: usize) -> Vec<Event> {
        let mut e = vec![Event::PlayPause, Event::Pause, Event::Resume { current: true }, Event::Resume { current: false },
            Event::Prev { restart: true }, Event::Prev { restart: false }, Event::Opened, Event::Opening, Event::Accepted,
            Event::Seek { radio: false }, Event::Seek { radio: true }, Event::Held];
        for repeat in REPEATS {
            e.push(Event::Next { repeat });
            for track in TRACKS { for scrubbing in [false, true] { e.push(Event::Ended { repeat, track, scrubbing }); } }
        }
        for track in TRACKS { e.push(Event::Failed { track }); }
        for i in 0..=bound + 1 {
            e.push(Event::Jump(i));
            e.push(Event::Remove(i));
            for autoplay in [false, true] { for len in 0..=bound { e.push(Event::Load { len, index: i, autoplay }); } }
        }
        e.push(Event::Jump(usize::MAX));
        e.push(Event::Remove(usize::MAX));
        e.push(Event::Load { len: bound, index: usize::MAX, autoplay: true });
        for count in 0..=2 { for next in [false, true] { e.push(Event::Insert { count, next }); } }
        e.push(Event::Insert { count: usize::MAX, next: true });
        e
    }

    /// Explicit-state model check: every state reachable from the empty
    /// stopped queue with at most BOUND entries satisfies the invariants,
    /// and every transition out of it satisfies the step properties.
    #[test]
    fn every_reachable_queue_state_is_valid_and_stops_are_published() {
        const BOUND: usize = 5;
        let all = events(BOUND);
        let mut transitions = 0usize;
        let states = explore([Queue { len: 0, index: 0, play: Play::Stopped, failures: 0 }], 10_000, |q| {
            all.iter().filter_map(|e| {
                let step = transition(*q, *e);
                transitions += 1;
                assert!(step.queue.valid(), "{q:?} --{e:?}--> {:?}", step.queue);
                if step.queue.play == Play::Stopped && q.play != Play::Stopped {
                    assert!(step.publish, "unpublished stop: {q:?} --{e:?}");
                }
                if step.effect == Effect::None && !matches!(e, Event::Load { .. } | Event::Opened | Event::Opening | Event::Accepted | Event::Seek { .. } | Event::Held) {
                    assert_eq!(step.queue, *q, "an ignored event changed the state: {e:?}");
                }
                if let Event::Ended { repeat: Repeat::Off, track: Track::Music, scrubbing: false } = e {
                    if q.index + 1 >= q.len {
                        assert_eq!((step.queue.play, step.effect, step.publish), (Play::Stopped, Effect::StopAtEnd, true));
                    }
                }
                if let Effect::Insert { at, .. } = step.effect { assert!(at <= q.len); }
                (step.queue.len <= BOUND).then_some(step.queue)
            }).collect()
        }, |q| assert!(q.valid()));
        // Every valid state is reachable. len 0: Stopped and Paused; len n:
        // n indices x 4 play states; each with 0..3 failures.
        assert_eq!(states, MAX_FAILURES * (2 + (1..=BOUND).map(|n| n * 4).sum::<usize>()));
        assert!(transitions > 10_000);
    }

    #[test]
    fn play_pause_on_an_empty_paused_queue_does_nothing() {
        let q = Queue { len: 0, index: 0, play: Play::Paused, failures: 0 };
        assert_eq!(transition(q, Event::PlayPause).effect, Effect::None);
        assert_eq!(transition(q, Event::Resume { current: true }).effect, Effect::None);
    }

    #[test]
    fn consecutive_failures_stop_after_three_songs_or_at_the_end() {
        let mut q = Queue { len: 10, index: 0, play: Play::Buffering, failures: 0 };
        let mut skipped = 0;
        loop {
            let step = transition(q, Event::Failed { track: Track::Music });
            q = step.queue;
            if step.effect == Effect::StopFailed { break; }
            skipped += 1;
        }
        assert_eq!((skipped, q.index, q.play), (2, 2, Play::Stopped));
        let last = Queue { len: 10, index: 9, play: Play::Buffering, failures: 0 };
        assert_eq!(transition(last, Event::Failed { track: Track::Music }).effect, Effect::StopFailed);
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// Unbounded in `len`, `index`, counts and indices (all `usize`).
    #[kani::proof]
    fn transition_preserves_validity_and_never_overflows() {
        let q: Queue = kani::any();
        kani::assume(q.valid());
        let e: Event = kani::any();
        let step = transition(q, e);
        assert!(step.queue.valid());
        if let Effect::Insert { at, .. } = step.effect { assert!(at <= q.len); }
    }

    #[kani::proof]
    fn entering_stopped_is_always_published() {
        let q: Queue = kani::any();
        kani::assume(q.valid());
        let step = transition(q, kani::any());
        if step.queue.play == Play::Stopped && q.play != Play::Stopped { assert!(step.publish); }
    }

    #[kani::proof]
    fn the_last_song_ending_with_repeat_off_stops_and_publishes() {
        let q: Queue = kani::any();
        kani::assume(q.valid() && q.index + 1 >= q.len);
        let step = transition(q, Event::Ended { repeat: Repeat::Off, track: Track::Music, scrubbing: false });
        assert!(step.queue.play == Play::Stopped && step.effect == Effect::StopAtEnd && step.publish);
    }

    #[kani::proof]
    fn an_empty_queue_is_never_made_active() {
        let q: Queue = kani::any();
        kani::assume(q.valid());
        let step = transition(q, kani::any());
        if step.queue.len == 0 { assert!(!step.queue.play.active() && step.queue.index == 0); }
    }

    #[kani::proof]
    fn a_station_or_book_never_skips_on_failure_and_a_station_never_advances() {
        let q: Queue = kani::any();
        kani::assume(q.valid() && q.len > 0);
        let track: Track = kani::any();
        kani::assume(!matches!(track, Track::Music));
        let failed = transition(q, Event::Failed { track });
        assert!(failed.queue.index == q.index && failed.queue.play == Play::Paused);
        let ended = transition(q, Event::Ended { repeat: kani::any(), track: Track::Radio, scrubbing: kani::any() });
        assert!(ended.queue.index == q.index && ended.queue.play == Play::Paused);
    }
}
