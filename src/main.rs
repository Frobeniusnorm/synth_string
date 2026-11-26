//! Synth String – a plug-and-play guitar looper for GNOME.

mod audio;
mod config;
mod export;
mod midi;
mod project;
mod ui;

use adw::prelude::*;
use gtk::{gdk, gio, glib};
use std::rc::Rc;

const APP_ID: &str = "dev.synthstring.SynthString";
const CSS: &str = include_str!("../data/style.css");

thread_local! {
    static CONTROLLER: std::cell::RefCell<Option<Rc<ui::app::App>>> = const { std::cell::RefCell::new(None) };
}

fn main() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).flags(gio::ApplicationFlags::HANDLES_OPEN).build();
    // `synth-string song.synthstring` (or double-click in the file manager) opens a project
    app.connect_open(|gtk_app, files, _| {
        gtk_app.activate();
        if let (Some(file), Some(controller)) = (files.first(), CONTROLLER.with(|c| c.borrow().clone()))
            && let Some(path) = file.path()
        {
            controller.load_project(path);
        }
    });
    app.connect_startup(|_| {
        // running from the source tree: find the app icon without installing it
        let dev_icons = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("data/icons");
        if dev_icons.is_dir() {
            gtk::IconTheme::for_display(&gdk::Display::default().expect("display")).add_search_path(&dev_icons);
        }
        // window icon for X11 sessions (on Wayland the dock uses the installed .desktop file)
        gtk::Window::set_default_icon_name(APP_ID);
        let provider = gtk::CssProvider::new();
        provider.load_from_string(CSS);
        gtk::style_context_add_provider_for_display(
            &gdk::Display::default().expect("display"),
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    });
    app.connect_activate(|gtk_app| {
        if let Some(w) = gtk_app.active_window() {
            w.present();
            return;
        }
        let controller = ui::app::App::new();
        CONTROLLER.with(|c| *c.borrow_mut() = Some(controller.clone()));
        ui::window::build(&controller, gtk_app);
        install_actions(&controller, gtk_app);
        controller.start();
        debug_hooks(&controller);
    });
    app.run()
}

fn install_actions(app: &Rc<ui::app::App>, gtk_app: &adw::Application) {
    let add = |name: &str, accels: &[&str], f: Box<dyn Fn()>| {
        let a = gio::SimpleAction::new(name, None);
        a.connect_activate(move |_, _| f());
        gtk_app.add_action(&a);
        if !accels.is_empty() {
            gtk_app.set_accels_for_action(&format!("app.{name}"), accels);
        }
    };
    {
        let app = app.clone();
        add("export", &["<Control>e"], Box::new(move || app.export_dialog()));
    }
    {
        let app = app.clone();
        add("undo", &["<Control>z"], Box::new(move || app.undo()));
    }
    {
        let app = app.clone();
        add("preferences", &["<Control>comma"], Box::new(move || ui::prefs::show(&app)));
    }
    {
        let app = app.clone();
        add("new", &["<Control>n"], Box::new(move || app.new_project()));
    }
    {
        let app = app.clone();
        add("open", &["<Control>o"], Box::new(move || app.open_project_dialog()));
    }
    {
        let app = app.clone();
        add("save", &["<Control>s"], Box::new(move || app.save_project(false, None)));
    }
    {
        let app = app.clone();
        add("save-as", &["<Control><Shift>s"], Box::new(move || app.save_project(true, None)));
    }
    {
        let app = app.clone();
        add("import", &["<Control>i"], Box::new(move || app.import_dialog()));
    }
    {
        let app = app.clone();
        add("learn-noise", &[], Box::new(move || app.relearn_noise()));
    }
    {
        let app = app.clone();
        add(
            "clear",
            &["<Control><Shift>Delete"],
            Box::new(move || {
                if app.tracks.borrow().is_empty() {
                    return;
                }
                let d = adw::AlertDialog::new(
                    Some("Clear all tracks?"),
                    Some("This removes the loop and every layer. Export first if you want to keep it."),
                );
                d.add_responses(&[("cancel", "Cancel"), ("clear", "Clear All")]);
                d.set_response_appearance("clear", adw::ResponseAppearance::Destructive);
                d.set_close_response("cancel");
                let app2 = app.clone();
                d.connect_response(None, move |_, r| {
                    if r == "clear" {
                        app2.clear_all();
                    }
                });
                d.present(Some(&app.ui().window));
            }),
        );
    }
    {
        let app = app.clone();
        add(
            "shortcuts",
            &["<Control>question"],
            Box::new(move || {
                let d = adw::AlertDialog::new(Some("Keyboard shortcuts"), None);
                let group = adw::PreferencesGroup::new();
                for (k, v) in [
                    ("Space", "Record · finish loop · add a layer"),
                    ("Enter", "Play / pause"),
                    ("Esc", "Cancel recording"),
                    ("M", "Metronome on/off"),
                    ("T", "Tap tempo"),
                    ("Ctrl+Z", "Undo last layer"),
                    ("A S D / J K L", "Drum pads (when the drum machine is open)"),
                    ("Ctrl+I", "Import audio file"),
                    ("Ctrl+E", "Export"),
                    ("Ctrl+S / Ctrl+O", "Save / open project"),
                    ("Ctrl+,", "Preferences"),
                ] {
                    let row = adw::ActionRow::builder().title(v).build();
                    row.add_suffix(&gtk::Label::builder().label(k).css_classes(["dim-label", "monospace"]).build());
                    group.add(&row);
                }
                d.set_extra_child(Some(&group));
                d.add_response("close", "Close");
                d.present(Some(&app.ui().window));
            }),
        );
    }
    {
        let app = app.clone();
        add(
            "about",
            &[],
            Box::new(move || {
                let about = adw::AboutDialog::builder()
                    .application_name("Synth String")
                    .application_icon("dev.synthstring.SynthString")
                    .version(env!("CARGO_PKG_VERSION"))
                    .comments("A plug-and-play guitar looper with automatic trimming, noise reduction and fun effects.")
                    .license_type(gtk::License::Gpl30)
                    .build();
                about.present(Some(&app.ui().window));
            }),
        );
    }
    add("quit", &["<Control>q"], {
        let app = app.clone();
        Box::new(move || {
            app.ui().window.close();
        })
    });
}

/// Developer hooks: `SYNTH_STRING_DEMO=1` injects synthetic tracks, `SYNTH_STRING_SNAPSHOT=file.png`
/// renders the window to a PNG after a few seconds (useful for headless UI checks).
fn debug_hooks(app: &Rc<ui::app::App>) {
    if std::env::var_os("SYNTH_STRING_SWITCH").is_some() {
        // cycle the input selection through the device popover combo, like a user would
        for k in 0..8u64 {
            let app = app.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(1500 + k * 700), move || {
                let combo = &app.ui().input_combo;
                let n = combo.model().map(|m| m.n_items()).unwrap_or(1).max(1);
                let next = (combo.selected() + 1) % n;
                eprintln!("switch input -> {next}");
                combo.set_selected(next);
            });
        }
    }
    if let Some(path) = std::env::var_os("SYNTH_STRING_IMPORT") {
        app.import_file(path.into());
    }
    if std::env::var_os("SYNTH_STRING_DEMO").is_some_and(|d| d == "drums") {
        // open the drum machine, start a drum layer and play a little beat on the pads
        app.ui().drums_toggle.set_active(true);
        app.drum_record_toggle();
        let beat: [(u64, u8); 8] = [(0, 0), (0, 2), (250, 2), (500, 1), (500, 2), (750, 2), (875, 0), (1000, 2)];
        for (ms, d) in beat {
            let app = app.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(400 + ms), move || app.drum_pad(d));
        }
    } else if let Some(demo) = std::env::var_os("SYNTH_STRING_DEMO") {
        app.inject_demo();
        if demo == "slow" {
            // bass at half speed (→ 2 loops long), placed at the last repetition so it wraps around
            let app = app.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(600), move || {
                let id = app.tracks.borrow()[0].id;
                let fx = crate::audio::effects::TrackFx { speed: 0.5, ..Default::default() };
                app.set_fx(id, fx);
                app.set_song_mode(true);
                app.set_song_reps(6);
                glib::timeout_add_local_once(std::time::Duration::from_millis(1500), move || {
                    for rep in [0, 2, 4, 5] {
                        app.toggle_clip_at(0, rep);
                    }
                    app.seek_fraction(0.05);
                });
            });
        }
        if demo == "song" {
            // arrange: 12 repetitions, third track ("solo") only in repetitions 2–4
            let app = app.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(600), move || {
                app.set_song_mode(true);
                app.set_song_reps(12);
                for rep in 0..12 {
                    if !(1..=3).contains(&rep) {
                        app.toggle_clip_at(2, rep);
                    }
                }
                app.seek_fraction(0.21);
            });
        }
    }
    if let Some(path) = std::env::var_os("SYNTH_STRING_SAVE_TO") {
        let app = app.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(2500), move || {
            *app.project_path.borrow_mut() = Some(path.into());
            app.save_project(false, None);
        });
    }
    if let Some(path) = std::env::var_os("SYNTH_STRING_SNAPSHOT") {
        let app = app.clone();
        glib::timeout_add_local_once(std::time::Duration::from_millis(3500), move || {
            let win = &app.ui().window;
            let (w, h) = (win.width() as f64, win.height() as f64);
            let snap = gtk::Snapshot::new();
            // draw the widget tree directly (works even if the compositor never showed the window)
            snap.append_color(
                &gdk::RGBA::new(0.14, 0.14, 0.15, 1.0),
                &gtk::graphene::Rect::new(0.0, 0.0, w as f32, h as f32),
            );
            if let Some(child) = win.child() {
                // force a layout in case the compositor never gave us a frame (window hidden)
                child.measure(gtk::Orientation::Horizontal, -1);
                child.allocate(w as i32, h as i32, -1, None);
                win.snapshot_child(&child, &snap);
            }
            let node = snap.to_node();
            let renderer = win.native().and_then(|n| n.renderer());
            if let (Some(node), Some(renderer)) = (node, renderer) {
                let tex = renderer.render_texture(node, None);
                if let Err(e) = tex.save_to_png(&path) {
                    eprintln!("[snapshot] {e}");
                }
            }
        });
    }
}
