//! Preferences dialog.

use super::app::App;
use crate::config::InputChannel;
use crate::midi::MidiAction;
use adw::prelude::*;
use std::rc::Rc;

pub fn show(app: &Rc<App>) {
    let cfg = app.cfg.borrow().clone();
    let dialog = adw::PreferencesDialog::new();
    dialog.set_search_enabled(false);

    // ------------------------------------------------------------ Audio
    let audio = adw::PreferencesPage::builder().title("Audio").icon_name("audio-x-generic-symbolic").build();
    let devices = adw::PreferencesGroup::builder()
        .title("Devices")
        .description("Instrument interfaces are detected automatically. Pick one explicitly to override.")
        .build();
    let input = adw::ComboRow::builder().title("Input").build();
    let output = adw::ComboRow::builder().title("Output").build();
    let fill = |combo: &adw::ComboRow, entries: &[(Option<String>, String)], cur: &Option<String>| {
        let names: Vec<&str> = entries.iter().map(|(_, l)| l.as_str()).collect();
        combo.set_model(Some(&gtk::StringList::new(&names)));
        combo.set_selected(entries.iter().position(|(n, _)| n == cur).unwrap_or(0) as u32);
    };
    let in_entries = app.source_entries();
    let out_entries = app.sink_entries();
    fill(&input, &in_entries, &cfg.input_device);
    fill(&output, &out_entries, &cfg.output_device);
    {
        let app = app.clone();
        input.connect_selected_notify(move |c| {
            if let Some((n, _)) = in_entries.get(c.selected() as usize) {
                app.select_input(n.clone());
            }
        });
    }
    {
        let app = app.clone();
        output.connect_selected_notify(move |c| {
            if let Some((n, _)) = out_entries.get(c.selected() as usize) {
                app.select_output(n.clone());
            }
        });
    }
    let channel = adw::ComboRow::builder()
        .title("Input channel")
        .subtitle("Use Left/Right if your guitar is on one channel only")
        .model(&gtk::StringList::new(&["Both (mix)", "Left", "Right"]))
        .selected(cfg.input_channel as u32)
        .build();
    {
        let app = app.clone();
        channel.connect_selected_notify(move |c| {
            app.cfg.borrow_mut().input_channel = match c.selected() {
                1 => InputChannel::Left,
                2 => InputChannel::Right,
                _ => InputChannel::Mix,
            };
            app.save_cfg();
            app.push_settings();
        });
    }
    devices.add(&input);
    devices.add(&channel);
    devices.add(&output);
    audio.add(&devices);

    let monitor = adw::PreferencesGroup::builder().title("Monitoring").build();
    let mon_mode = adw::ComboRow::builder()
        .title("Live monitoring")
        .subtitle("Automatic turns it off when you listen through the interface itself (it monitors in hardware)")
        .model(&gtk::StringList::new(&["Automatic", "On", "Off"]))
        .selected(match cfg.monitoring {
            None => 0,
            Some(true) => 1,
            Some(false) => 2,
        })
        .build();
    {
        let app = app.clone();
        mon_mode.connect_selected_notify(move |c| {
            app.cfg.borrow_mut().monitoring = match c.selected() {
                1 => Some(true),
                2 => Some(false),
                _ => None,
            };
            app.save_cfg();
            app.apply_monitoring();
        });
    }
    let gate = adw::SwitchRow::builder()
        .title("Noise gate on monitor")
        .subtitle("Silences hum and hiss between notes while you listen")
        .active(cfg.monitor_gate)
        .build();
    {
        let app = app.clone();
        gate.connect_active_notify(move |s| {
            app.cfg.borrow_mut().monitor_gate = s.is_active();
            app.save_cfg();
            app.apply_monitoring();
        });
    }
    monitor.add(&mon_mode);
    monitor.add(&gate);
    audio.add(&monitor);

    let latency = adw::PreferencesGroup::builder()
        .title("Timing")
        .description(format!(
            "Measured round-trip latency: {:.1} ms (compensated automatically for layers)",
            app.status.borrow().latency as f32 * 1000.0 / crate::audio::looper::SR as f32
        ))
        .build();
    let offset = adw::SpinRow::with_range(-100.0, 100.0, 1.0);
    offset.set_title("Extra latency compensation (ms)");
    offset.set_subtitle("Increase if new layers sound late, decrease if early");
    offset.set_value(cfg.latency_offset_ms as f64);
    {
        let app = app.clone();
        offset.connect_value_notify(move |s| {
            app.cfg.borrow_mut().latency_offset_ms = s.value() as f32;
            app.save_cfg();
            app.push_settings();
        });
    }
    latency.add(&offset);
    audio.add(&latency);
    dialog.add(&audio);

    // ------------------------------------------------------------ Recording
    let rec = adw::PreferencesPage::builder().title("Recording").icon_name("media-record-symbolic").build();
    let trig = adw::PreferencesGroup::builder()
        .title("Auto start & trim")
        .description(
            "Recording starts when your signal exceeds the threshold and silence at the end of the first take is cut.",
        )
        .build();
    let auto_thr = adw::SwitchRow::builder()
        .title("Automatic threshold")
        .subtitle("Follows the noise floor of your input")
        .active(cfg.threshold_db.is_none())
        .build();
    let thr = adw::SpinRow::with_range(-60.0, -6.0, 1.0);
    thr.set_title("Threshold (dB)");
    thr.set_value(cfg.threshold_db.unwrap_or(app.threshold_db.get()) as f64);
    thr.set_sensitive(cfg.threshold_db.is_some());
    {
        let app = app.clone();
        let thr = thr.clone();
        auto_thr.connect_active_notify(move |s| {
            thr.set_sensitive(!s.is_active());
            app.set_manual_threshold(if s.is_active() { None } else { Some(thr.value() as f32) });
        });
    }
    {
        let app = app.clone();
        let auto_thr = auto_thr.clone();
        thr.connect_value_notify(move |s| {
            if !auto_thr.is_active() {
                app.set_manual_threshold(Some(s.value() as f32));
            }
        });
    }
    let preroll = adw::SpinRow::with_range(0.0, 300.0, 5.0);
    preroll.set_title("Pre-roll (ms)");
    preroll.set_subtitle("Audio kept before the trigger so the pick attack isn't cut");
    preroll.set_value(cfg.preroll_ms as f64);
    {
        let app = app.clone();
        preroll.connect_value_notify(move |s| {
            app.cfg.borrow_mut().preroll_ms = s.value() as f32;
            app.save_cfg();
            app.push_settings();
        });
    }
    let tail = adw::SpinRow::with_range(0.0, 2000.0, 10.0);
    tail.set_title("Release tail (ms)");
    tail.set_subtitle("Ring-out kept after your last note (free tempo loops)");
    tail.set_value(cfg.release_tail_ms as f64);
    {
        let app = app.clone();
        tail.connect_value_notify(move |s| {
            app.cfg.borrow_mut().release_tail_ms = s.value() as f32;
            app.save_cfg();
            app.push_settings();
        });
    }
    trig.add(&auto_thr);
    trig.add(&thr);
    trig.add(&preroll);
    trig.add(&tail);
    rec.add(&trig);

    let noise = adw::PreferencesGroup::builder()
        .title("Noise reduction")
        .description(format!("Current noise floor: {:.0} dB", app.noise_floor_db.get()))
        .build();
    let nr_mode = adw::ComboRow::builder()
        .title("Noise reduction")
        .subtitle("Automatic enables it when the input is noisy")
        .model(&gtk::StringList::new(&["Automatic", "On", "Off"]))
        .selected(match cfg.noise_reduction {
            None => 0,
            Some(true) => 1,
            Some(false) => 2,
        })
        .build();
    {
        let app = app.clone();
        nr_mode.connect_selected_notify(move |c| {
            app.set_noise_reduction(match c.selected() {
                1 => Some(true),
                2 => Some(false),
                _ => None,
            });
        });
    }
    let strength = adw::SpinRow::with_range(1.0, 4.0, 0.25);
    strength.set_title("Strength");
    strength.set_subtitle("Higher removes more noise but may dull quiet notes");
    strength.set_digits(2);
    strength.set_value(cfg.noise_strength as f64);
    {
        let app = app.clone();
        strength.connect_value_notify(move |s| {
            app.cfg.borrow_mut().noise_strength = s.value() as f32;
            app.save_cfg();
            if app.nr_active.get() {
                let ids: Vec<u32> = app.tracks.borrow().iter().map(|t| t.id).collect();
                for id in ids {
                    app.reprocess(id);
                }
            }
        });
    }
    let learn = adw::ActionRow::builder()
        .title("Learn noise profile")
        .subtitle("Mute your strings, then click – takes one second")
        .build();
    let learn_btn = gtk::Button::builder().label("Learn").valign(gtk::Align::Center).build();
    {
        let app = app.clone();
        learn_btn.connect_clicked(move |_| app.relearn_noise());
    }
    learn.add_suffix(&learn_btn);
    noise.add(&nr_mode);
    noise.add(&strength);
    noise.add(&learn);
    rec.add(&noise);

    let metro = adw::PreferencesGroup::builder().title("Metronome").build();
    let click_vol = adw::SpinRow::with_range(0.0, 1.0, 0.05);
    click_vol.set_title("Click volume");
    click_vol.set_digits(2);
    click_vol.set_value(cfg.click_volume as f64);
    {
        let app = app.clone();
        click_vol.connect_value_notify(move |s| {
            app.cfg.borrow_mut().click_volume = s.value() as f32;
            app.save_cfg();
            app.push_metronome();
        });
    }
    metro.add(&click_vol);
    rec.add(&metro);
    dialog.add(&rec);

    // ------------------------------------------------------------ MIDI
    let midi_page =
        adw::PreferencesPage::builder().title("MIDI").icon_name("preferences-system-devices-symbolic").build();
    let ports = app.midi.ports.lock().unwrap().clone();
    let group = adw::PreferencesGroup::builder()
        .title("Footswitch control")
        .description(if ports.is_empty() {
            "No MIDI device connected.".to_string()
        } else {
            format!("Connected: {}", ports.join(", "))
        })
        .build();
    for action in MidiAction::ALL {
        let row = adw::ActionRow::builder().title(action.label()).build();
        let current = cfg.midi.get(action).map(|t| t.label()).unwrap_or_else(|| "Not mapped".into());
        row.set_subtitle(&current);
        let learn = gtk::Button::builder().label("Learn").valign(gtk::Align::Center).build();
        let clear = gtk::Button::builder()
            .icon_name("edit-clear-symbolic")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .tooltip_text("Remove mapping")
            .build();
        {
            let app = app.clone();
            let row = row.clone();
            let btn = learn.clone();
            learn.connect_clicked(move |_| {
                row.set_subtitle("Press the footswitch now…");
                btn.set_sensitive(false);
                let row2 = row.clone();
                let btn2 = btn.clone();
                let app2 = app.clone();
                *app.learning.borrow_mut() = Some((
                    action,
                    Box::new(move || {
                        let label = app2.cfg.borrow().midi.get(action).map(|t| t.label()).unwrap_or_default();
                        row2.set_subtitle(&label);
                        btn2.set_sensitive(true);
                    }),
                ));
            });
        }
        {
            let app = app.clone();
            let row = row.clone();
            clear.connect_clicked(move |_| {
                app.cfg.borrow_mut().midi.set(action, None);
                app.save_cfg();
                row.set_subtitle("Not mapped");
            });
        }
        row.add_suffix(&learn);
        row.add_suffix(&clear);
        group.add(&row);
    }
    midi_page.add(&group);
    dialog.add(&midi_page);

    {
        let app = app.clone();
        dialog.connect_closed(move |_| {
            app.learning.borrow_mut().take();
        });
    }
    dialog.present(Some(&app.ui().window));
}
