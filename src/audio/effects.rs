//! Offline, non-destructive per-track effects. Every effect is rendered circularly so the
//! processed loop stays seamless (tails wrap around the loop seam).

use super::dsp::{Fft, hann, lerp_at};
use rustfft::num_complex::Complex32;
use std::f32::consts::PI;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct TrackFx {
    /// Playback speed with unchanged pitch: 2–4× faster (the track repeats to fill its slot) or
    /// ½× / ¼× slower (the track gets 2× / 4× longer).
    pub speed: f32,
    /// −2..=2 octaves.
    pub octave: i32,
    /// 0 = dry only, 1 = shifted only.
    pub octave_blend: f32,
    pub drive: bool,
    pub drive_amount: f32,
    /// −1 (dark) .. 0 (neutral) .. 1 (bright).
    pub tone: f32,
    pub echo: bool,
    pub echo_ms: f32,
    pub echo_feedback: f32,
    pub echo_mix: f32,
    pub reverb: bool,
    pub reverb_size: f32,
    pub reverb_mix: f32,
    pub reverse: bool,
    /// Fade in over this many repetitions at the start of each stretch the track plays in
    /// (song mode) or of the export (loop mode); 0 = off. A volume curve, not rendered.
    pub fade_in: u32,
    /// Fade out over this many repetitions at the end (see `fade_in`); 0 = off.
    pub fade_out: u32,
}

impl Default for TrackFx {
    fn default() -> Self {
        Self {
            speed: 1.0,
            octave: 0,
            octave_blend: 1.0,
            drive: false,
            drive_amount: 0.3,
            tone: 0.0,
            echo: false,
            echo_ms: 375.0,
            echo_feedback: 0.4,
            echo_mix: 0.35,
            reverb: false,
            reverb_size: 0.6,
            reverb_mix: 0.3,
            reverse: false,
            fade_in: 0,
            fade_out: 0,
        }
    }
}

impl TrackFx {
    /// Whole-number speed-up factor (1 = none).
    pub fn speed_up(&self) -> u32 {
        if self.speed > 1.0 { (self.speed.round() as u32).clamp(1, 4) } else { 1 }
    }

    /// Whole-number slow-down factor (1 = none, 2 = half speed, 4 = quarter speed). The rendered
    /// track is this many times longer than the raw take.
    pub fn slow_down(&self) -> usize {
        if self.speed > 0.0 && self.speed < 1.0 { ((1.0 / self.speed).round() as usize).clamp(1, 4) } else { 1 }
    }

    /// Map an index into the rendered track back to the raw take (undoes speed and reverse),
    /// e.g. to place a drum hit played over the processed sound.
    pub fn source_index(&self, idx: usize, raw_len: usize) -> usize {
        if raw_len == 0 {
            return 0;
        }
        let i = idx / self.slow_down();
        let i = (i * self.speed_up() as usize) % raw_len;
        if self.reverse { raw_len - 1 - i } else { i }
    }

    /// The same settings without the fades (which don't change the rendered sound).
    pub fn sound(&self) -> TrackFx {
        TrackFx { fade_in: 0, fade_out: 0, ..self.clone() }
    }

    pub fn has_fade(&self) -> bool {
        self.fade_in > 0 || self.fade_out > 0
    }

    /// No effect changes the sound (fades are a volume curve and don't count).
    pub fn is_neutral(&self) -> bool {
        (self.octave == 0 || self.octave_blend <= 0.0)
            && self.speed_up() == 1
            && self.slow_down() == 1
            && !self.drive
            && self.tone.abs() < 0.01
            && !self.echo
            && !self.reverb
            && !self.reverse
    }

    /// Short summary for the track row ("Oct +1 · Echo · Reverb").
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        match (self.speed_up(), self.slow_down()) {
            (n, _) if n > 1 => parts.push(format!("{n}× speed")),
            (_, 2) => parts.push("½× speed".into()),
            (_, n) if n > 1 => parts.push(format!("1/{n}× speed")),
            _ => {}
        }
        if self.octave != 0 && self.octave_blend > 0.0 {
            parts.push(format!("Oct {:+}", self.octave));
        }
        if self.reverse {
            parts.push("Reverse".into());
        }
        if self.drive {
            parts.push("Drive".into());
        }
        if self.tone < -0.01 {
            parts.push("Dark".into());
        } else if self.tone > 0.01 {
            parts.push("Bright".into());
        }
        if self.echo {
            parts.push("Echo".into());
        }
        if self.fade_in > 0 {
            parts.push("Fade in".into());
        }
        if self.fade_out > 0 {
            parts.push("Fade out".into());
        }
        if self.reverb {
            parts.push("Reverb".into());
        }
        parts.join(" · ")
    }
}

/// Render `fx` onto one loop cycle `x`. The length is preserved, except when slowing down
/// (then it is `slow_down()` times longer).
pub fn render(x: &[f32], fx: &TrackFx, sr: f32) -> Vec<f32> {
    if x.is_empty() || fx.is_neutral() {
        return x.to_vec();
    }
    let mut cycle: Vec<f32> = x.to_vec();
    if fx.reverse {
        cycle.reverse();
    }
    if fx.speed_up() > 1 {
        cycle = speed_up_circular(&cycle, fx.speed_up());
    } else if fx.slow_down() > 1 {
        cycle = slow_down_circular(&cycle, fx.slow_down());
    }
    let len = cycle.len();
    if fx.octave != 0 && fx.octave_blend > 0.0 {
        let shifted = pitch_shift_circular(&cycle, 2f32.powi(fx.octave));
        let b = fx.octave_blend.clamp(0.0, 1.0);
        // equal-ish power blend, compensate level of the shifted signal
        cycle = cycle.iter().zip(&shifted).map(|(d, s)| d * (1.0 - b) + s * b).collect();
    }

    // Stateful effects: run over several repetitions so tails wrap, keep the last cycle.
    let mut tail_s = 0.05f32;
    if fx.echo {
        let fb = fx.echo_feedback.clamp(0.0, 0.92);
        let repeats = if fb > 0.0 { (0.001f32).ln() / fb.ln() } else { 1.0 };
        tail_s = tail_s.max(fx.echo_ms / 1000.0 * (repeats + 1.0));
    }
    if fx.reverb {
        tail_s = tail_s.max(1.0 + 4.0 * fx.reverb_size);
    }
    let reps = ((tail_s * sr / len as f32).ceil() as usize + 1).clamp(2, 24);

    let mut chain = Chain::new(fx, sr);
    let mut out = vec![0f32; len];
    for r in 0..reps {
        let last = r == reps - 1;
        for (i, &s) in cycle.iter().enumerate() {
            let y = chain.process(s);
            if last {
                out[i] = y;
            }
        }
    }
    out
}

/// Gentle overdrive: level-matched at a typical playing level, blended with the dry signal
/// and low-passed so it warms up instead of fizzing.
struct Drive {
    gain: f32,
    norm: f32,
    wet: f32,
    lp: f32,
    a: f32,
}

impl Drive {
    const REF: f32 = 0.2;
    fn new(amount: f32, sr: f32) -> Self {
        let amount = amount.clamp(0.0, 1.0);
        let gain = 1.0 + 8.0 * amount.powf(1.5);
        Self {
            gain,
            norm: Self::REF / (gain * Self::REF).tanh(),
            wet: 0.35 + 0.55 * amount,
            lp: 0.0,
            a: 1.0 - (-2.0 * PI * 5500.0 / sr).exp(),
        }
    }
    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let d = (x * self.gain).tanh() * self.norm;
        self.lp += self.a * (d - self.lp);
        x * (1.0 - self.wet) + self.lp * self.wet
    }
}

struct Chain {
    drive: Option<Drive>,
    tone: Tone,
    echo: Option<Echo>,
    reverb: Option<(Freeverb, f32)>,
}

impl Chain {
    fn new(fx: &TrackFx, sr: f32) -> Self {
        Self {
            drive: fx.drive.then(|| Drive::new(fx.drive_amount, sr)),
            tone: Tone::new(fx.tone, sr),
            echo: fx.echo.then(|| Echo::new(fx, sr)),
            reverb: fx.reverb.then(|| (Freeverb::new(fx.reverb_size, sr), fx.reverb_mix)),
        }
    }

    #[inline]
    fn process(&mut self, mut x: f32) -> f32 {
        if let Some(d) = &mut self.drive {
            x = d.process(x);
        }
        x = self.tone.process(x);
        if let Some(e) = &mut self.echo {
            x = e.process(x);
        }
        if let Some((rv, mix)) = &mut self.reverb {
            let wet = rv.process(x);
            x = x * (1.0 - *mix * 0.5) + wet * *mix;
        }
        x
    }
}

/// Tilt EQ: negative = low-pass (darker), positive = presence boost (brighter).
struct Tone {
    amount: f32,
    a: f32,
    lp: f32,
}

impl Tone {
    fn new(amount: f32, sr: f32) -> Self {
        let amount = amount.clamp(-1.0, 1.0);
        let cutoff = if amount < 0.0 {
            // 12 kHz at 0 → 600 Hz at -1 (exponential)
            12000.0 * (600.0f32 / 12000.0).powf(-amount)
        } else {
            1500.0
        };
        let a = 1.0 - (-2.0 * PI * cutoff / sr).exp();
        Self { amount, a, lp: 0.0 }
    }
    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        if self.amount.abs() < 0.01 {
            return x;
        }
        self.lp += self.a * (x - self.lp);
        if self.amount < 0.0 { self.lp } else { x + (x - self.lp) * self.amount * 1.5 }
    }
}

struct Echo {
    buf: Vec<f32>,
    idx: usize,
    fb: f32,
    mix: f32,
    damp: f32,
    lp: f32,
}

impl Echo {
    fn new(fx: &TrackFx, sr: f32) -> Self {
        let d = ((fx.echo_ms.clamp(20.0, 2000.0) / 1000.0) * sr) as usize;
        Self {
            buf: vec![0.0; d.max(1)],
            idx: 0,
            fb: fx.echo_feedback.clamp(0.0, 0.92),
            mix: fx.echo_mix.clamp(0.0, 1.0),
            damp: 0.35,
            lp: 0.0,
        }
    }
    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let delayed = self.buf[self.idx];
        // darken repeats like a tape/analog delay
        self.lp += (1.0 - self.damp) * (delayed - self.lp);
        self.buf[self.idx] = x + self.lp * self.fb;
        self.idx = (self.idx + 1) % self.buf.len();
        x + delayed * self.mix
    }
}

/// Classic Schroeder/Moorer "Freeverb" (mono).
struct Freeverb {
    combs: Vec<(Vec<f32>, usize, f32)>,
    allpasses: Vec<(Vec<f32>, usize)>,
    feedback: f32,
    damp: f32,
}

impl Freeverb {
    fn new(size: f32, sr: f32) -> Self {
        let scale = sr / 44100.0;
        let comb_t = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
        let ap_t = [556, 441, 341, 225];
        Self {
            combs: comb_t.iter().map(|&t| (vec![0.0; (t as f32 * scale) as usize], 0, 0.0)).collect(),
            allpasses: ap_t.iter().map(|&t| (vec![0.0; (t as f32 * scale) as usize], 0)).collect(),
            feedback: 0.7 + 0.28 * size.clamp(0.0, 1.0),
            damp: 0.25,
        }
    }
    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let input = x * 0.015;
        let mut out = 0.0;
        for (buf, idx, store) in &mut self.combs {
            let y = buf[*idx];
            *store = y * (1.0 - self.damp) + *store * self.damp;
            buf[*idx] = input + *store * self.feedback;
            *idx = (*idx + 1) % buf.len();
            out += y;
        }
        for (buf, idx) in &mut self.allpasses {
            let b = buf[*idx];
            buf[*idx] = out + b * 0.5;
            *idx = (*idx + 1) % buf.len();
            out = b - out;
        }
        out * 3.0
    }
}

/// Play a loop `speed` times faster without changing its pitch; the result repeats `speed`
/// times so the length stays the same. Rendered circularly → seamless.
pub fn speed_up_circular(x: &[f32], speed: u32) -> Vec<f32> {
    let len = x.len();
    if len == 0 || speed <= 1 {
        return x.to_vec();
    }
    let tiled: Vec<f32> = x.iter().chain(x).chain(x).copied().collect();
    let stretched = time_stretch(&tiled, 1.0 / speed as f32);
    let period = len as f64 / speed as f64;
    (0..len).map(|i| lerp_at(&stretched, period + (i as f64 % period))).collect()
}

/// Play a loop `factor` times slower without changing its pitch (the result is `factor` times
/// longer). Rendered circularly → seamless.
pub fn slow_down_circular(x: &[f32], factor: usize) -> Vec<f32> {
    let len = x.len();
    if len == 0 || factor <= 1 {
        return x.to_vec();
    }
    let tiled: Vec<f32> = x.iter().chain(x).chain(x).copied().collect();
    let stretched = time_stretch(&tiled, factor as f32);
    let n = len * factor;
    (0..n).map(|i| lerp_at(&stretched, (n + i) as f64)).collect()
}

/// Duration-preserving pitch shift by `ratio` (2.0 = one octave up) for a loop: the loop is
/// processed as three repetitions and the middle one is kept, so the result is seamless.
pub fn pitch_shift_circular(x: &[f32], ratio: f32) -> Vec<f32> {
    let len = x.len();
    if len == 0 || (ratio - 1.0).abs() < 1e-6 {
        return x.to_vec();
    }
    let tiled: Vec<f32> = x.iter().chain(x).chain(x).copied().collect();
    let shifted = pitch_shift(&tiled, ratio);
    shifted[len..2 * len].to_vec()
}

/// Phase-vocoder time stretch by `ratio` followed by resampling back to the original length.
pub fn pitch_shift(x: &[f32], ratio: f32) -> Vec<f32> {
    let stretched = time_stretch(x, ratio);
    let r = ratio as f64;
    (0..x.len()).map(|i| lerp_at(&stretched, i as f64 * r)).collect()
}

/// Phase vocoder with identity phase locking (Laroche & Dolson) – keeps guitar
/// harmonics coherent and transients reasonably crisp.
pub fn time_stretch(x: &[f32], ratio: f32) -> Vec<f32> {
    const N: usize = 4096;
    const HS: usize = N / 4;
    let ha = HS as f64 / ratio as f64;
    let w = hann(N);
    let mut fft = Fft::new(N);
    let bins = N / 2 + 1;

    let out_len = (x.len() as f64 * ratio as f64).ceil() as usize + N;
    let mut out = vec![0f32; out_len + N];
    let mut norm = vec![0f32; out_len + N];

    let mut prev_phase = vec![0f32; bins];
    let mut synth_phase = vec![0f32; bins];
    let mut first = true;
    let mut buf = vec![Complex32::default(); N];
    let mut mag = vec![0f32; bins];
    let mut phase = vec![0f32; bins];

    // pad the input at the front so the first frame is centred on sample 0
    let read = |pos: isize| -> f32 { if pos < 0 || pos as usize >= x.len() { 0.0 } else { x[pos as usize] } };

    let frames = ((x.len() as f64) / ha).ceil() as usize + 2;
    for f in 0..frames {
        let apos = (f as f64 * ha) as isize - (N as isize / 2);
        let spos = f * HS;
        if spos + N > out.len() {
            break;
        }
        for i in 0..N {
            buf[i] = Complex32::new(read(apos + i as isize) * w[i], 0.0);
        }
        fft.forward(&mut buf);
        for k in 0..bins {
            mag[k] = buf[k].norm();
            phase[k] = buf[k].arg();
        }
        if first {
            synth_phase.copy_from_slice(&phase);
            first = false;
        } else {
            // advance phases of spectral peaks, lock the surrounding bins to them
            let mut peak_of = vec![0usize; bins];
            let mut peaks = Vec::new();
            for k in 0..bins {
                let l = if k > 1 {
                    mag[k - 2].max(mag[k - 1])
                } else if k == 1 {
                    mag[0]
                } else {
                    0.0
                };
                let r = if k + 2 < bins {
                    mag[k + 1].max(mag[k + 2])
                } else if k + 1 < bins {
                    mag[k + 1]
                } else {
                    0.0
                };
                if mag[k] > l && mag[k] >= r {
                    peaks.push(k);
                }
            }
            if peaks.is_empty() {
                peaks.push(0);
            }
            // nearest-peak region assignment
            let mut pi = 0;
            for (k, p) in peak_of.iter_mut().enumerate() {
                while pi + 1 < peaks.len()
                    && (peaks[pi + 1] as isize - k as isize).abs() < (peaks[pi] as isize - k as isize).abs()
                {
                    pi += 1;
                }
                *p = peaks[pi];
            }
            let mut new_synth = synth_phase.clone();
            for &k in &peaks {
                let omega = 2.0 * PI * k as f32 / N as f32;
                let expected = omega * ha as f32;
                let mut dphi = phase[k] - prev_phase[k] - expected;
                dphi -= 2.0 * PI * (dphi / (2.0 * PI)).round();
                let inst = omega + dphi / ha as f32;
                new_synth[k] = synth_phase[k] + inst * HS as f32;
            }
            for k in 0..bins {
                let p = peak_of[k];
                if p != k {
                    new_synth[k] = new_synth[p] + (phase[k] - phase[p]);
                }
            }
            synth_phase = new_synth;
        }
        prev_phase.copy_from_slice(&phase);

        for k in 0..bins {
            buf[k] = Complex32::from_polar(mag[k], synth_phase[k]);
            if k > 0 && k < N / 2 {
                buf[N - k] = buf[k].conj();
            }
        }
        fft.inverse(&mut buf);
        for i in 0..N {
            out[spos + i] += buf[i].re * w[i];
            norm[spos + i] += w[i] * w[i];
        }
    }
    // remove the N/2 centring offset and normalise
    let off = N / 2;
    let want = (x.len() as f64 * ratio as f64).round() as usize;
    (0..want)
        .map(|i| {
            let j = i + off;
            if j < out.len() && norm[j] > 1e-3 { out[j] / norm[j] } else { 0.0 }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::dsp::{peak_frequency, rms, sine};

    #[test]
    fn octave_up_doubles_frequency_and_keeps_length() {
        let sr = 48000.0;
        let x = sine(220.0, sr, 48000, 0.5);
        let y = pitch_shift_circular(&x, 2.0);
        assert_eq!(y.len(), x.len());
        let f = peak_frequency(&y[8000..40000], sr);
        assert!((f - 440.0).abs() < 8.0, "got {f}");
        assert!(rms(&y) > 0.2, "level collapsed: {}", rms(&y));
    }

    #[test]
    fn octave_down_halves_frequency() {
        let sr = 48000.0;
        let x = sine(330.0, sr, 48000, 0.5);
        let y = pitch_shift_circular(&x, 0.5);
        assert_eq!(y.len(), x.len());
        let f = peak_frequency(&y[8000..40000], sr);
        assert!((f - 165.0).abs() < 8.0, "got {f}");
    }

    #[test]
    fn neutral_fx_is_identity_and_echo_wraps() {
        let sr = 48000.0;
        let x = sine(220.0, sr, 4800, 0.5);
        assert_eq!(render(&x, &TrackFx::default(), sr), x);
        // an impulse near the end of the loop must echo at the start (wrap-around)
        let mut imp = vec![0f32; 48000];
        imp[47000] = 1.0;
        let fx = TrackFx { echo: true, echo_ms: 100.0, echo_mix: 0.5, ..Default::default() };
        let y = render(&imp, &fx, sr);
        assert_eq!(y.len(), imp.len());
        let wrapped = 47000 + 4800 - 48000;
        assert!(y[wrapped - 5..wrapped + 5].iter().any(|v| v.abs() > 0.2));
    }

    #[test]
    fn speed_keeps_pitch_and_repeats() {
        let sr = 48000.0;
        let len = 96000;
        // a 220 Hz tone burst in the first quarter of the loop
        let mut x = vec![0f32; len];
        let tone = sine(220.0, sr, 24000, 0.5);
        x[..24000].copy_from_slice(&tone);
        let y = speed_up_circular(&x, 4);
        assert_eq!(y.len(), len);
        // pitch unchanged inside the sped-up burst (now 6000 samples long, 4 times per loop)
        let f = peak_frequency(&y[500..5500], sr);
        assert!((f - 220.0).abs() < 15.0, "pitch changed: {f}");
        for k in 0..4 {
            let start = k * 24000;
            assert!(rms(&y[start + 1000..start + 5000]) > 0.15, "burst {k} missing");
            assert!(rms(&y[start + 9000..start + 20000]) < 0.08, "gap {k} not silent");
        }
    }

    #[test]
    fn slow_down_keeps_pitch_and_doubles_length() {
        let sr = 48000.0;
        let len = 48000;
        let mut x = vec![0f32; len];
        x[..12000].copy_from_slice(&sine(220.0, sr, 12000, 0.5));
        let fx = TrackFx { speed: 0.5, ..Default::default() };
        assert_eq!(fx.slow_down(), 2);
        let y = render(&x, &fx, sr);
        assert_eq!(y.len(), 2 * len);
        let f = peak_frequency(&y[2000..22000], sr);
        assert!((f - 220.0).abs() < 15.0, "pitch changed: {f}");
        // the burst now lasts 24000 samples, the rest stays quiet
        assert!(rms(&y[2000..22000]) > 0.15);
        assert!(rms(&y[30000..90000]) < 0.08);
        assert_eq!(fx.source_index(30000, len), 15000);
        let fast = TrackFx { speed: 4.0, reverse: true, ..Default::default() };
        assert_eq!(fast.source_index(100, len), len - 1 - 400);
        // old projects stored the speed as an integer
        let old: TrackFx = serde_json::from_str(r#"{"speed": 4}"#).unwrap();
        assert_eq!(old.speed_up(), 4);
    }

    #[test]
    fn drive_is_gentle() {
        let sr = 48000.0;
        for amp in [0.05f32, 0.25, 0.6] {
            let x = sine(196.0, sr, 24000, amp);
            for amount in [0.3f32, 1.0] {
                let fx = TrackFx { drive: true, drive_amount: amount, ..Default::default() };
                let y = render(&x, &fx, sr);
                let gain_db = 20.0 * (rms(&y) / rms(&x)).log10();
                // default setting barely changes the level; even full drive stays within ±10 dB
                let limit = if amount < 0.5 { 3.0 } else { 10.0 };
                assert!(gain_db.abs() < limit, "amp {amp} amount {amount}: {gain_db:.1} dB");
                assert!(y.iter().all(|v| v.abs() <= 1.0));
            }
        }
    }

    #[test]
    fn reverse_reverses() {
        let x: Vec<f32> = (0..100).map(|i| i as f32 / 100.0).collect();
        let fx = TrackFx { reverse: true, ..Default::default() };
        let y = render(&x, &fx, 48000.0);
        assert!((y[0] - 0.99).abs() < 0.02 && y[99].abs() < 0.02);
    }
}

#[cfg(test)]
mod align_tests {
    use super::*;
    #[test]
    fn pitch_shift_keeps_timing() {
        let sr = 48000.0;
        let len = 48000;
        for ratio in [2.0f32, 0.5, 4.0] {
            let mut x = vec![0f32; len];
            for i in 0..4800 {
                let t = i as f32 / sr;
                x[20000 + i] = (2.0 * PI * 330.0 * t).sin() * (-t * 20.0).exp();
            }
            let y = pitch_shift_circular(&x, ratio);
            // onset = first sample above 10% of max
            let m = y.iter().fold(0f32, |a, v| a.max(v.abs()));
            let onset = y.iter().position(|v| v.abs() > 0.1 * m).unwrap();
            eprintln!("ratio {ratio}: onset {onset} (expected ~20000)");
            assert!((onset as i64 - 20000).abs() < 1500, "ratio {ratio}: onset {onset}");
        }
    }
}
