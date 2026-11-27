//! Application controller: owns the engine, the track models and all UI state.

use super::track_row::{self, TrackRow};
use super::widgets::{MeterState, RingState, TRACK_COLORS, compute_peaks, db_to_frac};
use crate::audio::denoise::{self, NoiseProfile};
use crate::audio::devices::{self, Device, DeviceList};
use crate::audio::dsp::{db_to_lin, lin_to_db};
use crate::audio::effects::{self, TrackFx};
use crate::audio::engine::EngineHandle;
use crate::audio::looper::{Command, Event, LoopInfo, MetronomeSettings, RecState, SR, Status};
use crate::config::Config;
use crate::export::{self, ExportTrack};
use crate::midi::{self, Midi, MidiAction, MidiTrigger};
use adw::prelude::*;
use gtk::{gio, glib};
use std::cell::{Cell, OnceCell, RefCell};
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum TrackKind {
    /// Played live on the guitar.
    #[default]
    Recorded,
    /// Imported from an audio file.
    Imported,
    /// Drum machine pattern.
    Drums,
}

pub struct TrackModel {
    pub id: u32,
    pub kind: TrackKind,
    /// Drum pattern (drum tracks only); the audio is rendered from it.
    pub drum_hits: Vec<crate::audio::drums::Hit>,
    pub name: String,
    pub raw: Arc<Vec<f32>>,
    pub noise: Option<NoiseProfile>,
    pub fx: TrackFx,
    pub gain: f32,
    pub mute: bool,
    pub solo: bool,
    pub processed: Arc<Vec<f32>>,
    pub render_gen: u64,
    /// Length in loop repetitions.
    pub cycles: u32,
    /// Song-mode placement (clip start repetitions); `None` = everywhere.
    pub clips: Option<Vec<u32>>,
    pub row: TrackRow,
}

pub struct Ui {
    pub window: adw::ApplicationWindow,
    pub toasts: adw::ToastOverlay,
    pub title: adw::WindowTitle,
    pub device_label: gtk::Label,
    pub device_button: gtk::MenuButton,
    pub record_btn: gtk::Button,
    pub record_icon: gtk::Image,
    pub play_btn: gtk::Button,
    pub status_title: gtk::Label,
    pub status_sub: gtk::Label,
    pub meter: gtk::DrawingArea,
    pub meter_state: Rc<MeterState>,
    pub threshold_label: gtk::Label,
    pub ring: gtk::DrawingArea,
    pub ring_state: Rc<RingState>,
    pub beat_dots: gtk::DrawingArea,
    pub metro_toggle: gtk::ToggleButton,
    pub bpm_spin: gtk::SpinButton,
    pub mult_dd: gtk::DropDown,
    pub list: gtk::ListBox,
    pub stack: gtk::Stack,
    pub pending: gtk::Box,
    pub pending_label: gtk::Label,
    pub pending_bar: gtk::ProgressBar,
    pub input_combo: adw::ComboRow,
    pub output_combo: adw::ComboRow,
    pub monitor_switch: adw::SwitchRow,
    pub nr_switch: adw::SwitchRow,
    pub popover_meter: gtk::DrawingArea,
    pub undo_btn: gtk::Button,
    pub export_btn: gtk::Button,
    pub timeline_card: gtk::Box,
    pub timeline: gtk::DrawingArea,
    pub timeline_hint: gtk::Label,
    pub song_switch: gtk::Switch,
    pub song_reps_spin: gtk::SpinButton,
    pub song_reps_box: gtk::Box,
    pub song_len_label: gtk::Label,
    pub drums_toggle: gtk::ToggleButton,
    pub drum_pads: Vec<gtk::Button>,
    pub drum_record: gtk::Button,
    pub drum_status: gtk::Label,
    pub drum_bars: gtk::DropDown,
    pub drum_bars_box: gtk::Box,
    pub drum_grid: gtk::DrawingArea,
}

/// MIDI learn in progress: the action being mapped and a callback once it is learned.
pub type MidiLearn = (MidiAction, Box<dyn Fn()>);

pub struct App {
    pub ui: OnceCell<Ui>,
    pub engine: RefCell<EngineHandle>,
    pub cfg: RefCell<Config>,
    pub devices: RefCell<DeviceList>,
    pub input: RefCell<Option<Device>>,
    pub output: RefCell<Option<Device>>,
    pub tracks: RefCell<Vec<TrackModel>>,
    next_id: Cell<u32>,
    pub status: RefCell<Status>,
    pub loop_info: Cell<Option<LoopInfo>>,
    floor_hist: RefCell<VecDeque<f32>>,
    pub noise_floor_db: Cell<f32>,
    pub threshold_db: Cell<f32>,
    auto_noisy: Cell<bool>,
    pub nr_active: Cell<bool>,
    pub noise_profile: RefCell<Option<NoiseProfile>>,
    pub midi: Midi,
    pub learning: RefCell<Option<MidiLearn>>,
    midi_hinted: Cell<bool>,
    tap_times: RefCell<Vec<Instant>>,
    pub syncing: Cell<bool>,
    t0: Instant,
    device_poll_busy: Cell<bool>,
    last_error: RefCell<Option<String>>,
    peak_hold: Cell<(f32, f64)>,
    startup_noise_pending: Cell<bool>,
    ticks: Cell<u64>,
    pub song_mode: Cell<bool>,
    /// Wide window (breakpoint): bigger timeline, hero and drum machine side by side.
    pub wide: Cell<bool>,
    pub song_reps: Cell<u32>,
    pub project_path: RefCell<Option<std::path::PathBuf>>,
    pub dirty: Cell<bool>,
    export_repeats: Cell<u32>,
    /// Name and kind for tracks injected by import / drum machine, keyed by id.
    pub pending_meta: RefCell<std::collections::HashMap<u32, (String, TrackKind)>>,
    pub drum_rec: RefCell<Option<super::extras::DrumRec>>,
    pub kit: Arc<crate::audio::drums::Kit>,
}

/// Lengths offered for new layers, in loop repetitions.
pub const MULTIPLIERS: [u32; 6] = [1, 2, 3, 4, 6, 8];

pub fn fmt_secs(samples: usize) -> String {
    let s = samples as f64 / SR as f64;
    if s >= 60.0 { format!("{}:{:04.1}", (s / 60.0) as u32, s % 60.0) } else { format!("{s:.1} s") }
}

impl App {
    pub fn new() -> Rc<Self> {
        let cfg = Config::load();
        let engine = EngineHandle::new();
        engine.set_channel(cfg.input_channel);
        Rc::new(Self {
            ui: OnceCell::new(),
            engine: RefCell::new(engine),
            cfg: RefCell::new(cfg),
            devices: RefCell::new(DeviceList::default()),
            input: RefCell::new(None),
            output: RefCell::new(None),
            tracks: RefCell::new(Vec::new()),
            next_id: Cell::new(1),
            status: RefCell::new(Status::default()),
            loop_info: Cell::new(None),
            floor_hist: RefCell::new(VecDeque::with_capacity(300)),
            noise_floor_db: Cell::new(-90.0),
            threshold_db: Cell::new(-45.0),
            auto_noisy: Cell::new(false),
            nr_active: Cell::new(false),
            noise_profile: RefCell::new(None),
            midi: midi::start(),
            learning: RefCell::new(None),
            midi_hinted: Cell::new(false),
            tap_times: RefCell::new(Vec::new()),
            syncing: Cell::new(false),
            t0: Instant::now(),
            device_poll_busy: Cell::new(false),
            last_error: RefCell::new(None),
            peak_hold: Cell::new((-90.0, 0.0)),
            startup_noise_pending: Cell::new(true),
            ticks: Cell::new(0),
            song_mode: Cell::new(false),
            wide: Cell::new(false),
            song_reps: Cell::new(0),
            project_path: RefCell::new(None),
            dirty: Cell::new(false),
            export_repeats: Cell::new(1),
            pending_meta: RefCell::new(Default::default()),
            drum_rec: RefCell::new(None),
            kit: crate::audio::drums::kit(),
        })
    }

    pub fn ui(&self) -> &Ui {
        self.ui.get().expect("ui built")
    }

    pub fn send(&self, c: Command) {
        self.engine.borrow().send(c);
    }

    pub fn toast(&self, text: &str) {
        let t = adw::Toast::builder().title(text).timeout(3).build();
        self.ui().toasts.add_toast(t);
    }

    pub fn save_cfg(&self) {
        self.cfg.borrow().save();
    }

    // ------------------------------------------------------------ startup / devices

    pub fn start(self: &Rc<Self>) {
        let list = devices::enumerate();
        *self.devices.borrow_mut() = list;
        self.refresh_device_widgets();
        self.choose_devices(true);
        self.push_settings();

        // ~30 fps UI refresh
        let app = self.clone();
        glib::timeout_add_local(Duration::from_millis(33), move || {
            app.tick();
            glib::ControlFlow::Continue
        });
        // hot-plug polling
        let app = self.clone();
        glib::timeout_add_local(Duration::from_millis(2000), move || {
            app.poll_devices();
            glib::ControlFlow::Continue
        });
        // learn a first noise profile once the input has settled
        let app = self.clone();
        glib::timeout_add_local_once(Duration::from_millis(1800), move || {
            app.send(Command::CaptureNoise(Vec::with_capacity(SR as usize)));
        });
    }

    /// Pick input/output (remembered choice or smart auto-detection) and (re)start audio if needed.
    pub fn choose_devices(&self, initial: bool) {
        let (inp, out) = {
            let list = self.devices.borrow();
            let cfg = self.cfg.borrow();
            (list.pick_input(cfg.input_device.as_deref()), list.pick_output(cfg.output_device.as_deref()))
        };
        let changed_in = self.input.borrow().as_ref().map(|d| &d.name) != inp.as_ref().map(|d| &d.name);
        let changed_out = self.output.borrow().as_ref().map(|d| &d.name) != out.as_ref().map(|d| &d.name);
        if !changed_in && !changed_out && !initial {
            return;
        }
        let old_in = self.input.borrow().clone();
        *self.input.borrow_mut() = inp.clone();
        *self.output.borrow_mut() = out.clone();
        self.engine.borrow_mut().start(inp.as_ref().map(|d| d.name.as_str()), out.as_ref().map(|d| d.name.as_str()));
        self.apply_monitoring();
        // reset noise-floor statistics for the new device
        self.floor_hist.borrow_mut().clear();
        self.update_device_label();

        let in_name = inp.as_ref().map(|d| devices::short_name(&d.description));
        let out_name = out.as_ref().map(|d| devices::short_name(&d.description));
        if initial {
            match (&in_name, &out_name) {
                (Some(i), Some(o)) => self.toast(&format!("🎸 {i} → 🔊 {o}")),
                (None, _) => self.toast("No audio input found – plug in your guitar interface"),
                _ => {}
            }
        } else if changed_in {
            match (&old_in, &in_name) {
                (Some(o), Some(n)) if self.devices.borrow().find_source(&o.name).is_none() => {
                    self.toast(&format!("{} disconnected – now using {n}", devices::short_name(&o.description)))
                }
                (_, Some(n)) => self.toast(&format!("🎸 Input: {n}")),
                (_, None) => self.toast("Input disconnected"),
            }
        } else if changed_out && let Some(o) = out_name {
            self.toast(&format!("🔊 Output: {o}"));
        }
    }

    pub fn apply_monitoring(&self) {
        let cfg = self.cfg.borrow();
        let auto = match (&*self.input.borrow(), &*self.output.borrow()) {
            (Some(i), Some(o)) => !self.devices.borrow().same_hardware(&i.name, &o.name),
            _ => true,
        };
        let enabled = cfg.monitoring.unwrap_or(auto);
        self.send(Command::SetMonitoring { enabled, gate: cfg.monitor_gate });
        if let Some(ui) = self.ui.get() {
            self.syncing.set(true);
            ui.monitor_switch.set_active(enabled);
            ui.monitor_switch.set_subtitle(if cfg.monitoring.is_none() {
                if auto {
                    "Automatic: on (output is not your interface)"
                } else {
                    "Automatic: off (your interface monitors directly)"
                }
            } else {
                "Hear your guitar through the selected output"
            });
            self.syncing.set(false);
        }
    }

    pub fn push_settings(&self) {
        let cfg = self.cfg.borrow();
        self.engine.borrow().set_channel(cfg.input_channel);
        self.send(Command::SetLatencyOffset((cfg.latency_offset_ms / 1000.0 * SR as f32) as i32));
        self.send(Command::SetTiming {
            preroll: (cfg.preroll_ms / 1000.0 * SR as f32) as usize,
            tail: (cfg.release_tail_ms / 1000.0 * SR as f32) as usize,
        });
        drop(cfg);
        self.push_metronome();
        self.update_threshold(true);
    }

    pub fn push_metronome(&self) {
        let cfg = self.cfg.borrow();
        let enabled = self.ui.get().map(|u| u.metro_toggle.is_active()).unwrap_or(false);
        self.send(Command::SetMetronome(MetronomeSettings {
            enabled,
            bpm: cfg.bpm,
            beats_per_bar: cfg.beats_per_bar,
            volume: cfg.click_volume,
        }));
    }

    pub fn poll_devices(self: &Rc<Self>) {
        if self.device_poll_busy.get() {
            return;
        }
        self.device_poll_busy.set(true);
        let app = self.clone();
        glib::spawn_future_local(async move {
            if let Ok(list) = gio::spawn_blocking(devices::enumerate).await {
                let changed = *app.devices.borrow() != list;
                if changed {
                    *app.devices.borrow_mut() = list;
                    app.refresh_device_widgets();
                    app.choose_devices(false);
                }
            }
            app.device_poll_busy.set(false);
        });
    }

    pub fn source_entries(&self) -> Vec<(Option<String>, String)> {
        let list = self.devices.borrow();
        let auto = list.pick_input(None).map(|d| devices::short_name(&d.description)).unwrap_or_else(|| "none".into());
        let mut v = vec![(None, format!("Automatic ({auto})"))];
        v.extend(list.sources.iter().map(|d| (Some(d.name.clone()), d.description.clone())));
        v
    }

    pub fn sink_entries(&self) -> Vec<(Option<String>, String)> {
        let list = self.devices.borrow();
        let auto = list.pick_output(None).map(|d| devices::short_name(&d.description)).unwrap_or_else(|| "none".into());
        let mut v = vec![(None, format!("System default ({auto})"))];
        v.extend(list.sinks.iter().map(|d| (Some(d.name.clone()), d.description.clone())));
        v
    }

    pub fn refresh_device_widgets(&self) {
        let Some(ui) = self.ui.get() else { return };
        self.syncing.set(true);
        let cfg = self.cfg.borrow();
        for (combo, entries, cur) in [
            (&ui.input_combo, self.source_entries(), cfg.input_device.clone()),
            (&ui.output_combo, self.sink_entries(), cfg.output_device.clone()),
        ] {
            let names: Vec<&str> = entries.iter().map(|(_, l)| l.as_str()).collect();
            // only replace the model when the device list really changed: swapping the model of a
            // combo row (especially from inside its own selection handler) wedges GTK
            let same = combo.model().and_downcast::<gtk::StringList>().is_some_and(|m| {
                m.n_items() as usize == names.len()
                    && names.iter().enumerate().all(|(i, n)| m.string(i as u32).is_some_and(|s| s == *n))
            });
            if !same {
                combo.set_model(Some(&gtk::StringList::new(&names)));
            }
            let idx = entries.iter().position(|(n, _)| *n == cur).unwrap_or(0) as u32;
            if combo.selected() != idx {
                combo.set_selected(idx);
            }
        }
        self.syncing.set(false);
    }

    /// Called from combo-row selection handlers: the actual switch runs once the handler returned,
    /// so widgets are never rebuilt (and audio never restarted) inside GTK's signal emission.
    pub fn select_input(self: &Rc<Self>, name: Option<String>) {
        self.select_device(name, true);
    }

    pub fn select_output(self: &Rc<Self>, name: Option<String>) {
        self.select_device(name, false);
    }

    fn select_device(self: &Rc<Self>, name: Option<String>, input: bool) {
        let app = self.clone();
        glib::idle_add_local_once(move || {
            {
                let mut cfg = app.cfg.borrow_mut();
                let slot = if input { &mut cfg.input_device } else { &mut cfg.output_device };
                if *slot == name {
                    return;
                }
                *slot = name;
            }
            app.save_cfg();
            app.refresh_device_widgets();
            app.choose_devices(false);
        });
    }

    fn update_device_label(&self) {
        let Some(ui) = self.ui.get() else { return };
        let inp = self.input.borrow();
        match &*inp {
            Some(d) => {
                ui.device_label.set_label(&devices::short_name(&d.description));
                ui.device_button.remove_css_class("missing");
                ui.device_button.add_css_class("connected");
            }
            None => {
                ui.device_label.set_label("No input");
                ui.device_button.remove_css_class("connected");
                ui.device_button.add_css_class("missing");
            }
        }
        let out = self.output.borrow();
        ui.device_button.set_tooltip_text(Some(&format!(
            "Input: {}\nOutput: {}",
            inp.as_ref().map(|d| d.description.as_str()).unwrap_or("none"),
            out.as_ref().map(|d| d.description.as_str()).unwrap_or("none")
        )));
    }

    // ------------------------------------------------------------ threshold / noise

    pub fn set_manual_threshold(&self, db: Option<f32>) {
        self.cfg.borrow_mut().threshold_db = db.map(|d| d.clamp(-60.0, -6.0));
        self.save_cfg();
        self.update_threshold(true);
    }

    fn update_threshold(&self, force: bool) {
        let cfg = self.cfg.borrow();
        let auto = (self.noise_floor_db.get() + 14.0).clamp(-54.0, -24.0);
        let t = cfg.threshold_db.unwrap_or(auto);
        let rec_busy = !matches!(self.status.borrow().rec, RecState::Idle);
        if force || (!rec_busy && (t - self.threshold_db.get()).abs() > 0.5) {
            self.threshold_db.set(t);
            self.send(Command::SetThreshold(db_to_lin(t)));
        }
        if let Some(ui) = self.ui.get() {
            ui.meter_state.threshold_db.set(self.threshold_db.get());
            ui.meter_state.auto_threshold.set(cfg.threshold_db.is_none());
            ui.threshold_label.set_label(&format!(
                "Starts recording above {:.0} dB{}",
                self.threshold_db.get(),
                if cfg.threshold_db.is_none() { " (auto)" } else { "" }
            ));
        }
    }

    fn update_noise_state(self: &Rc<Self>) {
        let want = {
            let cfg = self.cfg.borrow();
            cfg.noise_reduction.unwrap_or(self.auto_noisy.get())
        };
        if want != self.nr_active.get() {
            self.nr_active.set(want);
            let ids: Vec<u32> = self.tracks.borrow().iter().map(|t| t.id).collect();
            for id in ids {
                self.reprocess(id);
            }
        }
        if let Some(ui) = self.ui.get() {
            self.syncing.set(true);
            ui.nr_switch.set_active(want);
            ui.nr_switch.set_subtitle(&format!(
                "Noise floor {:.0} dB{}",
                self.noise_floor_db.get(),
                if self.cfg.borrow().noise_reduction.is_none() { " · automatic" } else { "" }
            ));
            self.syncing.set(false);
        }
    }

    pub fn set_noise_reduction(self: &Rc<Self>, on: Option<bool>) {
        self.cfg.borrow_mut().noise_reduction = on;
        self.save_cfg();
        self.update_noise_state();
    }

    pub fn relearn_noise(&self) {
        self.send(Command::CaptureNoise(Vec::with_capacity(SR as usize)));
    }

    // ------------------------------------------------------------ transport

    pub fn record(self: &Rc<Self>) {
        if self.drum_rec.borrow().is_some() {
            self.finish_drums();
            return;
        }
        let st = self.status.borrow().rec;
        let has_loop = self.status.borrow().loop_len > 0;
        if self.input.borrow().is_none() {
            self.toast("No input device – plug in your guitar interface first");
            return;
        }
        match st {
            RecState::Idle if !has_loop => {
                let id = self.alloc_id();
                self.send(Command::ArmFirst {
                    track_id: id,
                    buffer: Vec::with_capacity(SR as usize * 60 * 5),
                    noise: Vec::with_capacity(SR as usize),
                });
            }
            RecState::Idle => self.arm_overdub(None),
            RecState::ArmedFirst | RecState::ArmedOverdub => self.send(Command::CancelRecording),
            RecState::RecordingFirst | RecState::RecordingOverdub => self.send(Command::StopRecording),
        }
        // optimistic UI so the button reacts instantly
        self.status.borrow_mut().rec = match st {
            RecState::Idle if !has_loop => RecState::ArmedFirst,
            RecState::Idle => RecState::ArmedOverdub,
            _ => RecState::Idle,
        };
        self.refresh_transport();
    }

    pub fn set_next_id(&self, id: u32) {
        self.next_id.set(id);
    }

    pub fn alloc_track_id(&self) -> u32 {
        self.alloc_id()
    }

    fn alloc_id(&self) -> u32 {
        let id = self.next_id.get();
        self.next_id.set(id + 1);
        id
    }

    fn arm_overdub(&self, replace: Option<u32>) {
        let loop_len = self.status.borrow().loop_len;
        if loop_len == 0 {
            return;
        }
        let (id, len) = match replace {
            Some(id) => {
                let len = self.tracks.borrow().iter().find(|t| t.id == id).map(|t| t.raw.len()).unwrap_or(loop_len);
                (id, len)
            }
            None => {
                let mult = MULTIPLIERS[(self.ui().mult_dd.selected() as usize).min(MULTIPLIERS.len() - 1)] as usize;
                (self.alloc_id(), loop_len * mult)
            }
        };
        self.send(Command::ArmOverdub {
            track_id: id,
            buffer: vec![0.0; len],
            noise: Vec::with_capacity(SR as usize),
            replace: replace.is_some(),
        });
    }

    pub fn rerecord(self: &Rc<Self>, id: u32) {
        self.arm_overdub(Some(id));
        self.status.borrow_mut().rec = RecState::ArmedOverdub;
        self.status.borrow_mut().rec_track = Some(id);
        self.refresh_transport();
    }

    pub fn play_pause(&self) {
        let st = self.status.borrow().clone();
        if st.loop_len == 0 {
            return;
        }
        if st.playing && matches!(st.rec, RecState::ArmedOverdub | RecState::RecordingOverdub) {
            self.send(Command::CancelRecording);
        }
        self.send(Command::SetPlaying(!st.playing));
        self.status.borrow_mut().playing = !st.playing;
        self.refresh_transport();
    }

    pub fn undo(self: &Rc<Self>) {
        if self.undo_drum_hit() {
            return;
        }
        let st = self.status.borrow().rec;
        if st != RecState::Idle {
            self.send(Command::CancelRecording);
            return;
        }
        let last = self.tracks.borrow().iter().map(|t| t.id).max();
        if let Some(id) = last {
            self.delete_track(id);
        }
    }

    pub fn clear_all(&self) {
        self.drum_rec.borrow_mut().take();
        self.refresh_drum_panel();
        self.send(Command::ClearAll);
        let rows: Vec<gtk::ListBoxRow> = self.tracks.borrow_mut().drain(..).map(|t| t.row.row).collect();
        for r in rows {
            self.ui().list.remove(&r);
        }
        self.loop_info.set(None);
        self.status.borrow_mut().loop_len = 0;
        self.next_id.set(1);
        self.mark_dirty();
        self.refresh_list_visibility();
        self.refresh_song_widgets();
        self.refresh_transport();
    }

    pub fn delete_track(&self, id: u32) {
        if self.drum_rec.borrow().as_ref().is_some_and(|r| r.track_id == id) {
            self.drum_rec.borrow_mut().take();
            self.refresh_drum_panel();
        }
        self.send(Command::RemoveTrack(id));
        let mut tracks = self.tracks.borrow_mut();
        if let Some(i) = tracks.iter().position(|t| t.id == id) {
            let t = tracks.remove(i);
            self.ui().list.remove(&t.row.row);
        }
        if tracks.is_empty() {
            self.loop_info.set(None);
            self.status.borrow_mut().loop_len = 0;
            self.next_id.set(1);
        }
        drop(tracks);
        self.mark_dirty();
        self.refresh_list_visibility();
        self.refresh_song_widgets();
        self.refresh_transport();
    }

    pub fn set_mute(&self, id: u32, v: bool) {
        self.with_track(id, |t| t.mute = v);
    }
    pub fn set_solo(&self, id: u32, v: bool) {
        self.with_track(id, |t| t.solo = v);
    }
    pub fn set_gain(&self, id: u32, v: f32) {
        self.with_track(id, |t| t.gain = v);
    }

    fn with_track(&self, id: u32, f: impl FnOnce(&mut TrackModel)) {
        let mut tracks = self.tracks.borrow_mut();
        if let Some(t) = tracks.iter_mut().find(|t| t.id == id) {
            f(t);
            self.send(Command::SetTrack { id, gain: t.gain, mute: t.mute, solo: t.solo });
        }
        drop(tracks);
        self.mark_dirty();
        self.refresh_dim();
    }

    pub fn refresh_dim(&self) {
        let tracks = self.tracks.borrow();
        let any_solo = tracks.iter().any(|t| t.solo);
        for t in tracks.iter() {
            t.row.wave.dim.set(t.mute || (any_solo && !t.solo));
            t.row.area.queue_draw();
        }
        if let Some(ui) = self.ui.get() {
            ui.timeline.queue_draw();
        }
    }

    pub fn toggle_metronome(&self) {
        let ui = self.ui();
        ui.metro_toggle.set_active(!ui.metro_toggle.is_active());
    }

    pub fn tap_tempo(&self) {
        let now = Instant::now();
        let mut taps = self.tap_times.borrow_mut();
        taps.retain(|t| now.duration_since(*t) < Duration::from_secs(3));
        taps.push(now);
        if taps.len() >= 2 {
            let n = taps.len() - 1;
            let span = now.duration_since(taps[0]).as_secs_f32();
            let bpm = (60.0 * n as f32 / span).clamp(40.0, 240.0);
            drop(taps);
            self.ui().bpm_spin.set_value(bpm.round() as f64);
        }
    }

    // ------------------------------------------------------------ tracks / effects

    fn add_track_model(self: &Rc<Self>, id: u32, raw: Arc<Vec<f32>>, noise: Vec<f32>, clips: Option<Vec<u32>>) {
        let loop_len = self.loop_info.get().map(|i| i.len).unwrap_or(raw.len()).max(1);
        let cycles = (raw.len() / loop_len).max(1) as u32;
        let (name, kind) =
            self.pending_meta.borrow_mut().remove(&id).unwrap_or((format!("Track {id}"), TrackKind::Recorded));
        let fx = TrackFx::default();
        let color = (id as usize - 1) % TRACK_COLORS.len();
        let row = track_row::build(self, id, &name, color, &fx, cycles, kind);
        *row.wave.peaks.borrow_mut() = compute_peaks(&raw, 1200);
        self.ui().list.append(&row.row);
        let noise = NoiseProfile::learn(&noise);
        if let Some(n) = &noise {
            // a fresh measurement of the room – remember globally too
            *self.noise_profile.borrow_mut() = Some(n.clone());
        }
        self.tracks.borrow_mut().push(TrackModel {
            id,
            kind,
            drum_hits: Vec::new(),
            name,
            raw: raw.clone(),
            noise,
            fx,
            gain: 1.0,
            mute: false,
            solo: false,
            processed: raw,
            render_gen: 0,
            cycles,
            clips,
            row,
        });
        self.mark_dirty();
        self.refresh_list_visibility();
        self.push_arrangement();
        if kind != TrackKind::Recorded {
            if kind == TrackKind::Imported {
                self.toast(&format!(
                    "Imported “{}”",
                    self.tracks.borrow().last().map(|t| t.name.clone()).unwrap_or_default()
                ));
            }
            self.refresh_drum_panel();
            return;
        }
        if self.nr_active.get() {
            self.reprocess(id);
        }
    }

    /// Fill the (persistent) FX popover with fresh controls each time it opens.
    pub fn fill_fx(self: &Rc<Self>, id: u32, popover: &gtk::Popover) {
        let Some(fx) = self.tracks.borrow().iter().find(|t| t.id == id).map(|t| t.fx.clone()) else { return };
        let app = self.clone();
        let bpm = self.cfg.borrow().bpm;
        let bpm = self.loop_info.get().map(|i| i.bpm).unwrap_or(bpm);
        let l = self.loop_len().max(1);
        let raw_loops =
            self.tracks.borrow().iter().find(|t| t.id == id).map(|t| (t.raw.len() / l).max(1) as u32).unwrap_or(1);
        let content = super::fx::fx_content(fx, bpm, raw_loops, move |fx| app.set_fx(id, fx));
        popover.set_child(Some(&content));
    }

    pub fn set_fx(self: &Rc<Self>, id: u32, fx: TrackFx) {
        let sound_changed = {
            let mut tracks = self.tracks.borrow_mut();
            let Some(t) = tracks.iter_mut().find(|t| t.id == id) else { return };
            let cycles = t.row.wave.cycles.get();
            t.row.fx_label.set_label(&track_row::fx_text(&fx, cycles));
            if fx.is_neutral() && !fx.has_fade() {
                t.row.fx_button.remove_css_class("fx-active");
            } else {
                t.row.fx_button.add_css_class("fx-active");
            }
            // fades are a volume curve: only the sound settings need a re-render
            let sound_changed = t.fx.sound() != fx.sound();
            t.fx = fx;
            sound_changed
        };
        self.mark_dirty();
        if sound_changed {
            self.reprocess(id);
        }
        self.push_arrangement();
    }

    /// Re-render a track (noise reduction + effects) in the background, debounced.
    pub fn reprocess(self: &Rc<Self>, id: u32) {
        let generation = {
            let mut tracks = self.tracks.borrow_mut();
            let Some(t) = tracks.iter_mut().find(|t| t.id == id) else { return };
            t.render_gen += 1;
            t.row.spinner.set_visible(true);
            t.row.spinner.start();
            t.render_gen
        };
        let app = self.clone();
        glib::spawn_future_local(async move {
            glib::timeout_future(Duration::from_millis(180)).await;
            let job = {
                let tracks = app.tracks.borrow();
                let Some(t) = tracks.iter().find(|t| t.id == id) else { return };
                if t.render_gen != generation {
                    return; // a newer edit superseded this one
                }
                // noise reduction only makes sense for live guitar takes
                let nr = app.nr_active.get() && t.kind == TrackKind::Recorded;
                let profile = t.noise.clone().or_else(|| app.noise_profile.borrow().clone());
                let strength = app.cfg.borrow().noise_strength;
                (t.raw.clone(), t.fx.clone(), nr.then_some(profile).flatten(), strength)
            };
            let (raw, fx, profile, strength) = job;
            let result = gio::spawn_blocking(move || {
                let base: Vec<f32> = match &profile {
                    Some(p) => denoise::reduce(&raw, p, strength),
                    None => raw.to_vec(),
                };
                let out = effects::render(&base, &fx, SR as f32);
                let peaks = compute_peaks(&out, 1200);
                (out, peaks)
            })
            .await;
            let Ok((out, peaks)) = result else { return };
            let mut tracks = app.tracks.borrow_mut();
            let Some(t) = tracks.iter_mut().find(|t| t.id == id) else { return };
            if t.render_gen != generation {
                return;
            }
            let buf = Arc::new(out);
            // slowing down changes how many loops the track lasts
            let cycles = (buf.len() / app.loop_len().max(1)).max(1) as u32;
            let cycles_changed = cycles != t.cycles;
            if cycles_changed {
                t.cycles = cycles;
                t.row.wave.cycles.set(cycles);
                t.row.fx_label.set_label(&track_row::fx_text(&t.fx, cycles));
            }
            t.processed = buf.clone();
            *t.row.wave.peaks.borrow_mut() = peaks;
            t.row.area.queue_draw();
            t.row.spinner.stop();
            t.row.spinner.set_visible(false);
            drop(tracks);
            app.send(Command::ReplaceBuffer { id, buf });
            if cycles_changed {
                app.push_arrangement();
            }
            if let Some(ui) = app.ui.get() {
                ui.timeline.queue_draw();
            }
        });
    }

    pub fn refresh_list_visibility(&self) {
        let ui = self.ui();
        let has = !self.tracks.borrow().is_empty();
        let pending = ui.pending.is_visible();
        ui.stack.set_visible_child_name(if has || pending { "tracks" } else { "empty" });
        ui.list.set_visible(has);
        ui.undo_btn.set_sensitive(has);
        ui.export_btn.set_sensitive(has);
    }

    // ------------------------------------------------------------ periodic update

    fn tick(self: &Rc<Self>) {
        // pull status
        let st = {
            let engine = self.engine.borrow();
            let mut s = engine.status.lock().unwrap();
            let copy = s.clone();
            s.input_peak = 0.0;
            s.input_rms = 0.0;
            copy
        };
        *self.status.borrow_mut() = st.clone();
        self.ticks.set(self.ticks.get() + 1);
        if self.ticks.get().is_multiple_of(30) && std::env::var_os("SYNTH_STRING_DEBUG").is_some() {
            eprintln!(
                "[debug] rec={:?} playing={} loop={} latency={:.1}ms in_rms={:.1}dB floor={:.1}dB thr={:.1}dB nr={}",
                st.rec,
                st.playing,
                st.loop_len,
                st.latency as f32 * 1000.0 / SR as f32,
                lin_to_db(st.input_rms),
                self.noise_floor_db.get(),
                self.threshold_db.get(),
                self.nr_active.get()
            );
        }

        // engine errors
        let err = self.engine.borrow().errors.lock().unwrap().clone();
        if err != *self.last_error.borrow() {
            if let Some(e) = &err {
                self.toast(&format!("Audio problem: {e}"));
            }
            *self.last_error.borrow_mut() = err;
        }

        self.update_levels(&st);
        self.handle_events();
        self.handle_midi();
        self.refresh_transport();
    }

    fn update_levels(self: &Rc<Self>, st: &Status) {
        let ui = self.ui();
        let now = self.t0.elapsed().as_secs_f64();
        let peak_db = lin_to_db(st.input_peak);
        let rms_db = lin_to_db(st.input_rms);
        let ms = &ui.meter_state;
        let prev = ms.level_db.get();
        ms.level_db.set(if peak_db > prev { peak_db } else { (prev - 1.5).max(peak_db) });
        let (hold, at) = self.peak_hold.get();
        if peak_db > hold || now - at > 1.2 {
            self.peak_hold.set((peak_db, now));
        }
        ms.peak_db.set(self.peak_hold.get().0);
        ui.meter.queue_draw();
        ui.popover_meter.queue_draw();
        ui.ring_state.level.set(db_to_frac(ms.level_db.get()).powf(2.0));

        // noise floor: minimum of recent block RMS (ignores playing)
        if st.input_rms > 0.0 || self.input.borrow().is_some() {
            let mut h = self.floor_hist.borrow_mut();
            h.push_back(rms_db);
            while h.len() > 240 {
                h.pop_front();
            }
            if h.len() >= 30 {
                let floor = h.iter().cloned().fold(f32::INFINITY, f32::min).max(-100.0);
                self.noise_floor_db.set(floor);
                if h.len() >= 90 && floor > -58.0 && !self.auto_noisy.get() {
                    self.auto_noisy.set(true);
                    if self.cfg.borrow().noise_reduction.is_none() {
                        self.toast("Your input is a bit noisy – noise reduction switched on");
                    }
                }
            }
        }
        self.update_threshold(false);
        self.update_noise_state();
    }

    fn handle_events(self: &Rc<Self>) {
        let events: Vec<Event> = self.engine.borrow().events.try_iter().collect();
        for ev in events {
            match ev {
                Event::RecordingStarted => {}
                Event::LoopCreated { track_id, raw, noise, info } => {
                    self.loop_info.set(Some(info));
                    self.status.borrow_mut().loop_len = info.len;
                    match info.bars {
                        Some(b) => self.toast(&format!(
                            "Loop set · {b} bar{} at {:.0} BPM",
                            if b == 1 { "" } else { "s" },
                            info.bpm
                        )),
                        None if self
                            .pending_meta
                            .borrow()
                            .get(&track_id)
                            .is_some_and(|(_, k)| *k == TrackKind::Drums) =>
                        {
                            let bars = info.len as f64
                                / (60.0 * SR as f64 / info.bpm.max(1.0) as f64)
                                / self.cfg.borrow().beats_per_bar.max(1) as f64;
                            self.toast(&format!(
                                "Drum loop · {:.0} bar{} at {:.0} BPM",
                                bars,
                                if bars.round() == 1.0 { "" } else { "s" },
                                info.bpm
                            ))
                        }
                        None if self.pending_meta.borrow().contains_key(&track_id) => {
                            self.toast(&format!("Loop set · {} from the imported file", fmt_secs(info.len)))
                        }
                        None => self.toast(&format!("Loop set · {} — trimmed to your playing", fmt_secs(info.len))),
                    }
                    if info.bars.is_none() {
                        // adopt a tempo that fits the loop so the click and echoes line up
                        self.syncing.set(true);
                        self.ui().bpm_spin.set_value(info.bpm.round() as f64);
                        self.cfg.borrow_mut().bpm = info.bpm.round();
                        self.syncing.set(false);
                    }
                    self.add_track_model(track_id, raw, noise, None);
                }
                Event::DrumHit { drum, pos } => self.on_drum_hit(drum, pos),
                Event::OverdubDone { track_id, raw, noise, replaced, onset } => {
                    // injected tracks (imports, drum layers) play everywhere
                    let injected = self.pending_meta.borrow().contains_key(&track_id);
                    let song = self.song_mode.get() && !injected;
                    let placed =
                        if injected { raw.clone() } else { self.place_take(track_id, raw.clone(), onset, replaced) };
                    if !Arc::ptr_eq(&placed, &raw) {
                        self.send(Command::ReplaceBuffer { id: track_id, buf: placed.clone() });
                    }
                    let raw = placed;
                    if replaced {
                        self.mark_dirty();
                        let mut tracks = self.tracks.borrow_mut();
                        let l = self.loop_len().max(1);
                        if let Some(t) = tracks.iter_mut().find(|t| t.id == track_id) {
                            t.raw = raw.clone();
                            t.processed = raw.clone();
                            // a song-mode take can be one repetition longer than before
                            let cycles = (raw.len() / l).max(1) as u32;
                            if cycles != t.cycles {
                                t.cycles = cycles;
                                t.row.wave.cycles.set(cycles);
                                t.row.fx_label.set_label(&track_row::fx_text(&t.fx, cycles));
                            }
                            if let Some(n) = NoiseProfile::learn(&noise) {
                                t.noise = Some(n);
                            }
                            *t.row.wave.peaks.borrow_mut() = compute_peaks(&raw, 1200);
                            t.row.area.queue_draw();
                        }
                        let needs =
                            tracks.iter().find(|t| t.id == track_id).map(|t| !t.fx.is_neutral()).unwrap_or(false);
                        drop(tracks);
                        if needs || self.nr_active.get() {
                            self.reprocess(track_id);
                        }
                        self.push_arrangement();
                    } else {
                        let l = self.loop_len().max(1);
                        let clips = song.then(|| vec![(onset / l as u64) as u32]);
                        if let Some(c) = clips.as_ref().filter(|_| !injected) {
                            self.toast(&format!(
                                "Layer placed at repetition {} – click its lane to repeat it",
                                c[0] + 1
                            ));
                        }
                        self.add_track_model(track_id, raw, noise, clips);
                    }
                }
                Event::TakeDiscarded => {}
                Event::NoiseSample(v) => {
                    // only accept as noise if nobody was playing
                    let level = crate::audio::dsp::rms(&v);
                    if self.startup_noise_pending.replace(false) {
                        if lin_to_db(level) < self.threshold_db.get() {
                            *self.noise_profile.borrow_mut() = NoiseProfile::learn(&v);
                        }
                    } else if lin_to_db(level) < self.threshold_db.get() {
                        *self.noise_profile.borrow_mut() = NoiseProfile::learn(&v);
                        self.toast("Noise profile learned");
                        let ids: Vec<u32> = self.tracks.borrow().iter().map(|t| t.id).collect();
                        if self.nr_active.get() {
                            for id in ids {
                                self.reprocess(id);
                            }
                        }
                    } else {
                        self.toast("Too loud to learn noise – mute your strings and try again");
                    }
                }
                Event::Garbage(g) => drop(g),
            }
        }
    }

    fn handle_midi(self: &Rc<Self>) {
        let msgs: Vec<MidiTrigger> = self.midi.rx.try_iter().collect();
        for t in msgs {
            if let Some((action, done)) = self.learning.borrow_mut().take() {
                self.cfg.borrow_mut().midi.set(action, Some(t));
                self.save_cfg();
                self.toast(&format!("{} → {}", t.label(), action.label()));
                done();
                continue;
            }
            let action = self.cfg.borrow().midi.action_for(t);
            match action {
                Some(MidiAction::Record) => self.record(),
                Some(MidiAction::PlayPause) => self.play_pause(),
                Some(MidiAction::Undo) => self.undo(),
                Some(MidiAction::Metronome) => self.toggle_metronome(),
                None => {
                    if !self.midi_hinted.replace(true) && self.cfg.borrow().midi.entries.is_empty() {
                        let toast = adw::Toast::builder()
                            .title(format!("MIDI {} received – use it as record footswitch?", t.label()))
                            .button_label("Use for Record")
                            .timeout(8)
                            .build();
                        let app = self.clone();
                        toast.connect_button_clicked(move |_| {
                            app.cfg.borrow_mut().midi.set(MidiAction::Record, Some(t));
                            app.save_cfg();
                            app.toast("Footswitch mapped to Record");
                        });
                        self.ui().toasts.add_toast(toast);
                    }
                }
            }
        }
    }

    pub fn refresh_transport(&self) {
        let Some(ui) = self.ui.get() else { return };
        let st = self.status.borrow().clone();
        let t = self.t0.elapsed().as_secs_f64();
        let has_loop = st.loop_len > 0;
        let rs = &ui.ring_state;
        rs.t.set(t);

        // record button look
        for c in ["armed", "recording"] {
            ui.record_btn.remove_css_class(c);
        }
        let (icon, tip) = match st.rec {
            RecState::Idle => {
                ("media-record-symbolic", if has_loop { "Add a layer (Space)" } else { "Record (Space)" })
            }
            RecState::ArmedFirst | RecState::ArmedOverdub => {
                ui.record_btn.add_css_class("armed");
                ("media-record-symbolic", "Waiting for you to play – click to cancel")
            }
            RecState::RecordingFirst => {
                ui.record_btn.add_css_class("recording");
                ("media-playback-stop-symbolic", "Finish the loop (Space)")
            }
            RecState::RecordingOverdub => {
                ui.record_btn.add_css_class("recording");
                ("media-playback-stop-symbolic", "Stop overdub early (Space)")
            }
        };
        ui.record_icon.set_icon_name(Some(icon));
        ui.record_btn.set_tooltip_text(Some(tip));

        ui.play_btn.set_sensitive(has_loop);
        ui.play_btn.set_icon_name(if st.playing {
            "media-playback-pause-symbolic"
        } else {
            "media-playback-start-symbolic"
        });

        // ring
        rs.rec.set(match st.rec {
            RecState::ArmedFirst | RecState::ArmedOverdub => 1,
            RecState::RecordingFirst | RecState::RecordingOverdub => 2,
            RecState::Idle => 0,
        });
        rs.progress.set(has_loop.then(|| (st.pos % st.loop_len as u64) as f64 / st.loop_len as f64));
        rs.rec_progress.set(
            (st.rec == RecState::RecordingOverdub && st.rec_target > 0)
                .then(|| st.rec_samples as f64 / st.rec_target as f64),
        );
        if let Some(info) = self.loop_info.get() {
            let beat_len = 60.0 * SR as f64 / info.bpm as f64;
            rs.beats.set((info.len as f64 / beat_len).round() as u32);
        }
        rs.beats_per_bar.set(self.cfg.borrow().beats_per_bar);
        ui.ring.queue_draw();
        ui.beat_dots.queue_draw();

        // texts
        let ntracks = self.tracks.borrow().len();
        let (title, sub) = if self.input.borrow().is_none() {
            ("No input".to_string(), "Plug in your guitar interface – it will be picked up automatically".to_string())
        } else {
            match st.rec {
                RecState::ArmedFirst => {
                    ("Listening…".into(), "Start playing – recording begins with your first note".into())
                }
                RecState::RecordingFirst => (
                    format!("Recording  {}", fmt_secs(st.rec_samples)),
                    "Press Space when you're done – silence at the end is trimmed".into(),
                ),
                RecState::ArmedOverdub => (
                    "Ready to layer".into(),
                    format!("Play whenever you like – records exactly {} automatically", fmt_secs(st.rec_target)),
                ),
                RecState::RecordingOverdub => {
                    ("Layering…".into(), format!("{} left", fmt_secs(st.rec_target.saturating_sub(st.rec_samples))))
                }
                RecState::Idle if !has_loop => {
                    ("Press Space to record".into(), "Your first take sets the loop length".into())
                }
                RecState::Idle => {
                    let info = self.loop_info.get();
                    let bars = info
                        .and_then(|i| i.bars.map(|b| format!("{b} bar{} · ", if b == 1 { "" } else { "s" })))
                        .unwrap_or_default();
                    let bpm = info.map(|i| format!("{:.0} BPM · ", i.bpm)).unwrap_or_default();
                    (
                        if st.playing { "Looping".into() } else { "Paused".into() },
                        format!(
                            "{bars}{bpm}{} · {ntracks} track{} — Space adds a layer",
                            fmt_secs(st.loop_len),
                            if ntracks == 1 { "" } else { "s" }
                        ),
                    )
                }
            }
        };
        if ui.status_title.label() != title {
            ui.status_title.set_label(&title);
        }
        if ui.status_sub.label() != sub {
            ui.status_sub.set_label(&sub);
        }
        ui.title.set_subtitle(&if has_loop {
            format!("{} loop · {ntracks} track{}", fmt_secs(st.loop_len), if ntracks == 1 { "" } else { "s" })
        } else {
            "Guitar looper".into()
        });

        // pending overdub card
        let pending = matches!(st.rec, RecState::ArmedOverdub | RecState::RecordingOverdub);
        if ui.pending.is_visible() != pending {
            ui.pending.set_visible(pending);
            self.refresh_list_visibility();
        }
        if pending {
            let replacing =
                st.rec_track.and_then(|id| self.tracks.borrow().iter().find(|t| t.id == id).map(|t| t.name.clone()));
            let name = replacing.clone().map(|n| format!("Re-recording {n}")).unwrap_or_else(|| "New layer".into());
            ui.pending_label.set_label(&match st.rec {
                RecState::ArmedOverdub => format!("{name} – waiting for you to play…"),
                _ => format!("{name} – recording"),
            });
            ui.pending_bar.set_fraction(if st.rec_target > 0 {
                st.rec_samples as f64 / st.rec_target as f64
            } else {
                0.0
            });
        }

        // playheads
        let tracks = self.tracks.borrow();
        for tr in tracks.iter() {
            let len = tr.processed.len().max(1);
            let map = self.track_map(tr);
            let idx = crate::audio::looper::track_index(st.pos, st.loop_len, len, map.as_deref());
            tr.row.wave.playhead.set(if has_loop { idx.map(|i| i as f64 / len as f64) } else { None });
            tr.row.area.queue_draw();
        }
        drop(tracks);
        ui.timeline.queue_draw();
        if ui.drum_grid.is_visible() {
            ui.drum_grid.queue_draw();
        }
    }

    /// Synthetic plucked-string loops for demos and UI checks.
    pub fn inject_demo(&self) {
        let len = SR as usize * 4;
        let pluck = |freqs: &[f32], every: usize, decay: f32| -> Vec<f32> {
            let mut v = vec![0f32; len];
            for (k, start) in (0..len).step_by(every).enumerate() {
                let f = freqs[k % freqs.len()];
                for i in 0..(len - start).min(SR as usize) {
                    let t = i as f32 / SR as f32;
                    let env = (-t * decay).exp();
                    v[start + i] += 0.3
                        * env
                        * ((2.0 * std::f32::consts::PI * f * t).sin()
                            + 0.4 * (4.0 * std::f32::consts::PI * f * t).sin());
                }
            }
            v
        };
        let a = pluck(&[110.0, 146.8, 98.0, 130.8], SR as usize, 3.0);
        let b = pluck(&[440.0, 392.0, 523.2, 493.9, 440.0, 587.3, 523.2, 392.0], SR as usize / 2, 7.0);
        let c = pluck(&[220.0, 261.6, 329.6], SR as usize / 3, 9.0);
        for buf in [a, b, c] {
            let id = self.alloc_id();
            self.send(Command::Inject { id, buf: Arc::new(buf) });
        }
    }

    // ------------------------------------------------------------ export

    pub fn export_dialog(self: &Rc<Self>) {
        if self.tracks.borrow().is_empty() {
            self.toast("Record something first");
            return;
        }
        let ui = self.ui();
        let cfg = self.cfg.borrow().clone();
        let dialog = adw::AlertDialog::new(
            Some("Export recording"),
            Some("Saves a WAV file (48 kHz, 24-bit) with the effects you hear."),
        );
        let group = adw::PreferencesGroup::new();
        let repeats = adw::SpinRow::with_range(1.0, 64.0, 1.0);
        let song = self.song_mode.get();
        let total = self.song_reps_effective() as usize * self.loop_len();
        if song {
            repeats.set_title("Play the song");
            repeats.set_value(1.0);
            repeats.set_subtitle(&format!("times · {} per pass, as arranged", fmt_secs(total)));
        } else {
            repeats.set_title("Loop repeats");
            repeats.set_value(cfg.export_repeats as f64);
            repeats.set_subtitle(&format!("{} per repeat", fmt_secs(total)));
        }
        let stems = adw::SwitchRow::builder()
            .title("Export each track too")
            .subtitle("Separate stem files for your DAW")
            .active(cfg.export_stems)
            .build();
        group.add(&repeats);
        group.add(&stems);
        dialog.set_extra_child(Some(&group));
        dialog.add_responses(&[("cancel", "Cancel"), ("export", "Choose File…")]);
        dialog.set_response_appearance("export", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("export"));
        dialog.set_close_response("cancel");
        let app = self.clone();
        dialog.connect_response(None, move |_, resp| {
            if resp != "export" {
                return;
            }
            {
                let mut c = app.cfg.borrow_mut();
                if !app.song_mode.get() {
                    c.export_repeats = repeats.value() as u32;
                }
                c.export_stems = stems.is_active();
            }
            app.save_cfg();
            app.export_repeats.set(repeats.value() as u32);
            app.choose_export_file();
        });
        dialog.present(Some(&ui.window));
    }

    fn choose_export_file(self: &Rc<Self>) {
        let ui = self.ui();
        let stamp = glib::DateTime::now_local()
            .ok()
            .and_then(|d| d.format("%Y-%m-%d_%H-%M").ok())
            .map(|s| s.to_string())
            .unwrap_or_else(|| "loop".into());
        let filter = gtk::FileFilter::new();
        filter.add_suffix("wav");
        filter.set_name(Some("WAV audio"));
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let dialog = gtk::FileDialog::builder()
            .title("Export loop")
            .initial_name(format!("synth-string_{stamp}.wav"))
            .filters(&filters)
            .modal(true)
            .build();
        if let Some(music) = glib::user_special_dir(glib::UserDirectory::Music) {
            dialog.set_initial_folder(Some(&gio::File::for_path(music)));
        }
        let app = self.clone();
        dialog.save(Some(&ui.window), None::<&gio::Cancellable>, move |res| {
            let Ok(file) = res else { return };
            let Some(mut path) = file.path() else { return };
            if path.extension().is_none() {
                path.set_extension("wav");
            }
            app.run_export(path);
        });
    }

    fn run_export(self: &Rc<Self>, path: std::path::PathBuf) {
        let tracks: Vec<ExportTrack> = {
            let tracks = self.tracks.borrow();
            let any_solo = tracks.iter().any(|t| t.solo);
            tracks
                .iter()
                .map(|t| ExportTrack {
                    name: t.name.clone(),
                    buf: t.processed.clone(),
                    gain: t.gain,
                    audible: !t.mute && (!any_solo || t.solo),
                    map: self.track_map(t),
                    env: self.track_env(t),
                    fade: (t.fx.fade_in, t.fx.fade_out),
                })
                .collect()
        };
        let stems = self.cfg.borrow().export_stems;
        let song = export::Song {
            loop_len: self.loop_len(),
            song_len: self.song_reps_effective() as usize * self.loop_len(),
            repeats: self.export_repeats.get().max(1),
        };
        let app = self.clone();
        glib::spawn_future_local(async move {
            let p = path.clone();
            let res =
                gio::spawn_blocking(move || export::export(&tracks, &song, &p, stems).map_err(|e| e.to_string())).await;
            match res {
                Ok(Ok(files)) => {
                    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                    let msg = if files.len() > 1 {
                        format!("Exported {name} + {} stems", files.len() - 1)
                    } else {
                        format!("Exported {name}")
                    };
                    let toast = adw::Toast::builder().title(msg).button_label("Show").timeout(6).build();
                    let win = app.ui().window.clone();
                    toast.connect_button_clicked(move |_| {
                        let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(&path)));
                        launcher.open_containing_folder(Some(&win), None::<&gio::Cancellable>, |_| {});
                    });
                    app.ui().toasts.add_toast(toast);
                }
                Ok(Err(e)) => app.toast(&format!("Export failed: {e}")),
                Err(_) => app.toast("Export failed"),
            }
        });
    }
}
