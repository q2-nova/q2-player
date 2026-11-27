//! A playback PCM through libasound, loaded at run time: the Q2's ALSA 1.1.6
//! and a desktop's both work, and neither build needs ALSA headers.

#![forbid(unsafe_code)]
use std::ffi::c_int;
use super::pcm_backend::Handle;
use super::pcm_format::{PcmFormat,Quantizer};
const STATE_PREPARED: c_int = 2;
const STATE_XRUN: c_int = 4;
/// The Q2's internal codec playback PCM (the headphone route on mainline).
pub const INTERNAL_CODEC_PCM: &str = "hw:Q2,0";
#[cfg(test)]
pub(crate) const STATE_XRUN_FOR_TESTS: c_int = STATE_XRUN;
const STATE_SUSPENDED: c_int = 7;
fn stopped_transport(state: Option<c_int>) -> bool {
    matches!(state, Some(STATE_PREPARED | STATE_XRUN | STATE_SUSPENDED))
}

/// The device name of a sink that plays nothing, in real time (headless runs
/// and tests).
pub const FAKE: &str = "q2nd-fake";

/// A failed write can already have transferred frames. The rendered remainder
/// still belongs to the caller; an output change must not decode it again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteError {
    pub accepted_frames: u64,
    pub message: String,
}

#[cfg(test)]
pub struct WriteFixture {
    pub steps: std::collections::VecDeque<Result<usize, String>>,
    pub accepted: std::sync::Arc<std::sync::Mutex<Vec<i32>>>,
    pub queued: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub drained: std::sync::Arc<std::sync::atomic::AtomicBool>,
    pub consumed_on_error:std::collections::VecDeque<usize>,
    pub on_error:Option<std::sync::Arc<dyn Fn()+Send+Sync>>,
    pub first_write: Option<Box<dyn FnOnce() + Send>>,
    pub queue_limit:Option<usize>,
}

fn write_frames(
    samples: &[i32], channels: usize,
    mut write: impl FnMut(&[i32]) -> Result<usize, String>,
    mut accepted: impl FnMut(&[i32]),
) -> Result<(), WriteError> {
    if channels==0 || samples.len()%channels!=0 {
        return Err(WriteError {accepted_frames:0,message:"Unaligned PCM frames".into()});
    }
    let mut offset=0;
    while offset<samples.len() {
        let fail=|message|WriteError {accepted_frames:(offset/channels) as u64,message};
        let frames=write(&samples[offset..]).map_err(fail)?;
        if frames==0 {return Err(fail("PCM write made no progress".into()));}
        if frames>(samples.len()-offset)/channels {
            return Err(fail("PCM returned an invalid frame count".into()));
        }
        let end=offset+frames*channels;
        accepted(&samples[offset..end]);
        offset=end;
    }
    Ok(())
}

/// Recoveries kept per write for the playback trace; more are only counted.
const RECOVERIES_KEPT:usize=4;

/// One PCM write under ALSA's recovery contract: a negative return is
/// reported to `note`, recovered, and the write repeated; a failed recovery
/// is the error. The accepted frame count is the first non-negative return.
fn write_recovering(
    mut write:impl FnMut()->i64,mut recover:impl FnMut(c_int)->c_int,mut note:impl FnMut(c_int),
)->Result<usize,c_int> {
    for attempt in 0..=4 {
        let n=write();
        if n>=0 {return Ok(n as usize);}
        note(n as c_int);
        if attempt==4 {return Err(n as c_int);}
        let r=recover(n as c_int);
        if r<0 {return Err(r);}
    }
    unreachable!("bounded recovery either writes or reports its last failure")
}

/// An open playback PCM. DSP/retention use i32; transport is explicitly S16/S32.
pub struct Sink<'a> {
    _blocking: &'a crate::rt::Blocking,
    /// Frames queued when the latest write began (playtrace).
    pre_write_delay: Option<u64>,
    /// Recoveries inside writes since the player last took them (playtrace).
    recovered: Vec<crate::playtrace::Recovered>,
    recovered_dropped: u32,
    #[cfg(test)]
    recovery_script: std::collections::VecDeque<c_int>,
    #[cfg(test)]
    underrun_code: Option<c_int>,
    /// The fake queue holds audio written since it was last emptied on purpose.
    #[cfg(test)]
    fake_started: bool,
    #[cfg(test)]
    pub(crate) written_samples:Vec<i32>,
    tail: super::retained::Tail,
    output_gain: f64,
    shadow_capacity: u64,
    #[cfg(test)]
    fake_delay_error: bool,
    paused: Option<(super::retained::RetainedPcm, bool)>,
    pause_fault: Option<String>,
    prepared_queue_unverified:bool,
    /// The internal codec PCM (`hw:Q2,0`, stop threshold the buffer size):
    /// an XRUN there means the device played everything queued, so its
    /// unheard queue is empty even though the delay query fails with EPIPE.
    /// USB and Bluetooth outputs keep an XRUN's queue unknown.
    xrun_empties_queue:bool,
    pcm: Option<Handle<'a>>,
    pub format:PcmFormat,
    quantizer:Quantizer,
    transport:Vec<i16>,
    pub rate: u32,
    pub channels: u32,
    /// The fake sink's clock: when its queue empties.
    fake_until: Option<std::time::Instant>,
    #[cfg(test)]
    fake_delay_frames: Option<u64>,
    #[cfg(test)]
    fake_capture: Option<Vec<i32>>,
    #[cfg(test)]
    write_fixture: Option<WriteFixture>,
    #[cfg(test)]
    pause_code: Option<c_int>,
    #[cfg(test)]
    pcm_state_fixture: Option<(c_int,std::sync::Arc<std::sync::atomic::AtomicUsize>)>,
    #[cfg(test)]
    reset_codes: std::collections::VecDeque<c_int>,
    #[cfg(test)]
    reset_calls:usize,
    /// Frames the fake PCM accepted, shared with a test that watches for
    /// audio written when none may be.
    #[cfg(test)]
    frame_counter: Option<std::sync::Arc<std::sync::atomic::AtomicUsize>>,
}

/// The mainline Q2 BAIC/DAI contract is stereo at these exact rates.
/// ALSA's plug layer converts other source formats and reports delay in the
/// caller's frame/rate units. Other devices negotiate their own capabilities.
fn format_device(device:&str,rate:u32,channels:u32)->&str {
    if device=="hw:Q2,0" && (channels!=2 || ![44100,48000,96000].contains(&rate)) {
        "plughw:Q2,0"
    }else{device}
}

impl<'a> Sink<'a> {
    /// Explicit same-image USB recovery capability. Older libraries/kernels
    /// retain their ordinary recovery path; operational admission errors remain
    /// errors. This does not query a live PCM pointer or replay the RAM shadow.
    pub(crate) fn enable_xrun_carry(&mut self, blocking: &crate::rt::Blocking) -> Result<bool, String> {
        self.pcm.as_ref().map_or(Ok(false), |pcm| pcm.enable_xrun_carry(blocking))
    }

    /// Opens `device` (e.g. `plughw:0,0`, `default`, `bluealsa`) for S32
    /// frames at `rate`; `plug` layers convert to what the hardware takes.
    pub fn open(blocking: &'a crate::rt::Blocking, device: &str, rate: u32, channels: u32, latency_us: u32) -> Result<Sink<'a>, String> {
        Self::open_format(blocking,device,rate,channels,latency_us,PcmFormat::S32)
    }
    pub fn open_format(blocking:&'a crate::rt::Blocking,device:&str,rate:u32,channels:u32,latency_us:u32,format:PcmFormat)->Result<Sink<'a>,String> {
        if device == FAKE {
            let now = std::time::Instant::now();
            return Ok(Sink { format, quantizer:Quantizer::default(),transport:Vec::new(), _blocking: blocking, pre_write_delay: None, recovered: Vec::new(), recovered_dropped: 0, #[cfg(test)] recovery_script: Default::default(), #[cfg(test)] underrun_code: None, #[cfg(test)] fake_started: false, #[cfg(test)] written_samples:Vec::new(), tail: Default::default(), output_gain: 1.0, shadow_capacity: u64::from(rate)/5, #[cfg(test)] fake_delay_error: false, paused: None, pause_fault: None, prepared_queue_unverified:false, xrun_empties_queue:false, pcm: None, rate, channels, fake_until: Some(now), #[cfg(test)] fake_delay_frames: None, #[cfg(test)] fake_capture: None, #[cfg(test)] write_fixture: None, #[cfg(test)] pause_code: None, #[cfg(test)] pcm_state_fixture: None, #[cfg(test)] reset_codes: Default::default(), #[cfg(test)] reset_calls:0, #[cfg(test)] frame_counter: None });
        }
        let requested=device;
        let device=format_device(device,rate,channels);
        if device!=requested {log::info!("Q2 audio conversion: {rate} Hz, {channels} channels through {device}");}
        let pcm = Handle::open(blocking, device, rate, channels, latency_us, device != requested,format)?;
        let mut sink = Sink { format, quantizer:Quantizer::default(),transport:Vec::new(), _blocking: blocking, pre_write_delay: None, recovered: Vec::new(), recovered_dropped: 0, #[cfg(test)] recovery_script: Default::default(), #[cfg(test)] underrun_code: None, #[cfg(test)] fake_started: false, #[cfg(test)] written_samples:Vec::new(), tail: Default::default(), output_gain: 1.0, shadow_capacity: u64::from(rate)/5, #[cfg(test)] fake_delay_error: false, paused: None, pause_fault: None, prepared_queue_unverified:false, xrun_empties_queue:false, pcm: Some(pcm), rate, channels, fake_until: None, #[cfg(test)] fake_delay_frames: None, #[cfg(test)] fake_capture: None, #[cfg(test)] write_fixture: None, #[cfg(test)] pause_code: None, #[cfg(test)] pcm_state_fixture: None, #[cfg(test)] reset_codes: Default::default(), #[cfg(test)] reset_calls:0, #[cfg(test)] frame_counter: None };
        let pcm = sink.pcm.as_ref().unwrap();
        sink.shadow_capacity = pcm.capacity(blocking)?;
        if let Err(e) = pcm.batch_writes(blocking) {
            log::warn!("{device}: unbatched writes: {e}");
        }
        Ok(sink)
    }

    #[cfg(test)]
    pub fn write(&mut self, blocking: &crate::rt::Blocking, samples: &[i32]) -> Result<(), String> {
        self.write_progress(blocking, samples).map_err(|error|error.message)
    }

    /// Writes complete interleaved frames, reporting any accepted prefix even
    /// if the endpoint disappears partway through the packet.
    pub fn write_progress(&mut self, blocking: &crate::rt::Blocking, samples: &[i32]) -> Result<(), WriteError> {
        self.write_rendered(blocking, samples, None)
    }

    /// Preserve the original audible values through every accepted partial
    /// write. The unaccepted suffix remains owned by the caller.
    pub fn write_retained(&mut self, blocking: &crate::rt::Blocking, held: &super::retained::RetainedPcm) -> Result<(), WriteError> {
        if held.format!=self.format || held.output_gain() != self.output_gain || (held.rate,held.channels) != (self.rate,self.channels) {
            return Err(WriteError { accepted_frames: 0, message: "PCM gain/format domain mismatch".into() });
        }
        self.write_rendered(blocking, &held.samples, held.audible())
    }

    fn retained_tail(&self,frames:u64)->Option<super::retained::RetainedPcm> {
        self.tail.retained(frames,self.rate,self.channels,self.output_gain).map(|held|held.with_format(self.format))
    }

    /// Configure a newly opened empty sink. A live gain change needs an
    /// explicit retained-queue transaction, not relabeling its existing PCM.
    pub fn configure_output_gain(&mut self, gain: f64) -> Result<(), String> {
        if !gain.is_finite() || !(0.0..=1.0).contains(&gain) || self.tail.frames(self.channels) != 0 || self.paused.is_some() {
            return Err("Output gain requires an empty PCM".into());
        }
        self.output_gain = gain;
        Ok(())
    }

    pub fn output_gain(&self)->f64 {self.output_gain}

    /// A verified hardware control affects the existing running PCM as well
    /// as future writes. A paused snapshot instead keeps its audible ceiling
    /// and is re-encoded before replay, including after a mute/down/up sequence.
    pub fn change_output_gain(&mut self,blocking:&crate::rt::Blocking,gain:f64)->Result<(),String> {
        if !gain.is_finite() || !(0.0..=1.0).contains(&gain) {return Err("Invalid PCM output gain".into());}
        if gain==self.output_gain {return Ok(());}
        if let Some((mut held,hardware))=self.paused.take() {
            if let Err(error)=held.rebase_output(gain) {
                self.paused=Some((held,hardware));
                return Err(error);
            }
            let fault=self.pause_fault.clone();
            if !self.prepared_queue_unverified {self.flush(blocking);}
            self.paused=Some((held,false));self.pause_fault=fault;
        }
        self.output_gain=gain;
        Ok(())
    }

    pub fn retarget_running_gain(&mut self,blocking:&crate::rt::Blocking,old_soft:f32,new_soft:f32,gain:f64)->Result<(),String> {
        let Some((mut held,hardware))=self.paused.take() else {return Err("Headroom change needs retained PCM".into());};
        if let Err(error)=held.retarget_running(old_soft,new_soft,gain) {
            self.paused=Some((held,hardware));return Err(error);
        }
        let fault=self.pause_fault.clone();
        if !self.prepared_queue_unverified {self.flush(blocking);}
        self.paused=Some((held,false));self.pause_fault=fault;
        self.output_gain=gain;
        Ok(())
    }

    fn write_rendered(&mut self, blocking: &crate::rt::Blocking, samples: &[i32], audible: Option<&[f64]>) -> Result<(), WriteError> {
        debug_assert!(audible.is_none_or(|values|values.len()==samples.len()));
        let output_gain = self.output_gain;
        let mut accepted_samples = 0;
        let ch = self.channels as usize;
        if ch==0 || samples.len()%ch!=0 {
            return Err(WriteError {accepted_frames:0,message:"Unaligned PCM frames".into()});
        }
        let before=self.observed_delay(blocking);
        self.pre_write_delay=before.as_ref().ok().copied();
        if let Ok(delay)=before {self.tail.retain(delay,self.channels);}
        #[cfg(test)]
        {
            // Generated recoveries: scripted per write, or the fake queue's
            // own underrun (it ran empty after audio had been queued).
            let ran_dry=self.underrun_code.filter(|_|self.pre_write_delay==Some(0) && self.fake_started && self.pcm.is_none() && !samples.is_empty());
            if let Some(code)=self.recovery_script.pop_front().or(ran_dry) {
                self.note_recovery(crate::playtrace::boottime_us(),code);
            }
        }
        #[cfg(test)]
        if let Some(mut fixture)=self.write_fixture.take() {
            let result=write_frames(samples,ch,
                |remaining| {
                    // `usize::MAX` accepts the whole write.
                    let result=fixture.steps.pop_front().unwrap_or(Ok(usize::MAX)).map(|n|if n==usize::MAX {remaining.len()/ch}else{n});
                    if result.is_err() {
                        let consumed=fixture.consumed_on_error.pop_front().unwrap_or(0);
                        let _=fixture.queued.fetch_update(std::sync::atomic::Ordering::Relaxed,
                            std::sync::atomic::Ordering::Relaxed,|queued|Some(queued.saturating_sub(consumed)));
                        if let Some(on_error)=fixture.on_error.take() {on_error();}
                    }
                    result
                },
                |part| {
                    fixture.accepted.lock().unwrap().extend_from_slice(part);
                    let mut queued=fixture.queued.fetch_add(part.len()/ch,std::sync::atomic::Ordering::Relaxed)+part.len()/ch;
                    if let Some(limit)=fixture.queue_limit {
                        queued=queued.min(limit);
                        fixture.queued.store(queued,std::sync::atomic::Ordering::Relaxed);
                    }
                    self.written_samples.extend_from_slice(part);
                    let end = accepted_samples + part.len();
                    self.tail.append_output(part, output_gain, audible.map(|values|&values[accepted_samples..end]));
                    accepted_samples = end;
                    self.tail.retain(queued as u64,self.channels);
                });
            if accepted_samples > 0 {
                if let Some(observe) = fixture.first_write.take() { observe(); }
            }
            self.write_fixture=Some(fixture);
            if result.is_ok() && !samples.is_empty() {self.pause_fault=None;}
            return result;
        }
        let Some(pcm) = self.pcm.as_ref() else {
            #[cfg(test)]
            if let Some(capture) = &mut self.fake_capture {
                capture.extend_from_slice(samples);
                if !samples.is_empty() {self.pause_fault=None;}
                return Ok(());
            }
            // Queue the frames' duration, and block while over the latency.
            let secs = (samples.len() / ch) as f64 / self.rate as f64;
            let now = std::time::Instant::now();
            let until = self.fake_until.unwrap_or(now).max(now) + std::time::Duration::from_secs_f64(secs);
            self.fake_until = Some(until);
            let ahead = until.saturating_duration_since(now);
            if ahead > std::time::Duration::from_millis(200) {
                std::thread::sleep(ahead - std::time::Duration::from_millis(200));
            }
            #[cfg(test)]
            {self.written_samples.extend_from_slice(samples);self.fake_started|=!samples.is_empty();}
            #[cfg(test)]
            if let Some(counter)=&self.frame_counter {counter.fetch_add(samples.len()/ch,std::sync::atomic::Ordering::Relaxed);}
            self.record_written(blocking, samples, audible);
            if !samples.is_empty() {self.pause_fault=None;}
            return Ok(());
        };
        if self.format==PcmFormat::S16 {self.quantizer.render(samples,&mut self.transport);}
        let mut recovered=[crate::playtrace::Recovered::default();RECOVERIES_KEPT];
        let mut recoveries=0usize;
        let result=write_frames(samples,ch,|remaining| {
            write_recovering(
                ||if self.format==PcmFormat::S16 {
                    pcm.write_s16(blocking,&self.transport[samples.len()-remaining.len()..])
                }else{pcm.write(blocking,remaining)},
                |code|pcm.recover(blocking, code),
                |code| {
                    crate::diagnostics::health::audio_error(-code);
                    // One clock read: the log line and the trace event carry the same instant.
                    let t_us=crate::playtrace::boottime_us();
                    log::warn!("ALSA_WRITE_RECOVERY code={code} t={}.{:03}",t_us/1_000_000,t_us/1000%1000);
                    if recoveries<RECOVERIES_KEPT {recovered[recoveries]=crate::playtrace::Recovered {t_us,code};}
                    recoveries+=1;
                }).map_err(|code|pcm.error(blocking, code))
        },|accepted| {
            #[cfg(test)]
            self.written_samples.extend_from_slice(accepted);
            let end = accepted_samples + accepted.len();
            self.tail.append_output(accepted, output_gain, audible.map(|values|&values[accepted_samples..end]));
            accepted_samples = end;
            let delay = self.paused.as_ref().map(|(held, _)| Ok(held.frames())).unwrap_or_else(|| pcm.delay(blocking));
            let keep = match delay {
                Ok(delay) => { self.shadow_capacity = self.shadow_capacity.max(delay); delay }
                Err(_) => self.shadow_capacity,
            };
            self.tail.retain(keep, self.channels);
        });
        for recovery in &recovered[..recoveries.min(RECOVERIES_KEPT)] {self.note_recovery(recovery.t_us,recovery.code);}
        self.recovered_dropped=self.recovered_dropped.saturating_add(recoveries.saturating_sub(RECOVERIES_KEPT) as u32);
        if result.is_ok() && !samples.is_empty() {self.pause_fault=None;}
        result
    }

    fn note_recovery(&mut self,t_us:u64,code:c_int) {
        if self.recovered.len()<RECOVERIES_KEPT {self.recovered.push(crate::playtrace::Recovered {t_us,code});}
        else {self.recovered_dropped=self.recovered_dropped.saturating_add(1);}
    }

    /// Frames queued when the latest write began; None when the query failed.
    pub(crate) fn pre_write_delay(&self)->Option<u64> {self.pre_write_delay}

    /// Recoveries since the last `clear_recovered`, and how many more than
    /// the kept few there were.
    pub(crate) fn recovered(&self)->(&[crate::playtrace::Recovered],u32) {(&self.recovered,self.recovered_dropped)}

    pub(crate) fn clear_recovered(&mut self) {self.recovered.clear();self.recovered_dropped=0;}

    /// Generated recoveries, one per following write.
    #[cfg(test)]
    pub(crate) fn script_recoveries(&mut self,codes:impl IntoIterator<Item=c_int>) {self.recovery_script.extend(codes);}

    /// The fake PCM reports an underrun (as a recovery with `code`) when a
    /// write finds its queue empty after audio had been queued.
    #[cfg(test)]
    pub(crate) fn model_underruns(&mut self,code:c_int) {self.underrun_code=Some(code);}

    fn record_written(&mut self, blocking: &crate::rt::Blocking, samples: &[i32], audible: Option<&[f64]>) {
        self.tail.append_output(samples, self.output_gain, audible);
        let keep = match self.observed_delay(blocking) {
            Ok(delay) => {
                // Plug conversion can report delay beyond the PCM buffer.
                self.shadow_capacity = self.shadow_capacity.max(delay);
                delay
            }
            Err(_) => self.shadow_capacity,
        };
        // A persistent delay-query failure must not grow an unbounded shadow
        // while writes still succeed. Retain the negotiated/observed queue.
        self.tail.retain(keep, self.channels);
    }

    /// Frames written but not yet heard.
    pub fn delay(&self, blocking: &crate::rt::Blocking) -> u64 {
        self.observed_delay(blocking).unwrap_or_else(|_|self.tail.frames(self.channels))
    }

    /// Failed delay queries do not establish that the queue was heard. Keep
    /// its last retained tail for device-loss recovery instead of erasing it.
    pub fn observed_delay(&self, blocking: &crate::rt::Blocking)->Result<u64,String> {
        if let Some((held,_))=&self.paused {return Ok(held.frames());}
        self.transport_delay(blocking)
    }

    /// Whether an XRUN leaves nothing unheard on the PCM opened for `route`
    /// at `device` (see `xrun_empties_queue`).
    pub fn xrun_empties_queue_for(route:&super::output::Route,device:&str)->bool {
        matches!(route,super::output::Route::Headphones) && device==INTERNAL_CODEC_PCM
    }
    pub fn set_xrun_empties_queue(&mut self,on:bool) {self.xrun_empties_queue=on;}

    fn transport_delay(&self, blocking: &crate::rt::Blocking)->Result<u64,String> {
        let delay=self.transport_delay_reported(blocking);
        // An underrun on the internal codec played the whole queue.
        if delay.is_err() && self.xrun_empties_queue && self.transport_state(blocking)==Some(STATE_XRUN) {return Ok(0);}
        delay
    }

    fn transport_delay_reported(&self, blocking: &crate::rt::Blocking)->Result<u64,String> {
        #[cfg(test)]
        if let Some(fixture)=&self.write_fixture {
            return Ok(fixture.queued.load(std::sync::atomic::Ordering::Relaxed) as u64);
        }
        #[cfg(test)]
        if self.fake_delay_error {return Err("Generated delay failure".into());}
        #[cfg(test)]
        if let Some(frames)=self.fake_delay_frames {return Ok(frames);}
        let Some(pcm)=self.pcm.as_ref() else {
            let left=self.fake_until.map(|u|u.saturating_duration_since(std::time::Instant::now())).unwrap_or_default();
            return Ok((left.as_secs_f64()*self.rate as f64) as u64);
        };
        pcm.delay(blocking)
    }

    /// Unpaced owned PCM capture, exclusively for sample-transform tests.
    #[cfg(test)]
    pub(crate) fn capture_fake_pcm(&mut self) {
        assert!(self.pcm.is_none());
        self.fake_capture = Some(Vec::new());
    }

    #[cfg(test)]
    pub(crate) fn captured_fake_pcm(&self) -> &[i32] {
        self.fake_capture.as_deref().expect("PCM capture was enabled")
    }

    #[cfg(test)]
    pub(crate) fn set_fake_delay(&mut self, frames: u64) {
        assert!(self.pcm.is_none());
        self.fake_delay_frames = Some(frames);
    }

    #[cfg(test)]
    pub(crate) fn fail_fake_delay(&mut self, failed: bool) {
        assert!(self.pcm.is_none());
        self.fake_delay_error = failed;
    }

    /// Counts every frame the paced fake PCM accepts into `counter`.
    #[cfg(test)]
    pub(crate) fn count_frames(&mut self, counter: std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        assert!(self.pcm.is_none());
        self.frame_counter = Some(counter);
    }

    #[cfg(test)]
    pub(crate) fn script_writes(&mut self, fixture:WriteFixture) {
        assert!(self.pcm.is_none());
        self.write_fixture=Some(fixture);
    }

    #[cfg(test)]
    pub(crate) fn script_pause_code(&mut self,code:c_int) {
        assert!(self.pcm.is_none());self.pause_code=Some(code);
    }

    #[cfg(test)]
    pub(crate) fn script_pcm_state(&mut self,state:c_int,calls:std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        assert!(self.pcm.is_none());self.pcm_state_fixture=Some((state,calls));
    }

    fn pause_transport(&self, blocking: &crate::rt::Blocking,on:bool)->c_int {
        #[cfg(test)]
        if let Some((_,calls))=&self.pcm_state_fixture {calls.fetch_add(1,std::sync::atomic::Ordering::Relaxed);}
        #[cfg(test)]
        if let Some(code)=self.pause_code {return code;}
        self.pcm.as_ref().map(|pcm|pcm.pause(blocking, on)).unwrap_or(-libc::ENOSYS)
    }

    /// State observation does not request HWSYNC. The hw backend reads its
    /// mapped status (or SYNC_PTR with APPL|AVAIL_MIN); plug/rate delegate state.
    /// Only this worker writes/prepares this PCM, so a PREPARED queue cannot
    /// start between this observation and the software ownership transfer.
    fn transport_state(&self, blocking: &crate::rt::Blocking)->Option<c_int> {
        #[cfg(test)]
        if let Some((state,_))=&self.pcm_state_fixture {return Some(*state);}
        self.pcm.as_ref().map(|pcm| pcm.state(blocking))
    }

    fn transport_error(&self,blocking: &crate::rt::Blocking,code:c_int)->String {
        self.pcm.as_ref().map(|pcm|pcm.error(blocking, code))
            .unwrap_or_else(||std::io::Error::from_raw_os_error(-code).to_string())
    }

    /// Discard the backend queue only after the caller has saved its PCM.
    /// Keep that copy and refuse admission if either operation fails. In
    /// particular, a prepare failure after successful drop must not lose it.
    fn reset_transport(&mut self, blocking: &crate::rt::Blocking)->Result<(),String> {
        for prepare in [false,true] {
            #[cfg(test)]
            {self.reset_calls+=1;}
            #[cfg(test)]
            let scripted=self.reset_codes.pop_front();
            #[cfg(not(test))]
            let scripted:Option<c_int>=None;
            let code=scripted.unwrap_or_else(||self.pcm.as_ref().map(|pcm|pcm.reset_step(blocking, prepare)).unwrap_or(0));
            if code<0 {return Err(format!("{}: {}",if prepare {"prepare PCM"}else{"drop PCM"},self.transport_error(blocking, code)));}
        }
        self.tail.clear();self.prepared_queue_unverified=false;
        #[cfg(test)]
        if let Some(fixture)=&self.write_fixture {fixture.queued.store(0,std::sync::atomic::Ordering::Relaxed);}
        #[cfg(test)]
        if self.fake_delay_frames.is_some() {self.fake_delay_frames=Some(0);}
        self.fake_until=self.pcm.is_none().then(std::time::Instant::now);
        #[cfg(test)]
        {self.fake_started=false;}
        Ok(())
    }

    fn pause_stopped(&mut self, blocking: &crate::rt::Blocking,delay:Result<u64,String>)->Result<bool,String> {
        // PREPARED/XRUN/SUSPENDED have no running transport: hardware pause
        // is invalid. Preserve the verified unheard tail before preparing again.
        // Its accepted but unstarted client frames are still unheard. Require
        // a successful delay and a complete shadow before admitting the pause.
        let exact=delay.as_ref().ok().and_then(|frames|self.retained_tail(*frames));
        let complete=exact.is_some();
        let held=exact.unwrap_or_else(||self.retained_tail(self.tail.frames(self.channels)).unwrap());
        // Do not destroy backend data when complete ownership is unproven.
        // PREPARED cannot run; keep both it and the conservative shadow until
        // an explicit resume can establish a complete queue, or a seek/Stop.
        let ownership=match delay {
            Err(error)=>Err(format!("Stopped audio delay: {error}")),
            Ok(_) if !complete=>Err("Unable to retain stopped audio".into()),
            Ok(_)=>Ok(()),
        };
        self.prepared_queue_unverified=ownership.is_err();
        let result=ownership.and_then(|_|self.reset_transport(blocking)).map(|_|true);
        self.paused=Some((held,false));
        if let Err(error)=&result {self.pause_fault=Some(error.clone());}
        result
    }

    /// Retain the unheard tail even when the PCM cannot pause itself.
    /// Operational failures remain errors even if a conservative software
    /// snapshot can be kept; unsupported hardware pause is a separate case.
    pub fn pause(&mut self, blocking: &crate::rt::Blocking, on: bool) -> Result<bool,String> {
        if !on { return self.resume(blocking).map(|_|true); }
        let ceiling = self.tail.running_volume_ceiling();
        let already_paused = self.paused.is_some();
        let result = self.pause_running(blocking);
        if !already_paused {
            if let Some((highest, target)) = ceiling {
                self.lower_paused_gain(blocking, highest, target);
            }
        }
        result
    }

    fn pause_running(&mut self, blocking: &crate::rt::Blocking) -> Result<bool, String> {
        if let Some(error)=&self.pause_fault {return Err(error.clone());}
        if self.paused.is_some() { return Ok(true); }
        let observed=self.observed_delay(blocking);
        let state=self.transport_state(blocking);
        if stopped_transport(state) {
            if state!=Some(STATE_PREPARED) {crate::diagnostics::health::audio_error(if state==Some(STATE_XRUN) {libc::EPIPE}else{libc::ESTRPIPE});}
            return self.pause_stopped(blocking, observed);
        }
        let before=observed.unwrap_or_else(|_|self.tail.frames(self.channels));
        let code=self.pause_transport(blocking, true);
        // DMA can underrun between observing RUNNING and issuing pause. A
        // second state/delay observation belongs to this failed operation;
        // do not treat EBADFD itself as proof that a queue is safe to discard.
        if code<0 && stopped_transport(self.transport_state(blocking)) {
            crate::diagnostics::health::audio_error(-code);
            return self.pause_stopped(blocking, self.observed_delay(blocking));
        }
        let hardware=code>=0;
        let frames=if hardware {self.delay(blocking)} else {before};
        let saved=self.retained_tail(frames);
        if !hardware {self.flush(blocking);}
        let retained=if let Some(held)=saved {
            self.paused=Some((held,hardware));
            true
        } else {self.flush(blocking);false};
        if code<0 && ![-libc::ENOSYS,-libc::EINVAL,-libc::EOPNOTSUPP].contains(&code) {
            let message=self.pcm.as_ref().map(|pcm|pcm.error(blocking, code))
                .unwrap_or_else(||std::io::Error::from_raw_os_error(-code).to_string());
            self.pause_fault=Some(message.clone());
            return Err(message);
        }
        Ok(retained)
    }

    /// This is ownership metadata only: no query of a dropped/failed device.
    pub fn retains_paused_pcm(&self)->bool {self.paused.is_some()}
    /// Already-owned paused frames only: no delay/state query or PCM copy.
    pub(crate) fn retained_frames_snapshot(&self)->Option<u64> {
        self.paused.as_ref().map(|(held,_)|held.frames())
    }

    /// Saved queue metadata, without an I/O query on a failed device.
    pub fn paused_frames(&self)->u64 {self.paused.as_ref().map_or(0,|(held,_)|held.frames())}

    /// App-owned admission metadata only, without querying the PCM. The kind
    /// records the successful pause API/transfer, not independent device power:
    /// 0=no saved queue, 1=hardware-pause result, 2=owned software, 3=unverified.
    pub fn pause_ownership(&self)->(u8,u64) {
        self.paused.as_ref().map_or((0,0),|(held,hardware)|
            (if self.prepared_queue_unverified {3}else if *hardware {1}else{2},held.frames()))
    }

    /// A successful explicit retry may still be empty. Its old retained frame
    /// count is not output progress and must not clear the caller's fault.
    pub fn pause_fault_pending(&self)->bool {self.pause_fault.is_some()}

    /// Automatic output parking must not destroy an unverified backend queue.
    pub fn pause_transfer_unverified(&self)->bool {self.prepared_queue_unverified}

    /// Establish an unverified paused queue from a new delay observation,
    /// trimming the shadow to exactly what the device still holds. Nothing
    /// is dropped from the device; on failure the queue stays unverified.
    pub fn revalidate_paused(&mut self, blocking: &crate::rt::Blocking)->Result<(),String> {
        if !self.prepared_queue_unverified {return Ok(());}
        let verified:Result<(),String>=(|| {
            let frames=self.transport_delay(blocking)?;
            if !stopped_transport(self.transport_state(blocking)) {return Err("Stopped audio changed state; retry playback".into());}
            let held=&mut self.paused.as_mut().ok_or("Missing PREPARED queue shadow")?.0;
            let count=usize::try_from(frames).ok().and_then(|n|n.checked_mul(self.channels as usize))
                .filter(|count|*count<=held.samples.len()).ok_or("Incomplete PREPARED PCM retention")?;
            held.discard_prefix(held.samples.len()-count);
            Ok(())
        })();
        if let Err(error)=verified {self.pause_fault=Some(error.clone());return Err(error);}
        self.prepared_queue_unverified=false;
        Ok(())
    }

    pub fn resume(&mut self, blocking: &crate::rt::Blocking)->Result<(),String> {
        // A new delay observation belongs only to this explicit retry.
        // Revalidate before dropping any of the still-PREPARED queue.
        self.revalidate_paused(blocking)?;
        let Some((mut held,hardware))=self.paused.take() else {return Ok(());};
        let previous_fault=self.pause_fault.clone();
        if hardware && self.pcm.as_ref().is_some_and(|pcm| pcm.pause(blocking, false)>=0) {
            // Successful hardware resume preserves queued frames. prepare here
            // would discard them and skip audio despite a successful resume.
            self.pause_fault=None;
            return Ok(());
        }
        if let Err(error)=self.reset_transport(blocking) {
            self.paused=Some((held,false));self.pause_fault=Some(error.clone());return Err(error);
        }
        self.pause_fault=None;
        if let Err(error)=self.write_retained(blocking, &held) {
            let accepted=error.accepted_frames as usize*self.channels as usize;
            // A replay can itself be partially accepted and heard before loss.
            // Keep only its queued tail plus the unsubmitted original suffix.
            // Unknown delay conservatively retains the whole accepted prefix.
            // After reset this queue contains only a suffix of this replay's
            // accepted prefix. Trim the original block by that count so its
            // provenance and lowered-volume ceiling survive intact. If delay
            // ownership is unknown, conservatively retain the whole prefix.
            let queued=self.retained(blocking).map_or(accepted,|queued|queued.samples.len());
            held.discard_prefix(accepted.saturating_sub(queued));
            self.flush(blocking);self.paused=Some((held,false));
            self.pause_fault=Some(error.message.clone());return Err(error.message);
        }
        // Replaying an empty snapshot has not established output recovery.
        if held.samples.is_empty() {self.pause_fault=previous_fault;}
        Ok(())
    }

    /// Transfer an already credited tail to a newly opened, same-format PCM.
    /// A failed replay remains owned here with its updated unheard suffix.
    pub fn replay(&mut self, blocking: &crate::rt::Blocking,held:super::retained::RetainedPcm)->Result<(),String> {
        let matches = held.format==self.format && held.output_gain() == self.output_gain && (held.rate,held.channels) == (self.rate,self.channels);
        self.paused=Some((held,false));
        if !matches {
            let error = "PCM replay gain/format domain mismatch".to_string();
            self.pause_fault = Some(error.clone());
            return Err(error);
        }
        self.resume(blocking)
    }

    pub fn retained(&self, blocking: &crate::rt::Blocking)->Option<super::retained::RetainedPcm> {
        if let Some((held,_))=&self.paused {return Some(held.clone());}
        self.retained_tail(self.delay(blocking))
    }
    pub fn required_output_gain(&self,blocking:&crate::rt::Blocking)->Option<f64> {
        if let Some((held,_))=&self.paused {return Some(held.required_output_gain());}
        self.tail.required_output_gain(self.delay(blocking),self.channels,self.output_gain)
    }

    pub(crate) fn defer_running_volume(&mut self, previous: f32, target: f32) {
        self.tail.defer_running_volume(previous, target);
    }

    pub(crate) fn settled_running_volume(&mut self) {
        self.tail.settled_running_volume();
    }

    pub(crate) fn has_unsettled_running_volume(&self) -> bool {
        self.tail.running_volume_ceiling().is_some()
    }

    pub(crate) fn limit_unheard_volume(&self, pending: &mut super::retained::RetainedPcm) {
        if let Some((highest, target)) = self.tail.running_volume_ceiling() {
            pending.lower_gain(highest, target);
        }
    }

    pub(crate) fn scale_running_volume_ceiling(&mut self, target: f32) {
        self.tail.scale_running_volume_ceiling(target);
    }

    pub fn lower_paused_gain(&mut self, blocking: &crate::rt::Blocking, old:f32,new:f32) {
        if new>=old {return;}
        if let Some((mut held,_))=self.paused.take() {
            let fault=self.pause_fault.clone();
            held.lower_gain(old,new);
            if !self.prepared_queue_unverified {self.flush(blocking);}
            self.paused=Some((held,false));self.pause_fault=fault;
        }
    }

    /// Discards queued frames (for seeks and skips).
    pub fn flush(&mut self, blocking: &crate::rt::Blocking) {
        #[cfg(test)]
        if let Some(fixture)=&self.write_fixture {
            fixture.queued.store(0,std::sync::atomic::Ordering::Relaxed);
        }
        self.paused=None;self.pause_fault=None;self.prepared_queue_unverified=false;self.tail.clear();
        #[cfg(test)]
        {self.fake_started=false;}
        #[cfg(test)]
        if self.fake_delay_frames.is_some() { self.fake_delay_frames = Some(0); }
        match self.pcm.as_ref() {
            Some(pcm) => { pcm.reset_step(blocking, false); pcm.reset_step(blocking, true); },
            None => self.fake_until = Some(std::time::Instant::now()),
        }
    }

    /// Plays out what is queued.
    pub fn drain(&mut self, blocking: &crate::rt::Blocking) {
        #[cfg(test)]
        if let Some(fixture)=&self.write_fixture {
            fixture.queued.store(0,std::sync::atomic::Ordering::Relaxed);
            fixture.drained.store(true,std::sync::atomic::Ordering::Release);
        }
        #[cfg(test)]
        {self.fake_started=false;}
        match self.pcm.as_ref() {
            Some(pcm) => pcm.drain(blocking),
            None => std::thread::sleep(self.fake_until.map(|u| u.saturating_duration_since(std::time::Instant::now())).unwrap_or_default()),
        }
    }
}

impl Drop for Sink<'_> {
    fn drop(&mut self) {
        // Close the PCM before releasing retained samples and other state.
        drop(self.pcm.take());
    }
}

/// The DAC and amplifier controls around the PCM. The Q2 drives its CS43131
/// through ioctls; the simulator has nothing to drive.
///
/// The player thread owns the value and is its only caller. The order it
/// keeps on the headphone route: `power_on`, `open_sink`, `mute(false)`,
/// writes, `mute(true)`, PCM closed, `power_off`. Every method must be safe
/// to repeat (the player mutes and powers off again on error paths) and
/// none returns an error: a control that fails is logged by the
/// implementation and playback goes on.
pub trait AudioHw: Send {
    /// The ALSA device to open for the current output.
    fn device(&mut self) -> String;
    /// Open the selected PCM. Kept at the hardware boundary so generated
    /// backends can exercise the actual player worker with partial/fatal writes.
    ///
    /// The player passes a rate in 1..=768 000 Hz and 1..=8 channels
    /// (`q2_machines::output::Params::valid`; anything else is refused
    /// before this call) and `latency_us` > 0. On `Ok` the sink takes i32
    /// source samples and writes the requested transport width; on `Err` nothing is open.
    fn open_sink<'b>(&mut self, blocking: &'b crate::rt::Blocking,device:&str,rate:u32,channels:u32,latency_us:u32,format:PcmFormat)->Result<Sink<'b>,String> {
        Sink::open_format(blocking,device,rate,channels,latency_us,format)
    }
    /// Powers the output up before the PCM opens.
    fn power_on(&mut self, blocking: &crate::rt::Blocking);
    /// Powers it down after the PCM closes.
    fn power_off(&mut self, blocking: &crate::rt::Blocking);
    /// Mutes or unmutes the analogue output. Called with `true` before the
    /// PCM closes or pauses and with `false` once it runs on the headphone
    /// route (an external DAC keeps the internal output muted).
    fn mute(&mut self, blocking: &crate::rt::Blocking, on: bool);
    /// Sets half-dB attenuation (0=unity, 255=mute); false when the caller must
    /// scale samples instead. Errors refuse output; they are never a software fallback.
    fn set_attenuation(&mut self, blocking: &crate::rt::Blocking, steps: u8) -> Result<bool, String>;
    /// Sets the headphone amplifiers' operating point and returns the level
    /// they now hold. The player calls it only while no headphone PCM is
    /// open: the codec changes its full scale only with its HP stage
    /// powered down (kernel patch 0187 refuses otherwise). Outputs without
    /// the internal codecs hold the level asked for.
    fn set_headphone_power(&mut self, _blocking: &crate::rt::Blocking, level: crate::config::HeadphonePower)
        -> Result<crate::config::HeadphonePower, String> {
        Ok(level)
    }
    /// Selects the DAC's digital filter by its index in Settings; an index
    /// the DAC does not have is ignored by the implementation.
    fn set_filter(&mut self, blocking: &crate::rt::Blocking, filter: u8);
}

/// Software volume only, on the desktop's default device.
pub struct SoftHw(pub String);

impl AudioHw for SoftHw {
    fn device(&mut self) -> String {
        self.0.clone()
    }
    fn power_on(&mut self, _blocking: &crate::rt::Blocking) {}
    fn power_off(&mut self, _blocking: &crate::rt::Blocking) {}
    fn mute(&mut self, _blocking: &crate::rt::Blocking, _: bool) {}
    fn set_attenuation(&mut self, _blocking: &crate::rt::Blocking, _: u8) -> Result<bool, String> {
        Ok(false)
    }
    fn set_filter(&mut self, _blocking: &crate::rt::Blocking, _: u8) {}
}

/// The mainline image's CS43131 card: verified hardware volume in 0.5 dB steps,
/// with the codec settings the stock ioctls carried written as ALSA controls.
pub struct MainlineHw(pub String);

impl AudioHw for MainlineHw {
    fn device(&mut self) -> String {
        self.0.clone()
    }
    fn power_on(&mut self, _blocking: &crate::rt::Blocking) {}
    fn power_off(&mut self, _blocking: &crate::rt::Blocking) {}
    fn mute(&mut self, _blocking: &crate::rt::Blocking, _: bool) {}
    fn set_attenuation(&mut self, blocking: &crate::rt::Blocking, steps: u8) -> Result<bool, String> {
        super::mixer::set_codec_attenuation(blocking, steps)?;
        Ok(true)
    }
    fn set_headphone_power(&mut self, blocking: &crate::rt::Blocking, level: crate::config::HeadphonePower)
        -> Result<crate::config::HeadphonePower, String> {
        super::mixer::set_headphone_power(blocking, level)
    }
    fn set_filter(&mut self, blocking: &crate::rt::Blocking, filter: u8) {
        if let Err(e) = super::mixer::set_codec_filter(blocking, filter) {
            log::warn!("{e}");
        }
    }
}


#[cfg(test)]
mod format_tests {
    #[test]
    fn write_recovery_contract_reports_each_recovery_and_stops_on_a_failed_one() {
        // An underrun then a suspend, both recovered: the accepted count is
        // the first non-negative return, with both codes reported in order.
        let mut returns=std::collections::VecDeque::from([-32i64,-86,480]);
        let mut noted=Vec::new();
        let mut recovered=Vec::new();
        assert_eq!(super::write_recovering(||returns.pop_front().unwrap(),|code|{recovered.push(code);0},|code|noted.push(code)),Ok(480));
        assert_eq!((noted,recovered),(vec![-32,-86],vec![-32,-86]));
        // A recovery that fails is the write's error; nothing is retried.
        let mut writes=0;
        let mut noted=Vec::new();
        assert_eq!(super::write_recovering(||{writes+=1;-19},|code|code,|code|noted.push(code)),Err(-19));
        assert_eq!((writes,noted),(1,vec![-19]));
    }

    #[test]
    fn persistent_output_failure_cannot_spin_forever_after_successful_prepare() {
        let mut attempts=0;let mut repairs=0;
        assert_eq!(super::write_recovering(||{attempts+=1;-i64::from(libc::EPIPE)},|_|{repairs+=1;0},|_|{}),Err(-libc::EPIPE));
        assert_eq!((attempts,repairs),(5,4));
    }

    #[test]
    fn generated_recoveries_and_the_fake_underrun_model_are_kept_bounded_until_taken() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut sink=super::Sink::open(&blocking, super::FAKE,8000,2,400_000).unwrap();
        sink.capture_fake_pcm();
        sink.script_recoveries([-32,-32,-32,-32,-32,-32]);
        for _ in 0..6 {sink.write(&blocking, &[0;8]).unwrap();}
        let (kept,dropped)=sink.recovered();
        assert_eq!((kept.len(),dropped),(super::RECOVERIES_KEPT,2));
        assert!(kept.iter().all(|r|r.code==-32 && r.t_us>0));
        sink.clear_recovered();
        assert!(sink.recovered().0.is_empty() && sink.recovered().1==0);
        // The model: an empty queue is an underrun only after audio was
        // queued, and not after a deliberate flush. The queue depth is the
        // fixture's, not the wall clock's.
        let mut sink=super::Sink::open(&blocking, super::FAKE,8000,2,400_000).unwrap();
        sink.capture_fake_pcm();
        sink.model_underruns(-32);
        sink.set_fake_delay(0);
        sink.write(&blocking, &[0;160]).unwrap();
        assert_eq!((sink.pre_write_delay(),sink.recovered().0.len()),(Some(0),0),"the first write finds an empty queue by design");
        sink.fake_started=true; // the capture path does not pace a queue
        sink.write(&blocking, &[0;160]).unwrap();
        assert_eq!(sink.recovered().0.len(),1,"ran dry");
        sink.set_fake_delay(1600);
        sink.write(&blocking, &[0;160]).unwrap();
        assert_eq!((sink.pre_write_delay(),sink.recovered().0.len()),(Some(1600),1),"audio was queued: no underrun");
        sink.flush(&blocking);
        sink.write(&blocking, &[0;160]).unwrap();
        assert_eq!((sink.pre_write_delay(),sink.recovered().0.len()),(Some(0),1),"a flush empties the queue on purpose");
    }

    use super::format_device;
    fn prepared_sink<'a>(blocking: &'a crate::rt::Blocking)->(super::Sink<'a>,std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let mut sink=super::Sink::open(blocking, super::FAKE,8000,2,200000).unwrap();
        sink.tail.append(&[10,20,30,40,50,60,70,80]);sink.set_fake_delay(4);
        let calls=std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        sink.script_pcm_state(super::STATE_PREPARED,calls.clone());
        sink.script_pause_code(-libc::EBADFD);
        sink.capture_fake_pcm();
        (sink,calls)
    }

    #[test]
    fn prepared_queue_is_transferred_once_without_invalid_hardware_pause() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let (mut sink,calls)=prepared_sink(&blocking);
        assert!(sink.pause(&blocking, true).unwrap());assert!(sink.pause(&blocking, true).unwrap());
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed),0);
        assert_eq!(sink.paused_frames(),4);assert_eq!(sink.pause_ownership(),(2,4));
        assert!(sink.captured_fake_pcm().is_empty());
        sink.lower_paused_gain(&blocking, 1.0,0.5);
        assert_eq!(sink.retained(&blocking).unwrap().samples,vec![5,10,15,20,25,30,35,40]);
        sink.resume(&blocking).unwrap();sink.resume(&blocking).unwrap();
        assert_eq!(sink.captured_fake_pcm(),&[5,10,15,20,25,30,35,40]);
        assert!(sink.pause_fault.is_none());
    }

    #[test]
    fn underrun_and_suspend_pause_retain_exact_tail_without_bad_state_and_resume_once() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        for state in [super::STATE_XRUN,super::STATE_SUSPENDED] {
            for frames in [0,2,4] {
                let (mut sink,calls)=prepared_sink(&blocking);
                sink.script_pcm_state(state,calls.clone());sink.set_fake_delay(frames);
                assert!(sink.pause(&blocking,true).unwrap());
                assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed),0);
                assert_eq!(sink.paused_frames(),frames);
                sink.resume(&blocking).unwrap();sink.resume(&blocking).unwrap();
                assert_eq!(sink.captured_fake_pcm(), &[10,20,30,40,50,60,70,80][8-frames as usize*2..]);
                assert!(!sink.pause_fault_pending());
            }
        }
    }

    /// USB DAC and Bluetooth outputs (and a SUSPENDED internal codec): an
    /// unreadable delay leaves the stopped queue unknown.
    #[test]
    fn stopped_recovery_refuses_unknown_queue_and_retries_after_verified_delay() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        for (state,internal) in [(super::STATE_XRUN,false),(super::STATE_SUSPENDED,false),(super::STATE_SUSPENDED,true)] {
            let (mut sink,calls)=prepared_sink(&blocking);
            sink.set_xrun_empties_queue(internal);
            sink.script_pcm_state(state,calls);sink.fake_delay_error=true;
            assert!(sink.pause(&blocking,true).is_err());assert_eq!(sink.reset_calls,0);
            assert!(sink.resume(&blocking).is_err());assert_eq!(sink.reset_calls,0);
            sink.fake_delay_error=false;sink.set_fake_delay(2);
            sink.resume(&blocking).unwrap();
            assert_eq!(sink.captured_fake_pcm(), &[50,60,70,80]);
        }
    }

    #[test]
    fn only_the_internal_codec_route_empties_its_queue_on_xrun() {
        use super::super::output::{Route, UsbPlayback};
        assert!(super::Sink::xrun_empties_queue_for(&Route::Headphones,super::INTERNAL_CODEC_PCM));
        assert!(!super::Sink::xrun_empties_queue_for(&Route::Headphones,"default"),"another platform's headphone PCM");
        assert!(!super::Sink::xrun_empties_queue_for(&Route::Bluetooth,super::INTERNAL_CODEC_PCM));
        let usb=Route::Usb(UsbPlayback {identity:"generated-dac".into(),card:3,device:0});
        assert!(!super::Sink::xrun_empties_queue_for(&usb,super::INTERNAL_CODEC_PCM));
    }

    /// The internal codec, V0.128 on the Q2: a pause of an underrun PCM
    /// (delay fails with EPIPE) retains an empty, verified queue instead of
    /// an unverified one, and resume prepares and continues.
    #[test]
    fn internal_codec_xrun_pause_retains_an_empty_verified_queue() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let (mut sink,calls)=prepared_sink(&blocking);
        sink.set_xrun_empties_queue(true);
        sink.script_pcm_state(super::STATE_XRUN,calls.clone());sink.fake_delay_error=true;
        assert!(sink.pause(&blocking,true).unwrap(),"an empty queue is retained");
        assert!(!sink.pause_transfer_unverified() && !sink.pause_fault_pending());
        assert_eq!(sink.paused_frames(),0);
        assert_eq!(sink.pause_ownership(),(2,0));
        assert_eq!(sink.delay(&blocking),0);
        sink.resume(&blocking).unwrap();
        assert!(sink.captured_fake_pcm().is_empty(),"nothing unheard is replayed");
        // The same device on another route keeps the queue unknown.
        let (mut other,calls)=prepared_sink(&blocking);
        other.script_pcm_state(super::STATE_XRUN,calls);other.fake_delay_error=true;
        assert!(other.pause(&blocking,true).is_err());
        assert!(other.pause_transfer_unverified());
    }

    #[test]
    fn prepared_drop_or_prepare_failure_keeps_exact_pcm_and_sticky_refusal() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        for codes in [vec![-libc::ENODEV],vec![0,-libc::EIO]] {
            let (mut sink,calls)=prepared_sink(&blocking);
            sink.reset_codes=codes.into();
            assert!(sink.pause(&blocking, true).is_err());assert!(sink.pause(&blocking, true).is_err());
            assert_eq!(sink.retained(&blocking).unwrap().samples,vec![10,20,30,40,50,60,70,80]);
            sink.reset_codes=vec![-libc::ENODEV].into();
            assert!(sink.resume(&blocking).is_err());
            assert!(sink.captured_fake_pcm().is_empty());
            assert!(sink.pause(&blocking, true).is_err());
            assert_eq!(sink.retained(&blocking).unwrap().samples,vec![10,20,30,40,50,60,70,80]);
            sink.resume(&blocking).unwrap();sink.resume(&blocking).unwrap();
            assert_eq!(sink.captured_fake_pcm(),&[10,20,30,40,50,60,70,80]);
            assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed),0);
        }
    }

    #[test]
    fn prepared_unknown_delay_or_incomplete_shadow_cannot_admit_a_pause() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        for unknown in [false,true] {
            let (mut sink,calls)=prepared_sink(&blocking);
            if unknown {sink.fake_delay_error=true;}else{sink.set_fake_delay(5);}
            assert!(sink.pause(&blocking, true).is_err());assert!(sink.pause(&blocking, true).is_err());
            assert!(sink.pause_transfer_unverified());assert_eq!(sink.pause_ownership(),(3,4));assert_eq!(sink.reset_calls,0);
            assert_eq!(sink.retained(&blocking).unwrap().samples,vec![10,20,30,40,50,60,70,80]);
            sink.lower_paused_gain(&blocking, 1.0,0.5);
            assert!(sink.resume(&blocking).is_err());assert_eq!(sink.reset_calls,0);
            assert!(sink.captured_fake_pcm().is_empty());
            if unknown {
                sink.fake_delay_error=false;sink.resume(&blocking).unwrap();
                assert!(!sink.pause_transfer_unverified());
                assert_eq!(sink.captured_fake_pcm(),&[5,10,15,20,25,30,35,40]);
            }
            assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed),0);
        }
    }

    #[test]
    fn running_disconnected_and_unknown_states_keep_operational_pause_failures() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        for state in [None,Some(3),Some(8),Some(-1)] {
            for code in [-libc::ETIMEDOUT,-libc::ENODEV,-libc::EPIPE,-libc::EBADFD] {
                let (mut sink,calls)=prepared_sink(&blocking);
                sink.pcm_state_fixture=state.map(|state|(state,calls.clone()));
                sink.script_pause_code(code);
                assert!(sink.pause(&blocking, true).is_err(),"state={state:?} code={code}");
                assert!(sink.pause(&blocking, true).is_err());
                assert_eq!(sink.retained(&blocking).unwrap().samples,vec![10,20,30,40,50,60,70,80]);
                if state.is_some() {assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed),1);}
            }
        }
    }

    #[test]
    fn operational_pause_fault_survives_repeat_and_gain_transfer_until_output_progress() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut sink=super::Sink::open(&blocking, super::FAKE,8000,2,200000).unwrap();
        sink.tail.append(&[100;8]);sink.set_fake_delay(4);
        sink.script_pause_code(-libc::ETIMEDOUT);
        assert!(sink.pause(&blocking, true).is_err());
        assert!(sink.pause(&blocking, true).is_err());
        sink.lower_paused_gain(&blocking, 1.0,0.5);
        assert!(sink.pause(&blocking, true).is_err());
        assert_eq!(sink.retained(&blocking).unwrap().samples,vec![50;8]);
        sink.script_pause_code(-libc::ENOSYS);
        sink.resume(&blocking).unwrap();
        assert!(sink.pause(&blocking, true).unwrap());
    }
    #[test]
    fn empty_resume_does_not_clear_an_unconfirmed_operational_pause_fault() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut sink=super::Sink::open(&blocking, super::FAKE,8000,2,200000).unwrap();
        sink.script_pause_code(-libc::ETIMEDOUT);
        assert!(sink.pause(&blocking, true).is_err());
        sink.script_pause_code(-libc::ENOSYS);
        sink.resume(&blocking).unwrap();
        assert!(sink.pause(&blocking, true).is_err());
        sink.write_progress(&blocking, &[10,20]).unwrap();
        assert!(sink.pause(&blocking, true).unwrap());
    }
    #[test]
    fn repeated_delay_failure_keeps_only_bounded_latest_pcm() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut sink = super::Sink::open(&blocking, super::FAKE, 1000, 1, 200000).unwrap();
        sink.fake_delay_error = true;
        for i in 0..100 {
            sink.record_written(&blocking, &vec![i; 64], None);
            assert!(sink.tail.frames(1) <= 200);
        }
        assert_eq!(sink.delay(&blocking), 200);
        let held = sink.retained(&blocking).unwrap();
        assert_eq!(held.samples.len(), 200);
        assert_eq!(&held.samples[136..], &[99; 64]);
    }
    #[test]
    fn q2_uses_exact_native_rates_and_converts_mono_and_high_rate_sources() {
        for rate in [44100,48000,96000] {assert_eq!(format_device("hw:Q2,0",rate,2),"hw:Q2,0");}
        for rate in [8000,16000,22050,44100,48000,96000] {assert_eq!(format_device("hw:Q2,0",rate,1),"plughw:Q2,0");}
        for rate in [1,44099,44101,47999,48001,95999,96001,192000,u32::MAX] {assert_eq!(format_device("hw:Q2,0",rate,2),"plughw:Q2,0");}
    }
    #[test]
    fn external_dacs_and_bluetooth_keep_their_own_format_negotiation() {
        for device in ["hw:1,0","plughw:1,0","bluealsa:DEV=00:11:22:33:44:55,PROFILE=a2dp","default"] {
            for rate in [8000,16000,44100,48000,96000,192000] {for channels in [1,2,6] {assert_eq!(format_device(device,rate,channels),device);}}
        }
    }
}

#[cfg(test)]
mod write_tests {
    use super::write_frames;

    #[test]
    fn live_codec_changes_keep_normalized_unheard_pcm_through_mute() {
        let blocking=crate::rt::Blocking::dedicated_thread();
        for precise in [false,true] {
            let original=vec![100003,-100019,1000000001,-1000000007];
            let mut held=super::super::retained::RetainedPcm::at_output_gain(original.clone(),8000,2,0.1);
            if precise {held.rebase_output(0.3).unwrap();}
            let mut sink=super::Sink::open(&blocking,super::FAKE,8000,2,400000).unwrap();
            sink.configure_output_gain(held.output_gain()).unwrap();
            sink.script_writes(super::WriteFixture {steps:Default::default(),accepted:Default::default(),
                queued:Default::default(),drained:Default::default(),consumed_on_error:Default::default(),on_error:None,first_write:None,queue_limit:None});
            sink.write_retained(&blocking,&held).unwrap();
            for gain in [0.5,0.0,held.output_gain()] {sink.change_output_gain(&blocking,gain).unwrap();}
            let mut retained=sink.retained(&blocking).unwrap();
            retained.rebase_output(0.1).unwrap();
            assert_eq!(retained.samples,original,"live mute must not erase the normalized source shadow");
        }
    }

    #[test]
    fn refused_replay_keeps_audio_owned_and_does_not_write_pcm() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let mut sink = super::Sink::open(&blocking,super::FAKE,8000,2,400000).unwrap();
        let original = vec![13,-17,101,-103];
        let held = super::super::retained::RetainedPcm::at_output_gain(original.clone(),8000,2,0.1);
        assert!(sink.replay(&blocking,held).is_err());
        assert!(sink.written_samples.is_empty());
        let retained = sink.retained(&blocking).unwrap();
        assert_eq!(retained.samples,original);
        assert_eq!(retained.output_gain(),0.1);
        assert!(sink.pause_fault_pending());
    }

    #[test]
    fn failed_replay_preserves_original_gain_and_exact_remaining_frames() {
        use std::sync::{Arc,Mutex};
        let blocking = crate::rt::Blocking::dedicated_thread();
        for channels in 1..=4 {
            for boundary in 1..12 {
                let original: Vec<i32> = (0..12*channels).map(|n|n as i32*100003+97).collect();
                let mut held = super::super::retained::RetainedPcm::at_output_gain(original.clone(),8000,channels,0.1);
                held.rebase_output(0.3).unwrap();
                let mut sink = super::Sink::open(&blocking,super::FAKE,8000,channels,400000).unwrap();
                sink.configure_output_gain(0.3).unwrap();
                let consumed = boundary / 2;
                sink.script_writes(super::WriteFixture {
                    steps:[Ok(boundary),Err("generated route loss".into())].into(),
                    accepted:Arc::new(Mutex::new(Vec::new())),queued:Default::default(),drained:Default::default(),
                    consumed_on_error:[consumed].into(),on_error:None,first_write:None,queue_limit:None,
                });
                assert!(sink.replay(&blocking,held).is_err());
                let mut remaining = sink.retained(&blocking).unwrap();
                remaining.rebase_output(0.1).unwrap();
                assert_eq!(remaining.samples,original[consumed*channels as usize..]);
                assert!(sink.configure_output_gain(0.1).is_err(),"cannot relabel a stopped retained queue");
            }
        }
    }

    #[test]
    fn unplug_at_every_frame_boundary_reports_the_exact_accepted_prefix() {
        for channels in 1..=8 {
            let pcm:Vec<i32>=(0..37*channels).map(|n|n as i32*7919).collect();
            for boundary in 0..37 {
                let mut accepted=Vec::new();
                let mut calls=0;
                let error=write_frames(&pcm,channels,|remaining| {
                    calls+=1;
                    assert_eq!(remaining,&pcm[if calls==1 {0}else{boundary*channels}..]);
                    if calls==1 && boundary>0 {Ok(boundary)}else{Err("Synthetic ENODEV".into())}
                },|part|accepted.extend_from_slice(part)).unwrap_err();
                assert_eq!(error.accepted_frames,boundary as u64);
                assert_eq!(accepted,&pcm[..boundary*channels]);
                accepted.extend_from_slice(&pcm[error.accepted_frames as usize*channels..]);
                assert_eq!(accepted,pcm,"the decoder's packet must remain recoverable exactly once");
                assert!(calls<=2);
            }
        }
    }

    #[test]
    fn seeded_partial_writes_never_duplicate_skip_or_cross_a_frame_boundary() {
        let mut rng=fastrand::Rng::with_seed(0x55534250434d);
        for channels in 1..=8 {
            for _ in 0..1000 {
                let frames=rng.usize(1..128);
                let pcm:Vec<i32>=(0..frames*channels).map(|n|n as i32).collect();
                let mut accepted=Vec::new();
                write_frames(&pcm,channels,|remaining|Ok(rng.usize(1..=remaining.len()/channels)),
                    |part|accepted.extend_from_slice(part)).unwrap();
                assert_eq!(accepted,pcm);
            }
        }
    }

    #[test]
    fn zero_or_invalid_progress_is_bounded_and_unaligned_input_is_never_submitted() {
        for count in [0,5] {
            let mut calls=0;
            let mut accepted=Vec::new();
            let error=write_frames(&[1,2,3,4],2,|_|{calls+=1;Ok(count)},
                |part|accepted.extend_from_slice(part)).unwrap_err();
            assert_eq!(calls,1);
            assert!(accepted.is_empty());
            assert_eq!(error.accepted_frames,0);
        }
        for channels in [0,2] {
            let error=write_frames(&[1,2,3],channels,|_|panic!("unaligned write submitted"),
                |_|panic!("unaligned data accepted")).unwrap_err();
            assert_eq!(error.accepted_frames,0);
        }
    }
}

#[cfg(all(test, unix))]
#[path = "alsa_write_api_tests.rs"]
mod write_api_tests;
