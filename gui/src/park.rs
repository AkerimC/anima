//! The park, cafe and farm windows: somewhere for the characters to live,
//! with what they've built and found. One program, a window for each, all
//! sharing what's built (park.json); the daemon says what's going on right
//! now (animals, the stoves, the dishes) in a small file it keeps updating.

use adw::prelude::*;
use anima::render::character::park;
use gtk::gdk::prelude::GdkCairoContextExt;
use gtk::{gdk_pixbuf, gio, glib};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::{config, daemon};

/// What they're there for: the choices, and the setting each is (activity).
const ACTIVITIES: [(&str, f64); 3] = [("Play and work", 0.0), ("Only play", 1.0), ("Work first", 2.0)];

/// The choices, and the setting each one is (park_mode).
const MODES: [(&str, f64); 4] = [("Live on the desktop", 0.0), ("Live on the desktop, visit these", 3.0), ("Live in these windows", 1.0), ("Here, out now and then", 2.0)];

/// What every window shares: what's built, and what's going on.
struct Shared {
    park: park::Park,
    world: park::World,
    world_mtime: Option<std::time::SystemTime>,
    unit_frac: Option<f32>,
    farm: park::Farm,
    // animals as drawn (eased towards where the daemon says they are)
    shown: Vec<(String, f32, bool, f32, f32)>,
    pics: HashMap<(String, i32), Option<gtk::cairo::ImageSurface>>,
    // each window's background, drawn once for its size (place, w, h, unit)
    backs: HashMap<(&'static str, i32, i32, i32), gtk::cairo::ImageSurface>,
    // animals: where they were and are going, since when, how often news comes
    anim_from: Vec<(f32, f32, f32)>,
    anim_t0: std::time::Instant,
    anim_gap: f32,
    started: std::time::Instant,
}

thread_local! {
    static SHARED: RefCell<Option<Rc<RefCell<Shared>>>> = const { RefCell::new(None) };
}

fn shared() -> Rc<RefCell<Shared>> {
    SHARED.with(|s| {
        s.borrow_mut()
            .get_or_insert_with(|| {
                let mut park = park::read_park();
                // things from before they had numbers of their own
                let mut changed = false;
                for (k, t) in park.things.iter_mut().enumerate() {
                    if t.id == 0 {
                        t.id = new_id() + k as u64;
                        changed = true;
                    }
                }
                if changed {
                    park::write_park(&park);
                }
                Rc::new(RefCell::new(Shared {
                    park,
                    world: park::read_world(),
                    world_mtime: None,
                    unit_frac: park::read_unit_frac(),
                    farm: park::read_farm(),
                    shown: Vec::new(),
                    pics: HashMap::new(),
                    backs: HashMap::new(),
                    anim_from: Vec::new(),
                    anim_t0: std::time::Instant::now(),
                    anim_gap: 0.2,
                    started: std::time::Instant::now(),
                }))
            })
            .clone()
    })
}

fn new_id() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_micros() as u64)
}

/// `anima-gui --park`, `--cafe`, `--farm` or `--world` (all three): the
/// first one started opens the windows asked for by the later ones too.
pub fn run() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(park::APP_ID).flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE).build();
    app.connect_command_line(|app, cmd| {
        let args: Vec<String> = cmd.arguments().iter().map(|a| a.to_string_lossy().into_owned()).collect();
        let mut want: Vec<&str> = Vec::new();
        for a in &args {
            match a.as_str() {
                "--park" => want.push("park"),
                "--cafe" => want.push("cafe"),
                "--farm" => want.push("farm"),
                "--world" => want.extend(["park", "cafe", "farm"]),
                _ => {}
            }
        }
        if want.is_empty() {
            want.push("park");
        }
        // one after another, so each finds the ones before it and goes beside them
        // (the first right away: an open window keeps the program running)
        let open = |app: &adw::Application, place: &str| {
            let title = park::place_title(place);
            match app.windows().into_iter().find(|w| w.title().is_some_and(|t| t == title)) {
                Some(w) => w.present(),
                None => build(app, place),
            }
        };
        for (k, place) in want.into_iter().enumerate() {
            if k == 0 {
                open(app, place);
                continue;
            }
            let app = app.clone();
            glib::timeout_add_local_once(std::time::Duration::from_millis(1500 * k as u64), move || open(&app, place));
        }
        0.into()
    });
    app.connect_activate(|_| {});
    app.run()
}

/// One window's own: what's picked and being dragged.
struct Win {
    place: &'static str,
    sel: Option<usize>,
    drag_from: Option<f32>,
    drag_y: f32,
}

fn save(sh: &Shared) {
    park::write_park(&sh.park);
}

fn build(app: &adw::Application, place: &str) {
    let place: &'static str = park::PLACES.iter().find(|p| p.0 == place).map_or("park", |p| p.0);
    let title = park::place_title(place);
    let win = adw::ApplicationWindow::builder().application(app).title(title).default_width(1280).default_height(760).build();
    let sh = shared();
    let me = Rc::new(RefCell::new(Win { place, sel: None, drag_from: None, drag_y: 0.0 }));
    let toast = adw::ToastOverlay::new();

    let ground = gtk::DrawingArea::builder().hexpand(true).vexpand(true).build();
    let (s2, m2) = (sh.clone(), me.clone());
    ground.set_draw_func(move |_, cr, w, h| {
        let (w, h) = (w as f64, h as f64);
        let mut sh = s2.borrow_mut();
        let me = m2.borrow();
        let unit = unit_px(&sh, w);
        let time = sh.started.elapsed().as_secs_f64();
        let key = (me.place, w as i32, h as i32, unit as i32);
        if !sh.backs.contains_key(&key) {
            sh.backs.retain(|k, _| k.0 != me.place);
            if let Ok(surface) = gtk::cairo::ImageSurface::create(gtk::cairo::Format::Rgb24, w as i32, h as i32) {
                if let Ok(c) = gtk::cairo::Context::new(&surface) {
                    match me.place {
                        "cafe" => draw_cafe(&c, w, h),
                        "farm" => draw_farm(&c, w, h, unit),
                        _ => draw_park(&c, w, h),
                    }
                }
                surface.flush();
                sh.backs.insert(key, surface);
            }
        }
        if let Some(back) = sh.backs.get(&key) {
            cr.set_source_surface(back, 0.0, 0.0).ok();
            cr.paint().ok();
        }
        if me.place == "farm" {
            draw_farm_live(cr, w, h, time, unit);
        }
        let things: Vec<(usize, park::Thing)> = sh.park.things.iter().cloned().enumerate().filter(|(_, t)| t.place == me.place).collect();
        // the far ones first, so the near ones stand in front
        let mut things = things;
        things.sort_by(|a, b| b.1.y.total_cmp(&a.1.y));
        for (k, t) in &things {
            let busy = sh.world.busy.contains(k);
            let x = t.x as f64 * w;
            let gy = h - 3.0 - t.y as f64 * h;
            draw_thing(cr, t, x, gy, unit, busy, me.sel == Some(*k));
            let (grown, ready) = park::farm_state(t, &sh.farm);
            draw_extras(&mut sh, cr, t, x, gy, unit, grown, ready, time);
        }
        if me.place == "park" {
            for (px, dish, left) in sh.world.picnics.clone() {
                let x = px as f64 * w;
                let gy = h - 3.0;
                // a checked blanket on the grass, the food in the middle
                let (bw, bh) = (1.3 * unit, 0.07 * unit);
                for k in 0..8 {
                    if k % 2 == 0 {
                        cr.set_source_rgb(0.9, 0.25, 0.25);
                    } else {
                        cr.set_source_rgb(0.98, 0.96, 0.92);
                    }
                    let a = x - bw * 0.5 + k as f64 * bw / 8.0;
                    rect(cr, a, gy - bh, a + bw / 8.0, gy);
                }
                let size = 0.38 * unit * (0.35 + 0.65 * left as f64);
                draw_picture(&mut sh, cr, &park::food_picture(&dish), x - size * 0.5, gy - bh - size * 0.9, size, false);
            }
        }
        if me.place == "farm" {
            // where each one is now: between where it was and where it's
            // going, at an even pace over the time between updates
            let k = (sh.anim_t0.elapsed().as_secs_f32() / sh.anim_gap).min(1.0);
            let animals_now: Vec<(String, f32, bool, f32, f32)> = sh
                .world
                .animals
                .iter()
                .enumerate()
                .map(|(n, a)| {
                    let from = sh.anim_from.get(n).copied().unwrap_or((a.x, a.y, a.hop));
                    (a.kind.clone(), from.0 + (a.x - from.0) * k, a.left, from.2 + (a.hop - from.2) * k, from.1 + (a.y - from.1) * k)
                })
                .collect();
            sh.shown = animals_now;
            let mut animals = sh.shown.clone();
            animals.sort_by(|a, b| b.4.total_cmp(&a.4));
            for (kind, x, left, hop, up) in animals {
                let size = if kind == "cow" { 0.95 } else { 0.42 } * unit;
                let y = h - 3.0 - up as f64 * h - size - hop as f64 * 0.25 * unit;
                draw_picture(&mut sh, cr, &park::animal_picture(&kind), x as f64 * w - size * 0.5, y, size, !left);
            }
        }
    });

    // drag things along the floor; a click picks one
    let drag = gtk::GestureDrag::new();
    let (s2, m2, g2) = (sh.clone(), me.clone(), ground.clone());
    drag.connect_drag_begin(move |_, x, y| {
        let sh = s2.borrow();
        let mut me = m2.borrow_mut();
        let (w, h) = (g2.width() as f64, g2.height() as f64);
        let unit = unit_px(&sh, w);
        // nearest first: the lowest down
        let mut order: Vec<(usize, &park::Thing)> = sh.park.things.iter().enumerate().filter(|(_, t)| t.place == me.place).collect();
        order.sort_by(|a, b| a.1.y.total_cmp(&b.1.y));
        let hit = order.into_iter().find(|(_, t)| {
            let half = park::width(t) as f64 * unit * 0.5;
            let gy = h - 3.0 - t.y as f64 * h;
            (x - t.x as f64 * w).abs() <= half && y >= gy - tall(t) * unit && y <= gy + 6.0
        }).map(|(k, _)| k);
        me.sel = hit;
        me.drag_from = hit.map(|k| sh.park.things[k].x);
        me.drag_y = hit.map_or(0.0, |k| sh.park.things[k].y);
        g2.queue_draw();
    });
    let (s2, m2, g2) = (sh.clone(), me.clone(), ground.clone());
    drag.connect_drag_update(move |_, dx, dy| {
        let mut sh = s2.borrow_mut();
        let me = m2.borrow();
        let (Some(k), Some(from)) = (me.sel, me.drag_from) else { return };
        let w = g2.width().max(1) as f64;
        let unit = unit_px(&sh, w);
        let half = (park::width(&sh.park.things[k]) as f64 * unit * 0.5 / w) as f32;
        // on the farm, not out over the pond
        let right = if me.place == "farm" { 1.0 - park::POND } else { 1.0 };
        sh.park.things[k].x = (from + (dx / w) as f32).clamp(half, (right - half).max(half));
        // on the farm: up and down the meadow too
        if me.place == "farm" {
            let h = g2.height().max(1) as f64;
            sh.park.things[k].y = (me.drag_y - (dy / h) as f32).clamp(0.0, park::MEADOW);
        }
        g2.queue_draw();
    });
    let (s2, m2) = (sh.clone(), me.clone());
    drag.connect_drag_end(move |_, _, _| {
        if m2.borrow_mut().drag_from.take().is_some() {
            save(&s2.borrow());
        }
    });
    ground.add_controller(drag);

    // top left: the chest (park, farm) and the pantry (cafe, farm)
    let cards = gtk::Box::builder().spacing(12).halign(gtk::Align::Start).valign(gtk::Align::Start).margin_start(18).margin_top(18).build();
    if place != "cafe" {
        cards.append(&chest_card());
    }
    if place != "park" {
        cards.append(&pantry_card());
    }

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
        let (sh, me, bar, name, bigger) = (sh.clone(), me.clone(), bar.clone(), name.clone(), bigger.clone());
        Rc::new(move || {
            let sh = sh.borrow();
            let me = me.borrow();
            match me.sel.and_then(|k| sh.park.things.get(k)) {
                Some(t) => {
                    let label = park::recipe(&t.kind).map_or(t.kind.as_str(), |r| r.1);
                    name.set_label(&if t.kind == "slide" { format!("{label} ({} pieces)", t.pieces) } else { label.to_string() });
                    bigger.set_visible(t.kind == "slide" && t.pieces < 5);
                    bar.set_visible(true);
                }
                None => bar.set_visible(false),
            }
        })
    };
    {
        let sb = sync_bar.clone();
        let click = gtk::GestureClick::new();
        click.connect_released(move |_, _, _, _| sb());
        ground.add_controller(click);
    }
    let (s2, m2, g2, sb) = (sh.clone(), me.clone(), ground.clone(), sync_bar.clone());
    turn.connect_clicked(move |_| {
        if let Some(k) = m2.borrow().sel {
            let mut sh = s2.borrow_mut();
            sh.park.things[k].flip = !sh.park.things[k].flip;
            save(&sh);
        }
        g2.queue_draw();
        sb();
    });
    let (s2, m2, g2, sb, t2) = (sh.clone(), me.clone(), ground.clone(), sync_bar.clone(), toast.clone());
    bigger.connect_clicked(move |_| {
        let Some(k) = m2.borrow().sel else { return };
        let mut sh = s2.borrow_mut();
        let cost = park::recipe("slide_piece").map_or(&[][..], |r| r.2);
        if !sh.park.free && !park::spend(cost) {
            t2.add_toast(adw::Toast::new("Not enough in the chest for a piece"));
            return;
        }
        sh.park.things[k].pieces = (sh.park.things[k].pieces + 1).min(5);
        save(&sh);
        drop(sh);
        g2.queue_draw();
        sb();
    });
    let (s2, m2, g2, sb) = (sh.clone(), me.clone(), ground.clone(), sync_bar.clone());
    remove.connect_clicked(move |_| {
        let Some(k) = m2.borrow_mut().sel.take() else { return };
        let mut sh = s2.borrow_mut();
        let t = sh.park.things.remove(k);
        if !sh.park.free {
            if let Some(r) = park::recipe(&t.kind) {
                park::refund(r.2);
            }
            if let Some(r) = park::recipe("slide_piece") {
                for _ in 1..t.pieces {
                    park::refund(r.2);
                }
            }
        }
        save(&sh);
        drop(sh);
        g2.queue_draw();
        sb();
    });
    let (m2, g2, sb) = (me.clone(), ground.clone(), sync_bar.clone());
    done.connect_clicked(move |_| {
        m2.borrow_mut().sel = None;
        g2.queue_draw();
        sb();
    });

    let overlay = gtk::Overlay::builder().child(&ground).build();
    overlay.add_overlay(&cards);
    overlay.add_overlay(&bar);
    toast.set_child(Some(&overlay));

    // building: what can be made here and what it takes
    let list = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).margin_top(6).margin_bottom(6).margin_start(6).margin_end(6).build();
    let mut buttons = Vec::new();
    for &(id, label, cost, places) in park::RECIPES.iter().filter(|r| r.0 != "slide_piece" && r.3.contains(&place)) {
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
        buttons.push((b, id, cost));
        let _ = places;
    }
    let free = gtk::CheckButton::builder().label("Free building (trying out)").active(sh.borrow().park.free).margin_top(6).build();
    list.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    list.append(&free);
    let scroll = gtk::ScrolledWindow::builder().child(&list).propagate_natural_height(true).max_content_height(620).hscrollbar_policy(gtk::PolicyType::Never).build();
    let pop = gtk::Popover::builder().child(&scroll).build();
    let build_btn = gtk::MenuButton::builder().label("Build").popover(&pop).build();
    build_btn.add_css_class("suggested-action");
    let s2 = sh.clone();
    free.connect_toggled(move |c| {
        let mut sh = s2.borrow_mut();
        sh.park.free = c.is_active();
        save(&sh);
    });
    let sync_build = {
        let (sh, buttons) = (sh.clone(), buttons.iter().map(|(b, _, c)| (b.clone(), *c)).collect::<Vec<_>>());
        Rc::new(move || {
            let chest = park::read_chest();
            let free = sh.borrow().park.free;
            for (b, cost) in &buttons {
                b.set_sensitive(free || park::affordable(&chest, cost));
            }
        })
    };
    {
        let sb = sync_build.clone();
        pop.connect_show(move |_| sb());
        let sb = sync_build.clone();
        free.connect_toggled(move |_| sb());
    }
    for (b, id, cost) in buttons {
        let (s2, m2, g2, sb, sbar, t2, pop) = (sh.clone(), me.clone(), ground.clone(), sync_build.clone(), sync_bar.clone(), toast.clone(), pop.clone());
        b.connect_clicked(move |_| {
            let mut sh = s2.borrow_mut();
            if !sh.park.free && !park::spend(cost) {
                t2.add_toast(adw::Toast::new("Not enough in the chest"));
                return;
            }
            // somewhere free near the middle (of the land, on the farm)
            let w = g2.width().max(1) as f64;
            let unit = unit_px(&sh, w);
            let mut thing = park::Thing { kind: id.to_string(), place: place.to_string(), id: new_id(), x: 0.5, y: 0.0, flip: false, pieces: 1 };
            let me_half = park::width(&thing) as f64 * unit * 0.5 / w;
            let right = if place == "farm" { 1.0 - park::POND as f64 } else { 1.0 };
            let middle = right * 0.5;
            let clear = |x: f64, sh: &Shared| sh.park.things.iter().filter(|t| t.place == place).all(|t| (t.x as f64 - x).abs() > me_half + park::width(t) as f64 * unit * 0.5 / w);
            let spots = (0..60).map(|j| middle + (if j % 2 == 0 { 1.0 } else { -1.0 }) * (j / 2) as f64 * 0.02);
            if let Some(x) = spots.filter(|x| *x > me_half && *x < right - me_half).find(|&x| clear(x, &sh)) {
                thing.x = x as f32;
            }
            sh.park.things.push(thing);
            let k = sh.park.things.len() - 1;
            save(&sh);
            drop(sh);
            m2.borrow_mut().sel = Some(k);
            pop.popdown();
            g2.queue_draw();
            sb();
            sbar();
        });
    }

    // where they live
    let labels: Vec<&str> = MODES.iter().map(|m| m.0).collect();
    let mode = gtk::DropDown::from_strings(&labels);
    mode.set_tooltip_text(Some("Where the characters live while the park, cafe or farm is open"));
    let now = config::tuning_value("park_mode").unwrap_or(3.0).round();
    mode.set_selected(MODES.iter().position(|m| m.1 == now).unwrap_or(1) as u32);
    mode.connect_selected_notify(|d| {
        let v = MODES.get(d.selected() as usize).map_or(3.0, |m| m.1);
        config::set_tuning("park_mode", (v != 3.0).then_some(v));
        if daemon::running() {
            daemon::reload();
        }
    });
    // what they're there for
    let labels: Vec<&str> = ACTIVITIES.iter().map(|m| m.0).collect();
    let what = gtk::DropDown::from_strings(&labels);
    what.set_tooltip_text(Some("Only play: no hunger, farming, fishing or cooking; playing tires them and they nap it off. Work first: farming, fishing and cooking come before games."));
    let now = config::tuning_value("activity").unwrap_or(0.0).round();
    what.set_selected(ACTIVITIES.iter().position(|m| m.1 == now).unwrap_or(0) as u32);
    what.connect_selected_notify(|d| {
        let v = ACTIVITIES.get(d.selected() as usize).map_or(0.0, |m| m.1);
        config::set_tuning("activity", (v != 0.0).then_some(v));
        if daemon::running() {
            daemon::reload();
        }
    });
    let header = adw::HeaderBar::new();
    header.pack_start(&build_btn);
    if place == "cafe" {
        let book = gtk::MenuButton::builder().label("Recipes").build();
        let pop = gtk::Popover::new();
        book.set_popover(Some(&pop));
        let p2 = pop.clone();
        pop.connect_show(move |_| p2.set_child(Some(&recipe_book())));
        header.pack_start(&book);
    }
    // who does what
    let jobs = gtk::MenuButton::builder().label("People").build();
    let jpop = gtk::Popover::new();
    jobs.set_popover(Some(&jpop));
    let j2 = jpop.clone();
    jpop.connect_show(move |_| j2.set_child(Some(&jobs_list())));
    header.pack_start(&jobs);
    header.pack_end(&mode);
    header.pack_end(&what);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&toast));
    win.set_content(Some(&view));

    let css = gtk::CssProvider::new();
    css.load_from_string(".park-chest { padding: 12px 14px; min-width: 170px; background-color: @window_bg_color; }");
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }

    // keeping up with the daemon: news ten times a second
    let (s2, g2, w2) = (sh.clone(), ground.clone(), win.clone());
    let mut last_slow = std::time::Instant::now() - std::time::Duration::from_secs(5);
    glib::timeout_add_local(std::time::Duration::from_millis(100), move || {
        if !w2.is_visible() && w2.application().is_none() {
            return glib::ControlFlow::Break;
        }
        let mut sh = s2.borrow_mut();
        let m = std::fs::metadata(park::world_path()).and_then(|m| m.modified()).ok();
        let mut redraw = false;
        if m != sh.world_mtime {
            sh.world_mtime = m;
            // the animals carry on from where they're drawn now
            let from: Vec<(f32, f32, f32)> = sh.shown.iter().map(|a| (a.1, a.4, a.3)).collect();
            let gap = sh.anim_t0.elapsed().as_secs_f32().clamp(0.1, 0.6);
            sh.anim_gap = sh.anim_gap * 0.7 + gap * 0.3;
            sh.anim_t0 = std::time::Instant::now();
            sh.world = park::read_world();
            sh.anim_from = if from.len() == sh.world.animals.len() { from } else { sh.world.animals.iter().map(|a| (a.x, a.y, a.hop)).collect() };
            redraw = true;
        }
        if last_slow.elapsed().as_secs_f32() > 2.0 {
            last_slow = std::time::Instant::now();
            sh.unit_frac = park::read_unit_frac();
            sh.farm = park::read_farm();
            redraw = true;
        }
        drop(sh);
        if redraw {
            g2.queue_draw();
        }
        glib::ControlFlow::Continue
    });
    // every frame while something moves (only while the window is shown):
    // animals on the farm, steam and smoke over a pot
    let (s2, m2) = (sh.clone(), me.clone());
    let last = std::cell::Cell::new(std::time::Instant::now());
    ground.add_tick_callback(move |g, _| {
        let sh = s2.borrow();
        let place = m2.borrow().place;
        let animals = place == "farm" && !sh.world.animals.is_empty();
        let steam = place != "farm" && !sh.world.stoves.is_empty();
        drop(sh);
        // animals at 30 a second, the pond's ripples and steam at 20: smooth
        // enough, and each frame repaints the whole window
        let every = if animals { 0.033 } else { 0.05 };
        if (animals || steam || place == "farm") && last.get().elapsed().as_secs_f32() >= every {
            last.set(std::time::Instant::now());
            g.queue_draw();
        }
        glib::ControlFlow::Continue
    });

    win.maximize();
    win.present();
    // a workspace of its own, so it doesn't sit among other windows
    let t = title.to_string();
    std::thread::spawn(move || own_workspace(&t));
}

/// The chest: building materials found while fishing.
fn chest_card() -> gtk::Box {
    let card = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).valign(gtk::Align::Start).build();
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
    let who = gtk::Label::builder().xalign(0.0).wrap(true).max_width_chars(24).build();
    who.add_css_class("dim-label");
    who.add_css_class("caption");
    card.append(&who);
    let refresh = move || {
        let chest = park::read_chest();
        for (id, n) in &counts {
            n.set_label(&park::count(&chest, id).to_string());
        }
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
    glib::timeout_add_seconds_local(2, move || {
        refresh();
        glib::ControlFlow::Continue
    });
    card
}

/// The pantry: what the kitchen has to cook with.
fn pantry_card() -> gtk::Box {
    let card = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).valign(gtk::Align::Start).build();
    card.add_css_class("card");
    card.add_css_class("park-chest");
    let title = gtk::Label::builder().label("Pantry").xalign(0.0).build();
    title.add_css_class("heading");
    card.append(&title);
    let mut counts = Vec::new();
    for (id, label) in park::INGREDIENTS {
        let row = gtk::Box::builder().spacing(8).build();
        let pic = gtk::Image::from_file(park::food_picture(id));
        pic.set_pixel_size(26);
        row.append(&pic);
        row.append(&gtk::Label::builder().label(label).xalign(0.0).hexpand(true).build());
        let n = gtk::Label::builder().label("0").xalign(1.0).build();
        n.add_css_class("numeric");
        row.append(&n);
        card.append(&row);
        counts.push((id, n));
    }
    let refresh = move || {
        let p = park::read_pantry();
        for (id, n) in &counts {
            n.set_label(&p.get(*id).copied().unwrap_or(0).to_string());
        }
    };
    refresh();
    glib::timeout_add_seconds_local(2, move || {
        refresh();
        glib::ControlFlow::Continue
    });
    card
}

/// The jobs: who has each, and who's closest to getting one.
fn jobs_list() -> gtk::Widget {
    let j = park::read_jobs();
    let list = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(10).margin_top(8).margin_bottom(8).margin_start(8).margin_end(8).build();
    let note = gtk::Label::builder().label("Earned by doing the work; not everyone gets one").xalign(0.0).build();
    note.add_css_class("dim-label");
    note.add_css_class("caption");
    list.append(&note);
    for (id, label, place, slots) in park::JOBS {
        let holders: Vec<&String> = j.jobs.iter().filter(|e| e.1 == id).map(|e| e.0).collect();
        let head = gtk::Label::builder()
            .label(format!("<b>{label}</b> <small>({}, {}/{slots})</small>", park::place_title(place), holders.len()))
            .use_markup(true)
            .xalign(0.0)
            .build();
        list.append(&head);
        let text = if holders.is_empty() {
            // the closest ones without a job
            let mut near: Vec<(&String, f32)> = j.points.iter().filter(|(n, _)| !j.jobs.contains_key(*n)).filter_map(|(n, p)| p.get(id).map(|v| (n, *v))).collect();
            near.sort_by(|a, b| b.1.total_cmp(&a.1));
            if near.is_empty() {
                "Nobody yet".to_string()
            } else {
                near.iter().take(2).map(|(n, v)| format!("{n} {}/{:.0}", v.min(park::JOB_AT).floor(), park::JOB_AT)).collect::<Vec<_>>().join(", ")
            }
        } else {
            holders.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
        };
        let l = gtk::Label::builder().label(text).xalign(0.0).wrap(true).max_width_chars(36).build();
        if holders.is_empty() {
            l.add_css_class("dim-label");
        }
        list.append(&l);
        let part: Vec<&String> = j.part.iter().filter(|e| e.1 == id).map(|e| e.0).collect();
        if !part.is_empty() {
            let p = gtk::Label::builder().label(format!("Part-time ({}/{}): {}", part.len(), park::PART_SLOTS, part.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "))).xalign(0.0).wrap(true).max_width_chars(36).build();
            p.add_css_class("caption");
            list.append(&p);
        }
    }
    // how hungry each one is, and its favourite dish
    list.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    let head = gtk::Label::builder().label("<b>Hunger and favourite dishes</b>").use_markup(true).xalign(0.0).build();
    list.append(&head);
    let world = park::read_world();
    let mut lines: Vec<String> = world
        .hunger
        .iter()
        .map(|(n, h)| {
            let full = ((1.0 - h) * 100.0).round() as i32;
            let mood = if *h >= 0.9 { " · starving" } else if *h >= 0.55 { " · hungry" } else { "" };
            format!("{n}: {full}% full{mood} · ♥ {}", park::dish_label(park::favorite(n)))
        })
        .collect();
    lines.sort();
    let l = gtk::Label::builder().label(if lines.is_empty() { "Nobody in the windows yet".to_string() } else { lines.join("\n") }).xalign(0.0).wrap(true).max_width_chars(44).build();
    list.append(&l);
    gtk::ScrolledWindow::builder().child(&list).propagate_natural_height(true).max_content_height(640).min_content_width(300).build().upcast()
}

/// Every dish: what's been found (who by, how often made), the rest a "???".
fn recipe_book() -> gtk::Widget {
    let book = park::read_book();
    let grid = gtk::Grid::builder().row_spacing(8).column_spacing(10).margin_top(8).margin_bottom(8).margin_start(8).margin_end(8).build();
    let found = book.found.len();
    let head = gtk::Label::builder().label(format!("{found} of {} found", park::DISHES.len())).xalign(0.0).build();
    head.add_css_class("heading");
    grid.attach(&head, 0, 0, 2, 1);
    for (k, (id, label, stuff)) in park::DISHES.iter().enumerate() {
        let row = k as i32 + 1;
        match book.found.get(*id) {
            Some(f) => {
                let pic = gtk::Image::from_file(park::food_picture(id));
                pic.set_pixel_size(32);
                grid.attach(&pic, 0, row, 1, 1);
                let names: Vec<&str> = stuff.iter().map(|s| park::INGREDIENTS.iter().find(|i| i.0 == *s).map_or(*s, |i| i.1)).collect();
                let text = gtk::Label::builder()
                    .label(format!("<b>{label}</b>\n<small>{} · found by {} · made {}×</small>", names.join(" + "), glib::markup_escape_text(&f.by), f.times))
                    .use_markup(true)
                    .xalign(0.0)
                    .build();
                grid.attach(&text, 1, row, 1, 1);
            }
            None => {
                let pic = gtk::Label::builder().label("?").width_chars(3).build();
                pic.add_css_class("title-2");
                pic.add_css_class("dim-label");
                grid.attach(&pic, 0, row, 1, 1);
                let text = gtk::Label::builder().label("???").xalign(0.0).build();
                text.add_css_class("dim-label");
                grid.attach(&text, 1, row, 1, 1);
            }
        }
    }
    gtk::ScrolledWindow::builder().child(&grid).propagate_natural_height(true).max_content_height(600).min_content_width(320).build().upcast()
}

/// A PNG (cached at that size), mirrored if asked.
fn draw_picture(sh: &mut Shared, cr: &gtk::cairo::Context, path: &std::path::Path, x: f64, y: f64, size: f64, mirror: bool) {
    let px = size.round().max(4.0) as i32;
    let key = (path.to_string_lossy().into_owned(), px);
    let pic = sh
        .pics
        .entry(key)
        .or_insert_with(|| {
            let pb = gdk_pixbuf::Pixbuf::from_file_at_scale(path, px, px, true).ok()?;
            let surface = gtk::cairo::ImageSurface::create(gtk::cairo::Format::ARgb32, pb.width(), pb.height()).ok()?;
            let c = gtk::cairo::Context::new(&surface).ok()?;
            c.set_source_pixbuf(&pb, 0.0, 0.0);
            c.paint().ok()?;
            drop(c);
            surface.flush();
            Some(surface)
        })
        .clone();
    let Some(pic) = pic else { return };
    let (pw, ph) = (pic.width() as f64, pic.height() as f64);
    cr.save().ok();
    cr.translate(x + (size - pw) * 0.5, y + (size - ph));
    if mirror {
        cr.translate(pw, 0.0);
        cr.scale(-1.0, 1.0);
    }
    cr.set_source_surface(&pic, 0.0, 0.0).ok();
    cr.paint().ok();
    cr.restore().ok();
}

/// What goes with a thing right now: crops as they grow, eggs and milk,
/// steam over a cooking pot, a dish on a table.
#[allow(clippy::too_many_arguments)]
fn draw_extras(sh: &mut Shared, cr: &gtk::cairo::Context, t: &park::Thing, x: f64, gy: f64, u: f64, grown: f32, ready: u32, time: f64) {
    let g = grown as f64;
    match t.kind.as_str() {
        "stove" => {
            let Some((_, state, _)) = sh.world.stoves.iter().find(|s| s.0 == t.id).cloned() else { return };
            let dir = if t.flip { -1.0 } else { 1.0 };
            let pot = (x - dir * 0.2 * u, gy - 0.82 * u);
            match state.as_str() {
                "cooking" => {
                    for k in 0..4 {
                        let f = ((time * 0.6 + k as f64 * 0.25) % 1.0) as f64;
                        cr.set_source_rgba(1.0, 1.0, 1.0, 0.55 * (1.0 - f));
                        cr.arc(pot.0 + (f * 9.0 + k as f64).sin() * 0.06 * u, pot.1 - f * 0.6 * u, (0.05 + f * 0.08) * u, 0.0, std::f64::consts::TAU);
                        cr.fill().ok();
                    }
                }
                "burnt" => {
                    for k in 0..6 {
                        let f = ((time * 0.8 + k as f64 / 6.0) % 1.0) as f64;
                        cr.set_source_rgba(0.2, 0.2, 0.22, 0.7 * (1.0 - f));
                        cr.arc(pot.0 + (f * 7.0 + k as f64).sin() * 0.1 * u, pot.1 - f * 0.9 * u, (0.07 + f * 0.14) * u, 0.0, std::f64::consts::TAU);
                        cr.fill().ok();
                    }
                }
                _ => {
                    // ready: little stars over the pot
                    cr.set_source_rgba(1.0, 0.85, 0.2, 0.9);
                    for k in 0..5 {
                        let a = time * 2.0 + k as f64 * 1.26;
                        let (sx, sy) = (pot.0 + a.cos() * 0.25 * u, pot.1 - 0.25 * u + a.sin() * 0.12 * u);
                        cr.arc(sx, sy, 0.03 * u, 0.0, std::f64::consts::TAU);
                        cr.fill().ok();
                    }
                }
            }
        }
        "table" => {
            // one dish each: two sat at it, one at each end in front of them
            let on: Vec<_> = sh.world.dishes.iter().filter(|d| d.0 == t.id).cloned().collect();
            for (k, (_, dish, left)) in on.iter().enumerate() {
                let off = if on.len() < 2 { 0.0 } else { (k as f64 / (on.len() - 1) as f64 - 0.5) * 0.6 * u };
                let size = 0.4 * u * (0.35 + 0.65 * *left as f64);
                draw_picture(sh, cr, &park::food_picture(dish), x + off - size * 0.5, gy - 0.66 * u - size * 0.9, size, false);
            }
        }
        "stall" => {
            // a little striped stand with its snacks on show
            rgb(cr, DARK);
            for sx in [-0.5, 0.5] {
                bar(cr, (x + sx * u, gy), (x + sx * u, gy - 1.1 * u), 0.05 * u);
            }
            rgb(cr, WOOD);
            rect(cr, x - 0.58 * u, gy - 0.55 * u, x + 0.58 * u, gy);
            for k in 0..6 {
                let (a, b) = (x - 0.62 * u + k as f64 * 0.207 * u, x - 0.62 * u + (k + 1) as f64 * 0.207 * u);
                if k % 2 == 0 {
                    cr.set_source_rgb(0.92, 0.3, 0.32);
                } else {
                    cr.set_source_rgb(0.98, 0.95, 0.9);
                }
                rect(cr, a, gy - 1.25 * u, b, gy - 1.05 * u);
            }
            let size = 0.26 * u;
            for (k, snack) in park::SNACKS.iter().enumerate() {
                draw_picture(sh, cr, &park::food_picture(snack), x - 0.4 * u + k as f64 * 0.42 * u, gy - 0.55 * u - size * 0.9, size, false);
            }
        }
        "counter" => {
            // the long wooden counter with a bell, and whatever is waiting on it
            rgb(cr, DARK);
            rect(cr, x - 0.72 * u, gy - 0.62 * u, x + 0.72 * u, gy);
            rgb(cr, WOOD);
            rect(cr, x - 0.68 * u, gy - 0.58 * u, x + 0.68 * u, gy - 0.04 * u);
            cr.set_source_rgb(0.85, 0.85, 0.82);
            rect(cr, x - 0.8 * u, gy - 0.68 * u, x + 0.8 * u, gy - 0.6 * u);
            cr.set_source_rgb(0.85, 0.7, 0.25);
            let bx = x + if t.flip { -0.62 } else { 0.62 } * u;
            cr.arc(bx, gy - 0.69 * u, 0.06 * u, std::f64::consts::PI, 0.0);
            cr.fill().ok();
            let waiting: Vec<String> = sh.world.dishes.iter().filter(|d| d.0 == t.id).map(|d| d.1.clone()).collect();
            // in a heap: four along the counter, three on top of them, two, one...
            let size = 0.34 * u;
            let (mut row, mut col, mut in_row) = (0usize, 0usize, 4usize);
            for dish in &waiting {
                let dx = (col as f64 - (in_row as f64 - 1.0) * 0.5) * 0.36 * u;
                let dy = row as f64 * size * 0.62;
                draw_picture(sh, cr, &park::food_picture(dish), x + dx - size * 0.5, gy - 0.68 * u - size * 0.9 - dy, size, false);
                col += 1;
                if col >= in_row {
                    row += 1;
                    col = 0;
                    in_row = in_row.saturating_sub(1).max(1);
                }
            }
        }
        "carrots" => {
            cr.set_source_rgb(0.45, 0.3, 0.18);
            rect(cr, x - 0.5 * u, gy - 0.1 * u, x + 0.5 * u, gy);
            for k in 0..5 {
                let px = x - 0.4 * u + k as f64 * 0.2 * u;
                if ready > 0 {
                    cr.set_source_rgb(0.95, 0.5, 0.12);
                    rect(cr, px - 0.035 * u, gy - 0.16 * u, px + 0.035 * u, gy - 0.08 * u);
                }
                cr.set_source_rgb(0.3, 0.62, 0.25);
                let tall = (0.06 + 0.22 * g) * u;
                for d in [-0.04, 0.0, 0.04] {
                    bar(cr, (px, gy - 0.1 * u), (px + d * u * 1.5, gy - 0.1 * u - tall), 0.02 * u);
                }
            }
        }
        "wheat" => {
            cr.set_source_rgb(0.45, 0.3, 0.18);
            rect(cr, x - 0.6 * u, gy - 0.06 * u, x + 0.6 * u, gy);
            let (r, gr, b) = (0.35 + 0.55 * g, 0.62 + 0.15 * g, 0.25 - 0.1 * g);
            for k in 0..9 {
                let px = x - 0.52 * u + k as f64 * 0.13 * u;
                let tall = (0.12 + 0.62 * g) * u;
                let sway = (time * 1.3 + k as f64).sin() * 0.02 * u;
                cr.set_source_rgb(r, gr, b);
                bar(cr, (px, gy - 0.05 * u), (px + sway, gy - 0.05 * u - tall), 0.018 * u);
                if g > 0.6 {
                    cr.set_source_rgb(0.93, 0.78, 0.3);
                    bar(cr, (px + sway, gy - 0.05 * u - tall), (px + sway, gy - 0.05 * u - tall + 0.12 * u), 0.045 * u);
                }
            }
        }
        "apple_tree" => {
            rgb(cr, DARK);
            rect(cr, x - 0.09 * u, gy - 0.95 * u, x + 0.09 * u, gy);
            cr.set_source_rgb(0.32, 0.62, 0.3);
            for (dx, dy, r) in [(0.0, 1.4, 0.45), (-0.35, 1.1, 0.35), (0.35, 1.1, 0.35), (0.0, 1.0, 0.38)] {
                cr.arc(x + dx * u, gy - dy * u, r * u, 0.0, std::f64::consts::TAU);
                cr.fill().ok();
            }
            let n = if ready > 0 { 5 } else { (g * 4.0) as usize };
            for (k, (dx, dy)) in [(-0.3, 1.15), (0.25, 1.3), (0.05, 0.98), (0.38, 1.05), (-0.12, 1.5)].iter().enumerate().take(n) {
                if ready > 0 {
                    cr.set_source_rgb(0.9, 0.15, 0.15);
                } else {
                    cr.set_source_rgb(0.55, 0.75, 0.25);
                }
                let _ = k;
                cr.arc(x + dx * u, gy - dy * u, if ready > 0 { 0.06 } else { 0.035 } * u, 0.0, std::f64::consts::TAU);
                cr.fill().ok();
            }
        }
        "tree" => {
            // logs fallen under it, waiting to be picked up
            for k in 0..ready {
                let lx = x - 0.35 * u + k as f64 * 0.28 * u;
                cr.set_source_rgb(0.55, 0.36, 0.2);
                rect(cr, lx - 0.12 * u, gy - 0.1 * u, lx + 0.12 * u, gy);
                cr.set_source_rgb(0.85, 0.7, 0.45);
                cr.arc(lx + 0.12 * u, gy - 0.05 * u, 0.05 * u, 0.0, std::f64::consts::TAU);
                cr.fill().ok();
            }
            let _ = g;
        }
        "coop" => {
            cr.set_source_rgb(0.85, 0.75, 0.55);
            rect(cr, x - 0.45 * u, gy - 0.6 * u, x + 0.45 * u, gy);
            cr.set_source_rgb(0.75, 0.22, 0.2);
            cr.move_to(x - 0.55 * u, gy - 0.58 * u);
            cr.line_to(x, gy - 0.95 * u);
            cr.line_to(x + 0.55 * u, gy - 0.58 * u);
            cr.close_path();
            cr.fill().ok();
            cr.set_source_rgb(0.3, 0.2, 0.12);
            rect(cr, x - 0.12 * u, gy - 0.32 * u, x + 0.12 * u, gy);
            cr.set_source_rgb(0.98, 0.96, 0.9);
            for k in 0..ready {
                let ex = x + 0.25 * u + k as f64 * 0.11 * u;
                cr.save().ok();
                cr.translate(ex, gy - 0.05 * u);
                cr.scale(0.045 * u, 0.06 * u);
                cr.arc(0.0, 0.0, 1.0, 0.0, std::f64::consts::TAU);
                cr.restore().ok();
                cr.fill().ok();
            }
        }
        "barn" => {
            cr.set_source_rgb(0.72, 0.18, 0.15);
            rect(cr, x - 0.75 * u, gy - 0.95 * u, x + 0.75 * u, gy);
            cr.move_to(x - 0.85 * u, gy - 0.93 * u);
            cr.line_to(x, gy - 1.35 * u);
            cr.line_to(x + 0.85 * u, gy - 0.93 * u);
            cr.close_path();
            cr.set_source_rgb(0.4, 0.15, 0.12);
            cr.fill().ok();
            cr.set_source_rgb(0.97, 0.95, 0.9);
            rect(cr, x - 0.3 * u, gy - 0.6 * u, x + 0.3 * u, gy);
            cr.set_source_rgb(0.72, 0.18, 0.15);
            bar(cr, (x - 0.27 * u, gy - 0.57 * u), (x + 0.27 * u, gy - 0.03 * u), 0.05 * u);
            bar(cr, (x + 0.27 * u, gy - 0.57 * u), (x - 0.27 * u, gy - 0.03 * u), 0.05 * u);
            if ready > 0 {
                let size = 0.32 * u;
                draw_picture(sh, cr, &park::food_picture("milk"), x + 0.85 * u, gy - size, size, false);
            }
        }
        _ => {}
    }
}

/// A character's height in window pixels.
fn unit_px(sh: &Shared, w: f64) -> f64 {
    sh.unit_frac.map_or(110.0, |f| f as f64 * w).max(30.0)
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
        "tree" | "apple_tree" => 1.9,
        "coop" => 1.0,
        "counter" => 0.75,
        "stall" => 1.3,
        "barn" => 1.4,
        "wheat" => 0.8,
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

/// The cafe: a warm wall with windows, wood panelling, a wooden floor.
fn draw_cafe(cr: &gtk::cairo::Context, w: f64, h: f64) {
    let wall = gtk::cairo::LinearGradient::new(0.0, 0.0, 0.0, h);
    wall.add_color_stop_rgb(0.0, 0.98, 0.9, 0.78);
    wall.add_color_stop_rgb(1.0, 0.95, 0.84, 0.7);
    let _ = cr.set_source(&wall);
    cr.paint().ok();
    // windows with a bit of sky
    let (wy, wh) = (h * 0.14, h * 0.32);
    for k in 0..3 {
        let wx = w * (0.22 + 0.28 * k as f64);
        let ww = w * 0.13;
        cr.set_source_rgb(0.55, 0.38, 0.22);
        rect(cr, wx - 6.0, wy - 6.0, wx + ww + 6.0, wy + wh + 6.0);
        let sky = gtk::cairo::LinearGradient::new(0.0, wy, 0.0, wy + wh);
        sky.add_color_stop_rgb(0.0, 0.55, 0.78, 0.97);
        sky.add_color_stop_rgb(1.0, 0.82, 0.93, 1.0);
        let _ = cr.set_source(&sky);
        cr.rectangle(wx, wy, ww, wh);
        cr.fill().ok();
        cr.set_source_rgb(0.55, 0.38, 0.22);
        rect(cr, wx + ww * 0.5 - 3.0, wy, wx + ww * 0.5 + 3.0, wy + wh);
        rect(cr, wx, wy + wh * 0.5 - 3.0, wx + ww, wy + wh * 0.5 + 3.0);
    }
    // lamps hanging from the ceiling
    for k in 0..4 {
        let lx = w * (0.12 + 0.26 * k as f64);
        cr.set_source_rgb(0.3, 0.25, 0.2);
        bar(cr, (lx, 0.0), (lx, h * 0.08), 2.0);
        cr.set_source_rgb(0.95, 0.75, 0.35);
        cr.arc(lx, h * 0.095, h * 0.022, std::f64::consts::PI, 0.0);
        cr.fill().ok();
        cr.set_source_rgba(1.0, 0.9, 0.6, 0.25);
        cr.arc(lx, h * 0.11, h * 0.05, 0.0, std::f64::consts::TAU);
        cr.fill().ok();
    }
    // panelling and floor
    let panel = h * 0.68;
    cr.set_source_rgb(0.7, 0.5, 0.32);
    rect(cr, 0.0, panel, w, h);
    cr.set_source_rgb(0.6, 0.42, 0.26);
    rect(cr, 0.0, panel - 6.0, w, panel + 4.0);
    let mut x = 0.0;
    while x < w {
        rect(cr, x, panel + 4.0, x + 3.0, h * 0.92);
        x += w / 24.0;
    }
    cr.set_source_rgb(0.5, 0.34, 0.2);
    rect(cr, 0.0, h * 0.92, w, h);
}

/// The farm: sky and grass like the park, with a pond on the right and a
/// pier running out over it.
fn draw_farm(cr: &gtk::cairo::Context, w: f64, h: f64, u: f64) {
    draw_park(cr, w, h);
    // the meadow: green from halfway down, rolling at its top edge
    let top = h * (1.0 - park::MEADOW as f64) - 0.12 * u;
    let meadow = gtk::cairo::LinearGradient::new(0.0, top, 0.0, h);
    meadow.add_color_stop_rgb(0.0, 0.5, 0.74, 0.42);
    meadow.add_color_stop_rgb(1.0, 0.35, 0.6, 0.27);
    let _ = cr.set_source(&meadow);
    cr.move_to(0.0, h);
    let steps = 48;
    for k in 0..=steps {
        let x = w * k as f64 / steps as f64;
        let y = top - h * 0.025 * ((x / w * 5.0).sin() + 0.6 * (x / w * 11.0 + 2.0).sin());
        cr.line_to(x, y);
    }
    cr.line_to(w, h);
    cr.close_path();
    cr.fill().ok();
    // tufts scattered over it
    cr.set_source_rgba(0.28, 0.5, 0.22, 0.6);
    for k in 0..60 {
        let fx = ((k as f64 * 0.618).fract()) * w;
        let fy = top + 0.05 * h + ((k as f64 * 0.377).fract()) * (h - top - 0.08 * h);
        bar(cr, (fx, fy), (fx + 2.0, fy - 6.0), 2.0);
        bar(cr, (fx + 4.0, fy), (fx + 5.0, fy - 8.0), 2.0);
    }
    let shore = w * (1.0 - park::POND as f64);
    // all the way up the meadow
    let top = h * (1.0 - park::MEADOW as f64) - 0.1 * u;
    // the pond, a little sunk into the grass
    let water = gtk::cairo::LinearGradient::new(0.0, top, 0.0, h);
    water.add_color_stop_rgb(0.0, 0.35, 0.62, 0.85);
    water.add_color_stop_rgb(1.0, 0.18, 0.4, 0.68);
    let _ = cr.set_source(&water);
    // a wavy shore from the top of the meadow down to the floor
    cr.move_to(shore + w * 0.02, top);
    let steps = 20;
    for k in 0..=steps {
        let f = k as f64 / steps as f64;
        let y = top + (h - top) * f;
        let x = shore + w * (0.02 - 0.03 * f) + (f * 9.0).sin() * w * 0.006;
        cr.line_to(x, y);
    }
    cr.line_to(w, h);
    cr.line_to(w, top);
    cr.close_path();
    cr.fill().ok();
    // little jetties up the shore, for more anglers
    let (ja, jb) = (w * park::JETTY.0 as f64, w * park::JETTY.1 as f64);
    for y in park::JETTIES {
        let jy = h - 3.0 - y as f64 * h;
        cr.set_source_rgb(0.42, 0.28, 0.16);
        for px in [ja + 0.25 * (jb - ja), jb - 6.0] {
            rect(cr, px - 3.0, jy - 0.1 * u, px + 3.0, jy + 0.12 * u);
        }
        cr.set_source_rgb(0.62, 0.44, 0.27);
        rect(cr, ja, jy - 0.03 * u, jb, jy + 3.0);
    }
    // the pier: planks at the characters' feet, posts down into the water
    let (a, b) = (w * park::PIER.0 as f64, w * park::PIER.1 as f64);
    let deck = h - 3.0;
    cr.set_source_rgb(0.42, 0.28, 0.16);
    let mut px = a + 8.0;
    while px < b {
        rect(cr, px - 4.0, deck - 0.12 * u, px + 4.0, h);
        px += 0.45 * u;
    }
    cr.set_source_rgb(0.62, 0.44, 0.27);
    rect(cr, a, deck - 0.03 * u, b, deck + 3.0);
    cr.set_source_rgb(0.5, 0.34, 0.2);
    let mut px = a;
    while px < b {
        bar(cr, (px, deck - 0.03 * u), (px, deck + 3.0), 1.0);
        px += 0.16 * u;
    }
}

/// What moves on the farm's background: ripples on the pond, the reeds.
fn draw_farm_live(cr: &gtk::cairo::Context, w: f64, h: f64, time: f64, u: f64) {
    let shore = w * (1.0 - park::POND as f64);
    let top = h * (1.0 - park::MEADOW as f64) - 0.1 * u;
    // ripples all over it
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.35);
    for k in 0..14 {
        let y = top + (h - top) * (0.06 + 0.067 * k as f64);
        let x0 = shore + w * 0.03 + ((time * 0.3 + k as f64 * 0.37).sin() * 0.5 + 0.5) * (w - shore) * 0.55;
        bar(cr, (x0, y), (x0 + w * 0.02, y), 2.0);
    }
    // reeds at the edge
    cr.set_source_rgb(0.3, 0.5, 0.25);
    for k in 0..5 {
        let rx = shore - w * 0.025 + k as f64 * 6.0;
        bar(cr, (rx, h), (rx + (time + k as f64).sin() * 2.0, h - 0.35 * u - k as f64 * 3.0), 2.5);
    }
}

/// Puts a window (by title) beside the other park, cafe or farm windows, or
/// onto an empty workspace of its own when it's the first and opened among others.
fn own_workspace(title: &str) {
    let json = |args: &[&str]| -> Option<serde_json::Value> {
        let out = std::process::Command::new("niri").args(["msg", "--json"]).args(args).output().ok()?;
        serde_json::from_slice(&out.stdout).ok()
    };
    let mut me = None;
    for _ in 0..20 {
        std::thread::sleep(std::time::Duration::from_millis(150));
        let Some(wins) = json(&["windows"]) else { return };
        let list = wins.as_array().cloned().unwrap_or_default();
        if let Some(w) = list.iter().find(|w| w["app_id"].as_str() == Some(park::APP_ID) && w["title"].as_str() == Some(title)) {
            me = Some((w["id"].as_u64(), w["workspace_id"].as_u64(), list));
            break;
        }
    }
    let Some((Some(id), Some(ws), list)) = me else { return };
    let Some(spaces) = json(&["workspaces"]) else { return };
    let spaces = spaces.as_array().cloned().unwrap_or_default();
    // the others already open: in beside them, side by side on their workspace
    let sibling = list
        .iter()
        .find(|w| w["app_id"].as_str() == Some(park::APP_ID) && w["title"].as_str() != Some(title))
        .and_then(|w| w["workspace_id"].as_u64());
    if let Some(other) = sibling {
        if other != ws {
            if let Some(idx) = spaces.iter().find(|s| s["id"].as_u64() == Some(other)).and_then(|s| s["idx"].as_u64()) {
                let _ = std::process::Command::new("niri")
                    .args(["msg", "action", "move-window-to-workspace", "--window-id", &id.to_string(), "--focus", "true", &idx.to_string()])
                    .status();
            }
        }
        return;
    }
    if list.iter().filter(|w| w["workspace_id"].as_u64() == Some(ws)).count() <= 1 {
        return;
    }
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
