//! WAV export of the arranged song (mixdown) and individual stems.

use crate::audio::looper::{SR, env_gain, track_slot};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub struct ExportTrack {
    pub name: String,
    pub buf: Arc<Vec<f32>>,
    pub gain: f32,
    pub audible: bool,
    /// Arrangement map (see [`track_slot`]); `None` = loops everywhere.
    pub map: Option<Vec<i32>>,
    /// Song-mode fade envelope per repetition (see `env_gain`).
    pub env: Option<Vec<(f32, f32)>>,
    /// Loop mode (no `map`): fade in / out over this many loops at the start / end of the export.
    pub fade: (u32, u32),
    /// Song-mode filter curve (see `dsp::filter_at`).
    pub filter: Option<Vec<f32>>,
}

pub struct Song {
    pub loop_len: usize,
    /// Length of one pass through the song.
    pub song_len: usize,
    pub repeats: u32,
}

fn write_wav(path: &Path, samples: &[f32], scale: f32) -> anyhow::Result<()> {
    let spec =
        hound::WavSpec { channels: 2, sample_rate: SR, bits_per_sample: 24, sample_format: hound::SampleFormat::Int };
    let mut w = hound::WavWriter::create(path, spec)?;
    let full = (1 << 23) as f32 - 1.0;
    for &s in samples {
        let v = ((s * scale).clamp(-1.0, 1.0) * full) as i32;
        w.write_sample(v)?;
        w.write_sample(v)?;
    }
    w.finalize()?;
    Ok(())
}

/// Extra samples after the last pass so clips that wrap around the song end can finish.
fn wrap_tail(t: &ExportTrack, loop_len: usize) -> usize {
    let Some(m) = &t.map else { return 0 };
    // wrapped repetitions are at the start of the map: map[r] > r
    m.iter().enumerate().filter(|&(r, &s)| s > r as i32).map(|(r, _)| (r + 1) * loop_len).max().unwrap_or(0)
}

/// The export is linear: a clip that wraps around the song end is not heard at the very start
/// (nothing played before it), and after the last pass the song runs on until it has finished.
fn render(t: &ExportTrack, song: &Song, total: usize) -> Vec<f32> {
    let passes = song.repeats.max(1) as usize;
    let song_len = song.song_len.max(1);
    let main = song_len * passes; // without the wrap tail
    let lf = song.loop_len.max(1) as f32;
    // loop mode: fade over the whole export
    let loop_fade = |i: usize| {
        let (fin, fout) = t.fade;
        let mut g = 1.0f32;
        if fin > 0 {
            g *= (i as f32 / (fin as f32 * lf)).min(1.0);
        }
        if fout > 0 {
            g *= (main.saturating_sub(i) as f32 / (fout as f32 * lf)).min(1.0);
        }
        g
    };
    let mut out: Vec<f32> = (0..total)
        .map(|i| {
            let pass = i / song_len;
            let pos = (i % song_len) as u64;
            match track_slot(pos, song.loop_len, t.buf.len(), t.map.as_deref()) {
                Some((k, wrapped)) if (wrapped && pass >= 1) || (!wrapped && pass < passes) => {
                    let fade = match (&t.env, &t.map) {
                        (Some(e), _) => env_gain(pos, song.loop_len, e),
                        (None, None) => loop_fade(i),
                        _ => 1.0,
                    };
                    t.buf[k] * t.gain * fade
                }
                _ => 0.0,
            }
        })
        .collect();
    // the same per-block filter the engine uses
    if let Some(curve) = &t.filter {
        let mut f = crate::audio::dsp::TrackFilter::default();
        for (b, block) in out.chunks_mut(256).enumerate() {
            let pos = ((b * 256) % song_len) as u64;
            f.process(block, crate::audio::dsp::filter_at(curve, pos, song.loop_len), SR as f32);
        }
    }
    out
}

fn fade_out(x: &mut [f32]) {
    let n = (SR as usize / 50).min(x.len() / 2).max(1); // 20 ms
    let len = x.len();
    for i in 0..n {
        x[len - n + i] *= 1.0 - (i + 1) as f32 / n as f32;
    }
}

/// Write the mix to `path` (and stems next to it). The mix is normalised to −0.5 dBFS only if
/// it would clip – no limiter, no distortion. Stems share the same scale so they sum to the mix.
pub fn export(tracks: &[ExportTrack], song: &Song, path: &Path, stems: bool) -> anyhow::Result<Vec<PathBuf>> {
    let tail = tracks.iter().filter(|t| t.audible).map(|t| wrap_tail(t, song.loop_len)).max().unwrap_or(0);
    let total = song.song_len * song.repeats.max(1) as usize + tail;
    anyhow::ensure!(total > tail && !tracks.is_empty(), "Nothing to export yet");
    let rendered: Vec<Vec<f32>> = tracks.iter().map(|t| render(t, song, total)).collect();
    let mut mix = vec![0f32; total];
    for (t, r) in tracks.iter().zip(&rendered) {
        if t.audible {
            for (m, v) in mix.iter_mut().zip(r) {
                *m += v;
            }
        }
    }
    fade_out(&mut mix);
    let peak = mix.iter().fold(0f32, |a, v| a.max(v.abs()));
    let ceiling = 0.944; // −0.5 dBFS
    let scale = if peak > ceiling { ceiling / peak } else { 1.0 };
    write_wav(path, &mix, scale)?;
    let mut written = vec![path.to_path_buf()];

    if stems {
        let stem_base = path.with_extension("");
        for (t, mut r) in tracks.iter().zip(rendered) {
            let safe: String = t.name.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
            let p = PathBuf::from(format!("{}-{}.wav", stem_base.display(), safe));
            fade_out(&mut r);
            write_wav(&p, &r, scale)?;
            written.push(p);
        }
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exports_arranged_song_and_normalises() {
        let dir = std::env::temp_dir().join(format!("synth-string-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let tracks = vec![
            ExportTrack {
                name: "Track 1".into(),
                buf: Arc::new(vec![0.25; 4000]),
                gain: 1.0,
                audible: true,
                map: None,
                env: None,
                fade: (0, 0),
                filter: None,
            },
            // a "solo" only in repetition 2
            ExportTrack {
                name: "Solo".into(),
                buf: Arc::new(vec![0.9; 4000]),
                gain: 1.0,
                audible: true,
                map: Some(vec![-1, 1, -1]),
                env: None,
                fade: (0, 0),
                filter: None,
            },
        ];
        let song = Song { loop_len: 4000, song_len: 12000, repeats: 1 };
        let files = export(&tracks, &song, &dir.join("mix.wav"), true).unwrap();
        assert_eq!(files.len(), 3);
        let mix: Vec<i32> = hound::WavReader::open(&files[0]).unwrap().samples::<i32>().map(|s| s.unwrap()).collect();
        assert_eq!(mix.len(), 12000 * 2);
        let full = (1 << 23) as f32 - 1.0;
        let at = |frame: usize| mix[frame * 2] as f32 / full;
        // 1.15 peak → scaled to 0.944: rep 1 = 0.25*s, rep 2 = 1.15*s
        let s = 0.944 / 1.15;
        assert!((at(2000) - 0.25 * s).abs() < 1e-3, "{}", at(2000));
        assert!((at(6000) - 0.944).abs() < 1e-3, "{}", at(6000));
        assert!((at(9000) - 0.25 * s).abs() < 1e-3);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn wrapped_clip_is_linear_in_export() {
        // 3 reps of 1000; a 2-rep clip at rep 2 wraps into rep 0
        let t = ExportTrack {
            name: "Solo".into(),
            buf: Arc::new((0..2000).map(|i| i as f32).collect()),
            gain: 1.0,
            audible: true,
            map: Some(vec![2, -1, 2]),
            env: None,
            fade: (0, 0),
            filter: None,
        };
        let song = Song { loop_len: 1000, song_len: 3000, repeats: 2 };
        assert_eq!(wrap_tail(&t, 1000), 1000);
        let y = render(&t, &song, 7000);
        assert_eq!(y[500], 0.0, "no wrapped tail before the song started");
        assert_eq!(y[2500], 500.0);
        assert_eq!(y[3500], 1500.0, "second pass starts with the tail of the first");
        assert_eq!(y[5500], 500.0);
        assert_eq!(y[6500], 1500.0, "after the last pass the clip finishes");
    }

    #[test]
    fn loop_mode_export_fades() {
        let t = ExportTrack {
            name: "Pad".into(),
            buf: Arc::new(vec![1.0; 1000]),
            gain: 1.0,
            audible: true,
            map: None,
            env: None,
            fade: (1, 2),
            filter: None,
        };
        let song = Song { loop_len: 1000, song_len: 1000, repeats: 4 };
        let y = render(&t, &song, 4000);
        assert_eq!(y[0], 0.0);
        assert!((y[500] - 0.5).abs() < 1e-3, "fading in");
        assert_eq!(y[1500], 1.0);
        assert!((y[3000] - 0.5).abs() < 1e-3, "fading out over the last 2 loops");
        assert!(y[3999] < 0.01);
    }
}
