//! Cumulative listening receipts, independent of durability checkpoint cadence.
//! Cursor advances count at the output tempo; seeks and reopened decoders reset
//! the observation boundary. A paused cursor cannot manufacture listening time.
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub started_ms: u64,
    pub start_position_ms: u64,
    pub listened_ms: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub last_listened_ms: u64,
}

fn is_zero(value: &u64) -> bool { *value == 0 }

#[derive(Default)]
pub(super) struct Tracker {
    logical: String,
    session: Option<Session>,
    cursor: Option<(u64, u64, u64, u16)>,
    remainder: u64,
}

impl Tracker {
    pub fn observe(&mut self, logical: &str, position: u64, generation: u64, seek: u64, speed: u16, epoch: u64) {
        if self.logical != logical {
            self.logical = logical.to_owned();
            let mut bytes = [0u8; 16];
            fastrand::fill(&mut bytes);
            bytes[6] = (bytes[6] & 15) | 64;
            bytes[8] = (bytes[8] & 63) | 128;
            let hex = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
            self.session = Some(Session {
                id: format!("{}-{}-{}-{}-{}", &hex[..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..]),
                started_ms: epoch, start_position_ms: position, listened_ms: 0, last_listened_ms: 0,
            });
            self.cursor = None;
            self.remainder = 0;
        }
        if let Some((previous, previous_generation, previous_seek, previous_speed)) = self.cursor {
            if generation == previous_generation && seek == previous_seek && speed == previous_speed && position >= previous {
                let numerator = position.saturating_sub(previous).saturating_mul(100).saturating_add(self.remainder);
                let speed = u64::from(speed.max(1));
                if let Some(session) = &mut self.session {
                    let heard_ms = numerator / speed;
                    if heard_ms > 0 {
                        session.listened_ms = session.listened_ms.saturating_add(heard_ms);
                        session.last_listened_ms = epoch;
                    }
                }
                self.remainder = numerator % speed;
            } else {
                self.remainder = 0;
            }
        }
        self.cursor = Some((position, generation, seek, speed));
    }

    pub fn snapshot(&self) -> Option<Session> {
        self.session.as_ref().filter(|session| session.listened_ms > 0).cloned()
    }

    pub fn clear(&mut self) { *self = Self::default(); }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pause_seek_reopen_and_speed_do_not_manufacture_listening() {
        let mut t = Tracker::default();
        t.observe("book", 120_000, 1, 0, 100, 1000);
        assert!(t.snapshot().is_none());
        t.observe("book", 122_000, 1, 0, 100, 3000);
        let first = t.snapshot().unwrap();
        for n in 0..20 { t.observe("book", 122_000, 1, 0, 100, 4000+n*30_000); }
        assert_eq!(t.snapshot().unwrap(), first);
        t.observe("book", 3_600_000, 1, 1, 100, 700_000);
        t.observe("book", 3_602_000, 1, 1, 100, 702_000);
        t.observe("book", 3_602_000, 2, 1, 200, 703_000);
        t.observe("book", 3_606_000, 2, 1, 200, 705_000);
        assert_eq!(t.snapshot().unwrap().listened_ms, 6000);
        assert_eq!(t.snapshot().unwrap().id, first.id);
        t.observe("other book", 0, 3, 1, 100, 706_000);
        assert!(t.snapshot().is_none());
        t.observe("other book", 1000, 3, 1, 100, 707_000);
        assert_ne!(t.snapshot().unwrap().id, first.id);
    }
}
