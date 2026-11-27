pub(crate) mod callback;
pub mod alsa;
mod pcm_ffi;
mod pcm_backend;
pub mod pcm_format;
mod mixer_ffi;
mod retained;
mod packets;
pub mod output;
pub mod mixer;
mod gain;
mod tempo;
mod sonic_ffi;
mod checkpoints;
pub mod listening;
pub mod decoder;
pub mod player;
pub mod source;
pub mod interrupt;
pub mod readahead;

pub mod radio;

#[cfg(any(q2_eq_msa, test))]
mod eq_msa;
mod pcm_convert;
#[cfg(q2_eq_msa)]
mod pcm_msa;
/// Whether the kernel published MSA for this process (cached).
#[cfg(q2_eq_msa)]
pub(crate) fn msa_available() -> bool { eq_msa::available() }
pub mod eq;
pub mod autoeq;
pub mod eq_match;
