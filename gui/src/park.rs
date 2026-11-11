//! The park window: sky and grass for the characters to live on, the chest
//! with what they've found, and where they should live.

use adw::prelude::*;
use anima::render::character::park;
use gtk::glib;

use crate::{config, daemon};

const MODES: [&str; 3] = ["Live on the desktop", "Live in the park", "The park, out now and then"];

pub fn run() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(park::APP_ID).build();
    app.connect_activate(|app| {
        if let Some(win) = app.active_window() {
            win.present();
            return;
        }
        build(app);
    });
    app.run_with_args::<&str>(&[])
}

fn build(app: &adw::Application) {
    let win = adw::ApplicationWindow::builder().application(app).title("Anima Park").default_width(1280).default_height(760).build();

    let ground = gtk::DrawingArea::builder().hexpand(true).vexpand(true).build();
    ground.set_draw_func(|_, cr, w, h| draw_park(cr, w as f64, h as f64));

    // the chest, top left
    let card = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).halign(gtk::Align::Start).valign(gtk::Align::Start).margin_start(18).margin_top(18).build();
    card.add_css_class("card");
    card.add_css_class("park-chest");
    let title = gtk::Label::builder().label("Chest").xalign(0.0).build();
    title.add_css_class("heading");
    card.append(&title);
    let mut counts = Vec::new();
    for (id, label) in park::MATERIALS {
        let row = gtk::Box::builder().spacing(8).build();
        let pic = gtk::Image::from_file(park::picture(id));
        pic.set_pixel_size(28);
        row.append(&pic);
        row.append(&gtk::Label::builder().label(label).xalign(0.0).hexpand(true).build());
        let n = gtk::Label::builder().label("0").xalign(1.0).build();
        n.add_css_class("numeric");
        row.append(&n);
        card.append(&row);
        counts.push((id, n));
    }
    let who = gtk::Label::builder().xalign(0.0).wrap(true).max_width_chars(26).build();
    who.add_css_class("dim-label");
    who.add_css_class("caption");
    card.append(&who);
    let refresh = move || {
        let chest = park::read_chest();
        for (id, n) in &counts {
            n.set_label(&park::count(&chest, id).to_string());
        }
        // who has brought the most
        let mut by: std::collections::BTreeMap<String, u32> = Default::default();
        for m in chest.values() {
            for (name, k) in m {
                *by.entry(name.clone()).or_default() += k;
            }
        }
        let best = by.into_iter().max_by_key(|b| b.1);
        who.set_label(&best.map_or("Nothing yet: they find things while fishing".into(), |(n, k)| format!("Most found: {n} ({k})")));
    };
    refresh();
    glib::timeout_add_seconds_local(2, move || {
        refresh();
        glib::ControlFlow::Continue
    });

    let overlay = gtk::Overlay::builder().child(&ground).build();
    overlay.add_overlay(&card);

    // where they live
    let mode = gtk::DropDown::from_strings(&MODES);
    mode.set_tooltip_text(Some("Where the characters live while this window is open"));
    let now = config::tuning_value("park_mode").unwrap_or(2.0).round().clamp(0.0, 2.0) as u32;
    mode.set_selected(now);
    mode.connect_selected_notify(|d| {
        let v = d.selected() as f64;
        config::set_tuning("park_mode", (v != 2.0).then_some(v));
        if daemon::running() {
            daemon::reload();
        }
    });
    let header = adw::HeaderBar::new();
    header.pack_end(&mode);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&overlay));
    win.set_content(Some(&view));

    let css = gtk::CssProvider::new();
    css.load_from_string(".park-chest { padding: 12px 14px; min-width: 190px; }");
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
    win.maximize();
    win.present();
    // a workspace of its own, so it doesn't sit among other windows
    std::thread::spawn(own_workspace);
}

/// Sky, far hills and grass; the characters walk along the bottom.
fn draw_park(cr: &gtk::cairo::Context, w: f64, h: f64) {
    let sky = gtk::cairo::LinearGradient::new(0.0, 0.0, 0.0, h);
    sky.add_color_stop_rgb(0.0, 0.47, 0.72, 0.95);
    sky.add_color_stop_rgb(0.75, 0.80, 0.92, 0.99);
    let _ = cr.set_source(&sky);
    cr.paint().ok();
    // a few clouds
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.85);
    for (x, y, s) in [(0.18, 0.16, 1.0), (0.55, 0.10, 1.3), (0.82, 0.22, 0.9)] {
        let (cx, cy, r) = (w * x, h * y, h * 0.035 * s);
        for (dx, dy, k) in [(-1.2, 0.3, 0.8), (0.0, 0.0, 1.1), (1.2, 0.3, 0.85), (0.5, 0.45, 0.8), (-0.5, 0.45, 0.8)] {
            cr.arc(cx + dx * r, cy + dy * r, r * k, 0.0, std::f64::consts::TAU);
            cr.fill().ok();
        }
    }
    // far hills
    let ground = h * 0.86;
    cr.set_source_rgb(0.55, 0.77, 0.47);
    cr.move_to(0.0, ground);
    let steps = 64;
    for k in 0..=steps {
        let x = w * k as f64 / steps as f64;
        let y = ground - h * (0.10 + 0.05 * (x / w * 7.0).sin() + 0.03 * (x / w * 17.0 + 1.0).sin());
        cr.line_to(x, y);
    }
    cr.line_to(w, ground);
    cr.close_path();
    cr.fill().ok();
    // grass
    let grass = gtk::cairo::LinearGradient::new(0.0, ground, 0.0, h);
    grass.add_color_stop_rgb(0.0, 0.42, 0.70, 0.33);
    grass.add_color_stop_rgb(1.0, 0.33, 0.58, 0.26);
    let _ = cr.set_source(&grass);
    cr.rectangle(0.0, ground, w, h - ground);
    cr.fill().ok();
    // little tufts along the edge
    cr.set_source_rgb(0.36, 0.62, 0.28);
    let mut x = 6.0;
    while x < w {
        cr.move_to(x, ground + 2.0);
        cr.line_to(x + 3.0, ground - 7.0);
        cr.line_to(x + 6.0, ground + 2.0);
        cr.fill().ok();
        x += 23.0 + (x * 0.37).sin().abs() * 19.0;
    }
}

/// Moves the park onto an empty workspace when it opened among other windows.
fn own_workspace() {
    let json = |args: &[&str]| -> Option<serde_json::Value> {
        let out = std::process::Command::new("niri").args(["msg", "--json"]).args(args).output().ok()?;
        serde_json::from_slice(&out.stdout).ok()
    };
    let mut me = None;
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(150));
        let Some(wins) = json(&["windows"]) else { return };
        let list = wins.as_array().cloned().unwrap_or_default();
        if let Some(w) = list.iter().find(|w| w["app_id"].as_str() == Some(park::APP_ID)) {
            me = Some((w["id"].as_u64(), w["workspace_id"].as_u64(), list));
            break;
        }
    }
    let Some((Some(id), Some(ws), list)) = me else { return };
    if list.iter().filter(|w| w["workspace_id"].as_u64() == Some(ws)).count() <= 1 {
        return;
    }
    let Some(spaces) = json(&["workspaces"]) else { return };
    let spaces = spaces.as_array().cloned().unwrap_or_default();
    let output = spaces.iter().find(|s| s["id"].as_u64() == Some(ws)).and_then(|s| s["output"].as_str().map(String::from));
    // niri always keeps an empty one at the end
    let empty = spaces
        .iter()
        .filter(|s| s["output"].as_str().map(String::from) == output && !list.iter().any(|w| w["workspace_id"].as_u64() == s["id"].as_u64()))
        .filter_map(|s| s["idx"].as_u64())
        .max();
    let Some(idx) = empty else { return };
    let _ = std::process::Command::new("niri")
        .args(["msg", "action", "move-window-to-workspace", "--window-id", &id.to_string(), "--focus", "true", &idx.to_string()])
        .status();
}
