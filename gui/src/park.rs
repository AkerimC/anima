//! The park window: sky and grass for the characters to live on, the chest
//! with what they've found, and where they should live.

use adw::prelude::*;
use anima::render::character::park;
use gtk::glib;
use std::cell::RefCell;
use std::rc::Rc;

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

struct St {
    park: park::Park,
    sel: Option<usize>,
    drag_from: Option<f32>,
    busy: Vec<usize>,
    unit_frac: Option<f32>,
}

fn save(st: &St) {
    park::write_park(&st.park);
}

fn build(app: &adw::Application) {
    let win = adw::ApplicationWindow::builder().application(app).title("Anima Park").default_width(1280).default_height(760).build();
    let st = Rc::new(RefCell::new(St { park: park::read_park(), sel: None, drag_from: None, busy: park::read_busy(), unit_frac: park::read_unit_frac() }));
    let toast = adw::ToastOverlay::new();

    let ground = gtk::DrawingArea::builder().hexpand(true).vexpand(true).build();
    let s2 = st.clone();
    ground.set_draw_func(move |_, cr, w, h| {
        let (w, h) = (w as f64, h as f64);
        draw_park(cr, w, h);
        let st = s2.borrow();
        let unit = unit_px(&st, w);
        for (k, t) in st.park.things.iter().enumerate() {
            draw_thing(cr, t, t.x as f64 * w, h - 3.0, unit, st.busy.contains(&k), st.sel == Some(k));
        }
    });

    // drag things along the grass; a click picks one
    let drag = gtk::GestureDrag::new();
    let (s2, g2) = (st.clone(), ground.clone());
    drag.connect_drag_begin(move |_, x, y| {
        let mut st = s2.borrow_mut();
        let (w, h) = (g2.width() as f64, g2.height() as f64);
        let unit = unit_px(&st, w);
        let hit = st.park.things.iter().enumerate().rev().find(|(_, t)| {
            let half = park::width(t) as f64 * unit * 0.5;
            (x - t.x as f64 * w).abs() <= half && y >= h - 3.0 - tall(t) * unit
        }).map(|(k, _)| k);
        st.sel = hit;
        st.drag_from = hit.map(|k| st.park.things[k].x);
        g2.queue_draw();
    });
    let (s2, g2) = (st.clone(), ground.clone());
    drag.connect_drag_update(move |_, dx, _| {
        let mut st = s2.borrow_mut();
        let (Some(k), Some(from)) = (st.sel, st.drag_from) else { return };
        let w = g2.width().max(1) as f64;
        let unit = unit_px(&st, w);
        let half = (park::width(&st.park.things[k]) as f64 * unit * 0.5 / w) as f32;
        st.park.things[k].x = (from + (dx / w) as f32).clamp(half, (1.0 - half).max(half));
        g2.queue_draw();
    });
    let s2 = st.clone();
    drag.connect_drag_end(move |_, _, _| {
        let mut st = s2.borrow_mut();
        if st.drag_from.take().is_some() {
            save(&st);
        }
    });
    ground.add_controller(drag);

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
                if name != "Park" {
                    *by.entry(name.clone()).or_default() += k;
                }
            }
        }
        let best = by.into_iter().max_by_key(|b| b.1);
        who.set_label(&best.map_or("Nothing yet: they find things while fishing".into(), |(n, k)| format!("Most found: {n} ({k})")));
    };
    refresh();
    let (s2, g2) = (st.clone(), ground.clone());
    glib::timeout_add_local(std::time::Duration::from_millis(500), move || {
        refresh();
        let mut st = s2.borrow_mut();
        let (busy, unit) = (park::read_busy(), park::read_unit_frac());
        if busy != st.busy || unit != st.unit_frac {
            st.busy = busy;
            st.unit_frac = unit;
            g2.queue_draw();
        }
        glib::ControlFlow::Continue
    });

    // what's picked: turn it round, make a slide bigger, take it down
    let bar = gtk::Box::builder().spacing(8).halign(gtk::Align::Center).valign(gtk::Align::Start).margin_top(18).visible(false).build();
    bar.add_css_class("card");
    bar.add_css_class("park-chest");
    let name = gtk::Label::builder().build();
    name.add_css_class("heading");
    let turn = gtk::Button::builder().icon_name("object-flip-horizontal-symbolic").tooltip_text("Turn round").build();
    let bigger = gtk::Button::builder().label("Add a piece").tooltip_text("2 wood, 1 nail").build();
    let remove = gtk::Button::builder().icon_name("user-trash-symbolic").tooltip_text("Take down (materials go back in the chest)").build();
    remove.add_css_class("destructive-action");
    let done = gtk::Button::builder().icon_name("object-select-symbolic").tooltip_text("Done").build();
    for w in [name.upcast_ref::<gtk::Widget>(), turn.upcast_ref(), bigger.upcast_ref(), remove.upcast_ref(), done.upcast_ref()] {
        bar.append(w);
    }
    let sync_bar = {
        let (st, bar, name, bigger) = (st.clone(), bar.clone(), name.clone(), bigger.clone());
        move || {
            let st = st.borrow();
            match st.sel.and_then(|k| st.park.things.get(k)) {
                Some(t) => {
                    let label = park::recipe(&t.kind).map_or(t.kind.as_str(), |r| r.1);
                    name.set_label(&if t.kind == "slide" { format!("{label} ({} pieces)", t.pieces) } else { label.to_string() });
                    bigger.set_visible(t.kind == "slide" && t.pieces < 5);
                    bar.set_visible(true);
                }
                None => bar.set_visible(false),
            }
        }
    };
    let sync_bar = Rc::new(sync_bar);
    {
        let sb = sync_bar.clone();
        let click = gtk::GestureClick::new();
        click.connect_released(move |_, _, _, _| sb());
        ground.add_controller(click);
        let sb = sync_bar.clone();
        let g = ground.clone();
        let tick = gtk::EventControllerMotion::new();
        tick.connect_leave(move |_| sb());
        g.add_controller(tick);
    }
    let (s2, g2, sb) = (st.clone(), ground.clone(), sync_bar.clone());
    turn.connect_clicked(move |_| {
        let mut st = s2.borrow_mut();
        if let Some(k) = st.sel {
            st.park.things[k].flip = !st.park.things[k].flip;
            save(&st);
        }
        drop(st);
        g2.queue_draw();
        sb();
    });
    let (s2, g2, sb, t2) = (st.clone(), ground.clone(), sync_bar.clone(), toast.clone());
    bigger.connect_clicked(move |_| {
        let mut st = s2.borrow_mut();
        let Some(k) = st.sel else { return };
        let cost = park::recipe("slide_piece").map_or(&[][..], |r| r.2);
        if !st.park.free && !park::spend(cost) {
            t2.add_toast(adw::Toast::new("Not enough in the chest for a piece"));
            return;
        }
        st.park.things[k].pieces = (st.park.things[k].pieces + 1).min(5);
        save(&st);
        drop(st);
        g2.queue_draw();
        sb();
    });
    let (s2, g2, sb) = (st.clone(), ground.clone(), sync_bar.clone());
    remove.connect_clicked(move |_| {
        let mut st = s2.borrow_mut();
        let Some(k) = st.sel.take() else { return };
        let t = st.park.things.remove(k);
        if !st.park.free {
            if let Some(r) = park::recipe(&t.kind) {
                park::refund(r.2);
            }
            if let Some(r) = park::recipe("slide_piece") {
                for _ in 1..t.pieces {
                    park::refund(r.2);
                }
            }
        }
        save(&st);
        drop(st);
        g2.queue_draw();
        sb();
    });
    let (s2, g2, sb) = (st.clone(), ground.clone(), sync_bar.clone());
    done.connect_clicked(move |_| {
        s2.borrow_mut().sel = None;
        g2.queue_draw();
        sb();
    });

    let overlay = gtk::Overlay::builder().child(&ground).build();
    overlay.add_overlay(&card);
    overlay.add_overlay(&bar);
    toast.set_child(Some(&overlay));

    // building: what can be made and what it takes
    let list = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).margin_top(6).margin_bottom(6).margin_start(6).margin_end(6).build();
    let mut buttons = Vec::new();
    for &(id, label, cost) in park::RECIPES.iter().filter(|r| r.0 != "slide_piece") {
        let row = gtk::Box::builder().spacing(10).build();
        let text = gtk::Box::builder().orientation(gtk::Orientation::Vertical).hexpand(true).build();
        text.append(&gtk::Label::builder().label(label).xalign(0.0).build());
        let need: Vec<String> = cost.iter().map(|(m, k)| format!("{k} {}", park::MATERIALS.iter().find(|x| x.0 == *m).map_or(*m, |x| x.1).to_lowercase())).collect();
        let sub = gtk::Label::builder().label(need.join(", ")).xalign(0.0).build();
        sub.add_css_class("dim-label");
        sub.add_css_class("caption");
        text.append(&sub);
        row.append(&text);
        let b = gtk::Button::builder().label("Build").valign(gtk::Align::Center).build();
        row.append(&b);
        list.append(&row);
        buttons.push((b, cost));
        let _ = id;
    }
    let free = gtk::CheckButton::builder().label("Free building (trying out)").active(st.borrow().park.free).margin_top(6).build();
    list.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    list.append(&free);
    let pop = gtk::Popover::builder().child(&list).build();
    let build_btn = gtk::MenuButton::builder().label("Build").popover(&pop).build();
    build_btn.add_css_class("suggested-action");
    let s2 = st.clone();
    free.connect_toggled(move |c| {
        let mut st = s2.borrow_mut();
        st.park.free = c.is_active();
        save(&st);
    });
    // greyed out when the chest hasn't enough
    let sync_build = {
        let (st, buttons) = (st.clone(), buttons.iter().map(|(b, c)| (b.clone(), *c)).collect::<Vec<_>>());
        move || {
            let chest = park::read_chest();
            let free = st.borrow().park.free;
            for (b, cost) in &buttons {
                b.set_sensitive(free || park::affordable(&chest, cost));
            }
        }
    };
    let sync_build = Rc::new(sync_build);
    {
        let sb = sync_build.clone();
        pop.connect_show(move |_| sb());
        let sb = sync_build.clone();
        free.connect_toggled(move |_| sb());
    }
    for (k, (b, cost)) in buttons.into_iter().enumerate() {
        let id = park::RECIPES.iter().filter(|r| r.0 != "slide_piece").nth(k).map(|r| r.0).unwrap_or("chair");
        let (s2, g2, sb, sbar, t2, pop) = (st.clone(), ground.clone(), sync_build.clone(), sync_bar.clone(), toast.clone(), pop.clone());
        b.connect_clicked(move |_| {
            let mut st = s2.borrow_mut();
            if !st.park.free && !park::spend(cost) {
                t2.add_toast(adw::Toast::new("Not enough in the chest"));
                return;
            }
            // somewhere free near the middle
            let w = g2.width().max(1) as f64;
            let unit = unit_px(&st, w);
            let mut thing = park::Thing { kind: id.to_string(), x: 0.5, flip: false, pieces: 1 };
            let me = park::width(&thing) as f64 * unit * 0.5 / w;
            let clear = |x: f64, st: &St| st.park.things.iter().all(|t| (t.x as f64 - x).abs() > me + park::width(t) as f64 * unit * 0.5 / w);
            let spots = (0..40).map(|j| 0.5 + (if j % 2 == 0 { 1.0 } else { -1.0 }) * (j / 2) as f64 * 0.025);
            if let Some(x) = spots.filter(|x| *x > me && *x < 1.0 - me).find(|&x| clear(x, &st)) {
                thing.x = x as f32;
            }
            st.park.things.push(thing);
            st.sel = Some(st.park.things.len() - 1);
            save(&st);
            drop(st);
            pop.popdown();
            g2.queue_draw();
            sb();
            sbar();
        });
    }

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
    header.pack_start(&build_btn);
    header.pack_end(&mode);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&toast));
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

/// A character's height in window pixels.
fn unit_px(st: &St, w: f64) -> f64 {
    st.unit_frac.map_or(110.0, |f| f as f64 * w).max(30.0)
}

/// How tall a thing is (character heights), for picking it.
fn tall(t: &park::Thing) -> f64 {
    match t.kind.as_str() {
        "chair" => 0.9,
        "bench" => 0.75,
        "table" => 0.7,
        "stove" => 1.3,
        "swing" | "lamp" => 1.65,
        "seesaw" => 0.6,
        "slide" => park::slide_size(t.pieces).0 as f64 + 0.35,
        "tree" => 1.9,
        _ => 0.45,
    }
}

fn rect(cr: &gtk::cairo::Context, x0: f64, y0: f64, x1: f64, y1: f64) {
    cr.rectangle(x0.min(x1), y0.min(y1), (x1 - x0).abs(), (y1 - y0).abs());
    cr.fill().ok();
}

fn bar(cr: &gtk::cairo::Context, a: (f64, f64), b: (f64, f64), width: f64) {
    cr.set_line_width(width);
    cr.set_line_cap(gtk::cairo::LineCap::Round);
    cr.move_to(a.0, a.1);
    cr.line_to(b.0, b.1);
    cr.stroke().ok();
}

const WOOD: (f64, f64, f64) = (0.66, 0.45, 0.27);
const DARK: (f64, f64, f64) = (0.47, 0.31, 0.18);

fn rgb(cr: &gtk::cairo::Context, c: (f64, f64, f64)) {
    cr.set_source_rgb(c.0, c.1, c.2);
}

/// One built thing, standing on `gy` with its middle at `x`; `u` is a
/// character's height in pixels.
fn draw_thing(cr: &gtk::cairo::Context, t: &park::Thing, x: f64, gy: f64, u: f64, busy: bool, picked: bool) {
    let dir = if t.flip { -1.0 } else { 1.0 };
    let seat = park::SEAT as f64 * u;
    match t.kind.as_str() {
        "chair" => {
            rgb(cr, DARK);
            bar(cr, (x - 0.18 * u, gy), (x - 0.18 * u, gy - seat), 0.05 * u);
            bar(cr, (x + 0.18 * u, gy), (x + 0.18 * u, gy - seat), 0.05 * u);
            let back = x - dir * 0.2 * u;
            bar(cr, (back, gy - seat), (back, gy - 0.85 * u), 0.06 * u);
            rgb(cr, WOOD);
            rect(cr, x - 0.24 * u, gy - seat - 0.03 * u, x + 0.24 * u, gy - seat + 0.04 * u);
            rect(cr, back - 0.05 * u, gy - 0.82 * u, back + 0.05 * u, gy - 0.55 * u);
        }
        "bench" => {
            rgb(cr, DARK);
            for s in [-1.0, 1.0] {
                bar(cr, (x + s * 0.42 * u, gy), (x + s * 0.42 * u, gy - 0.72 * u), 0.05 * u);
            }
            rgb(cr, WOOD);
            rect(cr, x - 0.52 * u, gy - seat - 0.03 * u, x + 0.52 * u, gy - seat + 0.04 * u);
            rect(cr, x - 0.5 * u, gy - 0.58 * u, x + 0.5 * u, gy - 0.52 * u);
            rect(cr, x - 0.5 * u, gy - 0.71 * u, x + 0.5 * u, gy - 0.65 * u);
        }
        "table" => {
            rgb(cr, DARK);
            for s in [-1.0, 1.0] {
                bar(cr, (x + s * 0.52 * u, gy), (x + s * 0.52 * u, gy - 0.6 * u), 0.06 * u);
            }
            rgb(cr, WOOD);
            rect(cr, x - 0.66 * u, gy - 0.66 * u, x + 0.66 * u, gy - 0.58 * u);
            // a cloth and a cup
            cr.set_source_rgb(0.93, 0.36, 0.36);
            rect(cr, x - 0.3 * u, gy - 0.665 * u, x + 0.3 * u, gy - 0.6 * u);
            cr.set_source_rgb(1.0, 1.0, 1.0);
            rect(cr, x + 0.1 * u, gy - 0.76 * u, x + 0.18 * u, gy - 0.665 * u);
        }
        "stove" => {
            cr.set_source_rgb(0.32, 0.33, 0.36);
            let pipe = x + dir * 0.28 * u;
            rect(cr, pipe - 0.05 * u, gy - 1.3 * u, pipe + 0.05 * u, gy - 0.6 * u);
            cr.set_source_rgb(0.42, 0.43, 0.47);
            rect(cr, x - 0.38 * u, gy - 0.64 * u, x + 0.38 * u, gy);
            cr.set_source_rgb(0.25, 0.25, 0.28);
            rect(cr, x - 0.4 * u, gy - 0.67 * u, x + 0.4 * u, gy - 0.62 * u);
            cr.set_source_rgb(0.2, 0.2, 0.22);
            rect(cr, x - 0.25 * u, gy - 0.48 * u, x + 0.25 * u, gy - 0.12 * u);
            cr.set_source_rgb(1.0, 0.55, 0.15);
            rect(cr, x - 0.18 * u, gy - 0.3 * u, x + 0.18 * u, gy - 0.18 * u);
            // a pot on top
            cr.set_source_rgb(0.75, 0.2, 0.2);
            rect(cr, x - dir * 0.2 * u - 0.12 * u, gy - 0.82 * u, x - dir * 0.2 * u + 0.12 * u, gy - 0.67 * u);
        }
        "swing" => {
            let top = park::SWING_TOP as f64 * u;
            rgb(cr, DARK);
            for s in [-1.0, 1.0] {
                bar(cr, (x + s * 0.8 * u, gy), (x + s * 0.6 * u, gy - top), 0.06 * u);
                bar(cr, (x + s * 0.4 * u, gy), (x + s * 0.6 * u, gy - top), 0.06 * u);
            }
            rgb(cr, WOOD);
            bar(cr, (x - 0.66 * u, gy - top), (x + 0.66 * u, gy - top), 0.08 * u);
            if !busy {
                let low = gy - top + park::SWING_ROPE as f64 * u;
                cr.set_source_rgb(0.85, 0.72, 0.5);
                for s in [-1.0, 1.0] {
                    bar(cr, (x + s * 0.14 * u, gy - top), (x + s * 0.14 * u, low), 0.025 * u);
                }
                rgb(cr, WOOD);
                rect(cr, x - 0.22 * u, low - 0.03 * u, x + 0.22 * u, low + 0.03 * u);
            }
        }
        "seesaw" => {
            let pivot = park::SEESAW_PIVOT as f64 * u;
            let (half, tilt) = (park::SEESAW_HALF as f64 * u, park::SEESAW_TILT as f64 * u);
            cr.set_source_rgb(0.85, 0.3, 0.3);
            cr.move_to(x - 0.16 * u, gy);
            cr.line_to(x + 0.16 * u, gy);
            cr.line_to(x, gy - pivot);
            cr.close_path();
            cr.fill().ok();
            if !busy {
                rgb(cr, WOOD);
                bar(cr, (x - half, gy - pivot + tilt), (x + half, gy - pivot - tilt), 0.08 * u);
                cr.set_source_rgb(0.3, 0.45, 0.85);
                for s in [-1.0, 1.0] {
                    let (hx, hy) = (x + s * half * 0.8, gy - pivot - s * tilt * 0.8);
                    bar(cr, (hx, hy), (hx, hy - 0.16 * u), 0.04 * u);
                }
            }
        }
        "slide" => {
            let (top, reach) = park::slide_size(t.pieces);
            let (top, reach) = (top as f64 * u, reach as f64 * u);
            let half = park::width(t) as f64 * u * 0.5;
            let ladder = x - dir * (half - 0.12 * u);
            let lip = ladder + dir * 0.25 * u;
            // a pole under the chute
            cr.set_source_rgb(0.55, 0.57, 0.62);
            let mid = lip + dir * reach * 0.4;
            bar(cr, (mid, gy), (mid, gy - top * 0.45), 0.05 * u);
            // ladder
            cr.set_source_rgb(0.6, 0.62, 0.68);
            for s in [-1.0, 1.0] {
                bar(cr, (ladder + s * 0.09 * u, gy), (ladder + s * 0.09 * u, gy - top - 0.3 * u), 0.04 * u);
            }
            let mut y = gy - 0.16 * u;
            while y > gy - top {
                bar(cr, (ladder - 0.09 * u, y), (ladder + 0.09 * u, y), 0.03 * u);
                y -= 0.17 * u;
            }
            // the platform
            rgb(cr, WOOD);
            rect(cr, ladder - 0.12 * u, gy - top - 0.02 * u, lip, gy - top + 0.05 * u);
            // the chute, one colour per piece
            let colours = [(0.95, 0.35, 0.3), (0.98, 0.72, 0.2), (0.3, 0.7, 0.4), (0.3, 0.55, 0.9), (0.7, 0.4, 0.85)];
            let steps = 40;
            for k in 0..steps {
                let (s0, s1) = (k as f64 / steps as f64, (k + 1) as f64 / steps as f64);
                let piece = ((s0 * t.pieces as f64).floor() as usize).min(colours.len() - 1);
                let c = colours[piece];
                cr.set_source_rgb(c.0, c.1, c.2);
                let p = |s: f64| (lip + dir * reach * s, gy - top * (1.0 - s).powf(1.15));
                bar(cr, p(s0), p(s1), 0.09 * u);
            }
        }
        "flowers" => {
            for (k, c) in [(0.95, 0.4, 0.55), (1.0, 0.8, 0.2), (0.6, 0.45, 0.95), (1.0, 0.55, 0.3), (0.95, 0.4, 0.55)].iter().enumerate() {
                let fx = x - 0.3 * u + k as f64 * 0.15 * u;
                cr.set_source_rgb(0.3, 0.6, 0.3);
                bar(cr, (fx, gy - 0.12 * u), (fx, gy - 0.32 * u), 0.025 * u);
                cr.set_source_rgb(c.0, c.1, c.2);
                cr.arc(fx, gy - 0.34 * u, 0.055 * u, 0.0, std::f64::consts::TAU);
                cr.fill().ok();
            }
            rgb(cr, WOOD);
            rect(cr, x - 0.4 * u, gy - 0.15 * u, x + 0.4 * u, gy);
        }
        "tree" => {
            rgb(cr, DARK);
            rect(cr, x - 0.08 * u, gy - 0.9 * u, x + 0.08 * u, gy);
            cr.set_source_rgb(0.3, 0.6, 0.3);
            for (dx, dy, r) in [(0.0, 1.35, 0.42), (-0.32, 1.05, 0.32), (0.32, 1.05, 0.32), (0.0, 0.95, 0.35)] {
                cr.arc(x + dx * u, gy - dy * u, r * u, 0.0, std::f64::consts::TAU);
                cr.fill().ok();
            }
        }
        "lamp" => {
            cr.set_source_rgb(0.25, 0.27, 0.3);
            bar(cr, (x, gy), (x, gy - 1.5 * u), 0.05 * u);
            rect(cr, x - 0.1 * u, gy - 0.05 * u, x + 0.1 * u, gy);
            cr.set_source_rgba(1.0, 0.92, 0.55, 0.35);
            cr.arc(x, gy - 1.55 * u, 0.22 * u, 0.0, std::f64::consts::TAU);
            cr.fill().ok();
            cr.set_source_rgb(1.0, 0.9, 0.5);
            cr.arc(x, gy - 1.55 * u, 0.08 * u, 0.0, std::f64::consts::TAU);
            cr.fill().ok();
        }
        "sandbox" => {
            cr.set_source_rgb(0.93, 0.84, 0.6);
            rect(cr, x - 0.68 * u, gy - 0.14 * u, x + 0.68 * u, gy);
            rgb(cr, WOOD);
            rect(cr, x - 0.72 * u, gy - 0.18 * u, x - 0.64 * u, gy);
            rect(cr, x + 0.64 * u, gy - 0.18 * u, x + 0.72 * u, gy);
            cr.set_source_rgb(0.3, 0.55, 0.9);
            rect(cr, x + dir * 0.4 * u - 0.07 * u, gy - 0.26 * u, x + dir * 0.4 * u + 0.07 * u, gy - 0.12 * u);
        }
        _ => {}
    }
    if picked {
        let half = park::width(t) as f64 * u * 0.5;
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.9);
        cr.set_line_width(2.0);
        cr.set_dash(&[6.0, 4.0], 0.0);
        cr.rectangle(x - half - 6.0, gy - tall(t) * u - 6.0, half * 2.0 + 12.0, tall(t) * u + 9.0);
        cr.stroke().ok();
        cr.set_dash(&[], 0.0);
    }
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
