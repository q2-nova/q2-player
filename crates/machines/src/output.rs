//! The output sink's lifecycle (`Engine::sink`, `Engine::parked` and the
//! `Sink`'s own pause state in `audio/alsa.rs`).
//!
//! The player owns at most one open PCM. Pausing keeps the unheard queue
//! (`Retained`), or cannot prove what the device still holds (`Unverified`).
//! Parking closes the PCM and powers the DAC down while keeping the retained
//! queue in RAM; resuming reopens a PCM with the same parameters and replays
//! it. Releasing drops everything.
//!
//! # Invariants
//!
//! * A PCM is open in exactly the `Open` states; `Parked` and `Closed` hold
//!   none, so an open PCM and a parked queue never coexist.
//! * Audio is accepted only by an open, running PCM: after `Release`, a park
//!   or a pause, every write is refused until the PCM is opened or resumed.
//! * Park then resume opens a PCM with the parameters that were parked.
//! * The idle deadline and the sleep park never close a PCM whose paused
//!   queue is unverified while the player is paused (an explicit resume must
//!   revalidate it); only an explicit release or a flush does. With the
//!   player not paused (stopped after a failure) nothing will resume from
//!   that queue, so both release it and power the DAC down.
//! * Every open PCM has an idle deadline: the player looks again at most
//!   [`IDLE_RELEASE_MS`] after playback stopped, whatever the flow.
//! * The PCM parameters change only through `Ensure` with new parameters
//!   (which drains the old PCM first) or after a release.

/// The parameters a PCM is opened with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub struct Params {
    /// Frames per second.
    pub rate: u32,
    /// Interleaved channels.
    pub channels: u32,
}

/// The highest sample rate a PCM is opened for.
pub const MAX_RATE: u32 = 768_000;
/// The most interleaved channels a PCM is opened for.
pub const MAX_CHANNELS: u32 = 8;

impl Params {
    /// Whether a PCM can be opened for these: a positive rate up to
    /// [`MAX_RATE`] and 1 to [`MAX_CHANNELS`] channels. The player refuses
    /// anything else before it reaches the output (a zero rate would divide
    /// by zero in the position arithmetic).
    pub fn valid(self) -> bool {
        self.rate > 0 && self.rate <= MAX_RATE && self.channels > 0 && self.channels <= MAX_CHANNELS
    }
}

/// What an open PCM is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum Flow {
    /// Accepting audio.
    Running,
    /// Paused; the unheard queue is held (by the device or in RAM).
    Retained,
    /// Paused; ownership of the device's queue could not be established.
    Unverified,
}

/// Where the output is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum Output {
    /// No PCM and nothing kept.
    Closed,
    /// A PCM is open.
    Open {
        /// Its parameters.
        params: Params,
        /// Running or paused.
        flow: Flow,
    },
    /// No PCM; the unheard queue is kept in RAM for a PCM with `params`.
    Parked {
        /// The parameters the kept queue was rendered for.
        params: Params,
    },
}

impl Output {
    /// A PCM handle is held.
    pub fn open(self) -> bool { matches!(self, Output::Open { .. }) }
    /// The open or parked parameters.
    pub fn params(self) -> Option<Params> {
        match self {
            Output::Closed => None,
            Output::Open { params, .. } | Output::Parked { params } => Some(params),
        }
    }
}

/// Close the PCM and power the DAC down after this long without playing.
pub const IDLE_RELEASE_MS: u64 = 20_000;

/// What happened to the output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(kani, derive(kani::Arbitrary))]
pub enum Event {
    /// Audio with these parameters is about to be written.
    Ensure(Params),
    /// The open the last effect asked for finished.
    Opened {
        /// The parameters asked for.
        params: Params,
        /// It succeeded.
        ok: bool,
    },
    /// Rendered audio is offered to the PCM.
    Write,
    /// The PCM was paused.
    Paused {
        /// The unheard queue is held.
        retained: bool,
        /// Ownership of the device queue is unproven.
        unverified: bool,
    },
    /// The unheard queue was discarded (seek, skip, new selection,
    /// unsupported pause): an open PCM runs empty, a parked queue is gone.
    Flushed,
    /// Park request (sleep barrier, `ParkOutput`). `paused`: the player is
    /// paused; `retainable`: the PCM can hand over its unheard queue.
    Park {
        /// The player is in the paused state.
        paused: bool,
        /// The unheard queue can be copied out.
        retainable: bool,
    },
    /// Nothing has played for `idle_ms`.
    Idle {
        /// Time since playback stopped.
        idle_ms: u64,
        /// The player is in the paused state.
        paused: bool,
        /// The unheard queue can be copied out.
        retainable: bool,
    },
    /// The owner resumes. `ok`: a paused PCM resumed; for a parked queue,
    /// the reopened PCM accepted the replay.
    Resume {
        /// The resume or replay succeeded.
        ok: bool,
    },
    /// Give the output up (AirPlay, USB export, quit, station pause).
    Release,
}

/// What the player does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Effect {
    /// Nothing.
    None,
    /// The open PCM already has these parameters.
    Reuse,
    /// Open a PCM with these parameters, then report [`Event::Opened`].
    Open(Params),
    /// Let the old PCM play out, close it, then open as [`Effect::Open`].
    DrainThenOpen(Params),
    /// The PCM takes the audio.
    Accept,
    /// There is no running PCM: the audio must be kept, not written.
    Refuse,
    /// Copy out the unheard queue, close the PCM, power the DAC down.
    Park,
    /// Close the PCM, drop anything kept, power the DAC down.
    Release,
    /// Resume the paused PCM.
    ResumeSink,
    /// Open a PCM with the parked parameters and replay the kept queue.
    Replay(Params),
}

fn park(output: Output, paused: bool, retainable: bool) -> (Output, Effect) {
    match output {
        Output::Closed | Output::Parked { .. } => (output, Effect::None),
        // Resume must revalidate a paused unverified queue; a stopped player
        // never resumes from it.
        Output::Open { flow: Flow::Unverified, .. } if paused => (output, Effect::None),
        Output::Open { params, .. } if paused && retainable => (Output::Parked { params }, Effect::Park),
        Output::Open { .. } => (Output::Closed, Effect::Release),
    }
}

/// The output's response to `event`.
pub fn transition(output: Output, event: Event) -> (Output, Effect) {
    match event {
        Event::Ensure(want) => match output {
            Output::Open { params, .. } if params == want => (output, Effect::Reuse),
            Output::Open { .. } => (Output::Closed, Effect::DrainThenOpen(want)),
            Output::Closed | Output::Parked { .. } => (output, Effect::Open(want)),
        },
        Event::Opened { params, ok } => match output {
            Output::Open { .. } => (output, Effect::None),
            Output::Closed | Output::Parked { .. } if ok => (Output::Open { params, flow: Flow::Running }, Effect::None),
            Output::Closed | Output::Parked { .. } => (output, Effect::None),
        },
        Event::Write => match output {
            Output::Open { flow: Flow::Running, .. } => (output, Effect::Accept),
            _ => (output, Effect::Refuse),
        },
        Event::Paused { retained, unverified } => match output {
            Output::Open { params, .. } => {
                let flow = if unverified { Flow::Unverified } else if retained { Flow::Retained } else { Flow::Running };
                (Output::Open { params, flow }, Effect::None)
            }
            _ => (output, Effect::None),
        },
        Event::Flushed => match output {
            Output::Open { params, .. } => (Output::Open { params, flow: Flow::Running }, Effect::None),
            Output::Parked { .. } | Output::Closed => (Output::Closed, Effect::None),
        },
        Event::Park { paused, retainable } => park(output, paused, retainable),
        Event::Idle { idle_ms, paused, retainable } => {
            if output.open() && idle_ms >= IDLE_RELEASE_MS { park(output, paused, retainable) } else { (output, Effect::None) }
        }
        Event::Resume { ok } => match output {
            Output::Parked { params } if ok => (Output::Open { params, flow: Flow::Running }, Effect::Replay(params)),
            Output::Parked { params } => (output, Effect::Replay(params)),
            Output::Open { params, flow: Flow::Retained | Flow::Unverified } if ok =>
                (Output::Open { params, flow: Flow::Running }, Effect::ResumeSink),
            Output::Open { .. } => (output, Effect::ResumeSink),
            Output::Closed => (output, Effect::None),
        },
        Event::Release => match output {
            Output::Closed => (output, Effect::None),
            _ => (Output::Closed, Effect::Release),
        },
    }
}

/// How long the player thread may wait before the idle deadline needs a
/// look: the rest of [`IDLE_RELEASE_MS`] while any PCM is open (a paused
/// unverified queue gets a revalidation there), otherwise `None` (wait for a
/// command).
pub fn idle_wait_ms(output: Output, idle_ms: u64) -> Option<u64> {
    match output {
        Output::Open { .. } => Some(IDLE_RELEASE_MS.saturating_sub(idle_ms)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::explore;

    const A: Params = Params { rate: 44_100, channels: 2 };
    const B: Params = Params { rate: 96_000, channels: 2 };

    fn events() -> Vec<Event> {
        let mut e = vec![Event::Write, Event::Flushed, Event::Release, Event::Resume { ok: true }, Event::Resume { ok: false }];
        for p in [A, B] {
            e.push(Event::Ensure(p));
            for ok in [false, true] { e.push(Event::Opened { params: p, ok }); }
        }
        for a in [false, true] { for b in [false, true] {
            e.push(Event::Paused { retained: a, unverified: b });
            e.push(Event::Park { paused: a, retainable: b });
            for idle_ms in [0, IDLE_RELEASE_MS - 1, IDLE_RELEASE_MS, u64::MAX] { e.push(Event::Idle { idle_ms, paused: a, retainable: b }); }
        } }
        e
    }

    /// Model state: the output plus two ghosts, "released since the last
    /// open" and "the parameters last parked".
    #[test]
    fn every_reachable_output_state_refuses_writes_unless_running_and_keeps_parked_params() {
        let all = events();
        let states = explore([(Output::Closed, true)], 1_000, |(output, released)| {
            all.iter().map(|event| {
                let (next, effect) = transition(*output, *event);
                // Writes are accepted by a running PCM only.
                if *event == Event::Write {
                    assert_eq!(effect == Effect::Accept, matches!(output, Output::Open { flow: Flow::Running, .. }));
                    if *released { assert_eq!(effect, Effect::Refuse, "write after release: {output:?}"); }
                }
                // Park/resume keeps the parameters.
                if let (Output::Parked { params }, Event::Resume { ok }) = (output, event) {
                    assert_eq!(effect, Effect::Replay(*params));
                    assert_eq!(next, if *ok { Output::Open { params: *params, flow: Flow::Running } } else { *output });
                }
                if effect == Effect::Park { assert_eq!(next.params(), output.params()); }
                // A paused unverified queue survives park and idle; a stopped
                // player's is released at the deadline.
                if matches!(output, Output::Open { flow: Flow::Unverified, .. }) {
                    match event {
                        Event::Park { paused: true, .. } | Event::Idle { paused: true, .. } => assert_eq!((next, effect), (*output, Effect::None)),
                        Event::Park { paused: false, .. } => assert_eq!((next, effect), (Output::Closed, Effect::Release)),
                        Event::Idle { paused: false, idle_ms, .. } => assert_eq!((next, effect),
                            if *idle_ms >= IDLE_RELEASE_MS { (Output::Closed, Effect::Release) } else { (*output, Effect::None) }),
                        _ => {}
                    }
                }
                // Parameters change only through Ensure/Opened or a release.
                if output.params().is_some() && next.params().is_some() && output.params() != next.params() {
                    assert!(matches!(event, Event::Opened { .. }) && !output.open(), "{output:?} --{event:?}--> {next:?}");
                }
                let released = match (next, event) {
                    (Output::Closed, _) => true,
                    (Output::Open { .. }, _) => false,
                    (Output::Parked { .. }, _) => *released,
                };
                (next, released)
            }).collect()
        }, |(output, released)| {
            if *released { assert!(!matches!(output, Output::Open { .. })); }
        });
        // Closed, 2 params x (3 open flows + parked).
        assert_eq!(states, 1 + 2 * 4);
    }

    #[test]
    fn the_idle_deadline_parks_a_paused_retained_queue_and_releases_otherwise() {
        let paused = Output::Open { params: A, flow: Flow::Retained };
        assert_eq!(transition(paused, Event::Idle { idle_ms: IDLE_RELEASE_MS - 1, paused: true, retainable: true }), (paused, Effect::None));
        assert_eq!(transition(paused, Event::Idle { idle_ms: IDLE_RELEASE_MS, paused: true, retainable: true }), (Output::Parked { params: A }, Effect::Park));
        let stopped = Output::Open { params: A, flow: Flow::Running };
        assert_eq!(transition(stopped, Event::Idle { idle_ms: IDLE_RELEASE_MS, paused: false, retainable: true }), (Output::Closed, Effect::Release));
        assert_eq!(idle_wait_ms(paused, 5_000), Some(15_000));
        assert_eq!(idle_wait_ms(Output::Open { params: A, flow: Flow::Unverified }, 5_000), Some(15_000));
        assert_eq!(idle_wait_ms(Output::Parked { params: A }, 0), None);
    }

    /// V0.128, 2026-11-22: a pause on an underrun (XRUN) PCM could not
    /// establish its queue, the next songs failed to open and the player
    /// stopped; the PCM stayed open with the DAC powered for minutes.
    #[test]
    fn a_stopped_player_releases_an_unverified_queue_at_the_deadline_and_a_paused_one_keeps_it() {
        let unverified = Output::Open { params: A, flow: Flow::Unverified };
        for retainable in [false, true] {
            assert_eq!(transition(unverified, Event::Idle { idle_ms: IDLE_RELEASE_MS - 1, paused: false, retainable }), (unverified, Effect::None));
            assert_eq!(transition(unverified, Event::Idle { idle_ms: IDLE_RELEASE_MS, paused: false, retainable }), (Output::Closed, Effect::Release));
            assert_eq!(transition(unverified, Event::Park { paused: false, retainable }), (Output::Closed, Effect::Release));
            assert_eq!(transition(unverified, Event::Idle { idle_ms: u64::MAX, paused: true, retainable }), (unverified, Effect::None));
            assert_eq!(transition(unverified, Event::Park { paused: true, retainable }), (unverified, Effect::None));
        }
        assert_eq!(idle_wait_ms(unverified, IDLE_RELEASE_MS + 1), Some(0));
    }

    #[test]
    fn only_openable_parameters_are_valid() {
        for (rate, channels, ok) in [(44_100, 2, true), (8_000, 1, true), (MAX_RATE, MAX_CHANNELS, true), (0, 2, false),
            (44_100, 0, false), (MAX_RATE + 1, 2, false), (48_000, MAX_CHANNELS + 1, false), (u32::MAX, u32::MAX, false)] {
            assert_eq!(Params { rate, channels }.valid(), ok, "{rate} Hz, {channels} ch");
        }
    }

    #[test]
    fn a_new_format_drains_the_old_pcm_before_opening() {
        let open = Output::Open { params: A, flow: Flow::Running };
        assert_eq!(transition(open, Event::Ensure(A)), (open, Effect::Reuse));
        assert_eq!(transition(open, Event::Ensure(B)), (Output::Closed, Effect::DrainThenOpen(B)));
        assert_eq!(transition(Output::Closed, Event::Opened { params: B, ok: false }), (Output::Closed, Effect::None));
    }
}

#[cfg(kani)]
mod proofs {
    use super::*;

    /// Symbolic over all `u32` rates and channel counts.
    #[kani::proof]
    fn park_then_resume_reopens_with_the_parked_parameters() {
        let params: Params = kani::any();
        let flow: Flow = kani::any();
        let (parked, effect) = transition(Output::Open { params, flow }, Event::Park { paused: kani::any(), retainable: kani::any() });
        if effect == Effect::Park {
            assert!(parked == Output::Parked { params });
            let ok: bool = kani::any();
            let (resumed, effect) = transition(parked, Event::Resume { ok });
            assert!(effect == Effect::Replay(params));
            if ok { assert!(resumed == Output::Open { params, flow: Flow::Running }); } else { assert!(resumed == parked); }
        }
    }

    #[kani::proof]
    fn only_a_running_pcm_accepts_audio() {
        let output: Output = kani::any();
        let (next, effect) = transition(output, Event::Write);
        assert!(next == output);
        assert!((effect == Effect::Accept) == matches!(output, Output::Open { flow: Flow::Running, .. }));
    }

    /// Bounded: every sequence of 5 events after a release.
    #[kani::proof]
    #[kani::unwind(6)]
    fn no_write_is_accepted_after_a_release_until_a_pcm_opens() {
        let start: Output = kani::any();
        let (mut output, _) = transition(start, Event::Release);
        assert!(output == Output::Closed);
        let mut opened = false;
        for _ in 0..5 {
            let event: Event = kani::any();
            let (next, effect) = transition(output, event);
            if let Event::Opened { ok: true, .. } = event { opened = true; }
            if !opened { assert!(effect != Effect::Accept && !next.open()); }
            output = next;
        }
    }

    #[kani::proof]
    fn a_paused_unverified_queue_is_never_closed_by_park_or_idle() {
        let params: Params = kani::any();
        let output = Output::Open { params, flow: Flow::Unverified };
        let event: Event = kani::any();
        kani::assume(matches!(event, Event::Park { paused: true, .. } | Event::Idle { paused: true, .. }));
        assert!(transition(output, event) == (output, Effect::None));
    }

    /// Every open PCM, whatever its flow, is released or parked at the idle
    /// deadline unless the player is paused on an unverified queue; and the
    /// player always has a deadline to look at while a PCM is open.
    #[kani::proof]
    fn every_open_pcm_has_a_deadline_and_a_stopped_player_closes_it() {
        let output: Output = kani::any();
        let idle_ms: u64 = kani::any();
        let retainable: bool = kani::any();
        let paused: bool = kani::any();
        if output.open() {
            assert!(idle_wait_ms(output, idle_ms) == Some(IDLE_RELEASE_MS.saturating_sub(idle_ms)));
            let (next, effect) = transition(output, Event::Idle { idle_ms, paused, retainable });
            if idle_ms >= IDLE_RELEASE_MS && !paused { assert!(!next.open() && effect == Effect::Release); }
            if idle_ms >= IDLE_RELEASE_MS && paused && !matches!(output, Output::Open { flow: Flow::Unverified, .. }) {
                assert!(!next.open());
            }
        } else {
            assert!(idle_wait_ms(output, idle_ms).is_none());
        }
    }

    /// The player probes with `retainable: false` before it copies the
    /// queue out: whether anything happens does not depend on that flag.
    #[kani::proof]
    fn whether_a_park_acts_does_not_depend_on_retainability() {
        let output: Output = kani::any();
        let paused: bool = kani::any();
        let idle_ms: u64 = kani::any();
        for (probe, real) in [
            (Event::Park { paused, retainable: false }, Event::Park { paused, retainable: true }),
            (Event::Idle { idle_ms, paused, retainable: false }, Event::Idle { idle_ms, paused, retainable: true }),
        ] {
            assert!((transition(output, probe).1 == Effect::None) == (transition(output, real).1 == Effect::None));
        }
    }

    /// Bounded: every sequence of 4 events; parameters of a held output
    /// change only by opening from a state with no PCM.
    #[kani::proof]
    #[kani::unwind(5)]
    fn held_parameters_change_only_through_an_open() {
        let mut output: Output = kani::any();
        for _ in 0..4 {
            let event: Event = kani::any();
            let (next, _) = transition(output, event);
            if let (Some(before), Some(after)) = (output.params(), next.params()) {
                if before != after { assert!(matches!(event, Event::Opened { ok: true, .. }) && !output.open()); }
            }
            output = next;
        }
    }
}
