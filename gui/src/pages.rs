//! The window: a tab for the characters, one per group of behaviour
//! settings (built from anima's tuning table) and one for general options.

use crate::{config, daemon, roster, team};
use adw::prelude::*;
use anima::render::tuning::{FIELDS, Field};
use gtk::{gdk, glib};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;

/// Runs the last call only, after things have been quiet for a moment.
#[derive(Clone, Default)]
pub(crate) struct Debounce(Rc<RefCell<Option<glib::SourceId>>>);

impl Debounce {
    pub(crate) fn call(&self, ms: u64, f: impl FnOnce() + 'static) {
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

pub(crate) struct Ui {
    pub(crate) data: RefCell<team::Team>,
    // pictures are rebuilt on every change: don't decode them again
    pub(crate) team_thumbs: RefCell<HashMap<String, gdk::Texture>>,
    toast: adw::ToastOverlay,
    power: gtk::Switch,
    syncing: Cell<bool>,
    restart: Debounce,
    reload: Debounce,
}

impl Ui {
    pub(crate) fn say(&self, text: &str) {
        self.toast.add_toast(adw::Toast::builder().title(text).timeout(2).build());
    }

    pub(crate) fn is_on(&self) -> bool {
        self.power.is_active()
    }

    pub(crate) fn restart_soon(&self) {
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

/// A character picture for a `size` px box: the empty margin trimmed off so
/// the character fills it, scaled down smoothly at twice the size so it stays
/// sharp.
pub(crate) fn load_texture(path: &Path, size: i32) -> Option<gdk::Texture> {
    if path.extension().is_some_and(|e| e == "glb") {
        return None;
    }
    let img = image::open(path).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    let (mut x0, mut y0, mut x1, mut y1) = (w, h, 0, 0);
    for (x, y, p) in img.enumerate_pixels() {
        if p[3] > 8 {
            x0 = x0.min(x);
            y0 = y0.min(y);
            x1 = x1.max(x);
            y1 = y1.max(y);
        }
    }
    if x1 < x0 || y1 < y0 {
        (x0, y0, x1, y1) = (0, 0, w.max(1) - 1, h.max(1) - 1);
    }
    let (cw, ch) = (x1 - x0 + 1, y1 - y0 + 1);
    let crop = image::imageops::crop_imm(&img, x0, y0, cw, ch).to_image();
    let target = (size.max(8) * 2) as f32;
    let k = (target / cw.max(ch) as f32).min(1.0);
    let (nw, nh) = (((cw as f32 * k).round() as u32).max(1), ((ch as f32 * k).round() as u32).max(1));
    let small = image::imageops::resize(&crop, nw, nh, image::imageops::FilterType::Lanczos3);
    let bytes = glib::Bytes::from_owned(small.into_raw());
    Some(gdk::MemoryTexture::new(nw as i32, nh as i32, gdk::MemoryFormat::R8g8b8a8, &bytes, nw as usize * 4).upcast())
}

/// A sprite preview, or an icon for 3D models and unreadable files.
pub(crate) fn picture(tex: Option<&gdk::Texture>, size: i32) -> gtk::Widget {
    match tex {
        Some(tex) => {
            let pic = gtk::Picture::for_paintable(tex);
            pic.set_size_request(size, size);
            pic.set_can_shrink(true);
            pic.set_content_fit(gtk::ContentFit::Contain);
            pic.upcast()
        }
        None => gtk::Image::builder().icon_name("applications-graphics-symbolic").pixel_size(size / 2).build().upcast(),
    }
}

/// One setting: a slider and a number box sharing one value, and a button
/// back to the default once it's been changed.
fn tuning_row(ui: &Rc<Ui>, f: &'static Field) -> (adw::ActionRow, gtk::Adjustment) {
    let adj = gtk::Adjustment::new(f.default as f64, f.min as f64, f.max as f64, f.step as f64, f.step as f64 * 10.0, 0.0);
    adj.set_value(config::tuning_value(f.key).unwrap_or(f.default as f64));
    let shown = |v: f32| format!("{v:.*}", f.digits as usize);
    let row = adw::ActionRow::builder().title(f.label).subtitle(format!("Default {}", shown(f.default))).build();
    let scale = gtk::Scale::builder().adjustment(&adj).width_request(200).valign(gtk::Align::Center).draw_value(false).build();
    let spin = gtk::SpinButton::builder().adjustment(&adj).digits(f.digits).width_chars(6).valign(gtk::Align::Center).build();
    let reset = gtk::Button::builder().icon_name("edit-undo-symbolic").tooltip_text("Back to default").valign(gtk::Align::Center).build();
    reset.add_css_class("flat");
    reset.add_css_class("circular");
    let is_default = move |v: f64| (v - f.default as f64).abs() < 1e-4;
    reset.set_opacity(if is_default(adj.value()) { 0.0 } else { 1.0 });
    reset.set_sensitive(!is_default(adj.value()));
    let a = adj.clone();
    reset.connect_clicked(move |_| a.set_value(f.default as f64));
    row.add_suffix(&scale);
    row.add_suffix(&spin);
    row.add_suffix(&reset);
    let u = ui.clone();
    adj.connect_value_changed(move |a| {
        let v = a.value();
        reset.set_opacity(if is_default(v) { 0.0 } else { 1.0 });
        reset.set_sensitive(!is_default(v));
        config::set_tuning(f.key, (!is_default(v)).then_some(v));
        u.reload_soon();
    });
    (row, adj)
}

/// The current value of a tuning number.
fn tuning_now(key: &str) -> f64 {
    let default = FIELDS.iter().find(|f| f.key == key).map_or(0.0, |f| f.default as f64);
    config::tuning_value(key).unwrap_or(default)
}

/// One button per game, to start it right now.
fn play_now(ui: &Rc<Ui>) -> gtk::Widget {
    let flow = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .max_children_per_line(3)
        .min_children_per_line(2)
        .row_spacing(10)
        .column_spacing(10)
        .homogeneous(true)
        .build();
    let games: [(&str, &str, &'static str, &'static str); 12] = [
        ("Jumping contest", "go-up-symbolic", "contest", "game_min"),
        ("Cursor game", "input-mouse-symbolic", "cursor", "game_min"),
        ("Long jump", "go-next-symbolic", "long", "long_min"),
        ("Domino", "view-dual-symbolic", "domino", "party_min"),
        ("Tug of war", "object-flip-horizontal-symbolic", "tug", "tug_min"),
        ("Tag", "find-location-symbolic", "tag", "party_min"),
        ("Samba", "audio-x-generic-symbolic", "samba", "samba_min"),
        ("Volleyball", "weather-clear-symbolic", "volley", "volley_min"),
        ("Carrot grab", "emoji-food-symbolic", "carrot", "carrot_min"),
        ("Gift", "emblem-favorite-symbolic", "gift", "gift_min"),
        ("Blind man's buff", "view-conceal-symbolic", "blind", "blind_min"),
        ("Fishing", "weather-showers-symbolic", "fishing", "fish_min"),
    ];
    for (label, icon, name, min_key) in games {
        let inner = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).margin_top(10).margin_bottom(10).build();
        inner.append(&gtk::Image::builder().icon_name(icon).pixel_size(28).build());
        inner.append(&gtk::Label::new(Some(label)));
        let b = gtk::Button::builder().child(&inner).build();
        b.add_css_class("tile");
        let u = ui.clone();
        b.connect_clicked(move |_| {
            let need = tuning_now(min_key) as usize;
            let have = u.data.borrow().enabled().len();
            if !daemon::running() {
                u.say("Anima is off");
            } else if have < need {
                u.say(&format!("This game needs at least {need} characters, {have} are on"));
            } else {
                daemon::game(name);
                u.say(&format!("{label}!"));
            }
        });
        flow.insert(&b, -1);
    }
    let inner = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).margin_top(10).margin_bottom(10).build();
    inner.append(&gtk::Image::builder().icon_name("media-seek-forward-symbolic").pixel_size(28).build());
    inner.append(&gtk::Label::new(Some("Race")));
    let race = gtk::Button::builder().child(&inner).build();
    race.add_css_class("tile");
    race.add_css_class("suggested-action");
    race.connect_clicked(|_| daemon::race());
    flow.insert(&race, -1);
    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::builder().title("Play now").description("Starts straight away with the characters nearest the screen, whatever their energy").build();
    group.add(&flow);
    page.add(&group);
    page.upcast()
}

/// A tab of behaviour settings, like a settings app: sections on the left
/// (with a search over everything), the chosen section's settings on the right.
fn tuning_page(ui: &Rc<Ui>, tab: &str) -> gtk::Widget {
    let search = gtk::SearchEntry::builder().placeholder_text("Search").margin_start(10).margin_end(10).margin_top(10).margin_bottom(6).build();
    let nav = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::Single).build();
    nav.add_css_class("navigation-sidebar");
    let stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).hexpand(true).build();
    // per section: name, the count label in the list, its settings
    type Section = (String, gtk::Label, Vec<(adw::ActionRow, gtk::Adjustment, &'static Field)>);
    let sections: Rc<RefCell<Vec<Section>>> = Rc::default();
    let nav_row = |title: &str, count: &gtk::Label| {
        let b = gtk::Box::builder().spacing(8).margin_top(8).margin_bottom(8).margin_start(6).margin_end(6).build();
        b.append(&gtk::Label::builder().label(title).xalign(0.0).hexpand(true).wrap(true).build());
        b.append(count);
        gtk::ListBoxRow::builder().child(&b).build()
    };
    if tab == "Games" {
        let count = gtk::Label::new(None);
        nav.append(&nav_row("Play now", &count));
        stack.add_named(&play_now(ui), Some("0"));
        sections.borrow_mut().push(("Play now".into(), count, Vec::new()));
    }
    let mut groups: Vec<&str> = Vec::new();
    for f in FIELDS.iter().filter(|f| f.tab == tab) {
        if !groups.contains(&f.group) {
            groups.push(f.group);
        }
    }
    for g in groups {
        let k = sections.borrow().len();
        let page = adw::PreferencesPage::new();
        let group = adw::PreferencesGroup::builder().title(g).description("Changes apply straight away").build();
        page.add(&group);
        let mut rows = Vec::new();
        for f in FIELDS.iter().filter(|f| f.tab == tab && f.group == g) {
            let (row, adj) = tuning_row(ui, f);
            group.add(&row);
            rows.push((row, adj, f));
        }
        let reset = gtk::Button::builder().label("Reset this section").halign(gtk::Align::Center).margin_top(12).build();
        reset.add_css_class("pill");
        let adjs: Vec<(gtk::Adjustment, f64)> = rows.iter().map(|(_, a, f)| (a.clone(), f.default as f64)).collect();
        reset.connect_clicked(move |_| adjs.iter().for_each(|(a, d)| a.set_value(*d)));
        let foot = adw::PreferencesGroup::new();
        foot.add(&reset);
        page.add(&foot);
        stack.add_named(&page, Some(&k.to_string()));
        let count = gtk::Label::new(None);
        count.add_css_class("badge");
        nav.append(&nav_row(g, &count));
        // the badge says how many in it are changed
        let update = {
            let (count, adjs) = (count.clone(), rows.iter().map(|(_, a, f)| (a.clone(), f.default as f64)).collect::<Vec<_>>());
            move || {
                let n = adjs.iter().filter(|(a, d)| (a.value() - d).abs() > 1e-4).count();
                count.set_label(&n.to_string());
                count.set_visible(n > 0);
            }
        };
        update();
        let update = Rc::new(update);
        for (_, a, _) in &rows {
            let u = update.clone();
            a.connect_value_changed(move |_| u());
        }
        sections.borrow_mut().push((g.to_string(), count, rows));
    }
    let st = stack.clone();
    nav.connect_row_selected(move |_, row| {
        if let Some(r) = row {
            st.set_visible_child_name(&r.index().to_string());
        }
    });
    if let Some(first) = nav.row_at_index(0) {
        nav.select_row(Some(&first));
    }
    // search: keep the sections and settings that match, open the first
    let (secs, nv) = (sections.clone(), nav.clone());
    search.connect_search_changed(move |e| {
        let q = e.text().trim().to_lowercase();
        let mut first = None;
        for (k, (name, _, rows)) in secs.borrow().iter().enumerate() {
            let whole = q.is_empty() || name.to_lowercase().contains(&q);
            let mut any = whole;
            for (row, _, f) in rows {
                let hit = whole || f.label.to_lowercase().contains(&q);
                row.set_visible(hit);
                any |= hit;
            }
            if let Some(r) = nv.row_at_index(k as i32) {
                r.set_visible(any);
            }
            if any && first.is_none() {
                first = Some(k);
            }
        }
        if let Some(r) = first.and_then(|k| nv.row_at_index(k as i32)) {
            nv.select_row(Some(&r));
        }
    });
    let side = gtk::Box::builder().orientation(gtk::Orientation::Vertical).width_request(250).build();
    side.add_css_class("sidebar-pane");
    side.append(&search);
    side.append(&gtk::ScrolledWindow::builder().child(&nav).vexpand(true).hscrollbar_policy(gtk::PolicyType::Never).build());
    let body = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    body.append(&side);
    body.append(&stack);
    body.upcast()
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

    let look = adw::PreferencesGroup::builder().title("Look").build();
    spin(&look, "character", "opacity", "Opacity", 1.0, 0.05, 1.0, 0.05, 2);
    switch(&look, "character", "flip_x", "Mirror", false);
    choice(&look, "placement", "Placement", "screen-front", &[
        ("screen-front", "In front of windows"),
        ("wallpaper", "On the wallpaper"),
        ("window-inside", "Inside the window"),
        ("window-outside", "Outside the window"),
    ]);
    choice(&look, "corner", "Corner", "bottom-right", &[
        ("bottom-right", "Bottom right"),
        ("bottom-left", "Bottom left"),
        ("top-right", "Top right"),
        ("top-left", "Top left"),
    ]);
    let offset = config::read_offset();
    for (index, title) in [(0usize, "Horizontal offset (px)"), (1, "Vertical offset (px)")] {
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

    let sound = adw::PreferencesGroup::builder().title("Sound and music").build();
    switch(&sound, "character", "audio_reactive", "React to music", true);
    switch(&sound, "character", "voice", "Character voices", true);
    spin(&sound, "character", "voice_volume", "Volume", 0.6, 0.0, 1.0, 0.05, 2);
    switch(&sound, "general", "audio_capture", "Listen to desktop audio", true);
    page.add(&sound);

    let models = adw::PreferencesGroup::builder().title("3D characters").build();
    switch(&models, "character", "screen_light", "Light from screen colours", true);
    switch(&models, "character", "weapons", "Show weapons", false);
    switch(&models, "character", "windows", "One window per workspace (Hyprland)", true);
    page.add(&models);

    let perf = adw::PreferencesGroup::builder().title("Performance").build();
    spin(&perf, "character", "fps", "Character frame rate", 144.0, 10.0, 360.0, 1.0, 0);
    spin(&perf, "general", "fps", "Wallpaper frame rate", 60.0, 10.0, 360.0, 1.0, 0);
    spin(&perf, "general", "battery_fps", "Frame rate on battery", 30.0, 5.0, 360.0, 1.0, 0);
    switch(&perf, "general", "pause_on_fullscreen", "Pause on fullscreen", true);
    switch(&perf, "general", "pause_on_maximized", "Pause on maximized windows", false);
    switch(&perf, "general", "pause_on_battery", "Pause on battery", false);
    spin(&perf, "general", "low_battery_pause", "Pause on low battery (%)", 15.0, 0.0, 100.0, 1.0, 0);
    page.add(&perf);
    page.upcast()
}

const STYLE: &str = "
.sidebar-pane { background-color: alpha(currentColor, 0.035); border-right: 1px solid alpha(currentColor, 0.08); }
.badge { background-color: @accent_bg_color; color: @accent_fg_color; border-radius: 99px; padding: 0 7px; font-size: 0.8em; font-weight: bold; }
.tile { border-radius: 16px; padding: 6px; }
.char-card { border-radius: 18px; padding: 12px 8px 10px 8px; background-color: alpha(currentColor, 0.045); transition: background-color 150ms ease; }
.char-card:hover { background-color: alpha(currentColor, 0.09); }
.char-card.off { opacity: 0.5; }
.main-badge { background-color: @accent_bg_color; color: @accent_fg_color; border-radius: 99px; padding: 1px 9px; font-size: 0.8em; font-weight: bold; }
.bottom-bar { padding: 10px 14px; border-radius: 14px; background-color: alpha(currentColor, 0.045); }
.picker { background-color: @window_bg_color; border-left: 1px solid alpha(currentColor, 0.12); }
";

pub fn build_window(app: &adw::Application) -> adw::ApplicationWindow {
    let css = gtk::CssProvider::new();
    css.load_from_string(STYLE);
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
    let power = gtk::Switch::builder().valign(gtk::Align::Center).tooltip_text("Turn Anima on or off").active(daemon::running()).build();
    let ui = Rc::new(Ui {
        data: RefCell::new(team::Team::load()),
        team_thumbs: RefCell::default(),
        toast: adw::ToastOverlay::new(),
        power: power.clone(),
        syncing: Cell::new(false),
        restart: Debounce::default(),
        reload: Debounce::default(),
    });

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
    let open_tab = std::env::var("ANIMA_GUI_TAB").ok();
    stack.add_titled_with_icon(&roster::page(&ui), Some("characters"), "Team", "avatar-default-symbolic");
    for (tab, icon) in [
        ("Roaming", "find-location-symbolic"),
        ("Touch", "input-mouse-symbolic"),
        ("Social", "user-available-symbolic"),
        ("Games", "applications-games-symbolic"),
        ("Race", "media-seek-forward-symbolic"),
    ] {
        stack.add_titled_with_icon(&tuning_page(&ui, tab), Some(tab), tab, icon);
    }
    stack.add_titled_with_icon(&general_page(&ui), Some("general"), "General", "emblem-system-symbolic");

    if let Some(t) = open_tab {
        stack.set_visible_child_name(&t);
    }
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&adw::ViewSwitcher::builder().stack(&stack).policy(adw::ViewSwitcherPolicy::Wide).build()));
    let power_box = gtk::Box::builder().spacing(8).build();
    power_box.append(&power);
    header.pack_end(&power_box);

    let view = adw::ToolbarView::new();
    view.set_content(Some(&stack));
    view.add_top_bar(&header);
    ui.toast.set_child(Some(&view));

    adw::ApplicationWindow::builder().application(app).title("Anima").default_width(1120).default_height(740).content(&ui.toast).build()
}
