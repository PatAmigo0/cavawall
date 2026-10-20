//! Native analysis: captured samples to bar heights, cavawall's own rather
//! than cava's.
//!
//! One FFT per channel per frame, and none on a frame no new audio reached.
//! Bars are spaced on the Mel scale and weighted by A-weighting, so the
//! spacing follows pitch and the heights follow loudness as heard rather than
//! raw power, which the bass dominates. Levels are read in dB against a
//! reference that follows the music's loudness, so a quiet track still fills
//! the bars without a gain to tune. Motion comes from a spring per bar,
//! stepped by real elapsed time, so it moves the same at any framerate.

use cavawall::math::fma;
use realfft::{RealFftPlanner, RealToComplex};
use std::sync::Arc;

/// Samples per FFT. At 48 kHz a bin is about 11.7 Hz and the window about
/// 85 ms
pub const FFT_SIZE: usize = 4096;

/// Settings for the native backend, from the `[native]` table
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Params {
    /// Lowest and highest frequency the bars cover, Hz
    pub fmin: f32,
    pub fmax: f32,
    /// Below this level, in dB relative to full scale, the music counts as
    /// silent and the bars fall
    pub gate_db: f32,
    /// How many dB below the loudest recent band an empty bar sits
    pub range_db: f32,
    /// Spring stiffness: higher follows the music faster
    pub stiffness: f32,
    /// Damping ratio: 1 settles without overshoot, below 1 bounces
    pub damping: f32,
    /// How much of a bar's level spreads into its neighbours, 0 to 1
    pub flow: f32,
    /// Weight bands by A-weighting
    pub a_weighting: bool,
}

impl Default for Params {
    fn default() -> Self {
        Self {
            fmin: 30.0,
            fmax: 16_000.0,
            gate_db: -70.0,
            range_db: 45.0,
            stiffness: 170.0,
            damping: 0.9,
            flow: 0.12,
            a_weighting: true,
        }
    }
}

/// How the bars split between the channels
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// Left channel reversed across the left half, right across the right:
    /// cava's stereo, so a configured look carries over
    Stereo,
    Average,
    Left,
    Right,
}

/// Where a bar takes its value from the spectrum: an interval of FFT bins,
/// or, for a band narrower than one bin, a point between two
#[derive(Clone, Copy, Debug)]
enum Band {
    Bins { lo: usize, hi: usize },
    Point { bin: usize, frac: f32 },
}

pub struct Analyzer {
    params: Params,
    layout: Layout,
    /// Bars drawn, and how many come from each channel's spectrum
    bars: usize,
    per_channel: usize,
    fft: Arc<dyn RealToComplex<f32>>,
    window: Vec<f32>,
    /// The last FFT_SIZE frames of each channel, oldest first, rotating
    history: [Vec<f32>; 2],
    head: usize,
    /// Frames added since the last FFT
    fresh: usize,
    /// Frames in a row that were exactly zero; past FFT_SIZE the whole
    /// history is silence
    zeros: usize,
    /// The all-silent history has been analysed once, so the targets are
    /// already zero and another FFT of it would find nothing new
    settled: bool,
    bands: Vec<Band>,
    /// A-weighting at each band's centre, dB
    weight_db: Vec<f32>,
    /// Loudest band level lately, dB, the top of the range
    reference_db: f32,
    targets: Vec<f32>,
    position: Vec<f32>,
    velocity: Vec<f32>,
    scratch_in: Vec<f32>,
    scratch_out: Vec<realfft::num_complex::Complex<f32>>,
    scratch_fft: Vec<realfft::num_complex::Complex<f32>>,
    levels: [Vec<f32>; 2],
}

fn hz_to_mel(f: f32) -> f32 {
    2595.0 * (1.0 + f / 700.0).log10()
}

fn mel_to_hz(m: f32) -> f32 {
    700.0 * (10f32.powf(m / 2595.0) - 1.0)
}

/// IEC 61672 A-weighting, dB
fn a_weight_db(f: f32) -> f32 {
    let f2 = f * f;
    let ra = (12194.0f32.powi(2) * f2 * f2)
        / (fma(20.6, 20.6, f2)
            * (fma(107.7, 107.7, f2) * fma(737.9, 737.9, f2)).sqrt()
            * fma(12194.0, 12194.0, f2));
    fma(20.0, ra.max(f32::MIN_POSITIVE).log10(), 2.0)
}

/// Band edges for `n` bars, Mel-spaced between `fmin` and `fmax`, as bins
fn bands(n: usize, fmin: f32, fmax: f32, rate: f32) -> (Vec<Band>, Vec<f32>) {
    let bin_hz = rate / FFT_SIZE as f32;
    let top = (FFT_SIZE / 2) as f32;
    let (m0, m1) = (hz_to_mel(fmin), hz_to_mel(fmax));
    let edge = |i: usize| mel_to_hz(m0 + (m1 - m0) * i as f32 / n as f32) / bin_hz;
    let mut out = Vec::with_capacity(n);
    let mut centres = Vec::with_capacity(n);
    for i in 0..n {
        let (a, b) = (edge(i).clamp(0.0, top), edge(i + 1).clamp(0.0, top));
        centres.push((a + b) * 0.5 * bin_hz);
        let (lo, hi) = (a.ceil() as usize, b.floor() as usize);
        out.push(if hi > lo {
            Band::Bins { lo, hi }
        } else {
            let mid = (a + b) * 0.5;
            Band::Point { bin: mid.floor() as usize, frac: mid.fract() }
        });
    }
    (out, centres)
}

impl Analyzer {
    pub fn new(bars: usize, layout: Layout, rate: u32, params: Params) -> Self {
        let bars = bars.max(1);
        let rate = rate as f32;
        let fmax = params.fmax.min(rate * 0.5).max(params.fmin + 1.0);
        let per_channel = if layout == Layout::Stereo { bars.div_ceil(2) } else { bars };
        let (bands, centres) = bands(per_channel, params.fmin.max(1.0), fmax, rate);
        let weight_db = centres.iter().map(|&f| if params.a_weighting { a_weight_db(f) } else { 0.0 }).collect();
        let fft = RealFftPlanner::<f32>::new().plan_fft_forward(FFT_SIZE);
        let window = (0..FFT_SIZE)
            .map(|i| 0.5 * (1.0 - (std::f32::consts::TAU * i as f32 / (FFT_SIZE - 1) as f32).cos()))
            .collect();
        Self {
            params,
            layout,
            bars,
            per_channel,
            scratch_in: fft.make_input_vec(),
            scratch_out: fft.make_output_vec(),
            scratch_fft: fft.make_scratch_vec(),
            fft,
            window,
            history: [vec![0.0; FFT_SIZE], vec![0.0; FFT_SIZE]],
            head: 0,
            fresh: 0,
            zeros: 0,
            settled: false,
            bands,
            weight_db,
            reference_db: params.gate_db + params.range_db,
            targets: vec![0.0; bars],
            position: vec![0.0; bars],
            velocity: vec![0.0; bars],
            levels: [vec![0.0; per_channel], vec![0.0; per_channel]],
        }
    }

    /// Take interleaved stereo i16 frames
    pub fn push(&mut self, samples: &[i16]) {
        for f in samples.as_chunks::<2>().0 {
            self.history[0][self.head] = f32::from(f[0]) / 32768.0;
            self.history[1][self.head] = f32::from(f[1]) / 32768.0;
            self.head = (self.head + 1) % FFT_SIZE;
            if f == &[0, 0] {
                self.zeros += 1;
            } else {
                self.zeros = 0;
                self.settled = false;
            }
        }
        self.fresh += samples.len() / 2;
    }

    /// Band levels of one channel, dB, from the current history
    fn analyse(&mut self, ch: usize, mono: bool) {
        for i in 0..FFT_SIZE {
            let j = (self.head + i) % FFT_SIZE;
            let s = if mono { (self.history[0][j] + self.history[1][j]) * 0.5 } else { self.history[ch][j] };
            self.scratch_in[i] = s * self.window[i];
        }
        let _ = self.fft.process_with_scratch(&mut self.scratch_in, &mut self.scratch_out, &mut self.scratch_fft);
        // A full-scale sine through a Hann window peaks at N/4 in one bin
        let norm = 1.0 / (FFT_SIZE as f32 * 0.25).powi(2);
        let power = |k: usize| self.scratch_out.get(k).map_or(0.0, |c| c.norm_sqr() * norm);
        for (b, band) in self.bands.iter().enumerate() {
            let p = match *band {
                Band::Bins { lo, hi } => (lo..hi).map(power).sum::<f32>(),
                Band::Point { bin, frac } => fma(power(bin + 1), frac, power(bin) * (1.0 - frac)),
            };
            self.levels[ch][b] = fma(10.0, p.max(1e-12).log10(), self.weight_db[b]);
        }
    }

    /// Advance one frame, `dt` seconds after the last, and write the bars as
    /// cava's raw u16 little-endian frame
    pub fn frame(&mut self, dt: f32, out: &mut [u8]) {
        let dt = dt.clamp(1e-4, 0.1);
        // Digital silence, once analysed, needs no more FFTs: the parked case
        if self.fresh > 0 && !self.settled {
            self.fresh = 0;
            self.settled = self.zeros >= FFT_SIZE;
            match self.layout {
                Layout::Stereo => {
                    self.analyse(0, false);
                    self.analyse(1, false);
                }
                Layout::Average => self.analyse(0, true),
                Layout::Left => self.analyse(0, false),
                Layout::Right => {
                    self.analyse(1, false);
                    self.levels.swap(0, 1);
                }
            }
            self.retarget(dt);
        }
        self.step(dt);
        for (o, &x) in out.as_chunks_mut::<2>().0.iter_mut().zip(&self.position) {
            *o = ((x.clamp(0.0, 1.0) * 65535.0) as u16).to_le_bytes();
        }
    }

    /// Levels to targets: the range under the reference, the gate, the
    /// layout, then the flow between neighbours
    fn retarget(&mut self, dt: f32) {
        let p = self.params;
        let channels = if self.layout == Layout::Stereo { 2 } else { 1 };
        let loudest = self.levels[..channels].iter().flatten().fold(f32::MIN, |m, &l| m.max(l));
        // Rises at once, falls slowly: a loud passage sets the scale and a
        // quiet one regains it over a few seconds, not a few frames
        self.reference_db = loudest.max(fma(-6.0, dt, self.reference_db)).max(fma(p.range_db, 0.5, p.gate_db));
        let floor = self.reference_db - p.range_db;
        let gated = loudest < p.gate_db;
        let level = |db: f32| if gated { 0.0 } else { ((db - floor) / p.range_db).clamp(0.0, 1.0) };
        let n = self.per_channel;
        match self.layout {
            Layout::Stereo => {
                // Left reversed, then right; an odd count takes one fewer
                // from the right
                let right = self.bars - n;
                for i in 0..n {
                    self.targets[i] = level(self.levels[0][n - 1 - i]);
                }
                for i in 0..right {
                    self.targets[n + i] = level(self.levels[1][i]);
                }
            }
            _ => {
                for i in 0..self.bars {
                    self.targets[i] = level(self.levels[0][i]);
                }
            }
        }
        if p.flow > 0.0 && self.bars > 2 {
            let src = self.targets.clone();
            for i in 0..self.bars {
                let l = src[i.saturating_sub(1)];
                let r = src[(i + 1).min(self.bars - 1)];
                self.targets[i] = fma((l + r) * 0.5, p.flow, src[i] * (1.0 - p.flow));
            }
        }
    }

    /// One spring step toward the targets
    fn step(&mut self, dt: f32) {
        let k = self.params.stiffness.max(1.0);
        let c = 2.0 * self.params.damping.max(0.0) * k.sqrt();
        // Substeps keep a stiff spring stable at a low framerate
        let sub = ((dt * k.sqrt() / 0.5).ceil() as usize).clamp(1, 16);
        let h = dt / sub as f32;
        for i in 0..self.bars {
            let (mut x, mut v) = (self.position[i], self.velocity[i]);
            for _ in 0..sub {
                v = fma(fma(-c, v, k * (self.targets[i] - x)), h, v);
                x = fma(v, h, x);
            }
            self.position[i] = x.max(0.0);
            self.velocity[i] = if x <= 0.0 && v < 0.0 { 0.0 } else { v };
        }
    }

    /// Whether the bars are at rest at zero
    #[cfg(test)]
    pub fn at_rest(&self) -> bool {
        self.position.iter().zip(&self.targets).all(|(&x, &t)| x < 1e-4 && t == 0.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(hz: f32, amp: f32, frames: usize, rate: f32) -> Vec<i16> {
        (0..frames)
            .flat_map(|i| {
                let s = (amp * 32767.0 * (std::f32::consts::TAU * hz * i as f32 / rate).sin()) as i16;
                [s, s]
            })
            .collect()
    }

    fn run(a: &mut Analyzer, samples: &[i16], frames: usize) -> Vec<u16> {
        let mut out = vec![0u8; a.bars * 2];
        let chunk = samples.len() / frames;
        for f in 0..frames {
            a.push(&samples[f * chunk..(f + 1) * chunk]);
            a.frame(1.0 / 60.0, &mut out);
        }
        out.as_chunks::<2>().0.iter().map(|b| u16::from_le_bytes(*b)).collect()
    }

    #[test]
    fn bands_rise_in_frequency_and_stay_in_range() {
        let (b, centres) = bands(64, 30.0, 16_000.0, 48_000.0);
        assert_eq!(b.len(), 64);
        assert!(centres.windows(2).all(|w| w[1] > w[0]), "Mel spacing ascends");
        for band in b {
            match band {
                Band::Bins { lo, hi } => assert!(lo < hi && hi <= FFT_SIZE / 2),
                Band::Point { bin, frac } => assert!(bin < FFT_SIZE / 2 && (0.0..1.0).contains(&frac)),
            }
        }
    }

    #[test]
    fn a_weighting_is_zero_at_1khz_and_cuts_the_bass() {
        assert!(a_weight_db(1000.0).abs() < 0.1);
        assert!(a_weight_db(50.0) < -25.0);
    }

    #[test]
    fn a_tone_lights_its_own_bar() {
        let mut a = Analyzer::new(32, Layout::Average, 48_000, Params::default());
        let s = sine(1000.0, 0.5, 48_000, 48_000.0);
        let bars = run(&mut a, &s, 60);
        let (peak, _) = bars.iter().enumerate().max_by_key(|&(_, &h)| h).unwrap();
        let (bands, centres) = bands(32, 30.0, 16_000.0, 48_000.0);
        let _ = bands;
        let nearest = centres.iter().enumerate().min_by(|x, y| (x.1 - 1000.0).abs().total_cmp(&(y.1 - 1000.0).abs())).unwrap().0;
        assert!(peak.abs_diff(nearest) <= 1, "peak at bar {peak}, 1 kHz is bar {nearest}");
        assert!(bars[peak] > 40_000, "and it is tall: {}", bars[peak]);
    }

    #[test]
    fn silence_falls_and_rests() {
        let mut a = Analyzer::new(16, Layout::Average, 48_000, Params::default());
        run(&mut a, &sine(200.0, 0.5, 24_000, 48_000.0), 30);
        let bars = run(&mut a, &vec![0; 96_000], 120);
        assert!(bars.iter().all(|&h| h == 0), "{bars:?}");
        assert!(a.at_rest());
    }

    #[test]
    fn a_quiet_track_still_fills_the_bars() {
        let mut a = Analyzer::new(16, Layout::Average, 48_000, Params::default());
        let bars = run(&mut a, &sine(440.0, 0.02, 96_000, 48_000.0), 120);
        assert!(bars.iter().any(|&h| h > 40_000), "{bars:?}");
    }

    #[test]
    fn stereo_puts_left_reversed_then_right() {
        let mut a = Analyzer::new(8, Layout::Stereo, 48_000, Params { flow: 0.0, ..Params::default() });
        // A low tone in the left channel only
        let s: Vec<i16> = sine(60.0, 0.5, 48_000, 48_000.0).as_chunks::<2>().0.iter().flat_map(|f| [f[0], 0]).collect();
        let bars = run(&mut a, &s, 60);
        let left: u32 = bars[..4].iter().map(|&h| u32::from(h)).sum();
        let right: u32 = bars[4..].iter().map(|&h| u32::from(h)).sum();
        assert!(left > right * 4, "{bars:?}");
        assert!(bars[3] >= bars[0], "the left's bass sits in the middle: {bars:?}");
    }

    #[test]
    fn an_odd_stereo_count_draws_every_bar() {
        let mut a = Analyzer::new(7, Layout::Stereo, 48_000, Params::default());
        let bars = run(&mut a, &sine(300.0, 0.5, 48_000, 48_000.0), 60);
        assert_eq!(bars.len(), 7);
    }

    #[test]
    fn the_same_music_moves_the_same_at_any_framerate() {
        let s = sine(500.0, 0.5, 48_000, 48_000.0);
        let mut a = Analyzer::new(16, Layout::Average, 48_000, Params::default());
        let mut b = Analyzer::new(16, Layout::Average, 48_000, Params::default());
        let mut out_a = vec![0u8; 32];
        let mut out_b = vec![0u8; 32];
        for f in 0..30 {
            a.push(&s[f * 3200..(f + 1) * 3200]);
            a.frame(1.0 / 30.0, &mut out_a);
            for g in 0..4 {
                let at = f * 3200 + g * 800;
                b.push(&s[at..at + 800]);
                b.frame(1.0 / 120.0, &mut out_b);
            }
        }
        let (x, y) = (u16::from_le_bytes([out_a[14], out_a[15]]), u16::from_le_bytes([out_b[14], out_b[15]]));
        assert!(x.abs_diff(y) < 4000, "30 fps {x} vs 120 fps {y}");
    }
}
