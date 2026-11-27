#![forbid(unsafe_code)]
//! The player: a thread that owns the queue, the decoder and the PCM.
//! Command dispatch and publication live here; queue transitions, PCM ownership
//! and decoded buffering are implemented in private child modules.

mod queue;
mod output;
mod radio_listen;
pub use radio_listen::RadioListen;
mod buffering;
mod recovery;

use super::alsa::{AudioHw, Sink};
use crate::config::ReplayGainMode;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use subsonic::Song;
use symphonia::core::codecs::{CODEC_TYPE_NULL, Decoder, DecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::{SeekMode, SeekTo};
use symphonia::core::io::MediaSource;
use symphonia::core::units::{Time, TimeBase};

/// Where a song's bytes come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Downloaded for offline listening.
    Offline,
    /// Kept from an earlier stream.
    Cached,
    /// Streaming now (and being kept).
    Streaming,
    Radio,
}

/// An opened song: its bytes and what the format probe may assume.
pub struct OpenedTrack {
    /// The container's bytes from offset 0. A stream's reader blocks in
    /// `read` until its bytes arrive.
    pub source: Box<dyn MediaSource>,
    /// The file extension, a hint for the format probe.
    pub ext: Option<String>,
    /// Where the bytes come from (shown in the status).
    pub origin: Origin,
    /// A station's live "now playing" text; `None` for files.
    pub radio_metadata: Option<subsonic::radio::Metadata>,
}

impl OpenedTrack {
    /// A live station's stream.
    pub fn radio(radio: super::radio::Radio) -> Self {
        Self { radio_metadata: Some(radio.metadata()), source: Box::new(radio), ext: None, origin: Origin::Radio }
    }
}

/// What the player needs from the library.
///
/// The player thread is the only caller and makes one call at a time. The
/// UI thread uses the library concurrently, so implementations lock for
/// themselves, and they never call back into the player. Every `String`
/// error is a message for the owner: it is published in `Status::error`.
pub trait TrackSource: Send + Sync + 'static {
    /// Opens `song` for reading.
    ///
    /// Returns without waiting for a download: a stream returns a reader
    /// whose `read` blocks until its bytes arrive. `interrupt` is the
    /// cancellation flag for this open and for the returned reader's waits;
    /// the caller sets it when a command supersedes the song.
    ///
    /// On success the source yields the container's bytes from offset 0.
    /// The error `"interrupted"` means `interrupt` cut the wait: the player
    /// treats it as a command, not a failure. Any other error counts as a
    /// failed song (skipped; playback stops after three in a row).
    fn open(&self, _blocking: &crate::rt::Blocking, song: &Song, interrupt: &Arc<crate::audio::interrupt::Interrupt>) -> Result<OpenedTrack, String>;
    /// As [`open`](Self::open), but returns only once the whole file is on
    /// the card: for MP4 files whose movie metadata is at the end, and for
    /// seeks a partial stream cannot serve.
    fn open_complete(&self, blocking: &crate::rt::Blocking, song: &Song, interrupt: &Arc<crate::audio::interrupt::Interrupt>) -> Result<OpenedTrack,String> {
        self.open(blocking,song,interrupt)
    }
    /// The songs likely to play next, in queue order (at most 32), to fetch
    /// ahead. Called each time a song opens; must not block on the network.
    fn upcoming(&self, songs: &[Song]);
    /// A song started within its first second: tell the server it is
    /// playing now. Must not block on the network.
    fn now_playing(&self, song: &Song);
    /// A song longer than 30 s reached half its length or four minutes,
    /// whichever is first; called at most once per playback instance.
    /// `started_ms` is when it started, in epoch milliseconds.
    fn played(&self, song: &Song, started_ms: u64);
    /// Snapshot direct-service opt-in and account before the playback starts.
    fn direct_listen_generation(&self) -> Option<u64> { None }
    fn direct_song_played(&self, _generation: u64, _song: &Song, _started_ms: u64) {}
    fn direct_radio_played(&self, _generation: u64, _station: &Song, _artist: &str, _title: &str, _started_ms: u64, _kind: RadioListen) {}
    /// The queue or position changed, to be saved. `index` names an entry of
    /// `queue` (or is 0 for an empty queue) and `position_ms` is the audible
    /// position in it.
    fn save_queue(&self, queue: &[Song], index: usize, position_ms: u64);
    /// Where to resume the book or podcast at `queue[index]`: the entry and
    /// the position in it, from the durable bookmark, or `fallback_ms` in
    /// `index` when there is none. The player clamps the returned index to
    /// the queue. An error means the bookmark could not be read: the player
    /// keeps the previous queue and pauses rather than start at zero.
    fn spoken_resume(&self,_queue:&[Song],index:usize,fallback_ms:u64)->Result<(usize,u64),String> {Ok((index,fallback_ms))}
    /// The owner's skip settings for books and podcasts, read at each skip.
    fn spoken_skip(&self)->crate::config::SpokenSettings {Default::default()}
    /// The saved tempo for `song`'s book or series, in percent; `None`
    /// follows Settings. An error pauses playback.
    fn spoken_speed(&self,_song:&Song)->Result<Option<u16>,String> {Ok(None)}
    /// Saves the tempo for `song`'s book or series. `percent` is `None`
    /// (follow Settings) or 50..=300 in steps of five: the player validates
    /// it before the call. The tempo changes only if this returns `Ok`.
    fn set_spoken_speed(&self,_song:&Song,_percent:Option<u16>)->Result<(),String> {Ok(())}
    /// Durably records the listening position in a book or podcast. `Ok`
    /// means it is on the card. An error pauses playback and is shown.
    fn checkpoint(&self,_song:&Song,_position_ms:u64)->Result<(),String>{Ok(())}
    /// Async persistence boundary. Production submits directly to its DB owner;
    /// synthetic/custom sources retain their finite synchronous callback.
    fn checkpoint_async(self: Arc<Self>, song: Song, position_ms: u64, _session: Option<super::listening::Session>) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>> {
        Box::pin(async move {
            crate::rt::spawn_blocking(move |_| self.checkpoint(&song, position_ms)).await
                .expect("listening location callback stopped")
        })
    }
}

/// The repeat mode and the play state are the queue machine's
/// (`q2_machines::queue`, docs/state-machines.md).
pub use q2_machines::queue::{Play as PlayState, Repeat};
use q2_machines::{handoff, output as outfsm, queue as qfsm};

/// A snapshot of the player, published by the player thread and read with
/// [`PlayerHandle::status`].
///
/// Guarantees (checked by the property tests in `player_machine_tests.rs`):
/// `index` names an entry of `queue`, or is 0 when the queue is empty; an
/// empty queue is never `Playing` or `Buffering`; `generation` never
/// decreases; while `Playing`, `output_owned` is true and `sample_rate` is
/// the open PCM's rate.
#[derive(Debug, Clone)]
pub struct Status {
    /// What the player is doing.
    pub state: PlayState,
    /// The queue; a new `Arc` only when its contents or order change.
    pub queue: Arc<Vec<Song>>,
    /// The audible entry.
    pub index: usize,
    /// Changes on every new playback instance, including repeat-one.
    pub generation: u64,
    /// Changes on every completed seek.
    pub seek_revision: u64,
    /// The audible position: in the song, or in the whole book.
    pub position_ms: u64,
    /// The song's (or the whole book's) length; 0 when unknown.
    pub duration_ms: u64,
    /// The repeat mode.
    pub repeat: Repeat,
    /// The queue is shuffled.
    pub shuffle: bool,
    /// The volume, 0-100.
    pub volume: u8,
    /// Effective tempo; ordinary music and radio always remain at 100%.
    pub speed_percent: u16,
    /// Persisted book or podcast-series customization; None follows Settings.
    pub speed_override: Option<u16>,
    /// Last completed picker request, including rejected storage writes.
    pub speed_request: u64,
    /// Where the open song's bytes come from; `None` when none is open.
    pub origin: Option<Origin>,
    /// What the decoder sees, e.g. "FLAC 24/96".
    pub format: Option<String>,
    /// Decoded stream rate for frequency-response views; absent before opening.
    pub sample_rate: Option<u32>,
    /// The last failure, for the owner; cleared when playback succeeds.
    pub error: Option<String>,
    /// That failure's fixed code (crate::ux::Code), for automatic recovery.
    pub error_code: Option<u16>,
    /// Numeric output metadata; no device I/O is performed to publish this.
    pub output_revision:u64,
    pub output_usb_card:Option<u32>,
    pub output_usb_device:Option<u32>,
    pub output_owned:bool,
    pub pause_admission_failed:bool,
    pub output_pause_kind:u8,
    pub output_pause_frames:u64,
    /// Ephemeral song information; queue entries continue to identify stations.
    pub radio_title: Option<String>,
}

impl Default for Status {
    fn default() -> Self {
        Self { state: PlayState::Stopped, queue: Arc::new(Vec::new()), index: 0,
            generation: 0, seek_revision: 0, position_ms: 0, duration_ms: 0,
            repeat: Repeat::Off, shuffle: false, volume: 0, speed_percent: 100, speed_override: None, speed_request: 0,
            origin: None, format: None, sample_rate: None, error: None, error_code: None, output_revision:0, output_usb_card:None, output_usb_device:None, output_owned:false, pause_admission_failed:false, output_pause_kind:0, output_pause_frames:0, radio_title: None }
    }
}

impl Status {
    /// Compare all semantic state; ordinary media position is the sole omission.
    /// Exhaustive binding and unused-variable denial make new fields explicit.
    #[deny(unused_variables)]
    pub(crate) fn same_non_position(&self, other: &Self) -> bool {
        let Self { position_ms: _, state, queue, index, generation, seek_revision, duration_ms, repeat, shuffle, volume, speed_percent, speed_override, speed_request, origin, format, sample_rate, error, error_code, output_revision, output_usb_card, output_usb_device, output_owned, pause_admission_failed, output_pause_kind, output_pause_frames, radio_title } = self;
        state == &other.state
            && Arc::ptr_eq(queue, &other.queue)
            && index == &other.index
            && generation == &other.generation
            && seek_revision == &other.seek_revision
            && duration_ms == &other.duration_ms
            && repeat == &other.repeat
            && shuffle == &other.shuffle
            && volume == &other.volume
            && speed_percent == &other.speed_percent
            && speed_override == &other.speed_override
            && speed_request == &other.speed_request
            && origin == &other.origin
            && format == &other.format
            && sample_rate == &other.sample_rate
            && error == &other.error
            && error_code == &other.error_code
            && output_revision == &other.output_revision
            && output_usb_card == &other.output_usb_card
            && output_usb_device == &other.output_usb_device
            && output_owned == &other.output_owned
            && pause_admission_failed == &other.pause_admission_failed
            && output_pause_kind == &other.output_pause_kind
            && output_pause_frames == &other.output_pause_frames
            && radio_title == &other.radio_title
    }

    pub fn current(&self) -> Option<&Song> {
        self.queue.get(self.index)
    }

    pub fn is_current_radio(&self, song: &Song) -> bool {
        self.state != PlayState::Stopped && same_station(self.current(), song)
    }

    pub fn is_active(&self) -> bool {
        matches!(self.state, PlayState::Playing | PlayState::Buffering)
    }
}

fn same_station(current: Option<&Song>, selected: &Song) -> bool {
    selected.radio_url.as_ref().is_some_and(|url|
        current.and_then(|s| s.radio_url.as_ref()) == Some(url))
}

/// A command to the player thread, sent with [`PlayerHandle::send`].
///
/// Indices and counts are not trusted: the queue machine
/// (`q2_machines::queue`) clamps a `Play`/`Restore` index and ignores a
/// `Jump` or `Remove` outside the queue. Positions are clamped inside the
/// song, the volume to 100, and a tempo outside 50..=300 is ignored.
pub enum Cmd {
    /// Replace the queue and play `songs[index]`. The queue shares `songs`
    /// (a list's rows hold the same allocation) until it changes.
    Play { songs: Arc<Vec<Song>>, index: usize, shuffle: bool },
    /// Replace the queue, paused at a position (boot resume).
    Restore { songs: Arc<Vec<Song>>, index: usize, position_ms: u64 },
    PlayPause,
    /// Resume only the paused instance observed by a delayed UI action.
    Resume { generation: u64 },
    /// Apply the refreshed bookmark only to the still-paused observed instance.
    ResumeSpoken { generation: u64, seek_revision: u64 },
    Pause,
    SetSleepTimer { deadline: Option<Instant> },
    StartSleepFade { deadline: Instant },
    FinishSleepTimer { deadline: Instant, generation: u64, rewind_ms: u64 },
    Next,
    Prev,
    /// A seek gesture is tied to one playback instance; stale UI cannot seek its successor.
    Scrub { generation: u64, position: Option<u64>, active: bool },
    SeekBy(i64),
    Jump(usize),
    /// After the current song, or at the end.
    Insert { songs: Vec<Song>, next: bool },
    /// Complete the queue started by one specific Shuffle Songs request.
    AppendShuffle { seed: Arc<Vec<Song>>, songs: Vec<Song> },
    Remove(usize),
    SetRepeat(Repeat),
    SetShuffle(bool),
    SetVolume(u8),
    /// Whether the output sets its own level from the volume (Bluetooth headphones
    /// with absolute volume). On that route the samples then stay at unity: the
    /// volume is the headphones' amplifier, not a second attenuation here.
    SetRemoteVolume(bool),
    /// Pitch-preserving spoken tempo, 50..=300 in steps of five.
    SetSpeed { generation: u64, percent: Option<u16>, request: u64 },
    /// Re-resolve after the configured default has been published to the library.
    RefreshSpeed,
    /// The Headphone Power level; it reaches the codecs only with the
    /// headphone PCM closed (a pause closes it, see `Engine::apply_headphone_power`).
    SetHeadphonePower(crate::config::HeadphonePower),
    SetFilter(u8),
    SetReplayGain(ReplayGainMode),
    SetGapless(bool),
    SetEq(super::eq::Settings),
    /// Stop and let go of the output.
    Release,
    /// Stop electrical output while retaining the paused media session.
    ParkOutput,
    /// A topology event wakes the owner; transitions run after its command batch.
    OutputChanged,
    /// Save and release on the audio owner, then prevent late worker commands.
    PrepareHandoff(HandoffAck),
    /// Durable retained-RAM sleep barrier; never releases the media session.
    PrepareSleep(HandoffAck),
    CancelHandoff,
    /// Explicit generated measurement. Never interrupts I/O or publishes to the UI.
    ObserveMusicCursor {
        deadline: Instant,
        reply: tokio::sync::oneshot::Sender<Result<crate::numeric_probe::MusicCursor, &'static str>>,
    },
    Quit,
}

/// Production barriers wake an async observer. The synchronous variant exists
/// only for direct engine/state-machine fixtures, not runtime callers.
pub enum HandoffAck {
    Async(tokio::sync::oneshot::Sender<Result<(), &'static str>>),
    #[cfg(test)]
    Fixture(Sender<Result<(), &'static str>>),
}
impl HandoffAck {
    fn send(self, result: Result<(), &'static str>) -> Result<(), ()> {
        match self {
            Self::Async(reply) => reply.send(result).map_err(|_| ()),
            #[cfg(test)]
            Self::Fixture(reply) => reply.send(result).map_err(|_| ()),
        }
    }
}
#[cfg(test)]
impl From<Sender<Result<(), &'static str>>> for HandoffAck {
    fn from(reply: Sender<Result<(), &'static str>>) -> Self { Self::Fixture(reply) }
}

struct PendingHandoff {
    owner: Option<(PlayerHandle, u32)>,
    reply: Option<tokio::sync::oneshot::Receiver<Result<(), &'static str>>>,
}
impl Drop for PendingHandoff {
    fn drop(&mut self) {
        // Cancel is queued before the shared admission bit is released, so a
        // later attempt cannot overtake cleanup of this accepted request.
        drop(self.reply.take());
        if let Some((player, revision)) = self.owner.take() { player.cancel_handoff_revision(Some(revision)); }
    }
}

#[derive(Clone)]
struct IoSession {cancel:Arc<crate::audio::interrupt::Interrupt>,ready:bool}

/// The UI's handle on the player thread. Cloneable; every clone talks to
/// the same thread.
#[derive(Clone)]
pub struct PlayerHandle {
    tx: Arc<Sender<Cmd>>,
    command_gate: Arc<Mutex<()>>,
    interrupt: Arc<crate::audio::interrupt::Interrupt>,
    current_io: Arc<Mutex<Option<IoSession>>>,
    status: Arc<Mutex<Status>>,
    visible: Arc<AtomicBool>,
    worker: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
    handoff_pending: Arc<AtomicBool>,
    handoff_revision: Arc<AtomicU32>,
    trace: Arc<crate::playtrace::Trace>,
}

/// Play-control commands whose request and completion times are traced
/// (crate::playtrace). The sender and the player number them in the same
/// order, so both must classify a command identically.
fn request_code(cmd: &Cmd) -> Option<u32> {
    Some(match cmd {
        Cmd::Play { .. } => 1,
        Cmd::PlayPause => 2,
        Cmd::Resume { .. } | Cmd::ResumeSpoken { .. } => 3,
        Cmd::Pause => 4,
        Cmd::Next => 5,
        Cmd::Prev => 6,
        Cmd::Jump(_) => 7,
        Cmd::SeekBy(_) => 8,
        Cmd::Scrub { position: Some(_), .. } => 9,
        Cmd::SetVolume(_) => 10,
        _ => return None,
    })
}

fn origin_code(origin: Origin) -> u32 {
    match origin { Origin::Offline => 1, Origin::Cached => 2, Origin::Streaming => 3, Origin::Radio => 4 }
}

fn state_code(state: PlayState) -> u32 {
    match state { PlayState::Stopped => 0, PlayState::Playing => 1, PlayState::Paused => 2, PlayState::Buffering => 3 }
}

impl PlayerHandle {
    /// Deterministic UI boundary: retain the production command gate/handle,
    /// observe commands without an audio worker or host scheduling races.
    #[cfg(test)]
    pub(crate) fn fixture(status: Status) -> (Self, Receiver<Cmd>) {
        let (tx, rx) = mpsc::channel();
        (Self { tx:Arc::new(tx), command_gate: Arc::new(Mutex::new(())), interrupt: Arc::new(crate::audio::interrupt::Interrupt::new(false)),
            status: Arc::new(Mutex::new(status)), current_io: Arc::new(Mutex::new(None)), visible: Arc::new(AtomicBool::new(true)),
            worker: Arc::new(Mutex::new(None)), handoff_pending: Arc::new(AtomicBool::new(false)), handoff_revision: Arc::new(AtomicU32::new(0)),
            trace: Arc::new(crate::playtrace::Trace::new(false)) }, rx)
    }

    #[cfg(test)]
    pub(crate) fn fixture_status(&self, status: Status) { *self.status.lock().unwrap() = status; }

    /// Caller holds command_gate so a newly opening source cannot miss this
    /// command. Ready spoken reads survive pause; initial probing is cancelled.
    fn interrupt_locked(&self) {
        self.interrupt.store(true,Ordering::Relaxed);
        if let Some(io)=self.current_io.lock().unwrap().as_ref().filter(|io|!io.ready) {
            io.cancel.store(true,Ordering::Relaxed);
        }
    }

    /// Queues `cmd` and, for commands that supersede the current I/O, cuts
    /// the player's wait for a stream's bytes so a skip never waits on the
    /// network. Never blocks on the player. Commands are handled in the
    /// order sent; while a sleep or storage barrier is held they are dropped
    /// (all but `Quit`).
    pub fn send(&self, cmd: Cmd) {
        let same_radio = matches!(&cmd, Cmd::Play { songs, index: 0, shuffle: false }
            if songs.len() == 1 && self.status.lock().unwrap().is_current_radio(&songs[0]));
        let speed_noop = match &cmd {
            Cmd::SetSpeed { generation, .. } => {
                let status = self.status.lock().unwrap();
                *generation != status.generation || !status.current().is_some_and(|song| song.spoken.is_some())
            }
            Cmd::RefreshSpeed => !self.status.lock().unwrap().current().is_some_and(|song| song.spoken.is_some()),
            _ => false,
        };
        let cuts = !same_radio && !speed_noop && !matches!(cmd, Cmd::Insert { .. } | Cmd::AppendShuffle { .. } | Cmd::ObserveMusicCursor { .. } | Cmd::OutputChanged | Cmd::Resume { .. } | Cmd::ResumeSpoken { .. } | Cmd::SetEq(_) | Cmd::SetVolume(_) | Cmd::SetRemoteVolume(_) | Cmd::SetSleepTimer { .. } | Cmd::StartSleepFade { .. } | Cmd::SetHeadphonePower(_) | Cmd::SetFilter(_) | Cmd::SetRepeat(_) | Cmd::Scrub { position: None, .. });
        let _gate = self.command_gate.lock().unwrap();
        if let Some(code) = request_code(&cmd) { self.trace.request(code); }
        let _ = self.tx.send(cmd);
        if cuts {
            // Sending and clearing a batch's interrupt are serialized. A
            // command must never leave its own seek marked interrupted.
            self.interrupt_locked();
        }
    }

    /// Whether the screen shows the player: hidden, periodic position
    /// updates no longer wake the UI (state changes still do).
    pub fn set_visible(&self, visible: bool) {
        self.visible.store(visible, Ordering::Relaxed);
    }

    /// The latest published snapshot. Does not wait for the player.
    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    /// The player owns pause/checkpoint/output work; callers await its reply
    /// without occupying a finite worker. Admission is eager, and abandoning
    /// even an unpolled observer queues cancellation of only this acquisition.
    pub fn prepare_handoff_async(&self) -> impl std::future::Future<Output = Result<(), &'static str>> + Send + 'static {
        self.prepare_barrier_async(Duration::from_secs(8), false)
    }

    /// Retain the paused media session for sleep. Success transfers the held
    /// barrier to the caller, which must cancel it if the later handoff fails.
    pub fn prepare_sleep_async(&self) -> impl std::future::Future<Output = Result<(), &'static str>> + Send + 'static {
        self.prepare_barrier_async(Duration::from_secs(8), true)
    }

    fn prepare_barrier_async(&self, timeout: Duration, sleep: bool)
        -> impl std::future::Future<Output = Result<(), &'static str>> + Send + 'static {
        let deadline = Instant::now() + timeout;
        let admitted = (|| {
            let (receiver, revision, sent) = {
                let _gate = self.command_gate.lock().unwrap();
                if self.handoff_pending.swap(true, Ordering::AcqRel) {
                    return Err("A power or storage transition is already pending");
                }
                let revision = self.handoff_revision.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
                let (ack, receiver) = tokio::sync::oneshot::channel();
                let ack = HandoffAck::Async(ack);
                let sent = self.tx.send(if sleep { Cmd::PrepareSleep(ack) } else { Cmd::PrepareHandoff(ack) }).is_ok();
                if sent { self.interrupt_locked(); }
                (receiver, revision, sent)
            };
            // On failed send, cancellation runs only after the gate unlocks.
            let pending = PendingHandoff { owner: Some((self.clone(), revision)), reply: Some(receiver) };
            if !sent { return Err("Audio player is unavailable; transition cancelled"); }
            Ok(pending)
        })();
        async move {
            let mut pending = admitted?;
            let result = tokio::time::timeout_at(tokio::time::Instant::from_std(deadline),
                pending.reply.as_mut().expect("admitted reply")).await;
            // Drop uses this same receiver-before-cancel order on abandonment.
            drop(pending.reply.take());
            match result {
                Ok(Ok(Ok(()))) => {
                    let current = {
                        let (player, revision) = pending.owner.as_ref().expect("admitted handoff");
                        let _gate = player.command_gate.lock().unwrap();
                        player.handoff_revision.load(Ordering::Relaxed) == *revision
                            && player.handoff_pending.load(Ordering::Acquire)
                    };
                    if !current { return Err("Power or storage transition was cancelled"); }
                    pending.owner.take();
                    Ok(())
                }
                Ok(Ok(Err(error))) => Err(error),
                Ok(Err(_)) => Err("Audio player is unavailable; transition cancelled"),
                Err(_) => Err("Audio player is busy; transition cancelled"),
            }
        }
    }

    // Ordinary fixture compatibility executes the same async protocol. There
    // is no production worker that parks while waiting for a player reply.
    #[cfg(test)]
    pub fn prepare_handoff(&self) -> Result<(), &'static str> {
        self.prepare_barrier(Duration::from_secs(8), false)
    }
    #[cfg(test)]
    pub fn prepare_sleep(&self) -> Result<(), &'static str> {
        self.prepare_barrier(Duration::from_secs(8), true)
    }
    #[cfg(test)]
    fn prepare_barrier(&self, timeout: Duration, sleep: bool) -> Result<(), &'static str> {
        assert!(crate::rt::may_wait_for_tasks(), "await the player barrier on runtime/pool callers");
        let prepared = self.prepare_barrier_async(timeout, sleep);
        let (send, reply) = tokio::sync::oneshot::channel();
        crate::rt::io().spawn(async move { let _ = send.send(prepared.await); });
        reply.blocking_recv().unwrap_or(Err("Audio player is unavailable; transition cancelled"))
    }

    /// Restore ordinary commands when launch failed after a successful ack.
    pub fn cancel_handoff(&self) { self.cancel_handoff_revision(None); }

    fn cancel_handoff_revision(&self, revision: Option<u32>) {
        let _gate = self.command_gate.lock().unwrap();
        if revision.is_some_and(|revision| self.handoff_revision.load(Ordering::Relaxed) != revision
            || !self.handoff_pending.load(Ordering::Acquire)) { return; }
        let _ = self.tx.send(Cmd::CancelHandoff);
        self.interrupt_locked();
        self.handoff_pending.store(false, Ordering::Release);
    }

    /// Finish the final queue save and release audio before process shutdown.
    pub fn shutdown(&self) {
        self.send(Cmd::Quit);
        let mut worker = self.worker.lock().unwrap();
        if let Some(thread) = worker.take() {
            if thread.join().is_err() {
                log::error!("player thread panicked during shutdown");
            }
        }
    }
}

/// The playback settings the player starts with.
pub struct Settings {
    pub eq: super::eq::Settings,
    pub volume: u8,
    pub headphone_power: crate::config::HeadphonePower,
    pub filter: u8,
    pub replay_gain: ReplayGainMode,
    pub gapless: bool,
}

/// Starts the player thread. `notify` is called (from the player thread)
/// whenever a status worth redrawing was published; it must not block.
pub fn spawn(
    source: Arc<dyn TrackSource>,
    hw: Box<dyn AudioHw>,
    settings: Settings,
    notify: Arc<dyn Fn() + Send + Sync>,
) -> PlayerHandle {
    spawn_outputs(source,hw,settings,notify,None)
}

pub fn spawn_outputs(
    source:Arc<dyn TrackSource>,hw:Box<dyn AudioHw>,settings:Settings,
    notify:Arc<dyn Fn()+Send+Sync>,outputs:Option<super::output::Monitor>,
)->PlayerHandle {
    spawn_traced(source,hw,settings,notify,outputs,crate::playtrace::global())
}

/// As `spawn_outputs`, recording into `trace` (the process's, or a test's own).
pub(crate) fn spawn_traced(
    source:Arc<dyn TrackSource>,hw:Box<dyn AudioHw>,settings:Settings,
    notify:Arc<dyn Fn()+Send+Sync>,mut outputs:Option<super::output::Monitor>,trace:Arc<crate::playtrace::Trace>,
)->PlayerHandle {
    let (tx, rx) = mpsc::channel();
    let tx=Arc::new(tx);
    let interrupt = Arc::new(crate::audio::interrupt::Interrupt::new(false));
    let command_gate = Arc::new(Mutex::new(()));
    let status = Arc::new(Mutex::new(Status { volume: settings.volume, ..Status::default() }));
    let visible = Arc::new(AtomicBool::new(true));
    let current_io:Arc<Mutex<Option<IoSession>>>=Arc::new(Mutex::new(None));
    if let Some(outputs)=&mut outputs {
        let sender=Arc::downgrade(&tx);
        let gate=command_gate.clone();
        let pending=interrupt.clone();
        let io=current_io.clone();
        outputs.set_notify(Arc::new(move|| {
            let Some(sender)=sender.upgrade() else {return;};
            let _gate=gate.lock().unwrap();
            let _=sender.send(Cmd::OutputChanged);
            // Only interrupt the audio owner's bounded spoken-packet wait.
            // Initial probing and direct music I/O must never be cancelled by
            // a device change. The independent demux/read-ahead owner survives.
            if io.lock().unwrap().as_ref().is_some_and(|io|
                io.ready && !Arc::ptr_eq(&io.cancel,&pending)) {
                pending.store(true,Ordering::Relaxed);
            }
        }));
    }
    let route=outputs.as_ref().map(|outputs|outputs.snapshot()).unwrap_or(super::output::Snapshot {
        generation:0,route:super::output::Route::Headphones,
    });
    let worker_inputs = (source, hw, settings, notify, outputs, trace.clone(), visible.clone(), status.clone(), interrupt.clone(), current_io.clone(), command_gate.clone(), route);
    let worker = std::thread::Builder::new()
        .name("player".into())
        .spawn(move || {
            let (source, hw, settings, notify, outputs, trace, visible, status, interrupt, current_io, command_gate, route) = worker_inputs;
            let blocking = crate::rt::Blocking::dedicated_thread();
            let mut engine = Engine {
                blocking: &blocking,
                tap: crate::playtrace::Tap::new(trace.clone()),
                visible: visible.clone(),
                generation: 0,
                seek_revision: 0,
                seek_complete: false,
                scrub_until: None,
                scrub_ended: false,
                speed_percent: 100,
                speed_override: None,
                speed_request: 0,
                tempo: None,
                tempo_output: Vec::new(),
                checkpoints: super::checkpoints::Writer::new(source.clone()),
                listening: Default::default(),
                source,
                hw,
                status: status.clone(),
                notify,
                interrupt: interrupt.clone(),
                current_io:current_io.clone(),
                command_gate: command_gate.clone(),
                queue: Arc::default(),
                original: None,
                index: 0,
                state: PlayState::Stopped,
                sink: None,
                parked: None,
                pending_output: None,
                wide_buffers: Default::default(),
                outputs,
                route,
                resume_output:false,
                pause_admission_fault:None,
                bluetooth_fault:None,
                dec: None,
                resume_at: 0,
                base_ms: 0,
                frames: 0,
                volume: settings.volume,
                remote_volume: false,
                soft_volume: 1.0,
                output_gain: 1.0,
                hardware_volume: false,
                applied_codec_steps: None,
                source_format:Default::default(),
                applied_volume: soft_gain(settings.volume),
                volume_ramp: super::gain::GainRamp::default(),
                audible_volume_ramp: Default::default(),
                sleep_fade: None,
                sleep_timer_deadline: None,
                track_gain: 1.0,
                replay_peak_reserve: None,
                rg: settings.replay_gain,
                gapless: settings.gapless,
                repeat: Repeat::Off,
                started_ms: 0,
                scrobbled: false,
                direct_listen_generation: None,
                radio_listen: Default::default(),
                radio_external_metadata: false,
                direct_music_started_ms: 0,
                failures: 0,
                descriptor_recovery: Default::default(),
                idle_since: Instant::now(),
                published: Instant::now(),
                checkpointed: Instant::now(),
                error: None,
                ux: Default::default(),
                samples: super::decoder::Samples::default(),
                eq: super::eq::Equalizer::new(settings.eq),
                queue_rev: 0,
                published_rev: u64::MAX,
                published_state: PlayState::Stopped,
                handoff_frozen: false,
                headphone_power: settings.headphone_power,
                applied_power: None,
                sink_internal: true,
            };
            // No PCM is open yet, so the level applies at once.
            engine.apply_headphone_power();
            engine.hw.set_filter(engine.blocking, settings.filter);
            if let Err(error) = engine.apply_volume() { engine.error = Some(error); }
            engine.run(rx);
        })
        .expect("player thread");
    PlayerHandle { tx, interrupt, current_io, status, visible, command_gate, worker: Arc::new(Mutex::new(Some(worker))), handoff_pending: Arc::new(AtomicBool::new(false)), handoff_revision: Arc::new(AtomicU32::new(0)), trace }
}

struct Decoding {
    radio_metadata: Option<subsonic::radio::Metadata>,
    complete_for_seek: bool,
    discard_frames: u64,
    format: super::packets::Packets,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    time_base: Option<TimeBase>,
    rate: u32,
    channels: u32,
    duration_ms: u64,
    origin: Origin,
    label: String,
}

#[derive(Clone, Copy)]
enum SleepFade {
    Falling { deadline: Instant },
    Restoring { start: Instant, from: f64 },
}

struct Engine<'a> {
    // Created on the player thread; the engine cannot move to an executor.
    blocking: &'a crate::rt::Blocking,
    /// Bounded playback telemetry (crate::playtrace).
    tap: crate::playtrace::Tap,
    command_gate: Arc<Mutex<()>>,
    visible: Arc<AtomicBool>,
    generation: u64,
    seek_revision: u64,
    seek_complete: bool,
    scrub_until: Option<Instant>,
    scrub_ended: bool,
    speed_percent: u16,
    speed_override: Option<u16>,
    speed_request: u64,
    tempo: Option<super::tempo::Tempo<'a>>,
    tempo_output: Vec<i32>,
    checkpoints: super::checkpoints::Writer,
    listening: super::listening::Tracker,
    source: Arc<dyn TrackSource>,
    hw: Box<dyn AudioHw>,
    status: Arc<Mutex<Status>>,
    notify: Arc<dyn Fn() + Send + Sync>,
    interrupt: Arc<crate::audio::interrupt::Interrupt>,
    current_io: Arc<Mutex<Option<IoSession>>>,
    /// Shared with the list it was played from and with the published
    /// status; copied only when it changes (Arc::make_mut).
    queue: Arc<Vec<Song>>,
    /// The queue's order before shuffling.
    original: Option<Arc<Vec<Song>>>,
    index: usize,
    state: PlayState,
    sink: Option<Sink<'a>>,
    parked: Option<super::retained::RetainedPcm>,
    /// Rendered once but not yet accepted by a PCM. Decoder/DSP state is ahead
    /// of this buffer and must not advance again until it is submitted.
    pending_output: Option<super::retained::RetainedPcm>,
    /// The audible values and rendered samples of the last codec-volume
    /// packet, kept for the next (`wide_output`).
    wide_buffers: (Vec<f64>, Vec<i32>),
    outputs:Option<super::output::Monitor>,
    route:super::output::Snapshot,
    resume_output:bool,
    pause_admission_fault:Option<String>,
    /// The error the Bluetooth output last reported; a disconnect that follows clears it.
    bluetooth_fault:Option<String>,
    dec: Option<Decoding>,
    /// Where to start the current song when it opens, ms.
    resume_at: u64,
    /// The song position at `frames` = 0, ms.
    base_ms: u64,
    /// Frames of this song written since `base_ms`.
    frames: u64,
    volume: u8,
    /// The Bluetooth headphones apply `volume` themselves (`Cmd::SetRemoteVolume`).
    remote_volume: bool,
    soft_volume: f32,
    output_gain: f64,
    hardware_volume: bool,
    /// Last verified physical register; it can differ from the PCM's logical
    /// attenuation on Efficient and is invalid after any mixer failure.
    applied_codec_steps: Option<u8>,
    source_format:super::pcm_format::PcmFormat,
    applied_volume: f32,
    volume_ramp: super::gain::GainRamp,
    /// Listener-volume ramp applied before saturation when the codec gain stays fixed.
    audible_volume_ramp: super::gain::GainRamp,
    sleep_fade: Option<SleepFade>,
    sleep_timer_deadline: Option<Instant>,
    track_gain: f32,
    /// Extra half-dB steps of codec headroom discovered after a positive
    /// ReplayGain tag supplied a usable peak. None uses ordinary planning.
    replay_peak_reserve: Option<u8>,
    rg: ReplayGainMode,
    gapless: bool,
    repeat: Repeat,
    started_ms: u64,
    scrobbled: bool,
    direct_listen_generation: Option<u64>,
    radio_listen: radio_listen::Listen,
    radio_external_metadata: bool,
    direct_music_started_ms: u64,
    failures: usize,
    descriptor_recovery: recovery::DescriptorRecovery,
    idle_since: Instant,
    published: Instant,
    checkpointed: Instant,
    error: Option<String>,
    /// UX telemetry: the presented notice and the last stop-relevant command.
    ux: UxPlayer,
    samples: super::decoder::Samples,
    eq: super::eq::Equalizer,
    /// Bumped whenever the queue's contents or order change.
    queue_rev: u64,
    published_rev: u64,
    /// State in the last published snapshot: a change publishes at once, even dark.
    published_state: PlayState,
    handoff_frozen: bool,
    /// The owner's Headphone Power.
    headphone_power: crate::config::HeadphonePower,
    /// The last level written to the internal codecs and the level they
    /// then held (a codec driver without patch 0187 has two); None until
    /// written, or after a failed write (no volume make-up gain then). The
    /// asked level differs from `headphone_power` while a change waits for
    /// the headphone PCM to close.
    applied_power: Option<(crate::config::HeadphonePower, crate::config::HeadphonePower)>,
    /// The open sink was opened on the headphone route (the internal codecs);
    /// `route` can move on while it is still open. True when unknown.
    sink_internal: bool,
}

/// Latency of the PCM's buffer: room for a stall of the card or the network.
const LATENCY_US: u32 = 400_000;
/// Close the PCM and power the DAC down after this long without playing.
const IDLE_RELEASE: Duration = Duration::from_millis(outfsm::IDLE_RELEASE_MS);

struct OutputFailure {
    accepted:u64,
    pending:super::retained::RetainedPcm,
    error:String,
}
fn submit_rendered(blocking: &crate::rt::Blocking, sink:&mut Sink<'_>,pcm:&[i32],tap:&mut crate::playtrace::Tap)->Result<u64,OutputFailure> {
    submit_output(blocking,sink,pcm,None,tap)
}
fn submit_retained(blocking:&crate::rt::Blocking,sink:&mut Sink<'_>,pcm:&super::retained::RetainedPcm,tap:&mut crate::playtrace::Tap)->Result<u64,OutputFailure> {
    submit_output(blocking,sink,&pcm.samples,Some(pcm),tap)
}
fn submit_output(blocking:&crate::rt::Blocking,sink:&mut Sink<'_>,pcm:&[i32],retained:Option<&super::retained::RetainedPcm>,tap:&mut crate::playtrace::Tap)->Result<u64,OutputFailure> {
    let _poll=crate::diagnostics::PollScope::enter(206);
    let started=tap.now();
    let write=match retained {Some(held)=>sink.write_retained(blocking,held),None=>sink.write_progress(blocking,pcm)};
    let result=write.map(|()|(pcm.len()/sink.channels as usize) as u64)
        .map_err(|error|OutputFailure {
            accepted:error.accepted_frames,
            pending:retained.map(|held|held.suffix(error.accepted_frames as usize*sink.channels as usize))
                .unwrap_or_else(||super::retained::RetainedPcm::at_output_gain(
                    pcm[error.accepted_frames as usize*sink.channels as usize..].to_vec(),sink.rate,sink.channels,sink.output_gain()).with_format(sink.format)),
            error:error.message,
        });
    let accepted=match &result {Ok(frames)=>*frames,Err(failure)=>failure.accepted};
    if accepted>0 {crate::ux::pcm_written();}
    let (recovered,dropped)=sink.recovered();
    tap.written(started,accepted,sink.rate,sink.pre_write_delay(),recovered,dropped);
    sink.clear_recovered();
    result
}

fn epoch_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn time_from_ms(ms: u64) -> Time {
    Time::new(ms / 1000, (ms % 1000) as f64 / 1000.0)
}

fn ms_from_time(t: Time) -> u64 {
    t.seconds * 1000 + (t.frac * 1000.0) as u64
}

#[cfg(test)]
thread_local! {
    static PUBLICATION_METADATA_ENTERED: std::cell::RefCell<Option<Sender<()>>> = const { std::cell::RefCell::new(None) };
}
fn publication_radio_title(metadata: &subsonic::radio::Metadata) -> Option<String> {
    #[cfg(test)]
    PUBLICATION_METADATA_ENTERED.with(|entered| {
        if let Some(entered)=entered.borrow_mut().take() { entered.send(()).unwrap(); }
    });
    metadata.lock().unwrap().clone()
}

impl Engine<'_> {
    fn commands(&mut self, rx: &Receiver<Cmd>, first: Option<Cmd>) -> bool {
        let commands = {
            let _gate = self.command_gate.lock().unwrap();
            let commands: Vec<_> = first.into_iter().chain(rx.try_iter()).collect();
            self.interrupt.store(false, Ordering::Relaxed);
            commands
        };
        let mut commands = commands.into_iter().peekable();
        while let Some(mut cmd) = commands.next() {
            // A wheel burst represents one latest desired speed for this
            // instance. Retain barriers/other commands in their original order.
            while matches!((&cmd, commands.peek()),
                (Cmd::SetSpeed { generation, .. }, Some(Cmd::SetSpeed { generation: next, percent, .. }))
                if generation == next && percent.is_none_or(super::tempo::valid_speed)) {
                cmd = commands.next().unwrap();
            }
            let more = self.handle(cmd);
            self.tap.command_done();
            if !more { return false; }
        }
        true
    }

    fn run(mut self, rx: Receiver<Cmd>) {
        crate::diagnostics::register(3);
        crate::health::register(b'A');
        loop {
            crate::health::pulse();
            // At EOF while jogging, wait for another gesture or its lease to
            // expire. Never spin on EOF or advance underneath the user's hand.
            if self.scrub_ended && self.state != PlayState::Paused {
                let wait = self.scrub_until.map(|t| t.saturating_duration_since(Instant::now())).unwrap_or_default();
                if wait.is_zero() {
                    self.scrub_ended = false;
                    self.scrub_until = None;
                    self.ended();
                } else {
                    match rx.recv_timeout(crate::health::wait(wait)) {
                        Ok(cmd) => { if !self.commands(&rx, Some(cmd)) { return; } }
                        Err(RecvTimeoutError::Disconnected) => return,
                        Err(RecvTimeoutError::Timeout) => {}
                    }
                    continue;
                }
            }
            let active = matches!(self.state, PlayState::Playing | PlayState::Buffering);
            if !active {
                // Commands wake the channel immediately. No polling is needed
                // while waiting for the paused DAC's release deadline.
                let timeout=self.idle_wait();
                match rx.recv_timeout(crate::health::wait(timeout)) {
                    Ok(cmd) => {
                        if !self.commands(&rx, Some(cmd)) {
                            return;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        self.idle();
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            if !self.commands(&rx, None) { return; }
            self.refresh_route();
            if !matches!(self.state, PlayState::Playing | PlayState::Buffering) {
                self.publish(true);
                continue;
            }
            if !self.flush_pending_output() {self.publish(true);continue;}
            if self.dec.is_none() {
                match self.open_current() {
                    Ok(()) => {}
                    // A command cut the wait for the stream: not a failure.
                    Err(e) if e == INTERRUPTED => {
                        self.close_track();
                        continue;
                    }
                    Err(e) if self.seek_complete => {
                        if self.recover_descriptor(&e, true) { continue; }
                        self.error = Some(format!("Unable to seek: {e}"));
                        self.pause();
                        self.publish(true);
                        continue;
                    }
                    Err(e) => {
                        self.failed(e);
                        continue;
                    }
                }
            }
            if self.dec.is_none() || !matches!(self.state, PlayState::Playing | PlayState::Buffering) { continue; }
            match self.step() {
                Ok(true) => {}
                Ok(false) => self.ended(),
                Err(e) => self.failed(e),
            }
            self.publish(false);
        }
    }

    /// Applies a command; false to quit.
    fn handle(&mut self, cmd: Cmd) -> bool {
        let _poll=crate::diagnostics::PollScope::enter(203);
        // Compare on the queue owner, after earlier commands and before any
        // persistence or playback side effects. Even a new queue with the
        // same first song belongs to a different request.
        let cmd = match cmd {
            Cmd::AppendShuffle { seed, songs } => {
                if !Arc::ptr_eq(&self.queue, &seed) || songs.is_empty() { return true; }
                Cmd::Insert { songs, next: false }
            }
            other => other,
        };
        self.observe_listening();
        if let Some(code) = request_code(&cmd) { self.tap.handled(code); crate::ux::player_cmd(code); }
        self.ux.command(&cmd, self.state);
        if matches!(cmd, Cmd::CancelHandoff) {
            self.gate(handoff::Event::Cancel);
            self.sleep_probe("unfrozen");
            return true;
        }
        if let Cmd::PrepareSleep(ack) = cmd {
            let result = if self.gate(handoff::Event::Prepare) == handoff::Effect::Refuse {
                Err("A power or storage transition is already pending")
            } else {
                let decoder_loaded = self.dec.is_some() && !self.is_radio();
                let retained = matches!(self.pause_output(), Ok(true));
                // Refuse loss of retained PCM rather than reopening the source
                // and silently throwing away the user's buffered session.
                if !retained || (decoder_loaded && self.dec.is_none()) || (self.sink.is_some()
                    && self.sink.as_ref().and_then(|sink| sink.retained(self.blocking)).is_none()) {
                    Err("Audio output could not be retained; sleep cancelled")
                } else if !self.save() {
                    Err("Listening position was not saved; sleep cancelled")
                } else {
                    self.park_output();
                    self.gate(handoff::Event::Prepared { ok: true });
                    Ok(())
                }
            };
            self.publish(true);
            self.sleep_probe(if result.is_ok() { "prepared" } else { "prepare-failed" });
            if ack.send(result).is_err() { self.gate(handoff::Event::AckLost); }
            return true;
        }
        if let Cmd::PrepareHandoff(ack) = cmd {
            let result = if self.gate(handoff::Event::Prepare) == handoff::Effect::Refuse {
                Err("A power or storage transition is already pending")
            } else if self.pause_output().is_err() {
                Err("Audio output did not pause; USB drive did not open")
            } else {
                self.release_output();
                let position = self.position_ms();
                self.close_track();
                self.resume_at = position;
                self.base_ms = position;
                self.frames = 0;
                self.scrub_until = None;
                self.scrub_ended = false;
                if self.save() {
                    self.error = None;
                    self.gate(handoff::Event::Prepared { ok: true });
                    Ok(())
                } else {
                    Err("Listening position was not saved; USB drive did not open")
                }
            };
            self.publish(true);
            if ack.send(result).is_err() {
                self.gate(handoff::Event::AckLost);
            }
            return true;
        }
        if self.gate(handoff::Event::Command { quit: matches!(cmd, Cmd::Quit) }) == handoff::Effect::Ignore {
            return true;
        }
        if let Cmd::ObserveMusicCursor { deadline, reply } = cmd {
            let result = if Instant::now() >= deadline { Err("music cursor observation expired") }
                else { self.observe_music_cursor() };
            let result = if Instant::now() >= deadline { Err("music cursor observation expired") } else { result };
            let _ = reply.send(result);
            return true;
        }
        // Selecting the loaded station is navigation, not a fresh live tune.
        // Keep its decoder, buffer, metadata and paused/playing state intact.
        if let Cmd::Play { songs, index: 0, shuffle: false } = &cmd {
            if songs.len() == 1 && self.state != PlayState::Stopped
                && same_station(self.queue.get(self.index), &songs[0]) {
                return true;
            }
        }
        // Every production path that can make an empty queue nonempty passes
        // here. Prepare before publishing that queue or producing PCM; later
        // music/book transitions reuse the same durable owner.
        if matches!(&cmd, Cmd::Play { songs, .. } | Cmd::Restore { songs, .. } if !songs.is_empty())
            || matches!(&cmd, Cmd::Insert { songs, .. } if !songs.is_empty()) {
            if let Err(error) = self.prepare_checkpoints() {
                self.pause(); self.error = Some(error); self.publish(true); return true;
            }
        }
        if matches!(cmd,Cmd::Play {..}|Cmd::Restore {..}|Cmd::Jump(_)|Cmd::Remove(_))
            && !self.checkpoint_current() { self.pause(); self.publish(true); return true; }
        // Read the new book's checkpoint before replacing the current queue.
        // An unreadable checkpoint is not a new book at position zero.
        let incoming = match &cmd {
            Cmd::Play {songs,index,..}=>Some((songs,*index,0)),
            Cmd::Restore {songs,index,position_ms}=>Some((songs,*index,*position_ms)),
            _=>None,
        };
        let prepared_spoken = if let Some((songs,index,fallback))=incoming {
            let index=index.min(songs.len().saturating_sub(1));
            if songs.get(index).is_some_and(|song|song.spoken.is_some()) {
                match self.source.spoken_resume(songs,index,fallback) {
                    Ok(resume)=>Some(resume),
                    Err(error)=>{self.pause();self.error=Some(error);self.publish(true);return true;}
                }
            }else{None}
        }else{None};
        if matches!(cmd, Cmd::Play { .. } | Cmd::Restore { .. } | Cmd::Insert { .. } | Cmd::Remove(_) | Cmd::SetShuffle(_)) {
            self.queue_rev += 1;
        }
        match cmd {
            Cmd::Play { songs, index, shuffle } => {
                self.original = None;
                let step = qfsm::transition(self.cursor(), qfsm::Event::Load { len: songs.len(), index, autoplay: true });
                self.queue = songs;
                self.index = step.queue.index;
                if shuffle && !self.queue.iter().any(|s|s.spoken.is_some()) {
                    self.shuffle(true, false);
                }
                let (index,position)=prepared_spoken.unwrap_or((self.index,0));
                // The library's bookmark is validated like any other index.
                self.index=qfsm::clamp_index(self.queue.len(),index); self.start_song(position);
                self.conforms(step.queue.play);
            }
            Cmd::Restore { songs, index, position_ms } => {
                self.new_generation();
                self.close_track();
                self.original = None;
                let spoken = prepared_spoken.is_some();
                let (index,mut position_ms)=prepared_spoken.unwrap_or((index,position_ms));
                let step = qfsm::transition(self.cursor(), qfsm::Event::Load { len: songs.len(), index, autoplay: false });
                self.queue = songs;
                self.index = step.queue.index;
                // A saved position is not trusted beyond the song's length
                // (the server's whole seconds, rounded up); the decoder's
                // exact length applies when the song opens.
                if let Some(seconds) = self.queue.get(self.index).map(|song| u64::from(song.duration)).filter(|s| !spoken && *s > 0) {
                    position_ms = position_ms.min((seconds + 1) * 1000);
                }
                self.resume_at = position_ms;
                self.base_ms = position_ms;
                self.frames = 0;
                self.state = step.queue.play;
                self.load_speed();
            }
            Cmd::PlayPause => { let step = qfsm::transition(self.cursor(), qfsm::Event::PlayPause); self.run_queue(step); }
            Cmd::Resume { generation } => {
                let step = qfsm::transition(self.cursor(), qfsm::Event::Resume { current: generation == self.generation });
                self.run_queue(step);
            }
            Cmd::ResumeSpoken { generation, seek_revision } => {
                if self.state == PlayState::Paused && generation == self.generation && seek_revision == self.seek_revision {
                    match self.source.spoken_resume(&self.queue, self.index, self.position_ms()) {
                        Ok((index, position)) => {
                            if let Some(book) = self.queue.get(index).and_then(|s| s.spoken.as_ref()) {
                                let absolute = book.offset_ms.saturating_add(position);
                                if absolute != self.logical_position_ms() {
                                    // The paused cursor was already saved. Do not
                                    // save it again over a freshly reconciled phone
                                    // bookmark when crossing physical book files.
                                    if index == self.index { self.seek(position); }
                                    else { self.index = index; self.start_song(position); self.state = PlayState::Paused; }
                                }
                                if self.error.is_none() { self.resume(); }
                            }
                        }
                        Err(error) => self.error = Some(error),
                    }
                }
            }
            Cmd::SetSleepTimer { deadline } => {
                if self.sleep_timer_deadline != deadline { self.restore_sleep_gain(); }
                self.sleep_timer_deadline = deadline;
            }
            Cmd::StartSleepFade { deadline } => {
                if self.sleep_timer_deadline == Some(deadline) {
                    self.sleep_fade = Some(SleepFade::Falling { deadline });
                }
            }
            Cmd::FinishSleepTimer { deadline, generation, rewind_ms } => {
                if self.sleep_timer_deadline != Some(deadline) { return true; }
                self.sleep_timer_deadline = None;
                self.restore_sleep_gain();
                if self.generation == generation && matches!(self.state, PlayState::Playing | PlayState::Buffering) {
                    let seekable = !self.is_radio();
                    let spoken = self.queue.get(self.index).is_some_and(|s| s.spoken.is_some());
                    let position = if spoken { self.logical_position_ms() } else { self.position_ms() };
                    if self.pause_output().is_ok() && seekable {
                        let target = position.saturating_sub(rewind_ms);
                        if spoken { self.seek_spoken_absolute(target); } else { self.seek(target); }
                    }
                    self.save();
                }
            }
            Cmd::Pause => { let step = qfsm::transition(self.cursor(), qfsm::Event::Pause); self.run_queue(step); }
            Cmd::Next => {
                if self.skip_spoken(true) { self.publish(true); return true; }
                let step = qfsm::transition(self.cursor(), qfsm::Event::Next { repeat: self.repeat });
                self.run_queue(step);
            }
            Cmd::Prev => {
                if self.skip_spoken(false) { self.publish(true); return true; }
                let step = qfsm::transition(self.cursor(), qfsm::Event::Prev { restart: self.position_ms() > 3000 });
                self.run_queue(step);
            }
            Cmd::Scrub { generation, position, active } => {
                if generation == self.generation {
                    self.scrub_until = active.then(|| Instant::now() + Duration::from_secs(4));
                    let spoken=self.queue.get(self.index).is_some_and(|s|s.spoken.is_some());
                    if let Some(ms) = position {if spoken{self.seek_spoken_absolute(ms);}else{self.seek(ms);}}
                    if spoken && !active {self.save();}
                }
            }
            Cmd::SeekBy(d) => {
                if self.queue.get(self.index).is_some_and(|song| song.spoken.is_some()) {
                    let to = (i128::from(self.logical_position_ms()) + i128::from(d)).max(0).min(i128::from(u64::MAX)) as u64;
                    self.seek_spoken_absolute(to);
                } else {
                    // Saturating: an offset from outside is not trusted to fit.
                    let to = i64::try_from(self.position_ms()).unwrap_or(i64::MAX).saturating_add(d).max(0) as u64;
                    let dur = self.duration_ms();
                    self.seek(if dur > 0 { to.min(dur.saturating_sub(500)) } else { to });
                }
            }
            Cmd::Jump(i) => { let step = qfsm::transition(self.cursor(), qfsm::Event::Jump(i)); self.run_queue(step); }
            Cmd::Insert { songs, next } => {
                let step = qfsm::transition(self.cursor(), qfsm::Event::Insert { count: songs.len(), next });
                if let qfsm::Effect::Insert { at, arm } = step.effect {
                    if let Some(orig) = &mut self.original {
                        Arc::make_mut(orig).extend(songs.iter().cloned());
                    }
                    Arc::make_mut(&mut self.queue).splice(at..at, songs);
                    if arm {
                        self.index = step.queue.index;
                        self.state = step.queue.play;
                        self.resume_at = 0;
                        self.load_speed();
                    }
                    self.save();
                }
            }
            Cmd::Remove(i) => {
                let step = qfsm::transition(self.cursor(), qfsm::Event::Remove(i));
                if let qfsm::Effect::Remove(after) = step.effect {
                    Arc::make_mut(&mut self.queue).remove(i);
                    self.index = step.queue.index;
                    match after {
                        qfsm::AfterRemove::Keep => {}
                        qfsm::AfterRemove::Stop => { self.close_track(); self.state = step.queue.play; }
                        qfsm::AfterRemove::Start => self.start_song(0),
                        qfsm::AfterRemove::Reload => { self.close_track(); self.resume_at = 0; self.load_speed(); }
                    }
                    self.save();
                }
            }
            Cmd::SetRepeat(r) => self.repeat = r,
            Cmd::SetShuffle(on) => {if !self.queue.iter().any(|s|s.spoken.is_some()){self.shuffle(on,true);}},
            Cmd::SetVolume(v) => {
                let analog = matches!(self.route.route, super::output::Route::Headphones);
                let started = analog.then(Instant::now);
                let previous = self.volume;
                let old_soft = self.soft_volume;
                self.volume = v.min(100);
                let applied = self.apply_volume();
                if let Some(started) = started {
                    crate::ux::analog_volume(previous, self.volume, old_soft != self.soft_volume, applied.is_err(), started.elapsed());
                }
                if let Err(error) = applied {
                    self.release_output();
                    self.output_failed(error);
                }
            }
            Cmd::SetRemoteVolume(on) => {
                if self.remote_volume != on {
                    self.remote_volume = on;
                    if let Err(error) = self.apply_volume() {
                        self.release_output();
                        self.output_failed(error);
                    }
                }
            }
            Cmd::SetSpeed { generation, percent, request } => {
                if generation == self.generation && percent.is_none_or(super::tempo::valid_speed) {
                    self.speed_request = request;
                    if let Some(song) = self.queue.get(self.index).filter(|song| song.spoken.is_some()) {
                        match self.source.set_spoken_speed(song, percent) {
                            Ok(()) => { self.error = None; self.apply_speed(percent); },
                            Err(error) => self.error = Some(error),
                        }
                    }
                }
            }
            Cmd::RefreshSpeed => {
                if let Some(song) = self.queue.get(self.index).filter(|song| song.spoken.is_some()) {
                    match self.source.spoken_speed(song) {
                        Ok(percent) => self.apply_speed(percent),
                        Err(error) => { self.pause(); self.error = Some(error); }
                    }
                }
            }
            Cmd::SetEq(settings) => {
                if let Some(reserve) = &mut self.replay_peak_reserve { *reserve = 0; }
                self.eq.set(settings);
            },
            Cmd::SetHeadphonePower(level) => {
                self.headphone_power = level;
                if !self.apply_headphone_power() && self.state == PlayState::Paused {
                    // A paused PCM parks now, its queue kept, so the level
                    // lands before resume reopens the output.
                    self.park_output();
                    self.apply_headphone_power();
                }
            }
            Cmd::SetFilter(f) => self.hw.set_filter(self.blocking, f),
            Cmd::SetReplayGain(m) => {
                self.rg = m;
                self.refresh_track_gain();
                if let Err(error)=self.apply_volume() {self.output_failed(error);}
            }
            Cmd::SetGapless(on) => self.gapless = on,
            Cmd::ParkOutput => {
                self.pause();
                self.park_output();
            }
            Cmd::OutputChanged=>{}
            Cmd::Release => {
                if matches!(self.state, PlayState::Playing | PlayState::Buffering) {
                    self.pause();
                }
                self.release_output();
            }
            Cmd::Quit => {
                if !self.save(){log::error!("Listening position could not be saved during shutdown");}
                self.publish(true);
                self.release_output();
                return false;
            }
            Cmd::PrepareHandoff(_) | Cmd::PrepareSleep(_) | Cmd::CancelHandoff | Cmd::ObserveMusicCursor { .. } | Cmd::AppendShuffle { .. } => unreachable!(),
        }
        self.publish(true);
        true
    }

    fn prepare_checkpoints(&mut self) -> Result<(), String> {
        if self.checkpoints.is_prepared() { return Ok(()); }
        // Fail closed if a future entry bypasses pre-load preparation. Spawning
        // while old PCM is running (including a gapless tail) is not permitted.
        if self.sink.is_some() || self.parked.is_some() || self.pending_output.is_some() {
            return Err("Listening location writer was not prepared before output".into());
        }
        self.checkpoints.prepare_before_playback()
    }

    fn observe_listening(&mut self) {
        let (index, position) = self.audible_cursor();
        if let Some(book) = self.queue.get(index).and_then(|song| song.spoken.as_ref()) {
            self.listening.observe(&book.id, book.offset_ms.saturating_add(position), self.generation,
                self.seek_revision, self.speed_percent, epoch_ms());
        } else { self.listening.clear(); }
        self.checkpoints.set_session(self.listening.snapshot());
    }

    fn checkpoint_current(&mut self) -> bool {
        self.observe_listening();
        let (index, position) = self.audible_cursor();
        if let Some(song) = self.queue.get(index) {
            if let Err(error) = self.checkpoints.checkpoint(song, position, self.generation) {
                self.error = Some(error);
                return false;
            }
        }
        self.checkpointed = Instant::now();
        true
    }

    fn retire_io(&self) {
        if let Some(io) = self.current_io.lock().unwrap().take() {
            // Music uses the command interrupt itself. Do not manufacture a
            // new command while retiring its stream; spoken I/O is independent.
            if !Arc::ptr_eq(&io.cancel, &self.interrupt) {
                io.cancel.store(true, Ordering::Relaxed);
            }
        }
    }

    /// One summary line per opened track, at its end (crate::playtrace).
    fn end_trace(&mut self, reason: u32) {
        if let Some(summary) = self.tap.end_track(reason) { log::info!("{}", summary.line()); }
    }

    fn close_track(&mut self) {
        self.end_trace(2);
        self.resume_output=false;
        self.retire_io();
        self.parked = None;
        self.pending_output = None;
        self.eq.reset();
        self.tempo = None;
        self.dec = None;
    }

    fn load_speed(&mut self) -> bool {
        let Some(song) = self.queue.get(self.index).filter(|song| song.spoken.is_some()) else {
            self.speed_percent = 100;
            self.speed_override = None;
            return true;
        };
        match self.source.spoken_speed(song) {
            Ok(percent) => {
                self.speed_override = percent;
                self.speed_percent = self.resolved_speed(percent);
            }
            Err(error) => {
                // Read failures must never be mistaken for an absent override.
                self.state = PlayState::Paused;
                self.error = Some(error);
                return false;
            }
        }
        true
    }

    fn resolved_speed(&self, custom: Option<u16>) -> u16 {
        custom.filter(|&speed| super::tempo::valid_speed(speed))
            .unwrap_or_else(|| {
                let speed = self.source.spoken_skip().speed_percent;
                if super::tempo::valid_speed(speed) { speed } else { 100 }
            })
    }

    fn apply_speed(&mut self, custom: Option<u16>) {
        let speed = self.resolved_speed(custom);
        if self.speed_percent != speed {
            // Retire queued PCM and DSP lookahead at the old audible media
            // position, which can still belong to the preceding physical file.
            self.seek_spoken_absolute(self.logical_position_ms());
            self.speed_percent = speed;
        }
        self.speed_override = custom;
    }

    fn is_radio(&self) -> bool { self.queue.get(self.index).is_some_and(|s| s.radio_url.is_some()) }

    /// Numeric instrumentation only for an explicitly enabled generated guest.
    /// Never emits media identity, file paths, credentials or library contents.
    fn sleep_probe(&self, stage: &str) {
        if std::env::var("Q2_EMULATOR_TEST").as_deref() != Ok("1")
            || !std::path::Path::new("/run/q2nd/sleep-probe-enable").is_file() { return; }
        let io = self.current_io.lock().unwrap();
        let value = serde_json::json!({"stage": stage, "generation": self.generation,
            "position_ms": self.position_ms(), "decoder": self.dec.is_some(),
            "io_ready": io.as_ref().is_some_and(|io| io.ready),
            "io_cancelled": io.as_ref().is_some_and(|io| io.cancel.load(Ordering::Relaxed)),
            "parked_samples": self.parked.as_ref().map_or(0, |held| held.samples.len()),
            "sink": self.sink.is_some(), "frozen": self.handoff_frozen});
        use std::io::Write;
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open("/run/q2nd/sleep-probe") {
            let _ = writeln!(file, "{value}");
        }
    }

    fn seek(&mut self, ms: u64) {
        let duration=self.duration_ms();
        let target=if self.queue.get(self.index).is_some_and(|s|s.spoken.is_some()) && duration>0 {ms.min(duration.saturating_sub(1))}else{seek_position(ms,duration)};
        self.seek_at(target);
    }

    fn seek_at(&mut self, ms: u64) {
        if self.is_radio() { return; }
        self.tempo = None;
        let paused = self.state == PlayState::Paused;
        let (previous_index, previous) = self.audible_cursor();
        self.parked=None;
        self.pending_output=None;
        // A seek from Stopped plays from the new position; an empty queue has
        // nothing to seek in and stays as it is.
        self.state = qfsm::transition(self.cursor(), qfsm::Event::Seek { radio: false }).queue.play;
        // Publish before a demuxer waits for bytes. The UI must not claim
        // audible playback while a long seek is blocked on the network.
        self.publish(true);
        self.scrub_ended = false;
        if self.dec.as_ref().is_some_and(|d| d.complete_for_seek) {
            self.close_track();
            self.seek_complete = true;
            if let Some(s) = &mut self.sink { s.flush(self.blocking); }
        }
        // A gapless boundary can still have the previous file queued while
        // no decoder is open. Every seek must retire that pre-seek PCM.
        if let Some(s) = &mut self.sink { s.flush(self.blocking); }
        let Some(d) = &mut self.dec else {
            self.resume_at = ms;
            self.base_ms = ms;
            self.frames = 0;
            self.seek_revision = self.seek_revision.wrapping_add(1);
            return;
        };
        if let Some(s) = &mut self.sink { s.flush(self.blocking); }
        let to = SeekTo::Time { time: time_from_ms(ms), track_id: Some(d.track_id) };
        match d.format.seek(self.blocking, SeekMode::Accurate, to, &self.interrupt) {
            Ok(seeked) => {
                d.decoder.reset();
                self.eq.reset();
                let actual = d.time_base.map(|tb| ms_from_time(tb.calc_time(seeked.actual_ts))).unwrap_or(ms);
                d.discard_frames = ms.saturating_sub(actual) * d.rate as u64 / 1000;
                self.base_ms = ms;
                self.frames = 0;
                if let Some(s) = &mut self.sink {
                    s.flush(self.blocking);
                }
                self.state = if paused { PlayState::Paused } else { PlayState::Playing };
            }
            Err(e) => {
                let interrupted = matches!(&e, SymError::IoError(e) if super::source::is_cut(e));
                log::warn!("seek: {e}");
                self.close_track();
                if !interrupted { self.index = previous_index; }
                self.resume_at = if interrupted { ms } else { previous };
                self.base_ms = self.resume_at;
                self.frames = 0;
                if let Some(s) = &mut self.sink { s.flush(self.blocking); }
                if !interrupted && !self.recover_descriptor(&e.to_string(), !paused) {
                    // A failed seek must not turn into "no decoder" and skip
                    // the song. Pause safely; Play can restart this same track.
                    self.error = Some(format!("Unable to seek: {e}"));
                    self.pause();
                }
            }
        }
        self.seek_revision = self.seek_revision.wrapping_add(1);
    }

    fn open_current(&mut self) -> Result<(), String> {
        let _poll=crate::diagnostics::PollScope::enter(204);
        self.retire_io();
        let result = self.open_current_inner();
        if result.is_err() { self.retire_io(); }
        result
    }

    fn open_current_inner(&mut self) -> Result<(), String> {
        let song = self.queue.get(self.index).cloned().ok_or("empty queue")?;
        // Also covers direct benchmark/test engine entry, before probing can
        // publish or write anything. Ordinary queue admission already prepared.
        self.prepare_checkpoints()?;
        if !self.load_speed() { return Err(self.error.clone().unwrap_or_else(|| "Unable to read saved playback speed".into())); }
        self.state = PlayState::Buffering;
        self.publish(true);
        let io_cancel=if song.spoken.is_some() {Arc::new(crate::audio::interrupt::Interrupt::new(false))}else{self.interrupt.clone()};
        {
            let _gate=self.command_gate.lock().unwrap();
            if self.interrupt.load(Ordering::Relaxed) {return Err(INTERRUPTED.into());}
            *self.current_io.lock().unwrap()=Some(IoSession{cancel:io_cancel.clone(),ready:false});
        }
        let completing_seek = self.seek_complete;
        let open_started = Instant::now();
        let traced = self.tap.trace.enabled();
        let mut opened = if completing_seek { self.source.open_complete(self.blocking, &song, &io_cancel)? } else { self.source.open(self.blocking, &song, &io_cancel)? };
        let opened_after = open_started.elapsed();
        let origin = opened.origin;
        if traced { opened.source = Box::new(crate::playtrace::TimedSource::new(opened.source, self.tap.reads.clone())); }
        let probed = match super::decoder::probe(self.blocking, opened.source,opened.ext.as_deref().or(song.suffix.as_deref()),(origin==Origin::Streaming || origin==Origin::Radio) && song.spoken.is_none() && !completing_seek) {
            Err(e) if origin==Origin::Streaming && super::decoder::needs_complete_file(&e) => {
                // Tail-moov MP4 cannot decode until this metadata arrives.
                // Keep a compatible fallback, rather than skipping the song.
                let mut complete = self.source.open_complete(self.blocking, &song,&io_cancel)?;
                if traced { complete.source = Box::new(crate::playtrace::TimedSource::new(complete.source, self.tap.reads.clone())); }
                super::decoder::probe(self.blocking, complete.source,complete.ext.as_deref().or(song.suffix.as_deref()),false)
            }
            result => result,
        }.map_err(|e|sym_err("open",e))?;
        // The player thread opens the next song between ALSA writes with
        // about 400 ms buffered: a slow open shows as an underrun
        // (docs/bugs/stream-cache-underrun.md).
        let total = open_started.elapsed();
        crate::ux::record(crate::ux::Mark::TrackOpen, crate::ux::track_open_value(origin_code(origin), song.spoken.is_some(),
            opened_after.as_millis() as u64, (total - opened_after).as_millis() as u64));
        if total > Duration::from_millis(50) {
            log::info!("track open {} ms (source {} ms, probe {} ms, {:?}) t={}", total.as_millis(), opened_after.as_millis(),
                (total - opened_after).as_millis(), origin, crate::metrics::uptime());
        }
        if let Some(summary) = self.tap.opened(origin_code(origin), opened_after.as_millis() as u64, (total - opened_after).as_millis() as u64) {
            log::info!("{}", summary.line());
        }
        let format = probed.format;
        let track = format
            .tracks()
            .iter()
            .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
            .ok_or("no audio track")?;
        let params = track.codec_params.clone();
        let decoder = symphonia::default::get_codecs()
            .make(&params, &DecoderOptions::default())
            .map_err(|e| sym_err("decoder", e))?;
        let rate = params.sample_rate.ok_or("unknown sample rate")?;
        let channels = params.channels.map(|c| c.count() as u32).unwrap_or(2);
        let duration_ms = match (params.n_frames, params.time_base) {
            (Some(n), Some(tb)) => ms_from_time(tb.calc_time(n)),
            _ => song.duration as u64 * 1000,
        };
        let label = format_label(&song, &params, rate);
        log::info!("playing \"{}\" ({label}, {:?})", song.title, origin);
        let track_id = track.id;
        {
            let _gate=self.command_gate.lock().unwrap();
            if io_cancel.load(Ordering::Relaxed) {return Err(INTERRUPTED.into());}
            if let Some(io)=self.current_io.lock().unwrap().as_mut() {io.ready=true;}
        }
        let format=super::packets::Packets::new(format,song.spoken.is_some(),io_cancel).map_err(|_|"Unable to start spoken decoder worker")?;
        self.source_format=super::pcm_format::PcmFormat::for_source(params.bits_per_sample);
        self.radio_listen = Default::default();
        self.radio_external_metadata = song.radio_url.as_deref().is_some_and(|url|
            subsonic::radio::france::matches_stream(url) || subsonic::radio::kcrw::matches_stream(url));
        self.direct_music_started_ms = epoch_ms();
        self.direct_listen_generation = if song.spoken.is_none() { self.source.direct_listen_generation() } else { None };
        self.dec = Some(Decoding {
            radio_metadata: opened.radio_metadata,
            complete_for_seek: song.spoken.is_none() && !completing_seek && origin == Origin::Streaming && params.codec == symphonia::core::codecs::CODEC_TYPE_VORBIS,
            discard_frames: 0,
            format,
            decoder,
            track_id,
            time_base: params.time_base,
            rate,
            channels,
            duration_ms,
            origin,
            label,
        });
        self.seek_complete = false;
        // The first decoded SignalSpec is authoritative: MP4 sample entries
        // can disagree with AAC AudioSpecificConfig (notably mono books).
        self.refresh_track_gain();
        self.apply_volume()?;
        if self.resume_at > 0 {
            let at = self.resume_at;
            self.resume_at = 0;
            // A resume position is where the listener stopped, including the
            // final second. Interactive seek's end margin must not rewind it.
            let at = if duration_ms > 0 { at.min(duration_ms.saturating_sub(1)) } else { at };
            self.seek_at(at);
        } else {
            self.base_ms = 0;
            self.frames = 0;
        }
        if self.dec.is_none() { return Ok(()); }
        self.state = PlayState::Playing;
        self.sleep_probe("opened");
        self.failures = 0;
        if self.base_ms < 1000 {
            self.started_ms = epoch_ms();
            self.scrobbled = false;
            self.source.now_playing(&song);
        }
        // Enough for a fetch-ahead burst (Settings::batch, or fetch_minutes
        // of short songs).
        let upcoming: Vec<Song> = self.queue.iter().skip(self.index + 1).take(32).cloned().collect();
        self.source.upcoming(&upcoming);
        self.save();
        Ok(())
    }

    fn check_scrobble(&mut self) {
        if self.is_radio() {
            self.check_radio_listen();
            return;
        }
        self.check_direct_music_listen();
        if self.scrobbled {
            return;
        }
        let dur = self.duration_ms();
        if dur <= 30_000 { return; }
        // The position is the frames written less the PCM's queue, so it never
        // exceeds the frames written. Until those reach the threshold, skip
        // the position's PCM delay query (one ioctl per packet).
        if self.written_ms_bound().is_some_and(|bound| bound < (dur / 2).min(240_000)) { return; }
        let pos = self.position_ms();
        if dur > 30_000 && pos >= (dur / 2).min(240_000) {
            self.scrobbled = true;
            if let Some(song) = self.queue.get(self.index) {
                self.source.played(song, self.started_ms);

            }
        }
    }

    fn check_direct_music_listen(&mut self) {
        let (Some(generation), Some(song), Some(sink)) = (self.direct_listen_generation,
            self.queue.get(self.index), self.sink.as_ref()) else { return };
        let dur = self.duration_ms();
        if dur <= 30_000 { return; }
        let due = (dur / 2).min(240_000) * u64::from(sink.rate) / 1000;
        if self.frames < due { return; }
        let Ok(delay) = sink.observed_delay(self.blocking) else { return };
        if self.frames.saturating_sub(delay) >= due {
            self.direct_listen_generation = None;
            self.source.direct_song_played(generation, song, self.direct_music_started_ms);
        }
    }

    fn check_radio_listen(&mut self) {
        let (Some(generation), Some(station), Some(sink), Some(dec)) =
            (self.direct_listen_generation, self.queue.get(self.index), self.sink.as_ref(), self.dec.as_ref()) else { return };
        let Some(ref metadata) = dec.radio_metadata else { return };
        let Ok(metadata) = metadata.try_lock() else { return };
        self.radio_listen.update(metadata.as_deref(), &station.title, self.frames, sink.rate, self.radio_external_metadata, epoch_ms);
        drop(metadata);
        if !self.radio_listen.due(self.radio_external_metadata) { return; }
        let Ok(delay) = sink.observed_delay(self.blocking) else { return };
        self.radio_listen.qualify(self.frames.saturating_sub(delay), self.radio_external_metadata, |artist, title, started_ms, kind| {
            self.source.direct_radio_played(generation, station, artist, title, started_ms, kind);
        });
    }

    /// Diagnostic cursor from one owner turn and one strict delay read. Ordinary
    /// UI snapshots keep their existing publication cadence. A cross-track queued
    /// tail, unknown delay or inactive output cannot become measured progress.
    fn observe_music_cursor(&self) -> Result<crate::numeric_probe::MusicCursor, &'static str> {
        let begin_us = crate::playtrace::boottime_us();
        let song = self.queue.get(self.index).ok_or("music cursor queue missing")?;
        if self.state != PlayState::Playing || song.spoken.is_some() || self.is_radio()
            || self.error.is_some() || self.pause_admission_fault.is_some() || self.parked.is_some()
            || self.pending_output.is_some() || self.scrub_until.is_some() || self.seek_complete {
            return Err("music cursor requires uninterrupted active file playback");
        }
        let sink = self.sink.as_ref().ok_or("music cursor output missing")?;
        let queued = sink.observed_delay(self.blocking).map_err(|_| "music cursor delay unavailable")?;
        if sink.rate == 0 || queued > self.frames { return Err("music cursor crosses an unobserved output boundary"); }
        let heard = self.frames - queued;
        let position_ms = heard.checked_mul(1000).and_then(|v| self.base_ms.checked_add(v / u64::from(sink.rate)))
            .ok_or("music cursor position overflow")?;
        let end_us = crate::playtrace::boottime_us();
        if begin_us == 0 || end_us < begin_us { return Err("music cursor clock unavailable"); }
        Ok(crate::numeric_probe::MusicCursor {
            schema: 1, pid: std::process::id(), begin_us, end_us,
            generation: self.generation, seek_revision: self.seek_revision, queue_revision: self.queue_rev,
            queue_len: self.queue.len(), queue_index: self.index,
            selection_tag: song.id.bytes().fold(2166136261u32, |value, byte| (value ^ u32::from(byte)).wrapping_mul(16777619)),
            position_ms, base_ms: self.base_ms, written_frames: self.frames, queued_frames: queued,
            sample_rate: sink.rate, output_revision: u64::from(self.route.generation),
        })
    }

    fn logical_position_ms(&self) -> u64 {
        let Some(book) = self.queue.get(self.index).and_then(|song| song.spoken.as_ref()) else {
            return self.position_ms();
        };
        let base = i128::from(book.offset_ms) + i128::from(self.base_ms);
        let elapsed = self.frame_timing().map(|(rate, delay)| {
            (i128::from(self.frames) - i128::from(delay)) * 1000 * i128::from(self.speed_percent)
                / (i128::from(rate) * 100)
        }).unwrap_or(0);
        (base + elapsed).clamp(0, i128::from(book.duration_ms)) as u64
    }

    fn audible_cursor(&self) -> (usize, u64) {
        if let Some(book) = self.queue.get(self.index).and_then(|song| song.spoken.as_ref()) {
            let absolute = self.logical_position_ms();
            if absolute < book.offset_ms {
                return crate::library::spoken::selection(&self.queue, self.index, absolute);
            }
            return (self.index, absolute.saturating_sub(book.offset_ms));
        }
        (self.index, self.position_ms())
    }

    /// An upper bound of `position_ms` for music, from the frames written
    /// alone. None where the position is not `base + heard frames` at the
    /// open PCM's or decoder's rate (spoken tracks, a parked queue).
    fn written_ms_bound(&self) -> Option<u64> {
        if self.parked.is_some() || self.queue.get(self.index).is_some_and(|song| song.spoken.is_some()) { return None; }
        let rate = self.sink.as_ref().map(|sink| sink.rate).or_else(|| self.dec.as_ref().map(|dec| dec.rate))?;
        Some(self.base_ms.saturating_add(self.frames.saturating_mul(1000) / u64::from(rate)))
    }

    fn position_ms(&self) -> u64 {
        if let Some(book) = self.queue.get(self.index).and_then(|song| song.spoken.as_ref()) {
            return self.logical_position_ms().saturating_sub(book.offset_ms);
        }
        let Some((rate, delay)) = self.frame_timing() else { return self.base_ms; };
        let heard = self.frames.saturating_sub(delay);
        self.base_ms.saturating_add(heard.saturating_mul(1000) / u64::from(rate))
    }

    fn duration_ms(&self) -> u64 {
        match &self.dec {
            Some(d) if d.duration_ms > 0 => d.duration_ms,
            _ => self.queue.get(self.index).map(|s| s.duration as u64 * 1000).unwrap_or(0),
        }
    }

    fn refresh_track_gain(&mut self) {
        let (gain, peak) = self.selected_replay_gain();
        self.track_gain = gain.map_or(1.0, |db| replay_gain_factor(db, peak));
        // Valid positive normalization is already capped at 1 / peak. It
        // needs the slider's usual unity signal reserve, not full scale
        // multiplied by the gain again. The decoded packet still proves fit.
        self.replay_peak_reserve = (self.track_gain > 1.0 && usable_replay_peak(peak).is_some()).then_some(0);
    }

    fn selected_replay_gain(&self) -> (Option<f32>, Option<f32>) {
        let Some(rg) = self.queue.get(self.index).and_then(|s| s.replay_gain.as_ref()) else { return (None, None); };
        match self.rg {
            ReplayGainMode::Off => (None, None),
            ReplayGainMode::Track => (rg.track_gain.or(rg.album_gain), rg.track_peak.or(rg.album_peak)),
            ReplayGainMode::Album => (rg.album_gain.or(rg.track_gain), rg.album_peak.or(rg.track_peak)),
        }
    }

    fn save(&mut self)->bool {
        let saved=self.checkpoint_current();
        let (index, position) = self.audible_cursor();
        self.source.save_queue(&self.queue, index, position);
        saved
    }

    fn publish(&mut self, mut force: bool) {
        if let Some(error) = self.checkpoints.error(self.generation) {
            // Stop feeding before retrying a failed durable save; no false
            // "saved" acknowledgement or stale-generation error publication.
            self.pause();
            self.error = Some(error);
            force = true;
        }
        // Preserve snapshots and checkpoint cadence while dark, but do not wake
        // the UI merely for an unchanged-semantic position update.
        let visible = self.visible.load(Ordering::Relaxed);
        let interval = if visible { Duration::from_millis(200) } else { Duration::from_secs(30) };
        if !force && self.published_state == self.state && self.published.elapsed() < interval {
            return;
        }
        self.published_state = self.state;
        self.observe_listening();
        if self.state == PlayState::Playing && self.checkpointed.elapsed()>=Duration::from_secs(30) && self.queue.get(self.index).is_some_and(|s|s.spoken.is_some()) {
            let (index, position) = self.audible_cursor();
            if let Some(song) = self.queue.get(index) {
                self.checkpoints.periodic(song, position, self.generation);
                self.checkpointed = Instant::now();
            }
        }
        self.published = Instant::now();
        let mut position_ms = self.position_ms();
        let mut duration_ms = self.duration_ms();
        if let Some(spoken)=self.queue.get(self.index).and_then(|s|s.spoken.as_ref()) {
            position_ms = self.logical_position_ms(); duration_ms = spoken.duration_ms;
        }
        let audible_index = self.audible_cursor().0;
        // Prepare allocation and external metadata locking before the short
        // snapshot swap. UI status readers must not wait for either operation.
        let changed_queue = (self.published_rev != self.queue_rev).then(|| self.queue.clone());
        let format = self.dec.as_ref().map(|d| d.label.clone());
        let error = self.error.as_deref().map(|raw| self.ux.notice(raw));
        let radio_title = self.dec.as_ref().and_then(|d| d.radio_metadata.as_ref())
            .and_then(publication_radio_title);
        let spoken = self.queue.get(self.index).is_some_and(|song| song.spoken.is_some());
        let (output_pause_kind, output_pause_frames) = self.sink.as_ref().map(Sink::pause_ownership)
            .unwrap_or_else(||self.parked.as_ref().map_or((0,0),|held|(4,held.frames())));
        let (transition, retired) = {
            let mut previous = self.status.lock().unwrap();
            let queue = changed_queue.unwrap_or_else(|| previous.queue.clone());
            let next = Status {
                state: self.state, queue, index: audible_index,
                generation: self.generation, seek_revision: self.seek_revision,
                position_ms, duration_ms, repeat: self.repeat,
                shuffle: self.original.is_some(), volume: self.volume,
                speed_percent: if spoken { self.speed_percent } else { 100 },
                speed_override: if spoken { self.speed_override } else { None },
                speed_request: self.speed_request,
                origin: self.dec.as_ref().map(|d| d.origin),
                format,
                sample_rate: self.dec.as_ref().map(|d| d.rate), error,
                error_code: self.error.as_deref().map(|raw| crate::ux::Code::classify(raw).unwrap_or(crate::ux::Code::Unknown) as u16),
                output_revision: u64::from(self.route.generation),
                output_usb_card: match &self.route.route {super::output::Route::Usb(device)=>Some(device.card),_=>None},
                output_usb_device: match &self.route.route {super::output::Route::Usb(device)=>Some(device.device),_=>None},
                output_owned: self.sink.is_some(), pause_admission_failed: self.pause_admission_fault.is_some(),
                output_pause_kind, output_pause_frames,
                radio_title,
            };
            let changed = !previous.same_non_position(&next);
            (changed, std::mem::replace(&mut *previous, next))
        };
        self.ux.observe(retired.state, self.state, self.error.as_deref(), self.index + 1 >= self.queue.len());
        self.published_rev = self.queue_rev;
        drop(retired); // Queue/string destruction also stays outside the UI lock.
        if force || visible || transition { (self.notify)(); }
        if transition { self.tap.published(state_code(self.state)); }
    }
}

/// Stop-cause attribution and notice presentation for UX telemetry.
#[derive(Default)]
struct UxPlayer {
    /// The raw error last presented, and what the listener sees for it.
    notice: Option<(String, String)>,
    /// The last command that can stop playback, and when it was handled.
    last: Option<(crate::ux::StopCause, Instant)>,
}
impl UxPlayer {
    /// Presents each distinct error once (UX-006); later publications reuse it.
    fn notice(&mut self, raw: &str) -> String {
        if let Some((seen, shown)) = &self.notice { if seen == raw { return shown.clone(); } }
        let shown = playback_notice(raw);
        self.notice = Some((raw.to_owned(), shown.clone()));
        shown
    }
    fn command(&mut self, cmd: &Cmd, state: PlayState) {
        use crate::ux::StopCause;
        let cause = match cmd {
            Cmd::Pause | Cmd::PlayPause => StopCause::User,
            Cmd::FinishSleepTimer { .. } => StopCause::SleepTimer,
            Cmd::Release | Cmd::ParkOutput | Cmd::PrepareHandoff(_) | Cmd::PrepareSleep(_) | Cmd::Quit => StopCause::System,
            _ => return self.started(cmd, state),
        };
        self.last = Some((cause, Instant::now()));
        self.started(cmd, state);
    }
    fn started(&mut self, cmd: &Cmd, state: PlayState) {
        use crate::ux::{begin, Journey};
        let paused = matches!(state, PlayState::Paused | PlayState::Stopped);
        match cmd {
            // A Play that reopened the item already timed it from the press.
            Cmd::PlayPause | Cmd::Resume { .. } | Cmd::ResumeSpoken { .. } if paused && crate::ux::pending_audio().is_none() => begin(Journey::Resume),
            Cmd::Next | Cmd::Prev | Cmd::Jump(_) if !paused => begin(Journey::Skip),
            _ => {}
        }
    }
    fn observe(&mut self, was: PlayState, now: PlayState, error: Option<&str>, last_song: bool) {
        use crate::ux::StopCause;
        let active = |s: PlayState| matches!(s, PlayState::Playing | PlayState::Buffering);
        if error.is_none() { self.notice = None; }
        if !active(was) || active(now) { return; }
        let recent = self.last.take().filter(|(_, at)| at.elapsed() < Duration::from_secs(3)).map(|(cause, _)| cause);
        let expected = crate::ux::take_expected_stop();
        let (cause, detail) = match (error, expected, recent) {
            (Some(raw), _, _) => (StopCause::Error, crate::ux::Code::classify(raw).unwrap_or(crate::ux::Code::Unknown) as u32),
            (None, Some(cause), _) => (cause, 0),
            (None, None, Some(cause)) => (cause, 0),
            (None, None, None) if now == PlayState::Stopped && last_song => (StopCause::EndOfQueue, 0),
            (None, None, None) => (StopCause::Unexplained, state_code(now)),
        };
        crate::ux::stop(cause, detail);
    }
}

/// Backend wording belongs in numeric diagnostics, not listener-facing notices.
fn playback_notice(error:&str)->String {
    crate::ux::present_in(error, crate::ux::Journey::None)
}

#[cfg(test)]
mod playback_notice_tests {
    #[test]
    fn descriptor_failures_are_actionable_without_hiding_other_errors() {
        for error in ["File descriptor in bad state", "Invalid file descriptor (os error 9)", "Bad file descriptor"] {
            let shown=super::playback_notice(error);
            assert_eq!(shown, crate::ux::Code::Descriptor.message());assert!(!shown.contains("descriptor"));
        }
        assert_eq!(super::playback_notice("Insert a card"),"Insert a card");
    }
}

/// The highest ReplayGain applied. Quiet classical recordings need about
/// +7 to +12 dB against the -18 LUFS reference; Rockbox clamps tags at +17 dB
/// and MPD its scale at +23.5 dB (docs/replaygain.md).
pub const REPLAY_GAIN_MAX_DB: f32 = 15.0;
/// The lowest ReplayGain applied (Rockbox's tag clamp), so a damaged tag
/// cannot silence a song.
pub const REPLAY_GAIN_MIN_DB: f32 = -48.0;

/// The linear factor for a ReplayGain of `db` decibels. The gain is clamped
/// to [`REPLAY_GAIN_MIN_DB`, `REPLAY_GAIN_MAX_DB`] and limited so `peak` does
/// not clip. Without a usable peak (absent, zero, negative or not finite) the
/// factor is at most 1.0, as mpv and VLC treat a missing peak as full scale:
/// a boost needs the server to say there is room for it. The factor is never
/// below the floor, even for an implausible peak, and a gain that is not a
/// finite number leaves the song unchanged.
pub fn replay_gain_factor(db: f32, peak: Option<f32>) -> f32 {
    if !db.is_finite() { return 1.0; }
    let peak = usable_replay_peak(peak).unwrap_or(1.0);
    let floor = 10f32.powf(REPLAY_GAIN_MIN_DB / 20.0);
    let lin = 10f32.powf(db.clamp(REPLAY_GAIN_MIN_DB, REPLAY_GAIN_MAX_DB) / 20.0);
    lin.min(1.0 / peak).max(floor)
}

fn usable_replay_peak(peak: Option<f32>) -> Option<f32> {
    peak.filter(|p| p.is_finite() && *p > 0.0)
}

/// Keep all seek paths inside the stream, including coarse server durations.
pub fn seek_position(ms: u64, duration: u64) -> u64 {
    if duration == 0 { ms } else { ms.min(duration.saturating_sub(1000)) }
}

/// The software volume curve: 0.5 dB a step below 100, as the DAC's table.
pub fn soft_gain(volume: u8) -> f32 {
    if volume == 0 {
        return 0.0;
    }
    10f32.powf(-0.5 * (100 - volume.min(100)) as f32 / 20.0)
}

const INTERRUPTED: &str = "interrupted";

fn sym_err(what: &str, e: SymError) -> String {
    match e {
        SymError::IoError(e) if super::source::is_cut(&e) => INTERRUPTED.into(),
        SymError::Unsupported(f) => format!("unsupported format ({f})"),
        e => format!("{what}: {e}"),
    }
}

fn format_label(song: &Song, params: &symphonia::core::codecs::CodecParameters, rate: u32) -> String {
    format_label_parts(song, params.codec, params.bits_per_sample, rate)
}
fn format_label_parts(song: &Song, codec: symphonia::core::codecs::CodecType, bits: Option<u32>, rate: u32) -> String {
    let codec = symphonia::default::get_codecs()
        .get_codec(codec)
        .map(|c| c.short_name.to_ascii_uppercase())
        .unwrap_or_else(|| song.suffix.clone().unwrap_or_default().to_ascii_uppercase());
    // Symphonia names PCM by sample format (pcm_s16le, ...).
    let codec = if codec.starts_with("PCM") { "PCM".to_string() } else { codec };
    let khz = rate as f32 / 1000.0;
    let khz = if (khz - khz.round()).abs() < 0.05 { format!("{}", khz.round() as u32) } else { format!("{khz:.1}") };
    match bits {
        Some(bits) => format!("{codec} {bits}/{khz}"),
        None => match song.bit_rate {
            Some(br) if br > 0 => format!("{codec} {br}k"),
            _ => format!("{codec} {khz} kHz"),
        },
    }
}

#[cfg(test)]
#[path = "playtrace_player_tests.rs"]
mod playtrace_tests;

#[cfg(test)]
#[path = "player_machine_tests.rs"]
mod machine_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::alsa::{FAKE, SoftHw};
    use std::io::Cursor;

    /// A 16-bit stereo WAV of `secs` seconds of silence.
    fn wav(secs: f32) -> Vec<u8> {
        let rate = 8000u32;
        let frames = (rate as f32 * secs) as u32;
        let data = frames * 4;
        let mut v = Vec::new();
        v.extend_from_slice(b"RIFF");
        v.extend_from_slice(&(36 + data).to_le_bytes());
        v.extend_from_slice(b"WAVEfmt ");
        v.extend_from_slice(&16u32.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&rate.to_le_bytes());
        v.extend_from_slice(&(rate * 4).to_le_bytes());
        v.extend_from_slice(&4u16.to_le_bytes());
        v.extend_from_slice(&16u16.to_le_bytes());
        v.extend_from_slice(b"data");
        v.extend_from_slice(&data.to_le_bytes());
        v.resize(v.len() + data as usize, 0);
        v
    }

    #[derive(Default)]
    struct Recorder {
        audio:Mutex<Option<Vec<u8>>>,
        opened: Mutex<Vec<String>>,
        played: Mutex<Vec<String>>,
        saved: Mutex<Option<(usize, usize)>>,
        speeds: Mutex<std::collections::HashMap<String, u16>>,
        speed_writes: std::sync::atomic::AtomicUsize,
        default_speed: std::sync::atomic::AtomicU16,
        speed_read_fail: AtomicBool,
        speed_write_fail: AtomicBool,
        checkpointed_positions: Mutex<Vec<(String, u64)>>,
        queue_positions: Mutex<Vec<(usize, u64)>>,
        /// Open tracks as streamed from the server.
        streaming: AtomicBool,
    }

    impl TrackSource for Recorder {
        fn open(&self, _blocking: &crate::rt::Blocking, song: &Song, _: &Arc<crate::audio::interrupt::Interrupt>) -> Result<OpenedTrack, String> {
            self.opened.lock().unwrap().push(song.id.clone());
            if song.id == "missing" {
                return Err("Not available offline".into());
            }
            let audio=self.audio.lock().unwrap().clone().unwrap_or_else(||wav(1.2));
            let origin = if self.streaming.load(Ordering::Relaxed) { Origin::Streaming } else { Origin::Cached };
            Ok(OpenedTrack { source: Box::new(Cursor::new(audio)), ext: Some("wav".into()), origin, radio_metadata: None })
        }
        fn upcoming(&self, _: &[Song]) {}
        fn now_playing(&self, _: &Song) {}
        fn spoken_skip(&self) -> crate::config::SpokenSettings {
            let percent = self.default_speed.load(Ordering::Relaxed);
            crate::config::SpokenSettings { speed_percent: if super::super::tempo::valid_speed(percent) { percent } else { 100 }, ..Default::default() }
        }
        fn spoken_speed(&self, song: &Song) -> Result<Option<u16>, String> {
            if self.speed_read_fail.load(Ordering::Relaxed) { return Err("Generated speed read failure".into()); }
            Ok(crate::library::spoken::speed_target(song).and_then(|target| self.speeds.lock().unwrap().get(&target).copied()))
        }
        fn set_spoken_speed(&self, song: &Song, percent: Option<u16>) -> Result<(), String> {
            if self.speed_write_fail.load(Ordering::Relaxed) { return Err("Generated speed write failure".into()); }
            if let Some(target) = crate::library::spoken::speed_target(song) {
                self.speed_writes.fetch_add(1, Ordering::Relaxed);
                let mut speeds = self.speeds.lock().unwrap();
                if let Some(percent) = percent { speeds.insert(target, percent); } else { speeds.remove(&target); }
            }
            Ok(())
        }
        fn played(&self, song: &Song, _: u64) {
            self.played.lock().unwrap().push(song.id.clone());
        }
        fn checkpoint(&self, song: &Song, position: u64) -> Result<(), String> {
            self.checkpointed_positions.lock().unwrap().push((song.id.clone(), position));
            Ok(())
        }
        fn save_queue(&self, q: &[Song], i: usize, position: u64) {
            *self.saved.lock().unwrap() = Some((q.len(), i));
            self.queue_positions.lock().unwrap().push((i, position));
        }
    }

    fn song(id: &str) -> Song {
        Song { id: id.into(), title: id.into(), duration: 1, ..Default::default() }
    }

    #[test]
    fn a_played_list_is_shared_until_the_queue_changes() {
        // A list's rows, the queue and the published status hold one copy of
        // the songs; the queue copies only when it changes.
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut engine = deterministic_spoken_engine(&blocking, Arc::new(Recorder::default()));
        let list: Arc<Vec<Song>> = Arc::new((0..5).map(|n| song(&format!("generated-{n}"))).collect());
        engine.handle(Cmd::Play { songs: list.clone(), index: 2, shuffle: false });
        assert!(Arc::ptr_eq(&engine.queue, &list));
        engine.publish(true);
        assert!(Arc::ptr_eq(&engine.status.lock().unwrap().queue, &list));
        engine.handle(Cmd::Insert { songs: vec![song("generated-inserted")], next: true });
        assert!(!Arc::ptr_eq(&engine.queue, &list));
        assert_eq!((list.len(), engine.queue.len()), (5, 6));
        assert_eq!(engine.queue[3].id, "generated-inserted");
        // Shuffle: the shuffled order is the copy; the original stays shared.
        engine.handle(Cmd::Play { songs: list.clone(), index: 0, shuffle: false });
        engine.handle(Cmd::SetShuffle(true));
        assert!(Arc::ptr_eq(engine.original.as_ref().unwrap(), &list));
        assert!(!Arc::ptr_eq(&engine.queue, &list));
        let mut ids: Vec<_> = engine.queue.iter().map(|s| s.id.clone()).collect();
        ids.sort();
        assert_eq!(ids, list.iter().map(|s| s.id.clone()).collect::<Vec<_>>());
        engine.handle(Cmd::SetShuffle(false));
        assert!(Arc::ptr_eq(&engine.queue, &list));
    }

    #[test]
    fn hardware_initialization_stays_on_player_before_commands_and_shutdown_joins_it() {
        struct InitializingHw {
            entered: mpsc::Sender<()>,
            release: Option<Receiver<()>>,
            events: Arc<Mutex<Vec<(&'static str, std::thread::ThreadId)>>>,
        }
        impl InitializingHw {
            fn note(&self, event: &'static str) {
                self.events.lock().unwrap().push((event, std::thread::current().id()));
            }
        }
        impl AudioHw for InitializingHw {
            fn device(&mut self) -> String { FAKE.into() }
            fn power_on(&mut self, _: &crate::rt::Blocking) { self.note("power_on"); }
            fn power_off(&mut self, _: &crate::rt::Blocking) { self.note("power_off"); }
            fn mute(&mut self, _: &crate::rt::Blocking, _: bool) { self.note("mute"); }
            fn set_attenuation(&mut self, _: &crate::rt::Blocking, _: u8) -> Result<bool, String> { self.note("volume"); Ok(false) }
            fn set_headphone_power(&mut self, _: &crate::rt::Blocking, level: crate::config::HeadphonePower)
                -> Result<crate::config::HeadphonePower, String> {
                self.note("gain_enter");
                self.entered.send(()).unwrap();
                // Timeout bounds a broken test; only the explicit release admits progress.
                self.release.take().unwrap().recv_timeout(Duration::from_secs(3)).unwrap();
                self.note("gain_exit");
                Ok(level)
            }
            fn set_filter(&mut self, _: &crate::rt::Blocking, filter: u8) {
                self.note(if filter == 0 { "initial_filter" } else { "command_filter" });
            }
        }
        impl Drop for InitializingHw {
            fn drop(&mut self) { self.note("drop"); }
        }
        let events = Arc::new(Mutex::new(Vec::new()));
        let (entered, observed) = mpsc::channel();
        let (release, held) = mpsc::channel();
        let settings = Settings { eq: Default::default(), volume: 50, headphone_power: crate::config::HeadphonePower::Standard,
            filter: 0, replay_gain: ReplayGainMode::Off, gapless: true };
        let player = spawn(Arc::new(Recorder::default()), Box::new(InitializingHw {
            entered, release: Some(held), events: events.clone(),
        }), settings, Arc::new(|| {}));
        observed.recv_timeout(Duration::from_secs(3)).unwrap();
        // The constructor has returned while initialization is held. A queued
        // command cannot overtake it, even when shutdown follows immediately.
        player.send(Cmd::SetFilter(7));
        assert_eq!(events.lock().unwrap().iter().map(|(event, _)| *event).collect::<Vec<_>>(), ["gain_enter"]);
        release.send(()).unwrap();
        player.shutdown();
        let events = events.lock().unwrap();
        let names: Vec<_> = events.iter().map(|(event, _)| *event).collect();
        assert_eq!(&names[..5], ["gain_enter", "gain_exit", "initial_filter", "volume", "command_filter"]);
        assert_eq!(names.last(), Some(&"drop"));
        let owner = events[0].1;
        assert_ne!(owner, std::thread::current().id());
        assert!(events.iter().all(|(_, thread)| *thread == owner));
    }

    fn start() -> (Arc<Recorder>, PlayerHandle) {
        let rec = Arc::new(Recorder::default());
        let settings = Settings { eq: Default::default(), volume: 100, headphone_power: crate::config::HeadphonePower::Standard, filter: 0, replay_gain: ReplayGainMode::Off, gapless: true };
        let p = spawn(rec.clone(), Box::new(SoftHw(FAKE.into())), settings, Arc::new(|| {}));
        (rec, p)
    }

    struct ScriptedOutput {
        fault:Option<usize>,
        replay_fault:bool,
        captured:Arc<Mutex<Vec<i32>>>,
        drained:Arc<AtomicBool>,
    }
    impl AudioHw for ScriptedOutput {
        fn device(&mut self)->String {FAKE.into()}
        fn open_sink<'a>(&mut self, blocking: &'a crate::rt::Blocking,device:&str,rate:u32,channels:u32,latency:u32,format:crate::audio::pcm_format::PcmFormat)->Result<Sink<'a>,String> {
            let mut sink=Sink::open_format(blocking,device,rate,channels,latency,format)?;
            let mut steps=self.fault.take().map(|frames|std::collections::VecDeque::from([
                Ok(frames),Err("Synthetic DAC unplug".into())])).unwrap_or_default();
            if self.replay_fault && !steps.is_empty() {
                steps.extend([Ok(2),Err("Synthetic DAC replay unplug".into())]);
            }
            sink.script_writes(super::super::alsa::WriteFixture {
                steps,accepted:self.captured.clone(),queued:Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                drained:self.drained.clone(),
                consumed_on_error:std::collections::VecDeque::from([0,1]),
                on_error:None,
                first_write:None, queue_limit:None,
            });
            Ok(sink)
        }
        fn power_on(&mut self, _blocking: &crate::rt::Blocking) {}
        fn power_off(&mut self, _blocking: &crate::rt::Blocking) {}
        fn mute(&mut self, _blocking: &crate::rt::Blocking,_:bool) {}
        fn set_attenuation(&mut self, _blocking: &crate::rt::Blocking,_:u8) -> Result<bool, String> {Ok(false)}
        fn set_filter(&mut self, _blocking: &crate::rt::Blocking,_:u8) {}
    }

    fn output_source(percent:Option<u16>)->Arc<Recorder> {
        let source=Arc::new(Recorder::default());
        let mut bytes=wav(1.2);
        for (frame,stereo) in bytes[44..].chunks_exact_mut(4).enumerate() {
            let signal=(12000.0*(frame as f64*std::f64::consts::TAU*137.0/8000.0).sin()) as i16;
            stereo[..2].copy_from_slice(&signal.to_le_bytes());
            stereo[2..].copy_from_slice(&(signal/2).to_le_bytes());
        }
        *source.audio.lock().unwrap()=Some(bytes);
        source.default_speed.store(percent.unwrap_or(100),Ordering::Relaxed);
        source
    }
    fn output_settings()->Settings {
        let correction=super::super::eq::Settings {graphic_enabled:true,bands:[3,-2,4,-1,2,-3,1,3,-2,2],
            headphone_enabled:true,headphone:Some(super::super::eq::Profile {
                name:"Generated output continuity".into(),source:"Synthetic only".into(),preamp:-8.0,
                filters:vec![super::super::eq::Filter {kind:super::super::eq::Kind::Peak,hz:137.0,gain:-3.0,q:0.8}],
            })};
        Settings {eq:correction,volume:35,headphone_power: crate::config::HeadphonePower::Standard,filter:0,replay_gain:ReplayGainMode::Off,gapless:true}
    }
    fn output_fault_trace(percent:Option<u16>,fault:Option<usize>,park:bool,replay_fault:bool)->Vec<i32> {
        let source=output_source(percent);
        let captured=Arc::new(Mutex::new(Vec::new()));
        let drained=Arc::new(AtomicBool::new(false));
        let player=spawn(source.clone(),Box::new(ScriptedOutput {fault,replay_fault,captured:captured.clone(),drained:drained.clone()}),
            output_settings(),Arc::new(||{}));
        let mut selected=if percent.is_some() {spoken_queue()[0].clone()}else{song("generated-output-fault")};
        if let Some(book)=&mut selected.spoken {book.duration_ms=1200;}
        let id=selected.id.clone();
        player.send(Cmd::Play {songs:std::sync::Arc::new(vec![selected]),index:0,shuffle:false});
        if let Some(frames)=fault {
            let status=wait_for(&player,2,|s|s.error.is_some());
            assert_eq!(status.state,PlayState::Paused,"an output error must not retire the media decoder");
            assert_eq!(status.index,0);
            assert_eq!(status.position_ms,0,"accepted unheard frames are not audible progress");
            assert!(status.sample_rate.is_some(),"the actual decoder remains owned");
            let io=player.current_io.lock().unwrap();
            assert!(io.as_ref().unwrap().ready && !io.as_ref().unwrap().cancel.load(Ordering::Relaxed));
            drop(io);
            assert_eq!(captured.lock().unwrap().len(),frames*2);
            if park {player.send(Cmd::ParkOutput);}
            player.send(Cmd::Resume {generation:status.generation});
            if replay_fault {
                let deadline=Instant::now()+Duration::from_secs(2);
                while captured.lock().unwrap().len()<(frames+2)*2 && Instant::now()<deadline {
                    std::thread::sleep(Duration::from_millis(2));
                }
                assert_eq!(captured.lock().unwrap().len(),(frames+2)*2);
                player.send(Cmd::Resume {generation:status.generation});
            }
        }
        let deadline=Instant::now()+Duration::from_secs(3);
        while !drained.load(Ordering::Acquire) && Instant::now()<deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        player.shutdown();
        let done=player.status();
        assert!(drained.load(Ordering::Acquire),"the exact PCM must reach its final output drain");
        assert_eq!(done.state,PlayState::Stopped);
        assert!(done.error.is_none(),"successful output replay clears its error");
        assert_eq!(source.opened.lock().unwrap().as_slice(),[id]);
        let samples=captured.lock().unwrap();
        let pcm=if replay_fault {
            let prefix=fault.unwrap()*2;
            let mut heard=samples[prefix..prefix+2].to_vec();
            heard.extend_from_slice(&samples[prefix+4..]);
            heard
        }else{samples[fault.unwrap_or(0)*2..].to_vec()};
        assert!(!pcm.is_empty() && pcm.iter().any(|&sample|sample!=0));
        pcm
    }

    fn cursor_engine<'a>(blocking: &'a crate::rt::Blocking) -> Engine<'a> {
        let mut engine = deterministic_spoken_engine(blocking, Arc::new(Recorder::default()));
        engine.queue = std::sync::Arc::new(vec![song("generated-cursor")]);
        engine.state = PlayState::Playing;
        engine.generation = 3; engine.seek_revision = 4; engine.queue_rev = 5;
        engine.route.generation = 9;
        engine.visible.store(false, Ordering::Relaxed);
        engine.base_ms = 1000; engine.frames = 8000;
        let mut sink = Sink::open(blocking, FAKE, 8000, 2, 100).unwrap();
        sink.set_fake_delay(2000);
        engine.sink = Some(sink);
        engine.status.lock().unwrap().position_ms = 99;
        engine
    }

    #[test]
    fn bluetooth_absolute_volume_leaves_samples_at_unity_and_software_volume_otherwise() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut engine = deterministic_spoken_engine(&blocking, Arc::new(Recorder::default()));
        let events = Arc::new(Mutex::new(Vec::new()));
        engine.hw = Box::new(RoutingOutput { fault: None, on_error: None, captured: Default::default(), drained: Arc::new(AtomicBool::new(false)),
            events: events.clone(), queues: Default::default(), queue_limit: None, hardware_volume: true });
        engine.route.route = crate::audio::output::Route::Bluetooth;
        for volume in [1u8, 30, 60, 99, 100] {
            engine.handle(Cmd::SetRemoteVolume(false));
            engine.handle(Cmd::SetVolume(volume));
            // Headphones without absolute volume: the player's own 0.5 dB steps.
            assert_eq!((engine.soft_volume, engine.output_gain, engine.hardware_volume), (soft_gain(volume), 1.0, false), "software {volume}");
            // With it: the headphones' amplifier is the volume, the samples stay whole.
            engine.handle(Cmd::SetRemoteVolume(true));
            assert_eq!((engine.soft_volume, engine.output_gain, engine.hardware_volume), (1.0, 1.0, false), "absolute {volume}");
        }
        engine.handle(Cmd::SetVolume(0));
        assert_eq!(engine.soft_volume, 0.0, "zero still mutes");
        // Full volume is full scale either way: no ceiling below the headphones' range.
        engine.handle(Cmd::SetVolume(100));
        engine.handle(Cmd::SetRemoteVolume(false));
        assert_eq!(engine.soft_volume, 1.0);
        // The jack's codec volume does not depend on the Bluetooth flag.
        engine.route.route = crate::audio::output::Route::Headphones;
        engine.handle(Cmd::SetVolume(60));
        let wired = (engine.soft_volume, engine.output_gain, engine.hardware_volume);
        engine.handle(Cmd::SetRemoteVolume(true));
        engine.apply_volume().unwrap();
        assert_eq!((engine.soft_volume, engine.output_gain, engine.hardware_volume), wired);
        assert!(wired.2 && events.lock().unwrap().iter().any(|e| e.starts_with("codec-volume:")));
        // Only Bluetooth was ever unity: the codec route attenuates at 60.
        assert!(wired.1 < 1.0);
    }

    #[test]
    fn codec_volume_failure_refuses_pcm_and_closes_active_output_without_software_fallback() {
        struct VolumeHw {
            failing: Arc<AtomicBool>,
            opened: Arc<std::sync::atomic::AtomicUsize>,
        }
        impl AudioHw for VolumeHw {
            fn device(&mut self) -> String { FAKE.into() }
            fn open_sink<'b>(&mut self, blocking: &'b crate::rt::Blocking, device: &str, rate: u32, channels: u32, latency: u32, format:crate::audio::pcm_format::PcmFormat) -> Result<Sink<'b>, String> {
                self.opened.fetch_add(1, Ordering::Relaxed);
                Sink::open_format(blocking,device,rate,channels,latency,format)
            }
            fn power_on(&mut self, _: &crate::rt::Blocking) {}
            fn power_off(&mut self, _: &crate::rt::Blocking) {}
            fn mute(&mut self, _: &crate::rt::Blocking, _: bool) {}
            fn set_attenuation(&mut self, _: &crate::rt::Blocking, _: u8) -> Result<bool, String> {
                if self.failing.load(Ordering::Relaxed) { Err("generated codec readback failure".into()) }
                else { Ok(true) }
            }
            fn set_filter(&mut self, _: &crate::rt::Blocking, _: u8) {}
        }
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut engine = deterministic_spoken_engine(&blocking, Arc::new(Recorder::default()));
        let failing = Arc::new(AtomicBool::new(true));
        let opened = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        engine.hw = Box::new(VolumeHw { failing: failing.clone(), opened: opened.clone() });
        let trace=Arc::new(crate::playtrace::Trace::new(true));
        engine.tap=crate::playtrace::Tap::new(trace.clone());
        assert!(engine.ensure_sink(48000, 2).unwrap_err().contains("codec readback"));
        assert!(engine.sink.is_none());
        assert_eq!(opened.load(Ordering::Relaxed), 0);
        failing.store(false, Ordering::Relaxed);
        engine.ensure_sink(48000, 2).unwrap();
        assert_eq!(engine.soft_volume, 1.0);
        assert_eq!(opened.load(Ordering::Relaxed), 1);
        engine.state = PlayState::Playing;
        failing.store(true, Ordering::Relaxed);
        engine.handle(Cmd::SetVolume(40));
        assert!(engine.sink.is_none());
        assert_eq!(engine.state, PlayState::Paused);
        assert!(engine.error.as_ref().unwrap().contains("codec readback"));
        assert_eq!(engine.soft_volume, 1.0, "failed writes never select software fallback");
        let failed=trace.snapshot().events.into_iter().filter(|e|e.a==1).collect::<Vec<_>>();
        assert!(failed.iter().any(|e|e.kind==crate::playtrace::Kind::VolumeEnd && e.b==3 && e.c==1));
        assert!(!failed.iter().any(|e|e.kind==crate::playtrace::Kind::VolumeEnd && e.b==0));
        failing.store(false, Ordering::Relaxed);
        engine.ensure_sink(48000, 2).unwrap();
        assert_eq!(opened.load(Ordering::Relaxed), 2);
    }

    /// Records the Headphone Power writes, PCM opens and codec attenuation.
    struct PowerHw { events: Arc<Mutex<Vec<String>>>, failing: Arc<AtomicBool>, old_driver: bool }
    impl AudioHw for PowerHw {
        fn device(&mut self) -> String { FAKE.into() }
        fn open_sink<'b>(&mut self, blocking: &'b crate::rt::Blocking, device: &str, rate: u32, channels: u32, latency: u32, format:crate::audio::pcm_format::PcmFormat) -> Result<Sink<'b>, String> {
            self.events.lock().unwrap().push("open".into());
            Sink::open_format(blocking,device,rate,channels,latency,format)
        }
        fn power_on(&mut self, _: &crate::rt::Blocking) {}
        fn power_off(&mut self, _: &crate::rt::Blocking) {}
        fn mute(&mut self, _: &crate::rt::Blocking, _: bool) {}
        fn set_attenuation(&mut self, _: &crate::rt::Blocking, steps: u8) -> Result<bool, String> {
            self.events.lock().unwrap().push(format!("codec:{steps}"));
            Ok(true)
        }
        fn set_headphone_power(&mut self, _: &crate::rt::Blocking, level: crate::config::HeadphonePower) -> Result<crate::config::HeadphonePower, String> {
            if self.failing.load(Ordering::Relaxed) { return Err("generated EBUSY".into()); }
            self.events.lock().unwrap().push(format!("power:{}", level.label()));
            Ok(if self.old_driver && level == crate::config::HeadphonePower::Efficient { crate::config::HeadphonePower::Standard } else { level })
        }
        fn set_filter(&mut self, _: &crate::rt::Blocking, _: u8) {}
    }

    fn power_engine<'a>(blocking: &'a crate::rt::Blocking, old_driver: bool) -> (Engine<'a>, Arc<Mutex<Vec<String>>>, Arc<AtomicBool>) {
        let mut engine = deterministic_spoken_engine(blocking, Arc::new(Recorder::default()));
        let events = Arc::new(Mutex::new(Vec::new()));
        let failing = Arc::new(AtomicBool::new(false));
        engine.hw = Box::new(PowerHw { events: events.clone(), failing: failing.clone(), old_driver });
        engine.volume = 60;
        engine.headphone_power = crate::config::HeadphonePower::Efficient;
        engine.applied_power = None;
        (engine, events, failing)
    }

    fn taken(events: &Arc<Mutex<Vec<String>>>) -> Vec<String> { std::mem::take(&mut *events.lock().unwrap()) }

    #[test]
    fn headphone_power_is_written_before_the_pcm_opens_and_makes_up_efficient_volume() {
        use crate::config::HeadphonePower::*;
        let blocking = crate::rt::Blocking::dedicated_thread();
        let (mut engine, events, _) = power_engine(&blocking, false);
        engine.ensure_sink(48000, 2).unwrap();
        // Volume 60 is 40 half-dB steps; Efficient's 6 dB lower full scale
        // takes 12 of them back in the codec.
        assert_eq!(taken(&events), ["power:Efficient", "codec:28", "open"]);
        assert_eq!(engine.applied_power, Some((Efficient, Efficient)));
        // While stopped a change lands at once, before the next attenuation.
        engine.release_output();
        engine.handle(Cmd::SetHeadphonePower(Standard));
        engine.ensure_sink(48000, 2).unwrap();
        assert_eq!(taken(&events), ["power:Standard", "codec:40", "open"]);
        // Mute stays mute on Efficient.
        engine.release_output();
        engine.handle(Cmd::SetHeadphonePower(Efficient));
        engine.volume = 0;
        engine.apply_volume().unwrap();
        assert_eq!(taken(&events), ["power:Efficient", "codec:255"]);
    }

    #[test]
    fn headphone_power_waits_while_playing_and_lands_at_the_next_pause() {
        use crate::config::HeadphonePower::*;
        let blocking = crate::rt::Blocking::dedicated_thread();
        let (mut engine, events, _) = power_engine(&blocking, false);
        engine.ensure_sink(48000, 2).unwrap();
        engine.state = PlayState::Playing;
        taken(&events);
        engine.handle(Cmd::SetHeadphonePower(High));
        assert!(taken(&events).is_empty(), "nothing reaches the codecs with the headphone PCM open");
        assert!(engine.sink.is_some(), "the change does not interrupt playback");
        assert_eq!(engine.applied_power, Some((Efficient, Efficient)));
        engine.pause();
        assert!(engine.sink.is_none(), "the pause parks the output for the change");
        assert_eq!(taken(&events), ["power:High"]);
        assert!(engine.parked.is_some(), "the queue is kept");
        // Resume reopens with the new level's attenuation and writes no level.
        engine.resume();
        assert!(engine.sink.is_some());
        let reopened = taken(&events);
        assert!(!reopened.iter().any(|e| e.starts_with("power:")), "{reopened:?}");
        assert_eq!(reopened.iter().filter(|e| e.starts_with("codec:") || *e == "open").cloned().collect::<Vec<_>>().last().map(String::as_str), Some("open"));
        assert!(reopened.contains(&"codec:40".to_string()), "{reopened:?}");
        // Paused with the PCM open: the change parks it at once.
        engine.pause();
        assert!(engine.sink.is_some(), "a pause with nothing waiting keeps the PCM");
        taken(&events);
        engine.handle(Cmd::SetHeadphonePower(Maximum));
        assert!(engine.sink.is_none());
        assert_eq!(taken(&events), ["power:Maximum"]);
    }

    #[test]
    fn a_refused_headphone_power_write_gets_no_make_up_gain_and_is_tried_again_at_the_next_open() {
        use crate::config::HeadphonePower::*;
        let blocking = crate::rt::Blocking::dedicated_thread();
        let (mut engine, events, failing) = power_engine(&blocking, false);
        failing.store(true, Ordering::Relaxed);
        engine.ensure_sink(48000, 2).unwrap();
        assert_eq!(taken(&events), ["codec:40", "open"], "an unknown level is never made louder");
        assert_eq!(engine.applied_power, None);
        engine.state = PlayState::Playing;
        engine.pause();
        assert!(engine.sink.is_some(), "a failed write does not make every pause close the output");
        engine.release_output();
        failing.store(false, Ordering::Relaxed);
        engine.ensure_sink(48000, 2).unwrap();
        assert_eq!(taken(&events), ["power:Efficient", "codec:28", "open"]);
        // A codec driver without patch 0187 holds Standard for Efficient:
        // no make-up gain, and no rewrite at every open.
        let (mut engine, events, _) = power_engine(&blocking, true);
        engine.ensure_sink(48000, 2).unwrap();
        assert_eq!(taken(&events), ["power:Efficient", "codec:40", "open"]);
        assert_eq!(engine.applied_power, Some((Efficient, Standard)));
        engine.release_output();
        engine.ensure_sink(48000, 2).unwrap();
        assert_eq!(taken(&events), ["codec:40", "open"]);
    }

    #[test]
    fn music_cursor_is_fresh_without_snapshot_publication_or_screen_wake() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut engine = cursor_engine(&blocking);
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0)); let notice = count.clone();
        engine.notify = Arc::new(move || { notice.fetch_add(1, Ordering::Relaxed); });
        let published = engine.published;
        let (reply, mut received)=tokio::sync::oneshot::channel();
        assert!(engine.handle(Cmd::ObserveMusicCursor { deadline: Instant::now() + Duration::from_secs(2), reply }));
        let cursor = received.try_recv().unwrap().unwrap();
        assert_eq!(cursor.position_ms, 1750);
        assert_eq!((cursor.written_frames, cursor.queued_frames, cursor.sample_rate), (8000, 2000, 8000));
        assert_eq!((cursor.generation, cursor.seek_revision, cursor.queue_revision, cursor.output_revision), (3, 4, 5, 9));
        assert_eq!((cursor.queue_index, cursor.queue_len), (0, 1));
        assert_eq!(cursor.selection_tag, 2694481079);
        assert!(cursor.begin_us > 0 && cursor.end_us >= cursor.begin_us);
        assert_eq!(engine.status.lock().unwrap().position_ms, 99);
        assert_eq!(engine.published, published);
        assert_eq!(count.load(Ordering::Relaxed), 0);
        assert!(!engine.visible.load(Ordering::Relaxed));
        assert!(!engine.checkpoints.is_prepared());
    }

    #[test]
    fn music_cursor_refuses_unknown_delay_and_cross_track_tail() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut engine = cursor_engine(&blocking);
        engine.sink.as_mut().unwrap().fail_fake_delay(true);
        assert_eq!(engine.observe_music_cursor(), Err("music cursor delay unavailable"));
        engine.sink.as_mut().unwrap().fail_fake_delay(false);
        engine.sink.as_mut().unwrap().set_fake_delay(8001);
        assert_eq!(engine.observe_music_cursor(), Err("music cursor crosses an unobserved output boundary"));
        engine.sink.as_mut().unwrap().set_fake_delay(0);
        engine.base_ms = u64::MAX;
        assert_eq!(engine.observe_music_cursor(), Err("music cursor position overflow"));
    }

    #[test]
    fn music_cursor_refuses_inactive_faulted_or_unsupported_playback() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        for case in 0..8 {
            let mut engine = cursor_engine(&blocking);
            match case {
                0 => engine.state = PlayState::Paused,
                1 => engine.state = PlayState::Buffering,
                2 => engine.error = Some("Generated failure".into()),
                3 => engine.pause_admission_fault = Some("Generated pause failure".into()),
                4 => engine.sink = None,
                5 => Arc::make_mut(&mut engine.queue)[0].radio_url = Some("http://generated.invalid/".into()),
                6 => engine.queue = std::sync::Arc::new(spoken_queue()),
                _ => engine.scrub_until = Some(Instant::now() + Duration::from_secs(1)),
            }
            assert!(engine.observe_music_cursor().is_err(), "case {case}");
        }
    }

    #[test]
    fn music_cursor_honors_expiry_cancellation_and_storage_barriers() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut engine = cursor_engine(&blocking);
        let (reply, mut received)=tokio::sync::oneshot::channel();
        engine.handle(Cmd::ObserveMusicCursor { deadline: Instant::now() - Duration::from_secs(1), reply });
        assert_eq!(received.try_recv().unwrap(), Err("music cursor observation expired"));
        let (reply, received)=tokio::sync::oneshot::channel(); drop(received);
        engine.handle(Cmd::ObserveMusicCursor { deadline: Instant::now() + Duration::from_secs(2), reply });
        assert_eq!(engine.status.lock().unwrap().position_ms, 99);
        engine.handoff_frozen = true;
        let (reply, mut received)=tokio::sync::oneshot::channel();
        engine.handle(Cmd::ObserveMusicCursor { deadline: Instant::now() + Duration::from_secs(2), reply });
        assert!(matches!(received.try_recv(), Err(tokio::sync::oneshot::error::TryRecvError::Closed)));
    }

    #[test]
    fn sleep_timer_registration_does_not_cut_in_flight_media() {
        let (handle, commands) = PlayerHandle::fixture(Status::default());
        let cancel = Arc::new(crate::audio::interrupt::Interrupt::new(false));
        *handle.current_io.lock().unwrap() = Some(IoSession { cancel: cancel.clone(), ready: false });
        let deadline = Instant::now() + Duration::from_secs(300);
        handle.send(Cmd::SetSleepTimer { deadline:Some(deadline) });
        handle.send(Cmd::StartSleepFade { deadline });
        handle.send(Cmd::SetSleepTimer { deadline:None });
        assert!(matches!(commands.try_recv(), Ok(Cmd::SetSleepTimer {deadline:Some(at)}) if at==deadline));
        assert!(matches!(commands.try_recv(), Ok(Cmd::StartSleepFade {deadline:at}) if at==deadline));
        assert!(matches!(commands.try_recv(), Ok(Cmd::SetSleepTimer {deadline:None})));
        assert!(!handle.interrupt.load(Ordering::Relaxed));
        assert!(!cancel.load(Ordering::Relaxed));
    }

    #[test]
    fn adding_to_the_queue_preserves_the_opening_track() {
        let (handle, _commands) = PlayerHandle::fixture(Status::default());
        let cancel = Arc::new(crate::audio::interrupt::Interrupt::new(false));
        *handle.current_io.lock().unwrap() = Some(IoSession { cancel: cancel.clone(), ready: false });
        let seed = Arc::new(vec![song("first")]);
        handle.send(Cmd::AppendShuffle { seed, songs: vec![song("second")] });
        handle.send(Cmd::Insert { next: true, songs: vec![song("third")] });
        assert!(!handle.interrupt.load(Ordering::Relaxed));
        assert!(!cancel.load(Ordering::Relaxed));
        handle.send(Cmd::Next);
        assert!(cancel.load(Ordering::Relaxed), "a skip must still cut a blocked open");
    }

    #[test]
    fn shuffle_completion_belongs_to_the_exact_queue_that_requested_it() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut engine = deterministic_spoken_engine(&blocking, Arc::new(Recorder::default()));
        let first = Arc::new(vec![song("same-song")]);
        let second = Arc::new(vec![song("same-song")]);
        for seed in [&first, &second] {
            engine.handle(Cmd::Play { songs: seed.clone(), index: 0, shuffle: false });
        }
        engine.handle(Cmd::AppendShuffle { seed: first, songs: vec![song("stale")] });
        assert!(Arc::ptr_eq(&engine.queue, &second));
        let generation = engine.generation;
        engine.handle(Cmd::AppendShuffle { seed: second, songs: vec![song("correct")] });
        assert_eq!(engine.generation, generation);
        assert_eq!(engine.queue.len(), 2);
        assert_eq!(engine.queue[1].id, "correct");
    }

    #[test]
    fn music_cursor_request_does_not_interrupt_in_flight_media() {
        let (handle, commands) = PlayerHandle::fixture(Status::default());
        let cancel = Arc::new(crate::audio::interrupt::Interrupt::new(false));
        *handle.current_io.lock().unwrap() = Some(IoSession { cancel: cancel.clone(), ready: false });
        let (reply, _received)=tokio::sync::oneshot::channel();
        handle.send(Cmd::ObserveMusicCursor { deadline: Instant::now() + Duration::from_secs(2), reply });
        assert!(matches!(commands.try_recv().unwrap(), Cmd::ObserveMusicCursor { .. }));
        assert!(!handle.interrupt.load(Ordering::Relaxed));
        assert!(!cancel.load(Ordering::Relaxed));
    }

    #[test]
    fn hidden_checkpoint_failure_forces_notice_even_before_periodic_deadline() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let source=Arc::new(SpokenRecorder {positions:Default::default(),fail:AtomicBool::new(true),
            read_fail:AtomicBool::new(false),settings:Default::default()});
        let mut engine=deterministic_spoken_engine(&blocking, source);
        engine.prepare_checkpoints().unwrap();
        engine.queue=std::sync::Arc::new(spoken_queue()); engine.state=PlayState::Playing;
        engine.visible.store(false,Ordering::Relaxed);
        let notifications=Arc::new(std::sync::atomic::AtomicUsize::new(0)); let observed=notifications.clone();
        engine.notify=Arc::new(move || {observed.fetch_add(1,Ordering::Relaxed);});
        engine.publish(true); notifications.store(0,Ordering::Relaxed);
        engine.published=Instant::now()-Duration::from_secs(31);
        engine.checkpointed=Instant::now()-Duration::from_secs(31);
        engine.publish(false);
        let deadline=Instant::now()+Duration::from_secs(3);
        while notifications.load(Ordering::Relaxed)==0 && Instant::now()<deadline {
            engine.publish(false);
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(notifications.load(Ordering::Relaxed),1,"failed durability bypasses presentation suppression");
        let status=engine.status.lock().unwrap();
        assert_eq!(status.state,PlayState::Paused);
        assert!(status.error.is_some());
    }

    #[test]
    fn dark_output_ownership_and_fault_changes_notify_without_forced_publish() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut engine=deterministic_spoken_engine(&blocking, Arc::new(Recorder::default()));
        engine.prepare_checkpoints().unwrap();
        engine.queue=std::sync::Arc::new(spoken_queue()); engine.state=PlayState::Playing;
        let count=Arc::new(std::sync::atomic::AtomicUsize::new(0)); let seen=count.clone();
        engine.notify=Arc::new(move || {seen.fetch_add(1,Ordering::Relaxed);});
        engine.visible.store(false,Ordering::Relaxed);
        engine.publish(true); count.store(0,Ordering::Relaxed);
        engine.route.generation+=1;
        engine.published=Instant::now()-Duration::from_secs(31);
        engine.publish(false);
        assert_eq!(count.load(Ordering::Relaxed),1,"route acknowledgment is not position-only");
        assert_eq!(engine.status.lock().unwrap().output_revision,u64::from(engine.route.generation));
        engine.pause_admission_fault=Some("Generated pause ownership fault".into());
        engine.published=Instant::now()-Duration::from_secs(31);
        engine.publish(false);
        assert_eq!(count.load(Ordering::Relaxed),2,"admission failure cannot be coalesced while dark");
        assert!(engine.status.lock().unwrap().pause_admission_failed);
    }

    #[test]
    fn dark_buffering_to_playing_publishes_at_once() {
        // V0.107 device trace: with the screen off, Buffering → Playing waited
        // for the 30 s dark interval, so the UI and its journey saw Buffering.
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut engine=deterministic_spoken_engine(&blocking, Arc::new(Recorder::default()));
        engine.prepare_checkpoints().unwrap();
        engine.queue=std::sync::Arc::new(spoken_queue()); engine.state=PlayState::Buffering;
        let count=Arc::new(std::sync::atomic::AtomicUsize::new(0)); let seen=count.clone();
        engine.notify=Arc::new(move || {seen.fetch_add(1,Ordering::Relaxed);});
        engine.visible.store(false,Ordering::Relaxed);
        engine.publish(true); count.store(0,Ordering::Relaxed);
        engine.state=PlayState::Playing;
        engine.publish(false);
        assert_eq!(engine.status.lock().unwrap().state,PlayState::Playing);
        assert_eq!(count.load(Ordering::Relaxed),1);
        engine.publish(false);
        assert_eq!(count.load(Ordering::Relaxed),1,"unchanged dark state stays coalesced");
    }

    #[test]
    fn dark_nonforced_state_and_metadata_publication_remains_mandatory() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut engine=deterministic_spoken_engine(&blocking, Arc::new(Recorder::default()));
        engine.prepare_checkpoints().unwrap();
        engine.queue=std::sync::Arc::new(spoken_queue()); engine.state=PlayState::Playing;
        let count=Arc::new(std::sync::atomic::AtomicUsize::new(0)); let seen=count.clone();
        engine.notify=Arc::new(move || {seen.fetch_add(1,Ordering::Relaxed);});
        engine.visible.store(false,Ordering::Relaxed);
        engine.publish(true); count.store(0,Ordering::Relaxed);
        engine.state=PlayState::Buffering;
        engine.published=Instant::now()-Duration::from_secs(31);
        engine.publish(false);
        assert_eq!(count.load(Ordering::Relaxed),1,"a nonforced state transition is not position-only");
        engine.speed_request+=1;
        engine.published=Instant::now()-Duration::from_secs(31);
        engine.publish(false);
        assert_eq!(count.load(Ordering::Relaxed),2,"a changed acknowledgement remains mandatory");
    }

    #[test]
    fn dark_periodic_snapshots_coalesce_ui_wakes_without_dropping_checkpoints_or_forced_updates() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let source=Arc::new(Recorder::default());
        let mut engine=deterministic_spoken_engine(&blocking, source.clone());
        engine.prepare_checkpoints().unwrap();
        engine.queue=std::sync::Arc::new(spoken_queue()); engine.state=PlayState::Playing;
        let notifications=Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed=notifications.clone();
        engine.notify=Arc::new(move || { observed.fetch_add(1,Ordering::Relaxed); });
        engine.visible.store(false,Ordering::Relaxed);
        engine.publish(true); notifications.store(0,Ordering::Relaxed);
        for step in 1..=64 {
            engine.resume_at=step*1000; engine.base_ms=step*1000;
            engine.published=Instant::now()-Duration::from_secs(31);
            engine.checkpointed=Instant::now()-Duration::from_secs(31);
            engine.publish(false);
            assert_eq!(engine.status.lock().unwrap().position_ms,step*1000,"latest dark snapshot is retained");
            assert!(engine.checkpointed.elapsed()<Duration::from_secs(1),"periodic durability still scheduled");
        }
        assert_eq!(notifications.load(Ordering::Relaxed),0,"position-only hidden notifications are coalesced");
        assert!(engine.save(),"ordered final durability still completes");
        assert!(source.checkpointed_positions.lock().unwrap().iter().any(|(_,position)|*position==64000));
        engine.handle(Cmd::Pause);
        assert_eq!(notifications.load(Ordering::Relaxed),1,"pause publishes immediately while hidden");
        assert_eq!(engine.status.lock().unwrap().state,PlayState::Paused);
        engine.handle(Cmd::SetSpeed {generation:engine.generation,percent:Some(125),request:91});
        let forced=notifications.load(Ordering::Relaxed);
        assert!(forced>=2,"speed acknowledgement is mandatory, including any buffering transition");
        assert_eq!(engine.status.lock().unwrap().speed_request,91);
        engine.visible.store(true,Ordering::Relaxed);
        engine.published=Instant::now()-Duration::from_secs(1);
        engine.publish(false);
        assert_eq!(notifications.load(Ordering::Relaxed),forced+1,"visible periodic updates resume");
    }

    #[test]
    fn player_handle_partial_output_failure_retains_selection_and_exact_rendered_pcm() {
        for percent in [None,Some(50),Some(125),Some(200),Some(300)] {
            let expected=output_fault_trace(percent,None,false,false);
            for frames in [1,5] {
                let actual=output_fault_trace(percent,Some(frames),frames==5,false);
                assert_eq!(actual.len(),expected.len(),"sample count changed at {percent:?}% / {frames} frames");
                assert!(actual==expected,"rendered PCM changed at {percent:?}% / {frames} frames");
            }
            let replay=output_fault_trace(percent,Some(5),false,true);
            assert_eq!(replay.len(),expected.len(),"partial replay changed sample count at {percent:?}%");
            assert!(replay==expected,"partially heard replay must not duplicate samples at {percent:?}%");
        }
    }

    struct PauseFailureOutput {
        code:i32,
        state_fixture:Option<(i32,Arc<std::sync::atomic::AtomicUsize>)>,
        entered:Option<mpsc::Sender<()>>,
        release:mpsc::Receiver<()>,
        captured:Arc<Mutex<Vec<i32>>>,
        drained:Arc<AtomicBool>,
    }
    impl AudioHw for PauseFailureOutput {
        fn device(&mut self)->String {FAKE.into()}
        fn open_sink<'a>(&mut self, blocking: &'a crate::rt::Blocking,_:&str,rate:u32,channels:u32,latency:u32,format:crate::audio::pcm_format::PcmFormat)->Result<Sink<'a>,String> {
            let mut sink=Sink::open_format(blocking,FAKE,rate,channels,latency,format)?;
            sink.script_pause_code(self.code);
            if let Some((state,calls))=&self.state_fixture {sink.script_pcm_state(*state,calls.clone());}
            sink.script_writes(super::super::alsa::WriteFixture {steps:Default::default(),
                accepted:self.captured.clone(),queued:Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                drained:self.drained.clone(),consumed_on_error:Default::default(),on_error:None,first_write:None,queue_limit:Some(512)});
            if let Some(entered)=self.entered.take() {
                entered.send(()).unwrap();
                self.release.recv_timeout(Duration::from_secs(3)).expect("generated first-open gate released");
            }
            Ok(sink)
        }
        fn power_on(&mut self, _blocking: &crate::rt::Blocking) {} fn power_off(&mut self, _blocking: &crate::rt::Blocking) {} fn mute(&mut self, _blocking: &crate::rt::Blocking,_:bool) {}
        fn set_attenuation(&mut self, _blocking: &crate::rt::Blocking,_:u8) -> Result<bool, String> {Ok(false)}
        fn set_filter(&mut self, _blocking: &crate::rt::Blocking,_:u8) {}
    }

    fn pause_failure_trace(retry_barriers:bool,code:i32) {
        let expected=output_fault_trace(Some(100),None,false,false);
        let source=output_source(Some(100));
        let captured=Arc::new(Mutex::new(Vec::new()));let drained=Arc::new(AtomicBool::new(false));
        let (entered,opened)=mpsc::channel();let (release,gate)=mpsc::channel();
        let player=spawn_outputs(source.clone(),Box::new(PauseFailureOutput {code,state_fixture:None,entered:Some(entered),release:gate,
            captured:captured.clone(),drained:drained.clone()}),output_settings(),Arc::new(||{}),None);
        let mut selected=spoken_queue()[0].clone();selected.spoken.as_mut().unwrap().duration_ms=1200;
        let id=selected.id.clone();player.send(Cmd::Play {songs:std::sync::Arc::new(vec![selected]),index:0,shuffle:false});
        opened.recv_timeout(Duration::from_secs(3)).unwrap();
        player.send(Cmd::Pause);player.send(Cmd::SetRepeat(Repeat::One));release.send(()).unwrap();
        let paused=wait_for(&player,2,|s|s.state==PlayState::Paused && s.repeat==Repeat::One);
        let prefix=captured.lock().unwrap().len();
        let io_ready=player.current_io.lock().unwrap().as_ref().is_some_and(|io|io.ready && !io.cancel.load(Ordering::Relaxed));
        let mut repeated_acks=Vec::new();
        if retry_barriers {
            for park in [false,true] {
                if park {player.send(Cmd::ParkOutput);}
                player.send(Cmd::Pause);
                let (ack,reply)=mpsc::channel();player.send(Cmd::PrepareHandoff(ack.into()));
                repeated_acks.push(reply.recv_timeout(Duration::from_secs(3)).expect("worker barrier responds"));
                player.cancel_handoff();
            }
        }
        player.send(Cmd::SetRepeat(Repeat::All));
        let held=wait_for(&player,2,|s|s.repeat==Repeat::All);
        let no_auto_resume=held.state==PlayState::Paused && captured.lock().unwrap().len()==prefix;
        player.send(Cmd::SetRepeat(Repeat::Off));player.send(Cmd::Resume {generation:paused.generation});
        let deadline=Instant::now()+Duration::from_secs(3);
        while !drained.load(Ordering::Acquire) && Instant::now()<deadline {std::thread::sleep(Duration::from_millis(2));}
        player.shutdown();
        assert!(drained.load(Ordering::Acquire));
        assert!(repeated_acks.iter().all(Result::is_err),
            "repeat/park admission silently accepted an unresolved pause fault: {repeated_acks:?}");
        assert!(io_ready && no_auto_resume,"pause fault must keep a ready media session paused");
        assert_eq!(source.opened.lock().unwrap().as_slice(),[id]);
        assert_eq!(player.status().generation,paused.generation);
        let samples=captured.lock().unwrap();assert!(prefix>1024);
        let mut heard=samples[..prefix-1024].to_vec();heard.extend_from_slice(&samples[prefix..]);
        assert_eq!(heard,expected,"known queue ownership must survive the explicit retry");
        let unsupported=[-libc::ENOSYS,-libc::EINVAL,-libc::EOPNOTSUPP].contains(&code);
        // UX-006: the published notice is plain text; its presence carries the distinction.
        assert_eq!(paused.error.is_some(),!unsupported,
            "unsupported pause and operational failure must stay distinct");
        assert!(player.status().error.is_none(),"a successful explicit retry clears the output error");
    }

    #[test]
    fn player_handle_pause_timeout_is_visible_and_keeps_media_until_explicit_resume() {
        pause_failure_trace(false,-libc::ETIMEDOUT);
    }

    #[test]
    fn player_handle_unresolved_pause_fault_cannot_be_readmitted_after_repeat_or_park() {
        pause_failure_trace(true,-libc::ETIMEDOUT);
    }

    #[test]
    fn player_handle_unsupported_pause_keeps_existing_software_fallback() {
        for code in [-libc::ENOSYS,-libc::EINVAL,-libc::EOPNOTSUPP] {
            pause_failure_trace(false,code);
        }
    }

    #[test]
    fn player_handle_prepared_software_queue_pauses_without_a_transport_fault_or_source_reopen() {
        // The actual target recovered a write XRUN, then had software-buffered
        // PCM in PREPARED. Hardware pause is invalid in that state (EBADFD),
        // while the current queued suffix remains owned by this worker.
        let expected=output_fault_trace(Some(100),None,false,false);
        let source=output_source(Some(100));
        let captured=Arc::new(Mutex::new(Vec::new()));let drained=Arc::new(AtomicBool::new(false));
        let calls=Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (entered,opened)=mpsc::channel();let (release,gate)=mpsc::channel();
        let player=spawn_outputs(source.clone(),Box::new(PauseFailureOutput {code:-libc::EBADFD,
            state_fixture:Some((2,calls.clone())),entered:Some(entered),release:gate,
            captured:captured.clone(),drained:drained.clone()}),output_settings(),Arc::new(||{}),None);
        let mut selected=spoken_queue()[0].clone();selected.spoken.as_mut().unwrap().duration_ms=1200;
        let id=selected.id.clone();player.send(Cmd::Play {songs:std::sync::Arc::new(vec![selected]),index:0,shuffle:false});
        opened.recv_timeout(Duration::from_secs(3)).unwrap();
        player.send(Cmd::Pause);player.send(Cmd::SetRepeat(Repeat::One));release.send(()).unwrap();
        let paused=wait_for(&player,2,|s|s.state==PlayState::Paused && s.repeat==Repeat::One);
        let prefix=captured.lock().unwrap().len();assert!(prefix>1024);
        let initial_ready=player.current_io.lock().unwrap().as_ref().is_some_and(|io|io.ready && !io.cancel.load(Ordering::Relaxed));
        player.send(Cmd::Pause);player.send(Cmd::SetRepeat(Repeat::All));
        let repeated=wait_for(&player,2,|s|s.repeat==Repeat::All);
        player.send(Cmd::ParkOutput);player.send(Cmd::Pause);player.send(Cmd::SetRepeat(Repeat::One));
        let parked=wait_for(&player,2,|s|s.repeat==Repeat::One);
        let still_ready=player.current_io.lock().unwrap().as_ref().is_some_and(|io|io.ready && !io.cancel.load(Ordering::Relaxed));
        let stayed_paused=parked.state==PlayState::Paused && captured.lock().unwrap().len()==prefix;
        player.send(Cmd::SetRepeat(Repeat::Off));player.send(Cmd::Resume {generation:paused.generation});
        let deadline=Instant::now()+Duration::from_secs(3);
        while !drained.load(Ordering::Acquire) && Instant::now()<deadline {std::thread::sleep(Duration::from_millis(2));}
        player.shutdown();
        assert!(drained.load(Ordering::Acquire));
        assert!(initial_ready && still_ready && stayed_paused);
        assert_eq!(source.opened.lock().unwrap().as_slice(),[id]);
        assert_eq!(player.status().generation,paused.generation);
        assert_eq!((paused.generation,paused.position_ms),(parked.generation,parked.position_ms));
        let samples=captured.lock().unwrap();
        let mut heard=samples[..prefix-1024].to_vec();heard.extend_from_slice(&samples[prefix..]);
        assert_eq!(heard,expected,"the software queue must transfer exactly once through park/replay");
        assert!(paused.error.is_none() && !paused.pause_admission_failed && repeated.error.is_none() && parked.error.is_none(),
            "PREPARED software PCM was incorrectly treated as an operational pause failure: {paused:?}");
        assert_eq!(calls.load(Ordering::Relaxed),0,"hardware pause is invalid for PREPARED software PCM");
        assert!(player.status().error.is_none());
    }

    #[derive(Default)]
    struct RoutingQueues {
        opened: Arc<Mutex<Vec<Arc<std::sync::atomic::AtomicUsize>>>>,
        first_write: Option<mpsc::Sender<(bool, mpsc::Sender<()>)>>,
    }

    struct RoutingOutput {
        fault:Option<&'static str>,
        on_error:Option<Arc<dyn Fn()+Send+Sync>>,
        captured:Arc<Mutex<Vec<i32>>>,
        drained:Arc<AtomicBool>,
        events:Arc<Mutex<Vec<String>>>,
        queues:RoutingQueues,
        queue_limit:Option<usize>,
        hardware_volume:bool,
    }
    impl AudioHw for RoutingOutput {
        fn device(&mut self)->String {FAKE.into()}
        fn open_sink<'a>(&mut self, blocking: &'a crate::rt::Blocking,device:&str,rate:u32,channels:u32,latency:u32,format:crate::audio::pcm_format::PcmFormat)->Result<Sink<'a>,String> {
            self.events.lock().unwrap().push(format!("open:{device}"));
            let usb=device.starts_with("plughw:CARD=");
            let bluetooth=device==crate::bluetooth::pcm::DEVICE;
            let fault=if usb || matches!(self.fault,Some("headphone-open"|"descriptor"))
                || (bluetooth && matches!(self.fault,Some("bluetooth-disconnect"|"bluetooth-stall"))) {self.fault.take()}else{None};
            if matches!(fault,Some("open"|"headphone-open")) {return Err("Synthetic PCM format rejection".into());}
            let mut sink=Sink::open_format(blocking,FAKE,rate,channels,latency,format)?;
            let queued=Arc::new(std::sync::atomic::AtomicUsize::new(0));
            self.queues.opened.lock().unwrap().push(queued.clone());
            let steps=if fault==Some("descriptor") {
                std::collections::VecDeque::from([Ok(5),Err("write: File descriptor in bad state (os error 77)".into())])
            }else if fault==Some("write") {
                std::collections::VecDeque::from([Ok(5),Err("Synthetic USB unplug".into())])
            }else if let Some(fault@("bluetooth-disconnect"|"bluetooth-stall"))=fault {
                // The replay and two packets play, and the third write fails part way, with
                // the messages of `bluetooth::pcm::Writer::error` for ENODEV and EIO.
                let error=if fault=="bluetooth-disconnect" {"Bluetooth headphones disconnected"}else{"Bluetooth headphones stopped taking audio"};
                std::collections::VecDeque::from([Ok(usize::MAX),Ok(usize::MAX),Ok(usize::MAX),Ok(40),Err(error.into())])
            }else{Default::default()};
            sink.script_writes(super::super::alsa::WriteFixture {
                steps,accepted:self.captured.clone(),queued,
                drained:self.drained.clone(),consumed_on_error:Default::default(),
                on_error:if matches!(fault,Some("write"|"bluetooth-disconnect")){self.on_error.take()}else{None},
                first_write: self.queues.first_write.clone().map(|notify| Box::new(move || {
                    let (release, wait) = mpsc::channel();
                    if notify.send((bluetooth, release)).is_ok() {
                        // Bound orphaned fixture cleanup if the test stops.
                        let _ = wait.recv_timeout(Duration::from_secs(5));
                    }
                }) as Box<dyn FnOnce() + Send>),
                queue_limit:self.queue_limit,
            });
            Ok(sink)
        }
        fn power_on(&mut self, _blocking: &crate::rt::Blocking) {self.events.lock().unwrap().push("power:on".into());}
        fn power_off(&mut self, _blocking: &crate::rt::Blocking) {self.events.lock().unwrap().push("power:off".into());}
        fn mute(&mut self, _blocking: &crate::rt::Blocking,on:bool) {self.events.lock().unwrap().push(format!("mute:{on}"));}
        fn set_attenuation(&mut self, _blocking: &crate::rt::Blocking,volume:u8) -> Result<bool, String> {
            if self.hardware_volume {self.events.lock().unwrap().push(format!("codec-volume:{volume}"));}
            Ok(self.hardware_volume)
        }
        fn set_filter(&mut self, _blocking: &crate::rt::Blocking,_:u8) {}
    }

    #[test]
    fn player_loop_heals_descriptor_write_failure_without_a_new_selection() {
        let source=output_source(None);
        let captured=Arc::new(Mutex::new(Vec::new()));
        let drained=Arc::new(AtomicBool::new(false));
        let events=Arc::new(Mutex::new(Vec::new()));
        let player=spawn_outputs(source.clone(),Box::new(RoutingOutput {
            fault:Some("descriptor"),on_error:None,captured:captured.clone(),
            drained:drained.clone(),events:events.clone(),queues:Default::default(),
            queue_limit:None,hardware_volume:false,
        }),output_settings(),Arc::new(||{}),None);
        let generation=player.status().generation;
        let mut book=spoken_queue()[0].clone();book.spoken.as_mut().unwrap().duration_ms=1200;
        let id=book.id.clone();
        player.send(Cmd::Play {songs:std::sync::Arc::new(vec![book]),index:0,shuffle:false});
        let deadline=Instant::now()+Duration::from_secs(3);
        while !drained.load(Ordering::Acquire) && Instant::now()<deadline {std::thread::sleep(Duration::from_millis(2));}
        player.shutdown();
        assert!(drained.load(Ordering::Acquire),"automatic reopen did not reach output completion");
        assert!(captured.lock().unwrap().len()>10,"no output after the injected partial write");
        assert_eq!(source.opened.lock().unwrap().as_slice(),[id.clone(),id]);
        assert_eq!(events.lock().unwrap().iter().filter(|row|row.starts_with("open:")).count(),2);
        let status=player.status();
        assert!(status.error.is_none(),"descriptor fault survived successful output: {status:?}");
        assert_eq!((status.generation,status.index),(generation+1,0));
    }

    #[test]
    fn actual_discovery_routes_usb_loss_and_stale_reenumeration_without_reopening_media() {
        let expected=output_fault_trace(Some(125),None,false,false);
        for scene in ["unplug","stale","open"] {
            let source=output_source(Some(125));
            let (monitor,topology)=super::super::output::Monitor::fixture();
            let topology=Arc::new(topology);
            let dac=super::super::output::UsbPlayback {identity:"platform/usb/1-1@1:2:1234:5678".into(),card:73,device:0};
            topology.update(vec![dac.clone()]);
            let on_error=if scene=="stale" {
                let topology=topology.clone();
                Some(Arc::new(move|| {
                    topology.removed(b"ACTION=remove\0SUBSYSTEM=usb\0DEVPATH=/devices/platform/usb/1-1\0");
                    topology.update(vec![dac.clone()]);
                }) as Arc<dyn Fn()+Send+Sync>)
            }else{None};
            let captured=Arc::new(Mutex::new(Vec::new()));
            let drained=Arc::new(AtomicBool::new(false));
            let events=Arc::new(Mutex::new(Vec::new()));
            let player=spawn_outputs(source.clone(),Box::new(RoutingOutput {
                fault:Some(if scene=="open" {"open"}else{"write"}),on_error,
                captured:captured.clone(),drained:drained.clone(),events:events.clone(),
                queues:Default::default(),queue_limit:None,hardware_volume:false,
            }),output_settings(),Arc::new(||{}),Some(monitor));
            let initial_generation=player.status().generation;
            let mut book=spoken_queue()[0].clone();book.spoken.as_mut().unwrap().duration_ms=1200;
            let id=book.id.clone();
            player.send(Cmd::Play {songs:std::sync::Arc::new(vec![book]),index:0,shuffle:false});
            let deadline=Instant::now()+Duration::from_secs(3);
            while !drained.load(Ordering::Acquire) && Instant::now()<deadline {std::thread::sleep(Duration::from_millis(2));}
            player.shutdown();
            let status=player.status();
            assert!(drained.load(Ordering::Acquire),"no automatic fallback/replacement in {scene}");
            assert_eq!(status.state,PlayState::Stopped);
            assert!(status.error.is_none(),"output error survived successful {scene}");
            assert_eq!(status.generation,initial_generation+1,"output must not create a new media instance");
            assert_eq!(status.index,0);assert_eq!(status.position_ms,1200);
            assert_eq!(source.opened.lock().unwrap().as_slice(),[id]);
            let accepted=captured.lock().unwrap();
            let skipped=if scene=="open" {0}else{10};
            assert_eq!(accepted.len()-skipped,expected.len(),"PCM count changed in {scene}");
            assert!(accepted[skipped..]==expected,"PCM order/gain/EQ changed in {scene}");
            let events=events.lock().unwrap();
            let opens:Vec<_>=events.iter().filter(|event|event.starts_with("open:")).map(String::as_str).collect();
            assert_eq!(opens,if scene=="stale" {vec!["open:plughw:CARD=73,DEV=0","open:plughw:CARD=73,DEV=0"]}
                else{vec!["open:plughw:CARD=73,DEV=0","open:q2nd-fake"]});
            let first=events.iter().position(|event|event=="open:plughw:CARD=73,DEV=0").unwrap();
            let end=events.iter().position(|event|event=="open:q2nd-fake").unwrap_or(events.len());
            assert!(!events[first..end].iter().any(|event|event=="mute:false"),"internal unmute before headphones PCM in {scene}: {events:?}");
        }
    }

    #[test]
    fn bluetooth_takes_playback_on_connect_and_leaves_it_paused_on_disconnect() {
        let expected=output_fault_trace(Some(125),None,false,false);
        let source=output_source(Some(125));
        let monitor=super::super::output::Monitor::detached();
        let bluetooth=monitor.bluetooth_switch();
        let captured=Arc::new(Mutex::new(Vec::new()));
        let drained=Arc::new(AtomicBool::new(false));
        let events=Arc::new(Mutex::new(Vec::new()));
        let (first_write, accepted) = mpsc::channel();
        let player=spawn_outputs(source.clone(),Box::new(RoutingOutput {
            fault:None,on_error:None,captured:captured.clone(),drained:drained.clone(),events:events.clone(),
            queues:RoutingQueues { first_write: Some(first_write), ..Default::default() },queue_limit:None,hardware_volume:false,
        }),output_settings(),Arc::new(||{}),Some(monitor));
        let opens=||events.lock().unwrap().iter().filter(|event|event.starts_with("open:")).cloned().collect::<Vec<_>>();
        let wait=|what:&str,done:&dyn Fn()->bool| {
            let deadline=Instant::now()+Duration::from_secs(3);
            while !done() {assert!(Instant::now()<deadline,"{what}: {:?}",opens());std::thread::sleep(Duration::from_millis(1));}
        };
        let mut book=spoken_queue()[0].clone();book.spoken.as_mut().unwrap().duration_ms=1200;
        let id=book.id.clone();
        player.send(Cmd::Play {songs:std::sync::Arc::new(vec![book]),index:0,shuffle:false});
        // This synthetic PCM accepts samples without a hardware clock. Hold
        // each first accepted write while delivering the corresponding edge,
        // so a busy test thread cannot let the entire track run to EOF.
        let (on_bluetooth, release) = accepted.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(!on_bluetooth);
        bluetooth(true);
        release.send(()).unwrap();
        let (on_bluetooth, release) = accepted.recv_timeout(Duration::from_secs(3)).unwrap();
        assert!(on_bluetooth);
        assert_eq!(opens(),["open:q2nd-fake","open:q2bt"]);
        assert_eq!(player.status().state,PlayState::Playing);
        bluetooth(false);
        drop(accepted); // the subsequent wired resume can play to completion
        release.send(()).unwrap();
        wait("pause",&||player.status().state==PlayState::Paused);
        std::thread::sleep(Duration::from_millis(150));
        let status=player.status();
        assert_eq!((status.state,opens().len()),(PlayState::Paused,2),"playback moved to another output by itself");
        assert!(status.error.is_none(),"an ordinary disconnect is not an error: {:?}",status.error);
        assert!(!drained.load(Ordering::Acquire));
        // The listener presses Play: the kept audio continues on the wired output.
        player.send(Cmd::PlayPause);
        wait("resume",&||drained.load(Ordering::Acquire));
        player.shutdown();
        assert_eq!(opens(),["open:q2nd-fake","open:q2bt","open:q2nd-fake"]);
        assert_eq!(source.opened.lock().unwrap().as_slice(),[id]);
        // Each output accepted a contiguous run of the expected PCM. A new output starts
        // again at the audio its predecessor had queued and not played, so the runs step
        // back at a route change and nowhere else, and nothing is skipped.
        let accepted=captured.lock().unwrap();
        let (mut at,mut steps_back)=(0,0);
        let mut index=0;
        while index<accepted.len() {
            if at<expected.len() && accepted[index]==expected[at] {index+=1;at+=1;continue;}
            let run=64.min(accepted.len()-index);
            let earlier=(0..at).rev().find(|&start|start+run<=expected.len() && accepted[index..index+run]==expected[start..start+run]);
            at=earlier.unwrap_or_else(||panic!("output {index} of {} is not the queued audio continuing",accepted.len()));
            steps_back+=1;
        }
        assert_eq!(at,expected.len(),"the end of the audio was not played");
        assert!((1..=2).contains(&steps_back),"{steps_back} replays for two route changes");
    }

    /// the original product report on V0.128: switching the headphones off mid-song paused with an error.
    /// The Bluetooth PCM fails the write that is in progress. Either the route has already
    /// left (the thread's order on a disconnect), or the write stalled first and the link
    /// drops after. Both end as an ordinary pause, with the unsent audio kept for Play.
    #[test]
    fn headphones_that_disconnect_during_a_write_leave_playback_paused_without_an_error() {
        let expected=output_fault_trace(Some(125),None,false,false);
        for scene in ["bluetooth-disconnect","bluetooth-stall"] {
            let source=output_source(Some(125));
            let monitor=super::super::output::Monitor::detached();
            let bluetooth=monitor.bluetooth_switch();
            let captured=Arc::new(Mutex::new(Vec::new()));
            let drained=Arc::new(AtomicBool::new(false));
            let events=Arc::new(Mutex::new(Vec::new()));
            // Every publication with an error, from the moment the headphones go.
            let published:Arc<std::sync::OnceLock<Arc<Mutex<Status>>>>=Default::default();
            let disconnected=Arc::new(AtomicBool::new(false));
            let errors=Arc::new(Mutex::new(Vec::new()));
            let notify={
                let (published,disconnected,errors)=(published.clone(),disconnected.clone(),errors.clone());
                Arc::new(move|| {
                    let Some(status)=published.get() else {return};
                    let Ok(status)=status.try_lock() else {return};
                    if disconnected.load(Ordering::Acquire) {if let Some(error)=&status.error {errors.lock().unwrap().push(error.clone());}}
                })
            };
            let on_error:Arc<dyn Fn()+Send+Sync>={
                let (bluetooth,disconnected)=(bluetooth.clone(),disconnected.clone());
                Arc::new(move|| {disconnected.store(true,Ordering::Release);bluetooth(false);})
            };
            let player=spawn_outputs(source.clone(),Box::new(RoutingOutput {
                fault:Some(scene),on_error:Some(on_error),captured:captured.clone(),drained:drained.clone(),events:events.clone(),
                queues:Default::default(),queue_limit:None,hardware_volume:false,
            }),output_settings(),notify,Some(monitor));
            published.set(player.status.clone()).unwrap();
            let opens=||events.lock().unwrap().iter().filter(|event|event.starts_with("open:")).cloned().collect::<Vec<_>>();
            let wait=|what:&str,done:&dyn Fn()->bool| {
                let deadline=Instant::now()+Duration::from_secs(3);
                while !done() {assert!(Instant::now()<deadline,"{scene}, {what}: {:?} {:?}",opens(),player.status().error);std::thread::sleep(Duration::from_millis(1));}
            };
            let mut book=spoken_queue()[0].clone();book.spoken.as_mut().unwrap().duration_ms=1200;
            let id=book.id.clone();
            // The recording PCM runs without wall-clock pacing. Stage the
            // connection during its first wired write so the whole book cannot
            // reach EOF before the test thread schedules the route change.
            let first_write = captured.lock().unwrap();
            player.send(Cmd::Play {songs:std::sync::Arc::new(vec![book]),index:0,shuffle:false});
            wait("wired output", &|| opens().len() == 1);
            bluetooth(true);
            drop(first_write);
            if scene=="bluetooth-stall" {
                // The headphones stop taking audio: still connected, so this is a failure to show.
                wait("stall",&||player.status().error.is_some());
                assert_eq!(player.status().state,PlayState::Paused);
                // Then the link drops.
                disconnected.store(true,Ordering::Release);
                bluetooth(false);
            }
            wait("pause",&||disconnected.load(Ordering::Acquire) && player.status().state==PlayState::Paused && player.status().error.is_none());
            std::thread::sleep(Duration::from_millis(150));
            let status=player.status();
            assert_eq!(opens(),["open:q2nd-fake","open:q2bt"],"{scene}: playback moved to another output by itself");
            assert_eq!(status.state,PlayState::Paused,"{scene}");
            assert!(status.error.is_none() && status.error_code.is_none(),"{scene}: a disconnect is not an error: {:?}",status.error);
            if scene=="bluetooth-disconnect" {
                assert!(errors.lock().unwrap().is_empty(),"{scene}: an error was shown: {:?}",errors.lock().unwrap());
            }
            assert!(!drained.load(Ordering::Acquire));
            // Play continues the kept audio on the wired output.
            player.send(Cmd::PlayPause);
            wait("resume",&||drained.load(Ordering::Acquire));
            player.shutdown();
            assert_eq!(opens(),["open:q2nd-fake","open:q2bt","open:q2nd-fake"],"{scene}");
            assert_eq!(source.opened.lock().unwrap().as_slice(),[id],"{scene}");
            assert!(player.status().error.is_none(),"{scene}");
            let accepted=captured.lock().unwrap();
            let (mut at,mut steps_back,mut index)=(0,0,0);
            while index<accepted.len() {
                if at<expected.len() && accepted[index]==expected[at] {index+=1;at+=1;continue;}
                let run=64.min(accepted.len()-index);
                let earlier=(0..at).rev().find(|&start|start+run<=expected.len() && accepted[index..index+run]==expected[start..start+run]);
                at=earlier.unwrap_or_else(||panic!("{scene}: output {index} of {} is not the queued audio continuing",accepted.len()));
                steps_back+=1;
            }
            assert_eq!(at,expected.len(),"{scene}: the end of the audio was not played");
            assert!((1..=2).contains(&steps_back),"{scene}: {steps_back} replays for two route changes");
        }
    }

    struct OpeningOutputSource {
        inner:Arc<Recorder>,
        opened:mpsc::Sender<Arc<crate::audio::interrupt::Interrupt>>,
        gate:Arc<(Mutex<bool>,std::sync::Condvar)>,
    }
    impl TrackSource for OpeningOutputSource {
        fn open(&self, _blocking: &crate::rt::Blocking,song:&Song,cancel:&Arc<crate::audio::interrupt::Interrupt>)->Result<OpenedTrack,String> {
            let track=self.inner.open(_blocking,song,cancel)?;
            if song.id=="output-second" {
                let _=self.opened.send(cancel.clone());
                let (lock,cv)=&*self.gate;
                let mut released=lock.lock().unwrap();
                while !*released && !cancel.load(Ordering::Relaxed) {
                    released=cv.wait_timeout(released,Duration::from_millis(20)).unwrap().0;
                }
                if cancel.load(Ordering::Relaxed) {return Err(INTERRUPTED.into());}
            }
            Ok(track)
        }
        fn upcoming(&self,_:&[Song]) {}
        fn now_playing(&self,_:&Song) {}
        fn played(&self,_:&Song,_:u64) {}
        fn save_queue(&self,q:&[Song],index:usize,position:u64) {self.inner.save_queue(q,index,position);}
        fn spoken_skip(&self)->crate::config::SpokenSettings {self.inner.spoken_skip()}
        fn spoken_speed(&self,song:&Song)->Result<Option<u16>,String> {self.inner.spoken_speed(song)}
        fn checkpoint(&self,song:&Song,position:u64)->Result<(),String> {self.inner.checkpoint(song,position)}
    }

    fn routed_eof_trace(change:bool,hardware:bool)->Vec<i32> {
        let source=output_source(Some(125));
        let (opened,next_opening)=mpsc::channel();
        let gate=Arc::new((Mutex::new(false),std::sync::Condvar::new()));
        let generated=Arc::new(OpeningOutputSource {inner:source.clone(),opened,gate:gate.clone()});
        let (monitor,topology)=super::super::output::Monitor::fixture();
        let dac=super::super::output::UsbPlayback {identity:"generated-eof-dac".into(),card:74,device:0};
        if !hardware {topology.update(vec![dac.clone()]);}
        let captured=Arc::new(Mutex::new(Vec::new()));
        let drained=Arc::new(AtomicBool::new(false));
        let events=Arc::new(Mutex::new(Vec::new()));
        let queues:Arc<Mutex<Vec<Arc<std::sync::atomic::AtomicUsize>>>>=Default::default();
        let player=spawn_outputs(generated,Box::new(RoutingOutput {
            fault:None,on_error:None,captured:captured.clone(),drained:drained.clone(),events:events.clone(),
            queues:RoutingQueues { opened:queues.clone(), ..Default::default() },queue_limit:Some(512),hardware_volume:hardware,
        }),output_settings(),Arc::new(||{}),Some(monitor));
        let mut first=spoken_queue()[0].clone();first.id="output-first".into();
        first.spoken.as_mut().unwrap().offset_ms=0;first.spoken.as_mut().unwrap().duration_ms=2400;
        let mut second=first.clone();second.id="output-second".into();second.spoken.as_mut().unwrap().offset_ms=1200;
        player.send(Cmd::Play {songs:std::sync::Arc::new(vec![first,second]),index:0,shuffle:false});
        let pending=next_opening.recv_timeout(Duration::from_secs(3));
        let before=player.status();
        let prefix=captured.lock().unwrap().len();
        let tail=queues.lock().unwrap().first().map(|queue|queue.load(Ordering::Relaxed)).unwrap_or(0)*2;
        if change {topology.update(if hardware {vec![dac]}else{Vec::new()});}
        let cancelled=pending.as_ref().is_ok_and(|cancel|cancel.load(Ordering::Relaxed));
        *gate.0.lock().unwrap()=true;gate.1.notify_all();
        let deadline=Instant::now()+Duration::from_secs(3);
        while !drained.load(Ordering::Acquire) && Instant::now()<deadline {std::thread::sleep(Duration::from_millis(2));}
        player.shutdown();
        assert!(pending.is_ok(),"next file did not enter the generated Opening gate");
        assert!(!cancelled,"route discovery must not cancel pending Opening");
        assert!(drained.load(Ordering::Acquire));
        assert_eq!(before.index,0,"unheard EOF tail still belongs to first file");
        assert!(before.position_ms>1000 && before.position_ms<1200);
        assert_eq!(player.status().generation,before.generation);
        assert_eq!(player.status().position_ms,2400);
        assert_eq!(source.opened.lock().unwrap().as_slice(),["output-first","output-second"]);
        let samples=captured.lock().unwrap();
        if change {
            assert_eq!(tail,1024,"generated PCM models a bounded 512-frame hardware FIFO");
            let mut heard=samples[..prefix-tail].to_vec();
            if hardware {
                let gain=soft_gain(35) as f64;
                for sample in &mut heard {*sample=(*sample as f64*gain) as i32;}
            }
            heard.extend_from_slice(&samples[prefix..]);
            assert_eq!(events.lock().unwrap().iter().filter(|event|event.starts_with("open:")).count(),2);
            heard
        }else{samples.clone()}
    }

    #[test]
    fn actual_route_change_at_eof_retains_fifo_tail_and_does_not_cancel_next_opening() {
        let expected=routed_eof_trace(false,false);
        let actual=routed_eof_trace(true,false);
        assert_eq!(actual.len(),expected.len(),"EOF route change lost/duplicated queued PCM");
        assert!(actual==expected,"EOF transition must preserve continuous EQ/Sonic waveform");
    }

    #[test]
    fn route_events_cannot_open_output_while_storage_is_frozen() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let (monitor,topology)=super::super::output::Monitor::fixture();
        let mut engine=deterministic_spoken_engine(&blocking, Arc::new(Recorder::default()));
        let events=Arc::new(Mutex::new(Vec::new()));
        engine.hw=Box::new(RoutingOutput {fault:None,on_error:None,captured:Default::default(),
            drained:Arc::new(AtomicBool::new(false)),events:events.clone(),queues:Default::default(),queue_limit:None,hardware_volume:false});
        engine.route=monitor.snapshot();engine.outputs=Some(monitor);
        engine.state=PlayState::Paused;engine.handoff_frozen=true;engine.resume_output=true;
        engine.parked=Some(super::super::retained::RetainedPcm::new(vec![11,22,33,44],8000,2));
        engine.pending_output=Some(super::super::retained::RetainedPcm::new(vec![55,66],8000,2));
        topology.update(vec![super::super::output::UsbPlayback {identity:"generated-frozen-route".into(),card:75,device:0}]);
        engine.refresh_route();
        assert!(events.lock().unwrap().is_empty(),"frozen discovery must not open or power audio");
        assert_eq!(engine.parked.as_ref().unwrap().samples,[11,22,33,44]);
        assert_eq!(engine.pending_output.as_ref().unwrap().samples,[55,66]);
        engine.release_output();
        assert!(!engine.resume_output && engine.parked.is_none() && engine.pending_output.is_none(),
            "intentional storage release retires playback intent and every retained output segment");
    }

    fn commanded_codec_gain(events:&Arc<Mutex<Vec<String>>>)->f64 {
        let volume=events.lock().unwrap().iter().filter_map(|event|event.strip_prefix("codec-volume:")?.parse::<u8>().ok()).last().expect("codec control was written");
        super::super::gain::codec_gain(volume)
    }

    fn codec_usb_return_with_ramped_tail(loud:i32) {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut engine = deterministic_spoken_engine(&blocking, Arc::new(Recorder::default()));
        let (monitor, topology) = super::super::output::Monitor::fixture();
        topology.update(vec![super::super::output::UsbPlayback {
            identity: "generated-codec-return".into(), card: 77, device: 0,
        }]);
        let captured = Arc::new(Mutex::new(Vec::new()));
        let events = Arc::new(Mutex::new(Vec::new()));
        engine.hw = Box::new(RoutingOutput {
            fault: None, on_error: None, captured: captured.clone(),
            drained: Arc::new(AtomicBool::new(false)), events: events.clone(),
            queues: Default::default(), queue_limit: None, hardware_volume: true,
        });
        engine.route = monitor.snapshot(); engine.outputs = Some(monitor);
        engine.volume = 60;
        engine.apply_volume().unwrap();
        let gain = soft_gain(60);
        assert_eq!(engine.soft_volume, gain);
        // These are already rendered USB samples. Include a preceding louder
        // frame from a volume ramp: the current slider is not their provenance.
        engine.prepare_checkpoints().unwrap();
        let queued = vec![loud, -loud, 100_000_000, -100_000_000];
        let pending = vec![100_000_000, -100_000_000];
        engine.parked = Some(super::super::retained::RetainedPcm::new(queued.clone(), 8000, 2));
        engine.pending_output = Some(super::super::retained::RetainedPcm::new(pending.clone(), 8000, 2));
        engine.state = PlayState::Playing;
        topology.update(Vec::new());
        engine.refresh_route();
        assert!(engine.error.is_none(), "route return must preserve output ownership");
        assert!(engine.flush_pending_output());
        let expected: Vec<_> = queued.into_iter().chain(pending).collect();
        let codec_gain = commanded_codec_gain(&events);
        let actual = captured.lock().unwrap();
        assert_eq!(actual.len(), expected.len(), "route return lost or duplicated PCM");
        for (&sample, &expected) in actual.iter().zip(&expected) {
            let audible = (sample as f64 * codec_gain) as i32;
            assert!((i64::from(audible)-i64::from(expected)).abs() <= 2,
                "USB return applied volume twice: expected {expected}, audible {audible}");
        }
    }

    #[test]
    fn codec_usb_return_preserves_parked_and_pending_audible_level() {
        codec_usb_return_with_ramped_tail(200_000_000);
    }

    #[test]
    fn codec_usb_return_preserves_louder_ramp_tail_without_clipping() {
        codec_usb_return_with_ramped_tail(1_500_000_000);
    }

    #[test]
    fn codec_paused_volume_down_up_and_mute_keep_audible_ceiling() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut engine = deterministic_spoken_engine(&blocking,Arc::new(Recorder::default()));
        let events=Arc::new(Mutex::new(Vec::new()));
        engine.hw=Box::new(RoutingOutput {fault:None,on_error:None,captured:Default::default(),
            drained:Arc::new(AtomicBool::new(false)),events:events.clone(),queues:Default::default(),queue_limit:None,hardware_volume:true});
        engine.volume=60;engine.apply_volume().unwrap();
        engine.prepare_checkpoints().unwrap();
        engine.state=PlayState::Paused;
        engine.parked=Some(super::super::retained::RetainedPcm::at_output_gain(vec![1_000_000_000,-1_000_000_000],8000,2,engine.output_gain));
        engine.volume=40;engine.apply_volume().unwrap();
        let held=engine.parked.as_ref().unwrap();
        let quiet=held.samples[0] as f64*commanded_codec_gain(&events);
        engine.volume=80;engine.apply_volume().unwrap();
        let held=engine.parked.as_ref().unwrap();
        let raised=held.samples[0] as f64*commanded_codec_gain(&events);
        assert!((quiet-raised).abs()<1.0,"a paused volume increase must not amplify old PCM");
        engine.volume=0;engine.apply_volume().unwrap();
        engine.volume=100;engine.apply_volume().unwrap();
        assert_eq!(engine.parked.as_ref().unwrap().samples,[0,0]);
    }

    #[test]
    fn codec_positive_replaygain_keeps_low_volume_headroom() {
        use std::borrow::Cow;
        use symphonia::core::audio::{AudioBuffer, AudioBufferRef, Channels, Signal, SignalSpec};
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut engine = deterministic_spoken_engine(&blocking, Arc::new(Recorder::default()));
        let events=Arc::new(Mutex::new(Vec::new()));
        engine.hw = Box::new(RoutingOutput {
            fault: None, on_error: None, captured: Default::default(),
            drained: Arc::new(AtomicBool::new(false)), events: events.clone(),
            queues: Default::default(), queue_limit: None, hardware_volume: true,
        });
        engine.track_gain = replay_gain_factor(6.0,Some(0.5));
        engine.volume = 60; engine.apply_volume().unwrap();
        let spec = SignalSpec::new(48000, Channels::FRONT_LEFT | Channels::FRONT_RIGHT);
        let mut input = AudioBuffer::<i32>::new(4, spec);
        input.render_reserved(Some(4));
        for channel in 0..2 { input.chan_mut(channel).copy_from_slice(&[1_500_000_000, -1_500_000_000, 1_000_000_000, -1_000_000_000]); }
        let track_gain = engine.track_gain;
        let mut reference = super::super::decoder::Samples::default();
        reference.convert_eq_ramped(AudioBufferRef::S32(Cow::Borrowed(&input)), track_gain,
            soft_gain(60), &mut super::super::eq::Equalizer::new(Default::default()), &mut Default::default(), 0);
        engine.samples.convert_eq_ramped(AudioBufferRef::S32(Cow::Borrowed(&input)), track_gain,
            engine.soft_volume, &mut engine.eq, &mut engine.volume_ramp, 0);
        for (&sample, &expected) in engine.samples.samples().iter().zip(reference.samples()) {
            let audible = (sample as f64 * commanded_codec_gain(&events)) as i32;
            assert!((i64::from(audible)-i64::from(expected)).abs() <= 32,
                "codec path clipped before attenuation: expected {expected}, audible {audible}");
        }
    }

    #[test]
    fn adjacent_codec_steps_with_constant_dsp_compensation_never_pause_or_replay_pcm() {
        let blocking=crate::rt::Blocking::dedicated_thread();
        let mut engine=deterministic_spoken_engine(&blocking,Arc::new(Recorder::default()));
        let events=Arc::new(Mutex::new(Vec::new()));
        engine.hw=Box::new(RoutingOutput {fault:None,on_error:None,captured:Default::default(),
            drained:Arc::new(AtomicBool::new(false)),events:events.clone(),queues:Default::default(),queue_limit:None,hardware_volume:true});
        engine.track_gain=2.0;engine.volume=20;engine.apply_volume().unwrap();
        let compensation=engine.soft_volume;
        let calls=Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut sink=Sink::open(&blocking,FAKE,48000,2,400000).unwrap();
        sink.configure_output_gain(engine.output_gain).unwrap();
        sink.capture_fake_pcm();sink.set_fake_delay(4);
        sink.write(&blocking,&[100000000,-100000000,200000000,-200000000,100000000,-100000000,200000000,-200000000]).unwrap();
        sink.script_pcm_state(3,calls.clone()); // ALSA RUNNING; count pause attempts.
        engine.sink=Some(sink);engine.state=PlayState::Playing;
        let mut old_coefficients=Vec::new();
        for volume in (20..=70).chain((20..70).rev()) {
            engine.volume=volume;engine.apply_volume().unwrap();
            assert_eq!(engine.soft_volume,compensation,"equivalent half-dB domain at volume{volume}");
            assert_eq!(calls.load(Ordering::Relaxed),0,"volume{volume} paused live audio");
            assert_eq!(engine.sink.as_ref().unwrap().delay(&blocking),4);
            assert!(!engine.sink.as_ref().unwrap().retains_paused_pcm());
            let audible=1_000_000_000.0*f64::from(compensation)*commanded_codec_gain(&events);
            let desired=1_000_000_000.0*f64::from(soft_gain(volume));
            assert!((audible-desired).abs()<=desired*0.000001,"gain-domain arithmetic changed audible target");
            old_coefficients.push((f64::from(soft_gain(volume))/engine.output_gain) as f32);
        }
        assert!(old_coefficients.windows(2).filter(|p|p[0]!=p[1]).count()>20,
            "negative control: old float division would repeatedly pause/replay this same queue");
    }

    #[test]
    fn saturated_codec_step_ramps_future_frames_without_resetting_the_unheard_queue() {
        use std::borrow::Cow;
        use symphonia::core::audio::{AudioBuffer, AudioBufferRef, Channels, Signal, SignalSpec};
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut engine = deterministic_spoken_engine(&blocking, Arc::new(Recorder::default()));
        let events = Arc::new(Mutex::new(Vec::new()));
        engine.hw = Box::new(RoutingOutput {
            fault: None, on_error: None, captured: Default::default(), drained: Default::default(),
            events: events.clone(), queues: Default::default(), queue_limit: None, hardware_volume: true,
        });
        engine.track_gain = replay_gain_factor(15.0, Some(0.17));
        engine.volume = 80;
        engine.apply_volume().unwrap();
        assert_eq!(engine.output_gain, 1.0);
        let old = soft_gain(80);
        let new = soft_gain(74);
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut sink = Sink::open(&blocking, FAKE, 8000, 2, 400000).unwrap();
        sink.set_fake_delay(2);
        sink.write(&blocking, &[500_000_000, -500_000_000, 500_000_000, -500_000_000]).unwrap();
        sink.script_pcm_state(3, calls.clone());
        engine.sink = Some(sink);
        engine.state = PlayState::Playing;
        events.lock().unwrap().clear();

        engine.volume = 74;
        engine.apply_volume().unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert_eq!(engine.sink.as_ref().unwrap().delay(&blocking), 2);
        assert!(events.lock().unwrap().is_empty(), "verified codec level is unchanged");

        let render = |engine: &mut Engine<'_>| {
            let spec = SignalSpec::new(8000, Channels::FRONT_LEFT | Channels::FRONT_RIGHT);
            let mut input = AudioBuffer::<i32>::new(16, spec);
            input.render_reserved(Some(16));
            input.chan_mut(0).fill(200_000_000);
            input.chan_mut(1).fill(-200_000_000);
            engine.samples.convert_eq_ramped(AudioBufferRef::S32(Cow::Borrowed(&input)),
                engine.track_gain, engine.soft_volume, &mut engine.eq, &mut engine.volume_ramp, 0);
            engine.wide_output(8000, 2, 0).unwrap().unwrap()
        };
        let first = render(&mut engine);
        let second = render(&mut engine);
        let samples = first.audible().unwrap().iter().chain(second.audible().unwrap()).copied().collect::<Vec<_>>();
        let multiplier = 200_000_000.0 * f64::from(engine.track_gain);
        assert!(samples[0] < multiplier * f64::from(old));
        for stereo in samples.chunks_exact(2) { assert_eq!(stereo[0], -stereo[1]); }
        let left = samples.iter().step_by(2).copied().collect::<Vec<_>>();
        assert!(left.windows(2).all(|pair| pair[0] >= pair[1]));
        assert!((left[31] - multiplier * f64::from(new)).abs() < 1.0);

        // A pause before the old hardware queue drains must honor the newer
        // volume even if the digital ramp has already finished rendering.
        let sink = engine.sink.as_mut().unwrap();
        assert!(sink.pause(&blocking, true).unwrap());
        let held = sink.retained(&blocking).unwrap();
        let maximum = (500_000_000.0 * f64::from(new / old)).round() as i32;
        assert_eq!(held.samples, [maximum, -maximum, maximum, -maximum]);
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        engine.volume = 0;
        engine.apply_volume().unwrap();
        assert_eq!(commanded_codec_gain(&events), 0.0);
        engine.volume = 80;
        engine.apply_volume().unwrap();
        assert!(engine.sink.as_ref().unwrap().retained(&blocking).unwrap().samples.iter().all(|&sample| sample == 0));
    }

    #[test]
    fn replay_gain_peak_hint_keeps_wheel_in_codec_and_yields_to_a_measured_transient() {
        use std::borrow::Cow;
        use symphonia::core::audio::{AudioBuffer, AudioBufferRef, Channels, Signal, SignalSpec};
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut engine = deterministic_spoken_engine(&blocking, Arc::new(Recorder::default()));
        engine.prepare_checkpoints().unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        engine.hw = Box::new(RoutingOutput {
            fault: None, on_error: None, captured: Default::default(), drained: Default::default(),
            events: events.clone(), queues: Default::default(), queue_limit: None, hardware_volume: true,
        });
        let mut tagged = song("peak-bounded");
        tagged.replay_gain = Some(subsonic::ReplayGain {
            track_gain: Some(15.0), track_peak: Some(0.055), album_gain: Some(12.0),
            album_peak: Some(0.06), ..Default::default()
        });
        let mut untagged_peak = song("missing-or-damaged-peak");
        untagged_peak.replay_gain = Some(subsonic::ReplayGain {
            track_gain: Some(15.0), track_peak: Some(f32::NAN), ..Default::default()
        });
        engine.queue = Arc::new(vec![tagged, untagged_peak]);
        engine.volume = 65;
        engine.handle(Cmd::SetReplayGain(ReplayGainMode::Track));
        assert_eq!(engine.track_gain, replay_gain_factor(15.0, Some(0.055)));
        assert_eq!(engine.replay_peak_reserve, Some(0));
        assert_eq!(engine.soft_volume, 1.0);
        assert_eq!(commanded_codec_gain(&events), f64::from(soft_gain(65)));

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut sink = Sink::open(&blocking, FAKE, 8000, 2, 400000).unwrap();
        sink.configure_output_gain(engine.output_gain).unwrap();
        sink.set_fake_delay(4);
        sink.write(&blocking, &[200_000_000, -200_000_000].repeat(4)).unwrap();
        sink.script_pcm_state(3, calls.clone());
        engine.sink = Some(sink);
        engine.state = PlayState::Playing;

        for volume in [69, 72, 69, 65] {
            engine.volume = volume;
            engine.apply_volume().unwrap();
            assert_eq!(calls.load(Ordering::Relaxed), 0);
            assert_eq!(engine.soft_volume, 1.0);
            assert!(!engine.audible_volume_ramp.active());
            assert_eq!(commanded_codec_gain(&events), f64::from(soft_gain(volume)));
        }

        // A real decoded sample above its tag must be rendered at full
        // precision, despite the lower gain last sent to the codec.
        let amplitude = 750_000_000;
        let spec = SignalSpec::new(8000, Channels::FRONT_LEFT | Channels::FRONT_RIGHT);
        let mut input = AudioBuffer::<i32>::new(8, spec);
        input.render_reserved(Some(8));
        input.chan_mut(0).fill(amplitude);
        input.chan_mut(1).fill(-amplitude);
        engine.samples.convert_eq_ramped(AudioBufferRef::S32(Cow::Borrowed(&input)),
            engine.track_gain, engine.soft_volume, &mut engine.eq, &mut engine.volume_ramp, 0);
        let mut transient = engine.wide_output(8000, 2, 0).unwrap().unwrap();
        let expected = f64::from(amplitude) * f64::from(engine.track_gain) * f64::from(soft_gain(65));
        let needed = transient.required_output_gain();
        assert!(needed > engine.output_gain);
        engine.remember_decoded_headroom(needed);
        assert!(engine.replay_peak_reserve.unwrap() > 0);
        engine.ensure_sink_for(8000, 2, needed).unwrap();
        transient.rebase_output(engine.output_gain).unwrap();
        assert!((f64::from(transient.samples[0]) * engine.output_gain - expected).abs() < 0.5);
        let promoted = engine.output_gain;
        engine.apply_volume().unwrap();
        assert_eq!(engine.output_gain, promoted, "quiet packets cannot immediately give back the reserve");

        engine.sink = None;
        engine.handle(Cmd::SetReplayGain(ReplayGainMode::Album));
        assert_eq!(engine.track_gain, replay_gain_factor(12.0, Some(0.06)));
        assert_eq!(engine.replay_peak_reserve, Some(0));
        engine.index = 1;
        engine.handle(Cmd::SetReplayGain(ReplayGainMode::Track));
        assert_eq!(engine.track_gain, 1.0);
        assert_eq!(engine.replay_peak_reserve, None);
    }

    #[test]
    fn efficient_keeps_installed_peak_bounded_and_plain_loudness_across_the_slider() {
        use std::borrow::Cow;
        use symphonia::core::audio::{AudioBuffer, AudioBufferRef, Channels, Signal, SignalSpec};
        use crate::audio::{gain::{codec_gain, codec_steps}, mixer::EFFICIENT_FLOOR};
        use crate::config::HeadphonePower::Efficient;
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut engine = deterministic_spoken_engine(&blocking, Arc::new(Recorder::default()));
        let events = Arc::new(Mutex::new(Vec::new()));
        engine.hw = Box::new(RoutingOutput {
            fault: None, on_error: None, captured: Default::default(), drained: Default::default(),
            events: events.clone(), queues: Default::default(), queue_limit: None, hardware_volume: true,
        });
        engine.headphone_power = Efficient;
        engine.applied_power = Some((Efficient, Efficient));
        let mut tagged = song("peak-bounded");
        tagged.replay_gain = Some(subsonic::ReplayGain {
            track_gain: Some(15.0), track_peak: Some(0.055), ..Default::default()
        });
        engine.queue = Arc::new(vec![tagged]);
        let spec = SignalSpec::new(8000, Channels::FRONT_LEFT | Channels::FRONT_RIGHT);
        let mut input = AudioBuffer::<i32>::new(1, spec);
        input.render_reserved(Some(1));
        let source_peak: i32 = 100_000_000;
        input.chan_mut(0)[0] = source_peak;
        input.chan_mut(1)[0] = -source_peak;
        for mode in [ReplayGainMode::Off, ReplayGainMode::Track] {
            engine.handle(Cmd::SetReplayGain(mode));
            for volume in (0..=100).chain((0..100).rev()) {
                engine.volume = volume;
                engine.apply_volume().unwrap();
                let gain = f64::from(soft_gain(volume));
                let program = f64::from(source_peak) * f64::from(engine.track_gain);
                let physical = engine.applied_codec_steps.unwrap();
                if volume == 0 {
                    assert_eq!(physical, 255);
                    assert_eq!(commanded_codec_gain(&events), 0.0);
                    continue;
                }
                assert!((EFFICIENT_FLOOR..255).contains(&physical));
                // Independent installed planner, including f32 grid error
                // before the original conservative half-dB rounding.
                let installed_logical = codec_steps((gain * f64::from(engine.track_gain)).min(1.0));
                let installed_physical = installed_logical.saturating_sub(12).max(2);
                let expected = program * gain / codec_gain(installed_logical) * codec_gain(installed_physical);
                engine.samples.convert_eq_ramped(AudioBufferRef::S32(Cow::Borrowed(&input)),
                    engine.track_gain, engine.soft_volume, &mut engine.eq, &mut engine.volume_ramp, 0);
                let wide = engine.wide_output(8000, 2, 0).unwrap();
                let rendered = wide.as_ref().map_or_else(|| engine.samples.samples()[0], |pcm| pcm.samples[0]);
                let actual = f64::from(rendered) * commanded_codec_gain(&events);
                assert!((actual - expected).abs() <= expected.abs() * 0.000001 + 1.0,
                    "{mode:?} volume {volume}: expected {expected}, played {actual}");
            }
        }

        // The installed middle plateau has at least one adjacent step with
        // unchanged hardware but a different logical device gain. Exercise
        // the live register cache rather than treating either value as both.
        let expected_physical = |volume| {
            let installed = codec_steps((f64::from(soft_gain(volume)) * f64::from(engine.track_gain)).min(1.0));
            installed.saturating_sub(12).max(2) + (100 - volume) - installed
        };
        let plateau = (55..69).find(|&volume| expected_physical(volume) == expected_physical(volume + 1)).unwrap();
        let physical = expected_physical(plateau);
        engine.volume = plateau;
        engine.apply_volume().unwrap();
        assert_eq!(engine.applied_codec_steps, Some(physical));
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut sink = Sink::open(&blocking, FAKE, 8000, 2, 400000).unwrap();
        sink.configure_output_gain(engine.output_gain).unwrap();
        sink.script_pcm_state(3, calls.clone());
        sink.set_fake_delay(0);
        engine.sink = Some(sink);
        events.lock().unwrap().clear();
        let old_gain = engine.output_gain;
        engine.volume = plateau + 1;
        engine.apply_volume().unwrap();
        assert_ne!(engine.output_gain, old_gain);
        assert_eq!(engine.applied_codec_steps, Some(physical));
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert!(events.lock().unwrap().is_empty());
    }

    #[test]
    fn mixed_codec_crossing_uses_monotone_analog_then_pcm_gain_in_both_directions() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut engine = deterministic_spoken_engine(&blocking, Arc::new(Recorder::default()));
        let events = Arc::new(Mutex::new(Vec::new()));
        engine.hw = Box::new(RoutingOutput {
            fault: None, on_error: None, captured: Default::default(), drained: Default::default(),
            events: events.clone(), queues: Default::default(), queue_limit: None, hardware_volume: true,
        });
        engine.track_gain = replay_gain_factor(15.0, Some(0.17));
        engine.volume = 65;
        engine.apply_volume().unwrap();
        let codec_gain = super::super::gain::codec_gain;
        assert_eq!(engine.output_gain, codec_gain(5));
        let mut sink = Sink::open(&blocking, FAKE, 8000, 2, 400000).unwrap();
        sink.configure_output_gain(engine.output_gain).unwrap();
        sink.set_fake_delay(4);
        let packet = [250_000_000, -250_000_000].repeat(4);
        sink.write(&blocking, &packet).unwrap();
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        sink.script_pcm_state(3, calls.clone());
        engine.sink = Some(sink);
        engine.state = PlayState::Playing;

        for (old, new, new_steps) in [(65, 69, 1), (69, 72, 0), (72, 69, 1), (69, 65, 5)] {
            let old_codec = engine.output_gain;
            engine.volume = new;
            engine.apply_volume().unwrap();
            assert_eq!(commanded_codec_gain(&events), codec_gain(new_steps));
            assert_eq!(calls.load(Ordering::Relaxed), 0);
            assert_eq!(engine.sink.as_ref().unwrap().delay(&blocking), 4);
            let first = f64::from(soft_gain(old));
            let target = f64::from(soft_gain(new));
            let after_codec = first * engine.output_gain / old_codec;
            let direction = (target - first).signum();
            assert!(direction * (after_codec - first) > 0.0);
            assert!(direction * (target - after_codec) > -0.000001);
            if old == 72 || new == 72 {
                assert!(engine.audible_volume_ramp.active());
                let mut before = after_codec;
                while engine.audible_volume_ramp.active() {
                    let after = f64::from(engine.audible_volume_ramp.next());
                    assert!(direction * (after - before) >= 0.0);
                    assert!(direction * (target - after) >= 0.0);
                    before = after;
                }
                assert_eq!(before, target);
            } else {
                assert!(!engine.audible_volume_ramp.active());
            }
            let sink = engine.sink.as_mut().unwrap();
            sink.settled_running_volume();
            sink.write(&blocking, &packet).unwrap();
        }
    }

    #[test]
    fn volume_trace_distinguishes_same_domain_from_retained_headroom_transition() {
        use crate::playtrace::{Kind,Tap,Trace};
        let blocking=crate::rt::Blocking::dedicated_thread();
        let mut engine=deterministic_spoken_engine(&blocking,Arc::new(Recorder::default()));
        engine.hw=Box::new(RoutingOutput {fault:None,on_error:None,captured:Default::default(),
            drained:Arc::new(AtomicBool::new(false)),events:Default::default(),queues:Default::default(),queue_limit:None,hardware_volume:true});
        engine.track_gain=2.0;engine.volume=20;engine.apply_volume().unwrap();
        let calls=Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut sink=Sink::open(&blocking,FAKE,48000,2,400000).unwrap();
        sink.configure_output_gain(engine.output_gain).unwrap();
        sink.set_fake_delay(4); // Use the normal fake write: capture-only writes retain no tail.
        sink.write(&blocking,&[100000000,-100000000,200000000,-200000000,100000000,-100000000,200000000,-200000000]).unwrap();
        sink.script_pcm_state(3,calls.clone());
        engine.sink=Some(sink);engine.state=PlayState::Playing;
        let trace=Arc::new(Trace::new(true));engine.tap=Tap::new(trace.clone());
        assert_eq!(request_code(&Cmd::SetVolume(21)),Some(10));
        assert_eq!(trace.request(10),1);engine.handle(Cmd::SetVolume(21));
        assert!(engine.error.is_none());assert_eq!(calls.load(Ordering::Relaxed),0);
        // A live nonzero volume turn can stay in the running stream. Mute
        // must also silence the samples that are already in the PCM queue.
        assert_eq!(trace.request(10),2);engine.handle(Cmd::SetVolume(0));
        assert!(engine.error.is_none(),"{:?}",engine.error);assert!(calls.load(Ordering::Relaxed)>0);
        let events=trace.snapshot().events;
        let row=|serial,kind|events.iter().filter(|e|e.kind==kind && e.a==serial).collect::<Vec<_>>();
        assert_eq!(row(1,Kind::VolumePlan)[0].c & 18,16,"same compensation without running retarget");
        assert_eq!(row(2,Kind::VolumePlan)[0].c & 18,2,"changed compensation requires retained retarget");
        assert_eq!(row(1,Kind::VolumePhase).iter().map(|e|e.b).collect::<Vec<_>>(),
            [0x8000_0001,3,0x8000_0003,6,0x8000_0006]);
        assert_eq!(row(2,Kind::VolumePhase).iter().map(|e|e.b).collect::<Vec<_>>(),
            [0x8000_0001,2,0x8000_0002,3,0x8000_0003,6,0x8000_0006,4,0x8000_0004,5,0x8000_0005]);
        for phase in [2,4,5] {
            assert_eq!(row(2,Kind::VolumePhase).iter().find(|e|e.b==phase|0x8000_0000).unwrap().c,4);
        }
        for serial in [1,2] {assert_eq!((row(serial,Kind::VolumeEnd)[0].b,row(serial,Kind::VolumeEnd)[0].c),(0,0));}
        assert_eq!(trace.losses(),(0,0));
    }

    #[test]
    fn transport_width_changes_drain_and_parked_tail_keeps_its_own_width() {
        use super::super::pcm_format::PcmFormat;
        let blocking=crate::rt::Blocking::dedicated_thread();
        let mut engine=deterministic_spoken_engine(&blocking,Arc::new(Recorder::default()));
        let drained=Arc::new(AtomicBool::new(false));
        engine.hw=Box::new(RoutingOutput {fault:None,on_error:None,captured:Default::default(),
            drained:drained.clone(),events:Default::default(),queues:Default::default(),queue_limit:None,hardware_volume:false});
        engine.volume=60;
        engine.source_format=PcmFormat::S16;engine.ensure_sink(48000,2).unwrap();
        assert_eq!(engine.sink.as_ref().unwrap().format,PcmFormat::S16);
        engine.source_format=PcmFormat::S32;engine.ensure_sink(48000,2).unwrap();
        assert!(drained.load(Ordering::Relaxed));assert_eq!(engine.sink.as_ref().unwrap().format,PcmFormat::S32);
        engine.sink=None;engine.state=PlayState::Paused;
        engine.parked=Some(super::super::retained::RetainedPcm::new(vec![65536,-65536],48000,2).with_format(PcmFormat::S16));
        engine.resume();
        assert!(engine.error.is_none(),"{:?}",engine.error);
        assert_eq!(engine.sink.as_ref().unwrap().format,PcmFormat::S16);
        assert_eq!(engine.source_format,PcmFormat::S32,"next decoder width must not relabel the earlier tail");
    }

    #[test]
    fn codec_eq_transient_is_rendered_only_after_headroom_selection() {
        let blocking=crate::rt::Blocking::dedicated_thread();
        let mut engine=deterministic_spoken_engine(&blocking,Arc::new(Recorder::default()));
        let events=Arc::new(Mutex::new(Vec::new()));
        engine.hw=Box::new(RoutingOutput {fault:None,on_error:None,captured:Default::default(),
            drained:Arc::new(AtomicBool::new(false)),events:events.clone(),queues:Default::default(),
            queue_limit:None,hardware_volume:true});
        engine.volume=60;engine.apply_volume().unwrap();
        let mut settings=super::super::eq::Settings::default();
        settings.graphic_enabled=true;settings.bands[5]=12;
        let mut impulse=vec![0.0f32;4096];impulse[0]=1.0;
        let mut reference=super::super::eq::Equalizer::new(settings.clone());
        reference.prepare(8000,1);reference.process(&mut impulse);
        let mut pcm:Vec<i32>=impulse.iter().rev().map(|&v|if v>=0.0 {32767<<16}else{i32::MIN}).collect();
        engine.eq=super::super::eq::Equalizer::new(settings);
        engine.samples.process_spoken_output(&mut pcm,8000,1,&mut engine.eq,&mut engine.volume_ramp,1.0,engine.soft_volume);
        let mut held=engine.wide_output(8000,1,0).unwrap().unwrap();
        assert!(held.required_output_gain()>engine.output_gain,"fixture must exceed nominal EQ headroom");
        let expected=held.audible().unwrap().to_vec();
        engine.ensure_sink_for(8000,1,held.required_output_gain()).unwrap();
        held.rebase_output(engine.output_gain).unwrap();
        for (&sample,&expected) in held.samples.iter().zip(&expected) {
            assert!((f64::from(sample)*commanded_codec_gain(&events)-expected).abs()<=0.5);
        }
    }

    #[test]
    fn negative_replaygain_uses_codec_range_below_the_slider() {
        let blocking=crate::rt::Blocking::dedicated_thread();
        let mut engine=deterministic_spoken_engine(&blocking,Arc::new(Recorder::default()));
        let events=Arc::new(Mutex::new(Vec::new()));
        engine.hw=Box::new(RoutingOutput {fault:None,on_error:None,captured:Default::default(),
            drained:Arc::new(AtomicBool::new(false)),events:events.clone(),queues:Default::default(),
            queue_limit:None,hardware_volume:true});
        engine.volume=1;engine.track_gain=0.1;engine.apply_volume().unwrap();
        assert!(engine.output_gain<f64::from(soft_gain(1)));
        assert!((engine.soft_volume*engine.track_gain-1.0).abs()<0.06);
        assert!((engine.output_gain*f64::from(engine.soft_volume)-f64::from(soft_gain(1))).abs()<1e-9);
    }

    /// Heap requests of 100 decode steps in steady state, the samples they
    /// wrote, and whether the PCM's queue holds exact audible values (the
    /// codec-volume rendering) and not the decoder's integers. 20 s of a 137 Hz tone at 8 kHz, in the 1152-frame packets
    /// of the WAV reader; the fake PCM holds 512 frames.
    fn step_heap(volume:u8,hardware:bool,eq:bool,track_gain:f32)->(crate::test_alloc::Stats,Vec<i32>,bool) {
        let blocking=crate::rt::Blocking::dedicated_thread();
        let source=Arc::new(Recorder::default());
        let mut bytes=wav(20.0);
        for (frame,stereo) in bytes[44..].chunks_exact_mut(4).enumerate() {
            let signal=(12000.0*(frame as f64*std::f64::consts::TAU*137.0/8000.0).sin()) as i16;
            stereo[..2].copy_from_slice(&signal.to_le_bytes());
            stereo[2..].copy_from_slice(&(signal/2).to_le_bytes());
        }
        *source.audio.lock().unwrap()=Some(bytes);
        let captured=Arc::new(Mutex::new(Vec::with_capacity(8000*2*20)));
        let mut engine=deterministic_spoken_engine(&blocking,source);
        engine.hw=Box::new(RoutingOutput {fault:None,on_error:None,captured:captured.clone(),
            drained:Default::default(),events:Default::default(),queues:Default::default(),
            queue_limit:Some(512),hardware_volume:hardware});
        engine.handle(Cmd::Play {songs:std::sync::Arc::new(vec![song("generated-step-budget")]),index:0,shuffle:false});
        engine.handle(Cmd::SetVolume(volume));
        if eq {
            let mut settings=super::super::eq::Settings::default();
            settings.graphic_enabled=true;settings.bands=[3,2,1,0,-1,0,1,2,3,2];
            engine.handle(Cmd::SetEq(settings));
        }
        engine.open_current().unwrap();
        engine.track_gain=track_gain;engine.apply_volume().unwrap();
        // Every reused buffer reaches the packet size, and the test-only
        // copies of the written samples are sized, before counting begins.
        for _ in 0..20 {assert!(engine.step().unwrap());}
        assert_eq!(engine.hardware_volume,hardware);
        engine.sink.as_mut().unwrap().written_samples.reserve(8000*2*20);
        let before=captured.lock().unwrap().len();
        let ((),heap)=crate::test_alloc::measure(||for _ in 0..100 {assert!(engine.step().unwrap());});
        assert!(engine.error.is_none() && engine.pending_output.is_none(),"{:?}",engine.error);
        let written=captured.lock().unwrap()[before..].to_vec();
        (heap,written,engine.sink.as_ref().unwrap().retained(&blocking).is_some_and(|held|held.audible().is_some()))
    }

    #[test]
    fn codec_volume_steps_use_no_more_heap_than_unity_steps() {
        // Unity playback hands the decoder's integers to the PCM; what one
        // step allocates there (the demuxer's packet) is the floor.
        let (unity,unity_written,rendered)=step_heap(100,false,false,1.0);
        assert_eq!((unity_written.len(),rendered),(100*1152*2,false));
        let mut over=Vec::new();
        // The codec alone carries a plain volume: the integers pass through.
        for (label,volume,eq,track_gain,renders) in [("volume 60",60,false,1.0,false),
            ("volume 60 with the equalizer",60,true,1.0,true),("volume 100 with the equalizer",100,true,1.0,true),
            ("volume 60 with ReplayGain -6 dB",60,false,0.5,true),("volume 60 with ReplayGain +6 dB",60,false,2.0,true)] {
            let (heap,written,rendered)=step_heap(volume,true,eq,track_gain);
            assert_eq!((written.len(),rendered),(unity_written.len(),renders),"{label}");
            assert!(written.iter().any(|&sample|sample!=0),"{label}: silence is not the codec-volume path");
            if (heap.allocations,heap.bytes)!=(unity.allocations,unity.bytes) {over.push(format!("{label}: {heap:?}"));}
        }
        assert!(over.is_empty(),"100 unity steps ask for {unity:?}; more than that:\n{}",over.join("\n"));
    }

    #[test]
    fn hardware_volume_to_usb_at_eof_attenuates_retained_and_current_packet() {
        let expected=routed_eof_trace(false,false);
        let actual=routed_eof_trace(true,true);
        assert_eq!(actual.len(),expected.len());
        let difference=actual.iter().zip(&expected).map(|(&a,&b)|(i64::from(a)-i64::from(b)).abs()).max().unwrap();
        assert!(difference<=2,"hardware attenuation→software may differ only by integer rounding; delta={difference}");
    }

    #[test]
    fn hardware_volume_to_usb_attenuates_local_pending_after_open_failure() {
        let expected=output_fault_trace(Some(125),None,false,false);
        let source=output_source(Some(125));
        let (monitor,topology)=super::super::output::Monitor::fixture();
        let captured=Arc::new(Mutex::new(Vec::new()));
        let drained=Arc::new(AtomicBool::new(false));
        let player=spawn_outputs(source.clone(),Box::new(RoutingOutput {
            fault:Some("headphone-open"),on_error:None,captured:captured.clone(),drained:drained.clone(),
            events:Default::default(),queues:Default::default(),queue_limit:None,hardware_volume:true,
        }),output_settings(),Arc::new(||{}),Some(monitor));
        let mut book=spoken_queue()[0].clone();book.spoken.as_mut().unwrap().duration_ms=1200;
        let id=book.id.clone();
        player.send(Cmd::Play {songs:std::sync::Arc::new(vec![book]),index:0,shuffle:false});
        let paused=wait_for(&player,2,|status|status.error.is_some());
        let no_samples=captured.lock().unwrap().is_empty();
        topology.update(vec![super::super::output::UsbPlayback {identity:"generated-pending-gain".into(),card:76,device:0}]);
        let deadline=Instant::now()+Duration::from_secs(3);
        while !drained.load(Ordering::Acquire) && Instant::now()<deadline {std::thread::sleep(Duration::from_millis(2));}
        player.shutdown();
        assert_eq!(paused.state,PlayState::Paused);assert_eq!(paused.position_ms,0);
        assert!(no_samples && drained.load(Ordering::Acquire));
        assert_eq!(source.opened.lock().unwrap().as_slice(),[id]);
        let actual=captured.lock().unwrap();
        assert_eq!(actual.len(),expected.len());
        let difference=actual.iter().zip(&expected).map(|(&a,&b)|(i64::from(a)-i64::from(b)).abs()).max().unwrap();
        assert!(difference<=2,"local pending escaped USB attenuation; delta={difference}");
    }

    fn wait_for(p: &PlayerHandle, secs: u64, f: impl Fn(&Status) -> bool) -> Status {
        let end = Instant::now() + Duration::from_secs(secs);
        loop {
            let st = p.status();
            if f(&st) || Instant::now() > end {
                return st;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    struct SpokenRecorder {
        positions: Mutex<std::collections::HashMap<String, u64>>,
        fail: AtomicBool,
        read_fail: AtomicBool,
        settings: Mutex<crate::config::SpokenSettings>,
    }
    impl TrackSource for SpokenRecorder {
        fn open(&self, _blocking: &crate::rt::Blocking, _: &Song, _: &Arc<crate::audio::interrupt::Interrupt>) -> Result<OpenedTrack, String> {
            panic!("deterministic transition model does not decode")
        }
        fn upcoming(&self, _: &[Song]) {}
        fn now_playing(&self, _: &Song) {}
        fn played(&self, _: &Song, _: u64) {}
        fn save_queue(&self, _: &[Song], _: usize, _: u64) {}
        fn spoken_resume(&self, q: &[Song], i: usize, fallback_ms:u64) -> Result<(usize, u64), String> {
            if self.read_fail.load(Ordering::Relaxed) {return Err("Synthetic checkpoint read failure".into());}
            let Some(book) = q.get(i).and_then(|s| s.spoken.as_ref()) else {
                return Ok((i, fallback_ms));
            };
            Ok(crate::library::spoken::selection(
                q,
                i,
                self.positions.lock().unwrap().get(&book.id).copied().unwrap_or(book.offset_ms.saturating_add(fallback_ms)),
            ))
        }
        fn spoken_skip(&self) -> crate::config::SpokenSettings {
            *self.settings.lock().unwrap()
        }
        fn checkpoint(&self, song: &Song, position: u64) -> Result<(), String> {
            if let Some(book) = &song.spoken {
                if self.fail.load(Ordering::Relaxed) {
                    return Err("Synthetic card-full failure".into());
                }
                self.positions
                    .lock()
                    .unwrap()
                    .insert(book.id.clone(), book.offset_ms + position);
            }
            Ok(())
        }
    }
    fn deterministic_spoken_engine<'a>(blocking: &'a crate::rt::Blocking, source: Arc<dyn TrackSource>) -> Engine<'a> {
        Engine {
            blocking,
            tap: crate::playtrace::Tap::new(Arc::new(crate::playtrace::Trace::new(false))),
            visible: Arc::new(AtomicBool::new(true)),
            generation: 0,
            seek_revision: 0,
            seek_complete: false,
            scrub_until: None,
            scrub_ended: false,
            speed_percent: 100,
            speed_override: None,
            speed_request: 0,
            tempo: None,
            tempo_output: Vec::new(),
            checkpoints: super::super::checkpoints::Writer::new(source.clone()),
            listening: Default::default(),
            source,
            hw: Box::new(SoftHw(FAKE.into())),
            status: Arc::new(Mutex::new(Status::default())),
            notify: Arc::new(|| {}),
            interrupt: Arc::new(crate::audio::interrupt::Interrupt::new(false)),
            current_io:Arc::new(Mutex::new(None)),
            command_gate: Arc::new(Mutex::new(())),
            queue: Arc::default(),
            original: None,
            index: 0,
            state: PlayState::Stopped,
            sink: None,
        parked: None,
            pending_output: None,
            wide_buffers: Default::default(),
            outputs:None,
            route:super::super::output::Snapshot {generation:0,route:super::super::output::Route::Headphones},
            resume_output:false,
        pause_admission_fault:None,
            bluetooth_fault:None,
            dec: None,
            resume_at: 0,
            base_ms: 0,
            frames: 0,
            volume: 0,
            remote_volume: false,
            soft_volume: 0.0,
            output_gain: 1.0,
            hardware_volume: false,
            applied_codec_steps: None,
            source_format:Default::default(),
            applied_volume: 0.0,
            volume_ramp: Default::default(),
            audible_volume_ramp: Default::default(),
            sleep_fade: None,
            sleep_timer_deadline: None,
            track_gain: 1.0,
            replay_peak_reserve: None,
            rg: ReplayGainMode::Off,
            gapless: true,
            repeat: Repeat::Off,
            started_ms: 0,
            scrobbled: false,
                direct_listen_generation: None,
                radio_listen: Default::default(),
                radio_external_metadata: false,
                direct_music_started_ms: 0,
            failures: 0,
            descriptor_recovery: Default::default(),
            idle_since: Instant::now(),
            published: Instant::now(),
            checkpointed: Instant::now(),
            error: None,
            ux: Default::default(),
            samples: Default::default(),
            eq: super::super::eq::Equalizer::new(Default::default()),
            queue_rev: 0,
            published_rev: u64::MAX,
                published_state: PlayState::Stopped,
            handoff_frozen: false,
            // The earlier Low gain: these fixtures' codec steps stay as recorded.
            headphone_power: crate::config::HeadphonePower::Standard,
            applied_power: Some((crate::config::HeadphonePower::Standard, crate::config::HeadphonePower::Standard)),
            sink_internal: true,
        }
    }

    #[test]
    fn descriptor_failure_reopens_same_selection_at_audible_position_without_skipping() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let source = Arc::new(Recorder::default());
        *source.audio.lock().unwrap() = Some(wav(20.0));
        let mut engine = deterministic_spoken_engine(&blocking, source.clone());
        engine.queue = std::sync::Arc::new(vec![song("recover"), song("must-not-skip")]);
        engine.prepare_checkpoints().unwrap();
        engine.resume_at = 5000;
        engine.open_current().unwrap();
        engine.ensure_sink(8000, 1).unwrap();
        engine.sink.as_mut().unwrap().set_fake_delay(800);
        engine.frames = 1600;
        let position = engine.audible_cursor().1;
        let generation = engine.generation;
        engine.output_failed("write: File descriptor in bad state".into());
        assert_eq!(engine.state, PlayState::Buffering);
        assert_eq!((engine.index, engine.resume_at, engine.generation), (0, position, generation));
        assert!(engine.sink.is_none() && engine.dec.is_none() && engine.error.is_none());
        engine.open_current().unwrap();
        assert!(engine.step().unwrap());
        assert_eq!(source.opened.lock().unwrap().as_slice(), &["recover", "recover"]);
        assert_eq!(engine.index, 0);
        // Repeated faults without sustained output pause safely, never skip.
        engine.failed("Bad file descriptor".into());
        engine.failed("Bad file descriptor".into());
        assert_eq!(engine.state, PlayState::Paused);
        assert_eq!(engine.index, 0);
        assert!(engine.status.lock().unwrap().error.as_ref().unwrap().contains("Press Play"));
    }

    /// UXI-003: a stream cut by the network mid-song stops where it was heard
    /// with a transient error (the UI resumes it when the link returns); it
    /// does not skip the song. Opening failures and file errors still skip.
    #[test]
    fn network_cut_stream_pauses_at_heard_position_instead_of_skipping() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let source = Arc::new(Recorder::default());
        source.streaming.store(true, Ordering::Relaxed);
        *source.audio.lock().unwrap() = Some(wav(20.0));
        let mut engine = deterministic_spoken_engine(&blocking, source.clone());
        engine.queue = std::sync::Arc::new(vec![song("cut"), song("must-not-skip")]);
        engine.prepare_checkpoints().unwrap();
        engine.resume_at = 5000;
        engine.open_current().unwrap();
        engine.ensure_sink(8000, 1).unwrap();
        engine.sink.as_mut().unwrap().set_fake_delay(800);
        engine.frames = 1600;
        engine.state = PlayState::Playing;
        let position = engine.audible_cursor().1;
        let generation = engine.generation;
        engine.failed("read: stream: error reading a body from connection".into());
        assert_eq!(engine.state, PlayState::Paused);
        assert_eq!((engine.index, engine.resume_at, engine.generation), (0, position, generation));
        assert!(engine.dec.is_none());
        let code = engine.status.lock().unwrap().error_code;
        assert!(code.is_some_and(crate::ux::Code::transient), "{code:?}");
        // Resume reopens the same song.
        engine.open_current().unwrap();
        assert_eq!(source.opened.lock().unwrap().as_slice(), &["cut", "cut"]);
        // A failure that is not the network still skips.
        engine.state = PlayState::Playing;
        engine.failed("Can't play this file".into());
        assert_eq!(engine.index, 1);
    }

    #[test]
    fn checkpoint_owner_precedes_all_queue_admission_and_reuses_music_owner_for_books() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        for admission in 0..3 {
            let source = Arc::new(Recorder::default());
            let mut e = deterministic_spoken_engine(&blocking, source.clone());
            assert!(!e.checkpoints.is_prepared());
            e.handle(Cmd::Restore { songs: std::sync::Arc::new(vec![]), index: 0, position_ms: 0 });
            assert!(!e.checkpoints.is_prepared());
            let music = vec![song("first-music")];
            e.handle(match admission {
                0 => Cmd::Play { songs: std::sync::Arc::new(music), index: 0, shuffle: false },
                1 => Cmd::Restore { songs: std::sync::Arc::new(music), index: 0, position_ms: 123 },
                _ => Cmd::Insert { songs: music, next: false },
            });
            assert!(e.checkpoints.is_prepared());
            assert!(e.sink.is_none() && e.parked.is_none() && e.pending_output.is_none());
            let owner = e.checkpoints.owner_id();
            e.open_current().unwrap(); e.step().unwrap();
            assert!(e.sink.is_some());
            e.handle(Cmd::Play { songs: std::sync::Arc::new(spoken_queue()), index: 0, shuffle: false });
            e.open_current().unwrap(); e.step().unwrap();
            assert_eq!(e.checkpoints.owner_id(), owner);
            // The periodic path cannot be the first creation, including a
            // first book after music. Pause/replay and shutdown keep ownership.
            e.checkpointed = Instant::now() - Duration::from_secs(31);
            e.publish(true);
            e.handle(Cmd::Pause); e.handle(Cmd::PlayPause);
            assert_eq!(e.checkpoints.owner_id(), owner);
            assert!(!e.handle(Cmd::Quit));
            assert!(source.checkpointed_positions.lock().unwrap().iter().any(|(id,_)|id=="first-music"));
        }
    }

    #[test]
    fn checkpoint_preparation_covers_failed_open_and_rejects_unprepared_output() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut e = deterministic_spoken_engine(&blocking, Arc::new(Recorder::default()));
        e.handle(Cmd::Play { songs: std::sync::Arc::new(vec![song("missing")]), index: 0, shuffle: false });
        let owner = e.checkpoints.owner_id();
        assert!(owner.is_some());
        assert!(e.open_current().is_err());
        assert!(e.sink.is_none());
        e.handle(Cmd::Restore { songs: std::sync::Arc::new(spoken_queue()), index: 0, position_ms: 456 });
        assert_eq!(e.checkpoints.owner_id(), owner);
        e.handle(Cmd::Quit);

        // A future bypass may never create the thread while PCM exists.
        let mut invalid = deterministic_spoken_engine(&blocking, Arc::new(Recorder::default()));
        invalid.sink = Some(Sink::open(&blocking, FAKE, 8000, 2, LATENCY_US).unwrap());
        assert!(invalid.prepare_checkpoints().is_err());
        assert!(!invalid.checkpoints.is_prepared());
    }

    #[test]
    fn quit_during_first_queue_preparation_never_opens_media_or_starts_pcm() {
        struct Preparing {
            entered: mpsc::Sender<()>,
            release: Mutex<mpsc::Receiver<()>>,
            opens: std::sync::atomic::AtomicUsize,
            saves: std::sync::atomic::AtomicUsize,
        }
        impl TrackSource for Preparing {
            fn open(&self, _blocking: &crate::rt::Blocking, _: &Song, _: &Arc<crate::audio::interrupt::Interrupt>) -> Result<OpenedTrack,String> {
                self.opens.fetch_add(1, Ordering::Relaxed);
                Err("Queued shutdown must prevent opening".into())
            }
            fn upcoming(&self, _: &[Song]) {} fn now_playing(&self, _: &Song) {}
            fn played(&self, _: &Song, _: u64) {} fn save_queue(&self, _: &[Song], _: usize, _: u64) {}
            fn spoken_resume(&self, _: &[Song], index: usize, position: u64) -> Result<(usize,u64),String> {
                self.entered.send(()).unwrap();
                self.release.lock().unwrap().recv_timeout(Duration::from_secs(2)).unwrap();
                Ok((index,position))
            }
            fn checkpoint(&self, _: &Song, _: u64) -> Result<(),String> {
                self.saves.fetch_add(1,Ordering::Relaxed);Ok(())
            }
        }
        let (entered, begun) = mpsc::channel(); let (release, gate) = mpsc::channel();
        let source = Arc::new(Preparing { entered, release:Mutex::new(gate), opens:0.into(), saves:0.into() });
        let player = spawn(source.clone(), Box::new(SoftHw(FAKE.into())), Settings {
            eq:Default::default(),volume:0,headphone_power: crate::config::HeadphonePower::Standard,filter:0,replay_gain:ReplayGainMode::Off,gapless:true,
        }, Arc::new(||{}));
        player.send(Cmd::Play { songs:std::sync::Arc::new(spoken_queue()),index:0,shuffle:false });
        begun.recv_timeout(Duration::from_secs(2)).unwrap();
        player.send(Cmd::Quit);
        release.send(()).unwrap();
        player.shutdown();
        assert_eq!(source.opens.load(Ordering::Relaxed),0);
        assert_eq!(source.saves.load(Ordering::Relaxed),1,"accepted queue still receives its shutdown checkpoint");
    }
    #[test]
    fn parked_gapless_eof_keeps_unheard_pcm_and_next_selection() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        for percent in [100, 200] {
            let source = Arc::new(Recorder::default());
            source.default_speed.store(percent, Ordering::Relaxed);
            let mut e = deterministic_spoken_engine(&blocking, source.clone());
            let mut queue = spoken_queue()[..2].to_vec();
            for (index, song) in queue.iter_mut().enumerate() {
                song.duration = 1;
                song.spoken.as_mut().unwrap().offset_ms = index as u64 * 1200;
                song.spoken.as_mut().unwrap().duration_ms = 2400;
            }
            e.handle(Cmd::Play { songs: std::sync::Arc::new(queue), index: 0, shuffle: false });
            e.open_current().unwrap();
            while e.step().unwrap() {}
            assert!(e.sink.is_some());
            // The real decoder reached EOF with actual written PCM retained.
            // Pin its observed queue to an available 25ms tail for determinism.
            let held_frames = e.sink.as_ref().unwrap().retained(&blocking).unwrap().frames().min(200);
            assert!(held_frames > 0);
            e.sink.as_mut().unwrap().set_fake_delay(held_frames);
            let expected_samples = e.sink.as_ref().unwrap().retained(&blocking).unwrap().samples;
            e.ended();
            assert_eq!(e.index, 1);
            assert!(e.dec.is_none());
            let cursor = e.audible_cursor();
            let generation = e.generation;
            e.handle(Cmd::ParkOutput);
            assert_eq!(e.audible_cursor(), cursor);
            assert!(e.sink.is_none());
            assert_eq!(e.parked.as_ref().map(|pcm| &pcm.samples), Some(&expected_samples),
                "Output parking must keep the preceding file's unheard tail at {percent}%");
            assert_eq!(e.index, 1, "the next source remains selected behind its queued tail");
            e.handle(Cmd::Resume { generation });
            assert_eq!(e.sink.as_ref().unwrap().written_samples, expected_samples);
            e.open_current().unwrap();
            assert_eq!(&*source.opened.lock().unwrap(), &["file-0".to_string(), "file-1".to_string()]);
            assert_eq!(e.generation, generation);
            assert!(e.step().unwrap());
        }
    }

    #[test]
    fn actual_spoken_decoder_to_sink_preserves_quiet_detail_at_every_software_volume() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        struct QuietBook { bytes: Vec<u8>, percent: u16 }
        impl TrackSource for QuietBook {
            fn open(&self, _blocking: &crate::rt::Blocking, _: &Song, _: &Arc<crate::audio::interrupt::Interrupt>) -> Result<OpenedTrack,String> {
                Ok(OpenedTrack { source: Box::new(Cursor::new(self.bytes.clone())), ext: Some("wav".into()),
                    origin: Origin::Offline, radio_metadata: None })
            }
            fn upcoming(&self, _: &[Song]) {} fn now_playing(&self, _: &Song) {}
            fn played(&self, _: &Song, _: u64) {} fn save_queue(&self, _: &[Song], _: usize, _: u64) {}
            fn spoken_skip(&self) -> crate::config::SpokenSettings {
                crate::config::SpokenSettings { speed_percent: self.percent, ..Default::default() }
            }
        }
        let mut bytes = wav(2.0);
        for (frame, samples) in bytes[44..].chunks_exact_mut(4).enumerate() {
            let value = (64.0 * (frame as f64 * std::f64::consts::TAU / 32.0).sin()).round() as i16;
            samples[..2].copy_from_slice(&value.to_le_bytes());
            samples[2..].copy_from_slice(&value.to_le_bytes());
        }
        for percent in [50, 125, 150, 200, 250, 300] {
            let render = |volume| {
                let source = Arc::new(QuietBook { bytes: bytes.clone(), percent });
                let mut engine = deterministic_spoken_engine(&blocking, source);
                let mut book = spoken_queue()[0].clone(); book.spoken.as_mut().unwrap().duration_ms=2000;
                engine.handle(Cmd::Play { songs: std::sync::Arc::new(vec![book]), index:0, shuffle:false });
                engine.handle(Cmd::SetVolume(volume));
                engine.open_current().unwrap();
                engine.ensure_sink(8000,2).unwrap();
                engine.sink.as_mut().unwrap().capture_fake_pcm();
                while engine.step().unwrap() {}
                engine.sink.as_ref().unwrap().captured_fake_pcm().to_vec()
            };
            let reference = render(100);
            for volume in [0, 1, 20, 50, 80] {
                let actual = render(volume);
                let gain = soft_gain(volume) as f64;
                let expected: Vec<_> = reference.iter().map(|&sample|(sample as f64 * gain) as i32).collect();
                assert!(actual == expected, "quiet detail/gain depends on volume: speed{percent} volume{volume}");
            }
        }
    }

    #[test]
    fn spoken_pause_preserves_pcm_and_seek_speed_restarts_match_fresh_decoder() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        struct SignalBook { bytes: Vec<u8>, speed: std::sync::atomic::AtomicU16 }
        impl TrackSource for SignalBook {
            fn open(&self, _blocking: &crate::rt::Blocking, _: &Song, _: &Arc<crate::audio::interrupt::Interrupt>) -> Result<OpenedTrack,String> {
                Ok(OpenedTrack { source: Box::new(Cursor::new(self.bytes.clone())), ext: Some("wav".into()),
                    origin: Origin::Offline, radio_metadata: None })
            }
            fn upcoming(&self, _: &[Song]) {} fn now_playing(&self, _: &Song) {}
            fn played(&self, _: &Song, _: u64) {} fn save_queue(&self, _: &[Song], _: usize, _: u64) {}
            fn spoken_speed(&self, _: &Song) -> Result<Option<u16>,String> {
                Ok(Some(self.speed.load(Ordering::Relaxed)))
            }
            fn set_spoken_speed(&self, _: &Song, speed: Option<u16>) -> Result<(),String> {
                self.speed.store(speed.unwrap_or(100), Ordering::Relaxed); Ok(())
            }
        }
        let mut bytes = wav(3.0);
        for (frame, samples) in bytes[44..].chunks_exact_mut(4).enumerate() {
            let phase = frame as f64 * std::f64::consts::TAU * 137.0 / 8000.0;
            let value = (12000.0 * phase.sin() + 4000.0 * (2.0*phase).sin()) as i16;
            samples[..2].copy_from_slice(&value.to_le_bytes());
            samples[2..].copy_from_slice(&(value/2).to_le_bytes());
        }
        let mut rows = Vec::new();
        for percent in [50, 125, 150, 200, 250, 300] {
            for action in ["pause", "seek", "speed"] {
                let source = Arc::new(SignalBook { bytes:bytes.clone(), speed:std::sync::atomic::AtomicU16::new(percent) });
                let mut book = spoken_queue()[0].clone(); book.spoken.as_mut().unwrap().duration_ms=3000;
                let mut engine = deterministic_spoken_engine(&blocking, source.clone());
                engine.handle(Cmd::Play { songs:std::sync::Arc::new(vec![book.clone()]),index:0,shuffle:false });
                engine.handle(Cmd::SetVolume(100)); engine.open_current().unwrap(); engine.ensure_sink(8000,2).unwrap();
                engine.sink.as_mut().unwrap().capture_fake_pcm();
                while engine.frames < 2000 { assert!(engine.step().unwrap()); }
                let before = engine.sink.as_ref().unwrap().captured_fake_pcm().to_vec();
                let target = if action == "seek" { 777 } else { engine.logical_position_ms() };
                match action {
                    "pause" => { engine.pause(); engine.resume(); },
                    "seek" => engine.seek_spoken_absolute(target),
                    "speed" => { engine.handle(Cmd::SetSpeed { generation:engine.generation,
                        percent:Some(if percent==250 {150}else{250}),request:1 }); },
                    _ => unreachable!(),
                }
                assert_eq!(engine.logical_position_ms(),target);
                if engine.dec.is_none() { engine.open_current().unwrap(); }
                while engine.step().unwrap() {}
                let after = &engine.sink.as_ref().unwrap().captured_fake_pcm()[before.len()..];
                let mut fresh = deterministic_spoken_engine(&blocking, source);
                fresh.handle(Cmd::Restore { songs:std::sync::Arc::new(vec![book]),index:0,position_ms:if action == "pause" {0}else{target} });
                fresh.handle(Cmd::SetVolume(100)); fresh.resume(); fresh.open_current().unwrap();
                fresh.ensure_sink(8000,2).unwrap(); fresh.sink.as_mut().unwrap().capture_fake_pcm();
                while fresh.step().unwrap() {}
                let reference = fresh.sink.as_ref().unwrap().captured_fake_pcm();
                let reference = if action == "pause" { &reference[before.len()..] } else { reference };
                assert!(after == reference,
                    "PCM differs after {action} at speed{percent}: {} vs {} samples", after.len(), reference.len());
                // Pause retains the decoder/DSP and must match uninterrupted
                // output exactly. Seek/speed deliberately reopen at media time.
                // Their fresh-state equality proves reset ownership, not a
                // continuous join or perceptual quality.
                let step = (after[0] as f64-before[before.len()-2] as f64).abs()/2147483648.0;
                let typical_max = before.chunks_exact(2).map(|f|f[0]).collect::<Vec<_>>()
                    .windows(2).map(|pair|(pair[1] as f64-pair[0] as f64).abs()/2147483648.0).fold(0.0,f64::max);
                rows.push(serde_json::json!({"old_speed":percent,"new_speed":engine.speed_percent,
                    "action":action,"media_ms":target,"join_step_full_scale":step,
                    "preceding_max_step_full_scale":typical_max,"reference_pcm_equal":true,
                    "reference":if action=="pause" {"uninterrupted"}else{"fresh media-time restart"}}));
            }
        }
        let root=crate::library::generated_test_directory("spoken-transition-characterization");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("transition-characterization.json"),serde_json::to_vec_pretty(&rows).unwrap()).unwrap();
    }

    #[test]
    fn sleep_timer_uses_real_decoder_and_retains_queued_pcm_across_restart() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut bytes = wav(4.0);
        for frame in bytes[44..].chunks_exact_mut(4) {
            frame[..2].copy_from_slice(&12000i16.to_le_bytes());
            frame[2..].copy_from_slice(&12000i16.to_le_bytes());
        }
        let source = Arc::new(Recorder::default());
        *source.audio.lock().unwrap() = Some(bytes);
        let captured = Arc::new(Mutex::new(Vec::new()));
        let queues = Arc::new(Mutex::new(Vec::new()));
        let mut engine = deterministic_spoken_engine(&blocking, source);
        engine.hw = Box::new(RoutingOutput { fault:None, on_error:None, captured:captured.clone(),
            drained:Default::default(), events:Default::default(), queues:RoutingQueues { opened:queues.clone(), ..Default::default() },
            queue_limit:None, hardware_volume:false });
        engine.handle(Cmd::Play { songs: std::sync::Arc::new(vec![song("generated-timer")]), index:0, shuffle:false });
        engine.handle(Cmd::SetVolume(100));
        engine.open_current().unwrap();
        assert!(engine.step().unwrap());
        let baseline = captured.lock().unwrap().clone();
        assert!(!baseline.is_empty() && baseline.iter().all(|v| *v > 0));
        let deadline = Instant::now() + Duration::from_secs(5);
        engine.handle(Cmd::SetSleepTimer { deadline:Some(deadline) });
        engine.handle(Cmd::StartSleepFade { deadline });
        assert!(engine.step().unwrap());
        let faded = captured.lock().unwrap().clone();
        assert!(faded.len() > baseline.len());
        assert!(i64::from(faded[baseline.len()]) < i64::from(baseline[0]) * 3 / 5);
        assert!(faded.last().unwrap() < &faded[baseline.len()]);
        let before = engine.sink.as_ref().unwrap().retained(&blocking).unwrap().samples;
        let restarted = deadline + Duration::from_secs(1800);
        engine.handle(Cmd::SetSleepTimer { deadline:Some(restarted) });
        assert_eq!(engine.sink.as_ref().unwrap().retained(&blocking).unwrap().samples, before,
            "restart must not discard, duplicate, or amplify queued PCM");
        let begin = faded.len();
        assert!(engine.step().unwrap());
        let after = captured.lock().unwrap().clone();
        assert!(after.len() > begin);
        assert!(after.last().unwrap() > &after[begin], "future PCM must ramp back up");
        assert_eq!(engine.state, PlayState::Playing);
        assert_eq!(engine.volume, 100);
        assert!(!queues.lock().unwrap().is_empty());
    }

    #[test]
    fn stale_sleep_commands_cannot_change_a_new_timer_or_playback() {
        let blocking=crate::rt::Blocking::dedicated_thread();
        let mut engine=deterministic_spoken_engine(&blocking, Arc::new(Recorder::default()));
        engine.queue=std::sync::Arc::new(spoken_queue()); engine.state=PlayState::Playing; engine.generation=8; engine.base_ms=30_000;
        let old=Instant::now()+Duration::from_secs(5); let current=old+Duration::from_secs(30);
        engine.handle(Cmd::SetSleepTimer {deadline:Some(old)});
        engine.handle(Cmd::StartSleepFade {deadline:old});
        engine.handle(Cmd::SetSleepTimer {deadline:Some(current)});
        engine.handle(Cmd::StartSleepFade {deadline:current});
        engine.handle(Cmd::FinishSleepTimer {deadline:old,generation:8,rewind_ms:15_000});
        assert!(matches!(engine.sleep_fade,Some(SleepFade::Falling {deadline}) if deadline==current));
        assert_eq!(engine.state,PlayState::Playing); assert_eq!(engine.logical_position_ms(),30_000);
        engine.handle(Cmd::StartSleepFade {deadline:old});
        assert!(matches!(engine.sleep_fade,Some(SleepFade::Falling {deadline}) if deadline==current));
        engine.handle(Cmd::FinishSleepTimer {deadline:current,generation:7,rewind_ms:15_000});
        assert!(matches!(engine.sleep_fade,Some(SleepFade::Restoring {..})));
        assert_eq!(engine.state,PlayState::Playing); assert_eq!(engine.logical_position_ms(),30_000);
    }

    #[test]
    fn timer_expiry_rewinds_only_the_active_matching_selection() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        for (state, generation, expected) in [(PlayState::Playing, 4, 15000), (PlayState::Paused, 4, 30000), (PlayState::Playing, 3, 30000)] {
            let source = Arc::new(SpokenRecorder { positions: Default::default(), fail: AtomicBool::new(false), read_fail: AtomicBool::new(false), settings: Default::default() });
            let mut engine = deterministic_spoken_engine(&blocking, source);
            engine.prepare_checkpoints().unwrap();
            engine.queue = std::sync::Arc::new(spoken_queue());
            engine.state = state;
            engine.generation = 4;
            engine.base_ms = 30000;
            let deadline = Instant::now();
            engine.handle(Cmd::SetSleepTimer { deadline: Some(deadline) });
            engine.handle(Cmd::FinishSleepTimer { deadline, generation, rewind_ms: 15000 });
            assert_eq!(engine.logical_position_ms(), expected);
            if generation == 4 { assert_eq!(engine.state, PlayState::Paused); }
        }
    }

    #[test]
    fn seek_in_boundary_window_discards_previous_file_tail_before_decoder_reopen() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        for percent in [100, 200] {
            let mut engine = deterministic_spoken_engine(&blocking, Arc::new(Recorder::default()));
            engine.queue = std::sync::Arc::new(spoken_queue()[..2].to_vec());
            Arc::make_mut(&mut engine.queue)[0].spoken.as_mut().unwrap().offset_ms = 0;
            Arc::make_mut(&mut engine.queue)[1].spoken.as_mut().unwrap().offset_ms = 1200;
            engine.index = 1;
            engine.state = PlayState::Buffering;
            engine.speed_percent = percent;
            engine.ensure_sink(8000, 2).unwrap();
            engine.sink.as_mut().unwrap().set_fake_delay(1600);
            assert!(engine.logical_position_ms() < 1200);
            engine.seek_spoken_absolute(1300);
            assert_eq!(engine.sink.as_ref().unwrap().delay(&blocking), 0);
            assert_eq!((engine.index, engine.resume_at, engine.base_ms), (1, 100, 100));
            assert_eq!(engine.logical_position_ms(), 1300);
        }
    }

    #[test]
    fn queued_previous_file_tail_keeps_audible_checkpoint_pause_release_and_speed_seek_exact() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        for percent in [100, 200] {
            for action in ["pause", "release", "speed", "handoff"] {
                let source = Arc::new(Recorder::default());
                source.default_speed.store(percent, Ordering::Relaxed);
                let mut engine = deterministic_spoken_engine(&blocking, source.clone());
                let mut queue = spoken_queue()[..2].to_vec();
                for (index, song) in queue.iter_mut().enumerate() {
                    song.duration = 1;
                    song.spoken.as_mut().unwrap().offset_ms = index as u64 * 1200;
                    song.spoken.as_mut().unwrap().duration_ms = 2400;
                }
                engine.handle(Cmd::Play { songs: std::sync::Arc::new(queue), index: 0, shuffle: false });
                engine.open_current().unwrap();
                engine.ensure_sink(8000, 2).unwrap();
                engine.frames = 9600 * 100 / u64::from(percent);
                engine.sink.as_mut().unwrap().set_fake_delay(1600);
                // 200 ms of output tail still belongs to the preceding file.
                let expected = 1200 - 200 * u64::from(percent) / 100;
                assert_eq!(engine.logical_position_ms(), expected);
                engine.ended();
                assert_eq!(engine.index, 1);
                assert!(engine.dec.is_none());
                assert_eq!(engine.logical_position_ms(), expected);
                assert_eq!(engine.audible_cursor(), (0, expected));
                assert!(engine.checkpoint_current());
                assert_eq!(source.checkpointed_positions.lock().unwrap().last(), Some(&(engine.queue[0].id.clone(), expected)));
                engine.publish(true);
                assert_eq!(engine.status.lock().unwrap().position_ms, expected);
                match action {
                    "pause" => engine.pause(),
                    "release" => engine.release_output(),
                    "speed" => { engine.handle(Cmd::SetSpeed { generation: engine.generation, percent: Some(175), request: 1 }); },
                    "handoff" => {
                        let (tx, rx) = mpsc::channel(); engine.handle(Cmd::PrepareHandoff(tx.into()));
                        assert_eq!(rx.recv_timeout(Duration::from_secs(1)).unwrap(), Ok(()));
                    }
                    _ => unreachable!(),
                }
                assert_eq!(engine.index, 0, "{percent}% {action}");
                assert_eq!((engine.base_ms, engine.resume_at), (expected, expected), "{percent}% {action}");
                assert_eq!(engine.logical_position_ms(), expected);
                assert!(engine.save());
                assert_eq!(source.queue_positions.lock().unwrap().last(), Some(&(0, expected)));
                assert_eq!(source.checkpointed_positions.lock().unwrap().last(), Some(&(engine.queue[0].id.clone(), expected)));
            }
        }
    }

    #[test]
    fn sleep_at_real_gapless_eof_retains_unheard_tail_without_reopening_previous_file() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        for percent in [100, 200] {
            let source = Arc::new(Recorder::default());
            source.default_speed.store(percent, Ordering::Relaxed);
            let mut e = deterministic_spoken_engine(&blocking, source.clone());
            let mut queue = spoken_queue()[..2].to_vec();
            for (index, song) in queue.iter_mut().enumerate() {
                song.duration = 1;
                song.spoken.as_mut().unwrap().offset_ms = index as u64 * 1200;
                song.spoken.as_mut().unwrap().duration_ms = 2400;
            }
            e.handle(Cmd::Play { songs: std::sync::Arc::new(queue), index: 0, shuffle: false });
            e.open_current().unwrap();
            while e.step().unwrap() {}
            assert!(e.sink.is_some());
            // The real decoder reached EOF with actual written PCM retained.
            // Pin its observed queue to an available 25ms tail for determinism.
            let held_frames = e.sink.as_ref().unwrap().retained(&blocking).unwrap().frames().min(200);
            assert!(held_frames > 0);
            e.sink.as_mut().unwrap().set_fake_delay(held_frames);
            let expected_samples = e.sink.as_ref().unwrap().retained(&blocking).unwrap().samples;
            e.ended();
            assert_eq!(e.index, 1);
            assert!(e.dec.is_none());
            let cursor = e.audible_cursor();
            let generation = e.generation;
            let (ack, rx) = mpsc::channel();
            e.handle(Cmd::PrepareSleep(ack.into()));
            assert_eq!(rx.recv().unwrap(), Ok(()));
            assert_eq!(e.audible_cursor(), cursor);
            assert!(e.sink.is_none());
            assert_eq!(e.parked.as_ref().map(|pcm| &pcm.samples), Some(&expected_samples),
                "Sleep must keep the preceding file's unheard tail at {percent}%");
            assert_eq!(e.index, 1, "the next source remains selected behind its queued tail");
            e.handle(Cmd::CancelHandoff);
            e.handle(Cmd::Resume { generation });
            assert_eq!(e.sink.as_ref().unwrap().written_samples, expected_samples);
            e.open_current().unwrap();
            assert_eq!(&*source.opened.lock().unwrap(), &["file-0".to_string(), "file-1".to_string()]);
            assert_eq!(e.generation, generation);
            assert!(e.step().unwrap());
        }
    }

    #[test]
    fn spoken_speed_and_resume_commands_leave_unrelated_io_uninterrupted() {
        let (tx, _rx) = mpsc::channel();
        let status = Arc::new(Mutex::new(Status::default()));
        let interrupt = Arc::new(crate::audio::interrupt::Interrupt::new(false));
        let handle = PlayerHandle { tx:Arc::new(tx), current_io:Arc::new(Mutex::new(None)), command_gate: Arc::new(Mutex::new(())), interrupt: interrupt.clone(),
            status: status.clone(), visible: Arc::new(AtomicBool::new(true)), worker: Arc::new(Mutex::new(None)),
            handoff_pending: Arc::new(AtomicBool::new(false)), handoff_revision: Arc::new(AtomicU32::new(0)), trace: Arc::new(crate::playtrace::Trace::new(false)) };
        let opening = Arc::new(crate::audio::interrupt::Interrupt::new(false));
        *handle.current_io.lock().unwrap() = Some(IoSession { cancel:opening.clone(), ready:false });
        for generation in [0, 42, u64::MAX] {
            handle.send(Cmd::Resume { generation });
            assert!(!interrupt.load(Ordering::Relaxed), "resume cannot interrupt another active owner");
            assert!(!opening.load(Ordering::Relaxed), "stale resume cannot cancel a newly opening selection");
        }
        *handle.current_io.lock().unwrap() = None;
        for mut current in [song("music"), song("radio")] {
            if current.id == "radio" { current.radio_url = Some("https://generated.invalid/radio".into()); }
            status.lock().unwrap().queue = Arc::new(vec![current]);
            handle.send(Cmd::RefreshSpeed);
            handle.send(Cmd::SetSpeed { generation: 0, percent: Some(175), request: 1 });
            assert!(!interrupt.load(Ordering::Relaxed));
        }
        status.lock().unwrap().queue = Arc::new(vec![spoken_queue()[0].clone()]);
        handle.send(Cmd::SetSpeed { generation: 1, percent: Some(175), request: 2 });
        assert!(!interrupt.load(Ordering::Relaxed));
        handle.send(Cmd::SetSpeed { generation: 0, percent: Some(175), request: 3 });
        assert!(interrupt.load(Ordering::Relaxed));
    }

    #[test]
    fn default_custom_series_reset_and_storage_errors_resolve_without_losing_target_identity() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let source = Arc::new(Recorder::default());
        source.default_speed.store(125, Ordering::Relaxed);
        let mut engine = deterministic_spoken_engine(&blocking, source.clone());
        engine.handle(Cmd::Restore { songs: std::sync::Arc::new(vec![spoken_queue()[0].clone()]), index: 0, position_ms: 250 });
        assert_eq!((engine.speed_percent, engine.speed_override), (125, None));
        engine.handle(Cmd::SetSpeed { generation: engine.generation, percent: Some(175), request: 1 });
        source.default_speed.store(200, Ordering::Relaxed);
        let revision = engine.seek_revision;
        engine.handle(Cmd::RefreshSpeed);
        assert_eq!((engine.speed_percent, engine.speed_override), (175, Some(175)));
        assert_eq!(engine.seek_revision, revision);
        engine.handle(Cmd::SetSpeed { generation: engine.generation, percent: None, request: 2 });
        assert_eq!((engine.speed_percent, engine.speed_override), (200, None));
        let mut podcast = spoken_queue()[0].clone();
        podcast.spoken.as_mut().unwrap().source = subsonic::SpokenSource::Audiobookshelf {
            namespace: "generated-account".into(), item: "series-parent".into(), episode: Some("episode-a".into()), content_url: "a.mp3".into(),
        };
        engine.handle(Cmd::Restore { songs: std::sync::Arc::new(vec![podcast.clone()]), index: 0, position_ms: 300 });
        let old_generation = engine.generation;
        engine.handle(Cmd::SetSpeed { generation: engine.generation, percent: Some(250), request: 3 });
        if let subsonic::SpokenSource::Audiobookshelf { episode, .. } = &mut podcast.spoken.as_mut().unwrap().source { *episode = Some("episode-b".into()); }
        engine.handle(Cmd::Restore { songs: std::sync::Arc::new(vec![podcast]), index: 0, position_ms: 400 });
        assert_eq!((engine.speed_percent, engine.speed_override), (250, Some(250)));
        engine.handle(Cmd::SetSpeed { generation: old_generation, percent: None, request: 4 });
        assert_eq!((engine.speed_percent, engine.speed_request), (250, 3));
        source.speed_write_fail.store(true, Ordering::Relaxed);
        engine.handle(Cmd::SetSpeed { generation: engine.generation, percent: None, request: 5 });
        assert_eq!((engine.speed_percent, engine.speed_override, engine.speed_request), (250, Some(250), 5));
        assert!(engine.error.as_ref().unwrap().contains("write failure"));
        source.speed_write_fail.store(false, Ordering::Relaxed);
        engine.handle(Cmd::SetSpeed { generation: engine.generation, percent: None, request: 6 });
        assert_eq!((engine.speed_percent, engine.speed_override), (200, None));
        assert!(engine.error.is_none());
        source.speed_read_fail.store(true, Ordering::Relaxed);
        engine.handle(Cmd::RefreshSpeed);
        assert_eq!(engine.state, PlayState::Paused);
        assert!(engine.error.as_ref().unwrap().contains("read failure"));
        assert!(engine.open_current().unwrap_err().contains("read failure"));
        assert_eq!(engine.speed_percent, 200);
    }

    #[test]
    fn wheel_bursts_coalesce_durable_speed_writes_without_crossing_barriers_or_generations() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let source = Arc::new(Recorder::default());
        let mut engine = deterministic_spoken_engine(&blocking, source.clone());
        engine.handle(Cmd::Restore { songs: std::sync::Arc::new(vec![spoken_queue()[0].clone()]), index: 0, position_ms: 250 });
        let generation = engine.generation;
        let (tx, rx) = mpsc::channel();
        for (request, percent) in (50..=300).step_by(5).enumerate() {
            tx.send(Cmd::SetSpeed { generation, percent: Some(percent), request: request as u64 }).unwrap();
        }
        assert!(engine.commands(&rx, None));
        assert_eq!(source.speed_writes.load(Ordering::Relaxed), 1);
        assert_eq!(engine.speed_percent, 300);
        assert_eq!(engine.speed_request, 50);
        assert_eq!(engine.generation, generation);
        assert_eq!(engine.position_ms(), 250);
        tx.send(Cmd::SetSpeed { generation, percent: Some(150), request: 51 }).unwrap();
        tx.send(Cmd::Pause).unwrap(); // A durability/position barrier must retain ordering.
        tx.send(Cmd::SetSpeed { generation, percent: None, request: 52 }).unwrap();
        tx.send(Cmd::SetSpeed { generation: generation + 1, percent: Some(200), request: 53 }).unwrap();
        assert!(engine.commands(&rx, None));
        assert_eq!(source.speed_writes.load(Ordering::Relaxed), 3);
        assert_eq!((engine.speed_percent, engine.speed_override, engine.speed_request), (100, None, 52));
    }

    #[test]
    fn speed_changes_keep_media_position_and_survive_seek_pause_release_and_reopen() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut engine = deterministic_spoken_engine(&blocking, Arc::new(Recorder::default()));
        engine.handle(Cmd::Play { songs: std::sync::Arc::new(vec![spoken_queue()[0].clone()]), index: 0, shuffle: false });
        engine.open_current().unwrap();
        // No sink delay: exact generated media/output frame accounting.
        engine.frames = 4000;
        assert_eq!(engine.position_ms(), 500);
        engine.handle(Cmd::SetSpeed { generation: engine.generation, percent: Some(175), request: 1 });
        assert_eq!((engine.position_ms(), engine.speed_percent), (500, 175));
        assert!(engine.tempo.is_none());
        engine.frames = 1600;
        assert_eq!(engine.position_ms(), 850);
        engine.frames = 0;
        engine.handle(Cmd::SetSpeed { generation: engine.generation, percent: Some(176), request: 1 });
        assert_eq!(engine.speed_percent, 175);
        engine.seek(250);
        assert_eq!((engine.position_ms(), engine.speed_percent), (250, 175));
        engine.pause();
        engine.close_track();
        engine.resume_at = 250;
        engine.open_current().unwrap();
        // The source still reports default 100: a live command cannot be
        // undone by delayed config publication during decoder reopening.
        assert_eq!(engine.speed_percent, 175);
        assert!(engine.step().unwrap());
        assert!(engine.tempo.is_some());
        engine.release_output();
        assert!(engine.tempo.is_none() && engine.dec.is_none());
        engine.open_current().unwrap();
        assert_eq!(engine.speed_percent, 175);
        engine.handle(Cmd::SetSpeed { generation: engine.generation, percent: Some(100), request: 1 });
        assert!(engine.step().unwrap());
        assert!(engine.tempo.is_none());
        assert_eq!(engine.status.lock().unwrap().speed_percent, 100);
        engine.handle(Cmd::Play { songs: std::sync::Arc::new(vec![song("music-after-book")]), index: 0, shuffle: false });
        engine.handle(Cmd::SetSpeed { generation: engine.generation, percent: Some(300), request: 1 });
        assert_eq!(engine.speed_percent, 100);
        engine.open_current().unwrap();
        assert!(engine.step().unwrap());
        assert!(engine.tempo.is_none());
        assert_eq!(engine.status.lock().unwrap().speed_percent, 100);
    }

    #[test]
    fn decoder_eof_drains_tempo_once_and_saves_full_book_position() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut engine = deterministic_spoken_engine(&blocking, Arc::new(Recorder::default()));
        let mut book = spoken_queue()[0].clone();
        book.duration = 1;
        book.spoken.as_mut().unwrap().duration_ms = 1200;
        engine.handle(Cmd::Play { songs: std::sync::Arc::new(vec![book]), index: 0, shuffle: false });
        engine.handle(Cmd::SetSpeed { generation: engine.generation, percent: Some(300), request: 1 });
        engine.open_current().unwrap();
        while engine.step().unwrap() {}
        // 1.2 seconds of generated stereo PCM at 3x; tolerance remains
        // under one Sonic pitch-analysis window, with no missing drain tail.
        assert!((engine.frames as i64 - 3200).abs() < 160);
        assert!(engine.tempo.is_none());
        let frames = engine.frames;
        assert!(!engine.step().unwrap());
        assert_eq!(engine.frames, frames);
        engine.ended();
        assert_eq!(engine.state, PlayState::Stopped);
        assert_eq!(engine.status.lock().unwrap().position_ms, 1200);
    }

    #[test]
    fn decoded_rate_corrects_the_published_container_format_label() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut engine = deterministic_spoken_engine(&blocking, Arc::new(Recorder::default()));
        engine.queue = std::sync::Arc::new(vec![song("generated-format-hint")]);
        engine.open_current().unwrap();
        let decoded = engine.dec.as_mut().unwrap();
        decoded.rate = 48000;
        decoded.label = "PCM 16/48".into();
        assert!(engine.step().unwrap());
        engine.publish(true);
        let status = engine.status.lock().unwrap();
        assert_eq!(status.sample_rate, Some(8000));
        assert_eq!(status.format.as_deref(), Some("PCM 16/8"));
        assert_eq!(engine.sink.as_ref().unwrap().rate, 8000);
    }

    #[test]
    fn boot_restore_uses_durable_bookmark_over_stale_queue_and_keeps_legacy_fallback() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let source=Arc::new(SpokenRecorder {positions:Mutex::new(std::collections::HashMap::from([("generated-book".into(),102345)])),
            fail:AtomicBool::new(false),read_fail:AtomicBool::new(false),settings:Mutex::new(Default::default())});
        let mut engine=deterministic_spoken_engine(&blocking, source.clone());
        engine.handle(Cmd::Restore {songs:std::sync::Arc::new(spoken_queue()),index:0,position_ms:100});
        assert_eq!(engine.index,1);
        assert_eq!(engine.resume_at,2345);
        assert_eq!(engine.state,PlayState::Paused);
        assert_eq!(engine.status.lock().unwrap().position_ms,102345);
        let empty=Arc::new(SpokenRecorder {positions:Default::default(),fail:AtomicBool::new(false),
            read_fail:AtomicBool::new(false),settings:Mutex::new(Default::default())});
        let mut legacy=deterministic_spoken_engine(&blocking, empty);
        legacy.handle(Cmd::Restore {songs:std::sync::Arc::new(spoken_queue()),index:1,position_ms:7000});
        assert_eq!((legacy.index,legacy.resume_at),(1,7000));
    }

    #[test]
    fn refreshed_resume_crosses_files_without_resaving_old_cursor_and_rejects_stale_reply() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let source = Arc::new(SpokenRecorder { positions: Default::default(), fail: AtomicBool::new(false),
            read_fail: AtomicBool::new(false), settings: Mutex::new(Default::default()) });
        let mut engine = deterministic_spoken_engine(&blocking, source.clone());
        engine.handle(Cmd::Restore { songs: std::sync::Arc::new(spoken_queue()), index: 0, position_ms: 700 });
        let generation = engine.generation;
        let seek_revision = engine.seek_revision;
        source.positions.lock().unwrap().insert("generated-book".into(), 102345);
        engine.handle(Cmd::ResumeSpoken { generation: generation+1, seek_revision });
        assert_eq!(engine.state, PlayState::Paused);
        engine.handle(Cmd::ResumeSpoken { generation, seek_revision: seek_revision+1 });
        assert_eq!(engine.state, PlayState::Paused);
        engine.handle(Cmd::ResumeSpoken { generation, seek_revision });
        assert_eq!((engine.index, engine.resume_at), (1, 2345));
        assert_eq!(source.positions.lock().unwrap()["generated-book"], 102345);
        assert_eq!(engine.state, PlayState::Buffering);
    }

    #[test]
    fn unreadable_incoming_bookmark_retains_and_pauses_the_old_queue() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let source=Arc::new(SpokenRecorder {positions:Default::default(),fail:AtomicBool::new(false),
            read_fail:AtomicBool::new(false),settings:Mutex::new(Default::default())});
        let mut engine=deterministic_spoken_engine(&blocking, source.clone());
        let old=spoken_queue();
        engine.handle(Cmd::Restore {songs:std::sync::Arc::new(old.clone()),index:0,position_ms:700});
        source.read_fail.store(true,Ordering::Relaxed);
        engine.state=PlayState::Playing;
        let revision=engine.queue_rev;
        let mut other=old.clone();
        for song in &mut other {song.id.push_str("-other");song.spoken.as_mut().unwrap().id="other-generated-book".into();}
        assert!(engine.handle(Cmd::Play {songs:std::sync::Arc::new(other),index:0,shuffle:false}));
        assert_eq!(engine.queue[0].id,old[0].id);
        assert_eq!(engine.queue_rev,revision);
        assert_eq!(engine.state,PlayState::Paused);
        assert_eq!(engine.error.as_deref(),Some("Synthetic checkpoint read failure"));
        assert_eq!(source.positions.lock().unwrap().get("generated-book"),Some(&700));
        assert!(!source.positions.lock().unwrap().contains_key("other-generated-book"));
    }

    fn spoken_queue() -> Vec<Song> {
        (0..4)
            .map(|i| Song {
                id: format!("file-{i}"),
                duration: 100,
                spoken: Some(Box::new(subsonic::Spoken {
                    id: "generated-book".into(),
                    offset_ms: i * 100000,
                    duration_ms: 400000,
                    chapters: vec![
                        subsonic::Chapter {
                            start_ms: 0,
                            title: "A".into(),
                        },
                        subsonic::Chapter {
                            start_ms: 150000,
                            title: "B".into(),
                        },
                        subsonic::Chapter {
                            start_ms: 310000,
                            title: "C".into(),
                        },
                    ],
                    ..Default::default()
                })),
                ..Default::default()
            })
            .collect()
    }
    #[test]
    fn seek_discard_uses_decoded_mono_format_instead_of_container_channel_hint() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        struct Mono;
        impl TrackSource for Mono {
            fn open(&self, _blocking: &crate::rt::Blocking, _: &Song, _: &Arc<crate::audio::interrupt::Interrupt>) -> Result<OpenedTrack, String> {
                let mut bytes = wav(1.2);
                bytes[22..24].copy_from_slice(&1u16.to_le_bytes());
                bytes[28..32].copy_from_slice(&16000u32.to_le_bytes());
                bytes[32..34].copy_from_slice(&2u16.to_le_bytes());
                Ok(OpenedTrack {
                    source: Box::new(Cursor::new(bytes)),
                    ext: Some("wav".into()),
                    origin: Origin::Offline,
                    radio_metadata: None,
                })
            }
            fn upcoming(&self, _: &[Song]) {}
            fn now_playing(&self, _: &Song) {}
            fn played(&self, _: &Song, _: u64) {}
            fn save_queue(&self, _: &[Song], _: usize, _: u64) {}
        }
        let mut engine = deterministic_spoken_engine(&blocking, Arc::new(Mono));
        engine.handle(Cmd::Play {
            songs: std::sync::Arc::new(vec![song("generated-mono")]),
            index: 0,
            shuffle: false,
        });
        engine.open_current().unwrap();
        let decoding = engine.dec.as_mut().unwrap();
        decoding.channels = 2;
        decoding.discard_frames = 700;
        assert!(engine.step().unwrap());
        assert_eq!(engine.dec.as_ref().unwrap().channels, 1);
        assert_eq!(engine.sink.as_ref().unwrap().channels, 1);
        assert!(engine.frames > 0);
    }

    #[test]
    fn deterministic_spoken_transition_replay_matches_logical_book_positions() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let source = Arc::new(SpokenRecorder {
            positions: Default::default(),
            fail: AtomicBool::new(false), read_fail: AtomicBool::new(false),
            settings: Mutex::new(Default::default()),
        });
        let mut engine = deterministic_spoken_engine(&blocking, source.clone());
        engine.handle(Cmd::Restore {
            songs: std::sync::Arc::new(spoken_queue()),
            index: 0,
            position_ms: 0,
        });
        let mut model = 0u64;
        let mut rng = fastrand::Rng::with_seed(0xB00CCAFE);
        for step in 0..3000 {
            let settings = crate::config::SpokenSettings {
                skip_chapter: rng.bool(),
                skip_forward_seconds: rng.u32(1..300),
                skip_backward_seconds: rng.u32(1..300),
                speed_percent: 100,
            };
            *source.settings.lock().unwrap() = settings;
            match rng.u8(0..5) {
                0 | 1 => {
                    let forward = rng.bool();
                    model = if settings.skip_chapter {
                        if forward {
                            [0, 150000, 310000]
                                .into_iter()
                                .find(|&p| p > model)
                                .unwrap_or(399999)
                        } else {
                            // Independent three-chapter state model: the
                            // preceding chapter index is middle only from last.
                            if model>=310000 {150000}else{0}
                        }
                    } else {
                        let amount = u64::from(if forward { settings.skip_forward_seconds } else { settings.skip_backward_seconds }) * 1000;
                        if forward {
                            model.saturating_add(amount).min(399999)
                        } else {
                            model.saturating_sub(amount)
                        }
                    };
                    engine.handle(if forward { Cmd::Next } else { Cmd::Prev });
                }
                2 => {
                    model = rng.u64(0..399000);
                    engine.handle(Cmd::Scrub {
                        generation: engine.generation,
                        position: Some(model),
                        active: false,
                    });
                }
                3 => {
                    engine.handle(Cmd::Pause);
                    engine.handle(Cmd::Play {
                        songs: std::sync::Arc::new(vec![song("music")]),
                        index: 0,
                        shuffle: false,
                    });
                    assert_eq!(
                        source.positions.lock().unwrap().get("generated-book"),
                        Some(&model)
                    );
                    engine.handle(Cmd::Play {
                        songs: std::sync::Arc::new(spoken_queue()),
                        index: 0,
                        shuffle: false,
                    });
                    engine.handle(Cmd::Pause);
                }
                _ => {
                    let before = engine.position_ms();
                    engine.handle(Cmd::Scrub {
                        generation: engine.generation.wrapping_sub(1),
                        position: Some(0),
                        active: true,
                    });
                    assert_eq!(engine.position_ms(), before);
                }
            }
            let status = engine.status.lock().unwrap().clone();
            let actual = engine.queue[engine.index]
                .spoken
                .as_ref()
                .unwrap()
                .offset_ms
                + engine.position_ms();
            assert_eq!(actual, model, "seed B00CCAFE step {step}");
            assert_eq!(status.position_ms, model, "seed B00CCAFE step {step}");
            assert_eq!(status.duration_ms, 400000);
            assert_eq!(
                status.current().unwrap().spoken.as_ref().unwrap().id,
                "generated-book"
            );
        }
    }
    #[test]
    fn chapter_skip_without_chapters_retains_position_and_explains_settings() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let source = Arc::new(SpokenRecorder {
            positions: Default::default(),
            fail: AtomicBool::new(false), read_fail: AtomicBool::new(false),
            settings: Mutex::new(crate::config::SpokenSettings {
                skip_chapter: true,
                skip_forward_seconds: 30, skip_backward_seconds: 30, speed_percent: 100,
            }),
        });
        let mut engine = deterministic_spoken_engine(&blocking, source);
        let mut queue = spoken_queue();
        for song in &mut queue {
            song.spoken.as_mut().unwrap().chapters.clear();
        }
        engine.handle(Cmd::Restore {
            songs: std::sync::Arc::new(queue),
            index: 0,
            position_ms: 7000,
        });
        let generation = engine.generation;
        for command in [Cmd::Next, Cmd::Prev] {
            engine.handle(command);
            assert_eq!(engine.position_ms(), 7000);
            assert_eq!(engine.generation, generation);
        }
        assert!(engine.error.as_ref().unwrap().contains("no chapters"));
    }

    #[test]
    fn book_chapter_preference_never_changes_podcast_time_skip_or_episode() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let source = Arc::new(SpokenRecorder { positions: Default::default(), fail: AtomicBool::new(false),
            read_fail: AtomicBool::new(false), settings: Mutex::new(crate::config::SpokenSettings {
                skip_chapter: true, skip_forward_seconds: 30, skip_backward_seconds: 30, speed_percent: 100,
            }) });
        let mut engine = deterministic_spoken_engine(&blocking, source.clone());
        let mut episodes = spoken_queue();
        for (index, song) in episodes.iter_mut().enumerate() {
            let spoken = song.spoken.as_mut().unwrap();
            spoken.id = format!("generated-episode-{index}");
            spoken.offset_ms = 0;
            spoken.chapters.clear();
            spoken.source = subsonic::SpokenSource::Audiobookshelf { namespace: "generated".into(),
                item: "show".into(), episode: Some(index.to_string()), content_url: String::new() };
        }
        engine.handle(Cmd::Restore { songs: std::sync::Arc::new(episodes), index: 0, position_ms: 7000 });
        engine.handle(Cmd::Next);
        assert_eq!((engine.index, engine.position_ms()), (0, 37000));
        engine.handle(Cmd::Prev);
        assert_eq!((engine.index, engine.position_ms()), (0, 7000));
        assert!(engine.error.is_none());
        assert!(source.settings.lock().unwrap().skip_chapter);
        engine.handle(Cmd::Restore { songs: std::sync::Arc::new(spoken_queue()), index: 0, position_ms: 7000 });
        engine.handle(Cmd::Next);
        assert_eq!(engine.status.lock().unwrap().position_ms, 150000, "book chapter mode remains selected");
    }

    #[test]
    fn failed_shutdown_checkpoint_is_reported_and_quit_finishes() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let source=Arc::new(SpokenRecorder {positions:Default::default(),fail:AtomicBool::new(false),read_fail:AtomicBool::new(false),settings:Mutex::new(Default::default())});
        let mut engine=deterministic_spoken_engine(&blocking, source.clone());
        engine.handle(Cmd::Restore {songs:std::sync::Arc::new(spoken_queue()),index:0,position_ms:7000});
        source.fail.store(true,Ordering::Relaxed);assert!(!engine.handle(Cmd::Quit));
        assert!(engine.status.lock().unwrap().error.as_ref().unwrap().contains("card-full"));
    }

    #[test]
    fn sleep_barrier_checkpoint_failure_and_cancel_never_claim_success() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let source = Arc::new(SpokenRecorder { positions: Default::default(), fail: AtomicBool::new(true),
            read_fail: AtomicBool::new(false), settings: Mutex::new(Default::default()) });
        let mut e = deterministic_spoken_engine(&blocking, source.clone());
        e.handle(Cmd::Restore { songs: std::sync::Arc::new(spoken_queue()), index: 1, position_ms: 12345 });
        let (ack, rx) = mpsc::channel();
        e.handle(Cmd::PrepareSleep(ack.into()));
        assert!(rx.recv().unwrap().is_err());
        assert!(!e.handoff_frozen);
        assert_eq!(e.position_ms(), 12345);
        source.fail.store(false, Ordering::Relaxed);
        let (ack, rx) = mpsc::channel();
        drop(rx); // Timed-out caller: no frozen audio owner afterwards.
        e.handle(Cmd::PrepareSleep(ack.into()));
        assert!(!e.handoff_frozen);
        let (ack, rx) = mpsc::channel();
        e.handle(Cmd::PrepareSleep(ack.into()));
        assert_eq!(rx.recv().unwrap(), Ok(()));
        let generation = e.generation;
        for cmd in [Cmd::PlayPause, Cmd::Next, Cmd::SeekBy(7000)] {
            e.handle(cmd);
            assert_eq!((e.generation, e.position_ms()), (generation, 12345));
            assert_eq!(e.state, PlayState::Paused);
        }
        e.handle(Cmd::CancelHandoff);
        assert!(!e.handoff_frozen);
    }

    #[test]
    fn handoff_checkpoint_failures_stay_alive_and_success_freezes_late_commands() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let source = Arc::new(SpokenRecorder {
            positions: Default::default(), fail: AtomicBool::new(true),
            read_fail: AtomicBool::new(false), settings: Mutex::new(Default::default()),
        });
        let mut engine = deterministic_spoken_engine(&blocking, source.clone());
        engine.handle(Cmd::Restore { songs: std::sync::Arc::new(spoken_queue()), index: 1, position_ms: 12345 });
        for _ in 0..2 {
            let (ack, receiver) = mpsc::channel();
            assert!(engine.handle(Cmd::PrepareHandoff(ack.into())));
            assert!(receiver.recv().unwrap().is_err());
            assert!(!engine.handoff_frozen);
            assert_eq!(engine.state, PlayState::Paused);
            assert_eq!(engine.position_ms(), 12345);
            assert!(source.positions.lock().unwrap().is_empty());
        }
        source.fail.store(false, Ordering::Relaxed);
        let (ack, receiver) = mpsc::channel();
        assert!(engine.handle(Cmd::PrepareHandoff(ack.into())));
        assert_eq!(receiver.recv().unwrap(), Ok(()));
        assert_eq!(source.positions.lock().unwrap().get("generated-book"), Some(&112345));
        let generation = engine.generation;
        let mut rng = fastrand::Rng::with_seed(0x05B_2026);
        for _ in 0..256 {
            let cmd = match rng.u8(0..10) {
                0 => Cmd::Play { songs: std::sync::Arc::new(vec![song("late-worker")]), index: 0, shuffle: false },
                1 => Cmd::Restore { songs: std::sync::Arc::new(spoken_queue()), index: 0, position_ms: 0 },
                2 => Cmd::SeekBy(90000),
                3 => Cmd::Scrub { generation, position: Some(1000), active: true },
                4 => Cmd::Next,
                5 => Cmd::Prev,
                6 => Cmd::PlayPause,
                7 => Cmd::Remove(1),
                8 => Cmd::Insert { songs: vec![song("late-insert")], next: true },
                _ => Cmd::SetVolume(99),
            };
            assert!(engine.handle(cmd));
            assert!(engine.handoff_frozen);
            assert_eq!(engine.queue[engine.index].id, "file-1");
            assert_eq!(engine.position_ms(), 12345);
            assert_eq!(engine.generation, generation);
            assert_eq!(engine.state, PlayState::Paused);
            assert!(engine.sink.is_none() && engine.dec.is_none());
        }
        assert!(engine.handle(Cmd::CancelHandoff));
        assert!(!engine.handoff_frozen);
        engine.handle(Cmd::Restore { songs: std::sync::Arc::new(vec![song("after-cancel")]), index: 0, position_ms: 456 });
        assert_eq!(engine.queue[0].id, "after-cancel");
        assert_eq!(engine.position_ms(), 456);
        let (ack, receiver) = mpsc::channel();
        engine.handle(Cmd::PrepareHandoff(ack.into()));
        assert_eq!(receiver.recv().unwrap(), Ok(()));
        assert!(!engine.handle(Cmd::Quit), "normal shutdown remains allowed while frozen");
    }

    fn take_async_handoff(rx: &Receiver<Cmd>, sleep: bool) -> HandoffAck {
        match (sleep, rx.try_recv().expect("eager player admission")) {
            (true, Cmd::PrepareSleep(ack)) | (false, Cmd::PrepareHandoff(ack)) => ack,
            _ => panic!("wrong handoff command"),
        }
    }

    #[test]
    fn abandoned_async_handoff_cancels_unpolled_pending_and_delivered_replies() {
        let ui = crate::rt::Ui::new().unwrap();
        for sleep in [false, true] {
            for phase in 0..3 {
                let (player, commands) = PlayerHandle::fixture(Status::default());
                let mut pending = Box::pin(player.prepare_barrier_async(Duration::from_secs(2), sleep));
                let ack = take_async_handoff(&commands, sleep);
                assert!(ui.block_on(player.prepare_barrier_async(Duration::from_secs(2), sleep)).is_err());
                assert!(commands.try_recv().is_err(), "refused duplicate cannot cancel the first owner");
                if phase == 1 {
                    ui.block_on(async {
                        std::future::poll_fn(|cx| {
                            assert!(std::future::Future::poll(pending.as_mut(), cx).is_pending());
                            std::task::Poll::Ready(())
                        }).await;
                        let (pulse, pulsed) = tokio::sync::oneshot::channel();
                        crate::rt::io().spawn(async move { let _ = pulse.send(()); });
                        tokio::time::timeout(Duration::from_secs(1), pulsed).await.unwrap().unwrap();
                    });
                }
                let late = if phase == 2 { ack.send(Ok(())).unwrap(); None } else { Some(ack) };
                ui.block_on(async { drop(pending); });
                assert!(!player.handoff_pending.load(Ordering::Acquire));
                assert!(matches!(commands.try_recv().unwrap(), Cmd::CancelHandoff));
                if let Some(ack) = late { assert!(ack.send(Ok(())).is_err(), "late success must observe the lost receiver"); }
                player.send(Cmd::SetVolume(71));
                assert!(matches!(commands.try_recv().unwrap(), Cmd::SetVolume(71)));
            }
        }
    }

    #[test]
    fn async_handoff_timeout_and_stale_observers_preserve_newer_acquisition() {
        let ui = crate::rt::Ui::new().unwrap();
        for sleep in [false, true] {
            for poll_stale in [false, true] {
                let (player, commands) = PlayerHandle::fixture(Status::default());
                let old = player.prepare_barrier_async(Duration::from_secs(2), sleep);
                take_async_handoff(&commands, sleep).send(Ok(())).unwrap();
                player.cancel_handoff();
                assert!(matches!(commands.try_recv().unwrap(), Cmd::CancelHandoff));
                let next = player.prepare_barrier_async(Duration::from_secs(2), sleep);
                let ack = take_async_handoff(&commands, sleep);
                if poll_stale { assert_eq!(ui.block_on(old), Err("Power or storage transition was cancelled")); }
                else { ui.block_on(async { drop(old); }); }
                assert!(commands.try_recv().is_err(), "stale cleanup cannot release a newer barrier");
                assert!(player.handoff_pending.load(Ordering::Acquire));
                ack.send(Ok(())).unwrap();
                assert_eq!(ui.block_on(next), Ok(()));
                assert!(player.handoff_pending.load(Ordering::Acquire), "success transfers the held barrier");
                player.cancel_handoff();
                assert!(matches!(commands.try_recv().unwrap(), Cmd::CancelHandoff));
            }
            for outcome in 0..3 {
                let (player, commands) = PlayerHandle::fixture(Status::default());
                let pending = player.prepare_barrier_async(if outcome == 0 { Duration::ZERO } else { Duration::from_secs(2) }, sleep);
                let ack = take_async_handoff(&commands, sleep);
                let late = match outcome {
                    0 => Some(ack),
                    1 => { drop(ack); None },
                    _ => { ack.send(Err("generated durable checkpoint refusal")).unwrap(); None },
                };
                let result = ui.block_on(pending);
                assert_eq!(result, Err(match outcome {
                    0 => "Audio player is busy; transition cancelled",
                    1 => "Audio player is unavailable; transition cancelled",
                    _ => "generated durable checkpoint refusal",
                }));
                assert!(matches!(commands.try_recv().unwrap(), Cmd::CancelHandoff));
                assert!(!player.handoff_pending.load(Ordering::Acquire));
                if let Some(ack) = late { assert!(ack.send(Ok(())).is_err()); }
            }
            let (player, commands) = PlayerHandle::fixture(Status::default());
            drop(commands);
            assert_eq!(ui.block_on(player.prepare_barrier_async(Duration::from_secs(2), sleep)),
                Err("Audio player is unavailable; transition cancelled"));
            assert!(!player.handoff_pending.load(Ordering::Acquire));
        }
    }

    #[test]
    fn timed_out_handoff_queues_cancel_and_does_not_strand_the_worker() {
        struct BlockingCheckpoint { entered: Sender<()>, release: Mutex<Receiver<()>> }
        impl TrackSource for BlockingCheckpoint {
            fn open(&self, _blocking: &crate::rt::Blocking, _: &Song, _: &Arc<crate::audio::interrupt::Interrupt>) -> Result<OpenedTrack, String> { unreachable!() }
            fn upcoming(&self, _: &[Song]) {}
            fn now_playing(&self, _: &Song) {}
            fn played(&self, _: &Song, _: u64) {}
            fn save_queue(&self, _: &[Song], _: usize, _: u64) {}
            fn checkpoint(&self, _: &Song, _: u64) -> Result<(), String> {
                self.entered.send(()).unwrap();
                self.release.lock().unwrap().recv().unwrap();
                Ok(())
            }
        }
        for sleep in [false, true] {
        let (entered, entry) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let source = Arc::new(BlockingCheckpoint { entered, release: Mutex::new(released) });
        let p = spawn(source, Box::new(SoftHw(FAKE.into())), Settings {
            eq: Default::default(), volume: 0, headphone_power: crate::config::HeadphonePower::Standard, filter: 0,
            replay_gain: ReplayGainMode::Off, gapless: true,
        }, Arc::new(|| {}));
        p.send(Cmd::Restore { songs: std::sync::Arc::new(vec![song("before")]), index: 0, position_ms: 123 });
        assert_eq!(wait_for(&p, 2, |s| s.state == PlayState::Paused).position_ms, 123);
        let caller = p.clone();
        let (done, completion) = mpsc::channel();
        let task = std::thread::spawn(move || done.send(caller.prepare_barrier(Duration::from_millis(100), sleep)).unwrap());
        entry.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(completion.recv_timeout(Duration::from_secs(2)).unwrap().is_err());
        task.join().unwrap();
        assert!(!p.handoff_pending.load(Ordering::Acquire));
        release.send(()).unwrap();
        // SetVolume is a command-completion barrier: a stranded frozen engine
        // would reject it, even though the timed-out Prepare later saved OK.
        p.send(Cmd::SetVolume(71));
        assert_eq!(wait_for(&p, 2, |s| s.volume == 71).volume, 71);
        release.send(()).unwrap(); // Orderly shutdown's final checkpoint.
        p.shutdown();
        for _ in 0..2 {
            assert!(p.prepare_barrier(Duration::from_millis(100), sleep).is_err(), "closed worker channel must fail promptly");
            assert!(!p.handoff_pending.load(Ordering::Acquire));
        }
        }
    }

    #[test]
    fn closing_stream_cancels_owned_io_without_creating_a_command() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let source = Arc::new(SpokenRecorder {
            positions: Default::default(), fail: AtomicBool::new(false),
            read_fail: AtomicBool::new(false), settings: Mutex::new(Default::default()),
        });
        let mut engine = deterministic_spoken_engine(&blocking, source);
        for ready in [false, true] {
            let cancel = Arc::new(crate::audio::interrupt::Interrupt::new(false));
            *engine.current_io.lock().unwrap() = Some(IoSession { cancel: cancel.clone(), ready });
            engine.close_track();
            assert!(cancel.load(Ordering::Relaxed));
            assert!(engine.current_io.lock().unwrap().is_none());
            assert!(!engine.interrupt.load(Ordering::Relaxed));
        }
        *engine.current_io.lock().unwrap() = Some(IoSession { cancel: engine.interrupt.clone(), ready: true });
        engine.close_track();
        assert!(!engine.interrupt.load(Ordering::Relaxed));
        assert!(engine.current_io.lock().unwrap().is_none());
    }

    #[test]
    fn handoff_interrupts_open_io_and_can_be_cancelled_after_success() {
        struct InterruptedOpen { entered: Sender<()> }
        impl TrackSource for InterruptedOpen {
            fn open(&self, _blocking: &crate::rt::Blocking, _: &Song, interrupt: &Arc<crate::audio::interrupt::Interrupt>) -> Result<OpenedTrack, String> {
                self.entered.send(()).unwrap();
                let deadline = Instant::now() + Duration::from_secs(2);
                while !interrupt.load(Ordering::Relaxed) && Instant::now() < deadline { std::thread::yield_now(); }
                assert!(interrupt.load(Ordering::Relaxed), "handoff must interrupt blocking media I/O");
                Err(INTERRUPTED.into())
            }
            fn upcoming(&self, _: &[Song]) {}
            fn now_playing(&self, _: &Song) {}
            fn played(&self, _: &Song, _: u64) {}
            fn save_queue(&self, _: &[Song], _: usize, _: u64) {}
        }
        for (sleep, target) in [false, true].into_iter().flat_map(|sleep| [song("blocked-open"), spoken_queue()[0].clone()].into_iter().map(move |target| (sleep, target))) {
        let (entered, entry) = mpsc::channel();
        let p = spawn(Arc::new(InterruptedOpen { entered }), Box::new(SoftHw(FAKE.into())), Settings {
            eq: Default::default(), volume: 0, headphone_power: crate::config::HeadphonePower::Standard, filter: 0,
            replay_gain: ReplayGainMode::Off, gapless: true,
        }, Arc::new(|| {}));
        p.send(Cmd::Play { songs: std::sync::Arc::new(vec![target]), index: 0, shuffle: false });
        entry.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(p.prepare_barrier(Duration::from_secs(2), sleep), Ok(()));
        assert_eq!(p.status().state, PlayState::Paused);
        assert!(p.prepare_barrier(Duration::from_secs(2), sleep).is_err(), "cloned handles share one handoff");
        p.cancel_handoff();
        p.send(Cmd::Restore { songs: std::sync::Arc::new(vec![song("resumed-after-launch-failure")]), index: 0, position_ms: 500 });
        assert_eq!(wait_for(&p, 2, |s| s.position_ms == 500).position_ms, 500);
        p.shutdown();
        }
    }

    #[test]
    fn spoken_card_failure_preserves_current_book_and_reports_unsaved_position() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let source = Arc::new(SpokenRecorder {
            positions: Default::default(),
            fail: AtomicBool::new(false), read_fail: AtomicBool::new(false),
            settings: Mutex::new(Default::default()),
        });
        let mut engine = deterministic_spoken_engine(&blocking, source.clone());
        engine.handle(Cmd::Restore {
            songs: std::sync::Arc::new(spoken_queue()),
            index: 1,
            position_ms: 12345,
        });
        source.fail.store(true, Ordering::Relaxed);
        engine.handle(Cmd::Play {
            songs: std::sync::Arc::new(vec![song("music")]),
            index: 0,
            shuffle: false,
        });
        assert_eq!(engine.queue[engine.index].id, "file-1");
        assert_eq!(engine.position_ms(), 12345);
        assert_eq!(engine.state, PlayState::Paused);
        assert!(engine.error.as_ref().unwrap().contains("card-full"));
        source.fail.store(false, Ordering::Relaxed);
        engine.handle(Cmd::Play {
            songs: std::sync::Arc::new(vec![song("music")]),
            index: 0,
            shuffle: false,
        });
        assert_eq!(
            source.positions.lock().unwrap().get("generated-book"),
            Some(&112345)
        );
        assert_eq!(engine.queue[engine.index].id, "music");
    }
    #[test]
    fn shutdown_waits_for_the_final_queue_save_before_returning() {
        struct BlockingSave {
            entered: Sender<()>,
            release: Mutex<Receiver<()>>,
            saved: AtomicBool,
        }
        impl TrackSource for BlockingSave {
            fn open(&self, _blocking: &crate::rt::Blocking, _: &Song, _: &Arc<crate::audio::interrupt::Interrupt>) -> Result<OpenedTrack, String> {
                unreachable!("restored paused queue must not open a track")
            }
            fn upcoming(&self, _: &[Song]) {}
            fn now_playing(&self, _: &Song) {}
            fn played(&self, _: &Song, _: u64) {}
            fn save_queue(&self, queue: &[Song], _: usize, position: u64) {
                assert_eq!(queue[0].id, "saved");
                assert_eq!(position, 500);
                self.entered.send(()).unwrap();
                self.release.lock().unwrap().recv().unwrap();
                self.saved.store(true, Ordering::SeqCst);
            }
        }
        let (entered, entry) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let source = Arc::new(BlockingSave { entered, release: Mutex::new(released), saved: AtomicBool::new(false) });
        let settings = Settings { eq: Default::default(), volume: 100, headphone_power: crate::config::HeadphonePower::Standard, filter: 0,
            replay_gain: ReplayGainMode::Off, gapless: true };
        let p = spawn(source.clone(), Box::new(SoftHw(FAKE.into())), settings, Arc::new(|| {}));
        p.send(Cmd::Restore { songs: std::sync::Arc::new(vec![song("saved")]), index: 0, position_ms: 500 });
        let status = wait_for(&p, 2, |s| s.state == PlayState::Paused);
        assert_eq!(status.position_ms, 500);
        let (completed, completion) = mpsc::channel();
        let stopping = p.clone();
        let caller = std::thread::spawn(move || { stopping.shutdown(); completed.send(()).unwrap(); });
        entry.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(completion.try_recv(), Err(mpsc::TryRecvError::Empty)));
        assert!(!source.saved.load(Ordering::SeqCst));
        release.send(()).unwrap();
        completion.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(source.saved.load(Ordering::SeqCst));
        caller.join().unwrap();
        p.shutdown(); // Cloned handles share the completed join, without a second save.
    }

    #[test]
    fn long_spoken_pause_keeps_stream_decoder_and_unheard_audio() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        struct Buffered {
            bytes:Arc<Vec<u8>>, opens:std::sync::atomic::AtomicUsize,
            network:Arc<AtomicBool>, requests:Arc<std::sync::atomic::AtomicUsize>,
        }
        impl TrackSource for Buffered {
            fn open(&self, _blocking: &crate::rt::Blocking,_:&Song,interrupt:&Arc<crate::audio::interrupt::Interrupt>)->Result<OpenedTrack,String> {
                self.opens.fetch_add(1,Ordering::Relaxed);
                let bytes=self.bytes.clone();let network=self.network.clone();let requests=self.requests.clone();
                Ok(OpenedTrack{source:Box::new(super::super::source::RangeReader::new(bytes.len() as u64,interrupt.clone(),Arc::new(move |offset,length| {
                    requests.fetch_add(1,Ordering::Relaxed);
                    if !network.load(Ordering::Relaxed) {return Err(std::io::Error::other("generated temporarily unavailable network"));}
                    Ok(Some(bytes[offset as usize..offset as usize+length].to_vec()))
                }))),ext:Some("wav".into()),origin:Origin::Streaming,radio_metadata:None})
            }
            fn upcoming(&self,_:&[Song]) {} fn now_playing(&self,_:&Song) {}
            fn played(&self,_:&Song,_:u64) {} fn save_queue(&self,_:&[Song],_:usize,_:u64) {}
        }
        for speed in [50,100,175,300] {
            let mut encoded=wav(120.0);
            for(i,sample)in encoded[44..].chunks_exact_mut(2).enumerate() {sample.copy_from_slice(&(((i%1024) as i16-512)*42).to_le_bytes());}
            let source=Arc::new(Buffered{bytes:Arc::new(encoded),opens:0.into(),network:Arc::new(AtomicBool::new(true)),requests:Arc::new(0.into())});
            let mut e=deterministic_spoken_engine(&blocking, source.clone());
            e.volume=60;e.apply_volume().unwrap();
            let mut book=spoken_queue()[0].clone();book.duration=120;book.spoken.as_mut().unwrap().duration_ms=120_000;
            e.handle(Cmd::Play{songs:std::sync::Arc::new(vec![book]),index:0,shuffle:false});e.open_current().unwrap();
            e.speed_percent=speed;
            for _ in 0..3 {assert!(e.step().unwrap());}
            let deadline=Instant::now()+Duration::from_secs(2);
            while source.requests.load(Ordering::Relaxed)<2 && Instant::now()<deadline {std::thread::yield_now();}
            assert_eq!(source.requests.load(Ordering::Relaxed),2);
            source.network.store(false,Ordering::Relaxed);
            for _ in 0..3 {
                e.pause();let cursor=e.audible_cursor();
                assert!(e.dec.is_some(),"ordinary pause retained decoder at {speed}%");
                let before=e.sink.as_ref().unwrap().retained(&blocking).unwrap();
                e.idle_since=Instant::now()-IDLE_RELEASE;e.idle();
                assert!(e.sink.is_none() && e.dec.is_some());
                assert_eq!(e.parked.as_ref().unwrap().samples,before.samples);
                assert_eq!(e.audible_cursor(),cursor,"powering down output must not advance media");
                e.resume();assert!(e.error.is_none(),"{:?}",e.error);
                assert_eq!(e.state,PlayState::Playing);
                assert!(e.dec.is_some() && e.parked.is_none());
                assert_eq!(e.sink.as_ref().unwrap().written_samples,before.samples,"replay every unheard sample exactly once");
                assert!(e.step().unwrap(),"retained stream continues while network unavailable");
            }
            let generation = e.generation;
            let (ack, rx) = mpsc::channel();
            e.handle(Cmd::PrepareSleep(ack.into()));
            assert_eq!(rx.recv().unwrap(), Ok(()));
            assert_eq!(e.generation, generation);
            assert!(e.handoff_frozen && e.dec.is_some());
            e.handle(Cmd::CancelHandoff);
            let parked=e.audible_cursor();
            assert!(e.parked.is_some() && e.sink.is_none() && e.dec.is_some());
            e.handle(Cmd::ParkOutput); // Idempotent sleep preparation.
            assert_eq!(e.audible_cursor(),parked);
            let generation=e.generation;
            e.handle(Cmd::Resume { generation:generation.wrapping_sub(1) });
            assert_eq!(e.state,PlayState::Paused);
            e.handle(Cmd::Resume { generation });
            assert!(e.error.is_none());assert!(e.step().unwrap());
            e.handle(Cmd::Resume { generation });
            assert_eq!(e.state,PlayState::Playing,"duplicate resume must not toggle pause");
            assert_eq!(source.opens.load(Ordering::Relaxed),1);
            assert_eq!(source.requests.load(Ordering::Relaxed),2,"no new range needed for resume");
            e.pause();e.idle_since=Instant::now()-IDLE_RELEASE;e.idle();
            e.handle(Cmd::Play{songs:std::sync::Arc::new(vec![song("different")]),index:0,shuffle:false});
            assert!(e.parked.is_none() && e.dec.is_none(),"content switch releases retained state");
            e.pause();
            e.handle(Cmd::Resume { generation });
            assert_eq!(e.state,PlayState::Paused,"stale info-page action cannot resume replacement content");
        }
    }

    #[test]
    fn sleep_refuses_current_unverified_pause_fault_without_dropping_pcm() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut e=deterministic_spoken_engine(&blocking, Arc::new(Recorder::default()));
        e.handle(Cmd::Play{songs:std::sync::Arc::new(vec![spoken_queue()[0].clone()]),index:0,shuffle:false});
        e.open_current().unwrap();assert!(e.step().unwrap());
        let sink=e.sink.as_mut().unwrap();
        sink.script_pcm_state(2,Arc::new(0.into()));
        sink.script_pause_code(-libc::EBADFD);sink.set_fake_delay(u64::MAX);
        let (tx,rx)=mpsc::channel();e.handle(Cmd::PrepareSleep(tx.into()));
        assert!(rx.recv().unwrap().is_err());
        assert!(!e.handoff_frozen && e.pause_admission_fault.is_some());
        assert!(e.sink.as_ref().unwrap().pause_transfer_unverified());
        let held=e.sink.as_ref().unwrap().retained(&blocking).unwrap().samples;
        let (tx,rx)=mpsc::channel();e.handle(Cmd::PrepareSleep(tx.into()));
        assert!(rx.recv().unwrap().is_err());
        assert!(e.dec.is_some() && e.parked.is_none());
        assert_eq!(e.sink.as_ref().unwrap().retained(&blocking).unwrap().samples,held);
    }

    #[test]
    fn incomplete_prepared_queue_stays_open_and_waits_after_idle_deadline() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let source=Arc::new(Recorder::default());
        let mut e=deterministic_spoken_engine(&blocking, source);
        let (monitor,topology)=super::super::output::Monitor::fixture();
        e.route=monitor.snapshot();e.outputs=Some(monitor);
        e.handle(Cmd::Play{songs:std::sync::Arc::new(vec![spoken_queue()[0].clone()]),index:0,shuffle:false});
        e.open_current().unwrap();assert!(e.step().unwrap());
        let sink=e.sink.as_mut().unwrap();
        sink.script_pcm_state(2,Arc::new(0.into()));
        sink.script_pause_code(-libc::EBADFD);
        sink.set_fake_delay(u64::MAX);
        e.pause();
        assert!(e.pause_admission_fault.is_some() && e.dec.is_some());
        let generation=e.generation;let cursor=e.audible_cursor();
        let held=e.sink.as_ref().unwrap().retained(&blocking).unwrap().samples;
        for _ in 0..3 {
            e.idle_since=Instant::now()-IDLE_RELEASE;
            assert_eq!(e.idle_wait(),Duration::ZERO);
            e.idle();
            assert!(e.idle_wait()+Duration::from_secs(1)>IDLE_RELEASE,"a failed revalidation waits a whole deadline, never spins");
            e.handle(Cmd::ParkOutput);
            assert!(e.sink.is_some() && e.dec.is_some() && e.parked.is_none());
            assert!(e.sink.as_ref().unwrap().pause_transfer_unverified());
            assert_eq!(e.sink.as_ref().unwrap().retained(&blocking).unwrap().samples,held);
            assert_eq!((e.generation,e.audible_cursor()),(generation,cursor));
        }
        topology.update(vec![super::super::output::UsbPlayback {identity:"generated-unverified-topology".into(),card:74,device:0}]);
        e.refresh_route();
        assert!(e.sink.is_some() && e.dec.is_some() && e.parked.is_none(),"topology must not destroy an unverified PREPARED backend");
        assert!(e.sink.as_ref().unwrap().pause_transfer_unverified());
        assert_eq!(e.sink.as_ref().unwrap().retained(&blocking).unwrap().samples,held);
        assert_eq!((e.generation,e.audible_cursor()),(generation,cursor));
        assert!(e.idle_wait()<=IDLE_RELEASE);
        e.handle(Cmd::Play{songs:std::sync::Arc::new(vec![song("explicit-replacement")]),index:0,shuffle:false});
        assert!(e.sink.as_ref().is_none_or(|sink|!sink.pause_transfer_unverified()));
    }

    #[test]
    fn revalidated_empty_prepared_retry_does_not_clear_visible_fault_without_output() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut e=deterministic_spoken_engine(&blocking, Arc::new(Recorder::default()));
        e.handle(Cmd::Play{songs:std::sync::Arc::new(vec![spoken_queue()[0].clone()]),index:0,shuffle:false});
        e.open_current().unwrap();assert!(e.step().unwrap());
        let sink=e.sink.as_mut().unwrap();
        sink.script_pcm_state(2,Arc::new(0.into()));sink.set_fake_delay(u64::MAX);
        e.pause();assert!(e.pause_admission_fault.is_some());
        assert!(e.sink.as_ref().unwrap().paused_frames()>0);
        e.sink.as_mut().unwrap().set_fake_delay(0);
        e.resume();
        assert_eq!(e.sink.as_ref().unwrap().paused_frames(),0);
        assert!(e.pause_admission_fault.is_some() && e.error.is_some(),"prevalidation frame count must not credit an empty retry as output progress");
    }

    #[test]
    fn active_topology_change_refuses_unverified_prepared_transfer_without_retry() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut e=deterministic_spoken_engine(&blocking, Arc::new(Recorder::default()));
        let (monitor,topology)=super::super::output::Monitor::fixture();
        e.route=monitor.snapshot();e.outputs=Some(monitor);
        e.handle(Cmd::Play{songs:std::sync::Arc::new(vec![spoken_queue()[0].clone()]),index:0,shuffle:false});
        e.open_current().unwrap();assert!(e.step().unwrap());
        let sink=e.sink.as_mut().unwrap();
        sink.script_pcm_state(2,Arc::new(0.into()));sink.set_fake_delay(u64::MAX);
        topology.update(vec![super::super::output::UsbPlayback {identity:"generated-active-unverified".into(),card:74,device:0}]);
        e.refresh_route();
        assert_eq!(e.state,PlayState::Paused);
        assert!(!e.resume_output && e.pause_admission_fault.is_some());
        assert!(e.sink.as_ref().unwrap().pause_transfer_unverified());
        assert!(e.dec.is_some() && e.parked.is_none());
        assert!(e.idle_wait()<=IDLE_RELEASE,"the paused unverified queue is revalidated at the deadline");
    }

    /// V0.128 on the Q2 (2026-11-22, Wi-Fi associated, shuffle from the
    /// server): a stream failed to open, the PCM underran, a pause of the
    /// XRUN PCM could not establish its queue ("Stopped audio delay: Broken
    /// pipe"), the next song failed and the player stopped. The PCM stayed
    /// in XRUN with dac-power on for minutes: idle waits were an hour and the
    /// idle deadline never closed an unverified queue.
    #[test]
    fn an_unverified_output_left_by_xrun_and_failed_opens_is_released_after_the_stop() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let source=Arc::new(Recorder::default());
        let mut e=deterministic_spoken_engine(&blocking, source.clone());
        e.handle(Cmd::Play{songs:std::sync::Arc::new(vec![song("cached"),song("missing")]),index:0,shuffle:false});
        e.open_current().unwrap();assert!(e.step().unwrap());
        // The device underran: XRUN, and its delay query fails (EPIPE).
        let sink=e.sink.as_mut().unwrap();
        sink.script_pcm_state(super::super::alsa::STATE_XRUN_FOR_TESTS,Arc::new(0.into()));
        sink.fail_fake_delay(true);
        // A retained-queue transaction on it (the headroom retarget) fails,
        // and that failure ends the song.
        let error=e.sink.as_mut().unwrap().pause(&blocking,true).unwrap_err();
        assert!(error.starts_with("Stopped audio delay"),"{error}");
        assert!(e.sink.as_ref().unwrap().pause_transfer_unverified());
        e.failed(error);
        assert_eq!(e.index,1);
        // The next song cannot be opened: the queue stops.
        let error=e.open_current().unwrap_err();
        e.failed(error);
        assert_eq!(e.state,PlayState::Stopped);
        assert!(e.sink.is_some(),"the output is still open after the stop");
        let wait=e.idle_wait();
        assert!(wait<=IDLE_RELEASE && wait+Duration::from_secs(1)>IDLE_RELEASE,"the idle release counts from the stop: {wait:?}");
        e.idle_since=Instant::now()-IDLE_RELEASE;
        e.idle();
        assert!(e.sink.is_none() && e.parked.is_none(),"PCM closed and DAC powered down within IDLE_RELEASE_MS");
        assert_eq!(e.idle_wait(),Duration::from_secs(3600),"nothing left to release");
        // Play still works from the stopped queue.
        e.handle(Cmd::Play{songs:std::sync::Arc::new(vec![song("cached")]),index:0,shuffle:false});
        e.open_current().unwrap();assert!(e.step().unwrap());
        assert!(e.sink.is_some() && !e.sink.as_ref().unwrap().pause_transfer_unverified());
    }

    /// The decided XRUN contract (Product feedback, 2026-11-22): on the internal codec
    /// an underrun leaves nothing unheard, so the retained-queue pause that
    /// failed on V0.128 succeeds, the cached song is not skipped, and the
    /// idle deadline parks and powers the DAC down.
    #[test]
    fn internal_codec_xrun_pause_is_verified_and_parks_at_the_deadline() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut e=deterministic_spoken_engine(&blocking, Arc::new(Recorder::default()));
        e.handle(Cmd::Play{songs:std::sync::Arc::new(vec![song("cached"),song("next")]),index:0,shuffle:false});
        e.open_current().unwrap();assert!(e.step().unwrap());
        let sink=e.sink.as_mut().unwrap();
        sink.set_xrun_empties_queue(true);
        sink.script_pcm_state(super::super::alsa::STATE_XRUN_FOR_TESTS,Arc::new(0.into()));
        sink.fail_fake_delay(true);
        // The headroom retarget's transaction.
        assert!(e.sink.as_mut().unwrap().pause(&blocking,true).unwrap(),"an empty queue is retained");
        assert!(!e.sink.as_ref().unwrap().pause_transfer_unverified());
        e.sink.as_mut().unwrap().resume(&blocking).unwrap();
        assert_eq!(e.index,0,"no song is skipped");
        // An owner pause on the underrun PCM parks at the deadline.
        e.pause();
        assert!(e.error.is_none() && e.pause_admission_fault.is_none(),"{:?}",e.error);
        e.idle_since=Instant::now()-IDLE_RELEASE;e.idle();
        assert!(e.sink.is_none(),"PCM closed and DAC powered down");
        e.resume();
        assert_eq!(e.state,PlayState::Playing);
    }

    #[test]
    fn a_paused_unverified_queue_parks_at_the_deadline_once_its_delay_can_be_read() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut e=deterministic_spoken_engine(&blocking, Arc::new(Recorder::default()));
        e.handle(Cmd::Play{songs:std::sync::Arc::new(vec![spoken_queue()[0].clone()]),index:0,shuffle:false});
        e.open_current().unwrap();assert!(e.step().unwrap());
        let sink=e.sink.as_mut().unwrap();
        sink.script_pcm_state(2,Arc::new(0.into()));
        sink.script_pause_code(-libc::EBADFD);sink.fail_fake_delay(true);
        e.pause();
        assert!(e.sink.as_ref().unwrap().pause_transfer_unverified());
        let cursor=e.audible_cursor();
        // Still unreadable at the deadline: kept, retried a deadline later.
        e.idle_since=Instant::now()-IDLE_RELEASE;e.idle();
        assert!(e.sink.is_some() && e.parked.is_none());
        assert!(e.idle_wait()+Duration::from_secs(1)>IDLE_RELEASE);
        // Readable at the next deadline: exactly the unheard frames park.
        let sink=e.sink.as_mut().unwrap();
        sink.fail_fake_delay(false);sink.set_fake_delay(2);
        e.idle_since=Instant::now()-IDLE_RELEASE;e.idle();
        assert!(e.sink.is_none(),"PCM closed and DAC powered down");
        assert_eq!(e.parked.as_ref().unwrap().frames(),2);
        let parked=e.audible_cursor();
        assert!(parked.0==cursor.0 && parked.1>=cursor.1,"the exact queue moves the conservative cursor forward only: {cursor:?} -> {parked:?}");
        e.resume();
        assert_eq!(e.state,PlayState::Playing);
        assert_eq!(e.sink.as_ref().unwrap().written_samples.len(),2*2,"the two unheard frames replay once");
    }

    #[test]
    fn actual_sleep_handle_keeps_ready_stream_and_inflight_range_until_resume() {
        struct GatedRanges {
            bytes: Arc<Vec<u8>>, began: Sender<()>, release: Arc<(Mutex<bool>, std::sync::Condvar)>,
            opens: std::sync::atomic::AtomicUsize, cancel: Mutex<Option<Arc<crate::audio::interrupt::Interrupt>>>,
        }
        impl TrackSource for GatedRanges {
            fn open(&self, _blocking: &crate::rt::Blocking, _: &Song, cancel: &Arc<crate::audio::interrupt::Interrupt>) -> Result<OpenedTrack, String> {
                self.opens.fetch_add(1, Ordering::Relaxed);
                *self.cancel.lock().unwrap() = Some(cancel.clone());
                let bytes = self.bytes.clone(); let began = self.began.clone(); let gate = self.release.clone();
                Ok(OpenedTrack { source: Box::new(super::super::source::RangeReader::new(bytes.len() as u64, cancel.clone(), Arc::new(move |offset, length| {
                    if offset != 0 {
                        let _ = began.send(());
                        let mut ready = gate.0.lock().unwrap();
                        while !*ready { ready = gate.1.wait(ready).unwrap(); }
                    }
                    Ok(Some(bytes[offset as usize..offset as usize + length].to_vec()))
                }))), ext: Some("wav".into()), origin: Origin::Streaming, radio_metadata: None })
            }
            fn upcoming(&self, _: &[Song]) {} fn now_playing(&self, _: &Song) {}
            fn played(&self, _: &Song, _: u64) {} fn save_queue(&self, _: &[Song], _: usize, _: u64) {}
        }
        let (began, blocking) = mpsc::channel();
        let gate = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
        let source = Arc::new(GatedRanges { bytes: Arc::new(wav(30.0)), began, release: gate.clone(), opens: 0.into(), cancel: Mutex::new(None) });
        let p = spawn(source.clone(), Box::new(SoftHw(FAKE.into())), Settings { eq: Default::default(), volume: 50,
            headphone_power: crate::config::HeadphonePower::Standard, filter: 0, replay_gain: ReplayGainMode::Off, gapless: true }, Arc::new(|| {}));
        let mut book = spoken_queue()[0].clone(); book.duration = 30; book.spoken.as_mut().unwrap().duration_ms = 30_000;
        p.send(Cmd::Play { songs: std::sync::Arc::new(vec![book]), index: 0, shuffle: false });
        blocking.recv_timeout(Duration::from_secs(2)).unwrap();
        let playing = wait_for(&p, 2, |s| s.state == PlayState::Playing && s.position_ms > 100);
        let generation = playing.generation;
        assert_eq!(p.prepare_sleep(), Ok(()));
        let paused = p.status();
        assert_eq!((paused.state, paused.generation), (PlayState::Paused, generation));
        assert!(!source.cancel.lock().unwrap().as_ref().unwrap().load(Ordering::Relaxed), "ready source must not receive cancellation");
        assert_eq!(source.opens.load(Ordering::Relaxed), 1);
        p.send(Cmd::Resume { generation }); // Frozen late worker cannot resume output.
        p.cancel_handoff();
        p.send(Cmd::Resume { generation });
        *gate.0.lock().unwrap() = true; gate.1.notify_all();
        let resumed = wait_for(&p, 3, |s| s.position_ms > paused.position_ms + 200);
        assert!(resumed.error.is_none(), "{:?}", resumed.error);
        assert_eq!(resumed.generation, generation);
        assert_eq!(source.opens.load(Ordering::Relaxed), 1, "resume reused decoder/read-ahead session");
        assert!(!source.cancel.lock().unwrap().as_ref().unwrap().load(Ordering::Relaxed));
        p.shutdown();
    }

    #[test]
    fn parked_pause_releases_resources_for_handoff_and_keeps_cursor() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut e=deterministic_spoken_engine(&blocking, Arc::new(Recorder::default()));
        e.handle(Cmd::Play{songs:std::sync::Arc::new(vec![spoken_queue()[0].clone()]),index:0,shuffle:false});
        e.open_current().unwrap();assert!(e.step().unwrap());e.pause();
        let cursor=e.audible_cursor();e.idle_since=Instant::now()-IDLE_RELEASE;e.idle();
        assert!(e.parked.is_some());
        let(tx,rx)=mpsc::channel();e.handle(Cmd::PrepareHandoff(tx.into()));assert_eq!(rx.recv().unwrap(),Ok(()));
        assert!(e.dec.is_none()&&e.sink.is_none()&&e.parked.is_none());
        assert_eq!(e.audible_cursor(),cursor);
    }

    #[test]
    fn contended_radio_metadata_cannot_hold_published_status_lock() {
        let metadata:subsonic::radio::Metadata=Arc::new(Mutex::new(Some("Generated title".into())));
        let held=metadata.lock().unwrap();
        let owned=metadata.clone();
        let (status_tx,status_rx)=mpsc::channel();
        let (entered_tx,entered_rx)=mpsc::channel();
        let worker=std::thread::spawn(move || {
            let blocking=crate::rt::Blocking::dedicated_thread();
            let mut engine=deterministic_spoken_engine(&blocking,Arc::new(Recorder::default()));
            engine.handle(Cmd::Play {songs:std::sync::Arc::new(vec![song("generated-status")]),index:0,shuffle:false});
            engine.open_current().unwrap();
            engine.publish(true); // Establish the queue before introducing contention.
            engine.dec.as_mut().unwrap().radio_metadata=Some(owned);
            let before=engine.status.lock().unwrap().queue.clone();
            status_tx.send(engine.status.clone()).unwrap();
            PUBLICATION_METADATA_ENTERED.with(|hook| *hook.borrow_mut()=Some(entered_tx));
            engine.publish(true);
            let after=engine.status.lock().unwrap().clone();
            assert!(Arc::ptr_eq(&before,&after.queue),"unchanged queue must reuse its Arc");
            assert_eq!(after.radio_title.as_deref(),Some("Generated title"));
            Arc::make_mut(&mut engine.queue).push(song("new-status-entry")); engine.queue_rev+=1;
            engine.publish(true);
            let changed=engine.status.lock().unwrap().clone();
            assert!(!Arc::ptr_eq(&after.queue,&changed.queue));
            assert_eq!(changed.queue.len(),2);
            engine.handle(Cmd::Quit);
        });
        let status=status_rx.recv_timeout(Duration::from_secs(3));
        let entered=entered_rx.recv_timeout(Duration::from_secs(3));
        // Capture, then always release and join before assertions, including
        // the old implementation's failure where metadata waits under status.
        let available=status.as_ref().ok().map(|status| status.try_lock().is_ok());
        drop(held);
        worker.join().unwrap();
        assert!(status.is_ok() && entered.is_ok(),"publisher did not reach metadata contention");
        assert_eq!(available,Some(true),"metadata lookup held the UI snapshot mutex");
    }

    #[test]
    fn radio_metadata_reaches_status_and_clears_on_pause_and_reconnect() {
        use std::io::{Read, Write};
        struct Live;
        impl TrackSource for Live {
            fn open(&self, _blocking: &crate::rt::Blocking, song: &Song, interrupt: &Arc<crate::audio::interrupt::Interrupt>) -> Result<OpenedTrack, String> {
                Ok(OpenedTrack::radio(super::super::radio::Radio::open(song.radio_url.clone().unwrap(), None, interrupt.clone())))
            }
            fn upcoming(&self, _: &[Song]) {}
            fn now_playing(&self, _: &Song) {}
            fn played(&self, _: &Song, _: u64) {}
            fn save_queue(&self, _: &[Song], _: usize, _: u64) {}
        }
        let server = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/live", server.local_addr().unwrap());
        let worker = std::thread::spawn(move || {
            for connection in 0..2 {
                let (mut socket, _) = server.accept().unwrap();
                socket.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                socket.set_write_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    let mut byte = [0]; socket.read_exact(&mut byte).unwrap(); request.push(byte[0]);
                }
                assert!(String::from_utf8(request).unwrap().to_lowercase().contains("icy-metadata: 1"));
                let mut body = Vec::new();
                for (index, audio) in wav(8.0).chunks(32000).enumerate() {
                    body.extend_from_slice(audio);
                    if audio.len() == 32000 {
                        let title = if connection == 1 { "Reconnected" } else if index == 0 { "First" } else { "Second" };
                        let mut meta = format!("StreamTitle='Artist - {title}';").into_bytes();
                        let len = meta.len().div_ceil(16);
                        meta.resize(len * 16, 0);
                        body.push(len as u8);
                        body.extend(meta);
                    }
                }
                write!(socket, "HTTP/1.1 200 OK\r\nContent-Type: audio/wav\r\nicy-metaint: 32000\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                let _ = socket.write_all(&body);
            }
        });
        let p = spawn(Arc::new(Live), Box::new(SoftHw(FAKE.into())), Settings { eq: Default::default(), volume: 0, headphone_power: crate::config::HeadphonePower::Standard, filter: 0, replay_gain: ReplayGainMode::Off, gapless: true }, Arc::new(|| {}));
        let mut station = song("station"); station.radio_url = Some(url);
        p.send(Cmd::Play { songs: std::sync::Arc::new(vec![station]), index: 0, shuffle: false });
        for title in ["Artist - First", "Artist - Second"] {
            let status = wait_for(&p, 5, |s| s.radio_title.as_deref() == Some(title));
            assert_eq!(status.radio_title.as_deref(), Some(title), "{:?}", status.error);
            assert_eq!(status.current().unwrap().title, "station");
        }
        p.send(Cmd::Pause);
        let status = wait_for(&p, 2, |s| s.state == PlayState::Paused);
        assert_eq!(status.radio_title, None);
        p.send(Cmd::PlayPause);
        let status = wait_for(&p, 5, |s| s.radio_title.as_deref() == Some("Artist - Reconnected"));
        assert_eq!(status.radio_title.as_deref(), Some("Artist - Reconnected"));
        p.send(Cmd::Restore { songs: std::sync::Arc::new(vec![song("ordinary song")]), index: 0, position_ms: 0 });
        let status = wait_for(&p, 2, |s| s.current().is_some_and(|s| s.id == "ordinary song"));
        assert_eq!(status.radio_title, None);
        p.send(Cmd::Quit);
        worker.join().unwrap();
    }

    #[test]
    fn plays_through_the_queue() {
        let (rec, p) = start();
        p.send(Cmd::Play { songs: std::sync::Arc::new(vec![song("a"), song("b")]), index: 0, shuffle: false });
        let st = wait_for(&p, 3, |s| s.state == PlayState::Playing && s.position_ms > 300);
        assert_eq!(st.current().unwrap().id, "a");
        assert_eq!(st.format.as_deref(), Some("PCM 16/8"));
        let st = wait_for(&p, 6, |s| s.state == PlayState::Stopped);
        assert_eq!(st.state, PlayState::Stopped);
        assert_eq!(*rec.opened.lock().unwrap(), vec!["a", "b"]);
        assert_eq!(*rec.saved.lock().unwrap(), Some((2, 1)));
        p.send(Cmd::Quit);
    }

    #[test]
    fn skips_what_cannot_open_and_pauses() {
        let (rec, p) = start();
        p.send(Cmd::Play { songs: std::sync::Arc::new(vec![song("missing"), song("b")]), index: 0, shuffle: false });
        let st = wait_for(&p, 3, |s| s.state == PlayState::Playing);
        assert_eq!(st.current().unwrap().id, "b", "the missing song is skipped");
        p.send(Cmd::PlayPause);
        let st = wait_for(&p, 2, |s| s.state == PlayState::Paused);
        let at = st.position_ms;
        std::thread::sleep(Duration::from_millis(300));
        assert!(p.status().position_ms <= at + 50, "paused stays put");
        assert_eq!(rec.opened.lock().unwrap()[0], "missing");
        p.send(Cmd::Quit);
    }

    #[test]
    fn next_and_insert() {
        let (_rec, p) = start();
        p.send(Cmd::Play { songs: std::sync::Arc::new(vec![song("a"), song("b"), song("c")]), index: 0, shuffle: false });
        wait_for(&p, 3, |s| s.state == PlayState::Playing);
        p.send(Cmd::Insert { songs: vec![song("x")], next: true });
        p.send(Cmd::Next);
        let st = wait_for(&p, 3, |s| s.current().is_some_and(|c| c.id == "x"));
        let ids: Vec<&str> = st.queue.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["a", "x", "b", "c"]);
        p.send(Cmd::Quit);
    }

    #[test]
    fn restored_queue_next_advances_but_queue_end_needs_repeat_all() {
        let (_rec, p) = start();
        p.send(Cmd::Restore { songs: std::sync::Arc::new(vec![song("a")]), index: 0, position_ms: 500 });
        let restored = wait_for(&p, 2, |s| s.state == PlayState::Paused && s.position_ms == 500);
        for (repeat, barrier) in [(Repeat::Off, 37), (Repeat::One, 38)] {
            p.send(Cmd::SetRepeat(repeat)); p.send(Cmd::Next); p.send(Cmd::SetVolume(barrier));
            let end = wait_for(&p, 2, |s| s.volume == barrier);
            assert_eq!(end.generation, restored.generation, "a final-item Next is a no-op");
            assert_eq!(end.state, PlayState::Paused); assert_eq!(end.position_ms, 500);
        }
        p.send(Cmd::SetRepeat(Repeat::All)); p.send(Cmd::Next);
        let wrapped = wait_for(&p, 2, |s| s.generation != restored.generation && s.state == PlayState::Playing);
        assert_ne!(wrapped.generation, restored.generation); assert_eq!(wrapped.index, 0);
        p.send(Cmd::SetRepeat(Repeat::Off));
        p.send(Cmd::Restore { songs: std::sync::Arc::new(vec![song("a"), song("b")]), index: 0, position_ms: 500 });
        let restored = wait_for(&p, 2, |s| s.queue.len() == 2 && s.state == PlayState::Paused);
        p.send(Cmd::Next);
        let next = wait_for(&p, 2, |s| s.index == 1 && s.state == PlayState::Playing);
        assert_eq!(next.current().unwrap().id, "b");
        assert_ne!(next.generation, restored.generation);
        p.send(Cmd::Pause); let paused = wait_for(&p, 2, |s| s.state == PlayState::Paused);
        p.send(Cmd::Next); p.send(Cmd::SetVolume(39));
        let end = wait_for(&p, 2, |s| s.volume == 39);
        assert_eq!(end.index, 1); assert_eq!(end.generation, paused.generation);
        assert_eq!(end.state, PlayState::Paused);
        p.send(Cmd::Quit);
    }

    #[test]
    fn stale_scrub_cannot_seek_next_instance_of_same_song() {
        let (_, p) = start();
        p.send(Cmd::Restore { songs: std::sync::Arc::new(vec![song("a"), song("a")]), index: 0, position_ms: 0 });
        let old = wait_for(&p, 2, |s| s.state == PlayState::Paused);
        p.send(Cmd::Next);
        p.send(Cmd::Pause);
        p.send(Cmd::Scrub { generation: old.generation, position: Some(900), active: true });
        let new = wait_for(&p, 2, |s| s.generation != old.generation && s.state == PlayState::Paused);
        assert_eq!(new.index, 1);
        assert_eq!(new.position_ms, 0);
        assert_eq!(new.seek_revision, old.seek_revision);
        p.send(Cmd::Quit);
    }

    #[test]
    fn scrub_at_end_stays_on_track_and_paused_seek_stays_paused() {
        let (_, p) = start();
        p.send(Cmd::Play { songs: std::sync::Arc::new(vec![song("a"), song("b")]), index: 0, shuffle: false });
        let old = wait_for(&p, 2, |s| s.state == PlayState::Playing);
        p.send(Cmd::Scrub { generation: old.generation, position: Some(u64::MAX), active: true });
        std::thread::sleep(Duration::from_millis(1500));
        let end = p.status();
        assert_eq!(end.index, 0, "jogging must not advance at EOF");
        p.send(Cmd::Pause);
        p.send(Cmd::Scrub { generation: old.generation, position: Some(100), active: true });
        let paused = wait_for(&p, 2, |s| s.state == PlayState::Paused && s.seek_revision > end.seek_revision);
        assert_eq!(paused.state, PlayState::Paused);
        assert!(paused.position_ms < 300);
        p.send(Cmd::Scrub { generation: old.generation, position: None, active: false });
        p.send(Cmd::Quit);
    }

    #[test]
    fn all_seek_paths_clamp_inside_known_duration() {
        assert_eq!(seek_position(u64::MAX, 120000), 119000);
        assert_eq!(seek_position(100, 400), 0);
        assert_eq!(seek_position(4000, 0), 4000);
    }

    #[test]
    fn eight_minute_stream_seek_advances_and_paused_seek_stays_paused() {
        struct LongStream { bytes: Arc<Vec<u8>>, path: std::path::PathBuf }
        impl TrackSource for LongStream {
            fn open(&self, _blocking: &crate::rt::Blocking, _: &Song, interrupt: &Arc<crate::audio::interrupt::Interrupt>) -> Result<OpenedTrack, String> {
                let progress = super::super::source::Progress::new();
                progress.set_total(Some(self.bytes.len() as u64)); progress.advance(65536);
                let bytes = self.bytes.clone();
                let source = super::super::source::ProgressiveReader::open(_blocking, &self.path, progress, interrupt.clone()).unwrap()
                    .with_ranges(Arc::new(move |pos, size| {
                        std::thread::sleep(Duration::from_millis(100));
                        let start = pos as usize;
                        Ok(Some(bytes[start..(start + size).min(bytes.len())].to_vec()))
                    }));
                Ok(OpenedTrack { source:Box::new(source), ext:Some("wav".into()), origin:Origin::Streaming, radio_metadata:None })
            }
            fn upcoming(&self, _: &[Song]) {}
            fn now_playing(&self, _: &Song) {}
            fn played(&self, _: &Song, _: u64) {}
            fn save_queue(&self, _: &[Song], _: usize, _: u64) {}
        }
        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join(format!("../../build/perf/tests/long-seek-{}", fastrand::u64(..)));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("prefix.wav");
        let bytes = Arc::new(wav(600.0));
        std::fs::write(&path, &bytes[..65536]).unwrap();
        let settings = Settings { eq:Default::default(), volume:0, headphone_power: crate::config::HeadphonePower::Standard, filter:0, replay_gain:ReplayGainMode::Off, gapless:true };
        let player = spawn(Arc::new(LongStream { bytes, path }), Box::new(SoftHw(FAKE.into())), settings, Arc::new(|| {}));
        player.send(Cmd::Play { songs:std::sync::Arc::new(vec![Song { duration:600, ..song("long") }]), index:0, shuffle:false });
        let first = wait_for(&player, 2, |s| s.state == PlayState::Playing);
        assert_eq!(first.state, PlayState::Playing);
        player.send(Cmd::Scrub { generation:first.generation, position:Some(480_000), active:true });
        let sought = wait_for(&player, 3, |s| s.state == PlayState::Playing && s.position_ms > 480_100);
        assert!(sought.position_ms > 480_100, "{sought:?}");
        assert_eq!(sought.generation, first.generation);
        player.send(Cmd::Pause);
        wait_for(&player, 2, |s| s.state == PlayState::Paused);
        player.send(Cmd::Scrub { generation:first.generation, position:Some(200_000), active:true });
        let paused = wait_for(&player, 2, |s| s.seek_revision > sought.seek_revision);
        assert_eq!(paused.state, PlayState::Paused);
        assert_eq!(paused.position_ms, 200_000);
        assert!(paused.error.is_none());
        player.send(Cmd::Quit);
    }

    #[test]
    fn soft_volume_curve() {
        assert_eq!(soft_gain(0), 0.0);
        assert!((soft_gain(100) - 1.0).abs() < 1e-6);
        // 20 steps down is 10 dB.
        assert!((soft_gain(80) - 0.316).abs() < 0.01);
    }
    #[test]
    fn reselecting_same_radio_preserves_connection_and_pause_state() {
        let (rec, p) = start();
        let mut station = song("station");
        station.radio_url = Some("https://example.test/live".into());
        p.send(Cmd::Play { songs: std::sync::Arc::new(vec![station.clone()]), index: 0, shuffle: false });
        let first = wait_for(&p, 2, |s| s.state == PlayState::Playing && s.position_ms > 100);
        // The same URL can appear in Featured Stations and Navidrome with
        // different IDs/names. Neither identity nor playback should change.
        station.id = "another menu entry".into();
        station.title = "Station alias".into();
        assert!(first.is_current_radio(&station));
        p.send(Cmd::Play { songs: std::sync::Arc::new(vec![station.clone()]), index: 0, shuffle: false });
        p.send(Cmd::SetVolume(37)); // wait until the command batch is processed
        let selected = wait_for(&p, 2, |s| s.volume == 37);
        assert_eq!(selected.generation, first.generation);
        assert_eq!(selected.current().unwrap().id, "station");
        assert_eq!(rec.opened.lock().unwrap().len(), 1);
        p.send(Cmd::Pause);
        wait_for(&p, 2, |s| s.state == PlayState::Paused);
        p.send(Cmd::Play { songs: std::sync::Arc::new(vec![station.clone()]), index: 0, shuffle: false });
        p.send(Cmd::SetVolume(38));
        let paused = wait_for(&p, 2, |s| s.volume == 38);
        assert_eq!(paused.state, PlayState::Paused);
        assert_eq!(rec.opened.lock().unwrap().len(), 1);
        station.radio_url = Some("https://example.test/different".into());
        assert!(!paused.is_current_radio(&station));
        p.send(Cmd::Play { songs: std::sync::Arc::new(vec![station]), index: 0, shuffle: false });
        let changed = wait_for(&p, 2, |s| s.state == PlayState::Playing && s.generation != first.generation);
        assert_ne!(changed.generation, first.generation);
        assert_eq!(rec.opened.lock().unwrap().len(), 2);
        p.send(Cmd::Quit);
    }

    #[test]
    fn radio_failure_preserves_reason_and_retry_stays_on_station() {
        let (rec, p) = start();
        let mut station = song("missing");
        station.radio_url = Some("https://example.test/radio".into());
        p.send(Cmd::Play { songs: std::sync::Arc::new(vec![station, song("other")]), index: 0, shuffle: false });
        let failed = wait_for(&p, 2, |s| s.state == PlayState::Paused);
        assert_eq!(failed.error.as_deref(), Some("Not available offline"));
        assert_eq!(failed.index, 0);
        p.send(Cmd::PlayPause);
        let end = Instant::now() + Duration::from_secs(2);
        while rec.opened.lock().unwrap().len() < 2 && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(*rec.opened.lock().unwrap(), vec!["missing", "missing"]);
        assert_eq!(p.status().index, 0);
        p.send(Cmd::Quit);
    }

    #[test]
    fn radio_does_not_seek_repeat_or_advance_and_resume_reopens() {
        let (rec,p)=start();
        let mut station=song("station");station.radio_url=Some("https://example.test/radio".into());
        p.send(Cmd::SetRepeat(Repeat::One));
        p.send(Cmd::Play {songs:std::sync::Arc::new(vec![station,song("other")]),index:0,shuffle:false});
        let first=wait_for(&p,2,|s|s.state==PlayState::Playing && s.position_ms>100);
        p.send(Cmd::SeekBy(100000));
        let ended=wait_for(&p,3,|s|s.state==PlayState::Paused);
        assert_eq!(ended.state,PlayState::Paused);
        assert_eq!(ended.index,0);assert_eq!(ended.seek_revision,first.seek_revision);
        assert_eq!(rec.opened.lock().unwrap().len(),1);
        p.send(Cmd::PlayPause);
        wait_for(&p,2,|s|s.state==PlayState::Playing);
        assert_eq!(rec.opened.lock().unwrap().len(),2);
        p.send(Cmd::Quit);
    }

}
