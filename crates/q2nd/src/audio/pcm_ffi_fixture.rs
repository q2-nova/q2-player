//! Typed mock backend; unsafe ABI callbacks stay at the same boundary as production.
use super::*;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy)]
pub(crate) enum Step {
    Accept(usize),
    Error { code: c_int, consume_before: usize },
}

pub(crate) struct RawPcm {
    pub(crate) format:PcmFormat,
    pub(crate) channels: usize,
    pub(crate) expected: Vec<i32>,
    pub(crate) accepted: Vec<i32>,
    pub(crate) queued: Vec<i32>,
    pub(crate) emitted: Vec<i32>,
    pub(crate) discarded_by_recover: usize,
    pub(crate) discarded_payloads: Vec<Vec<i32>>,
    pub(crate) delay_history: Vec<(usize, usize)>,
    pub(crate) steps: VecDeque<Step>,
    pub(crate) recovery_codes: VecDeque<c_int>,
    pub(crate) requests: Vec<usize>,
    pub(crate) recoveries: Vec<(c_int, c_int)>,
    pub(crate) delay_calls: usize,
    pub(crate) closes: usize,
    pub(crate) carry_options: Vec<c_int>,
    pub(crate) carry_code: c_int,
    pub(crate) violation: Option<&'static str>,
}

unsafe fn raw<'a>(pcm: Pcm) -> &'a Mutex<RawPcm> {
    unsafe { &*(pcm.cast::<Arc<Mutex<RawPcm>>>()) }
}

unsafe extern "C" fn writei(pcm: Pcm, data: *const c_void, frames: c_ulong) -> c_long {
    let mut state = unsafe { raw(pcm) }.lock().unwrap();
    let frames = frames as usize;
    state.requests.push(frames);
    let count = frames * state.channels;
    let offset = state.accepted.len();
    let narrow;
    let samples=if state.format==PcmFormat::S16 {
        narrow=unsafe {std::slice::from_raw_parts(data.cast::<i16>(),count)}.iter().map(|&v|i32::from(v)<<16).collect::<Vec<_>>();
        narrow.as_slice()
    }else{unsafe {std::slice::from_raw_parts(data.cast::<i32>(),count)}};
    if offset + count > state.expected.len() || samples != &state.expected[offset..offset + count] {
        state.violation = Some("actual write pointer/request lost the rendered suffix");
        return -libc::EINVAL as c_long;
    }
    match state.steps.pop_front().unwrap_or(Step::Accept(frames)) {
        Step::Accept(count) => {
            // Deliberate zero/oversized API returns exercise actual admission;
            // an invalid positive return does not copy outside the input.
            if count <= frames {
                let end = count * state.channels;
                state.accepted.extend_from_slice(&samples[..end]);
                state.queued.extend_from_slice(&samples[..end]);
            }
            count as c_long
        }
        Step::Error { code, consume_before } => {
            let count = consume_before.min(state.queued.len() / state.channels) * state.channels;
            let emitted: Vec<_> = state.queued.drain(..count).collect();
            state.emitted.extend(emitted);
            code as c_long
        }
    }
}

unsafe extern "C" fn recover(pcm: Pcm, code: c_int, silent: c_int) -> c_int {
    let mut state = unsafe { raw(pcm) }.lock().unwrap();
    state.recoveries.push((code, silent));
    let result = state.recovery_codes.pop_front().unwrap_or(0);
    let carries = state.carry_code == 0 && state.carry_options.last() == Some(&1);
    if result >= 0 && code == -libc::EPIPE && !carries {
        state.discarded_by_recover += state.queued.len() / state.channels;
        let discarded = state.queued.clone();
        state.discarded_payloads.push(discarded);
        state.queued.clear();
    }
    result
}

unsafe extern "C" fn xrun_carry(pcm: Pcm, enable: c_int) -> c_int {
    let mut state = unsafe { raw(pcm) }.lock().unwrap();
    state.carry_options.push(enable);
    state.carry_code
}

unsafe extern "C" fn delay(pcm: Pcm, frames: *mut c_long) -> c_int {
    let mut state = unsafe { raw(pcm) }.lock().unwrap();
    state.delay_calls += 1;
    let accepted = state.accepted.len() / state.channels;
    let queued = state.queued.len() / state.channels;
    state.delay_history.push((accepted, queued));
    unsafe { *frames = queued as c_long; }
    0
}
unsafe extern "C" fn running(_: Pcm) -> c_int { 3 }
unsafe extern "C" fn drain(pcm: Pcm) -> c_int {
    let mut state = unsafe { raw(pcm) }.lock().unwrap();
    let queued = std::mem::take(&mut state.queued);
    state.emitted.extend(queued);
    0
}
unsafe extern "C" fn close(pcm: Pcm) -> c_int {
    let owned = unsafe { Box::from_raw(pcm.cast::<Arc<Mutex<RawPcm>>>()) };
    owned.lock().unwrap().closes += 1;
    0
}
unsafe extern "C" fn message(_: c_int) -> *const c_char { c"generated PCM error".as_ptr() }

// Unused slots are typed refusals, never transmuted or real ALSA entry points.
unsafe extern "C" fn open_no(_: *mut Pcm, _: *const c_char, _: c_int, _: c_int) -> c_int { -libc::ENOSYS }
unsafe extern "C" fn params_no(_: Pcm, _: c_int, _: c_int, _: c_uint, _: c_uint, _: c_int, _: c_uint) -> c_int { -libc::ENOSYS }
unsafe extern "C" fn one_no(_: Pcm) -> c_int { -libc::ENOSYS }
unsafe extern "C" fn pause_no(_: Pcm, _: c_int) -> c_int { -libc::ENOSYS }
unsafe extern "C" fn get_no(_: Pcm, _: *mut c_ulong, _: *mut c_ulong) -> c_int { -libc::ENOSYS }
unsafe extern "C" fn alloc_no(_: *mut *mut c_void) -> c_int { -libc::ENOSYS }
unsafe extern "C" fn free_no(_: *mut c_void) {}
unsafe extern "C" fn two_no(_: Pcm, _: *mut c_void) -> c_int { -libc::ENOSYS }
unsafe extern "C" fn uint_no(_: Pcm, _: *mut c_void, _: c_uint) -> c_int { -libc::ENOSYS }
unsafe extern "C" fn int_no(_: Pcm, _: *mut c_void, _: c_int) -> c_int { -libc::ENOSYS }
unsafe extern "C" fn rate_no(_: Pcm, _: *mut c_void, _: c_uint, _: c_int) -> c_int { -libc::ENOSYS }
unsafe extern "C" fn period_no(_: Pcm, _: *mut c_void, _: *mut c_ulong, _: *mut c_int) -> c_int { -libc::ENOSYS }
unsafe extern "C" fn buffer_no(_: Pcm, _: *mut c_void, _: *mut c_ulong) -> c_int { -libc::ENOSYS }
unsafe extern "C" fn ulong_no(_: Pcm, _: *mut c_void, _: c_ulong) -> c_int { -libc::ENOSYS }

fn fixture_api_with_carry(include: bool) -> &'static Api {
    static PRESENT: OnceLock<Api> = OnceLock::new();
    static ABSENT: OnceLock<Api> = OnceLock::new();
    (if include { &PRESENT } else { &ABSENT }).get_or_init(|| Api {
        _lib: libloading::os::unix::Library::this().into(),
        open: open_no, set_params: params_no, writei, recover, xrun_carry: if include { Some(xrun_carry) } else { None }, delay, state: running,
        drain, drop: one_no, prepare: one_no, pause: pause_no, close, strerror: message,
        get_params: get_no, hw_params_malloc: alloc_no, hw_params_free: free_no,
        hw_params_any: two_no, hw_params_set_resample: uint_no,
        hw_params_set_access: int_no, hw_params_set_format: int_no,
        hw_params_set_channels: uint_no, hw_params_set_rate: rate_no,
        hw_params_set_period: period_no, hw_params_set_buffer: buffer_no,
        hw_params: two_no, sw_params_set_start: ulong_no, sw_params_malloc: alloc_no,
        sw_params_current: two_no, sw_params_set_avail_min: ulong_no,
        sw_params: two_no, sw_params_free: free_no,
    })
}

pub(crate) fn handle<'a>(blocking: &'a crate::rt::Blocking, pcm: &[i32], steps: Vec<Step>, recovery_codes: Vec<c_int>, carry: bool) -> (Handle<'a>, Arc<Mutex<RawPcm>>) {
    handle_format(blocking,pcm,steps,recovery_codes,carry,PcmFormat::S32)
}
pub(crate) fn handle_format<'a>(blocking:&'a crate::rt::Blocking,pcm:&[i32],steps:Vec<Step>,recovery_codes:Vec<c_int>,carry:bool,format:PcmFormat)->(Handle<'a>,Arc<Mutex<RawPcm>>) {
    let state = Arc::new(Mutex::new(RawPcm {
        format, channels: 2, expected: pcm.to_vec(), accepted: vec![], queued: vec![], emitted: vec![],
        discarded_by_recover: 0, discarded_payloads: vec![], delay_history: vec![], steps: steps.into(), recovery_codes: recovery_codes.into(),
        requests: vec![], recoveries: vec![], delay_calls: 0, closes: 0, carry_options: vec![], carry_code: 0, violation: None,
    }));
    let handle = Handle { blocking, api: fixture_api_with_carry(carry), pcm: Box::into_raw(Box::new(state.clone())).cast(), rate: 48000, channels: 2, format, _not_sync: std::marker::PhantomData };
    (handle, state)
}
