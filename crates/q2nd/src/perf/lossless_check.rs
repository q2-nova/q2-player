#![forbid(unsafe_code)]
//! `--bench` kind "lossless-check": correctness only. Runs the vendored FLAC
//! and ALAC decoders' kernel self-tests (scalar code against the current
//! kernels on generated input), then decodes the fixture with the scalar
//! kernels forced and with the default kernels (MSA where the kernel reports
//! it) and fails unless every decoded i32 is the same. The decode cases'
//! `sample_md5` is taken after conversion at a gain and cannot see every bit.
use md5::{Digest, Md5};
use serde_json::{json, Value};
use std::path::Path;
use symphonia::core::audio::{AudioBufferRef, Signal};
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};

fn set_msa(want: Option<bool>) {
    symphonia_bundle_flac::q2::set_msa(want);
    symphonia_codec_alac::q2::set_msa(want);
}

fn decode(blocking: &crate::rt::Blocking, fixture: &Path) -> Result<(usize, String), String> {
    let file = super::open_fixture(blocking, fixture)?;
    let mut format = crate::audio::decoder::probe(blocking, Box::new(file), fixture.extension().and_then(|s| s.to_str()), false)
        .map_err(|e| e.to_string())?.format;
    let track = format.tracks().iter().find(|t| t.codec_params.codec != CODEC_TYPE_NULL).ok_or("no audio")?;
    let id = track.id;
    let mut decoder = symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default()).map_err(|e| e.to_string())?;
    let mut hash = Md5::new();
    let mut frames = 0;
    loop {
        let packet = match crate::audio::callback::with(blocking, || format.next_packet()) {
            Ok(p) => p,
            Err(symphonia::core::errors::Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.to_string()),
        };
        if packet.track_id() != id { continue; }
        let AudioBufferRef::S32(planar) = decoder.decode(&packet).map_err(|e| e.to_string())? else {
            return Err("the decoder did not return i32".into());
        };
        frames += planar.frames();
        for channel in 0..planar.spec().channels.count() {
            for sample in planar.chan(channel) { hash.update(sample.to_le_bytes()); }
        }
    }
    Ok((frames, format!("{:x}", hash.finalize())))
}

pub fn run(blocking: &crate::rt::Blocking, fixture: &Path) -> Result<Value, String> {
    set_msa(None);
    let msa = symphonia_bundle_flac::q2::msa() && symphonia_codec_alac::q2::msa();
    let flac_checks = symphonia_bundle_flac::q2::selftest()?;
    let alac_checks = symphonia_codec_alac::q2::selftest()?;
    set_msa(Some(false));
    let scalar = decode(blocking, fixture);
    set_msa(None);
    let default = decode(blocking, fixture);
    let (scalar, default) = (scalar?, default?);
    if scalar != default {
        return Err(format!("scalar {} frames {}, default {} frames {}", scalar.0, scalar.1, default.0, default.1));
    }
    Ok(json!({"frames": scalar.0, "i32_md5": scalar.1, "msa": msa, "flac_selftest": flac_checks, "alac_selftest": alac_checks}))
}
