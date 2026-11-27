//! Exercise the real Sink write/recover/record-written path through typed C ABI
//! callbacks. No WriteFixture shortcut, physical PCM, added production query or
//! runtime policy change. API acceptance and simulated emission stay separate.
use super::*;
use std::sync::{Arc, Mutex};

use crate::audio::pcm_ffi::fixture::{self, RawPcm, Step};

fn source(frames: usize, channels: usize) -> Vec<i32> {
    (0..frames * channels).map(|n| (n as i32).wrapping_mul(100003).wrapping_add(97)).collect()
}
fn sink<'a>(blocking: &'a crate::rt::Blocking, pcm: &[i32], steps: Vec<Step>, recovery_codes: Vec<c_int>) -> (Sink<'a>, Arc<Mutex<RawPcm>>) {
    sink_with_carry(blocking, pcm, steps, recovery_codes, true)
}
fn sink_with_carry<'a>(blocking: &'a crate::rt::Blocking, pcm: &[i32], steps: Vec<Step>, recovery_codes: Vec<c_int>, carry: bool) -> (Sink<'a>, Arc<Mutex<RawPcm>>) {
    let (handle, state) = fixture::handle(blocking, pcm, steps, recovery_codes, carry);
    let mut sink = Sink::open(blocking, FAKE, 48000, 2, 400000).unwrap();
    sink.pcm = Some(crate::audio::pcm_backend::Handle::Alsa(handle));
    assert!(sink.write_fixture.is_none() && sink.fake_capture.is_none() && sink.fake_delay_frames.is_none());
    (sink, state)
}

#[test]
fn actual_write_path_preserves_short_tails_and_retry_pointers_across_source_chunks() {
        let blocking = crate::rt::Blocking::dedicated_thread();
    let pcm = source(3 * 4096, 2);
    let steps = vec![Step::Accept(4080), Step::Error { code: -libc::EINTR, consume_before: 0 }, Step::Accept(16),
                     Step::Accept(4064), Step::Error { code: -libc::EINTR, consume_before: 0 }, Step::Accept(32),
                     Step::Accept(4048), Step::Error { code: -libc::EPIPE, consume_before: usize::MAX }, Step::Accept(48)];
    let (mut output, state) = sink(&blocking, &pcm, steps, vec![0, 0, 0]);
    for chunk in pcm.chunks(4096 * 2) { output.write_progress(&blocking, chunk).unwrap(); }
    output.drain(&blocking);
    drop(output);
    let state = state.lock().unwrap();
    assert_eq!(state.violation, None);
    assert_eq!(state.requests, [4096, 16, 16, 4096, 32, 32, 4096, 48, 48]);
    assert_eq!(state.recoveries, [(-libc::EINTR, 1), (-libc::EINTR, 1), (-libc::EPIPE, 1)]);
    assert_eq!(state.delay_calls, 9);
    assert_eq!(state.accepted, pcm);
    assert_eq!(state.emitted, pcm);
    assert_eq!(state.discarded_by_recover, 0);
    assert_eq!(state.closes, 1);
}

#[test]
fn actual_fatal_eagain_keeps_accepted_prefix_and_reports_exact_unsubmitted_suffix() {
        let blocking = crate::rt::Blocking::dedicated_thread();
    let pcm = source(4096, 2);
    let (mut output, state) = sink(&blocking, &pcm, vec![Step::Accept(4080), Step::Error { code: -libc::EAGAIN, consume_before: 0 }], vec![-libc::EAGAIN]);
    let error = output.write_progress(&blocking, &pcm).unwrap_err();
    assert_eq!(error.accepted_frames, 4080);
    let mut complete = output.retained(&blocking).unwrap().samples;
    complete.extend_from_slice(&pcm[error.accepted_frames as usize * 2..]);
    assert_eq!(complete, pcm);
    let state = state.lock().unwrap();
    assert_eq!(state.violation, None);
    assert_eq!(state.requests, [4096, 16]);
    assert_eq!(state.accepted, pcm[..4080 * 2]);
    assert!(state.emitted.is_empty());
    assert_eq!(state.queued, pcm[..4080 * 2]);
}

#[test]
fn successful_recovery_can_lose_previously_accepted_unheard_frames_without_remainder_bug() {
        let blocking = crate::rt::Blocking::dedicated_thread();
    let pcm = source(2 * 4096, 2);
    let (mut output, state) = sink(&blocking, &pcm, vec![Step::Accept(4096), Step::Error { code: -libc::EPIPE, consume_before: 4048 }, Step::Accept(4096)], vec![0]);
    for chunk in pcm.chunks(4096 * 2) { output.write_progress(&blocking, chunk).unwrap(); }
    // This deliberately preserves the existing causal limitation: correct API
    // acceptance/retry does not certify emission of the pre-recovery queue.
    assert_eq!(output.retained(&blocking).unwrap().samples, pcm[4096 * 2..]);
    output.drain(&blocking);
    let state = state.lock().unwrap();
    assert_eq!(state.violation, None);
    assert_eq!(state.accepted, pcm);
    assert_eq!(state.discarded_by_recover, 48);
    let mut expected = pcm[..4048 * 2].to_vec();
    expected.extend_from_slice(&pcm[4096 * 2..]);
    assert_eq!(state.emitted, expected);
    assert_ne!(state.emitted, pcm);
}

#[test]
fn actual_zero_or_oversized_api_progress_is_bounded_without_acceptance() {
        let blocking = crate::rt::Blocking::dedicated_thread();
    for count in [0, 4097] {
        let pcm = source(4096, 2);
        let (mut output, state) = sink(&blocking, &pcm, vec![Step::Accept(count)], vec![]);
        assert_eq!(output.write_progress(&blocking, &pcm).unwrap_err().accepted_frames, 0);
        let state = state.lock().unwrap();
        assert_eq!(state.requests, [4096]);
        assert!(state.accepted.is_empty() && state.emitted.is_empty() && state.queued.is_empty());
        assert!(state.recoveries.is_empty());
        assert_eq!(state.delay_calls, 1);
    }
}


#[test]
fn actual_recovery_loses_the_five_observed_sixteen_frame_tails_after_acceptance() {
        let blocking = crate::rt::Blocking::dedicated_thread();
    // Parameters come from the independent declared-source oracle, not a
    // proposed recovery policy. Backend emission is an explicit primitive.
    // C source control proves the modeled EPIPE -> PREPARE queue reset; this
    // executes the real C6-identical Rust write/recover/record-written path.
    let boundaries = [28672usize, 57344, 86016, 163840, 217088];
    let pcm = source(288000, 2);
    let (mut output, state) = sink(&blocking, &pcm, vec![], vec![]);
    for (index, chunk) in pcm.chunks(4096 * 2).enumerate() {
        let start = index * 4096;
        let retained = if boundaries.contains(&start) { 16 } else { 0 };
        {
            let mut backend = state.lock().unwrap();
            let queued = backend.queued.len() / 2;
            assert!(queued >= retained);
            let heard: Vec<_> = backend.queued.drain(..(queued - retained) * 2).collect();
            backend.emitted.extend(heard);
            if retained != 0 {
                assert_eq!(backend.queued, pcm[(start - 16) * 2..start * 2]);
                assert_eq!(output.tail.snapshot(16, 2).unwrap(), backend.queued);
                backend.steps.push_back(Step::Error { code: -libc::EPIPE, consume_before: 0 });
            }
            backend.steps.push_back(Step::Accept(chunk.len() / 2));
        }
        output.write_progress(&blocking, chunk).unwrap();
        if retained != 0 {
            // The actual entry delay retained16, then actual recover reset the
            // backend. record_written appends new data and retains new delay,
            // so its old16-frame RAM shadow is no longer represented.
            let backend = state.lock().unwrap();
            assert!(backend.delay_history.contains(&(start, 16)));
            assert_eq!(backend.delay_history.last(), Some(&(start + chunk.len() / 2, chunk.len() / 2)));
            assert_eq!(output.tail.frames(2), (chunk.len() / 2) as u64);
            assert_eq!(output.tail.snapshot(output.tail.frames(2), 2).unwrap(), chunk);
        }
    }
    output.drain(&blocking);
    drop(output);
    let backend = state.lock().unwrap();
    assert_eq!(backend.violation, None);
    assert_eq!(backend.accepted, pcm);
    assert_eq!(backend.recoveries, vec![(-libc::EPIPE, 1); 5]);
    assert_eq!(backend.discarded_by_recover, 80);
    let discarded: Vec<_> = boundaries.iter().map(|end| pcm[(end - 16) * 2..end * 2].to_vec()).collect();
    assert_eq!(backend.discarded_payloads, discarded);
    let mut expected = Vec::new();
    for frame in 0..288000 {
        if !boundaries.iter().any(|end| (*end - 16..*end).contains(&frame)) {
            expected.extend_from_slice(&pcm[frame * 2..frame * 2 + 2]);
        }
    }
    assert_eq!(backend.emitted, expected);
    assert_ne!(backend.emitted, pcm, "successful API recovery is not exact audio delivery");
    assert!(backend.queued.is_empty());
    assert_eq!(backend.closes, 1);
}

#[test]
fn actual_failed_prepare_before_reset_keeps_pending_sixteen_and_refuses_the_new_write() {
        let blocking = crate::rt::Blocking::dedicated_thread();
    let pcm = source(8192, 2);
    let (mut output, state) = sink(&blocking, &pcm, vec![Step::Accept(4096), Step::Error { code: -libc::EPIPE, consume_before: 4080 }], vec![-libc::EIO]);
    output.write_progress(&blocking, &pcm[..4096 * 2]).unwrap();
    let error = output.write_progress(&blocking, &pcm[4096 * 2..]).unwrap_err();
    assert_eq!(error.accepted_frames, 0);
    let retained = output.retained(&blocking).unwrap();
    assert_eq!(retained.samples, pcm[4080 * 2..4096 * 2]);
    let backend = state.lock().unwrap();
    assert_eq!(backend.discarded_by_recover, 0);
    assert_eq!(backend.queued, retained.samples);
    assert_eq!(backend.emitted, pcm[..4080 * 2]);
    let mut complete = backend.emitted.clone();
    complete.extend_from_slice(&retained.samples);
    complete.extend_from_slice(&pcm[4096 * 2..]);
    assert_eq!(complete, pcm);
}

#[test]
fn optional_usb_carry_admission_preserves_unsupported_and_operational_errors_without_queries() {
        let blocking = crate::rt::Blocking::dedicated_thread();
    for code in [0, -libc::EOPNOTSUPP, -libc::ENOTTY, -libc::EIO, -libc::ENOMEM] {
        let (mut output, state) = sink(&blocking, &[], vec![], vec![]);
        state.lock().unwrap().carry_code = code;
        let result = output.enable_xrun_carry(&blocking);
        match code {
            0 => assert_eq!(result, Ok(true)),
            code if code == -libc::EOPNOTSUPP || code == -libc::ENOTTY => assert_eq!(result, Ok(false)),
            _ => assert!(result.is_err()),
        }
        drop(output);
        let backend = state.lock().unwrap();
        assert_eq!(backend.carry_options, [1]);
        assert_eq!(backend.delay_calls, 0);
        assert!(backend.requests.is_empty() && backend.recoveries.is_empty());
        assert_eq!(backend.closes, 1);
    }
    let (mut output, state) = sink_with_carry(&blocking, &[], vec![], vec![], false);
    assert_eq!(output.enable_xrun_carry(&blocking), Ok(false));
    assert!(state.lock().unwrap().carry_options.is_empty());
}

#[test]
fn actual_sink_uses_backend_owned_carry_without_replaying_previously_accepted_source() {
        let blocking = crate::rt::Blocking::dedicated_thread();
    // The kernel control independently exercises the raw carry/fence. This
    // typed backend models its admitted queue preservation, not a real delay
    // witness or HCD. Rust must retry only the current unaccepted input.
    let boundaries = [28672usize, 57344, 86016, 163840, 217088];
    let pcm = source(288000, 2);
    let (mut output, state) = sink(&blocking, &pcm, vec![], vec![]);
    assert_eq!(output.enable_xrun_carry(&blocking), Ok(true));
    for (index, chunk) in pcm.chunks(4096 * 2).enumerate() {
        let start = index * 4096;
        let retained = if boundaries.contains(&start) { 16 } else { 0 };
        {
            let mut backend = state.lock().unwrap();
            let heard = backend.queued.len() - retained * 2;
            let samples: Vec<_> = backend.queued.drain(..heard).collect();
            backend.emitted.extend(samples);
            if retained != 0 { backend.steps.push_back(Step::Error { code: -libc::EPIPE, consume_before: 0 }); }
            backend.steps.push_back(Step::Accept(chunk.len() / 2));
        }
        output.write_progress(&blocking, chunk).unwrap();
        let backend = state.lock().unwrap();
        assert_eq!(backend.queued, pcm[(start - retained) * 2..start * 2 + chunk.len()]);
        assert_eq!(output.tail.frames(2) as usize, chunk.len() / 2 + retained);
    }
    output.drain(&blocking);drop(output);
    let backend = state.lock().unwrap();
    assert_eq!(backend.carry_options, [1]);
    assert_eq!(backend.violation, None);
    assert_eq!(backend.accepted, pcm);
    assert_eq!(backend.emitted, pcm);
    assert_eq!(backend.discarded_by_recover, 0);
    assert_eq!(backend.recoveries, vec![(-libc::EPIPE, 1); 5]);
    assert_eq!(backend.closes, 1);
}

#[test]
fn owned_pcm_rejects_unaligned_slice_before_ffi_and_closes_once() {
    // Inference is deliberately ambiguous if either owner implements Send.
    // This protects fake sinks too: no PCM pointer is needed for confinement.
    trait AmbiguousIfSend<A> { fn check() {} }
    impl<T: ?Sized> AmbiguousIfSend<()> for T {}
    struct Invalid;
    impl<T: ?Sized + Send> AmbiguousIfSend<Invalid> for T {}
    let _ = <Sink<'static> as AmbiguousIfSend<_>>::check;
    let _ = <Handle<'static> as AmbiguousIfSend<_>>::check;

        let blocking = crate::rt::Blocking::dedicated_thread();
    let (handle, state) = fixture::handle(&blocking, &[1, 2], vec![], vec![], true);
    assert_eq!(handle.write(&blocking, &[1]), -i64::from(libc::EINVAL));
    assert!(state.lock().unwrap().requests.is_empty());
    drop(handle);
    assert_eq!(state.lock().unwrap().closes, 1);
}

#[test]
fn actual_partial_write_keeps_gain_provenance_in_queued_and_unsubmitted_pcm() {
    let blocking = crate::rt::Blocking::dedicated_thread();
    let original = source(64,2);
    let mut held = super::super::retained::RetainedPcm::at_output_gain(original.clone(),48000,2,0.1);
    held.rebase_output(0.3).unwrap();
    let (mut output,state) = sink(&blocking,&held.samples,
        vec![Step::Accept(31),Step::Error {code:-libc::EAGAIN,consume_before:7}],vec![-libc::EAGAIN]);
    output.configure_output_gain(0.3).unwrap();
    let failure = output.write_retained(&blocking,&held).unwrap_err();
    assert_eq!(failure.accepted_frames,31);
    let mut remaining = output.retained(&blocking).unwrap();
    assert_eq!(remaining.frames(),24);
    let mut pending = held.suffix(31*2);
    remaining.rebase_output(0.1).unwrap();
    pending.rebase_output(0.1).unwrap();
    let recovered: Vec<_> = remaining.samples.into_iter().chain(pending.samples).collect();
    assert_eq!(recovered,original[7*2..],"keep canonical values across the actual write callback boundary");
    let state = state.lock().unwrap();
    assert_eq!(state.violation,None);
    assert_eq!(state.emitted,held.samples[..7*2]);
}

#[test]
fn s16_actual_ffi_short_writes_and_recovery_keep_quantized_suffix_and_wide_shadow() {
    let blocking=crate::rt::Blocking::dedicated_thread();
    for exact in [false,true] {
        let pcm:Vec<i32>=source(64,2).into_iter().map(|v|if exact {v&!65535}else{v}).collect();
        let mut quantized=Vec::new();Quantizer::default().render(&pcm,&mut quantized);
        let expected:Vec<_>=quantized.iter().map(|&v|i32::from(v)<<16).collect();
        let (handle,state)=fixture::handle_format(&blocking,&expected,vec![Step::Accept(7),
            Step::Error {code:-libc::EINTR,consume_before:0},Step::Accept(3)],vec![0],true,PcmFormat::S16);
        let mut output=Sink::open_format(&blocking,FAKE,48000,2,400000,PcmFormat::S16).unwrap();output.pcm=Some(crate::audio::pcm_backend::Handle::Alsa(handle));
        output.write_progress(&blocking,&pcm).unwrap();
        let held=output.retained(&blocking).unwrap();
        assert_eq!(held.format,PcmFormat::S16);assert_eq!(held.samples,pcm);
        output.drain(&blocking);drop(output);
        let state=state.lock().unwrap();
        assert_eq!(state.violation,None);assert_eq!(state.accepted,expected);assert_eq!(state.emitted,expected);
        assert_eq!(state.requests,[64,57,57,54]);assert_eq!(state.closes,1);
    }
}
