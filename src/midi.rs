//! MIDI footswitch control. Auto-connects to every hardware MIDI input (hot-plug aware) and
//! forwards messages to the UI thread, which handles "MIDI learn" and dispatch.

use crossbeam_channel::{Receiver, Sender};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MidiKind {
    Note,
    Cc,
    Program,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MidiTrigger {
    pub kind: MidiKind,
    pub channel: u8,
    pub number: u8,
}

impl MidiTrigger {
    pub fn label(&self) -> String {
        let k = match self.kind {
            MidiKind::Note => "Note",
            MidiKind::Cc => "CC",
            MidiKind::Program => "Program",
        };
        format!("{k} {} · ch {}", self.number, self.channel + 1)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MidiAction {
    Record,
    PlayPause,
    Undo,
    Metronome,
}

impl MidiAction {
    pub const ALL: [MidiAction; 4] =
        [MidiAction::Record, MidiAction::PlayPause, MidiAction::Undo, MidiAction::Metronome];
    pub fn label(&self) -> &'static str {
        match self {
            MidiAction::Record => "Record / Overdub",
            MidiAction::PlayPause => "Play / Pause",
            MidiAction::Undo => "Undo last track",
            MidiAction::Metronome => "Metronome on/off",
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct MidiMapping {
    pub entries: Vec<(MidiAction, MidiTrigger)>,
}

impl MidiMapping {
    pub fn get(&self, a: MidiAction) -> Option<MidiTrigger> {
        self.entries.iter().find(|(x, _)| *x == a).map(|(_, t)| *t)
    }
    pub fn set(&mut self, a: MidiAction, t: Option<MidiTrigger>) {
        self.entries.retain(|(x, tt)| *x != a && Some(*tt) != t);
        if let Some(t) = t {
            self.entries.push((a, t));
        }
    }
    pub fn action_for(&self, t: MidiTrigger) -> Option<MidiAction> {
        self.entries.iter().find(|(_, x)| *x == t).map(|(a, _)| *a)
    }
}

/// Parse a raw MIDI message into a trigger, ignoring note-offs / zero-velocity notes.
pub fn parse(msg: &[u8]) -> Option<MidiTrigger> {
    let status = *msg.first()?;
    let channel = status & 0x0f;
    match status & 0xf0 {
        0x90 if msg.len() >= 3 && msg[2] > 0 => Some(MidiTrigger { kind: MidiKind::Note, channel, number: msg[1] }),
        0xb0 if msg.len() >= 3 => Some(MidiTrigger { kind: MidiKind::Cc, channel, number: msg[1] }),
        0xc0 if msg.len() >= 2 => Some(MidiTrigger { kind: MidiKind::Program, channel, number: msg[1] }),
        _ => None,
    }
}

pub struct Midi {
    pub rx: Receiver<MidiTrigger>,
    pub ports: Arc<Mutex<Vec<String>>>,
}

/// Start the background MIDI watcher.
pub fn start() -> Midi {
    let (tx, rx) = crossbeam_channel::bounded::<MidiTrigger>(64);
    let ports = Arc::new(Mutex::new(Vec::new()));
    let ports2 = ports.clone();
    std::thread::Builder::new().name("midi".into()).spawn(move || watch(tx, ports2)).ok();
    Midi { rx, ports }
}

fn watch(tx: Sender<MidiTrigger>, ports_out: Arc<Mutex<Vec<String>>>) {
    let mut conns: Vec<(String, midir::MidiInputConnection<()>)> = Vec::new();
    loop {
        if let Ok(probe) = midir::MidiInput::new("Synth String probe") {
            let available: Vec<(String, midir::MidiInputPort)> = probe
                .ports()
                .into_iter()
                .filter_map(|p| probe.port_name(&p).ok().map(|n| (n, p)))
                .filter(|(n, _)| !n.contains("Midi Through") && !n.contains("Synth String"))
                .collect();
            let names: HashSet<&String> = available.iter().map(|(n, _)| n).collect();
            // drop vanished
            conns.retain(|(n, _)| names.contains(n));
            for (name, port) in &available {
                if conns.iter().any(|(n, _)| n == name) {
                    continue;
                }
                let Ok(mut input) = midir::MidiInput::new("Synth String") else { continue };
                input.ignore(midir::Ignore::All);
                let tx = tx.clone();
                let mut last: Option<(MidiTrigger, std::time::Instant)> = None;
                if let Ok(c) = input.connect(
                    port,
                    "Synth String in",
                    move |_, msg, _| {
                        if let Some(t) = parse(msg) {
                            // debounce footswitches that send press+release as two CCs
                            let now = std::time::Instant::now();
                            if let Some((lt, at)) = last
                                && lt == t
                                && t.kind == MidiKind::Cc
                                && now.duration_since(at) < Duration::from_millis(350)
                            {
                                return;
                            }
                            last = Some((t, now));
                            let _ = tx.try_send(t);
                        }
                    },
                    (),
                ) {
                    conns.push((name.clone(), c));
                }
            }
            *ports_out.lock().unwrap() = conns.iter().map(|(n, _)| n.clone()).collect();
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_messages() {
        assert_eq!(parse(&[0xb0, 80, 127]), Some(MidiTrigger { kind: MidiKind::Cc, channel: 0, number: 80 }));
        assert_eq!(parse(&[0xc3, 5]), Some(MidiTrigger { kind: MidiKind::Program, channel: 3, number: 5 }));
        assert_eq!(parse(&[0x90, 60, 0]), None);
        assert_eq!(parse(&[0x80, 60, 10]), None);
        let mut m = MidiMapping::default();
        let t = parse(&[0xb0, 80, 127]).unwrap();
        m.set(MidiAction::Record, Some(t));
        m.set(MidiAction::Undo, Some(t)); // stealing removes it from Record
        assert_eq!(m.get(MidiAction::Record), None);
        assert_eq!(m.action_for(t), Some(MidiAction::Undo));
    }
}
