//! Positive DDSketch for wake durations. Mapping adapted from DataDog/Saluki;
//! see vendor/saluki-ddsketch for pinned source, license and modifications.
//! At most 64 observations per report, 1280 possible bins, no dense allocation.
use serde::{Deserialize, Serialize};

pub(super) const MAX_US: u64 = 86_400_000_000;
const MAX_COUNT: u32 = 64;
const GAMMA: f64 = 1.01 / 0.99;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum Mapping {
    #[serde(rename = "logarithmic-1pct-us-v1")]
    Logarithmic,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Sketch {
    mapping: Mapping,
    pub(super) count: u32,
    pub(super) zero_count: u32,
    pub(super) sum_us: u64,
    pub(super) min_us: u64,
    pub(super) max_us: u64,
    pub(super) bins: Vec<(u16, u32)>,
}

// Saluki's exact logarithmic mapping specialized to nonnegative integer input.
fn index(value: u64) -> u16 { ((value as f64).ln() * (1.0 / GAMMA.ln())) as u16 }
#[cfg(test)]
fn representative(index: u16) -> f64 { (f64::from(index) * GAMMA.ln()).exp() * 1.01 }

impl Sketch {
    pub(super) fn sample(value: u64) -> Option<Self> {
        if value > MAX_US { return None; }
        Some(Self { mapping: Mapping::Logarithmic, count: 1, zero_count: u32::from(value == 0),
            sum_us: value, min_us: value, max_us: value,
            bins: if value == 0 { vec![] } else { vec![(index(value), 1)] } })
    }

    pub(super) fn merge(&mut self, other: &Self) -> bool {
        if !self.valid() || !other.valid() || self.count + other.count > MAX_COUNT { return false; }
        for &(key, count) in &other.bins {
            match self.bins.binary_search_by_key(&key, |&(key, _)| key) {
                Ok(at) => self.bins[at].1 += count,
                Err(at) => self.bins.insert(at, (key, count)),
            }
        }
        self.count += other.count;
        self.zero_count += other.zero_count;
        self.sum_us += other.sum_us;
        self.min_us = self.min_us.min(other.min_us);
        self.max_us = self.max_us.max(other.max_us);
        true
    }

    pub(super) fn valid(&self) -> bool {
        if self.count == 0 || self.count > MAX_COUNT || self.zero_count > self.count
            || self.min_us > self.max_us || self.max_us > MAX_US || self.bins.len() > MAX_COUNT as usize
            || self.sum_us < self.max_us + self.min_us * u64::from(self.count - 1)
            || self.sum_us > self.min_us + self.max_us * u64::from(self.count - 1)
            || (self.zero_count > 0) != (self.min_us == 0) { return false; }
        let mut total = u64::from(self.zero_count);
        let mut previous = None;
        for &(key, count) in &self.bins {
            if count == 0 || count > MAX_COUNT || key >= 1280 || previous.is_some_and(|last| key <= last)
                || self.max_us == 0 || key > index(self.max_us)
                || (self.min_us > 0 && key < index(self.min_us)) { return false; }
            total += u64::from(count);
            previous = Some(key);
        }
        total == u64::from(self.count)
    }

    #[cfg(test)]
    fn quantile(&self, q: f64) -> f64 {
        let rank = (q * f64::from(self.count - 1)).floor() as u32;
        if rank < self.zero_count { return 0.0; }
        let mut seen = self.zero_count;
        for &(key, count) in &self.bins {
            seen += count;
            if seen > rank { return representative(key).clamp(self.min_us as f64, self.max_us as f64); }
        }
        unreachable!("validated complete count")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tails_merge_and_relative_error_cover_full_microsecond_domain() {
        let samples: Vec<u64> = (0..64).map(|i| if i == 0 { 0 } else { (1.0 + (MAX_US as f64).ln() * f64::from(i) / 63.0).exp().min(MAX_US as f64) as u64 }).collect();
        let mut whole = Sketch::sample(samples[0]).unwrap();
        let mut parts = Vec::new();
        for chunk in samples.chunks(8) {
            let mut sketch = Sketch::sample(chunk[0]).unwrap();
            for &value in &chunk[1..] { assert!(sketch.merge(&Sketch::sample(value).unwrap())); }
            parts.push(sketch);
        }
        for &value in &samples[1..] { assert!(whole.merge(&Sketch::sample(value).unwrap())); }
        let mut combined = parts.remove(0);
        for part in parts { assert!(combined.merge(&part)); }
        assert_eq!(combined, whole);
        for q in [0.0, 0.5, 0.9, 0.95, 0.99, 1.0] {
            let actual = samples[(q * 63.0_f64).floor() as usize] as f64;
            assert!((whole.quantile(q) - actual).abs() <= actual * 0.010001);
        }
        assert!(!whole.merge(&Sketch::sample(1).unwrap()));
        assert!(Sketch::sample(MAX_US + 1).is_none());
        for value in [1, 10, 1_000, 285_000, 5_000_000, MAX_US] {
            let estimate = representative(index(value));
            assert!((estimate - value as f64).abs() <= value as f64 * 0.010001);
        }
    }
    #[test]
    fn corrupt_counts_mapping_and_private_fields_are_rejected() {
        let mut sketch = Sketch::sample(123).unwrap();
        sketch.count = 2; assert!(!sketch.valid());
        let mut value = serde_json::to_value(Sketch::sample(123).unwrap()).unwrap();
        value["message"] = "private-canary".into();
        assert!(serde_json::from_value::<Sketch>(value).is_err());
    }
}
