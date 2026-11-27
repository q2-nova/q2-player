// Deterministic PCM state model. Production methods are inserted by the harness.
use std::cell::Cell;
use std::ffi::{c_int, c_long};
use std::time::{Duration, Instant};
const FIXED: bool = /* FIXED */;
struct PcmState { queued: u64, can_pause: bool, paused: bool }
type Pcm = *mut PcmState;
struct Api {
    pause: unsafe fn(Pcm, c_int) -> c_int,
    drop: unsafe fn(Pcm) -> c_int,
    prepare: unsafe fn(Pcm) -> c_int,
    delay: unsafe fn(Pcm, *mut c_long) -> c_int,
}
unsafe fn pause(pcm: Pcm, on: c_int) -> c_int {
    let state = unsafe { &mut *pcm };
    if !state.can_pause { return -38; }
    state.paused = on != 0;
    0
}
unsafe fn discard(pcm: Pcm) -> c_int {
    unsafe { (*pcm).queued = 0; (*pcm).paused = false; }
    0
}
unsafe fn prepare(_: Pcm) -> c_int { 0 }
unsafe fn delay(pcm: Pcm, frames: *mut c_long) -> c_int {
    unsafe { *frames = (*pcm).queued as c_long; }
    0
}
static API: Api = Api {pause, drop: discard, prepare, delay};
struct Sink { api: Option<&'static Api>, pcm: Pcm, rate: u32, fake_until: Option<Instant> }
impl Sink {
    /* ACTUAL SINK */
}
impl Drop for Sink {
    fn drop(&mut self) { if !self.pcm.is_null() { unsafe { drop(Box::from_raw(self.pcm)); } } }
}
struct Hw;
impl Hw { fn mute(&mut self, _: bool) {} fn power_off(&mut self) {} }
struct Dec { rate: u32 }
enum PlayState { Playing, Paused }
struct Engine {
    sink: Option<Sink>, hw: Hw, dec: Option<Dec>, resume_at: u64,
    base_ms: u64, frames: u64, state: PlayState, idle_since: Instant,
    saved: Cell<u64>, radio: bool,
}
impl Engine {
    fn is_radio(&self) -> bool { self.radio }
    fn close_track(&mut self) { self.dec = None; }
    fn save(&self) { self.saved.set(self.position_ms()); }
    fn seek(&mut self, ms: u64) { self.seek_at(seek_position(ms, 1200)); }
    fn seek_at(&mut self, ms: u64) { self.base_ms = ms; self.frames = 0; }
    fn restore_branch(&mut self, duration_ms: u64) {
        let _ = duration_ms;
        /* ACTUAL RESUME BRANCH */
    }
    /* ACTUAL ENGINE */
}
/* ACTUAL SEEK LIMIT */
fn engine(can_pause: bool) -> Engine {
    let pcm = Box::into_raw(Box::new(PcmState { queued: 9600, can_pause, paused: false }));
    Engine {sink: Some(Sink {api: Some(&API), pcm, rate: 48000, fake_until: None}),
        hw: Hw, dec: Some(Dec {rate: 48000}), resume_at: 0, base_ms: 1000,
        frames: 48000, state: PlayState::Playing, idle_since: Instant::now(),
        saved: Cell::new(0), radio: false}
}
fn main() {
    let mut release = engine(true);
    assert_eq!(release.position_ms(), 1800);
    release.release_output();
    assert_eq!(release.resume_at, if FIXED {1800} else {2000});
    assert_eq!(release.position_ms(), release.resume_at);
    assert!(release.sink.is_none() && release.dec.is_none());

    let mut unsupported = engine(false);
    unsupported.pause();
    assert_eq!(unsupported.saved.get(), if FIXED {1800} else {2000});
    assert_eq!(unsupported.dec.is_none(), FIXED);
    assert_eq!(unsupported.sink.as_ref().unwrap().delay(), 0);
    unsupported.release_output();
    assert_eq!(unsupported.resume_at, if FIXED {1800} else {2000});

    let mut supported = engine(true);
    supported.pause();
    assert_eq!(supported.saved.get(), 1800);
    assert!(supported.dec.is_some());
    assert_eq!(supported.sink.as_ref().unwrap().delay(), 9600);
    supported.release_output();
    assert_eq!(supported.resume_at, if FIXED {1800} else {2000});

    // The fake sink follows the same discard path; no clock timing is asserted.
    let mut fake = engine(true);
    fake.sink = Some(Sink {api: None, pcm: std::ptr::null_mut(), rate: 48000,
        fake_until: Some(Instant::now() + Duration::from_secs(1))});
    fake.pause();
    assert_eq!(fake.dec.is_none(), FIXED);
    assert_eq!(fake.sink.as_ref().unwrap().delay(), 0);

    let mut radio = engine(true);
    radio.radio = true;
    radio.pause();
    assert_eq!(radio.saved.get(), 0);
    assert!(radio.sink.is_none() && radio.dec.is_none());

    let mut tail = engine(true);
    tail.seek(900);
    assert_eq!(tail.base_ms, 200);
    tail.resume_at = 900;
    tail.restore_branch(1200);
    assert_eq!(tail.base_ms, if FIXED {900} else {200});
    tail.resume_at = 5000;
    tail.restore_branch(1200);
    assert_eq!(tail.base_ms, if FIXED {1199} else {200});
    tail.resume_at = 900;
    tail.restore_branch(0);
    assert_eq!(tail.base_ms, if FIXED {900} else {200});
    assert_eq!(seek_position(900, 1200), 200); // User seek margin unchanged.

    if FIXED {
        println!("PASS actual routines: audible release, unsupported/supported pause, radio reset, fake fallback and exact final-second resume");
    } else {
        println!("REPRO actual baseline: 200ms release/pause skips; 900ms resume rewinds to 200ms seek margin");
    }
}
