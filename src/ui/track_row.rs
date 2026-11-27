//! One row in the track list: colour, name, waveform, FX, mute/solo, volume, actions.

use super::app::{App, TrackKind};
use super::widgets::{TRACK_COLORS, WaveState, waveform};
use crate::audio::effects::TrackFx;
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

pub struct TrackRow {
    pub row: gtk::ListBoxRow,
    pub wave: Rc<WaveState>,
    pub area: gtk::DrawingArea,
    pub fx_label: gtk::Label,
    pub spinner: gtk::Spinner,
    pub fx_button: gtk::MenuButton,
    mute: gtk::ToggleButton,
    solo: gtk::ToggleButton,
    vol: gtk::Scale,
}

impl TrackRow {
    /// Reflect loaded state in the widgets (used when opening a project).
    pub fn set_state(&self, gain: f32, mute: bool, solo: bool, fx: &TrackFx) {
        self.vol.set_value(gain as f64);
        self.mute.set_active(mute);
        self.solo.set_active(solo);
        self.fx_label.set_label(&fx_text(fx, self.wave.cycles.get()));
        if !fx.is_neutral() || fx.has_fade() {
            self.fx_button.add_css_class("fx-active");
        }
    }
}

pub fn build(app: &Rc<App>, id: u32, name: &str, color: usize, fx: &TrackFx, cycles: u32, kind: TrackKind) -> TrackRow {
    let (r, g, b) = TRACK_COLORS[color % TRACK_COLORS.len()];
    let root = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    root.set_margin_top(10);
    root.set_margin_bottom(10);
    root.set_margin_start(12);
    root.set_margin_end(10);

    // colour stripe
    let stripe = gtk::DrawingArea::builder().content_width(6).vexpand(true).build();
    stripe.set_draw_func(move |_, cr, w, h| {
        cr.set_source_rgb(r, g, b);
        let rad = w as f64 / 2.0;
        cr.arc(rad, rad, rad, std::f64::consts::PI, 0.0);
        cr.arc(rad, h as f64 - rad, rad, 0.0, std::f64::consts::PI);
        cr.fill().ok();
    });
    root.append(&stripe);

    // name + fx summary
    let names = gtk::Box::new(gtk::Orientation::Vertical, 2);
    names.set_valign(gtk::Align::Center);
    names.set_width_request(92);
    let title = gtk::Label::builder()
        .label(name)
        .xalign(0.0)
        .width_chars(10)
        .max_width_chars(10)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .css_classes(["heading"])
        .build();
    let fx_label = gtk::Label::builder()
        .label(fx_text(fx, cycles))
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .width_chars(15)
        .max_width_chars(15)
        .css_classes(["caption", "dim-label"])
        .build();
    names.append(&title);
    names.append(&fx_label);
    root.append(&names);

    // waveform (click toggles mute – quick and fun)
    let wave = Rc::new(WaveState {
        peaks: RefCell::new(Vec::new()),
        color: Cell::new((r, g, b)),
        playhead: Cell::new(None),
        dim: Cell::new(false),
        cycles: Cell::new(cycles),
    });
    let area = waveform(wave.clone());
    area.set_valign(gtk::Align::Center);
    area.set_tooltip_text(Some("Click to mute / unmute"));
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&area));
    overlay.set_hexpand(true);
    let spinner =
        gtk::Spinner::builder().halign(gtk::Align::End).valign(gtk::Align::Start).margin_end(6).margin_top(6).build();
    spinner.set_visible(false);
    overlay.add_overlay(&spinner);
    root.append(&overlay);

    // controls
    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    controls.set_valign(gtk::Align::Center);

    let fx_button = gtk::MenuButton::builder()
        .label("FX")
        .tooltip_text("Effects: octave, echo, reverb, …")
        .css_classes(["flat", "fx-button"])
        .build();
    {
        // one persistent popover; its content is rebuilt on every open so it reflects tempo
        let pop = gtk::Popover::new();
        let app = app.clone();
        pop.connect_show(move |p| app.fill_fx(id, p));
        fx_button.set_popover(Some(&pop));
    }
    controls.append(&fx_button);

    let mute = gtk::ToggleButton::builder()
        .label("M")
        .tooltip_text("Mute")
        .css_classes(["flat", "circular", "mute-button"])
        .build();
    let solo = gtk::ToggleButton::builder()
        .label("S")
        .tooltip_text("Solo")
        .css_classes(["flat", "circular", "solo-button"])
        .build();
    {
        let app = app.clone();
        mute.connect_toggled(move |b| app.set_mute(id, b.is_active()));
    }
    {
        let app = app.clone();
        solo.connect_toggled(move |b| app.set_solo(id, b.is_active()));
    }
    controls.append(&mute);
    controls.append(&solo);

    let vol = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.5, 0.01);
    vol.set_value(1.0);
    vol.set_draw_value(false);
    vol.set_width_request(96);
    vol.add_mark(1.0, gtk::PositionType::Bottom, None);
    vol.set_tooltip_text(Some("Volume"));
    {
        let app = app.clone();
        vol.connect_value_changed(move |s| app.set_gain(id, s.value() as f32));
    }
    controls.append(&vol);

    // more menu
    let menu = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let pop = gtk::Popover::builder().child(&menu).build();
    pop.add_css_class("menu");
    let item = |label: &str, icon: &str| {
        let b = gtk::Button::builder().css_classes(["flat"]).build();
        let c = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        c.append(&gtk::Image::from_icon_name(icon));
        c.append(&gtk::Label::new(Some(label)));
        b.set_child(Some(&c));
        b
    };
    let rerec = if kind == TrackKind::Drums {
        item("Add drum hits", "media-record-symbolic")
    } else {
        item("Re-record", "media-record-symbolic")
    };
    let del = item("Delete", "user-trash-symbolic");
    {
        let app = app.clone();
        let pop = pop.clone();
        rerec.connect_clicked(move |_| {
            pop.popdown();
            if kind == TrackKind::Drums {
                app.edit_drums(id);
            } else {
                app.rerecord(id);
            }
        });
    }
    {
        let app = app.clone();
        let pop = pop.clone();
        del.connect_clicked(move |_| {
            pop.popdown();
            app.delete_track(id);
        });
    }
    menu.append(&rerec);
    menu.append(&del);
    let more = gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .popover(&pop)
        .css_classes(["flat", "circular"])
        .tooltip_text("More")
        .build();
    controls.append(&more);
    root.append(&controls);

    let click = gtk::GestureClick::new();
    {
        let mute = mute.clone();
        click.connect_released(move |_, _, _, _| mute.set_active(!mute.is_active()));
    }
    area.add_controller(click);

    let row = gtk::ListBoxRow::builder().child(&root).activatable(false).selectable(false).build();
    TrackRow { row, wave, area, fx_label, spinner, fx_button, mute, solo, vol }
}

pub fn fx_text(fx: &TrackFx, cycles: u32) -> String {
    let mut s = fx.summary();
    if cycles > 1 {
        s = if s.is_empty() { format!("{cycles}× loop") } else { format!("{cycles}× · {s}") };
    }
    if s.is_empty() { "No effects".into() } else { s }
}
