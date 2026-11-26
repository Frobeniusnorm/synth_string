//! Persistent user settings (JSON in ~/.config/synth-string/config.json).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use crate::midi::MidiMapping;

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum InputChannel {
    #[default]
    Mix,
    Left,
    Right,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// PipeWire node name of the preferred input (last explicit user choice).
    pub input_device: Option<String>,
    /// PipeWire node name of the preferred output.
    pub output_device: Option<String>,
    pub input_channel: InputChannel,
    /// `None` = automatic (based on monitoring of the output device).
    pub monitoring: Option<bool>,
    pub monitor_gate: bool,
    /// Extra latency compensation in milliseconds (added to the measured one).
    pub latency_offset_ms: f32,
    /// `None` = automatic threshold derived from the noise floor.
    pub threshold_db: Option<f32>,
    pub release_tail_ms: f32,
    pub preroll_ms: f32,
    pub bpm: f32,
    pub beats_per_bar: u32,
    pub click_volume: f32,
    /// `None` = automatic (enabled when the input is noisy).
    pub noise_reduction: Option<bool>,
    pub noise_strength: f32,
    pub overdub_multiplier: u32,
    pub export_repeats: u32,
    pub export_stems: bool,
    /// Drum snap as note value 1/N (0 = off).
    pub drum_snap_div: u8,
    pub midi: MidiMapping,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            input_device: None,
            output_device: None,
            input_channel: InputChannel::Mix,
            monitoring: None,
            monitor_gate: true,
            latency_offset_ms: 0.0,
            threshold_db: None,
            release_tail_ms: 150.0,
            preroll_ms: 30.0,
            bpm: 100.0,
            beats_per_bar: 4,
            click_volume: 0.6,
            noise_reduction: None,
            noise_strength: 2.0,
            overdub_multiplier: 1,
            export_repeats: 4,
            export_stems: false,
            drum_snap_div: 16,
            midi: MidiMapping::default(),
        }
    }
}

fn path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("synth-string").join("config.json"))
}

impl Config {
    pub fn load() -> Self {
        path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(p, s);
        }
    }
}
