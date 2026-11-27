//! Credit a radio title only after a conservative interval of confirmed PCM.
#[derive(Clone, Copy)]
pub enum RadioListen {
    ListenBrainz,
    LastFm { duration_ms: Option<u64> },
}

#[derive(Default)]
pub(super) struct Listen {
    metadata: Option<String>,
    track: Option<(String, String)>,
    written_at_start: u64,
    previous_written: u64,
    started_ms: u64,
    rate: u32,
    from_boundary: bool,
    listenbrainz_delivered: bool,
    lastfm_delivered: bool,
    ended: Option<Ended>,
}

struct Ended {
    track: (String, String),
    started_ms: u64,
    duration_ms: u64,
    heard_at_least: u64,
}

fn song(metadata: &str, station: &str) -> Option<(String, String)> {
    let (artist, title) = metadata.split_once(" - ")?;
    let valid = |part: &str| {
        !part.is_empty()
            && !part.eq_ignore_ascii_case(station)
            && ![
                "unknown",
                "unknown artist",
                "unknown title",
                "advertisement",
                "live stream",
            ]
            .iter()
            .any(|token| part.eq_ignore_ascii_case(token))
    };
    let (artist, title) = (artist.trim(), title.trim());
    if metadata.eq_ignore_ascii_case(station) || !valid(artist) || !valid(title) {
        return None;
    }
    Some((artist.into(), title.into()))
}

impl Listen {
    pub(super) fn update(
        &mut self,
        metadata: Option<&str>,
        station: &str,
        written: u64,
        rate: u32,
        external: bool,
        now_ms: impl FnOnce() -> u64,
    ) {
        let continuous = rate != 0 && rate == self.rate && written >= self.previous_written;
        if !continuous {
            *self = Self::default();
        }
        if metadata != self.metadata.as_deref() || rate != self.rate {
            let had_track = self.track.is_some();
            // Only ICY with both observed title boundaries supports a
            // duration estimate. External metadata may lead the audio, and
            // the first title may already be halfway through when we tune in.
            if !external && continuous && self.from_boundary && !self.lastfm_delivered {
                let duration =
                    written.saturating_sub(self.written_at_start) * 1000 / u64::from(rate);
                let qualified = (duration / 2).min(240_000);
                if let Some(track) = self.track.take().filter(|_| duration > 30_000) {
                    self.ended = Some(Ended {
                        track,
                        started_ms: self.started_ms,
                        duration_ms: duration,
                        heard_at_least: self.written_at_start + qualified * u64::from(rate) / 1000,
                    });
                }
            }
            self.metadata = metadata.map(str::to_owned);
            self.track = metadata.and_then(|metadata| song(metadata, station));
            // The decoder may have discovered the metadata during this write.
            // Start after its last frame to avoid crediting the previous song.
            self.written_at_start = written;
            self.started_ms = now_ms();
            self.rate = rate;
            self.from_boundary = continuous && had_track;
            self.listenbrainz_delivered = false;
            self.lastfm_delivered = false;
        }
        self.previous_written = written;
    }

    fn threshold(&self, seconds: u64) -> u64 {
        self.written_at_start + u64::from(self.rate) * seconds
    }

    pub(super) fn due(&self, external: bool) -> bool {
        self.ended.is_some()
            || self.track.is_some()
                && (!self.listenbrainz_delivered
                    && self.previous_written >= self.threshold(if external { 70 } else { 60 })
                    || !self.lastfm_delivered
                        && self.previous_written
                            >= self.threshold(if external { 250 } else { 240 }))
    }

    pub(super) fn qualify(
        &mut self,
        heard: u64,
        external: bool,
        mut submit: impl FnMut(&str, &str, u64, RadioListen),
    ) {
        if self
            .ended
            .as_ref()
            .is_some_and(|ended| heard >= ended.heard_at_least)
        {
            let ended = self.ended.take().unwrap();
            submit(
                &ended.track.0,
                &ended.track.1,
                ended.started_ms,
                RadioListen::LastFm {
                    duration_ms: Some(ended.duration_ms),
                },
            );
        }
        let Some((artist, title)) = &self.track else {
            return;
        };
        // Require one minute for unknown-length ListenBrainz songs and the
        // Last.fm four-minute upper bound for unknown-length Last.fm songs.
        // A separately polled feed can lead the ten-second compressed queue.
        if !self.listenbrainz_delivered && heard >= self.threshold(if external { 70 } else { 60 }) {
            self.listenbrainz_delivered = true;
            submit(artist, title, self.started_ms, RadioListen::ListenBrainz);
        }
        if !self.lastfm_delivered && heard >= self.threshold(if external { 250 } else { 240 }) {
            self.lastfm_delivered = true;
            submit(
                artist,
                title,
                self.started_ms,
                RadioListen::LastFm { duration_ms: None },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credit_requires_heard_frames_for_one_stable_song() {
        let mut l = Listen::default();
        let mut credit = Vec::new();
        let mut submit =
            |_: &str, title: &str, time, kind| credit.push((title.to_owned(), time, kind));
        l.update(
            Some("Generated Station - Live Stream"),
            "Generated Station",
            10,
            100,
            true,
            || 1,
        );
        assert!(!l.due(true));
        l.update(
            Some("Artist - First"),
            "Generated Station",
            20,
            100,
            true,
            || 2,
        );
        assert!(!l.due(true));
        l.qualify(7019, true, &mut submit);
        l.update(
            Some("Artist - Next"),
            "Generated Station",
            7020,
            100,
            true,
            || 3,
        );
        l.qualify(14019, true, &mut submit);
        l.qualify(14020, true, &mut submit);
        l.qualify(15030, true, &mut submit);
        l.update(None, "Generated Station", 15030, 100, true, || 4);
        assert!(!l.due(true));
        assert!(
            matches!(credit.as_slice(), [(title, 3, RadioListen::ListenBrainz)] if title == "Next")
        );
    }

    #[test]
    fn lastfm_needs_both_icy_boundaries_or_four_minutes() {
        let mut l = Listen::default();
        let mut credit = Vec::new();
        let mut submit =
            |_: &str, title: &str, time, kind| credit.push((title.to_owned(), time, kind));
        l.update(Some("Artist - Initial"), "Station", 1, 100, false, || 1);
        l.update(Some("Artist - Bounded"), "Station", 3_502, 100, false, || 2);
        l.qualify(3_500, false, &mut submit);
        l.update(Some("Artist - Next"), "Station", 7_002, 100, false, || 3);
        l.qualify(5_251, false, &mut submit);
        l.qualify(5_252, false, &mut submit);
        assert!(
            matches!(credit.as_slice(), [(title, 2, RadioListen::LastFm { duration_ms: Some(35_000) })] if title == "Bounded")
        );
    }
}
