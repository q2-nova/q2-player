//! Decoded sample production and exact pending-output accounting.
//! Decoder/DSP state must not advance past PCM that the sink has not accepted.
//! Queue transitions and output ownership remain separate from this flow.

use super::*;

impl Engine<'_> {
    fn queued_seconds(&self) -> f64 {
        self.sink.as_ref().map_or(0.0, |sink| sink.delay(self.blocking) as f64 / f64::from(sink.rate))
    }
    pub(super) fn restore_sleep_gain(&mut self) {
        if let Some(SleepFade::Falling { deadline }) = self.sleep_fade {
            let remaining = (deadline.saturating_duration_since(Instant::now()).as_secs_f64() - self.queued_seconds()).max(0.0);
            self.sleep_fade = Some(SleepFade::Restoring { start: Instant::now() + Duration::from_secs_f64(self.queued_seconds()), from: (remaining / 10.0).min(1.0) });
        }
    }
    fn sleep_envelope(&mut self, rate: u32) -> Option<(f64, f64)> {
        let fade = self.sleep_fade?;
        let queued = self.queued_seconds();
        match fade {
            SleepFade::Falling { deadline } => {
                let remaining = (deadline.saturating_duration_since(Instant::now()).as_secs_f64() - queued).max(0.0);
                Some(((remaining / 10.0).min(1.0), -1.0 / (10.0 * f64::from(rate))))
            }
            SleepFade::Restoring { start, from } => {
                let elapsed = (Instant::now() + Duration::from_secs_f64(queued)).saturating_duration_since(start).as_secs_f64();
                if elapsed >= 0.12 { self.sleep_fade = None; return None; }
                Some(((from + (1.0-from) * elapsed / 0.12).min(1.0), (1.0-from) / (0.12 * f64::from(rate))))
            }
        }
    }

    pub(super) fn accepted_output(&mut self,result:Result<u64,OutputFailure>)->bool {
        match result {
            Ok(frames)=>{
                self.frames+=frames;
                self.descriptor_recovery.accepted(frames, self.sink.as_ref().map_or(0, |sink| sink.rate));
                if frames>0 && self.pause_admission_fault.take().is_some() {self.error=None;}
                true
            }
            Err(failure)=>{
                self.frames+=failure.accepted;
                self.pending_output=Some(failure.pending);
                self.output_failed(failure.error);
                false
            }
        }
    }

    pub(super) fn flush_pending_output(&mut self)->bool {
        let Some(mut pending)=self.pending_output.take() else {return true;};
        if let Err(error)=self.ensure_sink_format(pending.rate,pending.channels,pending.required_output_gain(),pending.format) {
            self.pending_output=Some(pending);self.output_failed(error);return false;
        }
        if let Err(error)=pending.rebase_output(self.output_gain) {
            self.pending_output=Some(pending);self.output_failed(error);return false;
        }
        let result=submit_retained(self.blocking,self.sink.as_mut().unwrap(),&pending,&mut self.tap);
        let accepted=self.accepted_output(result);
        if accepted {self.error=None;}
        accepted
    }

    /// Decodes and plays one packet; false at the end of the song.
    pub(super) fn step(&mut self) -> Result<bool, String> {
        let _poll=crate::diagnostics::PollScope::enter(205);
        let d = self.dec.as_mut().ok_or("no decoder")?;
        let packet = match d.format.next_packet(self.blocking, &self.interrupt) {
            Ok(p) => p,
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                self.finish_tempo()?;
                return Ok(self.pending_output.is_some());
            }
            Err(SymError::IoError(e)) if crate::audio::source::is_cut(&e) => {
                if d.format.retains_interrupted_packet() {return Ok(true);}
                // A command cut a wait for the stream: reopen where the
                // listener is, dropping what is queued past it.
                let (index, position) = self.audible_cursor();
                self.index = index;
                self.resume_at = position;
                self.base_ms = self.resume_at;
                self.frames = 0;
                if let Some(s) = &mut self.sink {
                    s.flush(self.blocking);
                }
                self.close_track();
                return Ok(true);
            }
            Err(SymError::ResetRequired) => { self.finish_tempo()?; return Ok(self.pending_output.is_some()); }
            Err(e) => return Err(sym_err("read", e)),
        };
        if packet.track_id() != d.track_id {
            return Ok(true);
        }
        // Keep only label metadata across the decoded-buffer borrow. The
        // container rate can differ from the decoder's authoritative SignalSpec.
        let label_codec = d.decoder.codec_params().codec;
        let label_bits = d.decoder.codec_params().bits_per_sample;
        let read_done = self.tap.now();
        let decoded = match d.decoder.decode(&packet) {
            Ok(b) => b,
            Err(SymError::DecodeError(e)) => {
                log::warn!("skipping a bad packet: {e}");
                return Ok(true);
            }
            Err(e) => return Err(sym_err("decode", e)),
        };
        let decode_done = self.tap.now();
        self.tap.decoded(read_done, decode_done, decoded.frames());
        let decoded_rate=decoded.spec().rate;
        let decoded_channels=decoded.spec().channels.count() as u32;
        if decoded_rate==0 || decoded_channels==0 {return Err("Decoder returned an invalid audio format".into());}
        if d.rate!=decoded_rate {
            if self.frames!=0 {return Err("Decoded sample rate changed during playback".into());}
            d.discard_frames=d.discard_frames.saturating_mul(decoded_rate as u64)/u64::from(d.rate.max(1));
            d.rate=decoded_rate;
            if let Some(song) = self.queue.get(self.index) {
                d.label = format_label_parts(song, label_codec, label_bits, decoded_rate);
            }
        }
        d.channels=decoded_channels;
        self.source_format=super::super::pcm_format::PcmFormat::for_source(label_bits);
        let frames = decoded.frames();
        if frames == 0 {
            return Ok(true);
        }
        let skip = d.discard_frames.min(frames as u64) as usize;
        d.discard_frames -= skip as u64;
        if skip == frames { return Ok(true); }
        let sample_offset = skip * d.channels as usize;
        // A closed PCM has no audible tail to ramp from. In particular, a
        // volume change while idle must never replay the previous louder gain.
        if self.sink.is_none() { self.volume_ramp.start(self.soft_volume, d.rate); }
        let stretching = self.speed_percent != 100 && self.queue.get(self.index).is_some_and(|song| song.spoken.is_some());
        let rendered_output_gain=self.output_gain;
        if stretching {
            // Sonic's working PCM is S16. Keep original level/pitch information
            // through it; apply headphone correction and audible gain to its
            // output so attenuation does not destroy quiet speech detail.
            self.samples.convert(decoded, 1.0);
        } else {
            self.samples.convert_eq_ramped(decoded, self.track_gain, self.soft_volume,
                &mut self.eq, &mut self.volume_ramp, skip);
        }
        let (rate,ch)=(d.rate,d.channels);
        if stretching {
            if self.tempo.as_ref().is_none_or(|tempo| tempo.rate != rate || tempo.channels != ch || tempo.percent != self.speed_percent) {
                self.tempo = Some(crate::audio::tempo::Tempo::new(self.blocking, rate, ch, self.speed_percent)?);
            }
            self.tempo.as_mut().unwrap().process(self.blocking, &self.samples.samples()[sample_offset..], &mut self.tempo_output)?;
            self.samples.process_spoken_output(&mut self.tempo_output, rate, ch as usize,
                &mut self.eq, &mut self.volume_ramp, self.track_gain, self.soft_volume);
        }
        // Keep the unclipped post-DSP signal until the codec domain is chosen.
        // Unity integer playback keeps its borrowed, allocation-free path.
        let unsettled = self.audible_volume_ramp.active();
        let mut wide = self.wide_output(rate,ch,if stretching {0}else{sample_offset})?;
        if let Some((gain, step)) = self.sleep_envelope(rate) {
            let pcm = if stretching { &self.tempo_output[..] } else { &self.samples.samples()[sample_offset..] };
            let held = wide.get_or_insert_with(|| crate::audio::retained::RetainedPcm::at_output_gain(pcm.to_vec(), rate, ch, rendered_output_gain).with_format(self.source_format));
            held.attenuate_linear(gain, step);
        }
        if self.outputs.as_ref().is_some_and(|outputs| outputs.changed(self.route.generation)) {
            if let (Some(sink), Some(pending)) = (&self.sink, &mut wide) {
                sink.limit_unheard_volume(pending);
            }
        }
        let headroom=wide.as_ref().map_or(0.0,|pcm|pcm.required_output_gain());
        self.refresh_route();
        if self.state==PlayState::Paused {
            let pcm=if stretching {&self.tempo_output}else{&self.samples.samples()[sample_offset..]};
            let pending=wide.take().unwrap_or_else(||crate::audio::retained::RetainedPcm::at_output_gain(pcm.to_vec(),rate,ch,rendered_output_gain).with_format(self.source_format));
            self.pending_output=Some(pending);
            return Ok(true);
        }
        if !unsettled {
            self.remember_decoded_headroom(headroom);
        }
        if let Err(error)=self.ensure_sink_for(rate,ch,headroom) {
            let pcm=if stretching {&self.tempo_output}else{&self.samples.samples()[sample_offset..]};
            let pending=wide.take().unwrap_or_else(||crate::audio::retained::RetainedPcm::at_output_gain(pcm.to_vec(),rate,ch,rendered_output_gain).with_format(self.source_format));
            self.pending_output=Some(pending);
            self.output_failed(error);return Ok(true);
        }
        let pcm=if stretching {&self.tempo_output}else{&self.samples.samples()[sample_offset..]};
        let result=if let Some(mut rendered)=wide {
            if let Err(error)=rendered.rebase_output(self.output_gain) {
                self.pending_output=Some(rendered);self.output_failed(error);return Ok(true);
            }
            let result=submit_retained(self.blocking,self.sink.as_mut().unwrap(),&rendered,&mut self.tap);
            // A refused suffix was copied into the failure; these go to the next packet.
            self.wide_buffers=rendered.into_buffers();
            result
        } else if rendered_output_gain==self.output_gain {
            // The steady-state path does not allocate a second packet copy.
            submit_rendered(self.blocking,self.sink.as_mut().unwrap(),pcm,&mut self.tap)
        } else {
            let mut rendered=crate::audio::retained::RetainedPcm::at_output_gain(pcm.to_vec(),rate,ch,rendered_output_gain).with_format(self.source_format);
            if let Err(error)=rendered.rebase_output(self.output_gain) {
                self.pending_output=Some(rendered);self.output_failed(error);return Ok(true);
            }
            submit_retained(self.blocking,self.sink.as_mut().unwrap(),&rendered,&mut self.tap)
        };
        if !self.accepted_output(result) {return Ok(true);}
        self.state = PlayState::Playing;
        self.check_scrobble();
        Ok(true)
    }

    pub(super) fn wide_output(&mut self,rate:u32,ch:u32,skip:usize)->Result<Option<crate::audio::retained::RetainedPcm>,String> {
        let source=self.samples.pre_volume();
        let ramping = self.audible_volume_ramp.active();
        if !ramping {
            if let Some(sink) = &mut self.sink { sink.settled_running_volume(); }
        }
        if !self.hardware_volume || self.applied_volume==0.0 || (!ramping && source.integer().is_some() && self.soft_volume==1.0) {
            return Ok(None);
        }
        // The buffers of the packet before: this runs for every packet with
        // the equalizer or ReplayGain on the codec volume.
        let (mut audible,samples)=std::mem::take(&mut self.wide_buffers);
        audible.clear();
        if ramping {
            source.extend_audible_ramped(skip, ch as usize, &mut self.audible_volume_ramp, &mut audible);
        } else {
            source.extend_audible(skip, f64::from(self.applied_volume), &mut audible);
        }
        // Rendered once at the open PCM's gain; step() rebases only when the
        // packet's headroom moves the codec.
        crate::audio::retained::RetainedPcm::from_audible_near_in(audible,samples,rate,ch,self.output_gain).map(|pcm|Some(pcm.with_format(self.source_format)))
    }

    pub(super) fn finish_tempo(&mut self) -> Result<(), String> {
        if let Some(mut tempo) = self.tempo.take() {
            tempo.finish(self.blocking, &mut self.tempo_output)?;
            self.samples.process_spoken_output(&mut self.tempo_output, tempo.rate, tempo.channels as usize,
                &mut self.eq, &mut self.volume_ramp, self.track_gain, self.soft_volume);
            if self.sink.is_some() {
                let unsettled = self.audible_volume_ramp.active();
                let mut wide=self.wide_output(tempo.rate,tempo.channels,0)?;
                if let Some((gain, step)) = self.sleep_envelope(tempo.rate) {
                    let held = wide.get_or_insert_with(|| crate::audio::retained::RetainedPcm::at_output_gain(self.tempo_output.clone(), tempo.rate, tempo.channels, self.output_gain).with_format(self.source_format));
                    held.attenuate_linear(gain, step);
                }
                if let Some(mut held)=wide {
                    let headroom = held.required_output_gain();
                    if !unsettled {
                        self.remember_decoded_headroom(headroom);
                    }
                    if let Err(error)=self.ensure_sink_for(tempo.rate,tempo.channels,headroom)
                        .and_then(|()|held.rebase_output(self.output_gain)) {
                        self.pending_output=Some(held);self.output_failed(error);return Ok(());
                    }
                    let result=submit_retained(self.blocking,self.sink.as_mut().unwrap(),&held,&mut self.tap);
                    self.wide_buffers=held.into_buffers();
                    self.accepted_output(result);
                } else {
                    let result=submit_rendered(self.blocking,self.sink.as_mut().unwrap(),&self.tempo_output,&mut self.tap);
                    self.accepted_output(result);
                }
            }
        }
        Ok(())
    }

    pub(super) fn frame_timing(&self) -> Option<(u32, u64)> {
        if let Some(held)=&self.parked {return Some((held.rate,held.frames()));}
        // Until the next format's sink opens, its queued tail still uses the
        // previous sink rate. Format changes explicitly drain that tail.
        let rate = self.sink.as_ref().map(|sink| sink.rate)
            .or_else(|| self.dec.as_ref().map(|dec| dec.rate))?;
        Some((rate, self.sink.as_ref().map(|sink| sink.delay(self.blocking)).unwrap_or(0)))
    }
}
