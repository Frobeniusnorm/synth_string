//! Custom-drawn widgets: input meter with threshold marker, loop ring, waveforms.

use gtk::prelude::*;
use gtk::{cairo, gdk};
use std::cell::{Cell, RefCell};
use std::f64::consts::PI;
use std::rc::Rc;

pub const TRACK_COLORS: [(f64, f64, f64); 8] = [
    (0.21, 0.52, 0.89), // blue
    (0.20, 0.82, 0.48), // green
    (1.00, 0.47, 0.00), // orange
    (0.57, 0.25, 0.67), // purple
    (0.96, 0.83, 0.18), // yellow
    (0.13, 0.56, 0.64), // teal
    (0.75, 0.38, 0.80), // pink
    (0.88, 0.11, 0.14), // red
];

fn fg(w: &impl IsA<gtk::Widget>) -> gdk::RGBA {
    w.as_ref().color()
}

fn rounded_rect(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(h / 2.0).min(w / 2.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -PI / 2.0, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, PI / 2.0);
    cr.arc(x + r, y + h - r, r, PI / 2.0, PI);
    cr.arc(x + r, y + r, r, PI, 1.5 * PI);
    cr.close_path();
}

// ------------------------------------------------------------------ meter

pub const METER_MIN_DB: f32 = -66.0;

#[derive(Default)]
pub struct MeterState {
    pub level_db: Cell<f32>,
    pub peak_db: Cell<f32>,
    pub threshold_db: Cell<f32>,
    pub auto_threshold: Cell<bool>,
}

pub fn db_to_frac(db: f32) -> f64 {
    ((db - METER_MIN_DB) / -METER_MIN_DB).clamp(0.0, 1.0) as f64
}

pub fn frac_to_db(f: f64) -> f32 {
    METER_MIN_DB + f.clamp(0.0, 1.0) as f32 * -METER_MIN_DB
}

/// Horizontal input meter. Dragging/clicking sets the record threshold via `on_threshold`.
pub fn meter(state: Rc<MeterState>, on_threshold: impl Fn(Option<f32>) + 'static) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::builder().content_height(22).hexpand(true).build();
    area.set_cursor_from_name(Some("col-resize"));
    area.set_tooltip_text(Some(
        "Input level. The marker is the record threshold – drag to change, double-click for automatic.",
    ));
    let st = state.clone();
    area.set_draw_func(move |a, cr, w, h| {
        let (w, h) = (w as f64, h as f64);
        let c = fg(a);
        let bar_h = 10.0;
        let y = (h - bar_h) / 2.0;
        rounded_rect(cr, 0.0, y, w, bar_h, 5.0);
        cr.set_source_rgba(c.red() as f64, c.green() as f64, c.blue() as f64, 0.12);
        cr.fill().ok();

        let lvl = db_to_frac(st.level_db.get());
        if lvl > 0.0 {
            let grad = cairo::LinearGradient::new(0.0, 0.0, w, 0.0);
            grad.add_color_stop_rgb(0.0, 0.20, 0.78, 0.45);
            grad.add_color_stop_rgb(0.72, 0.20, 0.82, 0.48);
            grad.add_color_stop_rgb(0.86, 0.96, 0.80, 0.18);
            grad.add_color_stop_rgb(1.0, 0.90, 0.15, 0.15);
            rounded_rect(cr, 0.0, y, (w * lvl).max(bar_h), bar_h, 5.0);
            cr.set_source(&grad).ok();
            cr.fill().ok();
        }
        // peak hold tick
        let pk = db_to_frac(st.peak_db.get());
        if pk > 0.0 {
            cr.set_source_rgba(c.red() as f64, c.green() as f64, c.blue() as f64, 0.6);
            cr.rectangle((w * pk - 1.0).max(0.0), y, 2.0, bar_h);
            cr.fill().ok();
        }
        // threshold marker
        let t = db_to_frac(st.threshold_db.get());
        let x = (w * t).round();
        cr.set_source_rgba(c.red() as f64, c.green() as f64, c.blue() as f64, 0.9);
        cr.rectangle(x - 1.0, 1.0, 2.0, h - 2.0);
        cr.fill().ok();
        cr.move_to(x - 4.0, 0.0);
        cr.line_to(x + 4.0, 0.0);
        cr.line_to(x, 5.0);
        cr.close_path();
        cr.fill().ok();
    });

    let on_threshold = Rc::new(on_threshold);
    let drag = gtk::GestureDrag::new();
    let start_x = Rc::new(Cell::new(0.0));
    {
        let a = area.clone();
        let f = on_threshold.clone();
        let sx = start_x.clone();
        drag.connect_drag_begin(move |_, x, _| {
            sx.set(x);
            f(Some(frac_to_db(x / a.width().max(1) as f64)));
        });
    }
    {
        let a = area.clone();
        let f = on_threshold.clone();
        let sx = start_x.clone();
        drag.connect_drag_update(move |_, dx, _| {
            f(Some(frac_to_db((sx.get() + dx) / a.width().max(1) as f64)));
        });
    }
    area.add_controller(drag);
    let click = gtk::GestureClick::new();
    {
        let f = on_threshold.clone();
        click.connect_pressed(move |_, n, _, _| {
            if n == 2 {
                f(None);
            }
        });
    }
    area.add_controller(click);
    area
}

// ------------------------------------------------------------------ loop ring

#[derive(Default)]
pub struct RingState {
    /// Loop progress 0..1 when a loop exists.
    pub progress: Cell<Option<f64>>,
    /// Number of beat ticks to draw around the loop.
    pub beats: Cell<u32>,
    pub beats_per_bar: Cell<u32>,
    /// 0 = idle, 1 = armed (waiting), 2 = recording
    pub rec: Cell<u8>,
    /// Overdub progress 0..1
    pub rec_progress: Cell<Option<f64>>,
    pub level: Cell<f64>,
    pub t: Cell<f64>,
}

pub fn loop_ring(state: Rc<RingState>) -> gtk::DrawingArea {
    let size = 168;
    let area = gtk::DrawingArea::builder().content_width(size).content_height(size).build();
    area.set_draw_func(move |a, cr, w, h| {
        let c = fg(a);
        let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);
        let r = cx.min(cy) - 10.0;
        cr.set_line_cap(cairo::LineCap::Round);
        // track
        cr.set_line_width(8.0);
        cr.set_source_rgba(c.red() as f64, c.green() as f64, c.blue() as f64, 0.1);
        cr.arc(cx, cy, r, 0.0, 2.0 * PI);
        cr.stroke().ok();

        let rec = state.rec.get();
        let t = state.t.get();
        if rec == 1 {
            // armed: a breathing red ring + a level reactive inner glow
            let pulse = 0.35 + 0.35 * (t * 4.0).sin().abs();
            cr.set_source_rgba(0.88, 0.11, 0.14, pulse);
            cr.set_line_width(6.0);
            cr.arc(cx, cy, r, 0.0, 2.0 * PI);
            cr.stroke().ok();
        }
        if let Some(p) = state.progress.get() {
            let (rr, gg, bb) = if rec == 2 { (0.88, 0.11, 0.14) } else { (0.21, 0.52, 0.89) };
            cr.set_source_rgba(rr, gg, bb, 0.95);
            cr.set_line_width(8.0);
            cr.arc(cx, cy, r, -PI / 2.0, -PI / 2.0 + 2.0 * PI * p.max(0.002));
            cr.stroke().ok();
            // beat ticks
            let beats = state.beats.get();
            let bpb = state.beats_per_bar.get().max(1);
            if beats > 1 && beats <= 64 {
                for b in 0..beats {
                    let ang = -PI / 2.0 + 2.0 * PI * b as f64 / beats as f64;
                    let bar = b % bpb == 0;
                    let (r0, r1) = if bar { (r - 16.0, r - 6.0) } else { (r - 12.0, r - 7.0) };
                    cr.set_source_rgba(
                        c.red() as f64,
                        c.green() as f64,
                        c.blue() as f64,
                        if bar { 0.55 } else { 0.25 },
                    );
                    cr.set_line_width(if bar { 2.5 } else { 1.5 });
                    cr.move_to(cx + r0 * ang.cos(), cy + r0 * ang.sin());
                    cr.line_to(cx + r1 * ang.cos(), cy + r1 * ang.sin());
                    cr.stroke().ok();
                }
            }
            // playhead dot
            let ang = -PI / 2.0 + 2.0 * PI * p;
            cr.set_source_rgba(rr, gg, bb, 1.0);
            cr.arc(cx + r * ang.cos(), cy + r * ang.sin(), 7.0, 0.0, 2.0 * PI);
            cr.fill().ok();
        } else if rec == 2 {
            // first take recording: spinning comet
            cr.set_source_rgba(0.88, 0.11, 0.14, 0.9);
            cr.set_line_width(8.0);
            let a0 = t * 3.0;
            cr.arc(cx, cy, r, a0, a0 + PI * 0.6);
            cr.stroke().ok();
        }
        if let Some(p) = state.rec_progress.get() {
            cr.set_source_rgba(0.88, 0.11, 0.14, 0.9);
            cr.set_line_width(4.0);
            cr.arc(cx, cy, r - 14.0, -PI / 2.0, -PI / 2.0 + 2.0 * PI * p.clamp(0.0, 1.0));
            cr.stroke().ok();
        }
        // level glow
        let lvl = state.level.get();
        if lvl > 0.01 {
            cr.set_source_rgba(0.20, 0.82, 0.48, 0.25 * lvl);
            cr.set_line_width(3.0 + 6.0 * lvl);
            cr.arc(cx, cy, r + 6.0, 0.0, 2.0 * PI);
            cr.stroke().ok();
        }
    });
    area
}

// ------------------------------------------------------------------ waveform

pub struct WaveState {
    pub peaks: RefCell<Vec<f32>>,
    pub color: Cell<(f64, f64, f64)>,
    pub playhead: Cell<Option<f64>>,
    pub dim: Cell<bool>,
    /// Number of loop cycles inside this track (1×, 2×, 4×) for divider lines.
    pub cycles: Cell<u32>,
}

pub fn compute_peaks(buf: &[f32], buckets: usize) -> Vec<f32> {
    if buf.is_empty() {
        return vec![0.0; buckets];
    }
    (0..buckets)
        .map(|b| {
            let s = b * buf.len() / buckets;
            let e = ((b + 1) * buf.len() / buckets).max(s + 1).min(buf.len());
            buf[s..e].iter().fold(0f32, |m, v| m.max(v.abs()))
        })
        .collect()
}

pub fn waveform(state: Rc<WaveState>) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::builder().content_height(52).hexpand(true).build();
    area.set_draw_func(move |a, cr, w, h| {
        let c = fg(a);
        let (w, h) = (w as f64, h as f64);
        let (r, g, b) = state.color.get();
        rounded_rect(cr, 0.0, 0.0, w, h, 8.0);
        cr.set_source_rgba(r, g, b, 0.10);
        cr.fill().ok();

        let peaks = state.peaks.borrow();
        let n = peaks.len().max(1);
        let mid = h / 2.0;
        let max = peaks.iter().cloned().fold(0.05f32, f32::max) as f64;
        let alpha = if state.dim.get() { 0.3 } else { 0.9 };
        cr.set_source_rgba(r, g, b, alpha);
        let cols = (w / 3.0).floor().max(1.0) as usize;
        for i in 0..cols {
            let s = i * n / cols;
            let e = ((i + 1) * n / cols).max(s + 1).min(n);
            let p = peaks[s..e].iter().cloned().fold(0f32, f32::max) as f64 / max;
            let hh = (p * (h / 2.0 - 4.0)).max(0.75);
            rounded_rect(cr, i as f64 * 3.0, mid - hh, 2.0, hh * 2.0, 1.0);
        }
        cr.fill().ok();

        let cycles = state.cycles.get();
        if cycles > 1 {
            cr.set_source_rgba(c.red() as f64, c.green() as f64, c.blue() as f64, 0.2);
            for k in 1..cycles {
                let x = (w * k as f64 / cycles as f64).round();
                cr.rectangle(x - 0.5, 4.0, 1.0, h - 8.0);
            }
            cr.fill().ok();
        }
        if let Some(p) = state.playhead.get() {
            let x = (w * p).round();
            cr.set_source_rgba(c.red() as f64, c.green() as f64, c.blue() as f64, 0.85);
            cr.rectangle(x - 1.0, 2.0, 2.0, h - 4.0);
            cr.fill().ok();
        }
    });
    area
}
