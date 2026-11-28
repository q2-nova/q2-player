#![forbid(unsafe_code)]
//! Layered headphone correction and graphic EQ. RBJ/W3C biquads, computed at
//! the actual stream rate. No allocations or coefficient design per sample.
use serde::{Deserialize, Serialize};

pub const FREQUENCIES: [f64; 10] = [31.25, 62.5, 125., 250., 500., 1000., 2000., 4000., 8000., 16000.];
pub const LABELS: [&str; 10] = ["31", "63", "125", "250", "500", "1k", "2k", "4k", "8k", "16k"];

/// Gentle taste curves for this ten-band EQ, not replicas of Apple's presets.
pub struct Preset { pub name: &'static str, pub bands: [i8;10] }
pub const PRESETS: [Preset;10] = [
    Preset {name:"Flat", bands:[0,0,0,0,0,0,0,0,0,0]},
    Preset {name:"Bass Boost", bands:[5,4,3,1,0,0,0,0,0,0]},
    Preset {name:"Bass Cut", bands:[-5,-4,-3,-1,0,0,0,0,0,0]},
    Preset {name:"Treble Boost", bands:[0,0,0,0,0,0,1,2,3,4]},
    Preset {name:"Treble Cut", bands:[0,0,0,0,0,0,-1,-2,-3,-4]},
    Preset {name:"Rock", bands:[4,3,2,0,-2,-1,1,2,3,3]},
    Preset {name:"Pop", bands:[-1,0,1,2,3,3,2,1,0,-1]},
    Preset {name:"Jazz", bands:[3,2,1,0,-1,0,1,2,3,2]},
    Preset {name:"Classical", bands:[3,2,1,0,-1,-1,0,1,2,3]},
    Preset {name:"Vocal", bands:[-3,-2,-1,0,2,3,3,1,0,-1]},
];

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub graphic_enabled: bool,
    /// Integer dB, clamped to +/-12 at the DSP boundary as well as in the UI.
    pub bands: [i8; 10],
    pub headphone_enabled: bool,
    pub headphone: Option<Profile>,
}
impl Settings {
    pub fn summary(&self) -> String {
        let headphone = self.headphone_enabled && self.headphone.as_ref().is_some_and(Profile::valid);
        match (self.graphic_enabled, headphone) {
            (true, true) => format!("{} + Headphones", self.preset_name()),
            (true, false) => self.preset_name().into(),
            (false, true) => "Headphones".into(),
            (false, false) => "Off".into(),
        }
    }

    pub fn preset_name(&self) -> &'static str {
        PRESETS.iter().find(|p|p.bands==self.bands).map_or("Custom",|p|p.name)
    }
    pub fn set_preset(&mut self, index: usize) -> bool {
        let Some(preset)=PRESETS.get(index) else {return false;};
        self.bands=preset.bands;
        self.graphic_enabled=true;
        true
    }
    fn sanitized(mut self) -> Self {
        self.bands=self.bands.map(|g|g.clamp(-12,12));
        if self.headphone.as_ref().is_some_and(|p|!p.valid()) {
            self.headphone=None;self.headphone_enabled=false;
        }
        self
    }
}
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Profile {
    pub name: String,
    pub source: String,
    pub preamp: f64,
    pub filters: Vec<Filter>,
}
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Kind { Peak, LowShelf, HighShelf }
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Filter { pub kind: Kind, pub hz: f64, pub gain: f64, pub q: f64 }
impl Filter {
    fn valid(&self) -> bool {
        self.hz.is_finite() && (10.0..=24000.0).contains(&self.hz)
            && self.gain.is_finite() && (-30.0..=30.0).contains(&self.gain)
            && self.q.is_finite() && (0.1..=20.0).contains(&self.q)
    }
}
impl Profile {
    pub fn valid(&self) -> bool {
        self.preamp.is_finite() && (-60.0..=0.0).contains(&self.preamp)
            && !self.filters.is_empty() && self.filters.len() <= 20 && self.filters.iter().all(Filter::valid)
    }
    /// AutoEQ's Equalizer APO ParametricEQ export. Reject partial/unsupported
    /// profiles instead of silently applying only some of their correction.
    pub fn parse(name: String, source: String, text: &str) -> Result<Self, String> {
        let bad = || "Unsupported or invalid AutoEQ profile".to_string();
        let mut preamp = None;
        let mut filters = Vec::new();
        if text.len() > 16384 { return Err(bad()); }
        for line in text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
            let w: Vec<_> = line.split_whitespace().collect();
            if w.len() == 3 && w[0] == "Preamp:" && w[2] == "dB" && preamp.is_none() {
                preamp = Some(w[1].parse().map_err(|_| bad())?);
            } else if w.len() == 12 && w[0] == "Filter" && w[1].ends_with(':')
                && w[4] == "Fc" && w[6] == "Hz" && w[7] == "Gain" && w[9] == "dB" && w[10] == "Q" {
                let kind = match w[3] { "PK" => Kind::Peak, "LSC" => Kind::LowShelf, "HSC" => Kind::HighShelf, _ => return Err(bad()) };
                let f = Filter { kind, hz: w[5].parse().map_err(|_| bad())?, gain: w[8].parse().map_err(|_| bad())?, q: w[11].parse().map_err(|_| bad())? };
                if !f.valid() { return Err(bad()); }
                match w[2] { "ON" => filters.push(f), "OFF" => {}, _ => return Err(bad()) }
            } else { return Err(bad()); }
        }
        let profile = Self { name, source, preamp: preamp.ok_or_else(bad)?, filters };
        if profile.valid() { Ok(profile) } else { Err(bad()) }
    }
}

#[derive(Clone)]
struct Biquad { b: [f64; 3], a: [f64; 2], state: Vec<[f64; 2]> }
impl Biquad {
    fn new(f: &Filter, rate: u32, channels: usize) -> Self {
        let a = 10f64.powf(f.gain / 40.0);
        let w = std::f64::consts::TAU * f.hz / rate as f64;
        let c = w.cos();
        let alpha = w.sin() / (2.0 * f.q);
        let beta = 2.0 * a.sqrt() * alpha;
        let (b, d) = match f.kind {
            Kind::Peak => ([1.0 + alpha*a, -2.0*c, 1.0-alpha*a], [1.0+alpha/a, -2.0*c, 1.0-alpha/a]),
            Kind::LowShelf => ([a*((a+1.0)-(a-1.0)*c+beta), 2.0*a*((a-1.0)-(a+1.0)*c), a*((a+1.0)-(a-1.0)*c-beta)],
                [(a+1.0)+(a-1.0)*c+beta, -2.0*((a-1.0)+(a+1.0)*c), (a+1.0)+(a-1.0)*c-beta]),
            Kind::HighShelf => ([a*((a+1.0)+(a-1.0)*c+beta), -2.0*a*((a-1.0)+(a+1.0)*c), a*((a+1.0)+(a-1.0)*c-beta)],
                [(a+1.0)-(a-1.0)*c+beta, 2.0*((a-1.0)-(a+1.0)*c), (a+1.0)-(a-1.0)*c-beta]),
        };
        Self { b: b.map(|v| v/d[0]), a: [d[1]/d[0], d[2]/d[0]], state: vec![[0.0;2];channels] }
    }
    fn sample(&mut self, x: f64, channel: usize) -> f64 {
        let s = &mut self.state[channel];
        let y = self.b[0]*x+s[0];
        s[0] = self.b[1]*x-self.a[0]*y+s[1];
        s[1] = self.b[2]*x-self.a[1]*y;
        // Avoid subnormal work after silence.
        for v in s { if v.abs()<1e-24 { *v=0.0; } }
        y
    }
    fn stereo(&mut self, x: [f64; 2]) -> [f64; 2] {
        let [left, right] = self.state.as_mut_slice() else { unreachable!("stereo filter history") };
        // Independent channels share coefficients. Keep each channel's scalar
        // operation order and precision, exposing independent FPU operations.
        let y = [self.b[0]*x[0]+left[0], self.b[0]*x[1]+right[0]];
        left[0] = self.b[1]*x[0]-self.a[0]*y[0]+left[1];
        right[0] = self.b[1]*x[1]-self.a[0]*y[1]+right[1];
        left[1] = self.b[2]*x[0]-self.a[1]*y[0];
        right[1] = self.b[2]*x[1]-self.a[1]*y[1];
        // Fixed-size loops let the scalar target unroll the silence checks.
        // A chained iterator adds a branchy iterator state machine per filter.
        for v in left {
            if v.abs()<1e-24 { *v=0.0; }
        }
        for v in right {
            if v.abs()<1e-24 { *v=0.0; }
        }
        y
    }
}
/// A biquad's power gain |H|² as a ratio of quadratics in s = sin²(ω/2), 0
/// at 0 Hz and 1 at Nyquist. In cos ω the squared terms cancel near 0 Hz
/// and lose most of their digits; in s the constant term is the squared
/// coefficient sum, which keeps them.
#[derive(Clone, Copy)]
struct Power { num: [f64; 3], den: [f64; 3] }
impl Power {
    fn new(f: &Biquad) -> Self {
        let q = |v: [f64; 3]| [(v[0]+v[1]+v[2]).powi(2), -4.0*(v[1]*(v[0]+v[2])+4.0*v[0]*v[2]), 16.0*v[0]*v[2]];
        Self { num: q(f.b), den: q([1.0, f.a[0], f.a[1]]) }
    }
    fn at_hz(&self, hz: f64, rate: u32) -> f64 { self.at((std::f64::consts::PI*hz/rate as f64).sin().powi(2)) }
    fn at(&self, s: f64) -> f64 {
        let q = |k: [f64; 3]| (k[2]*s+k[1])*s+k[0];
        q(self.num).max(1e-30)/q(self.den).max(1e-30)
    }
    /// The s where the gain turns, at most two: the roots of the quadratic
    /// N′D − ND′ (its cubic terms cancel). Between them it is monotonic.
    fn turns(&self) -> impl Iterator<Item = f64> {
        let ([n0, n1, n2], [d0, d1, d2]) = (self.num, self.den);
        let (a, b, c) = (n2*d1-n1*d2, 2.0*(n2*d0-n0*d2), n1*d0-n0*d1);
        let disc = b*b-4.0*a*c;
        let q = -0.5*(b+b.signum()*disc.max(0.0).sqrt());
        let roots = if a == 0.0 { [-c/b, f64::NAN] } else if disc < 0.0 { [f64::NAN; 2] } else { [q/a, c/q] };
        roots.into_iter().filter(|s| (0.0..=1.0).contains(s))
    }
}

/// Power gains within this factor (0.01 dB) of each other settle the peak search.
const PEAK_TOLERANCE: f64 = 1.002_305_238_077_899_7;

/// An upper bound on the chain's largest power gain from 0 Hz to Nyquist,
/// at most PEAK_TOLERANCE above it, and never below 1. The search starts
/// from 0 Hz, Nyquist and every filter's turning points. Each filter is
/// then monotonic between neighbouring points, so the product of each
/// filter's larger end value bounds the chain between them. An interval
/// whose bound is within the tolerance of the largest gain found at a
/// point is settled; any other is halved at a new point.
fn peak_power(filters: &[Biquad]) -> f64 {
    let powers: Vec<_> = filters.iter().map(Power::new).collect();
    let mut points: Vec<f64> = [0.0, 1.0].into_iter().chain(powers.iter().flat_map(Power::turns)).collect();
    points.sort_by(f64::total_cmp);
    points.dedup();
    // Each point's gain through each filter, `n` per point.
    let n = powers.len();
    let mut gains: Vec<f64> = points.iter().flat_map(|&s| powers.iter().map(move |p| p.at(s))).collect();
    let at = |gains: &[f64], k: usize| gains[k*n..(k+1)*n].iter().product::<f64>();
    let mut found = (0..points.len()).map(|k| at(&gains, k)).fold(1.0, f64::max);
    let mut open: Vec<_> = (1..points.len()).map(|k| (points[k-1], k-1, points[k], k)).collect();
    let mut peak: f64 = 1.0;
    while let Some((lo, i, hi, j)) = open.pop() {
        let bound = (0..n).map(|f| gains[i*n+f].max(gains[j*n+f])).product::<f64>();
        let mid = 0.5*(lo+hi);
        if bound <= found*PEAK_TOLERANCE || mid <= lo || mid >= hi { peak = peak.max(bound); continue; }
        let k = gains.len()/n;
        gains.extend(powers.iter().map(|p| p.at(mid)));
        found = found.max(at(&gains, k));
        open.push((lo, i, mid, k));
        open.push((mid, k, hi, j));
    }
    peak
}

#[derive(Clone, Default)]
struct Chain { filters: Vec<Biquad>, gain: f64 }
impl Chain {
    fn new(s: &Settings, rate: u32, channels: usize) -> Self {
        let mut specs = Vec::new();
        let mut preamp: f64 = 0.0;
        if let Some(p) = s.headphone.as_ref().filter(|p| s.headphone_enabled && p.valid()) {
            specs.extend(p.filters.iter().cloned()); preamp = p.preamp;
        }
        if s.graphic_enabled {
            specs.extend(FREQUENCIES.iter().zip(s.bands).filter(|(_,g)|*g!=0)
                .map(|(&hz,gain)|Filter {kind:Kind::Peak,hz,gain:gain.clamp(-12,12) as f64,q:std::f64::consts::SQRT_2}));
        }
        // A filter at/above Nyquist cannot be represented at this stream rate.
        specs.retain(|f| f.hz < rate as f64 * 0.49 && f.gain != 0.0);
        let filters: Vec<_> = specs.iter().map(|f|Biquad::new(f,rate,channels)).collect();
        if filters.is_empty() { return Self {filters,gain:10f64.powf(preamp/20.0)}; }
        let peak = peak_power(&filters);
        // Preserve the export's preamp when more conservative. One further dB
        // is margin for transients, which the response does not bound; final
        // PCM conversion saturates them.
        preamp = preamp.min(-10.0*peak.log10()-1.0);
        Self { filters, gain: 10f64.powf(preamp/20.0) }
    }
    fn sample(&mut self, input: f64, ch: usize) -> f64 {
        self.filters.iter_mut().fold(input*self.gain, |v,f|f.sample(v,ch))
    }
    fn stereo(&mut self, input: [f64; 2]) -> [f64; 2] {
        self.filters.iter_mut().fold(input.map(|v|v*self.gain), |v,f|f.stereo(v))
    }
    fn stereo_block(&mut self, pcm: &mut [f32]) {
        #[cfg(any(q2_eq_msa, test))]
        if !self.filters.is_empty() && !pcm.is_empty() && super::eq_msa::available() {
            use super::eq_msa::{Filter, MAX_FILTERS};
            assert!(self.filters.len() <= MAX_FILTERS);
            let mut packed = [Filter::default(); MAX_FILTERS];
            let packed = &mut packed[..self.filters.len()];
            for (out, filter) in packed.iter_mut().zip(&self.filters) {
                let [left, right] = filter.state.as_slice() else { unreachable!("stereo history") };
                *out = Filter { b: filter.b, a: filter.a, state: [*left, *right] };
            }
            super::eq_msa::process(packed, pcm, self.gain);
            for (filter, out) in self.filters.iter_mut().zip(packed) {
                filter.state.copy_from_slice(&out.state);
            }
            return;
        }
        for frame in pcm.chunks_exact_mut(2) {
            let out = self.stereo([frame[0] as f64, frame[1] as f64]);
            frame[0] = out[0] as f32; frame[1] = out[1] as f32;
        }
    }
    fn bypass(&self) -> bool { self.filters.is_empty() && self.gain == 1.0 }
}

/// An offline view of the same digital filters used by playback. Layer curves
/// show saved settings even when bypassed; combined uses only enabled layers.
/// Shape curves exclude preamp; add preamp_db to combined_db for the EQ output.
pub struct Response {
    pub hz: Vec<f64>,
    pub headphone_db: Vec<f64>,
    pub graphic_db: Vec<f64>,
    pub combined_db: Vec<f64>,
    pub preamp_db: f64,
}
impl Response {
    pub fn new(settings: &Settings, rate: u32, points: usize) -> Self {
        let settings=settings.clone().sanitized();
        let rate=rate.max(1000);
        let points=points.clamp(2,512);
        let upper=(rate as f64*0.49).min(20000.0);
        let hz: Vec<_>=(0..points).map(|i|20.0*(upper/20.0).powf(i as f64/(points-1) as f64)).collect();
        let headphone=Chain::new(&Settings {headphone_enabled:true,headphone:settings.headphone.clone(),..Default::default()},rate,0);
        let graphic=Chain::new(&Settings {graphic_enabled:true,bands:settings.bands,..Default::default()},rate,0);
        let combined=Chain::new(&settings,rate,0);
        let curve=|chain:&Chain| hz.iter().map(|&f| {
            chain.filters.iter().map(|filter|10.0*Power::new(filter).at_hz(f,rate).log10()).sum()
        }).collect();
        Self {headphone_db:curve(&headphone),graphic_db:curve(&graphic),combined_db:curve(&combined),
            preamp_db:20.0*combined.gain.log10(),hz}
    }
}

pub struct Equalizer {
    requested: Settings, applied: Option<Settings>, format: (u32,usize),
    current: Chain, previous: Option<Chain>, fade: usize, fade_len: usize,
}
impl Equalizer {
    pub fn new(settings: Settings) -> Self {
        Self { requested:settings.sanitized(), applied:None,format:(0,0),current:Chain::default(),previous:None,fade:0,fade_len:1 }
    }
    pub fn set(&mut self, settings: Settings) { self.requested = settings.sanitized(); }
    pub fn reset(&mut self) { self.applied=None; self.previous=None; }
    pub fn prepare(&mut self, rate: u32, channels: usize) {
        if self.format != (rate,channels) { self.format=(rate,channels); self.reset(); }
        if self.previous.is_none() && self.applied.as_ref() != Some(&self.requested) {
            let next = Chain::new(&self.requested,rate,channels);
            if self.applied.is_some() { self.previous=Some(std::mem::replace(&mut self.current,next)); }
            else { self.current=next; }
            self.applied=Some(self.requested.clone()); self.fade=0; self.fade_len=(rate as usize/100).max(1);
        }
    }
    pub fn bypass(&self) -> bool { self.previous.is_none() && self.current.bypass() }
    pub fn process(&mut self, samples: &mut [f32]) {
        let channels = self.format.1;
        let fading = if self.previous.is_some() { self.fade_len.saturating_sub(self.fade).min(samples.len()/channels) } else { 0 };
        let (transition, steady) = samples.split_at_mut(fading * channels);
        for frame in transition.chunks_exact_mut(channels) {
            let mix = self.fade as f64/self.fade_len as f64;
            for (ch,x) in frame.iter_mut().enumerate() {
                let input = *x as f64;
                let y = self.current.sample(input,ch);
                *x = if let Some(old)=&mut self.previous { (old.sample(input,ch)*(1.0-mix)+y*mix) as f32 } else { y as f32 };
            }
            if self.previous.is_some() { self.fade+=1; if self.fade>=self.fade_len { self.previous=None; } }
        }
        // Settings crossfade for only 10 ms. The rest of playback needs no
        // fade division or previous-chain branch for each frame/sample.
        if channels == 2 {
            self.current.stereo_block(steady);
        } else {
            for frame in steady.chunks_exact_mut(channels) {
                for (ch, x) in frame.iter_mut().enumerate() {
                    *x = self.current.sample(*x as f64, ch) as f32;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Profile {
        Profile::parse("Sennheiser HD 650".into(),"fixture".into(),include_str!("../../assets/autoeq/hd650-fixture.txt")).unwrap()
    }
    fn settings() -> impl proptest::strategy::Strategy<Value = Settings> {
        use proptest::prelude::*;
        let filter = (prop_oneof![Just(Kind::Peak), Just(Kind::LowShelf), Just(Kind::HighShelf)], 10.0f64..=24000.0, -30.0f64..=30.0, 0.1f64..=20.0)
            .prop_map(|(kind, hz, gain, q)| Filter { kind, hz, gain, q });
        let profile = (-60.0f64..=0.0, proptest::collection::vec(filter, 1..=20))
            .prop_map(|(preamp, filters)| Profile { name: "generated".into(), source: "generated".into(), preamp, filters });
        (any::<bool>(), proptest::array::uniform10(-12i8..=12), any::<bool>(), proptest::option::of(profile))
            .prop_map(|(graphic_enabled, bands, headphone_enabled, headphone)| Settings { graphic_enabled, bands, headphone_enabled, headphone })
    }
    /// The decoders' standard sample rates: every AAC rate and the higher
    /// FLAC, WAV and ALAC rates.
    const RATES: [u32; 15] = [7350, 8000, 11025, 12000, 16000, 22050, 24000, 32000, 44100, 48000, 64000, 88200, 96000, 176400, 192000];
    /// The chain's largest power gain on 2 × (points + 1) frequencies from
    /// 0 Hz to Nyquist, uniform in Hz and logarithmic from 1 Hz, and its
    /// frequency. Each filter is evaluated as a complex ratio, independent
    /// of `Power`. 2^14 points space the uniform grid 1/16 of the
    /// narrowest valid resonance apart (Q 20 at 0.49 × the rate) and the
    /// logarithmic grid 1/70 of it at low frequencies.
    fn reference_peak(chain: &Chain, rate: u32, points: usize) -> (f64, f64) {
        let nyquist = rate as f64 / 2.0;
        let power = |hz: f64| -> f64 {
            let w = std::f64::consts::TAU * hz / rate as f64;
            let (z1, z2) = ((w.cos(), -w.sin()), ((2.0 * w).cos(), -(2.0 * w).sin()));
            chain.filters.iter().map(|f| {
                let num = (f.b[0] + f.b[1] * z1.0 + f.b[2] * z2.0, f.b[1] * z1.1 + f.b[2] * z2.1);
                let den = (1.0 + f.a[0] * z1.0 + f.a[1] * z2.0, f.a[0] * z1.1 + f.a[1] * z2.1);
                (num.0 * num.0 + num.1 * num.1) / (den.0 * den.0 + den.1 * den.1)
            }).product()
        };
        (0..=points).map(|i| nyquist * i as f64 / points as f64)
            .chain((0..=points).map(|i| nyquist.powf(i as f64 / points as f64)))
            .map(|hz| (power(hz), hz)).fold((0.0, 0.0), |best, p| if p.0 > best.0 { p } else { best })
    }
    /// Valid settings weighted toward the sharpest responses: besides values
    /// uniform over each range, a Q from 15 to 20, a gain within ±2 dB and
    /// a center from 0.4 to 0.49 × one of `RATES`.
    fn sharp_settings() -> impl proptest::strategy::Strategy<Value = Settings> {
        use proptest::prelude::*;
        let hz = prop_oneof![10.0f64..=24000.0, (1.0f64..=24000f64.log10()).prop_map(|e| 10f64.powf(e)),
            (0..RATES.len(), 0.4f64..0.49).prop_map(|(r, f)| (RATES[r] as f64 * f).min(24000.0))];
        let filter = (prop_oneof![Just(Kind::Peak), Just(Kind::LowShelf), Just(Kind::HighShelf)], hz,
            prop_oneof![-30.0f64..=30.0, -2.0f64..=2.0], prop_oneof![0.1f64..=20.0, 15.0f64..=20.0])
            .prop_map(|(kind, hz, gain, q)| Filter { kind, hz, gain, q });
        let profile = (-60.0f64..=0.0, proptest::collection::vec(filter, 1..=20))
            .prop_map(|(preamp, filters)| Profile { name: "generated".into(), source: "generated".into(), preamp, filters });
        (any::<bool>(), proptest::array::uniform10(-12i8..=12), any::<bool>(), proptest::option::of(profile))
            .prop_map(|(graphic_enabled, bands, headphone_enabled, headphone)| Settings { graphic_enabled, bands, headphone_enabled, headphone })
    }
    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig { cases: 64, failure_persistence: None, ..proptest::prelude::ProptestConfig::default() })]
        /// At every decoder rate the preamp holds the combined response a dB
        /// below full scale at every frequency of the dense reference, and
        /// lowers the gain at most 0.05 dB more than the reference peak needs.
        #[test]
        fn preamp_bounds_the_dense_reference_peak_at_every_rate(settings in sharp_settings()) {
            let profile = settings.headphone.as_ref().filter(|_| settings.headphone_enabled).map_or(0.0, |p| p.preamp);
            for rate in RATES {
                let chain = Chain::new(&settings, rate, 0);
                if chain.filters.is_empty() { continue; }
                let preamp = 20.0 * chain.gain.log10();
                let (peak, hz) = reference_peak(&chain, rate, 1 << 14);
                let peak = 10.0 * peak.max(1.0).log10();
                proptest::prop_assert!(peak + preamp <= -1.0 + 1e-9, "{:+.4} dB at {:.2} Hz, {} Hz stream", peak + preamp, hz, rate);
                proptest::prop_assert!(preamp >= profile.min(-peak - 1.0) - 0.05, "preamp {:.4} dB for a {:.4} dB peak, {} Hz stream", preamp, peak, rate);
            }
        }
    }
    /// A sharp low shelf near Nyquist: its +8.34 dB peak at 3,567 Hz is
    /// narrower than the spacing of 1,025 logarithmic frequencies there.
    #[test]
    fn a_sharp_shelf_near_nyquist_keeps_headroom() {
        let filters = vec![Filter { kind: Kind::LowShelf, hz: 3549.98, gain: -1.068, q: 19.64 }];
        let settings = Settings { headphone_enabled: true, headphone: Some(Profile { name: "generated".into(), source: "generated".into(), preamp: 0.0, filters }), ..Default::default() };
        let mut chain = Chain::new(&settings, 8000, 1);
        let (peak, hz) = reference_peak(&chain, 8000, 1 << 14);
        let (peak, preamp) = (10.0 * peak.log10(), 20.0 * chain.gain.log10());
        assert!((peak - 8.338).abs() < 0.001 && (hz - 3567.25).abs() < 0.5, "{peak} dB at {hz} Hz");
        assert!(peak + preamp <= -1.0 + 1e-9 && peak + preamp >= -1.01, "{preamp} dB preamp");
        let response = Response::new(&settings, 8000, 512);
        assert!(response.combined_db.iter().all(|db| db + response.preamp_db <= -1.0 + 1e-9));
        assert!(20.0 * tone(&mut chain, 8000, hz).log10() < -0.99);
    }
    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig { cases: 64, failure_persistence: None, ..proptest::prelude::ProptestConfig::default() })]
        /// Every setting the UI or an AutoEQ import can produce, at every
        /// stream rate: the preamp leaves the combined response at or below
        /// full scale across the band, a full-scale square wave and noise
        /// stay finite, and the filters fall silent after the input does.
        #[test]
        fn any_valid_setting_keeps_headroom_stays_finite_and_decays(
            settings in settings(),
            rate in proptest::prelude::prop_oneof![proptest::prelude::Just(8000u32), proptest::prelude::Just(22050u32), proptest::prelude::Just(44100u32),
                proptest::prelude::Just(48000u32), proptest::prelude::Just(96000u32), proptest::prelude::Just(192000u32)],
            seed in proptest::prelude::any::<u32>(),
        ) {
            use proptest::prelude::*;
            let response = Response::new(&settings, rate, 512);
            for (hz, db) in response.hz.iter().zip(&response.combined_db) {
                // The plotted frequencies; the dense reference property covers the rest.
                prop_assert!(db + response.preamp_db <= 0.0, "{:.2} dB at {:.0} Hz, {} Hz stream", db + response.preamp_db, hz, rate);
            }
            let mut eq = Equalizer::new(settings.clone());
            eq.prepare(rate, 2);
            let mut state = seed | 1;
            let mut block: Vec<f32> = (0..2048).map(|i| {
                state = state.wrapping_mul(1664525).wrapping_add(1013904223);
                if i < 1024 { if (i / 32) % 2 == 0 { 1.0 } else { -1.0 } } else { (state >> 8) as f32 / 8_388_608.0 - 1.0 }
            }).collect();
            if !eq.bypass() { eq.process(&mut block); }
            // Thirty-one filters of up to 30 dB each bound this far below f32's range.
            prop_assert!(block.iter().all(|sample| sample.is_finite() && sample.abs() < 1e12), "peak {:?}", block.iter().fold(0f32, |peak, sample| peak.max(sample.abs())));
            if !eq.bypass() {
                let mut last = 1.0f32;
                // 60 s at most; a Q of 20 at 10 Hz rings for seconds.
                for _ in 0..(60 * rate as usize / 4096 + 1) {
                    let mut silence = vec![0.0f32; 8192];
                    eq.process(&mut silence);
                    last = silence.iter().fold(0f32, |peak, sample| peak.max(sample.abs()));
                    // Filter history is zero or a normal number: subnormal
                    // arithmetic traps to the kernel's emulation on the Q2.
                    for filter in &eq.current.filters {
                        prop_assert!(filter.state.iter().flatten().all(|value| *value == 0.0 || value.abs() >= 1e-24), "{:?}", filter.state);
                    }
                    if last < 1e-12 { break; }
                }
                // Not exact zero: flushing one of a filter's two history
                // values at 1e-24 can sustain a cycle near 1e-22 (below).
                // A sample under 2^-32 is PCM zero.
                prop_assert!(last < 1e-12, "{} left after 60 s of silence", last);
            }
        }
    }

    /// The 1e-24 flush keeps subnormals out of the filters but does not end
    /// every decay at zero: +8 dB at 31 Hz and 22.05 kHz keeps one channel
    /// cycling near 1e-22. That is -440 dB and converts to PCM zero; this
    /// test records the behaviour and bounds it.
    #[test]
    fn a_flush_sustained_cycle_stays_far_below_one_pcm_step() {
        let mut settings = Settings::default();
        settings.graphic_enabled = true;
        settings.bands[0] = 8;
        let mut eq = Equalizer::new(settings);
        eq.prepare(22050, 2);
        let mut state = 1u32;
        let mut block: Vec<f32> = (0..2048).map(|i| {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            if i < 1024 { if (i / 32) % 2 == 0 { 1.0 } else { -1.0 } } else { (state >> 8) as f32 / 8_388_608.0 - 1.0 }
        }).collect();
        eq.process(&mut block);
        let mut peak = 0f32;
        for _ in 0..40 {
            let mut silence = vec![0.0f32; 8192];
            eq.process(&mut silence);
            peak = silence.iter().fold(0f32, |peak, sample| peak.max(sample.abs()));
        }
        assert!(peak < 1e-20, "{peak:e}");
        let mut pcm = vec![7i32; 8192];
        let mut silence = vec![0.0f32; 8192];
        eq.process(&mut silence);
        super::super::pcm_convert::floats_pcm(false, &silence, &mut pcm, 2147483648.0);
        assert!(pcm.iter().all(|sample| *sample == 0));
    }

    #[test]
    fn block_processing_matches_sample_reference_through_crossfades() {
        // Retain the original per-frame equation as an independent oracle.
        fn reference(eq: &mut Equalizer, data: &mut [f32]) {
            for frame in data.chunks_exact_mut(eq.format.1) {
                let mix = eq.fade as f64 / eq.fade_len as f64;
                for (ch,x) in frame.iter_mut().enumerate() {
                    let input = *x as f64;
                    let y = eq.current.sample(input,ch);
                    *x = if let Some(old) = &mut eq.previous {
                        (old.sample(input,ch)*(1.0-mix)+y*mix) as f32
                    } else { y as f32 };
                }
                if eq.previous.is_some() {
                    eq.fade += 1;
                    if eq.fade >= eq.fade_len { eq.previous = None; }
                }
            }
        }
        for rate in [8000,44100,192000] {
            for channels in [1,2,6] {
                let mut actual = Equalizer::new(Settings::default());
                let mut expected = Equalizer::new(Settings::default());
                actual.prepare(rate,channels); expected.prepare(rate,channels);
                for enabled in [true,false,true] {
                    let settings = Settings { graphic_enabled: enabled, bands: [3,-2,4,-1,2,-3,1,3,-2,2], ..Default::default() };
                    actual.set(settings.clone()); expected.set(settings);
                    for frames in [0,1,7,256,2048,19] {
                        actual.prepare(rate,channels); expected.prepare(rate,channels);
                        let mut data: Vec<_> = (0..frames*channels).map(|n|(n as f32 * 0.13).sin()*0.3).collect();
                        let mut oracle = data.clone();
                        actual.process(&mut data); reference(&mut expected,&mut oracle);
                        assert_eq!(data,oracle,"{rate} Hz, {channels} channels, {frames} frames");
                        assert_eq!(actual.fade,expected.fade);
                        assert_eq!(actual.bypass(),expected.bypass());
                    }
                }
            }
        }
    }

    #[test]
    fn stereo_equations_preserve_scalar_bits_and_channel_histories() {
        let settings = [
            Settings::default(),
            Settings { graphic_enabled:true, bands:[12,-12,12,-12,12,-12,12,-12,12,-12], ..Default::default() },
            Settings { headphone_enabled:true, headphone:Some(fixture()), graphic_enabled:true, bands:[3;10] },
            Settings { headphone_enabled:true, headphone:Some(Profile {
                name:"generated boundaries".into(), source:"generated".into(), preamp:-30.0,
                filters:vec![
                    Filter {kind:Kind::Peak,hz:10.0,gain:30.0,q:20.0},
                    Filter {kind:Kind::LowShelf,hz:100.0,gain:-30.0,q:0.1},
                    Filter {kind:Kind::HighShelf,hz:20000.0,gain:30.0,q:20.0},
                ],
            }), ..Default::default() },
        ];
        for rate in [8000,44100,48000,96000,192000] {
            for settings in &settings {
                let mut pair=Chain::new(settings,rate,2);
                let mut scalar=pair.clone();
                let mut seed=0x51d923e7u32;
                for n in 0..8192 {
                    let input=std::array::from_fn(|ch| {
                        seed=seed.wrapping_mul(1664525).wrapping_add(1013904223);
                        match n {
                            0 => if ch==0 { 1.0 } else { 0.0 },
                            1..=1023 | 4096..=8191 => 0.0,
                            1024..=2047 => if ch==0 { 1e-30 } else { -1e-30 },
                            _ => (seed as i32 as f64)/(i32::MAX as f64),
                        }
                    });
                    let expected=[scalar.sample(input[0],0),scalar.sample(input[1],1)];
                    assert_eq!(pair.stereo(input).map(f64::to_bits),expected.map(f64::to_bits),"{rate} frame {n}");
                }
                for (actual,expected) in pair.filters.iter().zip(&scalar.filters) {
                    assert_eq!(actual.state.iter().map(|v|v.map(f64::to_bits)).collect::<Vec<_>>(),
                        expected.state.iter().map(|v|v.map(f64::to_bits)).collect::<Vec<_>>());
                }
            }
        }
    }

    #[test]
    fn stereo_block_matches_scalar_bits_with_maximum_filters_and_partial_frames() {
        let settings = Settings {
            graphic_enabled: true, bands: [12,-12,12,-12,12,-12,12,-12,12,-12],
            headphone_enabled: true, headphone: Some(Profile {
                name: "generated thirty filters".into(), source: "generated".into(), preamp: -30.0,
                filters: (0..20).map(|n| Filter {
                    kind: [Kind::Peak,Kind::LowShelf,Kind::HighShelf][n%3],
                    hz: 20.0 * 1.35f64.powi(n as i32), gain: if n%2==0 { 6.0 } else { -6.0 }, q: 1.0,
                }).collect(),
            }),
        };
        for rate in [8000,44100,48000,96000,192000] {
            let mut actual = Chain::new(&settings, rate, 2);
            let mut reference = actual.clone();
            if rate >= 44100 { assert_eq!(actual.filters.len(),30); }
            let mut seed = 0x51d923e7u32;
            for pattern in 0..4 {
                for frames in [0,1,7,256,2048,19] {
                    let mut pcm: Vec<f32> = (0..frames*2).map(|n| {
                        seed=seed.wrapping_mul(1664525).wrapping_add(1013904223);
                        match pattern {
                            0 => if n==0 { 1.0 } else if n==3 { -0.7 } else { 0.0 },
                            1 => if n%2==0 { 1e-30 } else { -1e-30 },
                            2 => seed as i32 as f32 / i32::MAX as f32,
                            _ => 0.0,
                        }
                    }).collect();
                    pcm.push(-0.123); // Incomplete frame must stay untouched.
                    let mut expected=pcm.clone();
                    for frame in expected.chunks_exact_mut(2) {
                        frame[0]=reference.sample(frame[0] as f64,0) as f32;
                        frame[1]=reference.sample(frame[1] as f64,1) as f32;
                    }
                    actual.stereo_block(&mut pcm);
                    assert_eq!(pcm.iter().map(|v|v.to_bits()).collect::<Vec<_>>(),
                        expected.iter().map(|v|v.to_bits()).collect::<Vec<_>>(),"{rate}/{pattern}/{frames}");
                    for (a,b) in actual.filters.iter().zip(&reference.filters) {
                        assert_eq!(a.state.iter().map(|s|s.map(f64::to_bits)).collect::<Vec<_>>(),
                            b.state.iter().map(|s|s.map(f64::to_bits)).collect::<Vec<_>>());
                    }
                }
            }
        }
    }

    #[test]
    fn plotted_curves_match_playback_and_respect_bypassed_layers() {
        let mut settings=Settings {headphone_enabled:true,headphone:Some(fixture()),graphic_enabled:true,bands:[2;10]};
        for rate in [8000,44100,96000] {
            let response=Response::new(&settings,rate,101);
            assert_eq!(response.hz[0],20.0);
            assert!((response.hz[100]-(rate as f64*0.49).min(20000.0)).abs()<1e-6);
            assert!(response.hz.windows(2).all(|p|p[1]>p[0]));
            for i in [10,50,90] {
                assert!((response.combined_db[i]-response.headphone_db[i]-response.graphic_db[i]).abs()<1e-8);
                let actual=20.0*tone(&mut Chain::new(&settings,rate,1),rate,response.hz[i]).log10();
                assert!((actual-response.combined_db[i]-response.preamp_db).abs()<0.1);
            }
        }
        settings.headphone_enabled=false;settings.graphic_enabled=false;
        let response=Response::new(&settings,48000,101);
        assert!(response.combined_db.iter().all(|v|*v==0.0));
        assert_eq!(response.preamp_db,0.0);
        assert!(response.headphone_db.iter().any(|v|v.abs()>1.0),"saved profile remains inspectable when bypassed");
        assert!(response.graphic_db.iter().any(|v|v.abs()>1.0));
        let flat=Response::new(&Settings::default(),48000,0);
        assert_eq!(flat.hz.len(),2);
        assert!(flat.combined_db.iter().all(|v|*v==0.0));
    }
    #[test]
    fn presets_only_change_the_graphic_layer_and_manual_edits_are_custom() {
        let mut settings=Settings {headphone_enabled:true,headphone:Some(fixture()),..Default::default()};
        let profile=settings.headphone.clone();
        for (i,preset) in PRESETS.iter().enumerate() {
            assert!(settings.set_preset(i));
            assert_eq!(settings.preset_name(),preset.name);
            assert!(settings.graphic_enabled && settings.headphone_enabled);
            assert_eq!(settings.headphone,profile);
            assert!(settings.bands.iter().all(|g|(-12..=12).contains(g)));
        }
        settings.bands[0]=11;
        assert_eq!(settings.preset_name(),"Custom");
        let before=settings.clone();assert!(!settings.set_preset(PRESETS.len()));assert_eq!(settings,before);
    }
    fn tone(chain: &mut Chain, rate:u32, hz:f64) -> f64 {
        let mut power=0.0;
        for n in 0..rate {
            let x=(std::f64::consts::TAU*hz*n as f64/rate as f64).sin()*0.1;
            let y=chain.sample(x,0);
            assert!(y.is_finite());
            if n>=rate/2 { power+=y*y; }
        }
        (power/(rate/2) as f64*2.0).sqrt()/0.1
    }
    #[test]
    fn custom_studio_shelves_match_archived_upstream_response() {
        let references=[
            include_str!("../../assets/autoeq/custom-studio/step-1-response.tsv"),
            include_str!("../../assets/autoeq/custom-studio/step-2-response.tsv"),
            include_str!("../../assets/autoeq/custom-studio/step-3-response.tsv"),
            include_str!("../../assets/autoeq/custom-studio/step-4-response.tsv"),
        ];
        for (entry,reference) in crate::audio::autoeq::search("custom studio").iter().zip(references) {
            let profile=entry.load(std::path::Path::new("/dev/null/generated"),true,None).unwrap();
            let settings=Settings {headphone_enabled:true,headphone:Some(profile.clone()),..Default::default()};
            let chain=Chain::new(&settings,44100,1);
            for line in reference.lines().skip(1) {
                let (hz,db)=line.split_once('\t').unwrap();
                let hz=hz.parse::<f64>().unwrap();let db=db.parse::<f64>().unwrap();
                let actual:f64=chain.filters.iter().map(|f|10.0*Power::new(f).at_hz(hz,44100).log10()).sum();
                // Upstream CSV retains optimizer precision; TXT rounds filter
                // values to integer Hz/0.1 dB/0.01 Q. Compare shape, no preamp.
                assert!((actual-db).abs()<0.25,"{} {hz} Hz: {actual} vs {db}",entry.name());
            }
            for rate in [8000,44100,48000,96000,192000] {
                let mut eq=Equalizer::new(settings.clone());eq.prepare(rate,2);
                let mut pcm:Vec<_>=(0..4096).map(|n|(n as f32*0.017).sin()*0.1).collect();
                eq.process(&mut pcm);
                assert!(pcm.iter().all(|x|x.is_finite()));
                assert!(Response::new(&settings,rate,512).preamp_db<=profile.preamp);
            }
        }
    }
    #[test]
    fn rbj_peak_and_shelves_match_measured_tones() {
        for rate in [8000,44100,48000,96000,192000] {
            for kind in [Kind::Peak,Kind::LowShelf,Kind::HighShelf] {
                let f=Filter{kind,hz:1000.0,gain:6.0,q:0.707};
                let mut chain=Chain {filters:vec![Biquad::new(&f,rate,2)],gain:1.0};
                let gain=tone(&mut chain,rate,1000.0);
                let expected=if kind==Kind::Peak {6.0} else {3.0};
                assert!((20.0*gain.log10()-expected).abs()<0.01,"{kind:?} at {rate}");
                assert_eq!(chain.sample(0.0,1),0.0,"no stereo crosstalk");
            }
        }
    }
    #[test]
    fn parser_rejects_partial_nonfinite_and_unbounded_profiles() {
        let text=include_str!("../../assets/autoeq/hd650-fixture.txt");
        assert_eq!(fixture().filters.len(),10);
        for bad in [text.replace("LSC","LS"),text.replace("-6.1","NaN"),text.replace("0.70","0.0"),
            text.replace("105 Hz","99999 Hz"),text.replace("ON PK","MAYBE PK"),text.replace("6.4 dB","inf dB"),
            text.replace("Preamp: -6.1 dB","")] {
            assert!(Profile::parse("test".into(),"test".into(),&bad).is_err());
        }
        assert!(Profile::parse("test".into(),"test".into(),&format!("{text}garbage")).is_err());
    }
    #[test]
    fn combined_headroom_and_low_rate_nyquist_are_bounded() {
        let mut s=Settings{graphic_enabled:true,headphone_enabled:true,headphone:Some(fixture()),bands:[12;10]};
        for rate in [8000,22050,44100,48000,96000,192000] {
            let mut chain=Chain::new(&s,rate,2);
            for hz in [31.25,63.,125.,500.,1000.,2000.,4000.,8000.,16000.] {
                if hz>=rate as f64/2.0 {continue;}
                let amplitude=tone(&mut chain,rate,hz);
                assert!(amplitude<1.0,"gain {amplitude} at {hz}/{rate}");
            }
        }
        s.bands=[0;10];
        let corrected=Chain::new(&s,48000,2);
        s.headphone_enabled=false;
        assert!(Chain::new(&s,48000,2).bypass());
        assert!(!corrected.bypass());
    }
    #[test]
    fn layers_compose_and_changes_crossfade_then_bypass() {
        let mut s=Settings::default();s.graphic_enabled=true;s.bands[5]=6;
        let graphic=Chain::new(&s,48000,2);
        s.headphone_enabled=true;s.headphone=Some(fixture());
        let combined=Chain::new(&s,48000,2);
        assert_eq!(combined.filters.len(),graphic.filters.len()+10);
        s.graphic_enabled=false;
        assert_eq!(Chain::new(&s,48000,2).filters.len(),10);
        let mut eq=Equalizer::new(Settings::default());eq.prepare(48000,2);
        assert!(eq.bypass());
        eq.set(s);eq.prepare(48000,2);
        let mut data=vec![0.25;4096];eq.process(&mut data);
        assert_eq!(data[0],0.25,"transition starts at previous output");
        assert!(data.iter().all(|x|x.is_finite() && x.abs()<1.0));
        eq.set(Settings::default());eq.prepare(48000,2);eq.process(&mut data);
        assert!(eq.bypass());
        eq.reset();eq.prepare(8000,1);
        assert!(eq.bypass());
    }
    #[test]
    fn persisted_layers_roundtrip_without_changing_other_playback_defaults() {
        let mut config=crate::config::Config::default();
        config.playback.eq.headphone=Some(fixture());
        config.playback.eq.headphone_enabled=true;
        config.playback.eq.graphic_enabled=true;
        config.playback.eq.bands[0]=-4;
        let text=toml::to_string(&config).unwrap();
        let restored:crate::config::Config=toml::from_str(&text).unwrap();
        assert_eq!(restored,config);
        assert_eq!(crate::config::Config::parse("[playback]\nvolume=43","fixture").playback.eq,Settings::default());
    }

    #[test]
    fn packet_boundaries_preserve_history_and_seek_reset_clears_it() {
        let s=Settings {headphone_enabled:true,headphone:Some(fixture()),..Default::default()};
        let mut whole=Equalizer::new(s.clone());whole.prepare(48000,2);
        let mut split=Equalizer::new(s);split.prepare(48000,2);
        let mut a:Vec<f32>=(0..8192).map(|i|(i as f32*0.13).sin()*0.1).collect();
        let mut b=a.clone();whole.process(&mut a);
        for packet in b.chunks_mut(512) {split.prepare(48000,2);split.process(packet);}
        assert_eq!(a,b,"continuous gapless PCM must not depend on packet boundaries");
        split.reset();split.prepare(48000,2);
        let mut silence=vec![0.0;2048];split.process(&mut silence);
        assert!(silence.iter().all(|v|*v==0.0));
        let mut invalid=fixture();invalid.preamp=f64::NAN;
        split.set(Settings {headphone_enabled:true,headphone:Some(invalid),..Default::default()});
        split.reset();split.prepare(48000,2);
        assert!(split.bypass());
    }
}
