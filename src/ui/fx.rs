//! Per-track effects popover.

use crate::audio::effects::TrackFx;
use gtk::prelude::*;
use std::cell::RefCell;
use std::rc::Rc;

fn section(title: &str, subtitle: &str) -> (gtk::Box, gtk::Box) {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let labels = gtk::Box::new(gtk::Orientation::Vertical, 0);
    labels.set_hexpand(true);
    let t = gtk::Label::builder().label(title).xalign(0.0).css_classes(["heading"]).build();
    let s = gtk::Label::builder().label(subtitle).xalign(0.0).css_classes(["caption", "dim-label"]).build();
    labels.append(&t);
    labels.append(&s);
    head.append(&labels);
    outer.append(&head);
    (outer, head)
}

fn slider(label: &str, min: f64, max: f64, val: f64, f: impl Fn(f64) + 'static) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let l = gtk::Label::builder().label(label).xalign(0.0).width_chars(9).css_classes(["caption"]).build();
    let s = gtk::Scale::with_range(gtk::Orientation::Horizontal, min, max, (max - min) / 100.0);
    s.set_value(val);
    s.set_hexpand(true);
    s.set_draw_value(false);
    s.connect_value_changed(move |s| f(s.value()));
    b.append(&l);
    b.append(&s);
    b
}

/// Callback that switches one effect on or off.
type SetFlag = Box<dyn Fn(&mut TrackFx, bool)>;

/// Build the FX popover content. `changed` is invoked with the new settings on every edit.
/// `raw_loops`: length of the raw take in loops (slowing down multiplies it; capped at 16 loops).
pub fn fx_content(
    initial: TrackFx,
    bpm: f32,
    raw_loops: u32,
    changed: impl Fn(TrackFx) + 'static,
) -> gtk::ScrolledWindow {
    let fx = Rc::new(RefCell::new(initial.clone()));
    let changed = Rc::new(changed);
    let emit = {
        let fx = fx.clone();
        let changed = changed.clone();
        Rc::new(move || changed(fx.borrow().clone()))
    };
    macro_rules! edit {
        ($body:expr) => {{
            let fx = fx.clone();
            let emit = emit.clone();
            move |v| {
                #[allow(clippy::redundant_closure_call)]
                ($body)(&mut *fx.borrow_mut(), v);
                emit();
            }
        }};
    }

    let content = gtk::Box::new(gtk::Orientation::Vertical, 14);
    content.set_margin_top(12);
    content.set_margin_bottom(12);
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.set_width_request(320);

    // ---- Speed
    let (speed, _) = section("Speed", "Same pitch · faster repeats to fill the slot, slower makes the track longer");
    let sseg = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    sseg.add_css_class("linked");
    sseg.set_homogeneous(true);
    let mut sfirst: Option<gtk::ToggleButton> = None;
    for (sp, label) in [(0.25f32, "¼×"), (0.5, "½×"), (1.0, "1×"), (2.0, "2×"), (3.0, "3×"), (4.0, "4×")] {
        let b = gtk::ToggleButton::with_label(label);
        if sp < 1.0 {
            let loops = raw_loops.max(1) * (1.0 / sp).round() as u32;
            let what = if sp == 0.5 { "Half" } else { "Quarter" };
            b.set_tooltip_text(Some(&format!("{what} speed – the track becomes {loops}× the loop")));
            b.set_sensitive(loops <= 16 || (initial.speed - sp).abs() < 0.01);
        }
        if let Some(f) = &sfirst {
            b.set_group(Some(f));
        } else {
            sfirst = Some(b.clone());
        }
        b.set_active((initial.speed - sp).abs() < 0.01 || (sp == 1.0 && initial.speed <= 0.0));
        let f = edit!(move |fx: &mut TrackFx, _v: ()| fx.speed = sp);
        b.connect_toggled(move |b| {
            if b.is_active() {
                f(())
            }
        });
        sseg.append(&b);
    }
    speed.append(&sseg);
    content.append(&speed);

    // ---- Octave
    let (oct, _) = section("Octave", "Shift the whole track up or down");
    let seg = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    seg.add_css_class("linked");
    seg.set_homogeneous(true);
    let mut first: Option<gtk::ToggleButton> = None;
    for o in [-2, -1, 0, 1, 2] {
        let label = match o {
            0 => "Off".to_string(),
            _ => format!("{o:+}"),
        };
        let b = gtk::ToggleButton::with_label(&label);
        if let Some(f) = &first {
            b.set_group(Some(f));
        } else {
            first = Some(b.clone());
        }
        b.set_active(initial.octave == o);
        let f = edit!(move |fx: &mut TrackFx, _v: ()| fx.octave = o);
        b.connect_toggled(move |b| {
            if b.is_active() {
                f(())
            }
        });
        seg.append(&b);
    }
    oct.append(&seg);
    oct.append(&slider(
        "Blend",
        0.0,
        1.0,
        initial.octave_blend as f64,
        edit!(|fx: &mut TrackFx, v: f64| fx.octave_blend = v as f32),
    ));
    content.append(&oct);

    // switch + sliders helper
    let toggled_section = |title: &str, sub: &str, active: bool, set: SetFlag, sliders: Vec<gtk::Box>| {
        let (sec, head) = section(title, sub);
        let sw = gtk::Switch::builder().active(active).valign(gtk::Align::Center).build();
        head.append(&sw);
        let body = gtk::Box::new(gtk::Orientation::Vertical, 4);
        for s in sliders {
            body.append(&s);
        }
        body.set_sensitive(active);
        sec.append(&body);
        let fx = fx.clone();
        let emit = emit.clone();
        sw.connect_active_notify(move |sw| {
            body.set_sensitive(sw.is_active());
            set(&mut fx.borrow_mut(), sw.is_active());
            emit();
        });
        sec
    };

    // ---- Tone & drive
    let (tone, _) = section("Tone", "Dark ← → Bright");
    tone.append(&slider(
        "Tone",
        -1.0,
        1.0,
        initial.tone as f64,
        edit!(|fx: &mut TrackFx, v: f64| fx.tone = if v.abs() < 0.05 { 0.0 } else { v as f32 }),
    ));
    content.append(&tone);

    content.append(&toggled_section(
        "Drive",
        "Warm overdrive",
        initial.drive,
        Box::new(|fx, on| fx.drive = on),
        vec![slider(
            "Amount",
            0.0,
            1.0,
            initial.drive_amount as f64,
            edit!(|fx: &mut TrackFx, v: f64| fx.drive_amount = v as f32),
        )],
    ));

    // ---- Echo, synced to tempo by default
    let beat_ms = 60_000.0 / bpm.max(30.0);
    let echo_time = gtk::DropDown::from_strings(&["1/16", "1/8", "Dotted 1/8", "1/4", "1/2"]);
    let divs = [0.25f32, 0.5, 0.75, 1.0, 2.0];
    let cur = divs
        .iter()
        .enumerate()
        .min_by(|a, b| {
            ((a.1 * beat_ms) - initial.echo_ms).abs().partial_cmp(&((b.1 * beat_ms) - initial.echo_ms).abs()).unwrap()
        })
        .map(|(i, _)| i)
        .unwrap_or(2);
    echo_time.set_selected(cur as u32);
    {
        let f = edit!(move |fx: &mut TrackFx, i: u32| fx.echo_ms = divs[i as usize] * beat_ms);
        echo_time.connect_selected_notify(move |d| f(d.selected()));
    }
    let time_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    time_row.append(&gtk::Label::builder().label("Time").xalign(0.0).width_chars(9).css_classes(["caption"]).build());
    echo_time.set_hexpand(true);
    time_row.append(&echo_time);
    let echo = toggled_section(
        "Echo",
        &format!("Synced to {:.0} BPM", bpm),
        initial.echo,
        Box::new(|fx, on| fx.echo = on),
        vec![
            time_row,
            slider(
                "Repeats",
                0.0,
                0.85,
                initial.echo_feedback as f64,
                edit!(|fx: &mut TrackFx, v: f64| fx.echo_feedback = v as f32),
            ),
            slider("Mix", 0.0, 1.0, initial.echo_mix as f64, edit!(|fx: &mut TrackFx, v: f64| fx.echo_mix = v as f32)),
        ],
    );
    content.append(&echo);

    content.append(&toggled_section(
        "Reverb",
        "Room to hall",
        initial.reverb,
        Box::new(|fx, on| fx.reverb = on),
        vec![
            slider(
                "Size",
                0.0,
                1.0,
                initial.reverb_size as f64,
                edit!(|fx: &mut TrackFx, v: f64| fx.reverb_size = v as f32),
            ),
            slider(
                "Mix",
                0.0,
                1.0,
                initial.reverb_mix as f64,
                edit!(|fx: &mut TrackFx, v: f64| fx.reverb_mix = v as f32),
            ),
        ],
    ));

    content.append(&toggled_section(
        "Reverse",
        "Play the track backwards",
        initial.reverse,
        Box::new(|fx, on| fx.reverse = on),
        vec![],
    ));

    // ---- Fade in / out (a volume curve over repetitions, no re-render)
    let (fade, _) = section(
        "Fade",
        "Song mode: over the first / last repetitions of each stretch the track plays in. Loop mode: in the export",
    );
    const FADE_REPS: [u32; 5] = [0, 1, 2, 4, 8];
    let fade_row = |label: &str, initial: u32, set: fn(&mut TrackFx, u32)| {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        row.append(&gtk::Label::builder().label(label).xalign(0.0).hexpand(true).build());
        let dd =
            gtk::DropDown::from_strings(&["Off", "1 repetition", "2 repetitions", "4 repetitions", "8 repetitions"]);
        dd.set_selected(FADE_REPS.iter().position(|r| *r == initial).unwrap_or(0) as u32);
        let f = edit!(move |fx: &mut TrackFx, i: u32| set(fx, FADE_REPS[(i as usize).min(FADE_REPS.len() - 1)]));
        dd.connect_selected_notify(move |d| f(d.selected()));
        row.append(&dd);
        row
    };
    fade.append(&fade_row("Fade in", initial.fade_in, |fx, r| fx.fade_in = r));
    fade.append(&fade_row("Fade out", initial.fade_out, |fx, r| fx.fade_out = r));
    content.append(&fade);

    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(560)
        .child(&content)
        .build()
}
