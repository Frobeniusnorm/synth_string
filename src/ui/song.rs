//! Song arrangement (repetitions of the base loop, per-track clip placement), the global
//! timeline with seeking, and project save / load.

use super::app::{App, TrackModel, fmt_secs};
use super::track_row;
use super::widgets::{TRACK_COLORS, compute_peaks};
use crate::audio::denoise::NoiseProfile;
use crate::audio::looper::{Command, LoopInfo, RestoreTrack, SR, track_index};
use crate::project::{self, Project, ProjectMeta, ProjectTrack, TrackMeta};
use adw::prelude::*;
use gtk::{cairo, gio, glib};
use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

pub const RULER_H: f64 = 18.0;

/// Height of the summed waveform (taller in a wide window).
fn wave_h(app: &App) -> f64 {
    if app.wide.get() { 132.0 } else { 56.0 }
}

/// Height of one track lane in song mode.
fn lane_h(app: &App) -> f64 {
    if app.wide.get() { 30.0 } else { 22.0 }
}
pub const LANE_GAP: f64 = 4.0;

/// Per repetition: the repetition at which the covering clip starts, or −1.
/// `clips == None` means "everywhere" (aligned to multiples of the track length, cut at the end).
/// Placed clips that run past the last repetition wrap around to the start (the song loops);
/// a wrapped tail never overrides a clip that really starts there.
pub fn build_map(clips: Option<&[u32]>, k: u32, reps: u32) -> Vec<i32> {
    let k = k.max(1);
    let mut map = vec![-1i32; reps as usize];
    match clips {
        None => {
            for s in (0..reps).step_by(k as usize) {
                for r in s..(s + k).min(reps) {
                    map[r as usize] = s as i32;
                }
            }
        }
        Some(c) => {
            let mut starts: Vec<u32> = c.iter().copied().filter(|&s| s < reps).collect();
            starts.sort_unstable();
            for &s in &starts {
                for r in s..(s + k).min(reps) {
                    map[r as usize] = s as i32;
                }
            }
            for &s in &starts {
                for r in reps..(s + k).min(s + reps) {
                    let r = (r - reps) as usize;
                    if map[r] < 0 {
                        map[r] = s as i32;
                    }
                }
            }
        }
    }
    map
}

/// Does a clip of length `k` starting at `c` cover repetition `rep` (wrapping at `reps`)?
pub fn clip_covers(c: u32, k: u32, rep: u32, reps: u32) -> bool {
    let reps = reps.max(1);
    (rep + reps - c % reps) % reps < k.max(1).min(reps)
}

/// Toggle a clip of length `k` at repetition `rep`: remove the clip covering it, or place a new
/// one there (removing clips it would overlap). Clips may wrap around the song end.
pub fn toggle_clip(clips: &mut Vec<u32>, k: u32, rep: u32, reps: u32) {
    let k = k.max(1);
    if let Some(i) = clips.iter().position(|&c| clip_covers(c, k, rep, reps)) {
        clips.remove(i);
    } else {
        clips.retain(|&c| !clip_covers(c, k, rep, reps) && !clip_covers(rep, k, c, reps));
        clips.push(rep);
        clips.sort_unstable();
    }
}

/// Turn a circularly recorded take (`raw[start]` = first played sample) into a clip in playing
/// order: `off` samples of silence, the take, silence up to the next whole loop.
pub fn linearize_take(raw: &[f32], start: usize, off: usize, l: usize) -> Vec<f32> {
    let m = raw.len();
    let l = l.max(1);
    let len = (off + m).div_ceil(l) * l;
    let mut out = vec![0f32; len];
    for i in 0..m {
        out[off + i] = raw[(start + i) % m];
    }
    // short fades: the take now has a real beginning and end (no loop seam)
    let f = (SR as usize / 200).min(m / 4);
    for i in 0..f {
        let g = i as f32 / f as f32;
        out[off + i] *= g;
        out[off + m - 1 - i] *= g;
    }
    out
}

/// Volume envelope for fades: per repetition (gain at its start, gain at its end). The track
/// plays in stretches of consecutive repetitions; each stretch fades in over its first
/// `fade_in` and out over its last `fade_out` repetitions. `None` when there is no fade.
pub fn fade_envelope(map: &[i32], fade_in: u32, fade_out: u32) -> Option<Vec<(f32, f32)>> {
    if fade_in == 0 && fade_out == 0 {
        return None;
    }
    let n = map.len();
    let mut env = vec![(1.0f32, 1.0f32); n];
    let mut r = 0;
    while r < n {
        if map[r] < 0 {
            r += 1;
            continue;
        }
        let a = r;
        while r < n && map[r] >= 0 {
            r += 1;
        }
        let b = r;
        let gain = |x: usize| {
            let mut g = 1.0f32;
            if fade_in > 0 {
                g *= ((x - a) as f32 / fade_in as f32).min(1.0);
            }
            if fade_out > 0 {
                g *= ((b - x) as f32 / fade_out as f32).min(1.0);
            }
            g
        };
        for (k, e) in env.iter_mut().enumerate().take(b).skip(a) {
            *e = (gain(k), gain(k + 1));
        }
    }
    Some(env)
}

fn lcm_reps(tracks: &[TrackModel]) -> u32 {
    fn gcd(a: u32, b: u32) -> u32 {
        if b == 0 { a } else { gcd(b, a % b) }
    }
    let mut l = 1u32;
    for t in tracks {
        let k = t.cycles.max(1);
        let n = l / gcd(l, k) * k;
        l = if n > 64 { l.max(k) } else { n };
    }
    l
}

impl App {
    pub fn loop_len(&self) -> usize {
        self.loop_info.get().map(|i| i.len).unwrap_or(0)
    }

    /// Number of loop repetitions in one pass through the song.
    pub fn song_reps_effective(&self) -> u32 {
        if self.song_mode.get() { self.song_reps.get().max(1) } else { lcm_reps(&self.tracks.borrow()) }
    }

    /// Arrangement map of a track as the engine should use it (`None` = plain looping).
    pub fn track_map(&self, t: &TrackModel) -> Option<Vec<i32>> {
        self.song_mode.get().then(|| build_map(t.clips.as_deref(), t.cycles, self.song_reps.get().max(1)))
    }

    /// Fade envelope of a track as the engine should use it (song mode only: a plain loop never ends).
    pub fn track_env(&self, t: &TrackModel) -> Option<Vec<(f32, f32)>> {
        let map = self.track_map(t)?;
        fade_envelope(&map, t.fx.fade_in, t.fx.fade_out)
    }

    pub fn push_arrangement(&self) {
        self.send(Command::SetSongReps(if self.song_mode.get() { self.song_reps.get().max(1) } else { 0 }));
        for t in self.tracks.borrow().iter() {
            self.send(Command::SetTrackMap {
                id: t.id,
                map: self.track_map(t).map(Arc::new),
                env: self.track_env(t).map(Arc::new),
            });
        }
        self.refresh_song_widgets();
    }

    pub fn refresh_song_widgets(&self) {
        let Some(ui) = self.ui.get() else { return };
        let has_loop = self.loop_len() > 0;
        ui.timeline_card.set_visible(has_loop);
        self.syncing.set(true);
        ui.song_switch.set_active(self.song_mode.get());
        ui.song_reps_spin.set_value(self.song_reps.get() as f64);
        self.syncing.set(false);
        ui.song_reps_box.set_visible(self.song_mode.get());
        let reps = self.song_reps_effective();
        let total = reps as usize * self.loop_len();
        ui.song_len_label.set_label(&if self.song_mode.get() {
            format!("{} · {} × {}", fmt_secs(total), reps, fmt_secs(self.loop_len()))
        } else {
            fmt_secs(total)
        });
        ui.timeline_hint.set_label(if self.song_mode.get() {
            "Click a lane to place or remove a track at that repetition · click the waveform to jump"
        } else {
            "Click the waveform to jump · turn on Song mode to arrange repetitions"
        });
        let lanes = if self.song_mode.get() { self.tracks.borrow().len() as f64 } else { 0.0 };
        ui.timeline.set_content_height((RULER_H + wave_h(self) + lanes * (lane_h(self) + LANE_GAP) + 4.0) as i32);
        ui.timeline.queue_draw();
    }

    /// The window crossed the wide-layout breakpoint.
    pub fn set_wide(&self, wide: bool) {
        if self.wide.replace(wide) != wide {
            self.refresh_song_widgets();
        }
    }

    pub fn set_song_mode(&self, on: bool) {
        if on == self.song_mode.get() {
            return;
        }
        if on && self.song_reps.get() == 0 {
            self.song_reps.set(lcm_reps(&self.tracks.borrow()).max(8));
        }
        self.song_mode.set(on);
        self.mark_dirty();
        self.push_arrangement();
        self.toast(if on {
            "Song mode – the loop plays as a song of repetitions you can arrange"
        } else {
            "Loop mode – every track loops"
        });
    }

    pub fn set_song_reps(&self, reps: u32) {
        if reps == self.song_reps.get() {
            return;
        }
        self.song_reps.set(reps.clamp(1, 999));
        self.mark_dirty();
        self.push_arrangement();
    }

    pub fn toggle_clip_at(&self, track_index_in_list: usize, rep: u32) {
        let reps = self.song_reps.get().max(1);
        if rep >= reps {
            return;
        }
        {
            let mut tracks = self.tracks.borrow_mut();
            let Some(t) = tracks.get_mut(track_index_in_list) else { return };
            let mut clips = t.clips.clone().unwrap_or_else(|| (0..reps).step_by(t.cycles.max(1) as usize).collect());
            toggle_clip(&mut clips, t.cycles, rep, reps);
            t.clips = Some(clips);
        }
        self.mark_dirty();
        self.push_arrangement();
    }

    pub fn seek_fraction(&self, f: f64) {
        let song_len = self.song_reps_effective() as usize * self.loop_len();
        if song_len == 0 {
            return;
        }
        if self.status.borrow().rec == crate::audio::looper::RecState::RecordingOverdub {
            self.toast("Can't jump while recording a layer");
            return;
        }
        let pos = (f.clamp(0.0, 0.9999) * song_len as f64) as u64;
        self.send(Command::Seek(pos));
        self.status.borrow_mut().pos = pos;
    }

    /// Where a freshly recorded layer goes. In song mode the take is laid out exactly in the order
    /// you played it: it starts at the repetition you started playing in (silence before your
    /// first note) and runs on from there, one repetition longer if needed – nothing wraps to the
    /// front. In loop mode the take stays circular (that's what a loop needs).
    pub fn place_take(&self, id: u32, raw: Arc<Vec<f32>>, onset: u64, replaced: bool) -> Arc<Vec<f32>> {
        let l = self.loop_len();
        if !self.song_mode.get() || l == 0 || raw.is_empty() {
            return raw;
        }
        let reps = self.song_reps.get().max(1);
        let rep = ((onset / l as u64) as u32).min(reps - 1);
        let mut tracks = self.tracks.borrow_mut();
        let existing = tracks.iter_mut().find(|t| t.id == id);
        let anchor = match existing {
            Some(t) if replaced => {
                let k = t.cycles.max(1);
                let clips = t.clips.clone().unwrap_or_else(|| (0..reps).step_by(k as usize).collect());
                match clips.iter().find(|&&c| clip_covers(c, k, rep, reps)) {
                    Some(&c) => c,
                    None => {
                        let mut c2 = clips.clone();
                        toggle_clip(&mut c2, k, rep, reps);
                        t.clips = Some(c2);
                        rep
                    }
                }
            }
            _ => rep,
        };
        drop(tracks);
        // offset of the first played sample from the start of the clip
        let reps_in = (rep + reps - anchor) % reps;
        let off = reps_in as usize * l + (onset % l as u64) as usize;
        Arc::new(linearize_take(&raw, (onset % raw.len() as u64) as usize, off, l))
    }

    pub fn mark_dirty(&self) {
        if !self.dirty.replace(true) {
            self.update_window_title();
        }
    }

    pub fn update_window_title(&self) {
        let Some(ui) = self.ui.get() else { return };
        let name = self
            .project_path
            .borrow()
            .as_ref()
            .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().to_string()))
            .unwrap_or_else(|| "Synth String".into());
        let has = !self.tracks.borrow().is_empty();
        ui.title.set_title(&if self.dirty.get() && has { format!("• {name}") } else { name });
    }

    // ------------------------------------------------------------ project files

    fn project_snapshot(&self) -> Project {
        let cfg = self.cfg.borrow();
        let info = self.loop_info.get();
        let tracks = self.tracks.borrow();
        Project {
            meta: ProjectMeta {
                loop_len: self.loop_len(),
                bpm: info.map(|i| i.bpm).unwrap_or(cfg.bpm),
                bars: info.and_then(|i| i.bars),
                beats_per_bar: cfg.beats_per_bar,
                song_mode: self.song_mode.get(),
                song_reps: self.song_reps.get(),
                tracks: Vec::new(),
            },
            tracks: tracks
                .iter()
                .map(|t| ProjectTrack {
                    meta: TrackMeta {
                        id: t.id,
                        name: t.name.clone(),
                        gain: t.gain,
                        mute: t.mute,
                        solo: t.solo,
                        fx: t.fx.clone(),
                        clips: t.clips.clone().unwrap_or_default(),
                        clips_everywhere: t.clips.is_none(),
                        kind: t.kind,
                        drum_hits: t.drum_hits.clone(),
                        ..Default::default()
                    },
                    audio: t.raw.to_vec(),
                    noise: t.noise.as_ref().map(|n| n.mags.clone()),
                })
                .collect(),
        }
    }

    /// Save to the current file, or ask for one. `then` runs after a successful save.
    pub fn save_project(self: &Rc<Self>, save_as: bool, then: Option<Box<dyn Fn()>>) {
        if self.tracks.borrow().is_empty() {
            self.toast("Nothing to save yet – record something first");
            return;
        }
        let current = self.project_path.borrow().clone();
        match current {
            Some(p) if !save_as => self.write_project(p, then),
            _ => {
                let dialog = gtk::FileDialog::builder()
                    .title("Save project")
                    .initial_name(format!("My song.{}", project::EXTENSION))
                    .filters(&project_filters())
                    .modal(true)
                    .build();
                if let Some(music) = glib::user_special_dir(glib::UserDirectory::Music) {
                    dialog.set_initial_folder(Some(&gio::File::for_path(music)));
                }
                let app = self.clone();
                dialog.save(Some(&self.ui().window), None::<&gio::Cancellable>, move |res| {
                    let Ok(file) = res else { return };
                    let Some(mut path) = file.path() else { return };
                    if path.extension().is_none_or(|e| e != project::EXTENSION) {
                        path.set_extension(project::EXTENSION);
                    }
                    app.write_project(path, then);
                });
            }
        }
    }

    fn write_project(self: &Rc<Self>, path: PathBuf, then: Option<Box<dyn Fn()>>) {
        let snapshot = self.project_snapshot();
        let app = self.clone();
        glib::spawn_future_local(async move {
            let p = path.clone();
            let res = gio::spawn_blocking(move || project::save(&p, &snapshot).map_err(|e| e.to_string())).await;
            match res {
                Ok(Ok(())) => {
                    *app.project_path.borrow_mut() = Some(path.clone());
                    app.dirty.set(false);
                    app.update_window_title();
                    app.toast(&format!(
                        "Saved {}",
                        path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
                    ));
                    if let Some(f) = then {
                        f();
                    }
                }
                Ok(Err(e)) => app.toast(&format!("Saving failed: {e}")),
                Err(_) => app.toast("Saving failed"),
            }
        });
    }

    /// Run `action` now, or after asking to save unsaved work.
    pub fn confirm_discard(self: &Rc<Self>, action: Rc<dyn Fn()>) {
        if !self.dirty.get() || self.tracks.borrow().is_empty() {
            action();
            return;
        }
        let d = adw::AlertDialog::new(
            Some("Save changes?"),
            Some("Your loops have unsaved changes. They are lost if you don't save them."),
        );
        d.add_responses(&[("cancel", "Cancel"), ("discard", "Discard"), ("save", "Save")]);
        d.set_response_appearance("discard", adw::ResponseAppearance::Destructive);
        d.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        d.set_default_response(Some("save"));
        d.set_close_response("cancel");
        let app = self.clone();
        d.connect_response(None, move |_, r| match r {
            "discard" => action(),
            "save" => {
                let action = action.clone();
                app.save_project(false, Some(Box::new(move || action())));
            }
            _ => {}
        });
        d.present(Some(&self.ui().window));
    }

    pub fn new_project(self: &Rc<Self>) {
        let app = self.clone();
        self.confirm_discard(Rc::new(move || {
            app.clear_all();
            app.song_mode.set(false);
            app.song_reps.set(0);
            *app.project_path.borrow_mut() = None;
            app.dirty.set(false);
            app.push_arrangement();
            app.update_window_title();
        }));
    }

    pub fn open_project_dialog(self: &Rc<Self>) {
        let app = self.clone();
        self.confirm_discard(Rc::new(move || {
            let dialog =
                gtk::FileDialog::builder().title("Open project").filters(&project_filters()).modal(true).build();
            if let Some(music) = glib::user_special_dir(glib::UserDirectory::Music) {
                dialog.set_initial_folder(Some(&gio::File::for_path(music)));
            }
            let app2 = app.clone();
            dialog.open(Some(&app.ui().window), None::<&gio::Cancellable>, move |res| {
                let Ok(file) = res else { return };
                if let Some(path) = file.path() {
                    app2.load_project(path);
                }
            });
        }));
    }

    pub fn load_project(self: &Rc<Self>, path: PathBuf) {
        let app = self.clone();
        glib::spawn_future_local(async move {
            let p = path.clone();
            let res = gio::spawn_blocking(move || project::load(&p).map_err(|e| e.to_string())).await;
            let project = match res {
                Ok(Ok(p)) => p,
                Ok(Err(e)) => return app.toast(&format!("Couldn't open project: {e}")),
                Err(_) => return app.toast("Couldn't open project"),
            };
            app.apply_project(project);
            *app.project_path.borrow_mut() = Some(path);
            app.dirty.set(false);
            app.update_window_title();
            app.toast("Project loaded – press Enter to play");
        });
    }

    fn apply_project(self: &Rc<Self>, project: Project) {
        self.clear_all();
        let meta = project.meta;
        if meta.loop_len == 0 || project.tracks.is_empty() {
            return;
        }
        let beat_len = 60.0 * SR as f64 / meta.bpm.max(20.0) as f64;
        self.loop_info.set(Some(LoopInfo { len: meta.loop_len, bars: meta.bars, bpm: meta.bpm }));
        self.status.borrow_mut().loop_len = meta.loop_len;
        self.song_mode.set(meta.song_mode);
        self.song_reps.set(meta.song_reps);
        {
            let mut c = self.cfg.borrow_mut();
            c.bpm = meta.bpm.round();
            c.beats_per_bar = meta.beats_per_bar.max(1);
        }
        self.syncing.set(true);
        self.ui().bpm_spin.set_value(meta.bpm.round() as f64);
        self.syncing.set(false);

        let mut restore = Vec::new();
        let mut max_id = 0;
        for pt in project.tracks {
            let tm = pt.meta;
            let raw = Arc::new(pt.audio);
            // slowed-down tracks play longer than the raw take
            let cycles = (raw.len() * tm.fx.slow_down() / meta.loop_len.max(1)).max(1) as u32;
            let color = (tm.id.max(1) as usize - 1) % TRACK_COLORS.len();
            let row = track_row::build(self, tm.id, &tm.name, color, &tm.fx, cycles, tm.kind);
            *row.wave.peaks.borrow_mut() = compute_peaks(&raw, 1200);
            row.set_state(tm.gain, tm.mute, tm.solo, &tm.fx);
            self.ui().list.append(&row.row);
            let clips = if tm.clips_everywhere { None } else { Some(tm.clips.clone()) };
            let model = TrackModel {
                id: tm.id,
                kind: tm.kind,
                drum_hits: tm.drum_hits.clone(),
                name: tm.name.clone(),
                raw: raw.clone(),
                noise: pt.noise.map(|mags| NoiseProfile { mags }),
                fx: tm.fx.clone(),
                gain: tm.gain,
                mute: tm.mute,
                solo: tm.solo,
                processed: raw.clone(),
                render_gen: 0,
                cycles,
                clips,
                row,
            };
            restore.push(RestoreTrack {
                id: tm.id,
                buf: raw,
                gain: tm.gain,
                mute: tm.mute,
                solo: tm.solo,
                map: self.track_map(&model).map(Arc::new),
            });
            max_id = max_id.max(tm.id);
            self.tracks.borrow_mut().push(model);
        }
        self.set_next_id(max_id + 1);
        self.send(Command::Restore { loop_len: meta.loop_len, beat_len, tracks: restore });
        self.push_metronome();
        self.push_arrangement();
        self.refresh_dim();
        self.refresh_list_visibility();
        let ids: Vec<u32> = self
            .tracks
            .borrow()
            .iter()
            .filter(|t| !t.fx.is_neutral() || (self.nr_active.get() && t.kind == super::app::TrackKind::Recorded))
            .map(|t| t.id)
            .collect();
        for id in ids {
            self.reprocess(id);
        }
    }
}

fn project_filters() -> gio::ListStore {
    let filter = gtk::FileFilter::new();
    filter.add_suffix(project::EXTENSION);
    filter.set_name(Some("Synth String projects"));
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);
    filters
}

// ---------------------------------------------------------------- timeline widget

/// Global timeline: rep ruler, summed waveform of everything audible with a playhead (click or
/// drag to jump), and in song mode one lane per track to place clips.
pub fn timeline(app: &Rc<App>) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::builder().hexpand(true).content_height((RULER_H + wave_h(app) + 4.0) as i32).build();
    area.set_cursor_from_name(Some("pointer"));
    {
        let app = app.clone();
        area.set_draw_func(move |a, cr, w, _h| draw_timeline(&app, a, cr, w as f64));
    }
    let drag = gtk::GestureDrag::new();
    let in_wave = Rc::new(Cell::new(false));
    let start_x = Rc::new(Cell::new(0.0));
    {
        let app = app.clone();
        let a = area.clone();
        let in_wave = in_wave.clone();
        let start_x = start_x.clone();
        drag.connect_drag_begin(move |_, x, y| {
            let w = a.width().max(1) as f64;
            start_x.set(x);
            if y < RULER_H + wave_h(&app) {
                in_wave.set(true);
                app.seek_fraction(x / w);
            } else {
                in_wave.set(false);
                if !app.song_mode.get() {
                    return;
                }
                let lane = ((y - RULER_H - wave_h(&app)) / (lane_h(&app) + LANE_GAP)).floor() as usize;
                let reps = app.song_reps.get().max(1);
                let rep = ((x / w) * reps as f64).floor() as u32;
                app.toggle_clip_at(lane, rep);
            }
        });
    }
    {
        let app = app.clone();
        let a = area.clone();
        drag.connect_drag_update(move |_, dx, _| {
            if in_wave.get() {
                app.seek_fraction((start_x.get() + dx) / a.width().max(1) as f64);
            }
        });
    }
    area.add_controller(drag);
    area
}

fn draw_timeline(app: &App, a: &gtk::DrawingArea, cr: &cairo::Context, w: f64) {
    let (wave_h, lane_h) = (wave_h(app), lane_h(app));
    let fg = a.color();
    let (fr, fgc, fb) = (fg.red() as f64, fg.green() as f64, fg.blue() as f64);
    let l = app.loop_len();
    if l == 0 {
        return;
    }
    let reps = app.song_reps_effective().max(1);
    let song_len = reps as u64 * l as u64;
    let st = app.status.borrow();
    let pos = st.pos.min(song_len.saturating_sub(1));
    let tracks = app.tracks.borrow();
    let any_solo = tracks.iter().any(|t| t.solo);
    let maps: Vec<Option<Vec<i32>>> = tracks.iter().map(|t| app.track_map(t)).collect();
    let envs: Vec<Option<Vec<(f32, f32)>>> = tracks.iter().map(|t| app.track_env(t)).collect();

    // ruler
    cr.set_font_size(10.0);
    let label_every = ((reps as f64 * 22.0 / w).ceil() as u32).max(1);
    for r in 0..reps {
        let x = (r as f64 / reps as f64 * w).round();
        cr.set_source_rgba(fr, fgc, fb, if r % label_every == 0 { 0.35 } else { 0.15 });
        cr.rectangle(x, 4.0, 1.0, RULER_H + wave_h - 4.0);
        let _ = cr.fill();
        if r % label_every == 0 && reps > 1 {
            cr.set_source_rgba(fr, fgc, fb, 0.6);
            cr.move_to(x + 3.0, 12.0);
            let _ = cr.show_text(&(r + 1).to_string());
        }
    }

    // summed envelope of everything audible
    let cols = (w / 3.0).floor().max(1.0) as usize;
    let mut env = vec![0f32; cols];
    for (c, e) in env.iter_mut().enumerate() {
        let p = ((c as f64 + 0.5) / cols as f64 * song_len as f64) as u64;
        for ((t, map), fade) in tracks.iter().zip(&maps).zip(&envs) {
            if t.mute || (any_solo && !t.solo) {
                continue;
            }
            let len = t.processed.len();
            if let Some(i) = track_index(p, l, len, map.as_deref()) {
                let peaks = t.row.wave.peaks.borrow();
                if !peaks.is_empty() {
                    let f = fade.as_deref().map_or(1.0, |f| crate::audio::looper::env_gain(p, l, f));
                    *e += peaks[(i * peaks.len() / len.max(1)).min(peaks.len() - 1)] * t.gain * f;
                }
            }
        }
    }
    let max = env.iter().cloned().fold(0.05f32, f32::max) as f64;
    let mid = RULER_H + wave_h / 2.0;
    let play_x = pos as f64 / song_len as f64 * w;
    for (c, e) in env.iter().enumerate() {
        let x = c as f64 * 3.0;
        let hh = ((*e as f64 / max) * (wave_h / 2.0 - 4.0)).max(0.75);
        if x < play_x {
            cr.set_source_rgba(0.21, 0.52, 0.89, 0.95);
        } else {
            cr.set_source_rgba(fr, fgc, fb, 0.35);
        }
        cr.rectangle(x, mid - hh, 2.0, hh * 2.0);
        let _ = cr.fill();
    }

    // lanes
    if app.song_mode.get() {
        for (i, ((t, map), fade)) in tracks.iter().zip(&maps).zip(&envs).enumerate() {
            let y = RULER_H + wave_h + 4.0 + i as f64 * (lane_h + LANE_GAP);
            cr.set_source_rgba(fr, fgc, fb, 0.06);
            cr.rectangle(0.0, y, w, lane_h);
            let _ = cr.fill();
            let (r, g, b) = TRACK_COLORS[(t.id.max(1) as usize - 1) % TRACK_COLORS.len()];
            let dim = t.mute || (any_solo && !t.solo);
            let map = map.as_deref().unwrap_or(&[]);
            let mut rep = 0usize;
            while rep < map.len() {
                let s = map[rep];
                if s < 0 {
                    rep += 1;
                    continue;
                }
                let mut end = rep;
                while end < map.len() && map[end] == s {
                    end += 1;
                }
                let x0 = rep as f64 / reps as f64 * w + 1.0;
                let x1 = end as f64 / reps as f64 * w - 1.0;
                // the wrapped-around end of a clip that started near the song end: lighter
                let wrapped = s as usize > rep;
                cr.set_source_rgba(
                    r,
                    g,
                    b,
                    match (dim, wrapped) {
                        (true, _) => 0.3,
                        (false, true) => 0.5,
                        (false, false) => 0.85,
                    },
                );
                rounded(cr, x0, y + 1.0, (x1 - x0).max(2.0), lane_h - 2.0, 5.0);
                match fade.as_deref() {
                    None => {
                        let _ = cr.fill();
                    }
                    Some(f) => {
                        // faded: the clip's shape follows its volume, the rest is a faint outline
                        cr.save().ok();
                        cr.clip();
                        cr.paint_with_alpha(0.3).ok();
                        let (top, bottom) = (y + 1.0, y + lane_h - 1.0);
                        let level = |g: f32| bottom - (bottom - top) * g as f64;
                        let xr = |k: usize| k as f64 / reps as f64 * w;
                        cr.move_to(xr(rep), bottom);
                        for (k, &(g0, g1)) in f.iter().enumerate().take(end).skip(rep) {
                            cr.line_to(xr(k), level(g0));
                            cr.line_to(xr(k + 1), level(g1));
                        }
                        cr.line_to(xr(end), bottom);
                        cr.close_path();
                        let _ = cr.fill();
                        cr.restore().ok();
                    }
                }
                rep = end;
            }
            cr.set_source_rgba(fr, fgc, fb, 0.75);
            cr.move_to(4.0, y + lane_h - 7.0);
            let _ = cr.show_text(&t.name);
        }
    }

    // playhead
    cr.set_source_rgba(0.88, 0.11, 0.14, 1.0);
    let h_total =
        RULER_H + wave_h + if app.song_mode.get() { tracks.len() as f64 * (lane_h + LANE_GAP) + 4.0 } else { 0.0 };
    cr.rectangle(play_x.round() - 1.0, 2.0, 2.0, h_total - 2.0);
    let _ = cr.fill();
    cr.arc(play_x.round(), 4.0, 4.0, 0.0, std::f64::consts::TAU);
    let _ = cr.fill();
}

fn rounded(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    use std::f64::consts::PI;
    let r = r.min(h / 2.0).min(w / 2.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -PI / 2.0, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, PI / 2.0);
    cr.arc(x + r, y + h - r, r, PI / 2.0, PI);
    cr.arc(x + r, y + r, r, PI, 1.5 * PI);
    cr.close_path();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_and_clip_toggling() {
        assert_eq!(build_map(None, 1, 4), vec![0, 1, 2, 3]);
        assert_eq!(build_map(None, 2, 5), vec![0, 0, 2, 2, 4]);
        // solo (3 reps long) at repetitions 2–4 (0-based 1..=3) of a 12-rep song
        let m = build_map(Some(&[1]), 3, 12);
        assert_eq!(&m[..6], &[-1, 1, 1, 1, -1, -1]);

        let mut clips = vec![0, 2, 4];
        toggle_clip(&mut clips, 2, 3, 12); // removes the clip starting at 2
        assert_eq!(clips, vec![0, 4]);
        toggle_clip(&mut clips, 2, 3, 12); // place at 3 → overlaps clip at 4 → it's removed
        assert_eq!(clips, vec![0, 3]);

        // a 3-rep solo placed at the last repetition wraps to reps 0 and 1
        assert_eq!(build_map(Some(&[5]), 3, 6), vec![5, 5, -1, -1, -1, 5]);
        // the wrapped tail never hides a clip that starts there
        assert_eq!(build_map(Some(&[0, 5]), 3, 6), vec![0, 0, 0, -1, -1, 5]);
        let mut clips = vec![0];
        toggle_clip(&mut clips, 2, 5, 6); // would cover 5 and 0 → replaces the clip at 0
        assert_eq!(clips, vec![5]);
        toggle_clip(&mut clips, 2, 0, 6); // clicking the wrapped part removes it
        assert!(clips.is_empty());
        assert!(clip_covers(5, 2, 0, 6) && !clip_covers(5, 2, 1, 6));
    }

    #[test]
    fn fades_follow_the_stretches_a_track_plays_in() {
        assert_eq!(fade_envelope(&[0, 1, 2], 0, 0), None);
        // plays in reps 1..=4 (and 6): fade out over 2 reps at the end of each stretch
        let map = [-1, 1, 2, 3, 4, -1, 6];
        let e = fade_envelope(&map, 0, 2).unwrap();
        assert_eq!(e[1], (1.0, 1.0));
        assert_eq!(e[3], (1.0, 0.5));
        assert_eq!(e[4], (0.5, 0.0));
        assert_eq!(e[6], (0.5, 0.0), "a 1-rep stretch with a 2-rep fade starts at half volume");
        let e = fade_envelope(&map, 1, 0).unwrap();
        assert_eq!(e[1], (0.0, 1.0));
        assert_eq!(e[2], (1.0, 1.0));
        assert_eq!(crate::audio::looper::env_gain(1500, 1000, &fade_envelope(&map, 0, 2).unwrap()), 1.0);
        assert_eq!(crate::audio::looper::env_gain(4500, 1000, &fade_envelope(&map, 0, 2).unwrap()), 0.25);
    }

    #[test]
    fn takes_keep_playing_order() {
        // 2-loop take (loop = 1000) started 300 samples into a repetition: recorded circularly,
        // so raw[300] is the first played sample and raw[0..300] holds the end of the take
        let played: Vec<f32> = (1..=2000).map(|v| v as f32).collect();
        let mut raw = vec![0f32; 2000];
        for (i, v) in played.iter().enumerate() {
            raw[(300 + i) % 2000] = *v;
        }
        let out = linearize_take(&raw, 300, 300, 1000);
        assert_eq!(out.len(), 3000, "grows by one repetition instead of wrapping");
        assert!(out[..300].iter().all(|v| *v == 0.0), "silence before the first note");
        assert_eq!(out[300 + 1000], 1001.0, "middle of the take in order");
        assert_eq!(out[300 + 1500], 1501.0);
        assert!(out[2300..].iter().all(|v| *v == 0.0), "silence after the end");
        // started exactly on a repetition: no growth
        assert_eq!(linearize_take(&played, 0, 0, 1000).len(), 2000);
    }
}
