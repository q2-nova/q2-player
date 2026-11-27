#![forbid(unsafe_code)]
//! `--bench` kind "jpeg-check": correctness only. Runs the vendored JPEG
//! decoder's kernel self-check (YCbCr to RGB rows and the chroma upsamplers:
//! the scalar code against the entries that use MSA where the kernel reports
//! it, on generated input with edge values and lengths that are not a
//! multiple of the vector width). Without a fixture it then compares them on
//! every input the decoder can pass (each y, cb, cr of 0..=255; each
//! upsampler sample and neighbour of 0..=255). With a fixture it decodes the
//! file with the scalar code forced and with the default code and compares
//! every RGB byte. Reports `identical` and `msa`, and fails on a difference.
use md5::{Digest, Md5};
use serde_json::{json, Value};
use std::path::Path;

const SEEDS: [u64; 4] = [1, 0x5EED, 0x0123_4567_89AB_CDEF, 0xFFFF_FFFF_0000_0001];
const ROUNDS: usize = 600;

fn decode(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let image = image::load_from_memory_with_format(bytes, image::ImageFormat::Jpeg).map_err(|e| e.to_string())?.into_rgb8();
    Ok((image.width(), image.height(), image.into_raw()))
}

pub fn run(blocking: &crate::rt::Blocking, fixture: &Path) -> Result<Value, String> {
    zune_jpeg::q2::set_msa(None);
    let msa = zune_jpeg::q2::msa();
    let (mut compared, mut values, mut difference) = (0, 0, None);
    for seed in SEEDS {
        let check = zune_jpeg::q2::check(seed, ROUNDS);
        compared += check.compared;
        values += check.values;
        difference = difference.or(check.difference.map(|d| format!("seed {seed:#x}: {d}")));
    }
    if let Some(difference) = difference { return Err(format!("kernels differ (msa {msa}): {difference}")); }
    let mut result = json!({"identical": true, "msa": msa, "compared": compared, "values": values});
    if fixture.as_os_str().is_empty() {
        let range = zune_jpeg::q2::check_decoder_range();
        if let Some(difference) = range.difference { return Err(format!("kernels differ in the decoder range (msa {msa}): {difference}")); }
        result["decoder_range"] = json!({"identical": true, "compared": range.compared, "values": range.values});
    } else {
        let bytes = super::read_fixture(blocking, fixture)?;
        zune_jpeg::q2::set_msa(Some(false));
        let scalar = decode(&bytes);
        zune_jpeg::q2::set_msa(None);
        let default = decode(&bytes);
        let (scalar, default) = (scalar?, default?);
        if scalar != default {
            return Err(format!("decodes differ (msa {msa}): scalar {}x{} {:x}, default {}x{} {:x}",
                scalar.0, scalar.1, Md5::digest(&scalar.2), default.0, default.1, Md5::digest(&default.2)));
        }
        result["decode"] = json!({"identical": true, "width": scalar.0, "height": scalar.1,
            "rgb_md5": format!("{:x}", Md5::digest(&scalar.2))});
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    #[test]
    fn kernel_check_passes_on_the_portable_code() {
        let result = super::run(&crate::rt::Blocking::dedicated_thread(), std::path::Path::new("")).unwrap();
        assert_eq!(result["identical"], true);
        assert_eq!(result["compared"], 4 * 4 * super::ROUNDS);
        assert_eq!(result["decoder_range"]["values"], 256 * 256 * 768 + 256 * 3072);
    }
}
