//! Bounded resource reopening on the player owner, without changing selection.
use super::*;

#[derive(Default)]
pub(super) struct DescriptorRecovery {
    generation: u64,
    attempts: u8,
    progress_us: u64,
    pending: Option<i32>,
}

/// The current backend boundary carries strings. Recognize only the two exact
/// OS error classes, never export the error text or treat every I/O error alike.
fn descriptor_code(error: &str) -> Option<i32> {
    let lower = error.to_ascii_lowercase();
    if lower.contains("file descriptor in bad state") || lower.contains("os error 77") {
        Some(libc::EBADFD)
    } else if lower.contains("bad file descriptor") || lower.contains("invalid file descriptor")
        || lower.contains("os error 9)") {
        Some(libc::EBADF)
    } else { None }
}

impl DescriptorRecovery {
    fn admit(&mut self, generation: u64, code: i32) -> bool {
        if self.generation != generation {
            *self = Self { generation, ..Self::default() };
        }
        if self.attempts >= 2 { self.pending = None; return false; }
        self.attempts += 1;
        self.progress_us = 0;
        self.pending = Some(code);
        true
    }

    pub(super) fn accepted(&mut self, frames: u64, rate: u32) {
        if self.attempts == 0 || frames == 0 || rate == 0 { return; }
        if let Some(code) = self.pending.take() {
            crate::diagnostics::health::audio_recovery(code, true);
            // UX-006: an automatic recovery reports its fixed code.
            crate::ux::recovered(crate::ux::Code::Descriptor, crate::ux::Journey::None);
        }
        self.progress_us = self.progress_us.saturating_add(frames.saturating_mul(1_000_000) / u64::from(rate));
        // One successful packet cannot replenish an endlessly failing device's
        // budget. Require five seconds of output progress before a new episode.
        if self.progress_us >= 5_000_000 { self.attempts = 0; }
    }
}

impl Engine<'_> {
    /// A music stream cut by the network part-way through a song stops where
    /// it was heard, with its error, instead of skipping the song: the UI
    /// resumes it when the link returns (UX-006, UXI-003). Opening failures
    /// and other errors keep the queue machine's skip.
    pub(super) fn interrupt_stream(&mut self, error: &str, play: bool) -> bool {
        if !play || self.track_kind() != qfsm::Track::Music
            || self.dec.as_ref().map(|dec| dec.origin) != Some(Origin::Streaming) { return false; }
        let (index, position) = self.audible_cursor();
        if position == 0 { return false; }
        if !crate::ux::Code::classify(error).is_some_and(|code| crate::ux::Code::transient(code as u16)) { return false; }
        log::warn!("playback: {error}; stopped at {} s for the link to return", position / 1000);
        self.hw.mute(self.blocking, true);
        self.close_track();
        if let Some(sink) = &mut self.sink { sink.flush(self.blocking); }
        self.index = index;
        self.resume_at = position;
        self.base_ms = position;
        self.frames = 0;
        self.resume_output = false;
        self.idle_since = Instant::now();
        self.state = PlayState::Paused;
        self.error = Some(error.to_owned());
        self.save();
        self.publish(true);
        true
    }

    /// Returns true when this descriptor error was handled (including bounded
    /// exhaustion). Reopen from a conservative audible cursor, never skip a
    /// book/track or seek to the decoder's read-ahead position.
    pub(super) fn recover_descriptor(&mut self, error: &str, play: bool) -> bool {
        let Some(code) = descriptor_code(error) else { return false; };
        crate::diagnostics::health::audio_error(code);
        let (index, position) = self.audible_cursor();
        let retry = !self.handoff_frozen && self.descriptor_recovery.admit(self.generation, code);
        self.hw.mute(self.blocking, true);
        self.close_track();
        self.sink = None;
        self.hw.power_off(self.blocking);
        self.index = index;
        self.resume_at = position;
        self.base_ms = position;
        self.frames = 0;
        self.pause_admission_fault = None;
        self.resume_output = false;
        self.idle_since = Instant::now();
        self.state = if retry && play { PlayState::Buffering } else { PlayState::Paused };
        self.error = if retry { None } else {
            crate::diagnostics::health::audio_recovery(code, false);
            Some("Playback connection could not be restored. Press Play to retry.".into())
        };
        // Checkpoint the retained location before any new read can block.
        self.save();
        self.publish(true);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn descriptor_retry_budget_requires_sustained_progress_and_recognizes_only_fd_errors() {
        assert_eq!(descriptor_code("read: Invalid file descriptor (os error 9)"), Some(libc::EBADF));
        assert_eq!(descriptor_code("File descriptor in bad state"), Some(libc::EBADFD));
        assert_eq!(descriptor_code("connection reset (os error 104)"), None);
        crate::ux::reset_for_test();
        let mut recovery = DescriptorRecovery::default();
        assert!(recovery.admit(1, libc::EBADF));
        recovery.accepted(10, 48000);
        // The first output after a reopen reports the recovery's fixed code.
        let sent = crate::ux::take_sent();
        assert_eq!(sent.iter().map(|r| (r.what, r.code)).collect::<Vec<_>>(),
            [(crate::ux::What::Recovered, crate::ux::Code::Descriptor as u16)]);
        assert!(recovery.admit(1, libc::EBADF));
        assert!(!recovery.admit(1, libc::EBADF));
        recovery.accepted(5 * 48000, 48000);
        assert!(recovery.admit(1, libc::EBADF));
        assert!(recovery.admit(2, libc::EBADFD));
    }
}
