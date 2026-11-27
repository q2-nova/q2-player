//! Automatic timer edge detection. Local clock input is sampled by the caller;
//! deadlines themselves are monotonic and survive clock adjustments.
use std::time::{Duration, Instant};

pub(super) fn in_window(minute: u16, start: u16, end: u16) -> bool {
    if minute >= 1440 || start >= 1440 || end >= 1440 || start == end { return false; }
    if start < end { minute >= start && minute < end }
    else { minute >= start || minute < end }
}

#[derive(Default)]
pub(super) struct AutoSleep {
    eligible: bool,
    generation: Option<u64>,
}
impl AutoSleep {
    /// Arm on starting eligible playback or crossing into the window. An
    /// existing manual/automatic deadline always wins. A cancellation cannot
    /// rearm until playback stops, changes selection, or leaves the window.
    pub fn tick(&mut self, now: Instant, minute: u16, start: u16, end: u16,
                duration: u16, spoken_playing: bool, generation: u64,
                deadline: &mut Option<Instant>) {
        let eligible = duration > 0 && spoken_playing && in_window(minute, start, end);
        let selection_changed = self.generation != Some(generation);
        if eligible && (!self.eligible || selection_changed) && deadline.is_none() {
            *deadline = Some(now + Duration::from_secs(u64::from(duration) * 60));
        }
        self.eligible = eligible;
        self.generation = Some(generation);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn windows_and_edges_are_local_and_half_open() {
        assert!(in_window(23*60,22*60,7*60));
        assert!(in_window(0,22*60,7*60));
        assert!(!in_window(7*60,22*60,7*60));
        assert!(in_window(8*60,8*60,9*60));
        assert!(!in_window(9*60,8*60,9*60));
        assert!(!in_window(0,0,0));
        assert!(!in_window(0,1440,0));
    }
    #[test]
    fn starts_on_play_or_entry_and_does_not_rearm_after_cancel() {
        let mut auto=AutoSleep::default(); let now=Instant::now(); let mut deadline=None;
        auto.tick(now, 21*60,22*60,7*60,30,true,1,&mut deadline); assert!(deadline.is_none());
        auto.tick(now, 22*60,22*60,7*60,30,true,1,&mut deadline); assert_eq!(deadline,Some(now+Duration::from_secs(1800)));
        deadline=None;
        auto.tick(now, 23*60,22*60,7*60,30,true,1,&mut deadline); assert!(deadline.is_none());
        auto.tick(now, 23*60,22*60,7*60,30,false,1,&mut deadline);
        auto.tick(now, 23*60,22*60,7*60,30,true,1,&mut deadline); assert!(deadline.is_some());
        let manual=now+Duration::from_secs(300); deadline=Some(manual);
        auto.tick(now, 23*60,22*60,7*60,30,true,2,&mut deadline); assert_eq!(deadline,Some(manual));
    }
}
