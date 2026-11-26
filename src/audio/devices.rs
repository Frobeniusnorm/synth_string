//! Audio device discovery via PipeWire's PulseAudio compatibility layer (`pactl -f json`)
//! plus a heuristic that picks the most likely guitar interface.

use serde_json::Value;
use std::process::Command;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    /// PipeWire node name (stable across reconnects), used as `target.object`.
    pub name: String,
    pub description: String,
    pub bus: String,
    /// Card / device identifier shared by source & sink of the same hardware.
    pub card: String,
    pub form_factor: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceList {
    pub sources: Vec<Device>,
    pub sinks: Vec<Device>,
    pub default_source: Option<String>,
    pub default_sink: Option<String>,
}

fn prop<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get("properties").and_then(|p| p.get(key)).and_then(|s| s.as_str()).unwrap_or("")
}

fn parse(json: &str, want_monitor: bool) -> Vec<Device> {
    let Ok(Value::Array(items)) = serde_json::from_str::<Value>(json) else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|it| {
            let name = it.get("name")?.as_str()?.to_string();
            let class = prop(it, "device.class");
            let is_monitor = class == "monitor" || name.ends_with(".monitor");
            if is_monitor != want_monitor {
                return None;
            }
            let description = it.get("description").and_then(|d| d.as_str()).unwrap_or(&name).to_string();
            let card = match prop(it, "device.name") {
                "" => prop(it, "alsa.card").to_string(),
                s => s.to_string(),
            };
            Some(Device {
                name,
                description,
                bus: prop(it, "device.bus").to_string(),
                card,
                form_factor: prop(it, "device.form_factor").to_string(),
            })
        })
        .collect()
}

fn pactl(args: &[&str]) -> Option<String> {
    let out = Command::new("pactl").args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub fn enumerate() -> DeviceList {
    let sources = pactl(&["-f", "json", "list", "sources"]).map(|s| parse(&s, false)).unwrap_or_default();
    let sinks = pactl(&["-f", "json", "list", "sinks"]).map(|s| parse(&s, false)).unwrap_or_default();
    DeviceList {
        sources,
        sinks,
        default_source: pactl(&["get-default-source"]),
        default_sink: pactl(&["get-default-sink"]),
    }
}

const GUITAR_WORDS: &[&str] = &[
    "guitar",
    "pocket",
    "master",
    "amp",
    "pedal",
    "effect",
    "multi",
    "line 6",
    "helix",
    "pod",
    "boss",
    "zoom",
    "nux",
    "valeton",
    "sonicake",
    "mooer",
    "focusrite",
    "scarlett",
    "irig",
    "behringer",
    "umc",
    "audient",
    "presonus",
    "steinberg",
    "motu",
    "katana",
    "spark",
    "fender",
    "positive grid",
    "tonex",
    "interface",
];
const MIC_WORDS: &[&str] = &["microphone", "snowball", "yeti", "webcam", "camera", "headset", "mic "];

/// Higher is better. Instrument interfaces are USB devices that have both a capture and a
/// playback side on the same card.
pub fn score_source(dev: &Device, list: &DeviceList) -> i32 {
    let text = format!("{} {}", dev.description, dev.name).to_lowercase();
    let mut score = 0;
    if dev.bus == "usb" {
        score += 30;
    }
    if !dev.card.is_empty() && list.sinks.iter().any(|s| s.card == dev.card) {
        score += 40;
    }
    if GUITAR_WORDS.iter().any(|w| text.contains(w)) {
        score += 25;
    }
    if MIC_WORDS.iter().any(|w| text.contains(w))
        || matches!(dev.form_factor.as_str(), "microphone" | "webcam" | "headset")
    {
        score -= 60;
    }
    if list.default_source.as_deref() == Some(dev.name.as_str()) {
        score += 5;
    }
    score
}

impl DeviceList {
    pub fn find_source(&self, name: &str) -> Option<&Device> {
        self.sources.iter().find(|d| d.name == name)
    }
    pub fn find_sink(&self, name: &str) -> Option<&Device> {
        self.sinks.iter().find(|d| d.name == name)
    }

    /// Best guess for the guitar input. A remembered choice wins if it is present.
    pub fn pick_input(&self, preferred: Option<&str>) -> Option<Device> {
        if let Some(p) = preferred.and_then(|p| self.find_source(p)) {
            return Some(p.clone());
        }
        self.sources.iter().max_by_key(|d| score_source(d, self)).cloned()
    }

    /// Output: remembered choice, else the system default sink (where the user listens).
    pub fn pick_output(&self, preferred: Option<&str>) -> Option<Device> {
        if let Some(p) = preferred.and_then(|p| self.find_sink(p)) {
            return Some(p.clone());
        }
        self.default_sink.as_deref().and_then(|d| self.find_sink(d)).or_else(|| self.sinks.first()).cloned()
    }

    /// True when the output plays through the same hardware as the input (e.g. the pedal's
    /// headphone jack) – such devices monitor the guitar in hardware already.
    pub fn same_hardware(&self, input: &str, output: &str) -> bool {
        match (self.find_source(input), self.find_sink(output)) {
            (Some(i), Some(o)) => !i.card.is_empty() && i.card == o.card,
            _ => false,
        }
    }
}

/// Short human name for a device ("Pocket Master" instead of "Pocket Master Digitales Stereo (IEC958)").
pub fn short_name(desc: &str) -> String {
    let cut = [" Analog", " Digital", " Analoges", " Digitales", " Stereo", " Mono", " Pro", " Multichannel"];
    let mut s = desc;
    for c in cut {
        if let Some(i) = s.find(c) {
            s = &s[..i];
        }
    }
    s.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dev(name: &str, desc: &str, bus: &str, card: &str, ff: &str) -> Device {
        Device {
            name: name.into(),
            description: desc.into(),
            bus: bus.into(),
            card: card.into(),
            form_factor: ff.into(),
        }
    }

    #[test]
    fn prefers_interface_over_usb_mic() {
        let list = DeviceList {
            sources: vec![
                dev("snowball", "Blue Snowball Analog Stereo", "usb", "c0", "microphone"),
                dev("internal", "Ryzen HD Audio Analog Stereo", "pci", "c2", ""),
                dev("pm", "Pocket Master Digital Stereo (IEC958)", "usb", "c4", ""),
            ],
            sinks: vec![
                dev("internal_out", "Ryzen HD Audio", "pci", "c2", ""),
                dev("pm_out", "Pocket Master Analog Stereo", "usb", "c4", ""),
            ],
            default_source: Some("internal".into()),
            default_sink: Some("internal_out".into()),
        };
        assert_eq!(list.pick_input(None).unwrap().name, "pm");
        assert_eq!(list.pick_input(Some("snowball")).unwrap().name, "snowball");
        assert_eq!(list.pick_input(Some("gone")).unwrap().name, "pm");
        assert_eq!(list.pick_output(None).unwrap().name, "internal_out");
        assert!(list.same_hardware("pm", "pm_out"));
        assert!(!list.same_hardware("pm", "internal_out"));
        assert_eq!(short_name("Pocket Master Digitales Stereo (IEC958)"), "Pocket Master");
    }
}
