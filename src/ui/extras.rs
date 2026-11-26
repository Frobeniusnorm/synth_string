//! Importing audio files as tracks and the keyboard drum machine.

use super::app::{App, MULTIPLIERS, TrackKind, fmt_secs};
use crate::audio::drums::{self, DRUM_NAMES, Hit};
use crate::audio::import::{self, Fit};
use crate::audio::looper::{Command, RecState, SR, track_index};
use adw::prelude::*;
use gtk::{gio, glib};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

/// An active drum recording session (pattern builds up pass after pass).
pub struct DrumRec {
    pub track_id: u32,
}

pub const DRUM_KEYS: [[&str; 2]; 3] = [["A", "J"], ["S", "K"], ["D", "L"]];

impl App {
    // ------------------------------------------------------------ import

    pub fn import_dialog(self: &Rc<Self>) {
        if self.status.borrow().rec != RecState::Idle {
            self.toast("Finish recording first");
            return;
        }
        let filter = gtk::FileFilter::new();
        for e in import::EXTENSIONS {
            filter.add_suffix(e);
        }
        filter.set_name(Some("Audio files"));
        let filters = gio::ListStore::new::<gtk::FileFilter>();
        filters.append(&filter);
        let dialog = gtk::FileDialog::builder().title("Import audio as a track").filters(&filters).modal(true).build();
        if let Some(music) = glib::user_special_dir(glib::UserDirectory::Music) {
            dialog.set_initial_folder(Some(&gio::File::for_path(music)));
        }
        let app = self.clone();
        dialog.open(Some(&self.ui().window), None::<&gio::Cancellable>, move |res| {
            let Ok(file) = res else { return };
            if let Some(path) = file.path() {
                app.import_file(path);
            }
        });
    }

    pub fn import_file(self: &Rc<Self>, path: PathBuf) {
        let name = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "Imported".into());
        self.toast(&format!("Importing {name}…"));
        let app = self.clone();
        glib::spawn_future_local(async move {
            let p = path.clone();
            let res = gio::spawn_blocking(move || import::decode(&p).map_err(|e| e.to_string())).await;
            let audio = match res {
                Ok(Ok(a)) => a,
                Ok(Err(e)) => return app.toast(&format!("Couldn't import {name}: {e}")),
                Err(_) => return app.toast("Import failed"),
            };
            let loop_len = app.loop_len();
            if loop_len == 0 {
                // the file defines the loop
                let trimmed = import::trim_silence(&audio).to_vec();
                if trimmed.len() < SR as usize / 4 {
                    return app.toast("That file is (almost) silent");
                }
                let mut buf = trimmed;
                let f = (SR as usize / 200).min(buf.len() / 4);
                let n = buf.len();
                for i in 0..f {
                    let g = i as f32 / f as f32;
                    buf[i] *= g;
                    buf[n - 1 - i] *= g;
                }
                app.inject_track(name, TrackKind::Imported, buf);
            } else {
                app.ask_import_fit(name, audio);
            }
        });
    }

    fn ask_import_fit(self: &Rc<Self>, name: String, audio: Vec<f32>) {
        let l = self.loop_len();
        let nearest = import::nearest_multiple(audio.len(), l);
        let dialog = adw::AlertDialog::new(
            Some(&format!("Fit “{name}” to the loop")),
            Some(&format!("The file is {} long, your loop is {}.", fmt_secs(audio.len()), fmt_secs(l))),
        );
        let group = adw::PreferencesGroup::new();
        let labels: Vec<String> =
            MULTIPLIERS.iter().map(|m| format!("{m}× loop ({})", fmt_secs(*m as usize * l))).collect();
        let refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
        let length = adw::ComboRow::builder().title("Length").model(&gtk::StringList::new(&refs)).build();
        let best = MULTIPLIERS
            .iter()
            .enumerate()
            .min_by_key(|(_, m)| (**m as i64 - nearest as i64).abs())
            .map(|(i, _)| i)
            .unwrap_or(0);
        length.set_selected(best as u32);
        let fit = adw::ComboRow::builder()
            .title("Fit")
            .model(&gtk::StringList::new(&["Stretch to fit (keeps pitch)", "Cut or pad with silence"]))
            .build();
        // stretching only makes sense when the tempo is roughly right
        let ratio = audio.len() as f64 / (MULTIPLIERS[best] as usize * l) as f64;
        fit.set_selected(if (0.8..1.25).contains(&ratio) { 0 } else { 1 });
        let trim = adw::SwitchRow::builder().title("Skip silence at the start").active(true).build();
        group.add(&length);
        group.add(&fit);
        group.add(&trim);
        dialog.set_extra_child(Some(&group));
        dialog.add_responses(&[("cancel", "Cancel"), ("import", "Import")]);
        dialog.set_response_appearance("import", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("import"));
        dialog.set_close_response("cancel");
        let app = self.clone();
        let audio = Rc::new(audio);
        dialog.connect_response(None, move |_, r| {
            if r != "import" {
                return;
            }
            let target = MULTIPLIERS[(length.selected() as usize).min(MULTIPLIERS.len() - 1)] as usize * l;
            let mode = if fit.selected() == 0 { Fit::Stretch } else { Fit::CutOrPad };
            let src: Vec<f32> =
                if trim.is_active() { audio[import::first_sound(&audio)..].to_vec() } else { audio.to_vec() };
            let app = app.clone();
            let name = name.clone();
            glib::spawn_future_local(async move {
                let res = gio::spawn_blocking(move || import::fit(&src, target, mode)).await;
                if let Ok(buf) = res {
                    app.inject_track(name, TrackKind::Imported, buf);
                }
            });
        });
        dialog.present(Some(&self.ui().window));
    }

    /// Add a ready-made buffer as a new track (defines the loop if there is none).
    pub fn inject_track(&self, name: String, kind: TrackKind, buf: Vec<f32>) -> u32 {
        let id = self.alloc_track_id();
        self.pending_meta.borrow_mut().insert(id, (name, kind));
        self.send(Command::Inject { id, buf: Arc::new(buf) });
        id
    }

    // ------------------------------------------------------------ drum machine

    /// Length of one beat in samples (loop tempo, or the metronome tempo before there is a loop).
    pub fn drum_beat(&self) -> f64 {
        let bpm = self.loop_info.get().map(|i| i.bpm).unwrap_or(self.cfg.borrow().bpm);
        60.0 * SR as f64 / bpm.max(20.0) as f64
    }

    pub fn drum_pad(self: &Rc<Self>, drum: u8) {
        let snap = self.cfg.borrow().drum_snap_div;
        self.send(Command::DrumHit { drum, snap });
        if let Some(pad) = self.ui().drum_pads.get(drum as usize) {
            pad.add_css_class("hit");
            let pad = pad.clone();
            glib::timeout_add_local_once(Duration::from_millis(110), move || pad.remove_css_class("hit"));
        }
    }

    /// Record button of the drum machine: start a drum layer, or finish the current one.
    pub fn drum_record_toggle(self: &Rc<Self>) {
        if self.drum_rec.borrow().is_some() {
            self.finish_drums();
            return;
        }
        if self.status.borrow().rec != RecState::Idle {
            self.toast("Finish the guitar recording first");
            return;
        }
        let l = self.loop_len();
        let len = if l == 0 {
            // no loop yet: the drum pattern defines it (N bars at the current tempo, with click)
            let (bpm, bpb) = {
                let c = self.cfg.borrow();
                (c.bpm, c.beats_per_bar)
            };
            let bars = [1usize, 2, 4, 8][(self.ui().drum_bars.selected() as usize).min(3)];
            if !self.ui().metro_toggle.is_active() {
                self.ui().metro_toggle.set_active(true);
            }
            (bars as f64 * bpb as f64 * 60.0 / bpm as f64 * SR as f64).round() as usize
        } else {
            MULTIPLIERS[(self.ui().mult_dd.selected() as usize).min(MULTIPLIERS.len() - 1)] as usize * l
        };
        let id = self.inject_track("Drums".into(), TrackKind::Drums, vec![0.0; len]);
        *self.drum_rec.borrow_mut() = Some(DrumRec { track_id: id });
        if !self.status.borrow().playing && l > 0 {
            self.play_pause();
        }
        self.refresh_drum_panel();
    }

    /// Keep adding hits to an existing drum track.
    pub fn edit_drums(self: &Rc<Self>, id: u32) {
        self.ui().drums_toggle.set_active(true);
        *self.drum_rec.borrow_mut() = Some(DrumRec { track_id: id });
        if !self.status.borrow().playing {
            self.play_pause();
        }
        self.refresh_drum_panel();
    }

    pub fn finish_drums(&self) {
        let Some(rec) = self.drum_rec.borrow_mut().take() else { return };
        let hits = self.tracks.borrow().iter().find(|t| t.id == rec.track_id).map(|t| t.drum_hits.len()).unwrap_or(0);
        if hits == 0 {
            self.delete_track(rec.track_id);
            self.toast("No hits recorded – drum layer removed");
        } else {
            self.toast(&format!("Drum layer done · {hits} hits"));
        }
        self.refresh_drum_panel();
    }

    /// Remove the most recent hit of the drum layer being recorded.
    pub fn undo_drum_hit(self: &Rc<Self>) -> bool {
        let Some(id) = self.drum_rec.borrow().as_ref().map(|r| r.track_id) else { return false };
        let removed =
            self.tracks.borrow_mut().iter_mut().find(|t| t.id == id).and_then(|t| t.drum_hits.pop()).is_some();
        if removed {
            self.rerender_drums(id);
        }
        true
    }

    pub fn on_drum_hit(self: &Rc<Self>, drum: u8, pos: Option<u64>) {
        let Some(id) = self.drum_rec.borrow().as_ref().map(|r| r.track_id) else { return };
        let Some(pos) = pos else { return };
        let l = self.loop_len();
        {
            let mut tracks = self.tracks.borrow_mut();
            let Some(t) = tracks.iter_mut().find(|t| t.id == id) else { return };
            let map = self.track_map(t);
            // the hit was played over the processed sound: map it back to the pattern's time
            let Some(idx) = track_index(pos, l, t.processed.len(), map.as_deref()) else { return };
            let idx = t.fx.source_index(idx, t.raw.len());
            let snap = self.cfg.borrow().drum_snap_div;
            t.drum_hits.push(Hit { pos: idx as u64, drum, snap });
        }
        self.mark_dirty();
        self.rerender_drums(id);
    }

    /// Re-synthesize a drum track from its pattern (instantly audible on the next pass).
    pub fn rerender_drums(self: &Rc<Self>, id: u32) {
        let beat = self.drum_beat();
        {
            let mut tracks = self.tracks.borrow_mut();
            let Some(t) = tracks.iter_mut().find(|t| t.id == id) else { return };
            let buf = Arc::new(drums::render(&t.drum_hits, t.raw.len(), &self.kit, beat));
            t.raw = buf.clone();
            if t.fx.is_neutral() {
                t.processed = buf.clone();
                *t.row.wave.peaks.borrow_mut() = super::widgets::compute_peaks(&buf, 1200);
                t.row.area.queue_draw();
                self.send(Command::ReplaceBuffer { id, buf });
            }
        }
        let needs_fx = self.tracks.borrow().iter().find(|t| t.id == id).is_some_and(|t| !t.fx.is_neutral());
        if needs_fx {
            self.reprocess(id);
        }
        self.refresh_drum_panel();
    }

    /// Snap for the hits you play from now on (1/N note, 0 = off). While a drum layer is being
    /// recorded, its existing hits follow the new snap too, so you can tighten a sloppy take.
    pub fn set_drum_snap(self: &Rc<Self>, div: u8) {
        self.cfg.borrow_mut().drum_snap_div = div;
        self.save_cfg();
        let Some(id) = self.drum_rec.borrow().as_ref().map(|r| r.track_id) else { return };
        let changed = {
            let mut tracks = self.tracks.borrow_mut();
            let Some(t) = tracks.iter_mut().find(|t| t.id == id) else { return };
            let mut changed = false;
            for h in t.drum_hits.iter_mut().filter(|h| h.snap != div) {
                h.snap = div;
                changed = true;
            }
            changed
        };
        if changed {
            self.mark_dirty();
            self.rerender_drums(id);
        }
    }

    pub fn refresh_drum_panel(&self) {
        let Some(ui) = self.ui.get() else { return };
        let rec = self.drum_rec.borrow();
        let has_loop = self.loop_len() > 0;
        ui.drum_bars_box.set_visible(!has_loop && rec.is_none());
        match &*rec {
            Some(r) => {
                let hits =
                    self.tracks.borrow().iter().find(|t| t.id == r.track_id).map(|t| t.drum_hits.len()).unwrap_or(0);
                ui.drum_record.set_label("Done");
                ui.drum_record.add_css_class("destructive-action");
                ui.drum_record.remove_css_class("suggested-action");
                ui.drum_status.set_label(&format!(
                    "Recording · {hits} hit{} — keep playing, every pass adds to the pattern. Space = done, Ctrl+Z = remove last hit",
                    if hits == 1 { "" } else { "s" }
                ));
            }
            None => {
                ui.drum_record.set_label("Record drum layer");
                ui.drum_record.remove_css_class("destructive-action");
                ui.drum_record.add_css_class("suggested-action");
                ui.drum_status.set_label(&format!(
                    "Play with {} / {} / {} (or {} {} {}). {}",
                    DRUM_KEYS[0][0],
                    DRUM_KEYS[1][0],
                    DRUM_KEYS[2][0],
                    DRUM_KEYS[0][1],
                    DRUM_KEYS[1][1],
                    DRUM_KEYS[2][1],
                    if has_loop {
                        "Recording loops over the layer length."
                    } else {
                        "Without a loop the pattern sets its length."
                    }
                ));
            }
        }
    }
}

/// The drum machine card (hidden until "Drums" is toggled on).
pub struct DrumPanel {
    pub card: gtk::Revealer,
    pub pads: Vec<gtk::Button>,
    pub record: gtk::Button,
    pub status: gtk::Label,
    pub bars: gtk::DropDown,
    pub bars_box: gtk::Box,
}

pub fn drum_panel(app: &Rc<App>) -> DrumPanel {
    let card = gtk::Box::new(gtk::Orientation::Vertical, 12);
    card.add_css_class("card");
    card.add_css_class("drum-card");

    let head = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    head.append(
        &gtk::Label::builder().label("Drum machine").xalign(0.0).hexpand(true).css_classes(["heading"]).build(),
    );
    let snap = gtk::DropDown::from_strings(&drums::SNAP_LABELS);
    snap.set_valign(gtk::Align::Center);
    snap.set_tooltip_text(Some("Snap the hits you play to this note value"));
    let cur = app.cfg.borrow().drum_snap_div;
    snap.set_selected(drums::SNAP_DIVS.iter().position(|d| *d == cur).unwrap_or(4) as u32);
    {
        let app = app.clone();
        snap.connect_selected_notify(move |d| {
            app.set_drum_snap(drums::SNAP_DIVS[(d.selected() as usize).min(drums::SNAP_DIVS.len() - 1)])
        });
    }
    let bars = gtk::DropDown::from_strings(&["1 bar", "2 bars", "4 bars", "8 bars"]);
    bars.set_selected(1);
    let bars_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    bars_box.append(&gtk::Label::builder().label("Length").css_classes(["dim-label"]).build());
    bars_box.append(&bars);
    head.append(&bars_box);
    head.append(&gtk::Label::builder().label("Snap").css_classes(["dim-label"]).build());
    head.append(&snap);
    let record = gtk::Button::builder().label("Record drum layer").css_classes(["pill", "suggested-action"]).build();
    {
        let app = app.clone();
        record.connect_clicked(move |_| app.drum_record_toggle());
    }
    head.append(&record);
    card.append(&head);

    let pads_box = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    pads_box.set_homogeneous(true);
    let mut pads = Vec::new();
    for (i, name) in DRUM_NAMES.iter().enumerate() {
        let inner = gtk::Box::new(gtk::Orientation::Vertical, 2);
        inner.append(&gtk::Label::builder().label(*name).css_classes(["title-3"]).build());
        inner.append(
            &gtk::Label::builder()
                .label(format!("{} · {}", DRUM_KEYS[i][0], DRUM_KEYS[i][1]))
                .css_classes(["dim-label", "caption"])
                .build(),
        );
        let pad = gtk::Button::builder().child(&inner).css_classes(["drum-pad", &format!("drum-pad-{i}")]).build();
        pad.set_focus_on_click(false);
        let app = app.clone();
        // fire on press (not release) so it feels like a pad
        let press = gtk::GestureClick::new();
        press.set_propagation_phase(gtk::PropagationPhase::Capture);
        press.connect_pressed(move |_, _, _, _| app.drum_pad(i as u8));
        pad.add_controller(press);
        pads_box.append(&pad);
        pads.push(pad);
    }
    card.append(&pads_box);
    let status = gtk::Label::builder().xalign(0.0).wrap(true).css_classes(["caption", "dim-label"]).build();
    card.append(&status);

    let revealer = gtk::Revealer::builder()
        .child(&card)
        .transition_type(gtk::RevealerTransitionType::SlideDown)
        .reveal_child(false)
        .build();
    DrumPanel { card: revealer, pads, record, status, bars, bars_box }
}
