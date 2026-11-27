//! Confined libasound ABI and owning PCM handle.

use libloading::Library;
use std::ffi::{CStr, CString, c_char, c_int, c_long, c_uint, c_ulong, c_void};
use std::sync::OnceLock;

type Pcm = *mut c_void;

const STREAM_PLAYBACK: c_int = 0;
const ACCESS_RW_INTERLEAVED: c_int = 3;
use super::pcm_format::PcmFormat;


struct Api {
    _lib: Library,
    open: unsafe extern "C" fn(*mut Pcm, *const c_char, c_int, c_int) -> c_int,
    set_params: unsafe extern "C" fn(Pcm, c_int, c_int, c_uint, c_uint, c_int, c_uint) -> c_int,
    writei: unsafe extern "C" fn(Pcm, *const c_void, c_ulong) -> c_long,
    recover: unsafe extern "C" fn(Pcm, c_int, c_int) -> c_int,
    xrun_carry: Option<unsafe extern "C" fn(Pcm, c_int) -> c_int>,
    delay: unsafe extern "C" fn(Pcm, *mut c_long) -> c_int,
    state: unsafe extern "C" fn(Pcm) -> c_int,
    drain: unsafe extern "C" fn(Pcm) -> c_int,
    drop: unsafe extern "C" fn(Pcm) -> c_int,
    prepare: unsafe extern "C" fn(Pcm) -> c_int,
    pause: unsafe extern "C" fn(Pcm, c_int) -> c_int,
    close: unsafe extern "C" fn(Pcm) -> c_int,
    strerror: unsafe extern "C" fn(c_int) -> *const c_char,
    get_params: unsafe extern "C" fn(Pcm, *mut c_ulong, *mut c_ulong) -> c_int,
    hw_params_malloc: unsafe extern "C" fn(*mut *mut c_void) -> c_int,
    hw_params_free: unsafe extern "C" fn(*mut c_void),
    hw_params_any: unsafe extern "C" fn(Pcm, *mut c_void) -> c_int,
    hw_params_set_resample: unsafe extern "C" fn(Pcm, *mut c_void, c_uint) -> c_int,
    hw_params_set_access: unsafe extern "C" fn(Pcm, *mut c_void, c_int) -> c_int,
    hw_params_set_format: unsafe extern "C" fn(Pcm, *mut c_void, c_int) -> c_int,
    hw_params_set_channels: unsafe extern "C" fn(Pcm, *mut c_void, c_uint) -> c_int,
    hw_params_set_rate: unsafe extern "C" fn(Pcm, *mut c_void, c_uint, c_int) -> c_int,
    hw_params_set_period: unsafe extern "C" fn(Pcm, *mut c_void, *mut c_ulong, *mut c_int) -> c_int,
    hw_params_set_buffer: unsafe extern "C" fn(Pcm, *mut c_void, *mut c_ulong) -> c_int,
    hw_params: unsafe extern "C" fn(Pcm, *mut c_void) -> c_int,
    sw_params_set_start: unsafe extern "C" fn(Pcm, *mut c_void, c_ulong) -> c_int,
    sw_params_malloc: unsafe extern "C" fn(*mut *mut c_void) -> c_int,
    sw_params_current: unsafe extern "C" fn(Pcm, *mut c_void) -> c_int,
    sw_params_set_avail_min: unsafe extern "C" fn(Pcm, *mut c_void, c_ulong) -> c_int,
    sw_params: unsafe extern "C" fn(Pcm, *mut c_void) -> c_int,
    sw_params_free: unsafe extern "C" fn(*mut c_void),
}

fn api(blocking: &crate::rt::Blocking) -> Result<&'static Api, String> {
    static API: OnceLock<Result<Api, String>> = OnceLock::new();
    API.get_or_init(|| load(blocking)).as_ref().map_err(Clone::clone)
}

fn load(_blocking: &crate::rt::Blocking) -> Result<Api, String> {
    let lib = unsafe { Library::new("libasound.so.2") }.map_err(|e| format!("libasound: {e}"))?;
    macro_rules! sym {
        ($name:literal) => {
            *unsafe { lib.get(concat!($name, "\0").as_bytes()) }.map_err(|e| format!("{}: {e}", $name))?
        };
    }
    Ok(Api {
        open: sym!("snd_pcm_open"),
        set_params: sym!("snd_pcm_set_params"),
        writei: sym!("snd_pcm_writei"),
        recover: sym!("snd_pcm_recover"),
        xrun_carry: unsafe { lib.get::<unsafe extern "C" fn(Pcm, c_int) -> c_int>(b"snd_pcm_q2_xrun_carry\0") }.ok().map(|symbol| *symbol),
        delay: sym!("snd_pcm_delay"),
        state: sym!("snd_pcm_state"),
        drain: sym!("snd_pcm_drain"),
        drop: sym!("snd_pcm_drop"),
        prepare: sym!("snd_pcm_prepare"),
        pause: sym!("snd_pcm_pause"),
        close: sym!("snd_pcm_close"),
        strerror: sym!("snd_strerror"),
        get_params: sym!("snd_pcm_get_params"),
        hw_params_malloc: sym!("snd_pcm_hw_params_malloc"),
        hw_params_free: sym!("snd_pcm_hw_params_free"),
        hw_params_any: sym!("snd_pcm_hw_params_any"),
        hw_params_set_resample: sym!("snd_pcm_hw_params_set_rate_resample"),
        hw_params_set_access: sym!("snd_pcm_hw_params_set_access"),
        hw_params_set_format: sym!("snd_pcm_hw_params_set_format"),
        hw_params_set_channels: sym!("snd_pcm_hw_params_set_channels"),
        hw_params_set_rate: sym!("snd_pcm_hw_params_set_rate"),
        hw_params_set_period: sym!("snd_pcm_hw_params_set_period_size_near"),
        hw_params_set_buffer: sym!("snd_pcm_hw_params_set_buffer_size_near"),
        hw_params: sym!("snd_pcm_hw_params"),
        sw_params_set_start: sym!("snd_pcm_sw_params_set_start_threshold"),
        sw_params_malloc: sym!("snd_pcm_sw_params_malloc"),
        sw_params_current: sym!("snd_pcm_sw_params_current"),
        sw_params_set_avail_min: sym!("snd_pcm_sw_params_set_avail_min"),
        sw_params: sym!("snd_pcm_sw_params"),
        sw_params_free: sym!("snd_pcm_sw_params_free"),
        _lib: lib,
    })
}

fn err(_blocking: &crate::rt::Blocking, api: &Api, code: c_int) -> String {
    crate::diagnostics::health::audio_error(code);
    if [-libc::EBADF,-libc::EBADFD].contains(&code) {
        return "Audio output needs to reconnect. Press Play to retry.".into();
    }
    unsafe { CStr::from_ptr((api.strerror)(code)) }.to_string_lossy().into_owned()
}

/// Owns one PCM and its immutable negotiated format. No raw pointer escapes.
/// Calls are serialized by the player. Shared references cannot cross threads.
pub(super) struct Handle<'a> {
    blocking: &'a crate::rt::Blocking,
    api: &'static Api,
    pcm: Pcm,
    rate: u32,
    channels: u32,
    format:PcmFormat,
    _not_sync: std::marker::PhantomData<std::cell::Cell<()>>,
}
// The retained player-stack witness prevents moving the PCM to another thread.

impl<'a> Handle<'a> {
    pub(super) fn open(blocking: &'a crate::rt::Blocking, device: &str, rate: u32, channels: u32, latency_us: u32, converted: bool, format:PcmFormat) -> Result<Self, String> {
        if rate == 0 || channels == 0 { return Err("Invalid PCM format".into()); }
        let api = api(blocking)?;
        let name = CString::new(device).map_err(|e| e.to_string())?;
        let mut pcm = std::ptr::null_mut();
        let rc = unsafe { (api.open)(&mut pcm, name.as_ptr(), STREAM_PLAYBACK, 0) };
        if rc < 0 { return Err(format!("open {device}: {}", err(blocking, api, rc))); }
        if pcm.is_null() { return Err("ALSA returned a null PCM".into()); }
        let handle = Self { blocking, api, pcm, rate, channels, format, _not_sync: std::marker::PhantomData };
        let configured = if converted { handle.converted_params(blocking, latency_us) } else {
            let rc = unsafe { (api.set_params)(pcm, format.alsa(), ACCESS_RW_INTERLEAVED, channels, rate, 1, latency_us) };
            if rc < 0 { Err(err(blocking, api, rc)) } else { Ok(()) }
        };
        configured.map_err(|error| format!("{device} at {rate} Hz, {channels} ch: {error}"))?;
        Ok(handle)
    }

    pub(super) fn capacity(&self, _blocking: &crate::rt::Blocking) -> Result<u64, String> {
        let (mut buffer, mut period) = (0, 0);
        let rc = unsafe { (self.api.get_params)(self.pcm, &mut buffer, &mut period) };
        if rc < 0 || buffer == 0 { return Err("Unable to determine PCM retention capacity".into()); }
        Ok(buffer as u64)
    }

    /// Frames are derived from this handle's format, never a caller-supplied
    /// count. ALSA borrows the slice only for the duration of this call.
    pub(super) fn write(&self, _blocking: &crate::rt::Blocking, samples: &[i32]) -> i64 {
        if self.format!=PcmFormat::S32 {return -i64::from(libc::EINVAL);}
        let channels = self.channels as usize;
        if samples.len() % channels != 0 { return -i64::from(libc::EINVAL); }
        let Ok(frames) = c_ulong::try_from(samples.len() / channels) else { return -i64::from(libc::EOVERFLOW); };
        unsafe { (self.api.writei)(self.pcm, samples.as_ptr().cast(), frames) as i64 }
    }
    pub(super) fn write_s16(&self,_blocking:&crate::rt::Blocking,samples:&[i16])->i64 {
        if self.format!=PcmFormat::S16 || samples.len()%self.channels as usize!=0 {return -i64::from(libc::EINVAL);}
        let Ok(frames)=c_ulong::try_from(samples.len()/self.channels as usize) else {return -i64::from(libc::EOVERFLOW);};
        unsafe {(self.api.writei)(self.pcm,samples.as_ptr().cast(),frames) as i64}
    }
    pub(super) fn recover(&self, _blocking: &crate::rt::Blocking, code: c_int) -> c_int { unsafe { (self.api.recover)(self.pcm, code, 1) } }
    pub(super) fn error(&self, blocking: &crate::rt::Blocking, code: c_int) -> String { err(blocking, self.api, code) }
    pub(super) fn delay(&self, blocking: &crate::rt::Blocking) -> Result<u64, String> {
        let mut frames = 0;
        let rc = unsafe { (self.api.delay)(self.pcm, &mut frames) };
        if rc < 0 { Err(self.error(blocking, rc)) } else { Ok(frames.max(0) as u64) }
    }
    pub(super) fn state(&self, _blocking: &crate::rt::Blocking) -> c_int { unsafe { (self.api.state)(self.pcm) } }
    pub(super) fn pause(&self, _blocking: &crate::rt::Blocking, on: bool) -> c_int { unsafe { (self.api.pause)(self.pcm, i32::from(on)) } }
    pub(super) fn reset_step(&self, _blocking: &crate::rt::Blocking, prepare: bool) -> c_int {
        unsafe { if prepare { (self.api.prepare)(self.pcm) } else { (self.api.drop)(self.pcm) } }
    }
    pub(super) fn drain(&self, _blocking: &crate::rt::Blocking) { unsafe { (self.api.drain)(self.pcm); } }
    pub(super) fn enable_xrun_carry(&self, blocking: &crate::rt::Blocking) -> Result<bool, String> {
        let Some(enable) = self.api.xrun_carry else { return Ok(false); };
        match unsafe { enable(self.pcm, 1) } {
            0 => Ok(true),
            code if code == -libc::EOPNOTSUPP || code == -libc::ENOTTY => Ok(false),
            code => Err(format!("USB audio recovery: {}", self.error(blocking, code))),
        }
    }
    /// Negotiate converted streams in integer client frames. The libasound
    /// convenience setup constrains period *time* after buffer time, which can
    /// leave a non-singleton period interval for 192k->96k and fail get_period_size.
    /// Explicit frame sizes resolve that constraint without changing source rate
    /// or substituting an arbitrary latency/rate cap. Native raw setup is untouched.
    fn converted_params(&self, blocking: &crate::rt::Blocking, latency_us: u32) -> Result<(), String> {
        let api = self.api;
        let check = |operation: &str, rc: c_int| {
            if rc < 0 { Err(format!("{operation}: {}", err(blocking, api, rc))) } else { Ok(()) }
        };
        let mut hw = std::ptr::null_mut();
        check("allocate hardware parameters", unsafe { (api.hw_params_malloc)(&mut hw) })?;
        let configured: Result<(), String> = (|| {
            check("hardware capabilities", unsafe { (api.hw_params_any)(self.pcm, hw) })?;
            check("software resampling", unsafe { (api.hw_params_set_resample)(self.pcm, hw, 1) })?;
            check("interleaved access", unsafe { (api.hw_params_set_access)(self.pcm, hw, ACCESS_RW_INTERLEAVED) })?;
            check("PCM format", unsafe { (api.hw_params_set_format)(self.pcm, hw, self.format.alsa()) })?;
            check("source channels", unsafe { (api.hw_params_set_channels)(self.pcm, hw, self.channels) })?;
            check("exact source rate", unsafe { (api.hw_params_set_rate)(self.pcm, hw, self.rate, 0) })?;
            let mut period = (u64::from(self.rate) * u64::from(latency_us) / 4_000_000)
                .clamp(1, c_ulong::MAX as u64) as c_ulong;
            check("client period frames", unsafe { (api.hw_params_set_period)(self.pcm, hw, &mut period, std::ptr::null_mut()) })?;
            let mut buffer = period.saturating_mul(4);
            check("client buffer frames", unsafe { (api.hw_params_set_buffer)(self.pcm, hw, &mut buffer) })?;
            check("apply hardware parameters", unsafe { (api.hw_params)(self.pcm, hw) })?;
            Ok(())
        })();
        unsafe { (api.hw_params_free)(hw) };
        configured?;
        let (mut buffer, mut period) = (0, 0);
        check("negotiated frame sizes", unsafe { (api.get_params)(self.pcm, &mut buffer, &mut period) })?;
        if buffer == 0 || period == 0 { return Err("Invalid negotiated buffer/period".into()); }
        let mut sw = std::ptr::null_mut();
        check("allocate software parameters", unsafe { (api.sw_params_malloc)(&mut sw) })?;
        let configured: Result<(), String> = (|| {
            check("software parameters", unsafe { (api.sw_params_current)(self.pcm, sw) })?;
            check("start threshold", unsafe { (api.sw_params_set_start)(self.pcm, sw, (buffer / period) * period) })?;
            check("period availability", unsafe { (api.sw_params_set_avail_min)(self.pcm, sw, period) })?;
            check("apply software parameters", unsafe { (api.sw_params)(self.pcm, sw) })?;
            check("prepare converted PCM", unsafe { (api.prepare)(self.pcm) })
        })();
        unsafe { (api.sw_params_free)(sw) };
        configured
    }

    /// The Q2's DMA periods are 512 frames (11.6 ms), and a blocking write
    /// returned whenever one period was free: about 48 player wakeups a
    /// second. Waking only once a quarter of the buffer is free decodes
    /// several packets per wakeup and still keeps three quarters queued.
    pub(super) fn batch_writes(&self, blocking: &crate::rt::Blocking) -> Result<(), String> {
        let api = self.api;
        let (mut buffer, mut period): (c_ulong, c_ulong) = (0, 0);
        let r = unsafe { (api.get_params)(self.pcm, &mut buffer, &mut period) };
        if r < 0 { return Err(err(blocking, api, r)); }
        let avail = buffer / 4;
        if avail <= period { return Ok(()); }
        let mut sw: *mut c_void = std::ptr::null_mut();
        let r = unsafe { (api.sw_params_malloc)(&mut sw) };
        if r < 0 { return Err(err(blocking, api, r)); }
        let r = unsafe {
            let r = (api.sw_params_current)(self.pcm, sw);
            let r = if r < 0 { r } else { (api.sw_params_set_avail_min)(self.pcm, sw, avail) };
            let r = if r < 0 { r } else { (api.sw_params)(self.pcm, sw) };
            (api.sw_params_free)(sw);
            r
        };
        if r < 0 { Err(err(blocking, api, r)) } else { Ok(()) }
    }

    fn close(&mut self, _blocking: &crate::rt::Blocking) { unsafe { (self.api.close)(self.pcm); } }
}
impl Drop for Handle<'_> {
    fn drop(&mut self) { self.close(self.blocking); }
}

#[cfg(test)]
#[path = "pcm_ffi_fixture.rs"]
pub(crate) mod fixture;
