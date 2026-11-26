//! Project files (`*.synthstring`): a small binary container holding a JSON description
//! followed by the raw (unprocessed) audio of every track and its noise profile, so effects and
//! noise reduction stay editable after reopening.
//!
//! Layout: `b"SYNTHSTR"` · u32 version · u64 JSON length · JSON · f32-LE blobs (in track order:
//! `len` samples of audio, then `noise_len` noise-profile magnitudes).

use crate::audio::effects::TrackFx;
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::Path;

const MAGIC: &[u8; 8] = b"SYNTHSTR";
const VERSION: u32 = 1;
pub const EXTENSION: &str = "synthstring";

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectMeta {
    pub loop_len: usize,
    pub bpm: f32,
    pub bars: Option<u32>,
    pub beats_per_bar: u32,
    pub song_mode: bool,
    pub song_reps: u32,
    pub tracks: Vec<TrackMeta>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TrackMeta {
    pub id: u32,
    pub name: String,
    pub gain: f32,
    pub mute: bool,
    pub solo: bool,
    pub fx: TrackFx,
    /// Song-mode clip start repetitions.
    pub clips: Vec<u32>,
    /// Track repeats in every repetition (ignores `clips`).
    pub clips_everywhere: bool,
    pub kind: crate::ui::app::TrackKind,
    pub drum_hits: Vec<crate::audio::drums::Hit>,
    pub len: usize,
    pub noise_len: usize,
}

pub struct ProjectTrack {
    pub meta: TrackMeta,
    pub audio: Vec<f32>,
    pub noise: Option<Vec<f32>>,
}

pub struct Project {
    pub meta: ProjectMeta,
    pub tracks: Vec<ProjectTrack>,
}

fn write_f32s(w: &mut impl Write, v: &[f32]) -> std::io::Result<()> {
    let mut bytes = Vec::with_capacity(v.len() * 4);
    for x in v {
        bytes.extend_from_slice(&x.to_le_bytes());
    }
    w.write_all(&bytes)
}

fn read_f32s(r: &mut impl Read, n: usize) -> std::io::Result<Vec<f32>> {
    let mut bytes = vec![0u8; n * 4];
    r.read_exact(&mut bytes)?;
    Ok(bytes.chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect())
}

pub fn save(path: &Path, project: &Project) -> anyhow::Result<()> {
    let mut meta = project.meta.clone();
    meta.tracks = project
        .tracks
        .iter()
        .map(|t| TrackMeta { len: t.audio.len(), noise_len: t.noise.as_ref().map_or(0, |n| n.len()), ..t.meta.clone() })
        .collect();
    let json = serde_json::to_vec(&meta)?;
    // write to a temp file first so a crash never leaves a half-written project
    let tmp = path.with_extension("synthstring.tmp");
    {
        let mut f = std::io::BufWriter::new(std::fs::File::create(&tmp)?);
        f.write_all(MAGIC)?;
        f.write_all(&VERSION.to_le_bytes())?;
        f.write_all(&(json.len() as u64).to_le_bytes())?;
        f.write_all(&json)?;
        for t in &project.tracks {
            write_f32s(&mut f, &t.audio)?;
            if let Some(n) = &t.noise {
                write_f32s(&mut f, n)?;
            }
        }
        f.flush()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

pub fn load(path: &Path) -> anyhow::Result<Project> {
    let mut f = std::io::BufReader::new(std::fs::File::open(path)?);
    let mut magic = [0u8; 8];
    f.read_exact(&mut magic)?;
    anyhow::ensure!(&magic == MAGIC, "Not a Synth String project");
    let mut b4 = [0u8; 4];
    f.read_exact(&mut b4)?;
    let version = u32::from_le_bytes(b4);
    anyhow::ensure!(version <= VERSION, "Project was saved by a newer version (format {version})");
    let mut b8 = [0u8; 8];
    f.read_exact(&mut b8)?;
    let mut json = vec![0u8; u64::from_le_bytes(b8) as usize];
    f.read_exact(&mut json)?;
    let meta: ProjectMeta = serde_json::from_slice(&json)?;
    let mut tracks = Vec::new();
    for tm in &meta.tracks {
        let audio = read_f32s(&mut f, tm.len)?;
        let noise = if tm.noise_len > 0 { Some(read_f32s(&mut f, tm.noise_len)?) } else { None };
        tracks.push(ProjectTrack { meta: tm.clone(), audio, noise });
    }
    Ok(Project { meta, tracks })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let path = std::env::temp_dir().join(format!("ss-proj-{}.synthstring", std::process::id()));
        let fx = TrackFx { octave: -1, echo: true, ..Default::default() };
        let p = Project {
            meta: ProjectMeta {
                loop_len: 3,
                bpm: 90.0,
                song_mode: true,
                song_reps: 12,
                beats_per_bar: 4,
                ..Default::default()
            },
            tracks: vec![
                ProjectTrack {
                    meta: TrackMeta {
                        id: 1,
                        name: "Track 1".into(),
                        gain: 0.8,
                        fx: fx.clone(),
                        clips: vec![0, 1, 2],
                        ..Default::default()
                    },
                    audio: vec![0.1, -0.2, 0.3],
                    noise: Some(vec![1.0, 2.0]),
                },
                ProjectTrack {
                    meta: TrackMeta {
                        id: 2,
                        name: "Solo".into(),
                        gain: 1.0,
                        solo: true,
                        clips: vec![1],
                        ..Default::default()
                    },
                    audio: vec![0.5; 6],
                    noise: None,
                },
            ],
        };
        save(&path, &p).unwrap();
        let q = load(&path).unwrap();
        std::fs::remove_file(&path).ok();
        assert_eq!(q.meta.song_reps, 12);
        assert_eq!(q.tracks.len(), 2);
        assert_eq!(q.tracks[0].audio, vec![0.1, -0.2, 0.3]);
        assert_eq!(q.tracks[0].noise, Some(vec![1.0, 2.0]));
        assert_eq!(q.tracks[0].meta.fx, fx);
        assert_eq!(q.tracks[1].audio.len(), 6);
        assert!(q.tracks[1].meta.solo);
        assert_eq!(q.tracks[1].meta.clips, vec![1]);
    }
}
