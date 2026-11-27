#![forbid(unsafe_code)]
//! The PCM a sink writes to: libasound, or the in-process Bluetooth PCM
//! (`bluetooth::pcm`), which follows the same states and return codes.
use super::pcm_format::PcmFormat;
use crate::bluetooth::pcm as bt;
use std::ffi::c_int;

pub(crate) enum Handle<'a> {
    Alsa(super::pcm_ffi::Handle<'a>),
    Bluetooth { writer: bt::Writer, format: PcmFormat },
}

impl<'a> Handle<'a> {
    pub(super) fn open(blocking: &'a crate::rt::Blocking, device: &str, rate: u32, channels: u32, latency_us: u32, converted: bool, format: PcmFormat) -> Result<Self, String> {
        if device == bt::DEVICE {
            return Ok(Self::Bluetooth { writer: bt::Writer::open(bt::endpoint(), rate, channels, latency_us)?, format });
        }
        super::pcm_ffi::Handle::open(blocking, device, rate, channels, latency_us, converted, format).map(Self::Alsa)
    }
    pub(super) fn capacity(&self, blocking: &crate::rt::Blocking) -> Result<u64, String> {
        match self { Self::Alsa(pcm) => pcm.capacity(blocking), Self::Bluetooth { writer, .. } => Ok(writer.capacity()) }
    }
    pub(super) fn write(&self, blocking: &crate::rt::Blocking, samples: &[i32]) -> i64 {
        match self {
            Self::Alsa(pcm) => pcm.write(blocking, samples),
            Self::Bluetooth { writer, format } => if *format == PcmFormat::S32 { writer.write(blocking, samples) } else { -i64::from(libc::EINVAL) },
        }
    }
    pub(super) fn write_s16(&self, blocking: &crate::rt::Blocking, samples: &[i16]) -> i64 {
        match self {
            Self::Alsa(pcm) => pcm.write_s16(blocking, samples),
            Self::Bluetooth { writer, format } => if *format == PcmFormat::S16 { writer.write_s16(blocking, samples) } else { -i64::from(libc::EINVAL) },
        }
    }
    pub(super) fn recover(&self, blocking: &crate::rt::Blocking, code: c_int) -> c_int {
        match self { Self::Alsa(pcm) => pcm.recover(blocking, code), Self::Bluetooth { writer, .. } => writer.recover(code) }
    }
    pub(super) fn error(&self, blocking: &crate::rt::Blocking, code: c_int) -> String {
        match self { Self::Alsa(pcm) => pcm.error(blocking, code), Self::Bluetooth { .. } => bt::Writer::error(code) }
    }
    pub(super) fn delay(&self, blocking: &crate::rt::Blocking) -> Result<u64, String> {
        match self { Self::Alsa(pcm) => pcm.delay(blocking), Self::Bluetooth { writer, .. } => Ok(writer.delay()) }
    }
    pub(super) fn state(&self, blocking: &crate::rt::Blocking) -> c_int {
        match self { Self::Alsa(pcm) => pcm.state(blocking), Self::Bluetooth { writer, .. } => writer.state() }
    }
    /// The Bluetooth PCM has no hardware pause: the sink retains its tail and drops, as for devices without one.
    pub(super) fn pause(&self, blocking: &crate::rt::Blocking, on: bool) -> c_int {
        match self { Self::Alsa(pcm) => pcm.pause(blocking, on), Self::Bluetooth { .. } => -libc::ENOSYS }
    }
    pub(super) fn reset_step(&self, blocking: &crate::rt::Blocking, prepare: bool) -> c_int {
        match self { Self::Alsa(pcm) => pcm.reset_step(blocking, prepare), Self::Bluetooth { writer, .. } => writer.reset_step(prepare) }
    }
    pub(super) fn drain(&self, blocking: &crate::rt::Blocking) {
        match self { Self::Alsa(pcm) => pcm.drain(blocking), Self::Bluetooth { writer, .. } => writer.drain(blocking) }
    }
    pub(super) fn enable_xrun_carry(&self, blocking: &crate::rt::Blocking) -> Result<bool, String> {
        match self { Self::Alsa(pcm) => pcm.enable_xrun_carry(blocking), Self::Bluetooth { .. } => Ok(false) }
    }
    pub(super) fn batch_writes(&self, blocking: &crate::rt::Blocking) -> Result<(), String> {
        match self { Self::Alsa(pcm) => pcm.batch_writes(blocking), Self::Bluetooth { .. } => Ok(()) }
    }
}

#[cfg(test)]
mod tests {
    use crate::audio::alsa::Sink;
    use crate::bluetooth::pcm as bt;

    /// The only test that uses the process's endpoint.
    #[test]
    fn a_sink_on_the_bluetooth_pcm_writes_pauses_with_its_unsent_tail_and_reports_disconnect() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let endpoint = bt::endpoint();
        assert!(Sink::open(&blocking, bt::DEVICE, 44100, 2, 400_000).err().unwrap().contains("not connected"));
        endpoint.set_available(true);
        let mut sink = Sink::open(&blocking, bt::DEVICE, 44100, 2, 400_000).unwrap();
        let pcm: Vec<i32> = (0..2 * 12000).map(|n| (n % 30000) << 16).collect();
        sink.write(&blocking, &pcm).unwrap();
        let taken = endpoint.take(2000).unwrap();
        assert_eq!((taken.rate, &taken.stereo[..4]), (44100, &[0i16, 1, 2, 3][..]));
        assert_eq!(sink.delay(&blocking), 10000);
        // No hardware pause: the sink keeps the 10000 unsent frames and empties the queue.
        assert_eq!(sink.pause(&blocking, true), Ok(true));
        assert_eq!(sink.paused_frames(), 10000);
        assert!(endpoint.take(1).is_none(), "nothing plays while paused");
        // Resume writes the kept frames again; the first of them is frame 2000.
        sink.pause(&blocking, false).unwrap();
        assert_eq!(sink.delay(&blocking), 10000);
        let taken = endpoint.take(1).unwrap();
        assert_eq!(taken.stereo, [4000, 4001]);
        endpoint.set_available(false);
        let error = sink.write(&blocking, &pcm[..2]).unwrap_err();
        assert!(error.contains("Bluetooth headphones disconnected"), "{error}");
        drop(sink);
    }
}
