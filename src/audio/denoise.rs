//! Noise reduction: a zero-latency gate for live monitoring and an offline spectral-subtraction
//! denoiser for recorded takes.

use super::dsp::{Fft, hann};
use rustfft::num_complex::Complex32;

const N: usize = 2048;
const HOP: usize = N / 4;

/// Average magnitude spectrum of a noise-only segment.
#[derive(Clone, Debug)]
pub struct NoiseProfile {
    pub mags: Vec<f32>,
}

impl NoiseProfile {
    /// Returns `None` if the segment is too short to be useful.
    pub fn learn(noise: &[f32]) -> Option<Self> {
        if noise.len() < N {
            return None;
        }
        let mut fft = Fft::new(N);
        let w = hann(N);
        let mut acc = vec![0f32; N / 2 + 1];
        let mut frames = 0;
        let mut buf = vec![Complex32::default(); N];
        let mut start = 0;
        while start + N <= noise.len() {
            for i in 0..N {
                buf[i] = Complex32::new(noise[start + i] * w[i], 0.0);
            }
            fft.forward(&mut buf);
            for (k, a) in acc.iter_mut().enumerate() {
                *a += buf[k].norm();
            }
            frames += 1;
            start += HOP;
        }
        acc.iter_mut().for_each(|a| *a /= frames as f32);
        Some(Self { mags: acc })
    }
}

/// Spectral subtraction. `strength` ~ 1.0 (gentle) .. 3.0 (aggressive).
pub fn reduce(x: &[f32], profile: &NoiseProfile, strength: f32) -> Vec<f32> {
    if x.is_empty() {
        return Vec::new();
    }
    let floor = 0.08f32; // never remove more than ~22 dB → avoids "underwater" artefacts
    let mut fft = Fft::new(N);
    let w = hann(N);
    // pad so that every sample is covered by full overlap
    let pad = N;
    let mut input = vec![0f32; x.len() + 2 * pad];
    input[pad..pad + x.len()].copy_from_slice(x);
    let mut out = vec![0f32; input.len()];
    let mut norm = vec![0f32; input.len()];
    let mut buf = vec![Complex32::default(); N];
    let mut prev_gain = vec![1f32; N / 2 + 1];
    let mut start = 0;
    while start + N <= input.len() {
        for i in 0..N {
            buf[i] = Complex32::new(input[start + i] * w[i], 0.0);
        }
        fft.forward(&mut buf);
        for k in 0..=N / 2 {
            let mag = buf[k].norm();
            let nmag = profile.mags[k] * strength;
            let p = mag * mag;
            let g = if p > 1e-20 { ((p - nmag * nmag) / p).max(0.0).sqrt() } else { 0.0 };
            // temporal smoothing: fast attack, slower release reduces musical noise
            let g = if g > prev_gain[k] { g } else { 0.6 * prev_gain[k] + 0.4 * g };
            prev_gain[k] = g;
            let g = g.max(floor);
            buf[k] *= g;
            if k > 0 && k < N / 2 {
                buf[N - k] = buf[k].conj();
            }
        }
        fft.inverse(&mut buf);
        for i in 0..N {
            out[start + i] += buf[i].re * w[i];
            norm[start + i] += w[i] * w[i];
        }
        start += HOP;
    }
    (0..x.len())
        .map(|i| {
            let n = norm[pad + i];
            if n > 1e-6 { out[pad + i] / n } else { 0.0 }
        })
        .collect()
}

/// Simple noise gate with hysteresis for the live monitor path (no look-ahead, no latency).
pub struct Gate {
    open_lin: f32,
    close_lin: f32,
    env: f32,
    gain: f32,
    attack: f32,
    release: f32,
    env_coef: f32,
    open: bool,
}

impl Gate {
    pub fn new(sr: f32) -> Self {
        Self {
            open_lin: 0.01,
            close_lin: 0.005,
            env: 0.0,
            gain: 0.0,
            attack: 1.0 - (-1.0 / (0.001 * sr)).exp(),
            release: 1.0 - (-1.0 / (0.12 * sr)).exp(),
            env_coef: 1.0 - (-1.0 / (0.01 * sr)).exp(),
            open: false,
        }
    }

    /// Set the open threshold (linear); close threshold sits 6 dB lower.
    pub fn set_threshold(&mut self, open_lin: f32) {
        self.open_lin = open_lin;
        self.close_lin = open_lin * 0.5;
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        self.env += self.env_coef * (x.abs() - self.env);
        if self.open {
            if self.env < self.close_lin {
                self.open = false;
            }
        } else if self.env > self.open_lin {
            self.open = true;
        }
        let target = if self.open { 1.0 } else { 0.0 };
        let c = if target > self.gain { self.attack } else { self.release };
        self.gain += c * (target - self.gain);
        x * self.gain
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::dsp::{noise, rms, sine};

    #[test]
    fn spectral_subtraction_reduces_noise_keeps_tone() {
        let sr = 48000.0;
        let n = noise(48000, 0.02, 7);
        let tone = sine(440.0, sr, 48000, 0.3);
        let noisy: Vec<f32> = tone.iter().zip(noise(48000, 0.02, 99)).map(|(a, b)| a + b).collect();
        let profile = NoiseProfile::learn(&n).unwrap();
        let cleaned = reduce(&noisy, &profile, 2.0);
        assert_eq!(cleaned.len(), noisy.len());
        let mid = 4096..44000;
        let err_before =
            rms(&noisy[mid.clone()].iter().zip(&tone[mid.clone()]).map(|(a, b)| a - b).collect::<Vec<_>>());
        let err_after =
            rms(&cleaned[mid.clone()].iter().zip(&tone[mid.clone()]).map(|(a, b)| a - b).collect::<Vec<_>>());
        assert!(err_after < err_before * 0.6, "noise not reduced: {err_before} -> {err_after}");
        // the tone itself survives
        let tone_rms = rms(&tone[mid.clone()]);
        assert!((rms(&cleaned[mid]) - tone_rms).abs() < tone_rms * 0.15);
        // pure noise gets strongly attenuated
        let only_noise = reduce(&noise(48000, 0.02, 5), &profile, 2.0);
        let r = rms(&only_noise[4096..44000]);
        assert!(r < 0.02 * 0.577 * 0.3);
    }

    #[test]
    fn gate_mutes_quiet_passes_loud() {
        let mut g = Gate::new(48000.0);
        g.set_threshold(0.05);
        let quiet: Vec<f32> = noise(4800, 0.01, 3).into_iter().map(|x| g.process(x)).collect();
        assert!(rms(&quiet[2400..]) < 0.001);
        let loud: Vec<f32> = sine(200.0, 48000.0, 9600, 0.5).into_iter().map(|x| g.process(x)).collect();
        assert!(rms(&loud[4800..]) > 0.3);
    }
}
