//! Importing audio files (MP3, WAV, FLAC, OGG/Vorbis, M4A/AAC, ALAC) as tracks.

use super::effects::time_stretch;
use super::looper::SR;
use std::path::Path;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
use symphonia::core::errors::Error;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;

pub const EXTENSIONS: &[&str] = &["mp3", "wav", "flac", "ogg", "oga", "m4a", "mp4", "aac", "aif", "aiff"];

/// Longest file we accept (10 minutes).
const MAX_SECONDS: usize = 600;

/// Decode a file to mono f32 at the engine rate.
pub fn decode(path: &Path) -> anyhow::Result<Vec<f32>> {
    let file = std::fs::File::open(path)?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| anyhow::anyhow!("unsupported or damaged file ({e})"))?;
    let mut format = probed.format;
    let track = format
        .tracks()
        .iter()
        .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .ok_or_else(|| anyhow::anyhow!("no audio track found"))?;
    let track_id = track.id;
    let mut rate = track.codec_params.sample_rate.unwrap_or(SR);
    let mut decoder = symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default())?;

    let mut mono: Vec<f32> = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(Error::ResetRequired) => break,
            Err(e) => return Err(e.into()),
        };
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(buf) => {
                let spec = *buf.spec();
                rate = spec.rate;
                let ch = spec.channels.count().max(1);
                let mut sb = SampleBuffer::<f32>::new(buf.capacity() as u64, spec);
                sb.copy_interleaved_ref(buf);
                for frame in sb.samples().chunks_exact(ch) {
                    mono.push(frame.iter().sum::<f32>() / ch as f32);
                }
                anyhow::ensure!(mono.len() < rate as usize * MAX_SECONDS, "file is longer than 10 minutes");
            }
            Err(Error::DecodeError(_)) => continue, // skip a corrupt frame
            Err(e) => return Err(e.into()),
        }
    }
    anyhow::ensure!(!mono.is_empty(), "the file contains no audio");
    Ok(resample(&mono, rate, SR))
}

/// Band-limited enough resampling for imports: linear interpolation after a light low-pass
/// when downsampling.
pub fn resample(x: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || x.is_empty() {
        return x.to_vec();
    }
    let mut src = x.to_vec();
    if to < from {
        // simple 2-pole low-pass at ~0.45 * target rate to avoid aliasing
        let fc = 0.45 * to as f32;
        let a = 1.0 - (-2.0 * std::f32::consts::PI * fc / from as f32).exp();
        for _ in 0..2 {
            let mut y = 0.0;
            for v in src.iter_mut() {
                y += a * (*v - y);
                *v = y;
            }
        }
    }
    let ratio = from as f64 / to as f64;
    let n = (x.len() as f64 / ratio).floor() as usize;
    (0..n).map(|i| super::dsp::lerp_at(&src, i as f64 * ratio)).collect()
}

/// Index of the first sample above −50 dBFS (minus 10 ms margin).
pub fn first_sound(x: &[f32]) -> usize {
    x.iter().position(|v| v.abs() > 0.003).unwrap_or(0).saturating_sub(SR as usize / 100)
}

/// Remove leading/trailing near-silence (below −50 dBFS), keeping 10 ms of margin.
pub fn trim_silence(x: &[f32]) -> &[f32] {
    let thr = 0.003;
    let first = x.iter().position(|v| v.abs() > thr).unwrap_or(0);
    let last = x.iter().rposition(|v| v.abs() > thr).map(|i| i + 1).unwrap_or(x.len());
    let margin = SR as usize / 100;
    &x[first.saturating_sub(margin)..(last + margin).min(x.len())]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fit {
    /// Cut off what is too long, pad with silence what is too short.
    CutOrPad,
    /// Time-stretch to the exact length (pitch stays the same).
    Stretch,
}

/// Nearest loop multiple (1..=8) for audio of `len` samples.
pub fn nearest_multiple(len: usize, loop_len: usize) -> u32 {
    ((len as f64 / loop_len.max(1) as f64).round() as u32).clamp(1, 8)
}

/// Make `x` exactly `target` samples long.
pub fn fit(x: &[f32], target: usize, mode: Fit) -> Vec<f32> {
    let mut out = match mode {
        Fit::CutOrPad => x.to_vec(),
        Fit::Stretch => time_stretch(x, target as f32 / x.len().max(1) as f32),
    };
    out.resize(target, 0.0);
    // short fades so the loop seam doesn't click
    let f = (SR as usize / 200).min(target / 4);
    for i in 0..f {
        let g = i as f32 / f as f32;
        out[i] *= g;
        out[target - 1 - i] *= g;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::dsp::{peak_frequency, sine};

    #[test]
    fn decodes_wav_and_resamples() {
        let path = std::env::temp_dir().join(format!("ss-import-{}.wav", std::process::id()));
        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 44100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(&path, spec).unwrap();
        for i in 0..44100 {
            let v = (0.5 * (2.0 * std::f32::consts::PI * 441.0 * i as f32 / 44100.0).sin() * 32767.0) as i16;
            w.write_sample(v).unwrap();
            w.write_sample(v).unwrap();
        }
        w.finalize().unwrap();
        let x = decode(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert!((x.len() as i64 - 48000).abs() < 10, "len {}", x.len());
        let f = peak_frequency(&x[4000..44000], SR as f32);
        assert!((f - 441.0).abs() < 4.0, "freq {f}");
    }

    #[test]
    fn fit_modes() {
        let x = sine(220.0, 48000.0, 50000, 0.5);
        assert_eq!(fit(&x, 48000, Fit::CutOrPad).len(), 48000);
        let s = fit(&x, 48000, Fit::Stretch);
        assert_eq!(s.len(), 48000);
        let f = peak_frequency(&s[4000..44000], 48000.0);
        assert!((f - 220.0).abs() < 5.0, "stretch must keep pitch, got {f}");
        assert_eq!(nearest_multiple(95000, 48000), 2);
        assert_eq!(nearest_multiple(10, 48000), 1);
        let padded = [vec![0.0; 1000], vec![0.5; 2000], vec![0.0; 1000]].concat();
        assert_eq!(trim_silence(&padded).len(), 2000 + 2 * 480);
    }
}
