//! PCM ownership, pause/park/replay and output handoff.
//! The engine and its borrowed Blocking witness stay on the player thread.
//! Queue selection and decoder advancement are defined in sibling modules.

use super::*;

impl Engine<'_> {
    /// Feeds the power/storage gate and stores its next state.
    pub(super) fn gate(&mut self, event: handoff::Event) -> handoff::Effect {
        let state = if self.handoff_frozen { handoff::Gate::Frozen } else { handoff::Gate::Open };
        let (next, effect) = handoff::transition(state, event);
        self.handoff_frozen = next == handoff::Gate::Frozen;
        effect
    }

    /// The output machine's view of the sink and the parked queue. The idle
    /// deadline, parking and opening decide on the PCM that is open.
    pub(super) fn output_view(&self) -> outfsm::Output {
        debug_assert!(self.sink.is_none() || self.parked.is_none(), "an open PCM and a parked queue coexist");
        match (&self.sink, &self.parked) {
            (Some(sink), _) => outfsm::Output::Open {
                params: outfsm::Params { rate: sink.rate, channels: sink.channels },
                flow: if sink.pause_transfer_unverified() { outfsm::Flow::Unverified }
                    else if sink.retains_paused_pcm() { outfsm::Flow::Retained } else { outfsm::Flow::Running },
            },
            (None, Some(held)) => outfsm::Output::Parked { params: outfsm::Params { rate: held.rate, channels: held.channels } },
            (None, None) => outfsm::Output::Closed,
        }
    }

    /// Parks or releases the output as the output machine decides for the
    /// event `make(paused, retainable)` (a park request or the idle deadline).
    pub(super) fn park_with(&mut self, make: impl Fn(bool, bool) -> outfsm::Event) {
        let view = self.output_view();
        let paused = self.state == PlayState::Paused;
        // Whether anything happens does not depend on retainability (proved
        // in q2_machines::output), so the queue is copied out only when needed.
        if outfsm::transition(view, make(paused, false)).1 == outfsm::Effect::None { return; }
        // At EOF the next file may be selected before its decoder opens,
        // while the preceding file still has an unheard retained PCM tail.
        let held = if paused { self.sink.as_ref().and_then(|sink| sink.retained(self.blocking)) } else { None };
        // One line per pause at most: lets a power capture confirm the DAC
        // went down (the stream path costs about 133 mW while open).
        log::info!("Audio output {} after {} ms without playing",
            if held.is_some() { "parked" } else { "released" }, self.idle_since.elapsed().as_millis());
        match outfsm::transition(view, make(paused, held.is_some())).1 {
            outfsm::Effect::Park => {
                self.hw.mute(self.blocking, true);self.sink=None;self.hw.power_off(self.blocking);
                self.tap.trace.event(crate::playtrace::Kind::SinkClose,0,0,0);
                self.parked=held;
            }
            _ => self.release_output(),
        }
    }

    pub(super) fn pause(&mut self) {
        self.resume_output=false;
        let _=self.pause_output(); // The error is published in the paused status.
        // A Headphone Power change made while playing: park now, the queue
        // kept, and write it with the PCM closed. (A failed write is tried
        // again at the next open instead.)
        let changed=self.applied_power.is_some_and(|(asked,_)|asked!=self.headphone_power);
        if changed && !self.apply_headphone_power() {
            self.park_output();
            self.apply_headphone_power();
        }
        self.save();
    }

    /// Writes the owner's Headphone Power to the codecs unless it is the
    /// level last written. The codecs change their full scale only with the
    /// HP stage powered down (DS1155F2 7.5.1; kernel patch 0187 refuses
    /// otherwise), so nothing is written while the headphone PCM is open.
    /// Returns false while the change still waits for the PCM to close.
    pub(super) fn apply_headphone_power(&mut self)->bool {
        let level=self.headphone_power;
        if self.applied_power.is_some_and(|(asked,_)|asked==level) {return true;}
        if self.sink.is_some() && self.sink_internal {
            log::info!("Headphone Power {} waits for the headphone output to close",level.label());
            return false;
        }
        self.applied_codec_steps = None;
        self.applied_power=match self.hw.set_headphone_power(self.blocking,level) {
            Ok(held) => { log::info!("Headphone Power {}",held.label()); Some((level,held)) }
            // Unknown level: no make-up gain, and the next open tries again.
            Err(error) => { log::warn!("{error}"); None }
        };
        true
    }

    pub(super) fn pause_output(&mut self)->Result<bool,String> {
        if let Some(error)=self.pause_admission_fault.clone() {
            self.state=PlayState::Paused;self.resume_output=false;self.hw.mute(self.blocking, true);
            self.error=Some(error.clone());return Err(error);
        }
        if self.is_radio() { self.close_track(); self.release_output(); self.resume_at = 0; self.base_ms = 0; self.frames = 0; }
        let (index, position) = self.audible_cursor();
        self.state = PlayState::Paused;
        self.hw.mute(self.blocking, true);
        let mut pause_error=None;
        let discarded=self.sink.as_mut().is_some_and(|sink|match sink.pause(self.blocking, true) {
            Ok(retained)=>!retained,
            Err(error)=>{pause_error=Some(error);!sink.retains_paused_pcm()}
        });
        if discarded && (self.dec.is_some() || self.queue.get(self.index).is_some_and(|song| song.spoken.is_some())) {
            // Unsupported hardware pause drops queued audio, including the
            // boundary window after EOF before the next decoder has opened.
            self.close_track();
            self.index = index;
            self.resume_at = position;
            self.base_ms = position;
            self.frames = 0;
        }
        self.idle_since = Instant::now();
        if self.sink.as_ref().is_some_and(Sink::pause_transfer_unverified) {
            // The idle release never parks an unverified queue (q2_machines::output).
            log::warn!("Paused with an unverified PCM queue: the output stays open and the DAC powered until resume, seek or stop");
        }
        self.tap.paused(!discarded,self.sink.as_ref().map_or(0,Sink::paused_frames));
        if let Some(error)=pause_error {
            if self.recover_descriptor(&error, false) { return Ok(false); }
            self.resume_output=false;
            let message=format!("Unable to pause output: {error}");
            self.error=Some(message.clone());
            self.pause_admission_fault=Some(message.clone());
            return Err(message);
        }
        Ok(!discarded)
    }

    pub(super) fn resume(&mut self) {
        // A nonempty successful replay establishes actual output progress.
        // Empty resume requests keep a fault until the next accepted write.
        let replayed=self.parked.as_ref().map_or_else(||self.sink.as_ref().map_or(0,Sink::paused_frames),|held|held.frames());
        self.resume_output=true;
        if let Some(mut held)=self.parked.take() {
            // Park then resume reopens with the parked parameters (proved in
            // q2_machines::output).
            let parked = outfsm::Output::Parked { params: outfsm::Params { rate: held.rate, channels: held.channels } };
            let outfsm::Effect::Replay(params) = outfsm::transition(parked, outfsm::Event::Resume { ok: true }).1 else { unreachable!() };
            if let Err(error)=self.ensure_sink_format(params.rate,params.channels,held.required_output_gain(),held.format) {
                self.parked=Some(held);self.output_failed(error);return;
            }
            if let Err(error)=held.rebase_output(self.output_gain) {
                self.parked=Some(held);self.sink=None;
                self.hw.mute(self.blocking,true);self.hw.power_off(self.blocking);
                self.output_failed(error);return;
            }
            if let Err(error)=self.sink.as_mut().unwrap().replay(self.blocking, held) {
                self.parked=self.sink.as_ref().and_then(|sink| sink.retained(self.blocking));
                self.sink=None;self.hw.mute(self.blocking, true);self.hw.power_off(self.blocking);
                self.output_failed(error);return;
            }
        } else if let Some(s)=&mut self.sink {
            if let Err(error)=s.resume(self.blocking) {self.output_failed(error);return;}
        }
        self.state = if self.dec.is_some() { PlayState::Playing } else { PlayState::Buffering };
        self.hw.mute(self.blocking, !(self.sink.is_some() && matches!(self.route.route,crate::audio::output::Route::Headphones)));
        self.resume_output=false;
        self.tap.resumed();
        if self.pause_admission_fault.is_none() || (replayed>0 && self.sink.as_ref().is_some_and(|sink|!sink.pause_fault_pending())) {
            self.pause_admission_fault=None;self.error=None;
        }
        self.sleep_probe("resumed");
    }

    pub(super) fn idle_wait(&self)->Duration {
        // A deadline applies only while a verified PCM is open.
        match outfsm::idle_wait_ms(self.output_view(), 0) {
            Some(_) => IDLE_RELEASE.saturating_sub(self.idle_since.elapsed()),
            None => Duration::from_secs(3600),
        }
    }

    pub(super) fn idle(&mut self) {
        let idle_ms = u64::try_from(self.idle_since.elapsed().as_millis()).unwrap_or(u64::MAX);
        // A paused queue whose ownership was not established keeps its PCM
        // (resume must not lose or repeat audio) until a new delay
        // observation establishes it; then it parks like any other. Retry
        // once per deadline instead of holding the DAC until the next command.
        if idle_ms >= outfsm::IDLE_RELEASE_MS && self.state == PlayState::Paused
            && self.sink.as_ref().is_some_and(Sink::pause_transfer_unverified) {
            if let Err(error) = self.sink.as_mut().unwrap().revalidate_paused(self.blocking) {
                log::warn!("Paused output kept open: its queue is still unverified ({error}); retrying in {} s",
                    outfsm::IDLE_RELEASE_MS / 1000);
                self.idle_since = Instant::now();
                return;
            }
        }
        self.park_with(|paused, retainable| outfsm::Event::Idle { idle_ms, paused, retainable });
    }

    pub(super) fn park_output(&mut self) {
        // Both the idle DAC deadline and the sleep request release electrical
        // output, without treating retained media as a new selection on wake.
        self.park_with(|paused, retainable| outfsm::Event::Park { paused, retainable });
    }

    pub(super) fn release_output(&mut self) {
        self.resume_output=false;
        let (index, position) = self.audible_cursor();
        let had_sink=self.sink.take().is_some();
        let had_parked=self.parked.take().is_some();
        let had_pending=self.pending_output.take().is_some();
        if had_sink || had_parked || had_pending {
            self.hw.mute(self.blocking, true);
            self.hw.power_off(self.blocking);
            // The decoder's position is ahead of what was heard; reopen there.
            if self.dec.is_some() || self.queue.get(self.index).is_some_and(|song| song.spoken.is_some()) {
                self.index = index;
                self.resume_at = position;
                self.base_ms = self.resume_at;
                self.frames = 0;
                self.dec = None;
                self.tempo = None;
                self.end_trace(3);
            }
            if had_sink { self.tap.trace.event(crate::playtrace::Kind::SinkClose,0,0,0); }
        }
    }

    pub(super) fn ensure_sink(&mut self,rate:u32,channels:u32)->Result<(),String> {
        self.ensure_sink_for(rate,channels,0.0)
    }
    pub(super) fn ensure_sink_for(&mut self, rate:u32, channels:u32, headroom:f64)->Result<(),String> {
        self.ensure_sink_format(rate,channels,headroom,self.source_format)
    }
    pub(super) fn ensure_sink_format(&mut self,rate:u32,channels:u32,headroom:f64,format:super::super::pcm_format::PcmFormat)->Result<(),String> {
        // Validate at the boundary: a PCM cannot be opened for these.
        let params = outfsm::Params { rate, channels };
        if !params.valid() { return Err(format!("Unsupported audio format: {rate} Hz, {channels} channels")); }
        self.prepare_checkpoints()?;
        if self.sink.as_ref().is_some_and(|sink|sink.format!=format) {
            // Width changes at a track boundary drain the preceding format's
            // tail just like rate/channel changes. Parked replay supplies its
            // own width, independently of the next decoder's metadata.
            self.sink.as_mut().unwrap().drain(self.blocking);self.sink=None;
        }
        match outfsm::transition(self.output_view(), outfsm::Event::Ensure(params)).1 {
            outfsm::Effect::Reuse => {
                if headroom>self.output_gain {self.apply_volume_for(headroom)?;}
                return Ok(());
            },
            // A new format: let the old song's tail play out first.
            outfsm::Effect::DrainThenOpen(_) => { if let Some(s) = &mut self.sink { s.drain(self.blocking); } }
            _ => {}
        }
        self.sink = None;
        let headphones=matches!(self.route.route,crate::audio::output::Route::Headphones);
        // With no PCM open the HP stages are down: a waiting Headphone Power
        // change lands here, before the attenuation that depends on it.
        self.apply_headphone_power();
        self.apply_volume_for(headroom)?; // Verify codec attenuation before opening PCM.
        let device=if headphones {
            self.hw.power_on(self.blocking);self.hw.device()
        }else{
            self.hw.mute(self.blocking, true);self.hw.power_off(self.blocking);self.route.route.pcm()
        };
        let mut sink = match self.hw.open_sink(self.blocking, &device, rate, channels, LATENCY_US,format) {
            Ok(sink) => sink,
            Err(error) => { self.hw.mute(self.blocking, true); self.hw.power_off(self.blocking); return Err(error); }
        };
        if !headphones { sink.enable_xrun_carry(self.blocking)?; }
        sink.set_xrun_empties_queue(Sink::xrun_empties_queue_for(&self.route.route,&device));
        sink.configure_output_gain(self.output_gain)?;
        self.sink = Some(sink);
        self.sink_internal = headphones;
        self.tap.trace.event(crate::playtrace::Kind::SinkOpen,rate,channels,0);
        self.hw.mute(self.blocking, !headphones);
        self.volume_ramp.start(self.soft_volume, rate);
        Ok(())
    }

    /// The output is Bluetooth and its route has already left: the headphones disconnected.
    /// Their thread changes the route before it fails writes (`bluetooth::Shared::output`).
    fn bluetooth_route_left(&self)->bool {
        matches!(self.route.route,crate::audio::output::Route::Bluetooth)
            && self.outputs.as_ref().is_some_and(|outputs| outputs.changed(self.route.generation)
                && !matches!(outputs.snapshot().route,crate::audio::output::Route::Bluetooth))
    }

    pub(super) fn output_failed(&mut self,error:String) {
        // A write the disconnect failed is the disconnect itself: an ordinary pause, with
        // the unsent audio kept, and no error (BLUETOOTH-001).
        if self.bluetooth_route_left() { self.refresh_route(); return; }
        if let (Some(sink), Some(pending)) = (&self.sink, &mut self.pending_output) {
            sink.limit_unheard_volume(pending);
        }
        if self.recover_descriptor(&error, self.resume_output || matches!(self.state, PlayState::Playing | PlayState::Buffering)) { return; }
        // Output loss is independent of media/decoder validity, including radio.
        self.resume_output|=matches!(self.state,PlayState::Playing|PlayState::Buffering);
        self.state=PlayState::Paused;
        self.hw.mute(self.blocking, true);
        if let Some(sink)=&mut self.sink {let _=sink.pause(self.blocking, true);} // Preserve the original output error.
        self.idle_since=Instant::now();
        self.bluetooth_fault=matches!(self.route.route,crate::audio::output::Route::Bluetooth).then(||error.clone());
        self.error=Some(error);
        self.save();
        self.publish(true);
        if matches!(self.route.route,crate::audio::output::Route::Usb(_)) {
            if let Some(outputs)=&self.outputs {outputs.reject(&self.route);}
        }
    }

    pub(super) fn refresh_route(&mut self) {
        let Some(outputs)=&self.outputs else {return;};
        if !outputs.changed(self.route.generation) {return;}
        let desired=outputs.snapshot();
        // Headphones that disconnect leave the music paused: it must not start
        // from another output on its own (BLUETOOTH-001).
        let leaving_bluetooth=matches!(self.route.route,crate::audio::output::Route::Bluetooth)
            && !matches!(desired.route,crate::audio::output::Route::Bluetooth);
        let resume=(matches!(self.state,PlayState::Playing|PlayState::Buffering)||self.resume_output) && !leaving_bluetooth;
        if let Some(sink)=&mut self.sink {
            self.hw.mute(self.blocking, true);
            // Device loss may make pause fail; retain its last-known tail.
            // Active routing recovery already owns this failure. An explicit
            // user pause keeps resume_output false and its visible error.
            let pause=sink.pause(self.blocking, true);
            if sink.pause_transfer_unverified() {
                // Topology is not permission to discard a stopped queue whose
                // complete ownership is unknown. Keep this handle and media;
                // explicit resume must revalidate, or selection may discard.
                let error=self.pause_admission_fault.clone().unwrap_or_else(||format!("Unable to pause output: {}",
                    pause.err().unwrap_or_else(||"Unverified PREPARED queue".into())));
                self.state=PlayState::Paused;self.resume_output=false;
                self.pause_admission_fault=Some(error.clone());self.error=Some(error);
                self.idle_since=Instant::now();return;
            }
            let Some(held)=sink.retained(self.blocking) else {
                self.route=desired;
                self.output_failed("Unable to retain queued output during device change".into());return;
            };
            self.parked=Some(held);
            self.sink=None;
            self.hw.power_off(self.blocking);
        }
        self.route=desired;
        if leaving_bluetooth {
            self.resume_output=false;
            // Headphones that stop taking audio and then drop the link: the error was the
            // disconnect coming, and the pause is an ordinary one.
            let fault=self.bluetooth_fault.take();
            let cleared=fault.is_some() && self.error==fault;
            if cleared {self.error=None;}
            if matches!(self.state,PlayState::Playing|PlayState::Buffering) {
                crate::ux::expect_stop(crate::ux::StopCause::Unplug);
                self.state=PlayState::Paused;self.idle_since=Instant::now();
                self.save();self.publish(true);
            } else if cleared {self.publish(true);}
        }
        if resume && !self.handoff_frozen {self.resume();}
    }

    pub(super) fn apply_volume(&mut self)->Result<(),String> {self.apply_volume_for(0.0)}

    /// The volume's gain on the samples. On Bluetooth with absolute volume the
    /// headphones' amplifier carries the volume and the samples stay at unity
    /// (zero still mutes); everywhere else it is the 0.5 dB-a-step curve.
    pub(super) fn digital_gain(&self)->f32 {
        if self.remote_volume && matches!(self.route.route,crate::audio::output::Route::Bluetooth) {
            if self.volume==0 {0.0} else {1.0}
        } else {soft_gain(self.volume)}
    }

    /// A decoded block can contradict its tag (or EQ can create a new peak).
    /// Keep established headroom for that track and DSP setup instead of
    /// giving it back on every volume detent. Retained blocks have their own
    /// measured headroom and must not update this new-source estimate.
    pub(super) fn remember_decoded_headroom(&mut self, headroom: f64) {
        if self.hardware_volume && headroom > self.output_gain {
            if let Some(reserve) = &mut self.replay_peak_reserve {
                let steps = crate::audio::gain::codec_steps(headroom);
                *reserve = (*reserve).max((100 - self.volume).saturating_sub(steps));
            }
        }
    }

    fn apply_volume_for(&mut self,extra_headroom:f64)->Result<(),String> {
        let _capture = crate::diagnostics::PollScope::enter(202);
        let _latency = crate::diagnostics::WallScope::new(3, 202);
        use crate::playtrace::VolumePhase;
        let mut volume_trace=self.tap.volume(self.volume,self.sink.as_ref().and_then(|sink|sink.pre_write_delay()));
        let previous=self.applied_volume;
        let desired=self.digital_gain();
        let old_soft=self.soft_volume;
        // Lower retained audio before selecting a domain. Route changes leave
        // desired unchanged, so they cannot masquerade as a volume reduction.
        if let Some(held)=&mut self.parked {held.lower_gain(previous,desired);}
        if let Some(held)=&mut self.pending_output {held.lower_gain(previous,desired);}
        use crate::audio::gain::{codec_compensation, codec_gain, codec_steps, mixed_gain_steps_fit, nominal_codec_steps};
        let live_codec = self.hardware_volume && self.sink_internal
            && matches!(self.route.route, crate::audio::output::Route::Headphones)
            && self.sink.as_ref().is_some_and(|sink| !sink.retains_paused_pcm());
        let slider_delta = i16::from(100 - self.volume) - i16::from(codec_steps(f64::from(previous)));
        let mut required = extra_headroom;
        for held in [&self.parked, &self.pending_output].into_iter().flatten() {
            required = required.max(held.required_output_gain());
        }
        // Match the installed f32 gain inputs and rounding for Efficient's
        // physical volume reference, including its historical grid boundary.
        let level = self.applied_power.map(|(_, held)| held);
        let efficient = level == Some(crate::config::HeadphonePower::Efficient);
        let mut installed_required = (f64::from(desired) * f64::from(self.track_gain)).min(1.0).max(required);
        let mut measured_steps = codec_steps(required);
        if let Some(sink) = &self.sink {
            if let Some(peak) = sink.required_output_gain(self.blocking) {
                let ratio = if previous > 0.0 { f64::from(desired) / f64::from(previous) } else { 1.0 };
                let ratio = if sink.retains_paused_pcm() { ratio.min(1.0) } else { ratio };
                let projected = (peak * ratio).min(1.0);
                installed_required = installed_required.max(projected);
                if live_codec && previous > 0.0 && desired > 0.0 {
                    // A live queue follows the slider delta. Translate the
                    // measured ceiling by that integer delta to preserve its
                    // grid position through an exact half-dB change.
                    let shifted = (i16::from(codec_steps(peak)) + slider_delta).clamp(0, 254) as u8;
                    measured_steps = measured_steps.min(shifted);
                } else {
                    measured_steps = measured_steps.min(codec_steps(projected));
                }
            }
        }
        let (planned_steps, physical_steps) = if desired == 0.0 { (255, 255) } else {
            let nominal = self.replay_peak_reserve.map_or_else(
                || nominal_codec_steps(self.volume, self.track_gain),
                |reserve| (100 - self.volume).saturating_sub(reserve),
            );
            let logical = nominal.min(measured_steps);
            if efficient {
                crate::audio::mixer::preserve_efficient_gain(logical, codec_steps(installed_required))
            } else {
                (logical, logical)
            }
        };
        let planned_gain = codec_gain(planned_steps);
        let planned_soft = if planned_gain > 0.0 {
            codec_compensation(self.volume, planned_steps)
        } else { 1.0 / self.track_gain.max(1.0) };
        let retarget = self.hardware_volume && old_soft != planned_soft;
        let verified_register = live_codec && self.applied_codec_steps == Some(physical_steps);
        let steady_codec = verified_register && previous > 0.0 && desired > 0.0 && planned_gain == self.output_gain;
        let mixed = live_codec && previous > 0.0 && desired > 0.0 && desired != previous
            && !steady_codec && retarget
            && self.sink.as_ref().is_some_and(|sink| !sink.has_unsettled_running_volume())
            && self.applied_codec_steps.is_some_and(|old_physical| {
                let pcm_delta = slider_delta - (i16::from(planned_steps) - i16::from(codec_steps(self.output_gain)));
                mixed_gain_steps_fit(pcm_delta, i16::from(physical_steps) - i16::from(old_physical))
            });
        let follows_codec = live_codec && previous > 0.0 && desired > 0.0 && !retarget && !steady_codec;
        let running = retarget && !(steady_codec || mixed) && self.sink.as_ref().is_some_and(|sink| !sink.retains_paused_pcm());
        let previous_codec_gain = self.output_gain;
        let flags=u32::from(matches!(self.route.route,crate::audio::output::Route::Headphones))
            | (u32::from(running)<<1)
            | (u32::from(self.sink.as_ref().is_some_and(|sink|sink.retains_paused_pcm()))<<2)
            | (u32::from(self.hardware_volume)<<3) | (u32::from(old_soft==planned_soft)<<4)
            | (u32::from(steady_codec && previous != desired) << 5)
            | (u32::from(mixed) << 6)
            | (u32::from(physical_steps) << 8);
        volume_trace.plan(planned_steps,flags,old_soft,planned_soft);
        if running {
            volume_trace.stage(VolumePhase::Pause);
            if !self.sink.as_mut().unwrap().pause(self.blocking,true)? {
                return Err("Unable to retain PCM for headroom change".into());
            }
            volume_trace.completed(self.sink.as_ref().and_then(|sink|sink.retained_frames_snapshot()));
        }
        volume_trace.stage(VolumePhase::Codec);
        let hardware = if matches!(self.route.route, crate::audio::output::Route::Headphones) {
            if verified_register {
                true
            } else {
                self.applied_codec_steps = None;
                self.hw.set_attenuation(self.blocking, physical_steps)?
            }
        } else { false };
        self.applied_codec_steps = hardware.then_some(physical_steps);
        volume_trace.completed(None);
        self.hardware_volume=hardware;
        self.output_gain=if hardware {planned_gain}else{1.0};
        self.soft_volume=if hardware {planned_soft}else{desired};
        self.applied_volume=desired;
        volume_trace.stage(VolumePhase::Rebase);
        if let Some(held)=&mut self.parked {held.rebase_output(self.output_gain)?;}
        if let Some(held)=&mut self.pending_output {held.rebase_output(self.output_gain)?;}
        if let Some(sink)=&mut self.sink {
            if running {
                volume_trace.completed(None);
                volume_trace.stage(VolumePhase::Retarget);
                sink.retarget_running_gain(self.blocking,old_soft,self.soft_volume,self.output_gain)?;
                let frames=sink.retained_frames_snapshot();
                volume_trace.completed(frames);
                volume_trace.stage(VolumePhase::Resume);
                sink.resume(self.blocking)?;
                volume_trace.completed(frames);
            } else {
                sink.lower_paused_gain(self.blocking,previous,desired);
                sink.change_output_gain(self.blocking,self.output_gain)?;
                if follows_codec {
                    sink.scale_running_volume_ceiling(desired);
                } else if live_codec && desired == 0.0 {
                    sink.scale_running_volume_ceiling(0.0);
                }
                volume_trace.completed(sink.retained_frames_snapshot());
            }
        } else {
            volume_trace.completed(None);
        }
        if mixed {
            let intermediate = (f64::from(previous) * planned_gain / previous_codec_gain) as f32;
            let sink = self.sink.as_mut().unwrap();
            // Queued PCM follows the codec ramp first. Continue from that
            // audible level on new PCM so neither stage overshoots the target.
            sink.defer_running_volume(intermediate, desired);
            self.audible_volume_ramp.start(intermediate, sink.rate);
            self.audible_volume_ramp.target(desired, sink.rate);
        } else if steady_codec {
            if previous != desired {
                let sink = self.sink.as_mut().unwrap();
                sink.defer_running_volume(previous, desired);
                self.audible_volume_ramp.target(desired, sink.rate);
            }
        } else if follows_codec && self.audible_volume_ramp.active() {
            self.audible_volume_ramp.scale(planned_gain / previous_codec_gain);
        } else {
            self.audible_volume_ramp.start(desired, self.sink.as_ref().map_or(48000, |sink| sink.rate));
        }
        if hardware && old_soft!=self.soft_volume {
            let rate=self.sink.as_ref().map(|sink|sink.rate).or_else(||self.dec.as_ref().map(|dec|dec.rate)).unwrap_or(48000);
            // Compensation must move with the codec domain, not fade again as
            // though normalization were a second listener volume command.
            self.volume_ramp.start(self.soft_volume,rate);
        }
        volume_trace.finish();
        Ok(())
    }
}
