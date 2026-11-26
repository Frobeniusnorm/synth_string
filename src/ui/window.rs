//! Builds the main window and wires widgets to the [`App`] controller.

use super::app::{App, Ui};
use super::widgets::{MeterState, RingState, loop_ring, meter};
use adw::prelude::*;
use glib::translate::IntoGlib;
use gtk::{gdk, gio, glib};
use std::rc::Rc;

pub fn build(app: &Rc<App>, gtk_app: &adw::Application) {
    let cfg = app.cfg.borrow().clone();

    // ---------------------------------------------------------------- header
    let title = adw::WindowTitle::new("Synth String", "Guitar looper");
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&title));

    let device_label = gtk::Label::new(Some("…"));
    let dev_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    dev_box.append(&gtk::Image::from_icon_name("audio-input-microphone-symbolic"));
    dev_box.append(&device_label);
    let (dev_popover, input_combo, output_combo, monitor_switch, nr_switch, popover_meter) = device_popover(app);
    let device_button =
        gtk::MenuButton::builder().child(&dev_box).popover(&dev_popover).css_classes(["device-pill", "flat"]).build();
    header.pack_start(&device_button);

    let menu = gio::Menu::new();
    let sec0 = gio::Menu::new();
    sec0.append(Some("New Project"), Some("app.new"));
    sec0.append(Some("Open Project…"), Some("app.open"));
    sec0.append(Some("Save Project"), Some("app.save"));
    sec0.append(Some("Save Project As…"), Some("app.save-as"));
    sec0.append(Some("Import Audio File…"), Some("app.import"));
    menu.append_section(None, &sec0);
    let sec1 = gio::Menu::new();
    sec1.append(Some("Clear All Tracks"), Some("app.clear"));
    sec1.append(Some("Learn Noise Profile"), Some("app.learn-noise"));
    menu.append_section(None, &sec1);
    let sec2 = gio::Menu::new();
    sec2.append(Some("Preferences"), Some("app.preferences"));
    sec2.append(Some("Keyboard Shortcuts"), Some("app.shortcuts"));
    sec2.append(Some("About Synth String"), Some("app.about"));
    menu.append_section(None, &sec2);
    let menu_btn = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .menu_model(&menu)
        .primary(true)
        .tooltip_text("Main menu")
        .build();
    header.pack_end(&menu_btn);

    let export_btn = gtk::Button::builder()
        .icon_name("document-save-symbolic")
        .tooltip_text("Export (Ctrl+E)")
        .action_name("app.export")
        .build();
    header.pack_end(&export_btn);
    let undo_btn = gtk::Button::builder()
        .icon_name("edit-undo-symbolic")
        .tooltip_text("Undo last layer (Ctrl+Z)")
        .action_name("app.undo")
        .build();
    header.pack_end(&undo_btn);

    // ---------------------------------------------------------------- hero / transport
    let ring_state = Rc::new(RingState::default());
    let ring = loop_ring(ring_state.clone());
    let record_icon = gtk::Image::from_icon_name("media-record-symbolic");
    record_icon.set_pixel_size(40);
    let record_btn = gtk::Button::builder()
        .child(&record_icon)
        .css_classes(["record-button", "circular"])
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .tooltip_text("Record (Space)")
        .build();
    record_btn.set_focus_on_click(false);
    {
        let app = app.clone();
        record_btn.connect_clicked(move |_| app.record());
    }
    let ring_overlay = gtk::Overlay::new();
    ring_overlay.set_child(Some(&ring));
    ring_overlay.add_overlay(&record_btn);
    ring_overlay.set_halign(gtk::Align::Center);

    let play_btn = gtk::Button::builder()
        .icon_name("media-playback-start-symbolic")
        .css_classes(["circular", "play-button"])
        .valign(gtk::Align::Center)
        .tooltip_text("Play / Pause (Enter)")
        .build();
    play_btn.set_focus_on_click(false);
    {
        let app = app.clone();
        play_btn.connect_clicked(move |_| app.play_pause());
    }

    let status_title =
        gtk::Label::builder().label("Press Space to record").xalign(0.0).css_classes(["big-status"]).wrap(true).build();
    let status_sub = gtk::Label::builder().label("").xalign(0.0).wrap(true).css_classes(["dim-label"]).build();

    let meter_state = Rc::new(MeterState::default());
    meter_state.level_db.set(-90.0);
    let meter_widget = {
        let app = app.clone();
        meter(meter_state.clone(), move |db| app.set_manual_threshold(db))
    };
    let threshold_label = gtk::Label::builder().xalign(0.0).css_classes(["caption", "dim-label"]).build();

    // metronome bar
    let metro_toggle = gtk::ToggleButton::builder().label("Click").tooltip_text("Metronome (M)").build();
    metro_toggle.add_css_class("metro-toggle");
    let bpm_spin = gtk::SpinButton::with_range(40.0, 240.0, 1.0);
    bpm_spin.set_value(cfg.bpm as f64);
    bpm_spin.set_tooltip_text(Some("Tempo (BPM)"));
    let tap = gtk::Button::builder().label("Tap").tooltip_text("Tap the tempo (T)").build();
    {
        let app = app.clone();
        tap.connect_clicked(move |_| app.tap_tempo());
    }
    let beats_dd = gtk::DropDown::from_strings(&["2/4", "3/4", "4/4", "5/4", "6/8", "7/8"]);
    beats_dd.set_selected(match cfg.beats_per_bar {
        2 => 0,
        3 => 1,
        5 => 3,
        6 => 4,
        7 => 5,
        _ => 2,
    });
    beats_dd.set_tooltip_text(Some("Beats per bar"));
    let beat_dots = beat_dots_widget(app.clone(), ring_state.clone());

    let metro_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    metro_box.append(&metro_toggle);
    let bpm_box = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    bpm_box.add_css_class("linked");
    bpm_box.append(&bpm_spin);
    bpm_box.append(&tap);
    metro_box.append(&bpm_box);
    metro_box.append(&beats_dd);
    metro_box.append(&beat_dots);
    let drums_toggle =
        gtk::ToggleButton::builder().label("Drums").tooltip_text("Drum machine – play with A S D").build();
    drums_toggle.add_css_class("metro-toggle");
    metro_box.append(&drums_toggle);

    let mult_labels: Vec<String> = super::app::MULTIPLIERS.iter().map(|m| format!("{m}× loop")).collect();
    let mult_refs: Vec<&str> = mult_labels.iter().map(|s| s.as_str()).collect();
    let mult_dd = gtk::DropDown::from_strings(&mult_refs);
    mult_dd.set_selected(super::app::MULTIPLIERS.iter().position(|&m| m == cfg.overdub_multiplier).unwrap_or(0) as u32);
    mult_dd.set_tooltip_text(Some("Length of new layers"));
    mult_dd.set_halign(gtk::Align::End);
    mult_dd.set_hexpand(true);
    metro_box.append(&mult_dd);

    {
        let app2 = app.clone();
        metro_toggle.connect_toggled(move |_| app2.push_metronome());
        let app2 = app.clone();
        bpm_spin.connect_value_changed(move |s| {
            if app2.syncing.get() {
                return;
            }
            app2.cfg.borrow_mut().bpm = s.value() as f32;
            app2.save_cfg();
            app2.push_metronome();
        });
        let app2 = app.clone();
        beats_dd.connect_selected_notify(move |d| {
            app2.cfg.borrow_mut().beats_per_bar = [2, 3, 4, 5, 6, 7][d.selected().min(5) as usize];
            app2.save_cfg();
            app2.push_metronome();
        });
        let app2 = app.clone();
        mult_dd.connect_selected_notify(move |d| {
            app2.cfg.borrow_mut().overdub_multiplier =
                super::app::MULTIPLIERS[(d.selected() as usize).min(super::app::MULTIPLIERS.len() - 1)];
            app2.save_cfg();
        });
    }

    let text_col = gtk::Box::new(gtk::Orientation::Vertical, 6);
    text_col.set_valign(gtk::Align::Center);
    text_col.set_hexpand(true);
    text_col.append(&status_title);
    text_col.append(&status_sub);
    let meter_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    meter_box.set_margin_top(10);
    meter_box.append(&meter_widget);
    meter_box.append(&threshold_label);
    text_col.append(&meter_box);

    let hero_top = gtk::Box::new(gtk::Orientation::Horizontal, 18);
    hero_top.append(&ring_overlay);
    hero_top.append(&play_btn);
    hero_top.append(&text_col);

    let hero = gtk::Box::new(gtk::Orientation::Vertical, 14);
    hero.add_css_class("card");
    hero.add_css_class("hero");
    hero.append(&hero_top);
    hero.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    hero.append(&metro_box);

    // ---------------------------------------------------------------- drum machine
    let drum = super::extras::drum_panel(app);
    {
        let rev = drum.card.clone();
        let app2 = app.clone();
        drums_toggle.connect_toggled(move |b| {
            rev.set_reveal_child(b.is_active());
            if !b.is_active() {
                app2.finish_drums();
            }
            app2.refresh_drum_panel();
        });
    }

    // ---------------------------------------------------------------- timeline / song
    let timeline = super::song::timeline(app);
    let song_switch = gtk::Switch::builder()
        .valign(gtk::Align::Center)
        .tooltip_text("Arrange the loop into a song of repetitions")
        .build();
    let song_reps_spin = gtk::SpinButton::with_range(1.0, 999.0, 1.0);
    song_reps_spin.set_valign(gtk::Align::Center);
    song_reps_spin.set_tooltip_text(Some("Song length in repetitions of the first loop"));
    let song_reps_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    song_reps_box.append(&song_reps_spin);
    song_reps_box.append(&gtk::Label::builder().label("repetitions").css_classes(["dim-label"]).build());
    song_reps_box.set_visible(false);
    let song_len_label = gtk::Label::builder().css_classes(["dim-label", "numeric"]).build();
    {
        let app2 = app.clone();
        song_switch.connect_active_notify(move |s| {
            if !app2.syncing.get() {
                app2.set_song_mode(s.is_active());
            }
        });
        let app2 = app.clone();
        song_reps_spin.connect_value_changed(move |s| {
            if !app2.syncing.get() {
                app2.set_song_reps(s.value() as u32);
            }
        });
    }
    let tl_head = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    tl_head.append(&gtk::Label::builder().label("Timeline").xalign(0.0).css_classes(["heading"]).build());
    tl_head.append(&song_len_label);
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    tl_head.append(&spacer);
    tl_head.append(&song_reps_box);
    tl_head.append(&gtk::Label::new(Some("Song mode")));
    tl_head.append(&song_switch);
    let timeline_hint = gtk::Label::builder().xalign(0.0).wrap(true).css_classes(["caption", "dim-label"]).build();
    let timeline_card = gtk::Box::new(gtk::Orientation::Vertical, 8);
    timeline_card.add_css_class("card");
    timeline_card.add_css_class("timeline-card");
    timeline_card.append(&tl_head);
    timeline_card.append(&timeline);
    timeline_card.append(&timeline_hint);
    timeline_card.set_visible(false);

    // ---------------------------------------------------------------- tracks
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    list.add_css_class("boxed-list");

    let pending_label = gtk::Label::builder().xalign(0.0).hexpand(true).css_classes(["heading"]).build();
    let pending_bar = gtk::ProgressBar::new();
    pending_bar.add_css_class("rec-progress");
    let pending = gtk::Box::new(gtk::Orientation::Vertical, 8);
    pending.add_css_class("card");
    pending.add_css_class("pending-card");
    let pending_head = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    dot.add_css_class("rec-dot");
    dot.set_valign(gtk::Align::Center);
    pending_head.append(&dot);
    pending_head.append(&pending_label);
    let cancel = gtk::Button::builder().label("Cancel").css_classes(["flat"]).build();
    {
        let app = app.clone();
        cancel.connect_clicked(move |_| app.send(crate::audio::looper::Command::CancelRecording));
    }
    pending_head.append(&cancel);
    pending.append(&pending_head);
    pending.append(&pending_bar);
    pending.set_visible(false);

    let tracks_box = gtk::Box::new(gtk::Orientation::Vertical, 12);
    let tracks_header = gtk::Label::builder().label("Tracks").xalign(0.0).css_classes(["heading"]).build();
    tracks_box.append(&tracks_header);
    tracks_box.append(&list);
    tracks_box.append(&pending);

    let empty = adw::StatusPage::builder()
        .icon_name(crate::APP_ID)
        .title("Ready when you are")
        .description("Press <b>Space</b> (or the red button) and just start playing.\nRecording begins with your first note, silence at both ends is trimmed, and every following layer fits the loop automatically.")
        .build();
    let empty_actions = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    empty_actions.set_halign(gtk::Align::Center);
    empty_actions.append(
        &gtk::Button::builder().label("Import Audio File…").action_name("app.import").css_classes(["pill"]).build(),
    );
    empty_actions
        .append(&gtk::Button::builder().label("Open Project…").action_name("app.open").css_classes(["pill"]).build());
    empty.set_child(Some(&empty_actions));
    empty.add_css_class("compact");

    let stack = gtk::Stack::new();
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.add_named(&empty, Some("empty"));
    stack.add_named(&tracks_box, Some("tracks"));
    stack.set_visible_child_name("empty");

    let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
    content.set_margin_top(18);
    content.set_margin_bottom(24);
    content.set_margin_start(12);
    content.set_margin_end(12);
    content.append(&hero);
    content.append(&drum.card);
    content.append(&timeline_card);
    content.append(&stack);

    let clamp = adw::Clamp::builder().maximum_size(920).tightening_threshold(700).child(&content).build();
    let scroller =
        gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).child(&clamp).vexpand(true).build();
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&scroller));

    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&toasts));

    let window = adw::ApplicationWindow::builder()
        .application(gtk_app)
        .title("Synth String")
        .icon_name("dev.synthstring.SynthString")
        .default_width(940)
        .default_height(720)
        .width_request(620)
        .height_request(480)
        .content(&view)
        .build();

    install_keys(app, &window);

    let ui = Ui {
        window: window.clone(),
        toasts,
        title,
        device_label,
        device_button,
        record_btn,
        record_icon,
        play_btn,
        status_title,
        status_sub,
        meter: meter_widget,
        meter_state,
        threshold_label,
        ring,
        ring_state,
        beat_dots,
        metro_toggle,
        bpm_spin,
        mult_dd,
        list,
        stack,
        pending,
        pending_label,
        pending_bar,
        input_combo,
        output_combo,
        monitor_switch,
        nr_switch,
        popover_meter,
        undo_btn,
        export_btn,
        timeline_card,
        timeline,
        timeline_hint,
        song_switch,
        song_reps_spin,
        song_reps_box,
        song_len_label,
        drums_toggle,
        drum_pads: drum.pads,
        drum_record: drum.record,
        drum_status: drum.status,
        drum_bars: drum.bars,
        drum_bars_box: drum.bars_box,
    };
    let _ = app.ui.set(ui);
    app.refresh_drum_panel();
    {
        // ask before losing unsaved work
        let app = app.clone();
        window.connect_close_request(move |w| {
            if !app.dirty.get() || app.tracks.borrow().is_empty() {
                return glib::Propagation::Proceed;
            }
            let w = w.clone();
            let app2 = app.clone();
            app.confirm_discard(Rc::new(move || {
                app2.dirty.set(false);
                w.close();
            }));
            glib::Propagation::Stop
        });
    }
    app.ui().undo_btn.set_sensitive(false);
    window.present();
}

/// Little beat indicator dots next to the metronome.
fn beat_dots_widget(app: Rc<App>, _ring: Rc<RingState>) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::builder().content_width(96).content_height(16).valign(gtk::Align::Center).build();
    area.set_draw_func(move |a, cr, w, h| {
        let st = app.status.borrow();
        let Some((beat, n)) = st.beat else { return };
        let n = n.clamp(1, 8);
        let c = a.color();
        let spacing = (w as f64 / n as f64).min(16.0);
        for i in 0..n {
            let x = 8.0 + i as f64 * spacing;
            let on = i == beat;
            let r = if i == 0 { 5.0 } else { 4.0 };
            if on {
                if i == 0 {
                    cr.set_source_rgb(0.88, 0.11, 0.14);
                } else {
                    cr.set_source_rgb(0.21, 0.52, 0.89);
                }
            } else {
                cr.set_source_rgba(c.red() as f64, c.green() as f64, c.blue() as f64, 0.2);
            }
            cr.arc(x, h as f64 / 2.0, r, 0.0, std::f64::consts::TAU);
            cr.fill().ok();
        }
    });
    area
}

type DevicePopover = (gtk::Popover, adw::ComboRow, adw::ComboRow, adw::SwitchRow, adw::SwitchRow, gtk::DrawingArea);

fn device_popover(app: &Rc<App>) -> DevicePopover {
    let group = adw::PreferencesGroup::new();
    let input_combo = adw::ComboRow::builder().title("Input").subtitle("Your guitar").build();
    let output_combo = adw::ComboRow::builder().title("Output").subtitle("Where you listen").build();
    let monitor_switch = adw::SwitchRow::builder().title("Live monitoring").build();
    let nr_switch = adw::SwitchRow::builder().title("Noise reduction").build();
    group.add(&input_combo);
    group.add(&output_combo);
    group.add(&monitor_switch);
    group.add(&nr_switch);

    {
        let app = app.clone();
        input_combo.connect_selected_notify(move |c| {
            if app.syncing.get() {
                return;
            }
            let entries = app.source_entries();
            if let Some((name, _)) = entries.get(c.selected() as usize) {
                app.select_input(name.clone());
            }
        });
    }
    {
        let app = app.clone();
        output_combo.connect_selected_notify(move |c| {
            if app.syncing.get() {
                return;
            }
            let entries = app.sink_entries();
            if let Some((name, _)) = entries.get(c.selected() as usize) {
                app.select_output(name.clone());
            }
        });
    }
    {
        let app = app.clone();
        monitor_switch.connect_active_notify(move |s| {
            if app.syncing.get() {
                return;
            }
            app.cfg.borrow_mut().monitoring = Some(s.is_active());
            app.save_cfg();
            app.apply_monitoring();
        });
    }
    {
        let app = app.clone();
        nr_switch.connect_active_notify(move |s| {
            if app.syncing.get() {
                return;
            }
            app.set_noise_reduction(Some(s.is_active()));
        });
    }

    // a mirror of the main meter for checking levels while choosing devices
    let pm_state = Rc::new(MeterState::default());
    let popover_meter = gtk::DrawingArea::builder().content_height(10).hexpand(true).build();
    {
        let app = app.clone();
        popover_meter.set_draw_func(move |a, cr, w, h| {
            let Some(ui) = app.ui.get() else { return };
            let lvl = super::widgets::db_to_frac(ui.meter_state.level_db.get());
            let c = a.color();
            cr.set_source_rgba(c.red() as f64, c.green() as f64, c.blue() as f64, 0.12);
            cr.rectangle(0.0, 0.0, w as f64, h as f64);
            cr.fill().ok();
            cr.set_source_rgb(0.20, 0.82, 0.48);
            cr.rectangle(0.0, 0.0, w as f64 * lvl, h as f64);
            cr.fill().ok();
        });
    }
    let _ = pm_state;

    let box_ = gtk::Box::new(gtk::Orientation::Vertical, 10);
    box_.set_margin_top(12);
    box_.set_margin_bottom(12);
    box_.set_margin_start(12);
    box_.set_margin_end(12);
    box_.set_width_request(380);
    box_.append(&gtk::Label::builder().label("Audio devices").xalign(0.0).css_classes(["title-4"]).build());
    box_.append(
        &gtk::Label::builder()
            .label("Picked automatically – instrument interfaces are preferred. Hot-plugging works.")
            .xalign(0.0)
            .wrap(true)
            .max_width_chars(40)
            .css_classes(["caption", "dim-label"])
            .build(),
    );
    box_.append(&group);
    let lvl_label = gtk::Label::builder().label("Input level").xalign(0.0).css_classes(["caption"]).build();
    box_.append(&lvl_label);
    box_.append(&popover_meter);
    let more = gtk::Button::builder()
        .label("More audio settings…")
        .css_classes(["flat"])
        .action_name("app.preferences")
        .build();
    box_.append(&more);

    let pop = gtk::Popover::builder().child(&box_).build();
    (pop, input_combo, output_combo, monitor_switch, nr_switch, popover_meter)
}

fn install_keys(app: &Rc<App>, window: &adw::ApplicationWindow) {
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    // held drum keys (ignore keyboard auto-repeat)
    let held: Rc<std::cell::RefCell<std::collections::HashSet<u32>>> = Rc::default();
    {
        let held = held.clone();
        keys.connect_key_released(move |_, key, _, _| {
            held.borrow_mut().remove(&key.to_lower().into_glib());
        });
    }
    let app = app.clone();
    let win = window.clone();
    keys.connect_key_pressed(move |_, key, _, state| {
        // let text fields (BPM spin button etc.) receive their keys
        if let Some(f) = gtk::prelude::GtkWindowExt::focus(&win)
            && (f.is::<gtk::Text>() || f.is::<gtk::Editable>())
        {
            return glib::Propagation::Proceed;
        }
        if state.intersects(gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK) {
            return glib::Propagation::Proceed;
        }
        // drum pads while the drum machine is open
        if app.ui.get().is_some_and(|u| u.drums_toggle.is_active()) {
            let drum = match key.to_lower() {
                gdk::Key::a | gdk::Key::j => Some(0),
                gdk::Key::s | gdk::Key::k => Some(1),
                gdk::Key::d | gdk::Key::l => Some(2),
                _ => None,
            };
            if let Some(d) = drum {
                if held.borrow_mut().insert(key.to_lower().into_glib()) {
                    app.drum_pad(d);
                }
                return glib::Propagation::Stop;
            }
        }
        match key {
            gdk::Key::space => app.record(),
            gdk::Key::Return | gdk::Key::KP_Enter => app.play_pause(),
            gdk::Key::m | gdk::Key::M => app.toggle_metronome(),
            gdk::Key::t | gdk::Key::T => app.tap_tempo(),
            gdk::Key::Escape => {
                if app.status.borrow().rec != crate::audio::looper::RecState::Idle {
                    app.send(crate::audio::looper::Command::CancelRecording);
                } else {
                    return glib::Propagation::Proceed;
                }
            }
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    });
    window.add_controller(keys);
}
