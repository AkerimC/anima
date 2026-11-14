//! The Anima Park window: a park of their own where characters visit or live
//! and play on the things built on the lawn. Shares what's built (`park.json`);
//! the daemon updates `world.json` with which swings/slides/seesaws are busy.

use adw::prelude::*;
use anima::render::character::park;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use gtk::{gio, glib};

use crate::{config, daemon};

/// The choices, and the setting each one is (park_mode).
const MODES: [(&str, f64); 4] = [
    ("Live on the desktop", 0.0),
    ("Live on the desktop, visit the park", 3.0),
    ("Live in Anima Park", 1.0),
    ("Live in Anima Park, go out now and then", 2.0),
];

/// Window state: what's built and what's currently in use.
struct Shared {
    park: park::Park,
    world: park::World,
    world_mtime: Option<std::time::SystemTime>,
    unit_frac: Option<f32>,
    // background, drawn once per window size (w, h, unit)
    backs: HashMap<(i32, i32, i32), gtk::cairo::ImageSurface>,
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
                    backs: HashMap::new(),
                }))
            })
            .clone()
    })
}

fn new_id() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(1, |d| d.as_micros() as u64)
}

/// `anima-gui --park`: opens or focuses the Anima Park window.
pub fn run() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(park::APP_ID).flags(gio::ApplicationFlags::HANDLES_COMMAND_LINE).build();
    app.connect_command_line(|app, _| {
        let title = park::place_title("park");
        match app.windows().into_iter().find(|w| w.title().is_some_and(|t| t == title)) {
            Some(w) => w.present(),
            None => build(app),
        }
        0.into()
    });
    app.connect_activate(|_| {});
    app.run()
}

/// Window selection, active drag start coordinates, and pending tube snap target.
struct Win {
    sel: Option<usize>,
    drag_from: Option<f32>,
    drag_y: f32,
    drag_moved: bool,
    cycle_pt: Option<(f64, f64)>,
    snap: Option<(usize, park::TubeSeg)>,
}

fn thing_hits(t: &park::Thing, tx_px: f64, gy: f64, u: f64, x: f64, y: f64) -> bool {
    let tube_r = (park::TUBE_RADIUS as f64 + 0.06) * u;
    match t.kind.as_str() {
        "slide" => {
            let layout = park::slide_layout(t);
            let ladder_x = tx_px + layout.ladder_x as f64 * u;
            let lip_x = tx_px + layout.lip_x as f64 * u;
            let top_y = gy - layout.top as f64 * u;
            let on_ladder = (x - ladder_x).abs() <= 0.22 * u && y >= top_y - 0.36 * u && y <= gy + 8.0;
            let on_plat = x >= ladder_x.min(lip_x) - 0.14 * u && x <= ladder_x.max(lip_x) + 0.14 * u && (y - top_y).abs() <= 0.18 * u;
            let on_tube = layout.samples.iter().any(|s| {
                let sx = tx_px + s.x as f64 * u;
                let sy = gy - (s.h as f64 + park::TUBE_RADIUS as f64) * u;
                (x - sx).hypot(y - sy) <= tube_r
            });
            on_ladder || on_plat || on_tube
        }
        "tube_straight" | "tube_turn" => {
            let (pts, _) = loose_tube_curve(t, tx_px, gy, u);
            let c32 = [
                (pts[0].0 as f32, pts[0].1 as f32),
                (pts[1].0 as f32, pts[1].1 as f32),
                (pts[2].0 as f32, pts[2].1 as f32),
                (pts[3].0 as f32, pts[3].1 as f32),
            ];
            (0..=16).any(|k| {
                let ((sx, sy), _) = park::eval_cubic(c32, k as f32 / 16.0);
                (x - sx as f64).hypot(y - sy as f64) <= tube_r
            })
        }
        _ => {
            let half = park::width(t) as f64 * u * 0.5;
            (x - tx_px).abs() <= half && y >= gy - tall(t) * u && y <= gy + 6.0
        }
    }
}

fn hits_at(sh: &Shared, x: f64, y: f64, w: f64, h: f64) -> Vec<usize> {
    let unit = unit_px(sh, w);
    let mut order: Vec<(usize, &park::Thing)> = sh.park.things.iter().enumerate().filter(|(_, t)| t.place == "park").collect();
    // Loose tube pieces take priority first, then nearest (`lowest y`) first
    order.sort_by(|a, b| tube_kind_of(&a.1.kind).is_none().cmp(&tube_kind_of(&b.1.kind).is_none()).then_with(|| a.1.y.total_cmp(&b.1.y)));
    order
        .into_iter()
        .filter(|(_, t)| {
            let u = unit * park::depth_scale(t.y) as f64;
            let tx_px = t.x as f64 * w;
            let gy = h - 3.0 - t.y as f64 * h;
            thing_hits(t, tx_px, gy, u, x, y)
        })
        .map(|(k, _)| k)
        .collect()
}

fn tube_kind_of(kind: &str) -> Option<park::TubeSeg> {
    match kind {
        "tube_straight" => Some(park::TubeSeg::Straight),
        "tube_turn" => Some(park::TubeSeg::Turn),
        _ => None,
    }
}

fn slide_exit_px(t: &park::Thing, w: f64, h: f64, unit: f64) -> (f64, f64, f32) {
    let layout = park::slide_layout(t);
    let x = t.x as f64 * w;
    let gy = h - 3.0 - t.y as f64 * h;
    let u = unit * park::depth_scale(t.y) as f64;
    let cx = x + layout.exit_x as f64 * u;
    let cy = gy - (layout.exit_h as f64 + park::TUBE_RADIUS as f64) * u;
    (cx, cy, layout.exit_dir)
}

fn loose_tube_curve(t: &park::Thing, x: f64, gy: f64, u: f64) -> ([(f64, f64); 4], f64) {
    let seg = tube_kind_of(&t.kind).unwrap_or(park::TubeSeg::Straight);
    let dir = if t.flip { -1.0_f32 } else { 1.0_f32 };
    let (pts, _) = park::next_tube_curve(0.0, 0.0, dir, seg);
    let min_x = pts.iter().map(|p| p.0).fold(f32::INFINITY, f32::min);
    let max_x = pts.iter().map(|p| p.0).fold(f32::NEG_INFINITY, f32::max);
    let min_h = pts.iter().map(|p| p.1).fold(f32::INFINITY, f32::min);
    let cx = (min_x + max_x) * 0.5;
    let lift_h = park::TUBE_RADIUS - min_h + 0.04;
    let map = |p: (f32, f32)| (x + (p.0 - cx) as f64 * u, gy - (p.1 + lift_h) as f64 * u);
    ([map(pts[0]), map(pts[1]), map(pts[2]), map(pts[3])], dir as f64)
}

fn find_snap_target(sh: &Shared, dragged: usize, w: f64, h: f64) -> Option<(usize, park::TubeSeg)> {
    let piece = sh.park.things.get(dragged)?;
    let seg = tube_kind_of(&piece.kind)?;
    let unit = unit_px(sh, w);
    let pu = unit * park::depth_scale(piece.y) as f64;
    let px = piece.x as f64 * w;
    let pgy = h - 3.0 - piece.y as f64 * h;
    let (inlet_x, inlet_y) = loose_tube_curve(piece, px, pgy, pu).0[0];
    let mut best: Option<(usize, f64)> = None;
    for (idx, st) in sh.park.things.iter().enumerate() {
        if idx == dragged || st.kind != "slide" || st.place != piece.place {
            continue;
        }
        if park::slide_segments(st).len() >= park::MAX_TUBE_PIECES {
            continue;
        }
        let (ex, ey, _) = slide_exit_px(st, w, h, unit);
        let su = unit * park::depth_scale(st.y) as f64;
        let dist = (inlet_x - ex).hypot(inlet_y - ey);
        if dist <= 0.95 * su && best.is_none_or(|(_, bd)| dist < bd) {
            best = Some((idx, dist));
        }
    }
    best.map(|(idx, _)| (idx, seg))
}

fn save(sh: &Shared) {
    park::write_park(&sh.park);
}

fn build(app: &adw::Application) {
    let title = park::place_title("park");
    let win = adw::ApplicationWindow::builder().application(app).title(title).default_width(1280).default_height(760).build();
    let sh = shared();
    let me = Rc::new(RefCell::new(Win {
        sel: None,
        drag_from: None,
        drag_y: 0.0,
        drag_moved: false,
        cycle_pt: None,
        snap: None,
    }));

    let ground = gtk::DrawingArea::builder().hexpand(true).vexpand(true).build();
    let (s2, m2) = (sh.clone(), me.clone());
    ground.set_draw_func(move |_, cr, w, h| {
        let (w, h) = (w as f64, h as f64);
        let mut sh = s2.borrow_mut();
        let me = m2.borrow();
        let unit = unit_px(&sh, w);
        let key = (w as i32, h as i32, unit as i32);
        if !sh.backs.contains_key(&key) {
            sh.backs.clear();
            if let Ok(surface) = gtk::cairo::ImageSurface::create(gtk::cairo::Format::Rgb24, w as i32, h as i32) {
                if let Ok(c) = gtk::cairo::Context::new(&surface) {
                    draw_park(&c, w, h);
                }
                surface.flush();
                sh.backs.insert(key, surface);
            }
        }
        if let Some(back) = sh.backs.get(&key) {
            cr.set_source_surface(back, 0.0, 0.0).ok();
            cr.paint().ok();
        }
        // Far ones first, near ones in front, and loose un-snapped tubes always on top
        let mut things: Vec<(usize, park::Thing)> = sh.park.things.iter().cloned().enumerate().filter(|(_, t)| t.place == "park").collect();
        things.sort_by(|a, b| tube_kind_of(&a.1.kind).is_some().cmp(&tube_kind_of(&b.1.kind).is_some()).then_with(|| b.1.y.total_cmp(&a.1.y)));
        for (k, t) in &things {
            let busy = sh.world.busy.contains(k);
            let x = t.x as f64 * w;
            let gy = h - 3.0 - t.y as f64 * h;
            let u = unit * park::depth_scale(t.y) as f64;
            draw_thing(cr, t, x, gy, u, busy, me.sel == Some(*k));
        }
        // Highlight slide snap nozzles and ghost preview when dragging a loose tube piece
        if me.drag_from.is_some()
            && let Some(sel) = me.sel
            && let Some(dragged) = sh.park.things.get(sel)
            && tube_kind_of(&dragged.kind).is_some()
        {
            for (idx, st) in sh.park.things.iter().enumerate() {
                if st.kind != "slide" || st.place != "park" || park::slide_segments(st).len() >= park::MAX_TUBE_PIECES {
                    continue;
                }
                let su = unit * park::depth_scale(st.y) as f64;
                let (ex, ey, _) = slide_exit_px(st, w, h, unit);
                let snapped_here = me.snap.is_some_and(|(si, _)| si == idx);
                if snapped_here {
                    if let Some((_, seg)) = me.snap {
                        let layout = park::slide_layout(st);
                        let (pts, _) = park::next_tube_curve(layout.exit_x, layout.exit_h, layout.exit_dir, seg);
                        let sx = st.x as f64 * w;
                        let sgy = h - 3.0 - st.y as f64 * h;
                        let map = |p: (f32, f32)| (sx + p.0 as f64 * su, sgy - (p.1 as f64 + park::TUBE_RADIUS as f64) * su);
                        stroke_tube_ghost(cr, map(pts[0]), map(pts[1]), map(pts[2]), map(pts[3]), su);
                    }
                    cr.set_source_rgba(0.25, 0.95, 0.55, 0.95);
                    cr.set_line_width(3.5);
                } else {
                    cr.set_source_rgba(0.45, 0.9, 1.0, 0.65);
                    cr.set_line_width(2.0);
                    cr.set_dash(&[5.0, 4.0], 0.0);
                }
                cr.arc(ex, ey, 0.34 * su, 0.0, std::f64::consts::TAU);
                cr.stroke().ok();
                cr.set_dash(&[], 0.0);
            }
        }
    });

    // drag things anywhere across the park lawn; a click picks one
    let drag = gtk::GestureDrag::new();
    let (s2, m2, g2) = (sh.clone(), me.clone(), ground.clone());
    drag.connect_drag_begin(move |_, x, y| {
        let sh = s2.borrow();
        let mut me = m2.borrow_mut();
        let (w, h) = (g2.width() as f64, g2.height() as f64);
        let hits = hits_at(&sh, x, y, w, h);
        if hits.is_empty() {
            me.sel = None;
            me.cycle_pt = None;
        } else if me.sel.is_some_and(|s| hits.contains(&s)) && hits.len() > 1 {
            me.cycle_pt = Some((x, y));
        } else {
            me.sel = Some(hits[0]);
            me.cycle_pt = None;
        }
        me.drag_from = me.sel.map(|k| sh.park.things[k].x);
        me.drag_y = me.sel.map_or(0.0, |k| sh.park.things[k].y);
        me.drag_moved = false;
        me.snap = None;
        g2.queue_draw();
    });
    let (s2, m2, g2) = (sh.clone(), me.clone(), ground.clone());
    drag.connect_drag_update(move |_, dx, dy| {
        let mut sh = s2.borrow_mut();
        let mut me = m2.borrow_mut();
        let (Some(k), Some(from)) = (me.sel, me.drag_from) else { return };
        if !me.drag_moved && dx.hypot(dy) <= 4.0 {
            return;
        }
        me.drag_moved = true;
        me.cycle_pt = None;
        let w = g2.width().max(1) as f64;
        let h = g2.height().max(1) as f64;
        let new_y = (me.drag_y - (dy / h) as f32).clamp(0.0, park::MEADOW);
        sh.park.things[k].y = new_y;
        let u = unit_px(&sh, w) * park::depth_scale(new_y) as f64;
        let half = (park::width(&sh.park.things[k]) as f64 * u * 0.5 / w) as f32;
        sh.park.things[k].x = (from + (dx / w) as f32).clamp(half, (1.0 - half).max(half));
        me.snap = find_snap_target(&sh, k, w, h);
        g2.queue_draw();
    });
    // what's picked: turn it round, grow/shrink a tube slide, take it down
    let bar = gtk::Box::builder().spacing(8).halign(gtk::Align::Center).valign(gtk::Align::Start).margin_top(18).visible(false).build();
    bar.add_css_class("card");
    bar.add_css_class("park-bar");
    let name = gtk::Label::builder().build();
    name.add_css_class("heading");
    let turn = gtk::Button::builder().icon_name("object-flip-horizontal-symbolic").tooltip_text("Turn round").build();
    let add_straight = gtk::Button::builder().label("+ Straight").tooltip_text("Add a straight tube segment").build();
    let add_turn = gtk::Button::builder().label("+ Turn").tooltip_text("Add a U-turn tube segment").build();
    let pop_piece = gtk::Button::builder().label("- Piece").tooltip_text("Remove last tube segment").build();
    let remove = gtk::Button::builder().icon_name("user-trash-symbolic").tooltip_text("Remove").build();
    remove.add_css_class("destructive-action");
    let done = gtk::Button::builder().icon_name("object-select-symbolic").tooltip_text("Done").build();
    for w in [
        name.upcast_ref::<gtk::Widget>(),
        turn.upcast_ref(),
        add_straight.upcast_ref(),
        add_turn.upcast_ref(),
        pop_piece.upcast_ref(),
        remove.upcast_ref(),
        done.upcast_ref(),
    ] {
        bar.append(w);
    }
    let sync_bar = {
        let (sh, me, bar, name, add_straight, add_turn, pop_piece) =
            (sh.clone(), me.clone(), bar.clone(), name.clone(), add_straight.clone(), add_turn.clone(), pop_piece.clone());
        Rc::new(move || {
            let sh = sh.borrow();
            let me = me.borrow();
            match me.sel.and_then(|k| sh.park.things.get(k)) {
                Some(t) => {
                    let label = park::recipe(&t.kind).map_or(t.kind.as_str(), |r| r.1);
                    let is_slide = t.kind == "slide";
                    let n_segs = if is_slide { park::slide_segments(t).len() } else { 0 };
                    name.set_label(&if is_slide { format!("{label} ({n_segs} pcs)") } else { label.to_string() });
                    add_straight.set_visible(is_slide && n_segs < park::MAX_TUBE_PIECES);
                    add_turn.set_visible(is_slide && n_segs < park::MAX_TUBE_PIECES);
                    pop_piece.set_visible(is_slide && n_segs > 1);
                    bar.set_visible(true);
                }
                None => bar.set_visible(false),
            }
        })
    };
    {
        let (s2, m2, g2, sb) = (sh.clone(), me.clone(), ground.clone(), sync_bar.clone());
        drag.connect_drag_end(move |_, _, _| {
            let mut me = m2.borrow_mut();
            let was_dragging = me.drag_from.take().is_some();
            let moved = me.drag_moved;
            me.drag_moved = false;
            let cycle_pt = me.cycle_pt.take();
            let snap = me.snap.take();
            if was_dragging {
                let mut sh = s2.borrow_mut();
                if !moved && let Some((cx, cy)) = cycle_pt {
                    let hits = hits_at(&sh, cx, cy, g2.width() as f64, g2.height() as f64);
                    if let Some(cur) = me.sel
                        && let Some(pos) = hits.iter().position(|&k| k == cur)
                        && hits.len() > 1
                    {
                        me.sel = Some(hits[(pos + 1) % hits.len()]);
                    }
                } else if moved {
                    if let (Some(k), Some((target, seg))) = (me.sel, snap)
                        && target < sh.park.things.len()
                        && sh.park.things[target].kind == "slide"
                    {
                        if sh.park.things[target].tubes.is_empty() {
                            sh.park.things[target].tubes = park::slide_segments(&sh.park.things[target]);
                        }
                        if sh.park.things[target].tubes.len() < park::MAX_TUBE_PIECES {
                            sh.park.things[target].tubes.push(seg);
                            sh.park.things.remove(k);
                            me.sel = Some(if k < target { target - 1 } else { target });
                        }
                    }
                    save(&sh);
                }
                drop(sh);
                drop(me);
                g2.queue_draw();
                sb();
            }
        });
    }
    ground.add_controller(drag);
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
    for (btn, seg) in [(add_straight, park::TubeSeg::Straight), (add_turn, park::TubeSeg::Turn)] {
        let (s2, m2, g2, sb) = (sh.clone(), me.clone(), ground.clone(), sync_bar.clone());
        btn.connect_clicked(move |_| {
            let Some(k) = m2.borrow().sel else { return };
            let mut sh = s2.borrow_mut();
            let t = &mut sh.park.things[k];
            if t.tubes.is_empty() {
                t.tubes = park::slide_segments(t);
            }
            if t.tubes.len() < park::MAX_TUBE_PIECES {
                t.tubes.push(seg);
            }
            save(&sh);
            drop(sh);
            g2.queue_draw();
            sb();
        });
    }
    let (s2, m2, g2, sb) = (sh.clone(), me.clone(), ground.clone(), sync_bar.clone());
    pop_piece.connect_clicked(move |_| {
        let Some(k) = m2.borrow().sel else { return };
        let mut sh = s2.borrow_mut();
        let t = &mut sh.park.things[k];
        if t.tubes.is_empty() {
            t.tubes = park::slide_segments(t);
        }
        if t.tubes.len() > 1 {
            t.tubes.pop();
        }
        save(&sh);
        drop(sh);
        g2.queue_draw();
        sb();
    });
    let (s2, m2, g2, sb) = (sh.clone(), me.clone(), ground.clone(), sync_bar.clone());
    remove.connect_clicked(move |_| {
        let Some(k) = m2.borrow_mut().sel.take() else { return };
        let mut sh = s2.borrow_mut();
        sh.park.things.remove(k);
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
    overlay.add_overlay(&bar);

    // placing park items freely
    let list = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).margin_top(6).margin_bottom(6).margin_start(6).margin_end(6).build();
    let mut buttons = Vec::new();
    for &(id, label) in park::RECIPES.iter().filter(|r| r.0 != "slide_piece") {
        let row = gtk::Box::builder().spacing(10).build();
        row.append(&gtk::Label::builder().label(label).xalign(0.0).hexpand(true).build());
        let b = gtk::Button::builder().label("Place").valign(gtk::Align::Center).build();
        row.append(&b);
        list.append(&row);
        buttons.push((b, id));
    }
    let scroll = gtk::ScrolledWindow::builder().child(&list).propagate_natural_height(true).max_content_height(620).hscrollbar_policy(gtk::PolicyType::Never).build();
    let pop = gtk::Popover::builder().child(&scroll).build();
    let build_btn = gtk::MenuButton::builder().label("Build").popover(&pop).build();
    build_btn.add_css_class("suggested-action");
    for (b, id) in buttons {
        let (s2, m2, g2, sbar, pop) = (sh.clone(), me.clone(), ground.clone(), sync_bar.clone(), pop.clone());
        b.connect_clicked(move |_| {
            let mut sh = s2.borrow_mut();
            let w = g2.width().max(1) as f64;
            let unit = unit_px(&sh, w);
            let mut thing = park::Thing {
                kind: id.to_string(),
                place: "park".to_string(),
                id: new_id(),
                x: 0.5,
                y: 0.15,
                flip: false,
                pieces: 1,
                tubes: Vec::new(),
            };
            // Spawn loose tube pieces right beside the selected (or latest) slide's exit ready to drag-snap
            let near_slide = tube_kind_of(id).and_then(|_| {
                let target = m2.borrow().sel.and_then(|k| sh.park.things.get(k)).filter(|t| t.kind == "slide").or_else(|| sh.park.things.iter().rev().find(|t| t.kind == "slide" && t.place == "park"))?;
                let layout = park::slide_layout(target);
                let u = unit * park::depth_scale(target.y) as f64;
                let half = (park::width(&thing) as f64 * u * 0.5 / w) as f32;
                let nx = target.x + ((layout.exit_x + layout.exit_dir * 0.9) as f64 * u / w) as f32;
                Some((nx.clamp(half, (1.0 - half).max(half)), target.y))
            });
            if let Some((x, y)) = near_slide {
                thing.x = x;
                thing.y = y;
            } else {
            let find_spot = |thing: &park::Thing, sh: &Shared| -> Option<(f32, f32)> {
                for y in [0.12_f32, 0.28, 0.44, 0.58, 0.02] {
                    let u = unit * park::depth_scale(y) as f64;
                    let me_half = park::width(thing) as f64 * u * 0.5 / w;
                    let clear = |x: f64| {
                        sh.park
                            .things
                            .iter()
                            .filter(|t| t.place == "park" && (t.y - y).abs() < 0.12)
                            .all(|t| (t.x as f64 - x).abs() > me_half + park::width(t) as f64 * unit * park::depth_scale(t.y) as f64 * 0.5 / w)
                    };
                    for j in 0..45 {
                        let x = 0.5 + (if j % 2 == 0 { 1.0 } else { -1.0 }) * (j / 2) as f64 * 0.02;
                        if x > me_half && x < 1.0 - me_half && clear(x) {
                            return Some((x as f32, y));
                        }
                    }
                }
                None
            };
            if let Some((x, y)) = find_spot(&thing, &sh) {
                thing.x = x;
                thing.y = y;
            }
            }
            sh.park.things.push(thing);
            let k = sh.park.things.len() - 1;
            save(&sh);
            drop(sh);
            m2.borrow_mut().sel = Some(k);
            pop.popdown();
            g2.queue_draw();
            sbar();
        });
    }

    // where they live
    let labels: Vec<&str> = MODES.iter().map(|m| m.0).collect();
    let mode = gtk::DropDown::from_strings(&labels);
    mode.set_tooltip_text(Some("Where the characters live while Anima Park is open"));
    let now = config::tuning_value("park_mode").unwrap_or(3.0).round();
    mode.set_selected(MODES.iter().position(|m| m.1 == now).unwrap_or(1) as u32);
    mode.connect_selected_notify(|d| {
        let v = MODES.get(d.selected() as usize).map_or(3.0, |m| m.1);
        config::set_tuning("park_mode", (v != 3.0).then_some(v));
        if daemon::running() {
            daemon::reload();
        }
    });
    let header = adw::HeaderBar::new();
    header.pack_start(&build_btn);
    header.pack_end(&mode);
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header);
    view.set_content(Some(&overlay));
    win.set_content(Some(&view));

    let css = gtk::CssProvider::new();
    css.load_from_string(".park-bar { padding: 12px 14px; min-width: 170px; background-color: @window_bg_color; }");
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }

    // poll daemon busy slots and character unit fraction
    let (s2, g2, w2) = (sh.clone(), ground.clone(), win.clone());
    let mut last_slow = std::time::Instant::now() - std::time::Duration::from_secs(5);
    glib::timeout_add_local(std::time::Duration::from_millis(150), move || {
        if !w2.is_visible() && w2.application().is_none() {
            return glib::ControlFlow::Break;
        }
        let mut sh = s2.borrow_mut();
        let m = std::fs::metadata(park::world_path()).and_then(|m| m.modified()).ok();
        let mut redraw = false;
        if m != sh.world_mtime {
            sh.world_mtime = m;
            sh.world = park::read_world();
            redraw = true;
        }
        if last_slow.elapsed().as_secs_f32() > 2.0 {
            last_slow = std::time::Instant::now();
            let frac = park::read_unit_frac();
            if frac != sh.unit_frac {
                sh.unit_frac = frac;
                redraw = true;
            }
        }
        drop(sh);
        if redraw {
            g2.queue_draw();
        }
        glib::ControlFlow::Continue
    });

    win.maximize();
    win.present();
    // a workspace of its own, so it doesn't sit among other windows
    let t = title.to_string();
    std::thread::spawn(move || own_workspace(&t));
}

/// A character's height in window pixels.
fn unit_px(sh: &Shared, w: f64) -> f64 {
    sh.unit_frac.map_or(110.0, |f| f as f64 * w).max(30.0)
}

/// How tall a thing is (character heights), for picking it.
fn tall(t: &park::Thing) -> f64 {
    park::height(t) as f64
}

const TUBE_COLORS: [(f64, f64, f64); 6] = [
    (0.94, 0.36, 0.33),
    (0.98, 0.68, 0.22),
    (0.28, 0.72, 0.56),
    (0.30, 0.58, 0.92),
    (0.68, 0.42, 0.86),
    (0.94, 0.46, 0.68),
];

fn stroke_bezier(cr: &gtk::cairo::Context, p0: (f64, f64), p1: (f64, f64), p2: (f64, f64), p3: (f64, f64), dy: f64, width: f64) {
    cr.set_line_width(width);
    cr.set_line_cap(gtk::cairo::LineCap::Butt);
    cr.move_to(p0.0, p0.1 + dy);
    cr.curve_to(p1.0, p1.1 + dy, p2.0, p2.1 + dy, p3.0, p3.1 + dy);
    cr.stroke().ok();
}

fn stroke_tube(cr: &gtk::cairo::Context, p0: (f64, f64), p1: (f64, f64), p2: (f64, f64), p3: (f64, f64), u: f64, c: (f64, f64, f64)) {
    cr.set_source_rgb(c.0 * 0.6, c.1 * 0.6, c.2 * 0.6);
    stroke_bezier(cr, p0, p1, p2, p3, 0.0, 0.58 * u);
    cr.set_source_rgb(c.0, c.1, c.2);
    stroke_bezier(cr, p0, p1, p2, p3, 0.0, 0.50 * u);
    cr.set_source_rgba(0.0, 0.0, 0.0, 0.18);
    stroke_bezier(cr, p0, p1, p2, p3, 0.10 * u, 0.14 * u);
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.42);
    stroke_bezier(cr, p0, p1, p2, p3, -0.12 * u, 0.11 * u);
}

fn stroke_tube_ghost(cr: &gtk::cairo::Context, p0: (f64, f64), p1: (f64, f64), p2: (f64, f64), p3: (f64, f64), u: f64) {
    cr.set_source_rgba(0.35, 0.95, 0.72, 0.42);
    stroke_bezier(cr, p0, p1, p2, p3, 0.0, 0.52 * u);
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.75);
    cr.set_dash(&[6.0, 4.0], 0.0);
    stroke_bezier(cr, p0, p1, p2, p3, 0.0, 0.05 * u);
    cr.set_dash(&[], 0.0);
}

fn draw_flange(cr: &gtk::cairo::Context, cx: f64, cy: f64, tx: f64, ty: f64, u: f64, c: (f64, f64, f64)) {
    let (nx, ny) = (-ty, tx);
    let r = 0.29 * u;
    let a = (cx - nx * r, cy - ny * r);
    let b = (cx + nx * r, cy + ny * r);
    cr.set_source_rgb(c.0 * 0.55, c.1 * 0.55, c.2 * 0.55);
    bar(cr, a, b, 0.095 * u);
    cr.set_source_rgb((c.0 + 0.14).min(1.0), (c.1 + 0.14).min(1.0), (c.2 + 0.14).min(1.0));
    bar(cr, a, b, 0.055 * u);
}

fn draw_mouth(cr: &gtk::cairo::Context, cx: f64, cy: f64, u: f64, c: (f64, f64, f64)) {
    let _ = cr.save();
    cr.translate(cx, cy);
    cr.scale(0.45, 1.0);
    cr.set_source_rgb(c.0 * 0.6, c.1 * 0.6, c.2 * 0.6);
    cr.arc(0.0, 0.0, 0.28 * u, 0.0, std::f64::consts::TAU);
    cr.fill().ok();
    cr.set_source_rgb(0.14, 0.12, 0.20);
    cr.arc(0.0, 0.0, 0.21 * u, 0.0, std::f64::consts::TAU);
    cr.fill().ok();
    cr.set_source_rgb(c.0, c.1, c.2);
    cr.set_line_width(0.06 * u);
    cr.arc(0.0, 0.0, 0.24 * u, 0.0, std::f64::consts::TAU);
    cr.stroke().ok();
    let _ = cr.restore();
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
            let layout = park::slide_layout(t);
            let top = layout.top as f64 * u;
            let ladder = x + layout.ladder_x as f64 * u;
            let lip = x + layout.lip_x as f64 * u;
            let center_h = |h: f32| gy - (h as f64 + park::TUBE_RADIUS as f64) * u;
            let pt = |p: (f32, f32)| (x + p.0 as f64 * u, center_h(p.1));

            // support stilts behind the tube curves (under alternating joints + ladder)
            cr.set_source_rgb(0.55, 0.57, 0.62);
            for (ji, &(jx, jh, _, _)) in layout.joints.iter().enumerate() {
                if ji > 0 && (ji % 2 == 1 || ji + 1 == layout.joints.len()) {
                    let sx = x + jx as f64 * u;
                    let sy = gy - jh as f64 * u;
                    if sy < gy - 0.1 * u {
                        bar(cr, (sx, gy), (sx, sy), 0.05 * u);
                    }
                }
            }
            // ladder
            cr.set_source_rgb(0.6, 0.62, 0.68);
            for s in [-1.0, 1.0] {
                bar(cr, (ladder + s * 0.09 * u, gy), (ladder + s * 0.09 * u, gy - top - 0.32 * u), 0.04 * u);
            }
            let mut ry = gy - 0.16 * u;
            while ry > gy - top {
                bar(cr, (ladder - 0.09 * u, ry), (ladder + 0.09 * u, ry), 0.03 * u);
                ry -= 0.17 * u;
            }
            // wooden top platform
            rgb(cr, WOOD);
            rect(cr, ladder.min(lip) - 0.12 * u, gy - top - 0.02 * u, ladder.max(lip) + 0.06 * u, gy - top + 0.05 * u);

            // pipe segments (top-to-bottom order so lower turns stay clean)
            for (si, curve) in layout.curves.iter().enumerate() {
                let c = TUBE_COLORS[si % TUBE_COLORS.len()];
                stroke_tube(cr, pt(curve.pts[0]), pt(curve.pts[1]), pt(curve.pts[2]), pt(curve.pts[3]), u, c);
            }
            // entrance mouth on platform + bolted flange collars between pieces + open exit mouth
            if let Some(&(jx, jh, _, _)) = layout.joints.first() {
                draw_mouth(cr, x + jx as f64 * u, center_h(jh), u, TUBE_COLORS[0]);
            }
            for (ji, &(jx, jh, jtx, jth)) in layout.joints.iter().enumerate().skip(1) {
                if ji + 1 < layout.joints.len() {
                    let c = TUBE_COLORS[(ji - 1) % TUBE_COLORS.len()];
                    draw_flange(cr, x + jx as f64 * u, center_h(jh), jtx as f64, -(jth as f64), u, c);
                }
            }
            if let Some(&(jx, jh, _, _)) = layout.joints.last() {
                let c = TUBE_COLORS[(layout.curves.len().saturating_sub(1)) % TUBE_COLORS.len()];
                draw_mouth(cr, x + jx as f64 * u, center_h(jh), u, c);
            }
        }
        "tube_straight" | "tube_turn" => {
            let (pts, _) = loose_tube_curve(t, x, gy, u);
            let c = if t.kind == "tube_turn" { TUBE_COLORS[1] } else { TUBE_COLORS[2] };
            stroke_tube(cr, pts[0], pts[1], pts[2], pts[3], u, c);
            draw_mouth(cr, pts[0].0, pts[0].1, u, c);
            draw_mouth(cr, pts[3].0, pts[3].1, u, c);
            // glowing inlet connector indicator so it's clear which end snaps
            cr.set_source_rgba(0.3, 0.95, 0.8, 0.9);
            cr.set_line_width(2.0);
            cr.arc(pts[0].0, pts[0].1, 0.16 * u, 0.0, std::f64::consts::TAU);
            cr.stroke().ok();
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

/// Sky, far hills and a deep park lawn covering `park::MEADOW` (70%) of the window.
fn draw_park(cr: &gtk::cairo::Context, w: f64, h: f64) {
    let ground = h * (1.0 - park::MEADOW as f64);
    let sky = gtk::cairo::LinearGradient::new(0.0, 0.0, 0.0, ground);
    sky.add_color_stop_rgb(0.0, 0.47, 0.72, 0.95);
    sky.add_color_stop_rgb(1.0, 0.80, 0.92, 0.99);
    let _ = cr.set_source(&sky);
    cr.paint().ok();
    // a few clouds in the top sky band
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.85);
    for (x, y, s) in [(0.18, 0.11, 0.85), (0.55, 0.07, 1.1), (0.82, 0.14, 0.75)] {
        let (cx, cy, r) = (w * x, h * y, h * 0.032 * s);
        for (dx, dy, k) in [(-1.2, 0.3, 0.8), (0.0, 0.0, 1.1), (1.2, 0.3, 0.85), (0.5, 0.45, 0.8), (-0.5, 0.45, 0.8)] {
            cr.arc(cx + dx * r, cy + dy * r, r * k, 0.0, std::f64::consts::TAU);
            cr.fill().ok();
        }
    }
    // far hills sitting along the horizon
    cr.set_source_rgb(0.55, 0.77, 0.47);
    cr.move_to(0.0, ground);
    let steps = 64;
    for k in 0..=steps {
        let x = w * k as f64 / steps as f64;
        let y = ground - h * (0.06 + 0.03 * (x / w * 7.0).sin() + 0.02 * (x / w * 17.0 + 1.0).sin());
        cr.line_to(x, y);
    }
    cr.line_to(w, ground);
    cr.close_path();
    cr.fill().ok();
    // deep grass lawn over the lower 70% of the window
    let grass = gtk::cairo::LinearGradient::new(0.0, ground, 0.0, h);
    grass.add_color_stop_rgb(0.0, 0.48, 0.74, 0.39);
    grass.add_color_stop_rgb(1.0, 0.33, 0.58, 0.26);
    let _ = cr.set_source(&grass);
    cr.rectangle(0.0, ground, w, h - ground);
    cr.fill().ok();
    // little tufts along the horizon edge
    cr.set_source_rgb(0.36, 0.62, 0.28);
    let mut x = 6.0;
    while x < w {
        cr.move_to(x, ground + 2.0);
        cr.line_to(x + 3.0, ground - 6.0);
        cr.line_to(x + 6.0, ground + 2.0);
        cr.fill().ok();
        x += 23.0 + (x * 0.37).sin().abs() * 19.0;
    }
    // subtle tufts scattered across the lawn
    cr.set_source_rgba(0.28, 0.5, 0.22, 0.55);
    for k in 0..64 {
        let fx = (k as f64 * 0.618).fract() * w;
        let fy = ground + 0.04 * h + (k as f64 * 0.377).fract() * (h - ground - 0.06 * h);
        bar(cr, (fx, fy), (fx + 2.0, fy - 6.0), 2.0);
        bar(cr, (fx + 4.0, fy), (fx + 5.0, fy - 8.0), 2.0);
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
