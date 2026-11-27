//! Small DSP helpers shared by the engine and offline processors.

use rustfft::num_complex::Complex32;
use std::sync::Arc;

pub fn db_to_lin(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

pub fn lin_to_db(lin: f32) -> f32 {
    if lin <= 1e-9 { -180.0 } else { 20.0 * lin.log10() }
}

pub fn rms(x: &[f32]) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
}

pub fn hann(n: usize) -> Vec<f32> {
    (0..n).map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos()).collect()
}

/// Reusable forward/inverse FFT pair.
pub struct Fft {
    pub n: usize,
    fwd: Arc<dyn rustfft::Fft<f32>>,
    inv: Arc<dyn rustfft::Fft<f32>>,
    scratch: Vec<Complex32>,
}

impl Fft {
    pub fn new(n: usize) -> Self {
        let mut planner = rustfft::FftPlanner::new();
        let fwd = planner.plan_fft_forward(n);
        let inv = planner.plan_fft_inverse(n);
        let len = fwd.get_inplace_scratch_len().max(inv.get_inplace_scratch_len());
        Self { n, fwd, inv, scratch: vec![Complex32::default(); len] }
    }
    pub fn forward(&mut self, buf: &mut [Complex32]) {
        self.fwd.process_with_scratch(buf, &mut self.scratch);
    }
    /// Inverse transform, normalised by 1/n.
    pub fn inverse(&mut self, buf: &mut [Complex32]) {
        self.inv.process_with_scratch(buf, &mut self.scratch);
        let s = 1.0 / self.n as f32;
        buf.iter_mut().for_each(|c| *c *= s);
    }
}

/// Linear-interpolated sample read with clamping.
pub fn lerp_at(x: &[f32], pos: f64) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    let i = pos.floor();
    let frac = (pos - i) as f32;
    let i = i as isize;
    let get = |k: isize| x[k.clamp(0, x.len() as isize - 1) as usize];
    get(i) * (1.0 - frac) + get(i + 1) * frac
}

/// Dominant frequency (Hz) – used by tests.
#[cfg(test)]
pub fn peak_frequency(x: &[f32], sr: f32) -> f32 {
    let n = x.len().next_power_of_two() / 2;
    let mut fft = Fft::new(n);
    let w = hann(n);
    let mut buf: Vec<Complex32> = (0..n).map(|i| Complex32::new(x[i] * w[i], 0.0)).collect();
    fft.forward(&mut buf);
    let (bin, _) =
        buf[1..n / 2].iter().enumerate().max_by(|a, b| a.1.norm().partial_cmp(&b.1.norm()).unwrap()).unwrap();
    (bin + 1) as f32 * sr / n as f32
}

#[cfg(test)]
pub fn sine(freq: f32, sr: f32, len: usize, amp: f32) -> Vec<f32> {
    (0..len).map(|i| amp * (2.0 * std::f32::consts::PI * freq * i as f32 / sr).sin()).collect()
}

/// Deterministic white noise (xorshift) in [-amp, amp].
#[cfg(test)]
pub fn noise(len: usize, amp: f32, seed: u32) -> Vec<f32> {
    let mut s = seed.max(1);
    (0..len)
        .map(|_| {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            (s as f32 / u32::MAX as f32 * 2.0 - 1.0) * amp
        })
        .collect()
}

/// Filter-curve points per loop repetition (song mode automation).
pub const FILTER_RES: usize = 16;

/// Value of a filter curve (−1 … 1, see [`TrackFilter`]) at song position `pos`.
pub fn filter_at(curve: &[f32], pos: u64, loop_len: usize) -> f32 {
    if curve.is_empty() || loop_len == 0 {
        return 0.0;
    }
    let x = pos as f64 / loop_len as f64 * FILTER_RES as f64;
    let i = x.floor() as usize;
    let f = (x - i as f64) as f32;
    let a = curve.get(i).copied().unwrap_or(0.0);
    let b = curve.get(i + 1).copied().unwrap_or(a);
    a + (b - a) * f
}

/// One track's sweepable filter: below 0 a low-pass (less treble, −1 ≈ 150 Hz), above 0 a
/// high-pass (less bass, +1 ≈ 3 kHz), 0 = open. A TPT state-variable filter, so the cutoff can
/// move every block without clicks.
#[derive(Default, Clone)]
pub struct TrackFilter {
    ic1: f32,
    ic2: f32,
    smoothed: f32,
}

impl TrackFilter {
    /// Process a block with the curve value `target` (smoothed, so drawn steps don't click).
    pub fn process(&mut self, x: &mut [f32], target: f32, sr: f32) {
        // ~10 ms glide towards the target per block
        let k_glide = 1.0 - (-(x.len() as f32) / (0.01 * sr)).exp();
        self.smoothed += (target.clamp(-1.0, 1.0) - self.smoothed) * k_glide;
        let v = self.smoothed;
        if v.abs() < 0.002 {
            // open: pass through, keep the state decaying towards the signal
            for s in x.iter() {
                self.ic1 *= 0.99;
                self.ic2 = *s;
            }
            return;
        }
        let fc = if v < 0.0 { 20_000.0 * (150.0f32 / 20_000.0).powf(-v) } else { 30.0 * (3000.0f32 / 30.0).powf(v) };
        let g = (std::f32::consts::PI * fc.min(0.45 * sr) / sr).tan();
        let k = std::f32::consts::SQRT_2; // Butterworth-ish Q
        let a1 = 1.0 / (1.0 + g * (g + k));
        let a2 = g * a1;
        let a3 = g * a2;
        for s in x.iter_mut() {
            let v3 = *s - self.ic2;
            let v1 = a1 * self.ic1 + a2 * v3;
            let v2 = self.ic2 + a2 * self.ic1 + a3 * v3;
            self.ic1 = 2.0 * v1 - self.ic1;
            self.ic2 = 2.0 * v2 - self.ic2;
            *s = if v < 0.0 { v2 } else { *s - k * v1 - v2 };
        }
    }
}

#[cfg(test)]
mod filter_tests {
    use super::*;

    #[test]
    fn closing_the_filter_removes_treble() {
        let sr = 48000.0;
        let hi = sine(6000.0, sr, 48000, 0.5);
        let lo = sine(100.0, sr, 48000, 0.5);
        let run = |x: &[f32], v: f32| {
            let mut f = TrackFilter::default();
            let mut y = x.to_vec();
            for c in y.chunks_mut(256) {
                f.process(c, v, sr);
            }
            rms(&y[24000..])
        };
        assert!(run(&hi, 0.0) > 0.3, "open filter keeps treble");
        assert!(run(&hi, -0.6) < 0.05, "closed low-pass removes treble");
        assert!(run(&lo, -0.6) > 0.3, "…but keeps the bass");
        assert!(run(&lo, 0.7) < 0.05, "high-pass removes bass");
        // curve lookup interpolates between points
        let curve = [0.0, -1.0];
        assert_eq!(filter_at(&curve, 0, 1600), 0.0);
        assert!((filter_at(&curve, 50, 1600) + 0.5).abs() < 1e-6);
    }
}
