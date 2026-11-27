#![forbid(unsafe_code)]
//! `--bench` kind "mp3-check": correctness only. Decodes the fixture with the
//! vendored MP3 decoder's scalar kernels, then with its default kernels (MSA
//! where the kernel reports it), and fails unless every decoded f32 has the
//! same bits. The decode cases' `sample_md5` is taken after conversion to
//! i32 and cannot see a difference in the lowest bits of quiet samples.
use md5::{Digest, Md5};
use serde_json::{json, Value};
use std::path::Path;
use symphonia::core::audio::{AudioBufferRef, Signal};
use symphonia::core::codecs::{DecoderOptions, CODEC_TYPE_NULL};

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
        let AudioBufferRef::F32(planar) = decoder.decode(&packet).map_err(|e| e.to_string())? else {
            return Err("the decoder did not return f32".into());
        };
        frames += planar.frames();
        for channel in 0..planar.spec().channels.count() {
            for sample in planar.chan(channel) { hash.update(sample.to_bits().to_le_bytes()); }
        }
    }
    Ok((frames, format!("{:x}", hash.finalize())))
}

pub fn run(blocking: &crate::rt::Blocking, fixture: &Path) -> Result<Value, String> {
    symphonia_bundle_mp3::q2::set_msa(Some(false));
    let scalar = decode(blocking, fixture);
    symphonia_bundle_mp3::q2::set_msa(None);
    let msa = symphonia_bundle_mp3::q2::msa();
    let default = decode(blocking, fixture);
    let (scalar, default) = (scalar?, default?);
    if scalar != default {
        return Err(format!("scalar {} frames {}, default {} frames {}", scalar.0, scalar.1, default.0, default.1));
    }
    Ok(json!({"frames": scalar.0, "f32_md5": scalar.1, "msa": msa}))
}
