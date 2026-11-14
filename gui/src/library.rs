//! The wallpaper library: every wallpaper Steam Workshop and the local
//! folder provide, as a grid of previews.  Clicking one applies it to the
//! focused monitor through the daemon's control socket.

use crate::daemon;
use crate::pages::Ui;
use adw::prelude::*;
use anima::ipc::{self, Request};
use anima::wallpaper::{self, Wallpaper};
use gtk::{gdk, glib};
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

const THUMB_W: i32 = 232;
const THUMB_H: i32 = 130;

/// A centre-cropped, downscaled first frame of `path`, sized for a card.
fn preview_texture(path: &Path) -> Option<gdk::Texture> {
    let img = image::open(path).ok()?.into_rgba8();
    let (iw, ih) = img.dimensions();
    if iw == 0 || ih == 0 {
        return None;
    }
    let (tw, th) = (THUMB_W as u32 * 2, THUMB_H as u32 * 2);
    let k = (tw as f32 / iw as f32).max(th as f32 / ih as f32);
    let nw = ((iw as f32 * k).round() as u32).max(tw);
    let nh = ((ih as f32 * k).round() as u32).max(th);
    let small = image::imageops::resize(&img, nw, nh, image::imageops::FilterType::Triangle);
    let crop = image::imageops::crop_imm(&small, (nw - tw) / 2, (nh - th) / 2, tw, th).to_image();
    let bytes = glib::Bytes::from_owned(crop.into_raw());
    Some(
        gdk::MemoryTexture::new(
            tw as i32,
            th as i32,
            gdk::MemoryFormat::R8g8b8a8,
            &bytes,
            tw as usize * 4,
        )
        .upcast(),
    )
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
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .build();
    sub.add_css_class("dim-label");
    sub.add_css_class("caption");

    let body = gtk::Box::new(gtk::Orientation::Vertical, 5);
    body.append(&placeholder());
    body.append(&head);
    body.append(&sub);

    let button = gtk::Button::builder()
        .has_frame(false)
        .child(&body)
        .tooltip_text(wp.entry.display().to_string())
        .build();
    button.add_css_class("wp-card");
    (button, body)
}

fn apply(ui: &Rc<Ui>, wp: &Wallpaper, button: gtk::Button) {
    let path = wp.root.display().to_string();
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
        let _ = tx.send(result.map(|r| r.message).map_err(|e| e.to_string()));
    });
    let ui = ui.clone();
    glib::timeout_add_local(Duration::from_millis(120), move || {
        match rx.try_recv() {
            Ok(Ok(msg)) => {
                ui.say(&msg);
                ui.set_current_card(&button);
                glib::ControlFlow::Break
            }
            Ok(Err(err)) => {
                ui.say(&format!("could not apply {title}: {err}"));
                glib::ControlFlow::Break
            }
            Err(mpsc::TryRecvError::Empty) => glib::ControlFlow::Continue,
            Err(mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
        }
    });
}

/// Ask the daemon which wallpaper is up and mark that card, off the UI thread.
fn mark_current(ui: &Rc<Ui>, cards: &Rc<RefCell<Vec<(gtk::Button, String)>>>) {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let current = ipc::request(&anima::util::socket_path(), &Request::Status { monitor: None })
            .ok()
            .and_then(|r| r.data)
            .and_then(|d| d.get("monitors")?.as_array()?.first()?.get("wallpaper")?.as_str().map(str::to_string));
        let _ = tx.send(current);
    });
    let (ui, cards) = (ui.clone(), cards.clone());
    glib::timeout_add_local(Duration::from_millis(150), move || match rx.try_recv() {
        Ok(Some(current)) => {
            if let Some((btn, _)) = cards
                .borrow()
                .iter()
                .find(|(_, entry)| entry == &current)
            {
                ui.set_current_card(btn);
            }
            glib::ControlFlow::Break
        }
        Ok(None) | Err(mpsc::TryRecvError::Disconnected) => glib::ControlFlow::Break,
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
        .max_children_per_line(5)
        .min_children_per_line(1)
        .build();

    let scrolled = gtk::ScrolledWindow::builder()
        .child(&flow)
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .build();

    let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    page.append(&bar);
    page.append(&scrolled);

    // (button, entry path, lowercase search text)
    let cards: Rc<RefCell<Vec<(gtk::Button, String)>>> = Rc::default();

    let populate = {
        let ui = ui.clone();
        let flow = flow.clone();
        let count = count.clone();
        let cards = cards.clone();
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
                let entry = wp.entry.display().to_string();
                button.set_widget_name(&wp.title);
                cards
                    .borrow_mut()
                    .push((button.clone(), entry.clone()));
                if let Some(preview) = wp.preview.clone() {
                    pending.push((preview, body.clone()));
                }
                let ui = ui.clone();
                let wp = wp.clone();
                let btn = button.clone();
                button.connect_clicked(move |_| apply(&ui, &wp, btn.clone()));
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
            mark_current(&ui, &cards);
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
