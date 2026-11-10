//! The Characters tab: the line-up on the left, characters to add on the
//! right. Changes wait for Apply; right-click a character to see what it's
//! doing and who its friends are.

use crate::pages::{Ui, load_texture, picture};
use crate::{config, daemon, team};
use adw::prelude::*;
use gtk::{gdk, glib};
use std::path::{Path, PathBuf};
use std::rc::Rc;

struct Roster {
    ui: Rc<Ui>,
    cards: gtk::FlowBox,
    summary: gtk::Label,
    apply: gtk::Button,
    pending: gtk::Label,
    lineups: gtk::Box,
}

fn name_of(path: &str) -> String {
    Path::new(path).parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn small(text: &str) -> gtk::Label {
    let l = gtk::Label::builder().label(text).xalign(0.0).build();
    l.add_css_class("dim-label");
    l.add_css_class("caption");
    l
}

fn clear_flow(f: &gtk::FlowBox) {
    while let Some(c) = f.first_child() {
        f.remove(&c);
    }
}

fn clear_box(b: &gtk::Box) {
    while let Some(c) = b.first_child() {
        b.remove(&c);
    }
}

fn thumb(ui: &Ui, path: &str, size: i32) -> gtk::Widget {
    let key = format!("{size}:{path}");
    let mut cache = ui.team_thumbs.borrow_mut();
    if !cache.contains_key(&key) {
        if let Some(t) = load_texture(Path::new(path), size) {
            cache.insert(key.clone(), t);
        }
    }
    picture(cache.get(&key), size)
}

/// Who is on screen right now, as anima's config has it.
fn applied() -> Vec<String> {
    let doc = config::read();
    if !config::boolean(&doc, "character", "enabled", true) {
        return Vec::new();
    }
    let source = config::text(&doc, "character", "source", "");
    if source.is_empty() {
        return Vec::new();
    }
    let extra: Vec<String> = doc
        .get("character")
        .and_then(|c| c.get("extra"))
        .and_then(|e| e.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default();
    std::iter::once(source).chain(extra).collect()
}

/// Send the line-up to anima; it changes in place, no restart.
fn apply(r: &Rc<Roster>) {
    let on = r.ui.data.borrow().enabled();
    let before = applied();
    if on == before {
        refresh(r);
        return;
    }
    // whoever is first in anima's list stays first while it's still in, so
    // nothing has to start over; only turning everyone off or on does
    let mut on = on;
    if let Some(k) = before.first().and_then(|f| on.iter().position(|p| p == f)) {
        let first = on.remove(k);
        on.insert(0, first);
    }
    if on == before {
        refresh(r);
        return;
    }
    let restart = on.is_empty() || before.is_empty();
    config::set_team(&on);
    if r.ui.is_on() {
        if restart {
            daemon::restart();
        } else {
            daemon::reload();
        }
    }
    r.ui.say("Applied");
    refresh(r);
}

/// Change the line-up being edited; nothing reaches the screen until Apply.
fn edit(r: &Rc<Roster>, f: impl FnOnce(&mut team::Team) + 'static) {
    let r = r.clone();
    glib::idle_add_local_once(move || {
        {
            let mut d = r.ui.data.borrow_mut();
            f(&mut d);
            d.save();
        }
        refresh(&r);
    });
}

fn refresh(r: &Rc<Roster>) {
    let members = r.ui.data.borrow().team.clone();
    let on = members.iter().filter(|m| m.on).count();
    r.summary.set_label(&format!("{} characters · {on} on", members.len()));
    let dirty = r.ui.data.borrow().enabled() != applied();
    r.apply.set_sensitive(dirty);
    r.pending.set_visible(dirty);
    clear_flow(&r.cards);
    for (i, m) in members.iter().enumerate() {
        r.cards.append(&card(r, i, m));
    }
    if members.is_empty() {
        r.cards.append(&small("Nobody yet: pick characters on the right."));
    }
    fill_lineups(r);
}

fn card(r: &Rc<Roster>, i: usize, m: &team::Member) -> gtk::Widget {
    let pic = thumb(&r.ui, &m.path, 92);
    pic.set_halign(gtk::Align::Center);
    let name = gtk::Label::builder().label(name_of(&m.path)).ellipsize(gtk::pango::EllipsizeMode::End).max_width_chars(11).build();
    name.add_css_class("caption-heading");
    let sw = gtk::Switch::builder().active(m.on).valign(gtk::Align::Center).tooltip_text("On screen").build();
    let rr = r.clone();
    sw.connect_active_notify(move |w| {
        let on = w.is_active();
        edit(&rr, move |d| {
            if let Some(m) = d.team.get_mut(i) {
                m.on = on;
            }
        });
    });
    let stage = gtk::Overlay::builder().child(&pic).build();
    sw.set_halign(gtk::Align::Center);
    let col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
    col.append(&stage);
    col.append(&name);
    col.append(&sw);
    let frame = gtk::Box::builder().orientation(gtk::Orientation::Vertical).width_request(128).tooltip_text("Right-click for status and friendships").build();
    frame.add_css_class("char-card");
    if !m.on {
        frame.add_css_class("off");
    }
    frame.append(&col);

    // right-click: a small menu, each choice opening its own little view
    let menu = gtk::Popover::new();
    menu.set_parent(&frame);
    let click = gtk::GestureClick::builder().button(3).build();
    let (rr, path) = (r.clone(), m.path.clone());
    click.connect_pressed(move |_, _, x, y| {
        menu.set_child(Some(&choices(&rr, &menu, i, &path)));
        menu.set_pointing_to(Some(&gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
        menu.popup();
    });
    frame.add_controller(click);
    frame.upcast()
}

fn menu_button(label: &str, icon: &str) -> gtk::Button {
    let row = gtk::Box::builder().spacing(8).build();
    row.append(&gtk::Image::from_icon_name(icon));
    row.append(&gtk::Label::new(Some(label)));
    let b = gtk::Button::builder().child(&row).build();
    b.add_css_class("flat");
    b
}

fn choices(r: &Rc<Roster>, pop: &gtk::Popover, i: usize, path: &str) -> gtk::Widget {
    let name = name_of(path);
    let list = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).build();
    let title = gtk::Label::builder().label(&name).xalign(0.0).margin_start(8).margin_bottom(4).build();
    title.add_css_class("heading");
    list.append(&title);
    let status = menu_button("Status", "dialog-information-symbolic");
    let (rr, p, n) = (r.clone(), pop.clone(), name.clone());
    status.connect_clicked(move |_| p.set_child(Some(&status_view(&rr, &p, i, &n))));
    list.append(&status);
    let friends = menu_button("Friendships", "system-users-symbolic");
    let (rr, p, n) = (r.clone(), pop.clone(), name.clone());
    friends.connect_clicked(move |_| p.set_child(Some(&friends_view(&rr, &p, i, &n))));
    list.append(&friends);
    let wear = menu_button("Wardrobe", "applications-graphics-symbolic");
    let (rr, p, n) = (r.clone(), pop.clone(), name.clone());
    wear.connect_clicked(move |_| p.set_child(Some(&wardrobe_view(&rr, &p, i, &n))));
    list.append(&wear);
    let fit = menu_button("Fit accessories", "object-flip-horizontal-symbolic");
    let (rr, p, n, pth) = (r.clone(), pop.clone(), name.clone(), path.to_string());
    fit.connect_clicked(move |_| p.set_child(Some(&fit_view(&rr, &p, i, &n, &pth))));
    list.append(&fit);
    list.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    let remove = menu_button("Remove from line-up", "user-trash-symbolic");
    let (rr, p) = (r.clone(), pop.clone());
    remove.connect_clicked(move |_| {
        p.popdown();
        edit(&rr, move |d| {
            if i < d.team.len() {
                d.team.remove(i);
            }
        });
    });
    list.append(&remove);
    list.upcast()
}

fn back_header(r: &Rc<Roster>, pop: &gtk::Popover, i: usize, name: &str, title: &str) -> gtk::Box {
    let head = gtk::Box::builder().spacing(6).margin_bottom(6).build();
    let back = gtk::Button::builder().icon_name("go-previous-symbolic").tooltip_text("Back").build();
    back.add_css_class("flat");
    let (rr, p) = (r.clone(), pop.clone());
    let path = r.ui.data.borrow().team.get(i).map(|m| m.path.clone()).unwrap_or_default();
    back.connect_clicked(move |_| p.set_child(Some(&choices(&rr, &p, i, &path))));
    head.append(&back);
    let t = gtk::Label::builder().label(format!("{name} · {title}")).xalign(0.0).build();
    t.add_css_class("heading");
    head.append(&t);
    head
}

fn eye_fixes_path() -> std::path::PathBuf {
    anima::util::data_dir().join("eyes.json")
}

fn eye_fixes() -> serde_json::Map<String, serde_json::Value> {
    std::fs::read_to_string(eye_fixes_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn save_eye_fixes(m: &serde_json::Map<String, serde_json::Value>) {
    if let Ok(text) = serde_json::to_string_pretty(m) {
        let _ = anima::util::atomic_write(&eye_fixes_path(), text.as_bytes());
    }
}

/// Click on the character's eyes: its sleep mask goes there from now on.
#[allow(dead_code)]
fn mask_view(r: &Rc<Roster>, pop: &gtk::Popover, i: usize, name: &str, path: &str) -> gtk::Widget {
    let b = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).width_request(260).build();
    b.append(&back_header(r, pop, i, name, "Sleep mask"));
    let hint = gtk::Label::builder().label("Click between its eyes. The mask moves there at once.").wrap(true).xalign(0.0).build();
    hint.add_css_class("dim-label");
    b.append(&hint);
    let pic = gtk::Picture::for_filename(path);
    pic.set_content_fit(gtk::ContentFit::Contain);
    pic.set_size_request(240, 300);
    // the chosen spot, drawn over the picture
    let mark = gtk::DrawingArea::builder().can_target(false).build();
    let spot: Rc<std::cell::Cell<Option<(f64, f64)>>> = Rc::new(std::cell::Cell::new(
        eye_fixes().get(path).and_then(|v| Some((v[0].as_f64()?, v[1].as_f64()?))),
    ));
    let (iw, ih) = image::image_dimensions(path).map(|(w, h)| (w as f64, h as f64)).unwrap_or((1.0, 1.0));
    // where the picture is drawn inside the widget
    let fit = move |w: f64, h: f64| {
        let a = iw / ih;
        if w / h > a { (h * a, h, (w - h * a) * 0.5, 0.0) } else { (w, w / a, 0.0, (h - w / a) * 0.5) }
    };
    let s = spot.clone();
    mark.set_draw_func(move |_, cr, w, h| {
        let Some((x, y)) = s.get() else { return };
        let (dw, dh, ox, oy) = fit(w as f64, h as f64);
        let (cx, cy) = (ox + x * dw, oy + y * dh);
        cr.set_source_rgba(0.98, 0.6, 0.75, 0.85);
        cr.rectangle(cx - dw * 0.22, cy - dh * 0.025, dw * 0.44, dh * 0.05);
        let _ = cr.fill();
    });
    let over = gtk::Overlay::builder().child(&pic).build();
    over.add_overlay(&mark);
    let click = gtk::GestureClick::new();
    let (s, m, key, u) = (spot.clone(), mark.clone(), path.to_string(), r.ui.clone());
    click.connect_pressed(move |g, _, x, y| {
        let Some(w) = g.widget() else { return };
        let (dw, dh, ox, oy) = fit(w.width() as f64, w.height() as f64);
        let (fx, fy) = ((x - ox) / dw, (y - oy) / dh);
        if !(0.0..=1.0).contains(&fx) || !(0.0..=1.0).contains(&fy) {
            return;
        }
        s.set(Some((fx, fy)));
        m.queue_draw();
        let mut all = eye_fixes();
        all.insert(key.clone(), serde_json::json!([fx, fy]));
        save_eye_fixes(&all);
        u.say("Sleep mask moved");
    });
    over.add_controller(click);
    b.append(&over);
    let reset = gtk::Button::with_label("Back to automatic");
    let (s, m, key, u) = (spot.clone(), mark.clone(), path.to_string(), r.ui.clone());
    reset.connect_clicked(move |_| {
        s.set(None);
        m.queue_draw();
        let mut all = eye_fixes();
        all.remove(&key);
        save_eye_fixes(&all);
        u.say("Sleep mask found automatically again");
    });
    b.append(&reset);
    b.upcast()
}

fn wardrobe_all() -> serde_json::Map<String, serde_json::Value> {
    std::fs::read_to_string(anima::render::character::wear::wardrobe_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// What the character wears: a switch for each thing there is. The weather
/// and Christmas put on their own things on top of these.
fn fits_all() -> anima::render::character::wear::Fits {
    std::fs::read_to_string(anima::render::character::wear::fit_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

fn save_fits(f: &anima::render::character::wear::Fits) {
    if let Ok(text) = serde_json::to_string_pretty(f) {
        let _ = anima::util::atomic_write(&anima::render::character::wear::fit_path(), text.as_bytes());
    }
}

/// The calm sprite as a cairo surface, premultiplied as cairo wants it.
fn sprite_surface(path: &str) -> Option<(gtk::cairo::ImageSurface, image::RgbaImage)> {
    let img = image::open(path).ok()?.to_rgba8();
    let (w, h) = img.dimensions();
    let mut surf = gtk::cairo::ImageSurface::create(gtk::cairo::Format::ARgb32, w as i32, h as i32).ok()?;
    let stride = surf.stride() as usize;
    {
        let mut data = surf.data().ok()?;
        for (x, y, p) in img.enumerate_pixels() {
            let a = p[3] as u32;
            let o = y as usize * stride + x as usize * 4;
            data[o] = (p[2] as u32 * a / 255) as u8;
            data[o + 1] = (p[1] as u32 * a / 255) as u8;
            data[o + 2] = (p[0] as u32 * a / 255) as u8;
            data[o + 3] = a as u8;
        }
    }
    Some((surf, img))
}

/// Fitting what it wears by hand: each accessory is a box on the picture;
/// drag it to move it, drag its corner or scroll to size it. Saved as it's
/// done, and shown on the desktop at once.
fn fit_view(r: &Rc<Roster>, pop: &gtk::Popover, i: usize, name: &str, path: &str) -> gtk::Widget {
    fit_pose(r, pop, i, name, path, false)
}

fn fit_pose(r: &Rc<Roster>, pop: &gtk::Popover, i: usize, name: &str, path: &str, happy: bool) -> gtk::Widget {
    use anima::render::character::wear;
    use std::cell::{Cell, RefCell};
    let b = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).width_request(320).build();
    b.append(&back_header(r, pop, i, name, "Fit accessories"));
    // calm and happy are pictures of their own: each is fitted on its own
    let poses = gtk::Box::builder().spacing(0).halign(gtk::Align::Center).build();
    poses.add_css_class("linked");
    let calm_b = gtk::ToggleButton::builder().label("Calm").active(!happy).build();
    let happy_b = gtk::ToggleButton::builder().label("Happy").active(happy).group(&calm_b).build();
    let happy_pic = wear::happy_path(path).filter(|p| std::path::Path::new(p).is_file());
    happy_b.set_sensitive(happy_pic.is_some());
    for (btn, want) in [(calm_b.clone(), false), (happy_b.clone(), true)] {
        let (rr, p, n, pth) = (r.clone(), pop.clone(), name.to_string(), path.to_string());
        btn.connect_toggled(move |t| {
            if t.is_active() && want != happy {
                p.set_child(Some(&fit_pose(&rr, &p, i, &n, &pth, want)));
            }
        });
    }
    poses.append(&calm_b);
    poses.append(&happy_b);
    b.append(&poses);
    // what it wears, plus the Santa hat
    let on: Vec<String> = wardrobe_all().get(name).and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
    let mut items: Vec<wear::Item> = wear::catalog().into_iter().filter(|it| on.contains(&it.id)).collect();
    items.push(wear::santa_item());
    let calm_path = path;
    let path = if happy { happy_pic.as_deref().unwrap_or(path) } else { path };
    let Some((surf, img)) = sprite_surface(path) else {
        b.append(&gtk::Label::new(Some("Couldn't read its picture")));
        return b.upcast();
    };
    let calm_anchors = image::open(calm_path).ok().and_then(|i| anima::render::anchors::find(&i.to_rgba8()));
    let found = anima::render::anchors::find(&img);
    let anchors = match (happy, calm_anchors, found) {
        (true, Some(c), Some(h)) => wear::happy_anchors(&c, h),
        (_, _, Some(a)) => a,
        _ => {
            b.append(&gtk::Label::new(Some("Couldn't find its head on the picture")));
            return b.upcast();
        }
    };
    let hint = gtk::Label::builder()
        .label("Pick one, then drag its box to move it, drag the corner or scroll over it to size it.")
        .wrap(true)
        .xalign(0.0)
        .build();
    hint.add_css_class("dim-label");
    b.append(&hint);
    let faces_right = calm_path.contains("_0010.") || calm_path.contains("_0011.");
    let fits: Rc<RefCell<wear::Fits>> = Rc::new(RefCell::new(fits_all()));
    let pick: Rc<Cell<usize>> = Rc::new(Cell::new(0));
    let items = Rc::new(items);
    let who = name.to_string();
    let (iw, ih) = (img.width() as f64, img.height() as f64);
    // the picture inside the widget: scale and offset
    let place = move |w: f64, h: f64| {
        let s = (w / iw).min(h / ih);
        (s, (w - iw * s) * 0.5, (h - ih * s) * 0.5)
    };
    let fit_of = {
        let (fits, who) = (fits.clone(), who.clone());
        move |id: &str| fits.borrow().get(&who).and_then(|m| m.get(&wear::fit_key(id, happy)).or_else(|| m.get(id))).copied()
    };
    let rect_of = {
        let (items, fit_of) = (items.clone(), fit_of.clone());
        move |k: usize| -> [f64; 4] {
            let it = &items[k];
            let [l, t, w, h] = wear::layout(&anchors, it, faces_right, fit_of(&it.id));
            [l as f64 * iw, t as f64 * ih, w as f64 * iw, h as f64 * ih]
        }
    };
    let area = gtk::DrawingArea::builder().content_width(300).content_height(340).build();
    // each item's picture, where it has one, drawn as on the desktop
    let pictures: Vec<Option<(gtk::cairo::ImageSurface, f64, f64)>> = items
        .iter()
        .map(|it| {
            let p = wear::picture_of(it)?;
            let (s, img) = sprite_surface(p.to_str()?)?;
            Some((s, img.width() as f64, img.height() as f64))
        })
        .collect();
    {
        let (pick, items, rect_of) = (pick.clone(), items.clone(), rect_of.clone());
        area.set_draw_func(move |_, cr, w, h| {
            let (s, ox, oy) = place(w as f64, h as f64);
            cr.set_source_rgb(0.16, 0.16, 0.18);
            let _ = cr.paint();
            let _ = cr.save();
            cr.translate(ox, oy);
            cr.scale(s, s);
            let _ = cr.set_source_surface(&surf, 0.0, 0.0);
            let _ = cr.paint();
            let _ = cr.restore();
            for k in 0..items.len() {
                let [x, y, rw, rh] = rect_of(k);
                let (x, y, rw, rh) = (ox + x * s, oy + y * s, rw * s, rh * s);
                let chosen = k == pick.get();
                // the picture fills the box's width at its own shape, about its middle
                if let Some((pic, pw, ph)) = &pictures[k] {
                    let (dw, dh) = (rw, rw * ph / pw);
                    let _ = cr.save();
                    cr.translate(x, y + rh * 0.5 - dh * 0.5);
                    cr.scale(dw / pw, dh / ph);
                    let _ = cr.set_source_surface(pic, 0.0, 0.0);
                    let _ = cr.paint();
                    let _ = cr.restore();
                    if !chosen {
                        continue;
                    }
                }
                if chosen {
                    if pictures[k].is_none() {
                        cr.set_source_rgba(1.0, 0.45, 0.65, 0.25);
                        cr.rectangle(x, y, rw, rh);
                        let _ = cr.fill();
                    }
                    cr.set_source_rgb(1.0, 0.45, 0.65);
                    cr.set_line_width(2.0);
                } else {
                    cr.set_source_rgba(0.8, 0.8, 0.85, 0.7);
                    cr.set_line_width(1.0);
                }
                cr.rectangle(x, y, rw, rh);
                let _ = cr.stroke();
                if chosen {
                    // the corner to size it by
                    cr.rectangle(x + rw - 5.0, y + rh - 5.0, 10.0, 10.0);
                    let _ = cr.fill();
                }
                cr.set_font_size(10.0);
                cr.move_to(x + 2.0, y - 3.0);
                let _ = cr.show_text(&items[k].label);
            }
        });
    }
    // dragging: move, or size from the corner
    let save = {
        let fits = fits.clone();
        move || save_fits(&fits.borrow())
    };
    let set_fit = {
        let (fits, who) = (fits.clone(), who.clone());
        move |id: &str, f: [f32; 4]| {
            fits.borrow_mut().entry(who.clone()).or_default().insert(wear::fit_key(id, happy), f);
        }
    };
    let drag = gtk::GestureDrag::new();
    let start: Rc<Cell<Option<([f32; 4], bool)>>> = Rc::new(Cell::new(None));
    {
        let (pick, items, rect_of, start, fit_of, area2) = (pick.clone(), items.clone(), rect_of.clone(), start.clone(), fit_of.clone(), area.clone());
        drag.connect_drag_begin(move |g, x, y| {
            let Some(w) = g.widget() else { return };
            let (s, ox, oy) = place(w.width() as f64, w.height() as f64);
            let (px, py) = ((x - ox) / s, (y - oy) / s);
            // the corner of the one picked, else whichever box it's in (smallest first)
            let [cx, cy, cw, ch] = rect_of(pick.get());
            let corner = ((px - (cx + cw)).abs() * s < 9.0) && ((py - (cy + ch)).abs() * s < 9.0);
            if !corner {
                let mut hit: Option<(usize, f64)> = None;
                for k in 0..items.len() {
                    let [rx, ry, rw, rh] = rect_of(k);
                    if px >= rx && px <= rx + rw && py >= ry && py <= ry + rh && hit.is_none_or(|h| rw * rh < h.1) {
                        hit = Some((k, rw * rh));
                    }
                }
                if let Some((k, _)) = hit {
                    pick.set(k);
                }
            }
            let id = &items[pick.get()].id;
            start.set(Some((fit_of(id).unwrap_or([0.0, 0.0, 1.0, 1.0]), corner)));
            area2.queue_draw();
        });
    }
    {
        let (pick, items, start, set_fit, area2, rect_of) = (pick.clone(), items.clone(), start.clone(), set_fit.clone(), area.clone(), rect_of.clone());
        drag.connect_drag_update(move |g, dx, dy| {
            let Some(w) = g.widget() else { return };
            let Some((f, corner)) = start.get() else { return };
            let (s, _, _) = place(w.width() as f64, w.height() as f64);
            let (du, dv) = ((dx / s / iw) as f32, (dy / s / ih) as f32);
            let id = items[pick.get()].id.clone();
            let next = if corner {
                // grow from the corner: the box's size at the start of the drag, plus the pull
                let base = rect_of(pick.get());
                let k = ((du / (base[2] / iw) as f32 + dv / (base[3] / ih) as f32) * 0.5).max(-0.9);
                [f[0], f[1], (f[2] * (1.0 + k)).max(0.1), (f[3] * (1.0 + k)).max(0.1)]
            } else {
                [f[0] + du, f[1] + dv, f[2], f[3]]
            };
            set_fit(&id, next);
            area2.queue_draw();
        });
    }
    {
        let (save, start, u) = (save.clone(), start.clone(), r.ui.clone());
        drag.connect_drag_end(move |_, _, _| {
            start.set(None);
            save();
            u.say("Fitted");
        });
    }
    area.add_controller(drag);
    // the wheel sizes the one picked
    let scroll = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
    {
        let (pick, items, set_fit, fit_of, area2, save) = (pick.clone(), items.clone(), set_fit.clone(), fit_of.clone(), area.clone(), save.clone());
        scroll.connect_scroll(move |_, _, dy| {
            let id = items[pick.get()].id.clone();
            let f = fit_of(&id).unwrap_or([0.0, 0.0, 1.0, 1.0]);
            let k = if dy < 0.0 { 1.06 } else { 1.0 / 1.06 };
            set_fit(&id, [f[0], f[1], (f[2] * k).clamp(0.1, 6.0), (f[3] * k).clamp(0.1, 6.0)]);
            area2.queue_draw();
            save();
            gtk::glib::Propagation::Stop
        });
    }
    area.add_controller(scroll);
    let frame = gtk::Frame::builder().child(&area).build();
    b.append(&frame);
    // which one: buttons by name
    let names = gtk::FlowBox::builder().selection_mode(gtk::SelectionMode::None).max_children_per_line(3).build();
    for k in 0..items.len() {
        let btn = gtk::Button::with_label(&items[k].label);
        btn.add_css_class("flat");
        let (pick, area2) = (pick.clone(), area.clone());
        btn.connect_clicked(move |_| {
            pick.set(k);
            area2.queue_draw();
        });
        names.insert(&btn, -1);
    }
    b.append(&names);
    let reset = gtk::Button::with_label("Reset the one picked");
    {
        let (pick, items, fits, who, area2, save) = (pick.clone(), items.clone(), fits.clone(), who.clone(), area.clone(), save.clone());
        reset.connect_clicked(move |_| {
            if let Some(m) = fits.borrow_mut().get_mut(&who) {
                m.remove(&wear::fit_key(&items[pick.get()].id, happy));
            }
            save();
            area2.queue_draw();
        });
    }
    b.append(&reset);
    if on.is_empty() {
        let none = gtk::Label::builder().label("It wears nothing yet: pick things in Wardrobe (the Santa hat is here to fit for Christmas).").wrap(true).xalign(0.0).build();
        none.add_css_class("dim-label");
        b.append(&none);
    }
    b.upcast()
}

fn wardrobe_view(r: &Rc<Roster>, pop: &gtk::Popover, i: usize, name: &str) -> gtk::Widget {
    use anima::render::character::wear;
    let b = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).width_request(280).build();
    b.append(&back_header(r, pop, i, name, "Wardrobe"));
    let hint = gtk::Label::builder()
        .label("Worn at once, on top of what the weather brings (umbrellas, snow, Santa hats). Locked things are found while fishing.")
        .wrap(true)
        .xalign(0.0)
        .build();
    hint.add_css_class("dim-label");
    b.append(&hint);
    let on: Vec<String> = wardrobe_all().get(name).and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
    let inv: wear::Inventory = std::fs::read_to_string(wear::inventory_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let list = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).build();
    // what it owns first, then the rest, locked till it fishes them up
    let mut all = wear::catalog();
    all.sort_by_key(|it| !wear::owns(&inv, name, &it.id));
    for item in all {
        let owned = wear::owns(&inv, name, &item.id);
        let row = gtk::Box::builder().spacing(8).build();
        let label = gtk::Label::builder().label(if owned { item.label.clone() } else { format!("🔒 {}", item.label) }).xalign(0.0).hexpand(true).build();
        if !owned {
            label.add_css_class("dim-label");
            label.set_tooltip_text(Some("Not found yet: it turns up on a fishing line"));
        }
        row.append(&label);
        let sw = gtk::Switch::builder().active(owned && on.contains(&item.id)).sensitive(owned).valign(gtk::Align::Center).build();
        let (who, id, u) = (name.to_string(), item.id.clone(), r.ui.clone());
        sw.connect_active_notify(move |s| {
            let mut all = wardrobe_all();
            let mut mine: Vec<String> = all.get(&who).and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default();
            mine.retain(|x| x != &id);
            if s.is_active() {
                mine.push(id.clone());
            }
            if mine.is_empty() {
                all.remove(&who);
            } else {
                all.insert(who.clone(), serde_json::json!(mine));
            }
            if let Ok(text) = serde_json::to_string_pretty(&all) {
                let _ = anima::util::atomic_write(&wear::wardrobe_path(), text.as_bytes());
            }
            u.say(if s.is_active() { "Put on" } else { "Taken off" });
        });
        row.append(&sw);
        list.append(&row);
    }
    let scroll = gtk::ScrolledWindow::builder().child(&list).min_content_height(260).max_content_height(360).propagate_natural_height(true).build();
    b.append(&scroll);
    let add = gtk::Button::with_label("Add your own…");
    add.set_tooltip_text(Some("Drop a PNG in this folder, with a .toml of the same name to say where it goes (see wear.rs)"));
    add.connect_clicked(|_| {
        let dir = wear::accessories_dir();
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::process::Command::new("xdg-open").arg(&dir).spawn();
    });
    b.append(&add);
    b.upcast()
}

/// Everything anima knows about the characters, or None if it isn't running.
fn live() -> Option<Vec<serde_json::Value>> {
    let reply = anima::ipc::request(&anima::util::socket_path(), &anima::ipc::Request::CharacterInfo).ok()?;
    reply.data?.as_array().cloned()
}

fn status_view(r: &Rc<Roster>, pop: &gtk::Popover, i: usize, name: &str) -> gtk::Widget {
    let b = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).width_request(260).build();
    b.append(&back_header(r, pop, i, name, "Status"));
    let me = live().and_then(|all| all.into_iter().find(|c| c["name"].as_str() == Some(name)));
    let Some(me) = me else {
        b.append(&small("Not on screen right now (off, or Anima isn't running)."));
        return b.upcast();
    };
    let line = |key: &str, value: String| {
        let row = gtk::Box::builder().spacing(8).build();
        let k = gtk::Label::builder().label(key).xalign(0.0).width_chars(12).build();
        k.add_css_class("dim-label");
        row.append(&k);
        row.append(&gtk::Label::builder().label(value).xalign(0.0).wrap(true).hexpand(true).build());
        row
    };
    b.append(&line("Doing", me["doing"].as_str().unwrap_or("").to_string()));
    let with: Vec<&str> = me["with"].as_array().map(|a| a.iter().filter_map(|v| v.as_str()).collect()).unwrap_or_default();
    if !with.is_empty() {
        b.append(&line("With", with.join(", ")));
    }
    if let Some(ws) = me["workspace"].as_u64() {
        b.append(&line("Workspace", ws.to_string()));
    }
    let energy = me["energy"].as_f64().unwrap_or(0.0);
    let row = gtk::Box::builder().spacing(8).build();
    let k = gtk::Label::builder().label("Social battery").xalign(0.0).width_chars(12).build();
    k.add_css_class("dim-label");
    row.append(&k);
    row.append(&gtk::LevelBar::builder().min_value(0.0).max_value(1.0).value(energy).hexpand(true).valign(gtk::Align::Center).build());
    b.append(&row);
    // what it has got good at
    let skills: Vec<(String, f64)> = me["skills"].as_array().map(|a| a.iter().filter_map(|v| Some((v["game"].as_str()?.to_string(), v["skill"].as_f64()?))).collect()).unwrap_or_default();
    if !skills.is_empty() {
        let grid = gtk::Grid::builder().row_spacing(4).column_spacing(8).margin_top(4).build();
        grid.attach(&small("Getting good at"), 0, 0, 2, 1);
        for (k, (game, v)) in skills.iter().take(6).enumerate() {
            let row = k as i32 + 1;
            let name = gtk::Label::builder().label(game).xalign(0.0).width_chars(14).build();
            name.add_css_class("dim-label");
            grid.attach(&name, 0, row, 1, 1);
            grid.attach(&gtk::LevelBar::builder().min_value(0.0).max_value(1.0).value(*v).hexpand(true).valign(gtk::Align::Center).build(), 1, row, 1, 1);
        }
        b.append(&grid);
    }
    let keeps: Vec<&str> = me["keepsakes"].as_array().map(|a| a.iter().filter_map(|v| v.as_str()).collect()).unwrap_or_default();
    if !keeps.is_empty() {
        // "a star ×3, a rubber duck"
        let mut seen: Vec<(&str, usize)> = Vec::new();
        for k in keeps {
            match seen.iter_mut().find(|s| s.0 == k) {
                Some(s) => s.1 += 1,
                None => seen.push((k, 1)),
            }
        }
        let text = seen.iter().map(|(k, n)| if *n > 1 { format!("{k} ×{n}") } else { k.to_string() }).collect::<Vec<_>>().join(", ");
        b.append(&line("Keepsakes", text));
    }
    let found: Vec<&str> = me["found"].as_array().map(|a| a.iter().filter_map(|v| v.as_str()).collect()).unwrap_or_default();
    if !found.is_empty() {
        b.append(&line("Found", found.join(", ")));
    }
    let played = me["played"].as_f64().unwrap_or(0.0);
    if played >= 0.5 {
        b.append(&line("Games lately", format!("{played:.0}")));
    }
    b.upcast()
}

fn friends_view(r: &Rc<Roster>, pop: &gtk::Popover, i: usize, name: &str) -> gtk::Widget {
    let b = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).width_request(300).build();
    b.append(&back_header(r, pop, i, name, "Friendships"));
    let me = live().and_then(|all| all.into_iter().find(|c| c["name"].as_str() == Some(name)));
    // anima running: the live numbers; otherwise what it last saved
    let bonds: Vec<(String, f64, f64)> = match me {
        Some(me) => me["bonds"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| Some((v["with"].as_str()?.to_string(), v["friend"].as_f64()?, v["rival"].as_f64().unwrap_or(0.0)))).collect())
            .unwrap_or_default(),
        None => saved_bonds(name),
    };
    if bonds.is_empty() {
        b.append(&small("Hasn't really met anyone yet."));
        return b.upcast();
    }
    let grid = gtk::Grid::builder().row_spacing(4).column_spacing(8).build();
    grid.attach(&small("Friends"), 1, 0, 1, 1);
    grid.attach(&small("Rivals"), 2, 0, 1, 1);
    for (k, (other, friend, rival)) in bonds.iter().take(12).enumerate() {
        let row = k as i32 + 1;
        grid.attach(&gtk::Label::builder().label(other).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).max_width_chars(14).build(), 0, row, 1, 1);
        grid.attach(&gtk::LevelBar::builder().min_value(0.0).max_value(3.0).value(*friend).width_request(90).valign(gtk::Align::Center).build(), 1, row, 1, 1);
        grid.attach(&gtk::LevelBar::builder().min_value(0.0).max_value(3.0).value(*rival).width_request(60).valign(gtk::Align::Center).build(), 2, row, 1, 1);
    }
    b.append(&grid);
    if bonds.len() > 12 {
        b.append(&small(&format!("and {} more", bonds.len() - 12)));
    }
    b.upcast()
}

fn saved_bonds(name: &str) -> Vec<(String, f64, f64)> {
    let file = config::home().join(".local/share/anima/bonds.json");
    let map: std::collections::HashMap<String, [f64; 2]> = std::fs::read_to_string(file).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    let mut v: Vec<(String, f64, f64)> = map
        .into_iter()
        .filter_map(|(k, v)| {
            let (a, b) = k.split_once('|')?;
            let other = if a == name { b } else if b == name { a } else { return None };
            Some((other.to_string(), v[0], v[1]))
        })
        .collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1));
    v
}

fn fill_lineups(r: &Rc<Roster>) {
    clear_box(&r.lineups);
    let saved: Vec<(String, usize)> = r.ui.data.borrow().groups.iter().map(|(k, v)| (k.clone(), v.len())).collect();
    if saved.is_empty() {
        r.lineups.append(&small("No saved line-ups yet."));
    }
    for (name, count) in saved {
        let row = gtk::Box::builder().spacing(4).build();
        let inner = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).margin_top(4).margin_bottom(4).build();
        let head = gtk::Box::builder().spacing(8).build();
        let title = gtk::Label::builder().label(&name).xalign(0.0).hexpand(true).build();
        title.add_css_class("heading");
        head.append(&title);
        head.append(&small(&format!("{count} characters")));
        inner.append(&head);
        let faces = gtk::Box::builder().spacing(2).build();
        let members: Vec<team::Member> = r.ui.data.borrow().groups.get(&name).cloned().unwrap_or_default();
        for m in members.iter().filter(|m| m.on).take(10) {
            faces.append(&thumb(&r.ui, &m.path, 34));
        }
        if members.iter().filter(|m| m.on).count() > 10 {
            faces.append(&small(&format!("+{}", members.iter().filter(|m| m.on).count() - 10)));
        }
        inner.append(&faces);
        let load = gtk::Button::builder().child(&inner).hexpand(true).tooltip_text("Put this line-up in the editor (Apply shows it)").build();
        load.add_css_class("flat");
        let (rr, nm) = (r.clone(), name.clone());
        load.connect_clicked(move |_| {
            let nm = nm.clone();
            edit(&rr, move |d| {
                if let Some(saved) = d.groups.get(&nm).cloned() {
                    d.team = saved;
                }
            });
        });
        row.append(&load);
        let del = gtk::Button::builder().icon_name("user-trash-symbolic").tooltip_text("Delete").build();
        del.add_css_class("flat");
        let (rr, nm) = (r.clone(), name.clone());
        del.connect_clicked(move |_| {
            let nm = nm.clone();
            edit(&rr, move |d| {
                d.groups.remove(&nm);
            });
        });
        row.append(&del);
        r.lineups.append(&row);
    }
}

/// The right side: every character; click one to pick an outfit.
fn picker(r: &Rc<Roster>) -> gtk::Widget {
    let search = gtk::SearchEntry::builder().placeholder_text("Search characters").build();
    let grid = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .max_children_per_line(8)
        .min_children_per_line(3)
        .row_spacing(2)
        .column_spacing(2)
        .valign(gtk::Align::Start)
        .homogeneous(true)
        .build();
    let dirs: Rc<Vec<PathBuf>> = Rc::new(team::characters());
    let mut pending: Vec<(gtk::Picture, PathBuf)> = Vec::new();
    for d in dirs.iter() {
        let name = d.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let pic = gtk::Picture::builder().width_request(52).height_request(52).can_shrink(true).build();
        if let Some(o) = team::outfits(d).into_iter().next() {
            pending.push((pic.clone(), o.calm));
        }
        let label = gtk::Label::builder().label(&name).ellipsize(gtk::pango::EllipsizeMode::End).max_width_chars(10).build();
        label.add_css_class("caption");
        let inner = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(1).build();
        inner.append(&pic);
        inner.append(&label);
        let pop = gtk::Popover::new();
        let b = gtk::MenuButton::builder().child(&inner).tooltip_text(&name).popover(&pop).build();
        b.add_css_class("flat");
        let (rr, dir) = (r.clone(), d.clone());
        pop.connect_show(move |p| p.set_child(Some(&outfits(&rr, &dir, p))));
        grid.append(&b);
    }
    let mut next = 0usize;
    glib::idle_add_local(move || {
        for (pic, path) in pending.iter().skip(next).take(8) {
            if let Some(t) = load_texture(path, 52) {
                pic.set_paintable(Some(&t));
            }
        }
        next += 8;
        if next < pending.len() { glib::ControlFlow::Continue } else { glib::ControlFlow::Break }
    });
    let (s, ds) = (search.clone(), dirs.clone());
    grid.set_filter_func(move |child| {
        let q = s.text().trim().to_lowercase();
        q.is_empty() || ds.get(child.index() as usize).is_some_and(|d| d.file_name().is_some_and(|n| n.to_string_lossy().to_lowercase().contains(&q)))
    });
    let g = grid.clone();
    search.connect_search_changed(move |_| g.invalidate_filter());
    let side = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).margin_start(8).margin_end(10).margin_top(8).margin_bottom(8).build();
    side.append(&search);
    side.append(&gtk::ScrolledWindow::builder().child(&grid).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build());
    side.upcast()
}

/// A character's outfits, in the little window that opens from it.
fn outfits(r: &Rc<Roster>, dir: &Path, pop: &gtk::Popover) -> gtk::Widget {
    let flow = gtk::FlowBox::builder().selection_mode(gtk::SelectionMode::None).max_children_per_line(3).min_children_per_line(1).row_spacing(6).column_spacing(6).build();
    let in_team: Vec<String> = r.ui.data.borrow().team.iter().map(|m| m.path.clone()).collect();
    for o in team::outfits(dir) {
        let path = o.calm.to_string_lossy().into_owned();
        let already = in_team.contains(&path);
        // calm on the left, happy on the right
        let pics = gtk::Box::builder().spacing(2).halign(gtk::Align::Center).build();
        pics.append(&thumb(&r.ui, &path, 72));
        if let Some(h) = &o.happy {
            pics.append(&thumb(&r.ui, &h.to_string_lossy(), 72));
        }
        let inner = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).build();
        inner.append(&pics);
        let label = gtk::Label::builder().label(if already { format!("✓ {}", o.name) } else { o.name.clone() }).wrap(true).max_width_chars(14).justify(gtk::Justification::Center).build();
        label.add_css_class("caption");
        inner.append(&label);
        let b = gtk::Button::builder().child(&inner).sensitive(!already).tooltip_text(if already { "Already in the line-up" } else { "Add to the line-up" }).build();
        b.add_css_class("flat");
        let (rr, p) = (r.clone(), pop.clone());
        b.connect_clicked(move |_| {
            p.popdown();
            let p2 = path.clone();
            edit(&rr, move |d| {
                if !d.team.iter().any(|m| m.path == p2) {
                    d.team.push(team::Member { path: p2, on: true, crew: String::new() });
                }
            });
        });
        flow.append(&b);
    }
    let title = gtk::Label::builder().label(dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()).xalign(0.0).margin_bottom(4).build();
    title.add_css_class("heading");
    let b = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    b.append(&title);
    b.append(&gtk::ScrolledWindow::builder().child(&flow).propagate_natural_height(true).max_content_height(460).min_content_width(480).hscrollbar_policy(gtk::PolicyType::Never).build());
    b.upcast()
}

fn save_button(r: &Rc<Roster>) -> gtk::MenuButton {
    let entry = gtk::Entry::builder().placeholder_text("Name, e.g. Evening crew").width_chars(22).build();
    let save = gtk::Button::with_label("Save");
    save.add_css_class("suggested-action");
    let row = gtk::Box::builder().spacing(6).build();
    row.append(&entry);
    row.append(&save);
    let pop = gtk::Popover::builder().child(&row).build();
    let button = gtk::MenuButton::builder().label("Save as line-up…").popover(&pop).tooltip_text("Save these characters under a name").build();
    let go = {
        let (rr, e, p) = (r.clone(), entry.clone(), pop.clone());
        Rc::new(move || {
            let name = e.text().trim().to_string();
            if name.is_empty() || rr.ui.data.borrow().team.is_empty() {
                return;
            }
            let existed = rr.ui.data.borrow().groups.contains_key(&name);
            {
                let mut d = rr.ui.data.borrow_mut();
                let team = d.team.clone();
                d.groups.insert(name.clone(), team);
                d.save();
            }
            e.set_text("");
            p.popdown();
            fill_lineups(&rr);
            rr.ui.say(&if existed { format!("Updated \"{name}\"") } else { format!("Saved \"{name}\"") });
        })
    };
    let g = go.clone();
    save.connect_clicked(move |_| g());
    entry.connect_activate(move |_| go());
    button
}

fn settings_row(ui: &Rc<Ui>) -> gtk::Widget {
    let doc = config::read();
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.2, 1.5, 0.05);
    scale.set_hexpand(true);
    scale.set_value(config::number(&doc, "character", "scale", 0.45));
    let (u, wait) = (ui.clone(), crate::pages::Debounce::default());
    scale.connect_value_changed(move |s| {
        let (u, v) = (u.clone(), (s.value() * 100.0).round() / 100.0);
        wait.call(400, move || {
            config::set("character", "scale", v);
            u.restart_soon();
        });
    });
    let toggle = |key: &'static str, default: bool| {
        let sw = gtk::Switch::builder().valign(gtk::Align::Center).active(config::boolean(&doc, "character", key, default)).build();
        let u = ui.clone();
        sw.connect_active_notify(move |w| {
            config::set("character", key, w.is_active());
            // applied as it runs: nobody gets reloaded
            u.reload_soon();
        });
        sw
    };
    let row = gtk::Box::builder().spacing(8).build();
    row.append(&gtk::Label::new(Some("Size")));
    row.append(&scale);
    row.append(&gtk::Label::new(Some("Follow mouse")));
    row.append(&toggle("look_at_mouse", true));
    let race = gtk::Button::with_label("Race");
    race.connect_clicked(|_| daemon::race());
    row.append(&race);
    row.upcast()
}

pub fn page(ui: &Rc<Ui>) -> gtk::Widget {
    let r = Rc::new(Roster {
        ui: ui.clone(),
        cards: gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .max_children_per_line(12)
            .min_children_per_line(3)
            .row_spacing(10)
            .column_spacing(10)
            .valign(gtk::Align::Start)
            .homogeneous(true)
            .build(),
        summary: gtk::Label::builder().xalign(0.0).build(),
        apply: gtk::Button::builder().label("Apply").tooltip_text("Show this line-up on screen").build(),
        pending: small("Not applied yet"),
        lineups: gtk::Box::new(gtk::Orientation::Vertical, 0),
    });
    r.summary.add_css_class("heading");
    r.apply.add_css_class("suggested-action");
    let rr = r.clone();
    r.apply.connect_clicked(move |_| apply(&rr));

    // toolbar
    let list = gtk::ScrolledWindow::builder().child(&r.lineups).propagate_natural_height(true).max_content_height(460).min_content_width(420).build();
    let saved_pop = gtk::Popover::builder().child(&list).build();
    let saved = gtk::MenuButton::builder().label("Line-ups").popover(&saved_pop).tooltip_text("Saved line-ups").build();
    let bar = gtk::Box::builder().spacing(8).margin_start(10).margin_end(10).margin_top(8).margin_bottom(6).build();
    bar.append(&r.summary);
    bar.append(&r.pending);
    bar.append(&gtk::Box::builder().hexpand(true).build());
    bar.append(&saved);
    bar.append(&save_button(&r));
    bar.append(&r.apply);
    let add = gtk::ToggleButton::builder().tooltip_text("Show or hide the character picker").build();
    let add_label = gtk::Label::new(Some("‹  Add characters"));
    add.set_child(Some(&add_label));
    add.add_css_class("suggested-action");
    bar.append(&add);

    // left: the line-up; right: characters to add
    let left = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).margin_start(10).margin_end(8).margin_top(4).margin_bottom(8).build();
    left.append(&gtk::ScrolledWindow::builder().child(&r.cards).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build());
    left.append(&small("Switch: on screen · Right-click: status, friendships, remove"));
    let bottom = settings_row(ui);
    bottom.add_css_class("bottom-bar");
    left.append(&bottom);
    left.set_hexpand(true);
    let pick = picker(&r);
    pick.add_css_class("picker");
    // the picker slides in over the line-up from the right, and out again
    let paned = adw::OverlaySplitView::builder()
        .content(&left)
        .sidebar(&pick)
        .sidebar_position(gtk::PackType::End)
        .collapsed(true)
        .show_sidebar(false)
        .min_sidebar_width(380.0)
        .max_sidebar_width(560.0)
        .sidebar_width_fraction(0.45)
        .vexpand(true)
        .build();
    let (pv, lbl) = (paned.clone(), add_label.clone());
    add.connect_toggled(move |b| {
        let open = b.is_active();
        pv.set_show_sidebar(open);
        lbl.set_label(if open { "Done  ›" } else { "‹  Add characters" });
    });
    let tb = add.clone();
    paned.connect_show_sidebar_notify(move |p| {
        if tb.is_active() != p.shows_sidebar() {
            tb.set_active(p.shows_sidebar());
        }
    });

    let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    page.append(&bar);
    page.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    page.append(&paned);
    refresh(&r);
    page.upcast()
}
