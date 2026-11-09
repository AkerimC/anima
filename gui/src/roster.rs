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
            u.restart_soon();
        });
        sw
    };
    let row = gtk::Box::builder().spacing(8).build();
    row.append(&gtk::Label::new(Some("Size")));
    row.append(&scale);
    row.append(&gtk::Label::new(Some("Walk")));
    row.append(&toggle("walk", true));
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
