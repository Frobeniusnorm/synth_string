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
