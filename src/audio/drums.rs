//! A tiny synthesized drum kit (no samples needed) and pattern rendering.

use super::looper::SR;
use serde::{Deserialize, Serialize};
use std::f32::consts::PI;
use std::sync::Arc;

pub const DRUM_NAMES: [&str; 3] = ["Kick", "Snare", "Hi-hat"];

/// Snap choices: note value 1/N (0 = off).
pub const SNAP_DIVS: [u8; 5] = [0, 2, 4, 8, 16];
pub const SNAP_LABELS: [&str; 5] = ["Off", "1/2", "1/4", "1/8", "1/16"];

fn default_snap() -> u8 {
    16 // older projects were always snapped to 1/16
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hit {
    /// Position inside the drum track buffer (samples), as played.
    pub pos: u64,
    pub drum: u8,
    /// Snap of this hit: note value 1/N (0 = not snapped). Kept per hit, so a layer can mix
    /// e.g. a kick on quarter notes with 1/16 hi-hats.
    #[serde(default = "default_snap")]
    pub snap: u8,
}

/// Grid in samples for a 1/`div` note, given the length of one beat (a quarter note).
pub fn grid_len(div: u8, beat: f64) -> Option<f64> {
    (div > 0 && beat > 1.0).then(|| beat * 4.0 / div as f64)
}

fn noise(seed: &mut u32) -> f32 {
    *seed ^= *seed << 13;
    *seed ^= *seed >> 17;
    *seed ^= *seed << 5;
    *seed as f32 / u32::MAX as f32 * 2.0 - 1.0
}

fn kick(sr: f32) -> Vec<f32> {
    let n = (0.45 * sr) as usize;
    let mut phase = 0.0f32;
    let mut seed = 1;
    (0..n)
        .map(|i| {
            let t = i as f32 / sr;
            let f = 48.0 + 120.0 * (-t * 32.0).exp();
            phase += 2.0 * PI * f / sr;
            let body = phase.sin() * (-t * 7.5).exp();
            let click = if t < 0.003 { noise(&mut seed) * (1.0 - t / 0.003) * 0.4 } else { 0.0 };
            (body + click) * 0.85
        })
        .collect()
}

fn snare(sr: f32) -> Vec<f32> {
    let n = (0.28 * sr) as usize;
    let mut seed = 7;
    let mut lp = 0.0;
    let a = 1.0 - (-2.0 * PI * 1200.0 / sr).exp();
    (0..n)
        .map(|i| {
            let t = i as f32 / sr;
            let tone = ((2.0 * PI * 185.0 * t).sin() + 0.5 * (2.0 * PI * 330.0 * t).sin()) * (-t * 22.0).exp();
            let nz = noise(&mut seed);
            lp += a * (nz - lp);
            let rattle = (nz - lp) * (-t * 13.0).exp(); // high-passed noise
            (0.45 * tone + 0.7 * rattle) * 0.8
        })
        .collect()
}

fn hihat(sr: f32) -> Vec<f32> {
    let n = (0.09 * sr) as usize;
    let mut seed = 99;
    let (mut lp1, mut lp2) = (0.0, 0.0);
    let a = 1.0 - (-2.0 * PI * 6500.0 / sr).exp();
    (0..n)
        .map(|i| {
            let t = i as f32 / sr;
            let nz = noise(&mut seed);
            lp1 += a * (nz - lp1);
            let hp = nz - lp1;
            lp2 += a * (hp - lp2);
            let hp2 = hp - lp2;
            hp2 * (-t * 48.0).exp() * 0.55
        })
        .collect()
}

/// The three drum one-shots at the engine rate.
pub fn kit() -> Arc<[Vec<f32>; 3]> {
    let sr = SR as f32;
    Arc::new([kick(sr), snare(sr), hihat(sr)])
}

/// Snap a position to a grid of `grid` samples (wrapping inside `len`).
pub fn quantize(pos: u64, grid: f64, len: usize) -> u64 {
    if grid <= 1.0 {
        return pos;
    }
    ((pos as f64 / grid).round() * grid).round() as u64 % len.max(1) as u64
}

/// Render a pattern into a loop buffer of `len` samples (sounds wrap around the seam).
/// `beat` is the length of one beat in samples (used for each hit's snap).
pub fn render(hits: &[Hit], len: usize, kit: &[Vec<f32>; 3], beat: f64) -> Vec<f32> {
    let mut out = vec![0f32; len];
    if len == 0 {
        return out;
    }
    for h in hits {
        let Some(sample) = kit.get(h.drum as usize) else { continue };
        let start = match grid_len(h.snap, beat) {
            Some(g) => quantize(h.pos, g, len),
            None => h.pos % len as u64,
        } as usize;
        for (i, v) in sample.iter().enumerate() {
            out[(start + i) % len] += v;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_and_quantize() {
        let kit = kit();
        let len = 48000;
        let beat = 12000.0; // 240 bpm → 1/16 = 3000 samples
        let grid = grid_len(16, beat).unwrap();
        let hits = [Hit { pos: 2900, drum: 0, snap: 16 }, Hit { pos: 47990, drum: 2, snap: 16 }];
        let out = render(&hits, len, &kit, beat);
        assert_eq!(out.len(), len);
        assert_eq!(quantize(2900, grid, len), 3000);
        assert_eq!(quantize(47990, grid, len), 0, "snaps across the seam");
        assert!(out[3000..3200].iter().any(|v| v.abs() > 0.05), "kick at 3000");
        assert!(out[0..200].iter().any(|v| v.abs() > 0.01), "hat wrapped to 0");
        // first sound of a single hit = where it was placed
        let onset = |h: Hit| render(&[h], len, &kit, beat).iter().position(|v| *v != 0.0).unwrap();
        assert_eq!(onset(Hit { pos: 13000, drum: 1, snap: 4 }), 12000, "1/4 snaps to the beat");
        assert_eq!(onset(Hit { pos: 13000, drum: 1, snap: 8 }), 12000);
        assert_eq!(onset(Hit { pos: 19000, drum: 1, snap: 8 }), 18000, "1/8 = half a beat");
        assert_eq!(onset(Hit { pos: 11000, drum: 1, snap: 2 }), 0, "1/2 = two beats");
        assert_eq!(onset(Hit { pos: 26000, drum: 1, snap: 0 }), 26000, "unsnapped hit stays");
        assert!(out[30000..47000].iter().all(|v| *v == 0.0), "nothing between the hits");
        assert_eq!(grid_len(0, beat), None);
        assert_eq!(grid_len(2, beat), Some(24000.0));
        for s in kit.iter() {
            assert!(s.iter().all(|v| v.abs() <= 1.0));
        }
    }
}
