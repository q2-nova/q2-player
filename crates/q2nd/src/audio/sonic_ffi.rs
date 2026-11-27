//! Owning Sonic stream; format, input cadence and C buffers remain private.
use std::ffi::{c_int, c_void};
use std::ptr::NonNull;

unsafe extern "C" {
    fn sonicCreateStream(rate: c_int, channels: c_int) -> *mut c_void;
    fn sonicDestroyStream(stream: *mut c_void);
    fn sonicSetSpeed(stream: *mut c_void, speed: f32);
    fn sonicWriteShortToStream(stream: *mut c_void, samples: *const i16, frames: c_int) -> c_int;
    fn sonicReadShortFromStream(stream: *mut c_void, samples: *mut i16, frames: c_int) -> c_int;
    fn sonicFlushStream(stream: *mut c_void) -> c_int;
}

pub(super) struct Stage<'a> {
    blocking: &'a crate::rt::Blocking,
    stream: NonNull<c_void>,
    channels: u32,

    scratch: Vec<i16>,
    input: Vec<i16>,
    block_frames: usize,
}
// The player-stack witness keeps creation, processing and destruction on its owner.

impl<'a> Stage<'a> {
    pub(super) fn new(blocking: &'a crate::rt::Blocking, rate: u32, channels: u32, speed: f32) -> Result<Self, String> {
        if !(8000..=192000).contains(&rate)
            || !(1..=8).contains(&channels)
            || !(0.5..=2.0).contains(&speed)
        {
            return Err("Unsupported spoken playback speed or audio format".into());
        }
        let stream = NonNull::new(unsafe { sonicCreateStream(rate as c_int, channels as c_int) })
            .ok_or("Unable to allocate spoken speed processor")?;
        unsafe {
            sonicSetSpeed(stream.as_ptr(), speed);
        }
        Ok(Self {
            blocking,
            stream,
            channels,
            scratch: vec![0; 1024 * channels as usize],
            input: Vec::with_capacity((rate as usize / 100).min(1024) * channels as usize),
            block_frames: (rate as usize / 100).min(1024),
        })
    }

    fn read(&mut self, _blocking: &crate::rt::Blocking, output: &mut Vec<i32>) {
        loop {
            let count = unsafe {
                sonicReadShortFromStream(self.stream.as_ptr(), self.scratch.as_mut_ptr(), 1024)
            } as usize;
            if count == 0 {
                break;
            }
            output.extend(
                self.scratch[..count * self.channels as usize]
                    .iter()
                    .map(|&s| i32::from(s) << 16),
            );
        }
    }

    pub(super) fn process(&mut self, blocking: &crate::rt::Blocking, input: &[i32], output: &mut Vec<i32>) -> Result<(), String> {
        let channels = self.channels as usize;
        if input.len() % channels != 0 {
            return Err("Incomplete spoken PCM frame".into());
        }
        output.clear();
        // Sonic's play-time accounting depends on write boundaries. Submit a
        // canonical <=10 ms cadence, independent of decoder packet boundaries,
        // while retaining less than one bounded block between calls.
        let mut offset = 0;
        let block_samples = self.block_frames * channels;
        while offset < input.len() {
            let count = (block_samples - self.input.len()).min(input.len() - offset);
            self.input.extend(
                input[offset..offset + count]
                    .iter()
                    .map(|sample| (sample >> 16) as i16),
            );
            offset += count;
            if self.input.len() == block_samples {
                self.write_pending(blocking, output)?;
            }
        }
        Ok(())
    }

    fn write_pending(&mut self, blocking: &crate::rt::Blocking, output: &mut Vec<i32>) -> Result<(), String> {
        if self.input.is_empty() {
            return Ok(());
        }
        if unsafe {
            sonicWriteShortToStream(
                self.stream.as_ptr(),
                self.input.as_ptr(),
                (self.input.len() / self.channels as usize) as c_int,
            )
        } == 0
        {
            return Err("Spoken speed processor ran out of memory".into());
        }
        self.input.clear();
        self.read(blocking, output);
        Ok(())
    }

    pub(super) fn finish(&mut self, blocking: &crate::rt::Blocking, output: &mut Vec<i32>) -> Result<(), String> {
        output.clear();
        self.write_pending(blocking, output)?;
        if unsafe { sonicFlushStream(self.stream.as_ptr()) } == 0 {
            return Err("Unable to drain spoken speed processor".into());
        }
        self.read(blocking, output);
        Ok(())
    }
}

impl Stage<'_> {
    fn close(&mut self, _blocking: &crate::rt::Blocking) {
        unsafe {
            sonicDestroyStream(self.stream.as_ptr());
        }
    }
}


impl Drop for Stage<'_> {
    fn drop(&mut self) { self.close(self.blocking); }
}
