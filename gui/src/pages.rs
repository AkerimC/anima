//! The window: a tab for the characters, one per group of behaviour
//! settings (built from anima's tuning table) and one for general options.

use crate::{config, daemon, team};
use adw::prelude::*;
use anima::render::tuning::{FIELDS, Field};
use gtk::{gdk, gdk_pixbuf, glib};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

/// Runs the last call only, after things have been quiet for a moment.
#[derive(Clone, Default)]
struct Debounce(Rc<RefCell<Option<glib::SourceId>>>);

impl Debounce {
    fn call(&self, ms: u64, f: impl FnOnce() + 'static) {
        if let Some(id) = self.0.borrow_mut().take() {
            id.remove();
        }
        let slot = self.0.clone();
        let id = glib::timeout_add_local_once(std::time::Duration::from_millis(ms), move || {
            slot.borrow_mut().take();
            f();
        });
        *self.0.borrow_mut() = Some(id);
    }
}

struct Ui {
    data: RefCell<team::Team>,
    // the team strip is rebuilt on every change: don't decode its images again
    team_thumbs: RefCell<HashMap<String, gdk::Texture>>,
    group_names: RefCell<Vec<String>>,
    toast: adw::ToastOverlay,
    power: gtk::Switch,
    syncing: Cell<bool>,
    current: gtk::Label,
    team_box: gtk::Box,
    outfits: gtk::FlowBox,
    groups: gtk::DropDown,
    group_name: gtk::Entry,
    restart: Debounce,
    reload: Debounce,
}

impl Ui {
    fn say(&self, text: &str) {
        self.toast.add_toast(adw::Toast::builder().title(text).timeout(2).build());
    }

    fn is_on(&self) -> bool {
        self.power.is_active()
    }

    fn restart_soon(&self) {
        if self.is_on() {
            self.restart.call(500, daemon::restart);
        }
    }

    fn reload_soon(&self) {
        if self.is_on() {
            self.reload.call(250, daemon::reload);
        }
    }
}

fn load_texture(path: &Path, size: i32) -> Option<gdk::Texture> {
    if path.extension().is_some_and(|e| e == "glb") {
        return None;
    }
    let p = gdk_pixbuf::Pixbuf::from_file_at_scale(path, size, size, true).ok()?;
    #[allow(deprecated)]
    Some(gdk::Texture::for_pixbuf(&p))
}

/// A sprite preview, or an icon for 3D models and unreadable files.
fn picture(tex: Option<&gdk::Texture>, size: i32) -> gtk::Widget {
    match tex {
        Some(tex) => {
            let pic = gtk::Picture::for_paintable(tex);
            pic.set_size_request(size, size);
            pic.set_can_shrink(true);
            pic.upcast()
        }
        None => gtk::Image::builder().icon_name("applications-graphics-symbolic").pixel_size(size / 2).build().upcast(),
    }
}

fn thumb(path: &Path, size: i32) -> gtk::Widget {
    picture(load_texture(path, size).as_ref(), size)
}

fn team_thumb(ui: &Ui, path: &str) -> gtk::Widget {
    let mut cache = ui.team_thumbs.borrow_mut();
    if !cache.contains_key(path) {
        if let Some(t) = load_texture(Path::new(path), 64) {
            cache.insert(path.to_string(), t);
        }
    }
    picture(cache.get(path), 64)
}

fn folder_name(path: &Path) -> String {
    path.parent().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Write the characters that are on. Only the extras changing is applied
/// live; a new main character needs a restart.
fn apply_team(ui: &Rc<Ui>) {
    ui.data.borrow().save();
    let on = ui.data.borrow().enabled();
    let doc = config::read();
    let enabled = config::boolean(&doc, "character", "enabled", true);
    let restart = match on.first() {
        None => enabled,
        Some(first) => config::text(&doc, "character", "source", "") != *first || !enabled,
    };
    config::set_team(&on);
    if ui.is_on() {
        if restart {
            daemon::restart();
        } else {
            daemon::reload();
        }
    }
    update_team(ui);
}

fn update_team(ui: &Rc<Ui>) {
    let members = ui.data.borrow().team.clone();
    let on = members.iter().filter(|m| m.on).count();
    ui.current.set_label(&if members.is_empty() {
        "Karakter seçilmedi".to_string()
    } else {
        format!("Ekip: {on}/{} karakter açık · ilk açık olan ana karakter", members.len())
    });
    while let Some(c) = ui.team_box.first_child() {
        ui.team_box.remove(&c);
    }
    for (i, m) in members.iter().enumerate() {
        let pic = team_thumb(ui, &m.path);
        pic.set_opacity(if m.on { 1.0 } else { 0.35 });
        let sw = gtk::Switch::builder().active(m.on).halign(gtk::Align::Center).tooltip_text("Aç/kapat").build();
        let u = ui.clone();
        sw.connect_active_notify(move |w| {
            let active = w.is_active();
            let u = u.clone();
            // not while the switch is still being handled: this rebuilds the strip
            glib::idle_add_local_once(move || {
                if let Some(m) = u.data.borrow_mut().team.get_mut(i) {
                    m.on = active;
                }
                apply_team(&u);
            });
        });
        let rm = gtk::Button::builder().icon_name("window-close-symbolic").tooltip_text("Ekipten çıkar").halign(gtk::Align::Center).build();
        rm.add_css_class("circular");
        rm.add_css_class("flat");
        let u = ui.clone();
        rm.connect_clicked(move |_| {
            let u = u.clone();
            glib::idle_add_local_once(move || {
                {
                    let mut d = u.data.borrow_mut();
                    if i < d.team.len() {
                        d.team.remove(i);
                    }
                }
                apply_team(&u);
            });
        });
        let col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).tooltip_text(folder_name(Path::new(&m.path))).build();
        col.append(&pic);
        col.append(&sw);
        col.append(&rm);
        ui.team_box.append(&col);
    }
}

fn pick_main(ui: &Rc<Ui>, calm: &Path, name: &str) {
    let path = calm.to_string_lossy().into_owned();
    {
        let mut d = ui.data.borrow_mut();
        d.team.retain(|m| m.path != path);
        d.team.insert(0, team::Member { path, on: true });
    }
    apply_team(ui);
    ui.say(&format!("{} · {name}", folder_name(calm)));
}

fn add_extra(ui: &Rc<Ui>, calm: &Path, name: &str) {
    ui.data.borrow_mut().team.push(team::Member { path: calm.to_string_lossy().into_owned(), on: true });
    apply_team(ui);
    ui.say(&format!("Eklendi: {} · {name}", folder_name(calm)));
}

fn show_outfits(ui: &Rc<Ui>, dir: &Path) {
    while let Some(c) = ui.outfits.first_child() {
        ui.outfits.remove(&c);
    }
    for o in team::outfits(dir) {
        let pics = gtk::Box::builder().spacing(4).halign(gtk::Align::Center).build();
        pics.append(&thumb(&o.calm, 96));
        if let Some(h) = &o.happy {
            pics.append(&thumb(h, 96));
        }
        let inner = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).build();
        inner.append(&pics);
        inner.append(&gtk::Label::builder().label(&o.name).wrap(true).max_width_chars(18).justify(gtk::Justification::Center).build());
        let btn = gtk::Button::builder().child(&inner).tooltip_text("Ana karakter yap\nSol poz: durma/yürüme · Sağ: zıplama/yakalama").build();
        btn.add_css_class("flat");
        let (u, calm, name) = (ui.clone(), o.calm.clone(), o.name.clone());
        btn.connect_clicked(move |_| pick_main(&u, &calm, &name));
        let add = gtk::Button::builder().icon_name("list-add-symbolic").tooltip_text("Ekibe ekle").halign(gtk::Align::Center).build();
        add.add_css_class("circular");
        let (u, calm, name) = (ui.clone(), o.calm.clone(), o.name.clone());
        add.connect_clicked(move |_| add_extra(&u, &calm, &name));
        let card = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(2).build();
        card.append(&btn);
        card.append(&add);
        ui.outfits.append(&card);
    }
}

fn refresh_groups(ui: &Rc<Ui>) {
    let names: Vec<String> = ui.data.borrow().groups.keys().cloned().collect();
    let shown: Vec<&str> = if names.is_empty() { vec!["(grup yok)"] } else { names.iter().map(String::as_str).collect() };
    ui.groups.set_model(Some(&gtk::StringList::new(&shown)));
    ui.groups.set_sensitive(!names.is_empty());
    *ui.group_names.borrow_mut() = names;
}

fn selected_group(ui: &Rc<Ui>) -> Option<String> {
    ui.group_names.borrow().get(ui.groups.selected() as usize).cloned()
}

fn save_group(ui: &Rc<Ui>) {
    let name = ui.group_name.text().trim().to_string();
    if name.is_empty() || ui.data.borrow().team.is_empty() {
        return;
    }
    {
        let mut d = ui.data.borrow_mut();
        let team = d.team.clone();
        d.groups.insert(name.clone(), team);
        d.save();
    }
    ui.group_name.set_text("");
    refresh_groups(ui);
    ui.say(&format!("Grup kaydedildi: {name}"));
}

fn characters_page(ui: &Rc<Ui>) -> gtk::Widget {
    // left: character list with search
    let search = gtk::SearchEntry::builder().placeholder_text("Karakter ara").build();
    let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::Single).build();
    list.add_css_class("navigation-sidebar");
    let dirs: Rc<Vec<PathBuf>> = Rc::new(team::characters());
    for d in dirs.iter() {
        let label = gtk::Label::builder()
            .label(d.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
            .xalign(0.0)
            .margin_start(8)
            .margin_top(6)
            .margin_bottom(6)
            .build();
        list.append(&gtk::ListBoxRow::builder().child(&label).build());
    }
    let (s, ds) = (search.clone(), dirs.clone());
    list.set_filter_func(move |row| {
        let q = s.text().trim().to_lowercase();
        q.is_empty() || ds.get(row.index() as usize).is_some_and(|d| d.to_string_lossy().rsplit('/').next().unwrap_or("").to_lowercase().contains(&q))
    });
    let l = list.clone();
    search.connect_search_changed(move |_| l.invalidate_filter());
    let (u, ds) = (ui.clone(), dirs.clone());
    list.connect_row_selected(move |_, row| {
        if let Some(d) = row.and_then(|r| ds.get(r.index() as usize)) {
            show_outfits(&u, d);
        }
    });
    let left = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).margin_start(6).margin_end(6).margin_top(6).build();
    left.append(&search);
    left.append(&gtk::ScrolledWindow::builder().child(&list).vexpand(true).width_request(220).build());

    // saved groups
    let gbar = gtk::Box::builder().spacing(6).margin_start(12).margin_end(12).margin_top(8).build();
    gbar.append(&gtk::Label::new(Some("Grup")));
    gbar.append(&ui.groups);
    let load = gtk::Button::with_label("Yükle");
    let u = ui.clone();
    load.connect_clicked(move |_| {
        let Some(name) = selected_group(&u) else { return };
        let saved = u.data.borrow().groups.get(&name).cloned();
        if let Some(saved) = saved {
            u.data.borrow_mut().team = saved;
            apply_team(&u);
            u.say(&format!("Grup yüklendi: {name}"));
        }
    });
    gbar.append(&load);
    let del = gtk::Button::with_label("Sil");
    let u = ui.clone();
    del.connect_clicked(move |_| {
        let Some(name) = selected_group(&u) else { return };
        {
            let mut d = u.data.borrow_mut();
            d.groups.remove(&name);
            d.save();
        }
        refresh_groups(&u);
    });
    gbar.append(&del);
    gbar.append(&ui.group_name);
    let u = ui.clone();
    ui.group_name.connect_activate(move |_| save_group(&u));
    let save = gtk::Button::with_label("Ekibi kaydet");
    let u = ui.clone();
    save.connect_clicked(move |_| save_group(&u));
    gbar.append(&save);
    refresh_groups(ui);

    // size, walking, follow, race
    let doc = config::read();
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.2, 1.5, 0.05);
    scale.set_hexpand(true);
    scale.set_value(config::number(&doc, "character", "scale", 0.45));
    let (u, wait) = (ui.clone(), Debounce::default());
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
    let controls = gtk::Box::builder().spacing(10).margin_start(12).margin_end(12).margin_top(8).margin_bottom(12).build();
    controls.append(&gtk::Label::new(Some("Boyut")));
    controls.append(&scale);
    controls.append(&gtk::Label::new(Some("Gezinme")));
    controls.append(&toggle("walk", true));
    controls.append(&gtk::Label::new(Some("Fare takibi")));
    controls.append(&toggle("look_at_mouse", true));
    let race = gtk::Button::with_label("Yarış başlat");
    race.connect_clicked(|_| daemon::race());
    controls.append(&race);

    let right = gtk::Box::builder().orientation(gtk::Orientation::Vertical).hexpand(true).build();
    right.append(&gbar);
    right.append(&ui.current);
    right.append(&gtk::ScrolledWindow::builder().child(&ui.team_box).vscrollbar_policy(gtk::PolicyType::Never).min_content_height(112).build());
    right.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    right.append(&gtk::ScrolledWindow::builder().child(&ui.outfits).vexpand(true).hexpand(true).build());
    right.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    right.append(&controls);

    let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    body.append(&left);
    body.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    body.append(&right);

    // open on the character in use
    update_team(ui);
    let source = PathBuf::from(config::text(&doc, "character", "source", ""));
    if let Some(i) = dirs.iter().position(|d| Some(d.as_path()) == source.parent()) {
        if let Some(row) = list.row_at_index(i as i32) {
            list.select_row(Some(&row));
        }
    }
    body.upcast()
}

fn tuning_row(ui: &Rc<Ui>, f: &'static Field) -> adw::SpinRow {
    let adj = gtk::Adjustment::new(f.default as f64, f.min as f64, f.max as f64, f.step as f64, f.step as f64 * 10.0, 0.0);
    let row = adw::SpinRow::builder().title(f.label).adjustment(&adj).digits(f.digits).build();
    row.set_value(config::tuning_value(f.key).unwrap_or(f.default as f64));
    let reset = gtk::Button::builder().icon_name("edit-undo-symbolic").tooltip_text(format!("Varsayılana dön ({})", f.default)).valign(gtk::Align::Center).build();
    reset.add_css_class("flat");
    let is_default = move |v: f64| (v - f.default as f64).abs() < 1e-4;
    reset.set_visible(!is_default(row.value()));
    let r = row.clone();
    reset.connect_clicked(move |_| r.set_value(f.default as f64));
    row.add_suffix(&reset);
    let u = ui.clone();
    row.connect_value_notify(move |r| {
        let v = r.value();
        reset.set_visible(!is_default(v));
        config::set_tuning(f.key, (!is_default(v)).then_some(v));
        u.reload_soon();
    });
    row
}

/// The current value of a tuning number.
fn tuning_now(key: &str) -> f64 {
    let default = FIELDS.iter().find(|f| f.key == key).map_or(0.0, |f| f.default as f64);
    config::tuning_value(key).unwrap_or(default)
}

/// One button per game, to start it right now.
fn force_buttons(ui: &Rc<Ui>) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Şimdi başlat")
        .description("Enerjiye ve bekleme sürelerine bakmaz, ekrana en yakın karakterler oynar")
        .build();
    let flow = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .max_children_per_line(4)
        .min_children_per_line(2)
        .row_spacing(6)
        .column_spacing(6)
        .homogeneous(true)
        .build();
    let games: [(&str, &'static str, &'static str); 9] = [
        ("Zıplama oyunu", "contest", "game_min"),
        ("İmleç oyunu", "cursor", "game_min"),
        ("İleri atlama", "long", "long_min"),
        ("Domino", "domino", "party_min"),
        ("Halat çekme", "tug", "party_min"),
        ("Ebelemece", "tag", "party_min"),
        ("Samba", "samba", "party_min"),
        ("Voleybol", "volley", "volley_min"),
        ("Havuç kapmaca", "carrot", "carrot_min"),
    ];
    for (label, name, min_key) in games {
        let b = gtk::Button::with_label(label);
        let u = ui.clone();
        b.connect_clicked(move |_| {
            let need = tuning_now(min_key) as usize;
            let have = u.data.borrow().enabled().len();
            if !daemon::running() {
                u.say("Anima kapalı");
            } else if have < need {
                u.say(&format!("Bu oyun için en az {need} karakter gerekiyor, ekipte {have} açık"));
            } else {
                daemon::game(name);
            }
        });
        flow.insert(&b, -1);
    }
    let race = gtk::Button::with_label("Yarış");
    race.add_css_class("suggested-action");
    race.connect_clicked(|_| daemon::race());
    flow.insert(&race, -1);
    group.add(&flow);
    group
}

/// A tab of behaviour settings: every field of the table with this tab name.
fn tuning_page(ui: &Rc<Ui>, tab: &str) -> gtk::Widget {
    let page = adw::PreferencesPage::new();
    if tab == "Oyunlar" {
        page.add(&force_buttons(ui));
    }
    let rows: Rc<RefCell<Vec<(adw::SpinRow, f64)>>> = Rc::default();
    let mut current: Option<(&str, adw::PreferencesGroup)> = None;
    for f in FIELDS.iter().filter(|f| f.tab == tab) {
        if current.as_ref().is_none_or(|c| c.0 != f.group) {
            let g = adw::PreferencesGroup::builder().title(f.group).build();
            page.add(&g);
            current = Some((f.group, g));
        }
        let row = tuning_row(ui, f);
        rows.borrow_mut().push((row.clone(), f.default as f64));
        if let Some((_, g)) = &current {
            g.add(&row);
        }
    }
    let group = adw::PreferencesGroup::new();
    let all = gtk::Button::builder().label("Bu sekmedeki her şeyi varsayılana döndür").halign(gtk::Align::Center).build();
    all.add_css_class("pill");
    let r = rows.clone();
    all.connect_clicked(move |_| r.borrow().iter().for_each(|(row, d)| row.set_value(*d)));
    group.add(&all);
    page.add(&group);
    page.upcast()
}

fn general_page(ui: &Rc<Ui>) -> gtk::Widget {
    let page = adw::PreferencesPage::new();
    let doc = config::read();
    let spin = |group: &adw::PreferencesGroup, section: &'static str, key: &'static str, title: &str, def: f64, min: f64, max: f64, step: f64, digits: u32| {
        let adj = gtk::Adjustment::new(config::number(&doc, section, key, def), min, max, step, step * 10.0, 0.0);
        let row = adw::SpinRow::builder().title(title).adjustment(&adj).digits(digits).build();
        let u = ui.clone();
        row.connect_value_notify(move |r| {
            if digits == 0 {
                config::set(section, key, r.value() as i64);
            } else {
                config::set(section, key, r.value());
            }
            u.restart_soon();
        });
        group.add(&row);
    };
    let switch = |group: &adw::PreferencesGroup, section: &'static str, key: &'static str, title: &str, def: bool| {
        let row = adw::SwitchRow::builder().title(title).active(config::boolean(&doc, section, key, def)).build();
        let u = ui.clone();
        row.connect_active_notify(move |r| {
            config::set(section, key, r.is_active());
            u.restart_soon();
        });
        group.add(&row);
    };
    let choice = |group: &adw::PreferencesGroup, key: &'static str, title: &str, def: &str, values: &'static [(&'static str, &'static str)]| {
        let labels: Vec<&str> = values.iter().map(|v| v.1).collect();
        let current = config::text(&doc, "character", key, def);
        let row = adw::ComboRow::builder().title(title).model(&gtk::StringList::new(&labels)).build();
        row.set_selected(values.iter().position(|v| v.0 == current).unwrap_or(0) as u32);
        let u = ui.clone();
        row.connect_selected_notify(move |r| {
            if let Some(v) = values.get(r.selected() as usize) {
                config::set("character", key, v.0);
                u.restart_soon();
            }
        });
        group.add(&row);
    };

    let look = adw::PreferencesGroup::builder().title("Görünüm").build();
    spin(&look, "character", "opacity", "Saydamlık", 1.0, 0.05, 1.0, 0.05, 2);
    switch(&look, "character", "flip_x", "Yatay aynala", false);
    choice(&look, "placement", "Yerleşim", "screen-front", &[
        ("screen-front", "Ekranın önünde"),
        ("wallpaper", "Duvar kağıdında"),
        ("window-inside", "Pencerenin içinde"),
        ("window-outside", "Pencerenin dışında"),
    ]);
    choice(&look, "corner", "Köşe", "bottom-right", &[
        ("bottom-right", "Sağ alt"),
        ("bottom-left", "Sol alt"),
        ("top-right", "Sağ üst"),
        ("top-left", "Sol üst"),
    ]);
    let offset = config::read_offset();
    for (index, title) in [(0usize, "Yatay kaydırma (piksel)"), (1, "Dikey kaydırma (piksel)")] {
        let adj = gtk::Adjustment::new(offset[index], -2000.0, 2000.0, 1.0, 10.0, 0.0);
        let row = adw::SpinRow::builder().title(title).adjustment(&adj).digits(0).build();
        let u = ui.clone();
        row.connect_value_notify(move |r| {
            config::set_offset(index, r.value());
            u.restart_soon();
        });
        look.add(&row);
    }
    page.add(&look);

    let sound = adw::PreferencesGroup::builder().title("Ses ve müzik").build();
    switch(&sound, "character", "audio_reactive", "Müziğe tepki versin", true);
    switch(&sound, "character", "voice", "Karakter sesleri", true);
    spin(&sound, "character", "voice_volume", "Ses seviyesi", 0.6, 0.0, 1.0, 0.05, 2);
    switch(&sound, "general", "audio_capture", "Masaüstü sesini dinle", true);
    page.add(&sound);

    let models = adw::PreferencesGroup::builder().title("3D karakterler").build();
    switch(&models, "character", "screen_light", "Ekran renkleriyle aydınlat", true);
    switch(&models, "character", "weapons", "Silahları göster", false);
    switch(&models, "character", "windows", "Çalışma alanı başına pencere (Hyprland)", true);
    page.add(&models);

    let perf = adw::PreferencesGroup::builder().title("Performans").build();
    spin(&perf, "character", "fps", "Karakter kare hızı", 144.0, 10.0, 360.0, 1.0, 0);
    spin(&perf, "general", "fps", "Duvar kağıdı kare hızı", 60.0, 10.0, 360.0, 1.0, 0);
    spin(&perf, "general", "battery_fps", "Pilde kare hızı", 30.0, 5.0, 360.0, 1.0, 0);
    switch(&perf, "general", "pause_on_fullscreen", "Tam ekranda duraklat", true);
    switch(&perf, "general", "pause_on_maximized", "Büyütülmüş pencerede duraklat", false);
    switch(&perf, "general", "pause_on_battery", "Pilde duraklat", false);
    spin(&perf, "general", "low_battery_pause", "Düşük pilde duraklat (%)", 15.0, 0.0, 100.0, 1.0, 0);
    page.add(&perf);
    page.upcast()
}

pub fn build_window(app: &adw::Application) -> adw::ApplicationWindow {
    let power = gtk::Switch::builder().valign(gtk::Align::Center).tooltip_text("Anima'yı aç/kapat").active(daemon::running()).build();
    let ui = Rc::new(Ui {
        data: RefCell::new(team::Team::load()),
        team_thumbs: RefCell::default(),
        group_names: RefCell::default(),
        toast: adw::ToastOverlay::new(),
        power: power.clone(),
        syncing: Cell::new(false),
        current: gtk::Label::builder().xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).margin_start(12).margin_top(8).build(),
        team_box: gtk::Box::builder().spacing(8).margin_start(12).margin_end(12).margin_top(6).build(),
        outfits: gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .max_children_per_line(4)
            .min_children_per_line(1)
            .row_spacing(12)
            .column_spacing(12)
            .margin_start(12)
            .margin_end(12)
            .margin_top(12)
            .margin_bottom(12)
            .valign(gtk::Align::Start)
            .build(),
        groups: gtk::DropDown::builder().hexpand(true).build(),
        group_name: gtk::Entry::builder().placeholder_text("Yeni grup adı").hexpand(true).build(),
        restart: Debounce::default(),
        reload: Debounce::default(),
    });
    ui.current.add_css_class("dim-label");

    let u = ui.clone();
    power.connect_active_notify(move |sw| {
        if u.syncing.get() {
            return;
        }
        if sw.is_active() && !daemon::running() {
            daemon::start();
        } else if !sw.is_active() && daemon::running() {
            daemon::stop();
        }
    });
    // keep the switch honest if the daemon dies or is started elsewhere
    let u = ui.clone();
    glib::timeout_add_seconds_local(2, move || {
        let running = daemon::running();
        if running != u.power.is_active() {
            u.syncing.set(true);
            u.power.set_active(running);
            u.syncing.set(false);
        }
        glib::ControlFlow::Continue
    });

    let stack = adw::ViewStack::new();
    stack.add_titled_with_icon(&characters_page(&ui), Some("characters"), "Karakterler", "avatar-default-symbolic");
    for (tab, icon) in [
        ("Gezinme", "find-location-symbolic"),
        ("Etkileşim", "input-mouse-symbolic"),
        ("Sohbet", "user-available-symbolic"),
        ("Oyunlar", "applications-games-symbolic"),
        ("Yarış", "media-seek-forward-symbolic"),
    ] {
        stack.add_titled_with_icon(&tuning_page(&ui, tab), Some(tab), tab, icon);
    }
    stack.add_titled_with_icon(&general_page(&ui), Some("general"), "Genel", "emblem-system-symbolic");

    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::ViewSwitcher::builder().stack(&stack).policy(adw::ViewSwitcherPolicy::Wide).build()));
    let power_box = gtk::Box::builder().spacing(8).build();
    power_box.append(&gtk::Label::new(Some("Çalışıyor")));
    power_box.append(&power);
    header.pack_end(&power_box);

    let view = adw::ToolbarView::new();
    view.set_content(Some(&stack));
    view.add_top_bar(&header);
    ui.toast.set_child(Some(&view));

    adw::ApplicationWindow::builder().application(app).title("Anima").default_width(1040).default_height(700).content(&ui.toast).build()
}
