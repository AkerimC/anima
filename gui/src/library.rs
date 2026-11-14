//! The wallpaper library: every wallpaper Steam Workshop and the local
//! folder provide, as a grid of previews.  Clicking one applies it to the
//! focused monitor through the daemon's control socket.

use crate::daemon;
use crate::pages::{Debounce, Ui};
use adw::prelude::*;
use anima::config::Config;
use anima::ipc::{self, Request};
use anima::wallpaper::{self, Wallpaper, WallpaperProperty, eval_property_condition};
use gtk::{gdk, glib};
use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::Path;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

const THUMB_W: i32 = 204;
const THUMB_H: i32 = 115;

fn crop_preview_rgba(path: &Path, tw: u32, th: u32) -> Option<(Vec<u8>, u32, u32)> {
    let img = image::open(path).ok()?.into_rgba8();
    let (iw, ih) = img.dimensions();
    if iw == 0 || ih == 0 {
        return None;
    }
    let k = (tw as f32 / iw as f32).max(th as f32 / ih as f32);
    let nw = ((iw as f32 * k).round() as u32).max(tw);
    let nh = ((ih as f32 * k).round() as u32).max(th);
    let small = image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Triangle);
    let crop = image::imageops::crop_imm(&small, (nw - tw) / 2, (nh - th) / 2, tw, th).to_image();
    Some((crop.into_raw(), tw, th))
}

fn rgba_to_texture(raw: Vec<u8>, w: u32, h: u32) -> gdk::Texture {
    let bytes = glib::Bytes::from_owned(raw);
    gdk::MemoryTexture::new(
        w as i32,
        h as i32,
        gdk::MemoryFormat::R8g8b8a8,
        &bytes,
        w as usize * 4,
    )
    .upcast()
}

/// A centre-cropped, downscaled first frame of `path`, sized for a card.
fn preview_texture(path: &Path) -> Option<gdk::Texture> {
    let (raw, tw, th) = crop_preview_rgba(path, THUMB_W as u32, THUMB_H as u32)?;
    Some(rgba_to_texture(raw, tw, th))
}

/// Local wallpapers first, then the Steam Workshop, de-duplicated by path.
fn collect() -> Vec<Wallpaper> {
    let cfg = anima::config::Config::load(&anima::util::config_path()).unwrap_or_default();
    let mut items: Vec<Wallpaper> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut push = |wp: Wallpaper| {
        if seen.insert(wp.entry.clone()) {
            items.push(wp);
        }
    };
    for path in wallpaper::scan(&cfg.wallpapers_dir()).into_values() {
        if let Ok(wp) = Wallpaper::resolve(&path) {
            push(wp);
        }
    }
    for path in wallpaper::scan(Path::new("examples/wallpapers")).into_values() {
        if let Ok(wp) = Wallpaper::resolve(&path) {
            push(wp);
        }
    }
    for wp in wallpaper::scan_workshop() {
        push(wp);
    }
    items
}

fn kind_badge(kind: &str) -> &'static str {
    match kind {
        "scene" => "Scene",
        "video" => "Video",
        "web" => "Web",
        "application" => "App",
        "shader" => "Shader",
        _ => "Image",
    }
}

fn placeholder() -> gtk::Widget {
    gtk::Image::builder()
        .icon_name("preferences-desktop-wallpaper-symbolic")
        .pixel_size(48)
        .width_request(THUMB_W)
        .height_request(THUMB_H)
        .build()
        .upcast()
}

fn thumbnail(tex: &gdk::Texture) -> gtk::Widget {
    let pic = gtk::Picture::for_paintable(tex);
    pic.set_content_fit(gtk::ContentFit::Cover);
    pic.set_size_request(THUMB_W, THUMB_H);
    pic.set_can_shrink(true);
    pic.upcast()
}

fn card(wp: &Wallpaper) -> (gtk::Button, gtk::Box) {
    let title = gtk::Label::builder()
        .label(&wp.title)
        .xalign(0.0)
        .width_chars(16)
        .max_width_chars(18)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .hexpand(true)
        .build();
    title.add_css_class("wp-title");

    let badge = gtk::Label::builder()
        .label(kind_badge(wp.kind.as_str()))
        .valign(gtk::Align::Center)
        .build();
    badge.add_css_class("wp-badge");

    let head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    head.append(&title);
    head.append(&badge);

    let sub = gtk::Label::builder()
        .label(match &wp.workshop_id {
            Some(id) => format!("Workshop {id}"),
            None => wp
                .root
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string(),
        })
        .xalign(0.0)
        .width_chars(16)
        .max_width_chars(20)
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .build();
    sub.add_css_class("dim-label");
    sub.add_css_class("caption");

    let body = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(5)
        .width_request(THUMB_W)
        .build();
    body.append(&placeholder());
    body.append(&head);
    body.append(&sub);

    let button = gtk::Button::builder()
        .has_frame(false)
        .child(&body)
        .hexpand(false)
        .valign(gtk::Align::Start)
        .tooltip_text(wp.entry.display().to_string())
        .build();
    button.add_css_class("wp-card");
    (button, body)
}

#[derive(Clone)]
struct DaemonProps {
    monitor: String,
    wallpaper: String,
    schema: Vec<WallpaperProperty>,
    overrides: BTreeMap<String, String>,
}

fn json_val_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn parse_daemon_props(
    data: Option<&serde_json::Value>,
    match_wallpaper: Option<&str>,
) -> Option<DaemonProps> {
    let monitors = data?.get("monitors")?.as_array()?;
    let entry = match_wallpaper
        .and_then(|target| {
            monitors
                .iter()
                .find(|m| m.get("wallpaper").and_then(|w| w.as_str()) == Some(target))
        })
        .or_else(|| monitors.first())?;
    let monitor = entry.get("monitor")?.as_str()?.to_string();
    let wallpaper = entry
        .get("wallpaper")
        .and_then(|w| w.as_str())
        .unwrap_or_default()
        .to_string();
    let schema = entry
        .get("schema")
        .and_then(|s| serde_json::from_value::<Vec<WallpaperProperty>>(s.clone()).ok())
        .unwrap_or_default();
    let mut overrides = BTreeMap::new();
    if let Some(obj) = entry.get("overrides").and_then(|o| o.as_object()) {
        for (k, v) in obj {
            overrides.insert(k.clone(), json_val_to_string(v));
        }
    }
    Some(DaemonProps {
        monitor,
        wallpaper,
        schema,
        overrides,
    })
}

fn send_ipc_bg(ui: &Rc<Ui>, req: Request, success_msg: Option<&'static str>) {
    if !daemon::running() {
        return;
    }
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let res = ipc::request(&anima::util::socket_path(), &req)
            .map(|r| r.message)
            .map_err(|e| e.to_string());
        let _ = tx.send(res);
    });
    let ui = ui.clone();
    glib::timeout_add_local(Duration::from_millis(60), move || match rx.try_recv() {
        Ok(Ok(msg)) => {
            if let Some(custom) = success_msg {
                ui.say(custom);
            } else if !msg.is_empty() && msg != "ok" {
                let _ = &msg;
            }
            glib::ControlFlow::Break
        }
        Ok(Err(err)) => {
            ui.say(&format!("property error: {err}"));
            glib::ControlFlow::Break
        }
        Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
        Err(mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
    });
}

fn bool_is_true(s: &str) -> bool {
    matches!(
        s.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn parse_we_rgba(s: &str) -> gdk::RGBA {
    let trimmed = s.trim();
    if trimmed.starts_with('#') {
        if let Ok(c) = gdk::RGBA::parse(trimmed) {
            return c;
        }
    }
    let nums: Vec<f32> = trimmed
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|t| !t.is_empty())
        .filter_map(|t| t.parse::<f32>().ok())
        .collect();
    if nums.len() >= 3 {
        let scale = if nums[0] > 1.0 || nums[1] > 1.0 || nums[2] > 1.0 {
            1.0 / 255.0
        } else {
            1.0
        };
        return gdk::RGBA::new(
            (nums[0] * scale).clamp(0.0, 1.0),
            (nums[1] * scale).clamp(0.0, 1.0),
            (nums[2] * scale).clamp(0.0, 1.0),
            1.0,
        );
    }
    gdk::RGBA::new(1.0, 1.0, 1.0, 1.0)
}

fn rgba_to_we_string(c: &gdk::RGBA) -> String {
    format!("{:.4} {:.4} {:.4}", c.red(), c.green(), c.blue())
}

fn slider_params(prop: &WallpaperProperty) -> (f64, f64, f64, u32) {
    let min = prop.min.unwrap_or(0.0);
    let max = prop
        .max
        .unwrap_or_else(|| if min >= 100.0 { min + 100.0 } else { 100.0 })
        .max(min + 0.001);
    let step = match prop.step {
        Some(s) if s > 0.0 => s,
        _ => {
            if !prop.fraction {
                if (max - min) <= 2.0 && (min.fract().abs() > 1e-6 || max.fract().abs() > 1e-6) {
                    0.01
                } else {
                    1.0
                }
            } else {
                let span = max - min;
                if span <= 2.0 {
                    0.01
                } else if span <= 20.0 {
                    0.1
                } else {
                    0.5
                }
            }
        }
    };
    let digits = if !prop.fraction && (step - step.round()).abs() < 1e-6 {
        0
    } else if step < 0.01 {
        3
    } else if step < 0.1 {
        2
    } else if step < 1.0 {
        1
    } else if prop.fraction {
        2
    } else {
        0
    };
    (min, max, step, digits)
}

fn format_slider_value(val: f64, digits: u32) -> String {
    if digits == 0 {
        format!("{}", val.round() as i64)
    } else {
        format!("{:.*}", digits as usize, val)
    }
}

fn values_differ(prop: &WallpaperProperty, cur: &str, def: &str) -> bool {
    match prop.prop_type.as_str() {
        "bool" => bool_is_true(cur) != bool_is_true(def),
        "slider" => match (cur.trim().parse::<f64>(), def.trim().parse::<f64>()) {
            (Ok(a), Ok(b)) => (a - b).abs() > 1e-4,
            _ => cur.trim() != def.trim(),
        },
        "color" => {
            let ca = parse_we_rgba(cur);
            let cb = parse_we_rgba(def);
            (ca.red() - cb.red()).abs() > 0.002
                || (ca.green() - cb.green()).abs() > 0.002
                || (ca.blue() - cb.blue()).abs() > 0.002
        }
        _ => cur.trim() != def.trim(),
    }
}

fn local_wallpaper_id(wp: &Wallpaper) -> String {
    wp.workshop_id
        .clone()
        .unwrap_or_else(|| wp.entry.to_string_lossy().into_owned())
}

struct PropRowHandle {
    prop: WallpaperProperty,
    search_text: String,
    row_widget: gtk::Widget,
    reset_btn: gtk::Button,
    apply_ui: Rc<dyn Fn(&str)>,
}

struct GroupHandle {
    group: adw::PreferencesGroup,
    cond: Option<String>,
    row_indices: Vec<usize>,
}

struct SectionSpec {
    title: String,
    cond: Option<String>,
    props: Vec<WallpaperProperty>,
}

struct InspectorState {
    ui: Rc<Ui>,
    active_wp: Option<Wallpaper>,
    active_monitor: Option<String>,
    thumb_gen: u64,
    values: BTreeMap<String, String>,
    rows: Vec<PropRowHandle>,
    groups: Vec<GroupHandle>,
    syncing: Rc<Cell<bool>>,
    header_thumb: gtk::Box,
    header_title: gtk::Label,
    header_badge: gtk::Label,
    header_sub: gtk::Label,
    reset_all_btn: gtk::Button,
    search_entry: gtk::SearchEntry,
    groups_box: gtk::Box,
    empty_lbl: gtk::Label,
}

#[derive(Clone)]
struct Inspector {
    widget: gtk::Box,
    state: Rc<RefCell<InspectorState>>,
}

impl Inspector {
    fn new(ui: &Rc<Ui>) -> Self {
        let panel = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .width_request(350)
            .hexpand(false)
            .vexpand(true)
            .build();
        panel.add_css_class("picker");

        let header_thumb = gtk::Box::builder()
            .width_request(80)
            .height_request(45)
            .valign(gtk::Align::Center)
            .build();
        header_thumb.append(
            &gtk::Image::builder()
                .icon_name("preferences-desktop-wallpaper-symbolic")
                .pixel_size(24)
                .hexpand(true)
                .vexpand(true)
                .build(),
        );

        let header_title = gtk::Label::builder()
            .label("No wallpaper selected")
            .xalign(0.0)
            .max_width_chars(26)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .hexpand(true)
            .build();
        header_title.add_css_class("wp-title");

        let header_badge = gtk::Label::builder()
            .label("")
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        header_badge.add_css_class("wp-badge");

        let title_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        title_row.append(&header_title);
        title_row.append(&header_badge);

        let header_sub = gtk::Label::builder()
            .label("Click any wallpaper to inspect & customize")
            .xalign(0.0)
            .max_width_chars(30)
            .ellipsize(gtk::pango::EllipsizeMode::Middle)
            .build();
        header_sub.add_css_class("dim-label");
        header_sub.add_css_class("caption");

        let meta_col = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(3)
            .hexpand(true)
            .valign(gtk::Align::Center)
            .build();
        meta_col.append(&title_row);
        meta_col.append(&header_sub);

        let reset_all_btn = gtk::Button::builder()
            .icon_name("view-refresh-symbolic")
            .tooltip_text("Reset all properties to default")
            .valign(gtk::Align::Center)
            .sensitive(false)
            .build();
        reset_all_btn.add_css_class("flat");

        let header_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(10)
            .margin_top(12)
            .margin_bottom(8)
            .margin_start(14)
            .margin_end(14)
            .build();
        header_box.append(&header_thumb);
        header_box.append(&meta_col);
        header_box.append(&reset_all_btn);

        let search_entry = gtk::SearchEntry::builder()
            .placeholder_text("Filter properties…")
            .margin_start(14)
            .margin_end(14)
            .margin_bottom(8)
            .sensitive(false)
            .build();

        let groups_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(16)
            .margin_top(4)
            .margin_bottom(18)
            .margin_start(14)
            .margin_end(14)
            .build();

        let empty_lbl = gtk::Label::builder()
            .label("Select a wallpaper from the library to customize its effects, colors, and properties.")
            .wrap(true)
            .justify(gtk::Justification::Center)
            .valign(gtk::Align::Center)
            .vexpand(true)
            .margin_start(24)
            .margin_end(24)
            .build();
        empty_lbl.add_css_class("dim-label");

        let scroll_content = gtk::Box::new(gtk::Orientation::Vertical, 0);
        scroll_content.append(&groups_box);
        scroll_content.append(&empty_lbl);

        let clamp = adw::Clamp::builder()
            .maximum_size(350)
            .tightening_threshold(350)
            .child(&scroll_content)
            .build();

        let scrolled = gtk::ScrolledWindow::builder()
            .child(&clamp)
            .vexpand(true)
            .hscrollbar_policy(gtk::PolicyType::External)
            .propagate_natural_width(false)
            .min_content_width(350)
            .max_content_width(350)
            .build();

        panel.append(&header_box);
        panel.append(&search_entry);
        panel.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        panel.append(&scrolled);

        let state = Rc::new(RefCell::new(InspectorState {
            ui: ui.clone(),
            active_wp: None,
            active_monitor: None,
            thumb_gen: 0,
            values: BTreeMap::new(),
            rows: Vec::new(),
            groups: Vec::new(),
            syncing: Rc::new(Cell::new(false)),
            header_thumb,
            header_title,
            header_badge,
            header_sub,
            reset_all_btn: reset_all_btn.clone(),
            search_entry: search_entry.clone(),
            groups_box,
            empty_lbl,
        }));

        {
            let st = state.clone();
            search_entry.connect_search_changed(move |_| {
                refresh_visibility(&st.borrow());
            });
        }
        {
            let st = state.clone();
            reset_all_btn.connect_clicked(move |_| {
                reset_all(&st);
            });
        }

        Self {
            widget: panel,
            state,
        }
    }

    fn widget(&self) -> gtk::Box {
        self.widget.clone()
    }

    fn matches_wallpaper(&self, entry: &Path) -> bool {
        self.state
            .borrow()
            .active_wp
            .as_ref()
            .is_some_and(|w| w.entry == entry)
    }

    fn load_wallpaper(
        &self,
        wp: &Wallpaper,
        monitor: Option<String>,
        schema: Option<Vec<WallpaperProperty>>,
        overrides: Option<BTreeMap<String, String>>,
    ) {
        let mut st = self.state.borrow_mut();
        let same_wp = st.active_wp.as_ref().is_some_and(|w| w.entry == wp.entry);
        st.active_wp = Some(wp.clone());
        if monitor.is_some() {
            st.active_monitor = monitor;
        } else if st.active_monitor.is_none() {
            let cfg = Config::load(&anima::util::config_path()).unwrap_or_default();
            st.active_monitor = cfg.monitors.first().map(|m| m.name.clone());
        }

        let props = schema.unwrap_or_else(|| wp.properties.clone());
        let saved = overrides.unwrap_or_else(|| {
            let cfg = Config::load(&anima::util::config_path()).unwrap_or_default();
            let mon = st.active_monitor.as_deref().unwrap_or("");
            cfg.properties_for(mon, &local_wallpaper_id(wp))
        });

        st.header_title.set_label(&wp.title);
        st.header_badge.set_label(kind_badge(wp.kind.as_str()));
        st.header_badge.set_visible(true);
        let sub = match &wp.workshop_id {
            Some(id) => match &st.active_monitor {
                Some(mon) => format!("Workshop {id} · {mon}"),
                None => format!("Workshop {id}"),
            },
            None => wp
                .root
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_string(),
        };
        st.header_sub.set_label(&sub);
        st.search_entry.set_sensitive(!props.is_empty());
        st.reset_all_btn.set_sensitive(!props.is_empty());

        let sections = build_sections(&props);
        let same_schema = same_wp && {
            let expected = sections
                .iter()
                .flat_map(|s| s.props.iter().map(|p| (p.key.as_str(), p.prop_type.as_str())));
            let existing = st
                .rows
                .iter()
                .map(|r| (r.prop.key.as_str(), r.prop.prop_type.as_str()));
            expected.eq(existing)
        };
        if same_schema {
            st.syncing.set(true);
            let mut changed_updates: Vec<(Rc<dyn Fn(&str)>, gtk::Button, bool, String, String)> =
                Vec::new();
            for handle in &st.rows {
                let new_val = saved
                    .get(&handle.prop.key)
                    .cloned()
                    .unwrap_or_else(|| handle.prop.default_value.clone());
                if st.values.get(&handle.prop.key) != Some(&new_val) {
                    let is_dirty =
                        values_differ(&handle.prop, &new_val, &handle.prop.default_value);
                    changed_updates.push((
                        handle.apply_ui.clone(),
                        handle.reset_btn.clone(),
                        is_dirty,
                        handle.prop.key.clone(),
                        new_val,
                    ));
                }
            }
            for (apply_ui, reset_btn, is_dirty, key, new_val) in changed_updates {
                apply_ui(&new_val);
                reset_btn.set_sensitive(is_dirty);
                st.values.insert(key, new_val);
            }
            st.syncing.set(false);
            refresh_visibility(&st);
            return;
        }

        st.thumb_gen = st.thumb_gen.wrapping_add(1);
        let target_gen = st.thumb_gen;
        while let Some(c) = st.header_thumb.first_child() {
            st.header_thumb.remove(&c);
        }
        st.header_thumb.append(
            &gtk::Image::builder()
                .icon_name("preferences-desktop-wallpaper-symbolic")
                .pixel_size(24)
                .hexpand(true)
                .vexpand(true)
                .build(),
        );
        if let Some(preview_path) = wp.preview.clone() {
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(crop_preview_rgba(&preview_path, 160, 90));
            });
            let st_rc = self.state.clone();
            glib::timeout_add_local(Duration::from_millis(30), move || match rx.try_recv() {
                Ok(Some((raw, w, h))) => {
                    let s = st_rc.borrow();
                    if s.thumb_gen == target_gen {
                        let tex = rgba_to_texture(raw, w, h);
                        let pic = gtk::Picture::for_paintable(&tex);
                        pic.set_content_fit(gtk::ContentFit::Cover);
                        pic.set_size_request(80, 45);
                        pic.set_can_shrink(true);
                        while let Some(c) = s.header_thumb.first_child() {
                            s.header_thumb.remove(&c);
                        }
                        s.header_thumb.append(&pic);
                    }
                    glib::ControlFlow::Break
                }
                Ok(None) | Err(mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
                Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
            });
        }

        while let Some(c) = st.groups_box.first_child() {
            st.groups_box.remove(&c);
        }
        st.rows.clear();
        st.groups.clear();
        st.values.clear();

        for p in &props {
            let v = saved
                .get(&p.key)
                .cloned()
                .unwrap_or_else(|| p.default_value.clone());
            st.values.insert(p.key.clone(), v);
        }

        let groups_box = st.groups_box.clone();
        let syncing = st.syncing.clone();
        let initial_values = st.values.clone();
        drop(st);

        let mut built_rows = Vec::new();
        let mut built_groups = Vec::new();

        for sec in sections {
            let group = adw::PreferencesGroup::builder()
                .title(&sec.title)
                .build();
            let mut row_indices = Vec::new();
            for prop in sec.props {
                if prop.prop_type == "text" {
                    let row = adw::ActionRow::builder()
                        .title(&prop.text)
                        .use_markup(false)
                        .build();
                    row.set_title_lines(0);
                    row.add_css_class("dim-label");
                    group.add(&row);
                    let idx = built_rows.len();
                    row_indices.push(idx);
                    built_rows.push(PropRowHandle {
                        search_text: prop.text.to_lowercase(),
                        prop,
                        row_widget: row.upcast(),
                        reset_btn: gtk::Button::new(),
                        apply_ui: Rc::new(|_| {}),
                    });
                    continue;
                }
                let cur = initial_values
                    .get(&prop.key)
                    .cloned()
                    .unwrap_or_else(|| prop.default_value.clone());
                let handle = build_prop_row(&self.state, &syncing, &prop, &cur);
                group.add(&handle.row_widget);
                let idx = built_rows.len();
                row_indices.push(idx);
                built_rows.push(handle);
            }
            if !row_indices.is_empty() {
                groups_box.append(&group);
                built_groups.push(GroupHandle {
                    group,
                    cond: sec.cond,
                    row_indices,
                });
            }
        }

        let mut st = self.state.borrow_mut();
        st.rows = built_rows;
        st.groups = built_groups;
        refresh_visibility(&st);
    }
}

fn build_sections(props: &[WallpaperProperty]) -> Vec<SectionSpec> {
    let mut sections: Vec<SectionSpec> = Vec::new();
    let mut playback = SectionSpec {
        title: "Playback & Color Adjustments".to_string(),
        cond: None,
        props: Vec::new(),
    };
    let mut current: Option<SectionSpec> = None;

    for prop in props {
        let is_playback = prop.order.unwrap_or(0) < 0;
        if is_playback {
            if prop.prop_type != "group" && prop.prop_type != "text" {
                playback.props.push(prop.clone());
            }
            continue;
        }
        match prop.prop_type.as_str() {
            "group" => {
                if let Some(prev) = current.take() {
                    if !prev.props.is_empty() {
                        sections.push(prev);
                    }
                }
                let title = if prop.text.trim().is_empty() {
                    "Properties".to_string()
                } else {
                    prop.text.clone()
                };
                current = Some(SectionSpec {
                    title,
                    cond: prop.condition.clone(),
                    props: Vec::new(),
                });
            }
            "text" => {
                let text = prop.text.trim();
                if text.is_empty() {
                    continue;
                }
                if text.len() <= 80 {
                    if let Some(prev) = current.take() {
                        if !prev.props.is_empty() {
                            sections.push(prev);
                        }
                    }
                    current = Some(SectionSpec {
                        title: text.to_string(),
                        cond: prop.condition.clone(),
                        props: Vec::new(),
                    });
                } else {
                    current
                        .get_or_insert_with(|| SectionSpec {
                            title: "Wallpaper Properties".to_string(),
                            cond: None,
                            props: Vec::new(),
                        })
                        .props
                        .push(prop.clone());
                }
            }
            _ => {
                current
                    .get_or_insert_with(|| SectionSpec {
                        title: "Wallpaper Properties".to_string(),
                        cond: None,
                        props: Vec::new(),
                    })
                    .props
                    .push(prop.clone());
            }
        }
    }

    if let Some(last) = current {
        if !last.props.is_empty() {
            sections.push(last);
        }
    }
    if !playback.props.is_empty() {
        sections.push(playback);
    }
    sections
}

fn on_user_change(state: &Rc<RefCell<InspectorState>>, key: &str, new_val: String) {
    let (ui, monitor) = {
        let mut st = state.borrow_mut();
        st.values.insert(key.to_string(), new_val.clone());
        for handle in &st.rows {
            if handle.prop.key == key {
                let changed = values_differ(&handle.prop, &new_val, &handle.prop.default_value);
                handle.reset_btn.set_sensitive(changed);
            }
        }
        refresh_visibility(&st);
        (st.ui.clone(), st.active_monitor.clone())
    };
    send_ipc_bg(
        &ui,
        Request::SetProperty {
            monitor,
            key: key.to_string(),
            value: new_val,
        },
        None,
    );
}

fn reset_single(state: &Rc<RefCell<InspectorState>>, key: &str) {
    let (ui, monitor) = {
        let mut st = state.borrow_mut();
        let Some(idx) = st.rows.iter().position(|r| r.prop.key == key) else {
            return;
        };
        let def = st.rows[idx].prop.default_value.clone();
        let apply_ui = st.rows[idx].apply_ui.clone();
        st.syncing.set(true);
        apply_ui(&def);
        st.rows[idx].reset_btn.set_sensitive(false);
        st.syncing.set(false);
        st.values.insert(key.to_string(), def);
        refresh_visibility(&st);
        (st.ui.clone(), st.active_monitor.clone())
    };
    send_ipc_bg(
        &ui,
        Request::ResetProperties {
            monitor,
            key: Some(key.to_string()),
        },
        None,
    );
}

fn reset_all(state: &Rc<RefCell<InspectorState>>) {
    let (ui, monitor) = {
        let mut st = state.borrow_mut();
        if st.rows.is_empty() {
            return;
        }
        st.syncing.set(true);
        let resets: Vec<(String, String, Rc<dyn Fn(&str)>)> = st
            .rows
            .iter()
            .filter(|r| r.prop.prop_type != "text")
            .map(|r| {
                r.reset_btn.set_sensitive(false);
                (
                    r.prop.key.clone(),
                    r.prop.default_value.clone(),
                    r.apply_ui.clone(),
                )
            })
            .collect();
        for (_, def, apply_ui) in &resets {
            apply_ui(def);
        }
        st.syncing.set(false);
        for (k, def, _) in resets {
            st.values.insert(k, def);
        }
        refresh_visibility(&st);
        (st.ui.clone(), st.active_monitor.clone())
    };
    send_ipc_bg(
        &ui,
        Request::ResetProperties { monitor, key: None },
        Some("Reset wallpaper properties to default"),
    );
}

fn refresh_visibility(st: &InspectorState) {
    let q = st.search_entry.text().trim().to_lowercase();
    let mut row_vis = Vec::with_capacity(st.rows.len());
    for handle in &st.rows {
        let cond_ok = handle
            .prop
            .condition
            .as_deref()
            .map(|c| eval_property_condition(c, &st.values))
            .unwrap_or(true);
        let search_ok = q.is_empty() || handle.search_text.contains(&q);
        let vis = cond_ok && search_ok;
        handle.row_widget.set_visible(vis);
        row_vis.push(vis);
    }

    let mut any_visible = false;
    for g in &st.groups {
        let g_cond_ok = g
            .cond
            .as_deref()
            .map(|c| eval_property_condition(c, &st.values))
            .unwrap_or(true);
        let has_vis_row = g.row_indices.iter().any(|&i| row_vis[i]);
        let g_vis = g_cond_ok && has_vis_row;
        g.group.set_visible(g_vis);
        if g_vis {
            any_visible = true;
        }
    }

    if st.active_wp.is_none() {
        st.empty_lbl.set_label(
            "Select a wallpaper from the library to customize its effects, colors, and properties.",
        );
        st.empty_lbl.set_visible(true);
    } else if !any_visible {
        st.empty_lbl.set_label(if q.is_empty() {
            "No adjustable properties for this wallpaper."
        } else {
            "No matching properties."
        });
        st.empty_lbl.set_visible(true);
    } else {
        st.empty_lbl.set_visible(false);
    }
}

fn make_reset_button(state: &Rc<RefCell<InspectorState>>, prop: &WallpaperProperty, cur: &str) -> gtk::Button {
    let btn = gtk::Button::builder()
        .icon_name("edit-undo-symbolic")
        .tooltip_text("Reset to default")
        .valign(gtk::Align::Center)
        .sensitive(values_differ(prop, cur, &prop.default_value))
        .build();
    btn.add_css_class("flat");
    let st = state.clone();
    let key = prop.key.clone();
    btn.connect_clicked(move |_| reset_single(&st, &key));
    btn
}

fn build_prop_row(
    state: &Rc<RefCell<InspectorState>>,
    syncing: &Rc<Cell<bool>>,
    prop: &WallpaperProperty,
    cur: &str,
) -> PropRowHandle {
    let title = if prop.text.trim().is_empty() {
        prop.key.clone()
    } else {
        prop.text.clone()
    };
    let search_text = format!("{} {}", title, prop.key).to_lowercase();
    let reset_btn = make_reset_button(state, prop, cur);

    if prop.prop_type == "combo" && !prop.options.is_empty() {
        let labels: Vec<&str> = prop.options.iter().map(|o| o.label.as_str()).collect();
        let model = gtk::StringList::new(&labels);
        let find_idx = |val: &str| -> u32 {
            let trimmed = val.trim();
            prop.options
                .iter()
                .position(|o| o.value.trim() == trimmed)
                .or_else(|| {
                    prop.options
                        .iter()
                        .position(|o| o.value.trim().eq_ignore_ascii_case(trimmed))
                })
                .unwrap_or(0) as u32
        };

        let row = adw::ComboRow::builder()
            .title(&title)
            .use_markup(false)
            .model(&model)
            .selected(find_idx(cur))
            .build();
        row.set_title_lines(2);
        row.set_tooltip_text(Some(&prop.key));
        row.add_suffix(&reset_btn);

        let st = state.clone();
        let sync = syncing.clone();
        let key = prop.key.clone();
        let options = prop.options.clone();
        row.connect_selected_notify(move |r| {
            if sync.get() {
                return;
            }
            if let Some(opt) = options.get(r.selected() as usize) {
                on_user_change(&st, &key, opt.value.clone());
            }
        });

        let row_ref = row.clone();
        let options_ref = prop.options.clone();
        let apply_ui: Rc<dyn Fn(&str)> = Rc::new(move |val: &str| {
            let trimmed = val.trim();
            let idx = options_ref
                .iter()
                .position(|o| o.value.trim() == trimmed)
                .or_else(|| {
                    options_ref
                        .iter()
                        .position(|o| o.value.trim().eq_ignore_ascii_case(trimmed))
                })
                .unwrap_or(0) as u32;
            row_ref.set_selected(idx);
        });

        return PropRowHandle {
            prop: prop.clone(),
            search_text,
            row_widget: row.upcast(),
            reset_btn,
            apply_ui,
        };
    }

    let row = adw::ActionRow::builder()
        .title(&title)
        .use_markup(false)
        .build();
    row.set_title_lines(2);
    row.set_tooltip_text(Some(&prop.key));

    let apply_ui: Rc<dyn Fn(&str)> = match prop.prop_type.as_str() {
        "bool" => {
            let sw = gtk::Switch::builder()
                .active(bool_is_true(cur))
                .valign(gtk::Align::Center)
                .build();
            row.add_suffix(&sw);
            let st = state.clone();
            let sync = syncing.clone();
            let key = prop.key.clone();
            sw.connect_active_notify(move |s| {
                if sync.get() {
                    return;
                }
                let val = if s.is_active() { "true" } else { "false" };
                on_user_change(&st, &key, val.to_string());
            });
            let sw_ref = sw;
            Rc::new(move |val: &str| {
                sw_ref.set_active(bool_is_true(val));
            })
        }
        "slider" => {
            let (min, max, step, digits) = slider_params(prop);
            let cur_num = cur
                .trim()
                .parse::<f64>()
                .unwrap_or_else(|_| prop.default_value.trim().parse::<f64>().unwrap_or(min))
                .clamp(min, max);
            let adj = gtk::Adjustment::new(cur_num, min, max, step, step * 10.0, 0.0);
            let scale = gtk::Scale::new(gtk::Orientation::Horizontal, Some(&adj));
            scale.set_draw_value(false);
            scale.set_hexpand(false);
            scale.set_size_request(84, -1);
            scale.set_valign(gtk::Align::Center);

            let spin = gtk::SpinButton::new(Some(&adj), step, digits);
            spin.set_width_chars(5);
            spin.set_valign(gtk::Align::Center);

            let ctrl = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            ctrl.append(&scale);
            ctrl.append(&spin);
            row.add_suffix(&ctrl);

            let st = state.clone();
            let sync = syncing.clone();
            let key = prop.key.clone();
            let deb = Debounce::default();
            adj.connect_value_changed(move |a| {
                if sync.get() {
                    return;
                }
                let val_str = format_slider_value(a.value(), digits);
                {
                    let mut s = st.borrow_mut();
                    s.values.insert(key.clone(), val_str.clone());
                    for h in &s.rows {
                        if h.prop.key == key {
                            h.reset_btn
                                .set_sensitive(values_differ(&h.prop, &val_str, &h.prop.default_value));
                        }
                    }
                    refresh_visibility(&s);
                }
                let st2 = st.clone();
                let key2 = key.clone();
                deb.call(35, move || {
                    let (ui, monitor) = {
                        let s = st2.borrow();
                        (s.ui.clone(), s.active_monitor.clone())
                    };
                    send_ipc_bg(
                        &ui,
                        Request::SetProperty {
                            monitor,
                            key: key2,
                            value: val_str,
                        },
                        None,
                    );
                });
            });

            let adj_ref = adj;
            Rc::new(move |val: &str| {
                let v = val.trim().parse::<f64>().unwrap_or(min).clamp(min, max);
                adj_ref.set_value(v);
            })
        }
        "color" => {
            let dialog = gtk::ColorDialog::builder()
                .modal(true)
                .with_alpha(false)
                .build();
            let btn = gtk::ColorDialogButton::new(Some(dialog));
            btn.set_rgba(&parse_we_rgba(cur));
            btn.set_valign(gtk::Align::Center);
            row.add_suffix(&btn);

            let st = state.clone();
            let sync = syncing.clone();
            let key = prop.key.clone();
            let deb = Debounce::default();
            btn.connect_rgba_notify(move |b| {
                if sync.get() {
                    return;
                }
                let val_str = rgba_to_we_string(&b.rgba());
                let st2 = st.clone();
                let key2 = key.clone();
                deb.call(35, move || {
                    on_user_change(&st2, &key2, val_str);
                });
            });

            let btn_ref = btn;
            Rc::new(move |val: &str| {
                btn_ref.set_rgba(&parse_we_rgba(val));
            })
        }
        _ => {
            let entry = gtk::Entry::builder()
                .text(cur)
                .valign(gtk::Align::Center)
                .width_chars(10)
                .max_width_chars(14)
                .build();
            row.add_suffix(&entry);

            let st = state.clone();
            let sync = syncing.clone();
            let key = prop.key.clone();
            let deb = Debounce::default();
            entry.connect_changed(move |e| {
                if sync.get() {
                    return;
                }
                let val_str = e.text().to_string();
                let st2 = st.clone();
                let key2 = key.clone();
                deb.call(120, move || {
                    on_user_change(&st2, &key2, val_str);
                });
            });

            let entry_ref = entry;
            Rc::new(move |val: &str| {
                entry_ref.set_text(val);
            })
        }
    };

    row.add_suffix(&reset_btn);
    PropRowHandle {
        prop: prop.clone(),
        search_text,
        row_widget: row.upcast(),
        reset_btn,
        apply_ui,
    }
}

fn apply(ui: &Rc<Ui>, wp: &Wallpaper, button: gtk::Button, inspector: &Inspector) {
    inspector.load_wallpaper(wp, None, None, None);
    let path = wp.root.display().to_string();
    let entry_path = wp.entry.clone();
    let entry_str = wp.entry.display().to_string();
    let title = wp.title.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        if !daemon::running() {
            daemon::start();
            std::thread::sleep(Duration::from_millis(1200));
        }
        let result = ipc::request(
            &anima::util::socket_path(),
            &Request::Set {
                monitor: None,
                path,
            },
        );
        let msg = result.map(|r| r.message).map_err(|e| e.to_string());
        let daemon_props = if msg.is_ok() {
            ipc::request(
                &anima::util::socket_path(),
                &Request::ListProperties { monitor: None },
            )
            .ok()
            .and_then(|r| parse_daemon_props(r.data.as_ref(), Some(&entry_str)))
        } else {
            None
        };
        let _ = tx.send((msg, daemon_props));
    });
    let ui = ui.clone();
    let wp = wp.clone();
    let inspector = inspector.clone();
    glib::timeout_add_local(Duration::from_millis(120), move || match rx.try_recv() {
        Ok((Ok(msg), daemon_props)) => {
            ui.say(&msg);
            ui.set_current_card(&button);
            if inspector.matches_wallpaper(&entry_path) {
                if let Some(dp) = daemon_props {
                    inspector.load_wallpaper(
                        &wp,
                        Some(dp.monitor),
                        Some(dp.schema),
                        Some(dp.overrides),
                    );
                }
            }
            glib::ControlFlow::Break
        }
        Ok((Err(err), _)) => {
            ui.say(&format!("could not apply {title}: {err}"));
            glib::ControlFlow::Break
        }
        Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
        Err(mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
    });
}

/// Ask the daemon which wallpaper is up and mark that card, off the UI thread.
fn mark_current(
    ui: &Rc<Ui>,
    cards: &Rc<RefCell<Vec<(gtk::Button, Wallpaper)>>>,
    inspector: &Inspector,
) {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let status = ipc::request(&anima::util::socket_path(), &Request::Status { monitor: None })
            .ok()
            .and_then(|r| r.data);
        let first_mon = status
            .as_ref()
            .and_then(|d| d.get("monitors")?.as_array()?.first())
            .cloned();
        let current = first_mon
            .as_ref()
            .and_then(|m| m.get("wallpaper")?.as_str().map(str::to_string));
        let mon_name = first_mon
            .as_ref()
            .and_then(|m| m.get("monitor")?.as_str().map(str::to_string));
        let daemon_props = if let Some(ref cur) = current {
            ipc::request(
                &anima::util::socket_path(),
                &Request::ListProperties { monitor: mon_name },
            )
            .ok()
            .and_then(|r| parse_daemon_props(r.data.as_ref(), Some(cur)))
        } else {
            None
        };
        let _ = tx.send((current, daemon_props));
    });
    let (ui, cards, inspector) = (ui.clone(), cards.clone(), inspector.clone());
    glib::timeout_add_local(Duration::from_millis(150), move || match rx.try_recv() {
        Ok((Some(current), daemon_props)) => {
            if let Some((btn, wp)) = cards
                .borrow()
                .iter()
                .find(|(_, w)| w.entry.display().to_string() == current)
            {
                ui.set_current_card(btn);
                if let Some(dp) = daemon_props {
                    let _ = &dp.wallpaper;
                    inspector.load_wallpaper(
                        wp,
                        Some(dp.monitor),
                        Some(dp.schema),
                        Some(dp.overrides),
                    );
                } else {
                    inspector.load_wallpaper(wp, None, None, None);
                }
            }
            glib::ControlFlow::Break
        }
        Ok((None, _)) | Err(mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
        Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
    });
}

pub fn page(ui: &Rc<Ui>) -> gtk::Widget {
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search wallpapers")
        .hexpand(true)
        .build();
    let count = gtk::Label::new(None);
    count.add_css_class("dim-label");
    let rescan = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Rescan wallpapers")
        .build();

    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    bar.add_css_class("bottom-bar");
    bar.set_margin_top(10);
    bar.set_margin_bottom(10);
    bar.set_margin_start(14);
    bar.set_margin_end(14);
    bar.append(&search);
    bar.append(&count);
    bar.append(&rescan);

    let flow = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .homogeneous(true)
        .column_spacing(12)
        .row_spacing(12)
        .margin_top(12)
        .margin_bottom(18)
        .margin_start(14)
        .margin_end(14)
        .valign(gtk::Align::Start)
        .max_children_per_line(4)
        .min_children_per_line(1)
        .build();

    let scrolled = gtk::ScrolledWindow::builder()
        .child(&flow)
        .hexpand(true)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_width(false)
        .build();

    let left = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .hexpand(true)
        .vexpand(true)
        .build();
    left.append(&bar);
    left.append(&scrolled);

    let inspector = Inspector::new(ui);

    let page = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    page.append(&left);
    page.append(&inspector.widget());

    let cards: Rc<RefCell<Vec<(gtk::Button, Wallpaper)>>> = Rc::default();

    let populate = {
        let ui = ui.clone();
        let flow = flow.clone();
        let count = count.clone();
        let cards = cards.clone();
        let inspector = inspector.clone();
        move || {
            while let Some(child) = flow.first_child() {
                flow.remove(&child);
            }
            cards.borrow_mut().clear();
            let items = collect();
            count.set_label(&format!("{} wallpapers", items.len()));
            let mut pending = Vec::new();
            for wp in &items {
                let (button, body) = card(wp);
                button.set_widget_name(&wp.title);
                cards.borrow_mut().push((button.clone(), wp.clone()));
                if let Some(preview) = wp.preview.clone() {
                    pending.push((preview, body.clone()));
                }
                let ui = ui.clone();
                let wp = wp.clone();
                let btn = button.clone();
                let insp = inspector.clone();
                button.connect_clicked(move |_| apply(&ui, &wp, btn.clone(), &insp));
                flow.append(&button);
            }
            for (i, (path, body)) in pending.into_iter().enumerate() {
                // Decode off the frame that builds the window: one preview per
                // idle tick keeps the grid responsive on a large library.
                glib::timeout_add_local_once(
                    Duration::from_millis(i as u64 * 12),
                    move || {
                        if let Some(tex) = preview_texture(&path) {
                            if let Some(old) = body.first_child() {
                                body.remove(&old);
                            }
                            body.prepend(&thumbnail(&tex));
                        }
                    },
                );
            }
            mark_current(&ui, &cards, &inspector);
        }
    };
    populate();

    let rebuild = populate.clone();
    rescan.connect_clicked(move |_| rebuild());

    let flow2 = flow.clone();
    let cards2 = cards.clone();
    search.connect_search_changed(move |e| {
        let q = e.text().trim().to_lowercase();
        for (button, _) in cards2.borrow().iter() {
            let hit = q.is_empty() || button.widget_name().to_lowercase().contains(&q);
            button.set_visible(hit);
        }
        let _ = &flow2;
    });

    page.upcast()
}
