//! Hyprland behind [`Compositor`]: its UNIX sockets (`.socket.sock` for
//! requests, `.socket2.sock` for events), the row of workspaces the
//! characters walk along, and contrib/hyprland-plugin for exact workspace
//! offsets and window placement. Without the plugin, swipes and workspace
//! animations are replayed from Hyprland's own logic.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use calloop::channel::Sender;

use super::compositor::{Compositor, Row, WindowState};
use super::governor::{GovernorEvent, Strip};
use crate::render::character::WindowRect;

pub struct Hyprland;

impl Hyprland {
    pub fn detect() -> Option<Self> {
        hypr_socket1().map(|_| Self)
    }
}

impl Compositor for Hyprland {
    fn name(&self) -> &'static str {
        "Hyprland"
    }

    fn run(&self, tx: Sender<GovernorEvent>) {
        run(tx);
    }

    fn window_state(&self) -> Option<WindowState> {
        query_hypr_active_window()
    }

    fn strip(&self) -> Option<Strip> {
        hypr_view()
    }

    fn swipe(&self, active: bool) {
        *SWIPE.lock().unwrap() = active.then(Swipe::default);
    }

    fn swipe_scroll(&self, dx: f32) {
        if let Some(sw) = SWIPE.lock().unwrap().as_mut() {
            sw.dx += dx;
            sw.path += dx.abs();
            sw.frames += 1;
        }
    }

    fn can_place_windows(&self) -> bool {
        hypr_plugin::loaded()
    }

    fn set_window_mode(&self, on: bool) {
        WINDOW_MODE.store(on, std::sync::atomic::Ordering::Relaxed);
    }

    fn row(&self) -> Option<Row> {
        hypr_row()
    }

    fn place_workspace_window(&self, slot: usize) {
        hypr_place_workspace(slot);
    }
}

fn run(tx: Sender<GovernorEvent>) {
    let Some(path) = hypr_socket2() else {
        super::governor::poll_forever(tx);
        return;
    };

    let stream = match UnixStream::connect(&path) {
        Ok(stream) => stream,
        Err(err) => {
            log::debug!("cannot connect to Hyprland IPC ({}): {err}", path.display());
            super::governor::poll_forever(tx);
            return;
        }
    };

    // clicks, drags and gestures come from evdev; the position stays exact
    let bounds: super::pointer::Bounds = std::sync::Arc::new(std::sync::Mutex::new(
        hypr_output_rect().unwrap_or(((0.0, 0.0, 1920.0, 1080.0), String::new())),
    ));
    super::pointer::spawn(tx.clone(), bounds.clone(), true);

    // Lightweight cursor + active window geometry tracker (handles dragging/resizing windows smoothly).
    let tx_poll = tx.clone();
    std::thread::Builder::new()
        .name("anima-cursor".into())
        .spawn(move || {
            let mut ticks = 0u32;
            let mut last_pos = (-1.0f32, -1.0f32);
            let mut last_rect: Option<WindowRect> = None;
            loop {
                if let Some((x, y)) = query_hypr_cursor() {
                    if (x - last_pos.0).abs() > 1.0 || (y - last_pos.1).abs() > 1.0 {
                        last_pos = (x, y);
                        *super::pointer::SYNC.lock().unwrap() = Some((x, y));
                        if tx_poll.send(GovernorEvent::CursorPos(x, y)).is_err() {
                            return;
                        }
                    }
                }
                if ticks % 3 == 0 {
                    if let Some((fs, max, rect)) = query_hypr_active_window() {
                        if rect != last_rect {
                            last_rect = rect;
                            if tx_poll
                                .send(GovernorEvent::WindowState {
                                    fullscreen: fs,
                                    maximized: max,
                                    active_rect: rect,
                                })
                                .is_err()
                            {
                                return;
                            }
                        }
                    }
                }
                ticks = ticks.wrapping_add(1);
                if ticks % 60 == 0 {
                    if tx_poll.send(GovernorEvent::Power).is_err() {
                        return;
                    }
                }
                std::thread::sleep(Duration::from_millis(45));
            }
        })
        .ok();

    update_hypr_strip();
    log::info!("listening to Hyprland events at {}", path.display());
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let Ok(line) = line else { break };

        if ["workspace>>", "focusedmon>>", "createworkspace>>", "destroyworkspace>>", "moveworkspace>>",
            "openwindow>>", "closewindow>>", "movewindow>>"]
            .iter()
            .any(|k| line.starts_with(k))
        {
            update_hypr_strip();
        }

        if let Some(rest) = line.strip_prefix("focusedmon>>") {
            if let Some((name, _workspace)) = rest.split_once(',') {
                if let Some(r) = hypr_output_rect() {
                    *bounds.lock().unwrap() = r;
                }
                if tx
                    .send(GovernorEvent::FocusedMonitor(name.to_string()))
                    .is_err()
                {
                    return;
                }
            }
            continue;
        }

        let relevant = line.starts_with("fullscreen>>")
            || line.starts_with("activewindow>>")
            || line.starts_with("workspace>>")
            || line.starts_with("createwindow>>")
            || line.starts_with("closewindow>>")
            || line.starts_with("movewindow>>")
            || line.starts_with("openwindow>>")
            || line.starts_with("changefloatingmode>>");

        if relevant {
            if let Some((fs, max, rect)) = query_hypr_active_window() {
                if tx
                    .send(GovernorEvent::WindowState {
                        fullscreen: fs,
                        maximized: max,
                        active_rect: rect,
                    })
                    .is_err()
                {
                    return;
                }
            }
        }
    }
}

fn hypr_dir() -> Option<PathBuf> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    let signature = std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
    Some(PathBuf::from(runtime).join("hypr").join(signature))
}

fn hypr_socket1() -> Option<PathBuf> {
    let p = hypr_dir()?.join(".socket.sock");
    p.exists().then_some(p)
}

fn hypr_socket2() -> Option<PathBuf> {
    let p = hypr_dir()?.join(".socket2.sock");
    p.exists().then_some(p)
}

fn hypr_request(sock1: &Path, cmd: &[u8]) -> Option<String> {
    let mut stream = UnixStream::connect(sock1).ok()?;
    stream.set_read_timeout(Some(Duration::from_millis(30))).ok()?;
    stream.write_all(cmd).ok()?;
    let mut out = String::new();
    stream.read_to_string(&mut out).ok()?;
    Some(out)
}

fn query_hypr_cursor() -> Option<(f32, f32)> {
    let sock1 = hypr_socket1()?;
    let s = hypr_request(&sock1, b"cursorpos")?;
    let (xs, ys) = s.trim().split_once(',')?;
    let x = xs.trim().parse::<f32>().ok()?;
    let y = ys.trim().parse::<f32>().ok()?;
    Some((x, y))
}

/// Hyprland's open workspaces on the focused monitor, side by side by id,
/// one monitor width plus `general:gaps_workspaces` apart. The compositor
/// reports neither swipe offsets nor animations, so `hypr_view` replays its
/// logic (UnifiedWorkspaceSwipeGesture.cpp, WorkspaceAnimationController.cpp).
struct HyprRow {
    monitor: String,
    /// open workspace ids, sorted
    ids: Vec<i64>,
    idx: usize,
    count: usize,
    monitor_w: f32,
    /// distance between neighbouring workspaces
    step: f32,
    /// animation towards `idx`: start position and time
    from: f32,
    since: std::time::Instant,
    /// swipe in progress: `dx` where the current segment began, and the
    /// last segment delta (Hyprland's `m_delta`)
    swipe: Option<(f32, f32)>,
    /// a released swipe already moved `idx`; the event that follows only confirms it
    predicted: bool,
}

static HYPR_ROW: std::sync::Mutex<Option<HyprRow>> = std::sync::Mutex::new(None);

/// Characters are drawn into one window per workspace, placed through the
/// plugin (`hypr_place_workspace`): Hyprland slides those with their
/// workspaces itself, so the strip view stays on the active workspace
/// instead of following the slide.
static WINDOW_MODE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// A three/four-finger touchpad swipe in progress, in pixels after
/// libinput's acceleration (what Hyprland's gesture sees).
#[derive(Clone, Copy, Debug, Default)]
struct Swipe {
    /// horizontal travel so far
    dx: f32,
    /// sum of |per-frame travel| and frame count, for the average speed
    path: f32,
    frames: u32,
}

static SWIPE: std::sync::Mutex<Option<Swipe>> = std::sync::Mutex::new(None);

fn hypr_row() -> Option<Row> {
    let row = HYPR_ROW.lock().unwrap();
    let r = row.as_ref()?;
    if r.ids.is_empty() {
        return None;
    }
    // the plugin's active workspace is a frame ahead of the IPC events
    let (active, sliding) = match hypr_plugin::read(&r.monitor) {
        Some((id, offset)) => (r.ids.iter().filter(|&&i| i < id).count().min(r.ids.len() - 1), offset != 0.0),
        None => (r.idx.min(r.ids.len() - 1), false),
    };
    Some(Row { count: r.ids.len(), active, step: r.step, sliding })
}

/// Keep window `slot` on the row's `slot`th workspace, at its top-left.
fn hypr_place_workspace(slot: usize) {
    let row = HYPR_ROW.lock().unwrap();
    let Some(r) = row.as_ref() else { return };
    if let Some(&id) = r.ids.get(slot) {
        hypr_plugin::write_window(slot, id, 0.0, 0.0, &r.monitor);
    }
}



fn update_hypr_strip() {
    let Some(sock1) = hypr_socket1() else { return };
    let Some(((_, _, w, _), mon)) = hypr_output_rect() else { return };
    let json = |cmd: &[u8]| hypr_request(&sock1, cmd).and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
    let Some(active) = json(b"j/activeworkspace").and_then(|v| v["id"].as_i64()) else { return };
    let Some(list) = json(b"j/workspaces") else { return };
    // workspaces with real windows: characters alone must not keep one alive
    let busy: Vec<i64> = json(b"j/clients")
        .and_then(|c| c.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter(|c| c["class"].as_str() != Some(super::charwin::APP_ID))
        .filter_map(|c| c["workspace"]["id"].as_i64())
        .collect();
    let mut ids: Vec<i64> = list
        .as_array()
        .into_iter()
        .flatten()
        .filter(|ws| ws["monitor"].as_str() == Some(mon.as_str()))
        .filter_map(|ws| ws["id"].as_i64())
        .filter(|&id| id > 0) // special workspaces are negative
        .filter(|id| *id == active || busy.contains(id))
        .collect();
    ids.sort();
    let Some(idx) = ids.iter().position(|&id| id == active) else { return };
    let step = w + hypr_options().gaps;
    let now = std::time::Instant::now();
    let mut row = HYPR_ROW.lock().unwrap();
    let Some(r) = row.as_mut() else {
        *row = Some(HyprRow {
            monitor: mon, ids, idx, count: 0, monitor_w: w, step,
            from: idx as f32 * step, since: now, swipe: None, predicted: false,
        });
        let r = row.as_mut().unwrap();
        r.count = r.ids.len();
        return;
    };
    r.count = ids.len();
    r.ids = ids;
    r.monitor = mon;
    r.monitor_w = w;
    r.step = step;
    if r.idx == idx {
        r.predicted = false;
        return;
    }
    if std::mem::take(&mut r.predicted) || r.swipe.is_some() {
        // guessed wrong: carry on from where the row is drawn
        r.from = r.animated(now);
    } else {
        // keybind etc.: the new workspace slides in from one step away,
        // however far apart the two are
        let dir = if idx > r.idx { 1.0 } else { -1.0 };
        r.from = (idx as f32 - dir) * step;
    }
    r.since = now;
    r.idx = idx;
}

impl HyprRow {
    fn target(&self) -> f32 {
        self.idx as f32 * self.step
    }

    fn animated(&self, now: std::time::Instant) -> f32 {
        let anim = hypr_workspace_anim();
        let t = if anim.duration > 0.0 { (now - self.since).as_secs_f32() / anim.duration } else { 1.0 };
        self.from + (self.target() - self.from) * anim.ease(t.min(1.0))
    }
}

/// Where Hyprland draws the row right now: under the fingers during a
/// three-finger swipe, then its workspace animation to the target.
fn hypr_view() -> Option<Strip> {
    let mut row = HYPR_ROW.lock().unwrap();
    let r = row.as_mut()?;
    if let Some((active, offset)) = hypr_plugin::read(&r.monitor) {
        // exact: the active workspace's slot minus where Hyprland draws it.
        // A workspace not listed yet (just created) goes where its id sorts.
        let idx = r.ids.iter().filter(|&&id| id < active).count();
        let count = r.ids.len() + usize::from(!r.ids.contains(&active));
        let slide = if WINDOW_MODE.load(std::sync::atomic::Ordering::Relaxed) { 0.0 } else { offset };
        let view = (idx as f32 - slide) * r.step;
        let width = count.saturating_sub(1) as f32 * r.step + r.monitor_w;
        return Some(Strip { view_x: view, width, screen_w: r.monitor_w, ws: active as u64, exact: true });
    }
    let now = std::time::Instant::now();
    let opts = hypr_options();
    let swipe = *SWIPE.lock().unwrap();
    let last = r.count.saturating_sub(1);
    let view = match (swipe, r.swipe) {
        (Some(sw), seg) => {
            let base = seg.map_or(sw.dx, |s| s.0);
            let raw = (sw.dx - base) * if opts.invert { -1.0 } else { 1.0 };
            // no neighbour past either end: stays put
            let mut delta = raw.clamp(-opts.distance, opts.distance);
            if (delta < 0.0 && r.idx == 0) || (delta > 0.0 && r.idx >= last) {
                delta = 0.0;
            }
            let mut base = base;
            if opts.forever && delta.abs() >= opts.distance {
                // Hyprland switches right away and starts a new segment
                r.idx = if delta < 0.0 { r.idx - 1 } else { r.idx + 1 };
                r.predicted = true;
                base = sw.dx;
                delta = 0.0;
            }
            r.swipe = Some((base, delta));
            let v = r.target() + delta / opts.distance * r.step;
            r.from = v;
            r.since = now;
            v
        }
        (None, Some((_, delta))) => {
            // released: Hyprland commits past the cancel ratio or when fast enough
            r.swipe = None;
            let fast = opts.min_speed > 0.0
                && SWIPE_SPEED.lock().unwrap().is_some_and(|v| v >= opts.min_speed);
            if delta.abs() >= 2.0 && (delta.abs() >= opts.distance * opts.cancel_ratio || fast) {
                r.idx = if delta < 0.0 { r.idx.saturating_sub(1) } else { (r.idx + 1).min(last) };
                r.predicted = true;
            }
            if delta.abs() < 2.0 {
                r.from = r.target();
            }
            r.animated(now)
        }
        (None, None) => r.animated(now),
    };
    *SWIPE_SPEED.lock().unwrap() = swipe.map(|s| s.path / s.frames.max(1) as f32);
    let width = r.count.saturating_sub(1) as f32 * r.step + r.monitor_w;
    Some(Strip { view_x: view, width, screen_w: r.monitor_w, ws: r.idx as u64, exact: true })
}

/// Reader for contrib/hyprland-plugin: each monitor's active workspace and
/// the offset Hyprland draws it at, in workspace steps.
mod hypr_plugin {
    use std::sync::atomic::{AtomicU32, Ordering, fence};
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    const MAGIC: u32 = 0x414e_4859;
    const SIZE: usize = 20480;
    const RECORDS: usize = 128;
    const RECORD: usize = 64;
    const MONITORS: usize = 16;

    // mapping address (0 = none) and the last attempt to map it
    static MAP: Mutex<(usize, Option<Instant>)> = Mutex::new((0, None));

    fn map() -> Option<*const u8> {
        let mut m = MAP.lock().unwrap();
        if m.0 != 0 {
            return Some(m.0 as *const u8);
        }
        if m.1.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
            return None;
        }
        m.1 = Some(Instant::now());
        let path = std::path::PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?).join("anima-hyprland.shm");
        let file = std::fs::OpenOptions::new().read(true).write(true).open(path).ok()?;
        // shorter than the mapping would SIGBUS on read
        if file.metadata().ok()?.len() < SIZE as u64 {
            return None;
        }
        use std::os::unix::io::AsRawFd;
        let p = unsafe {
            libc::mmap(std::ptr::null_mut(), SIZE, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, file.as_raw_fd(), 0)
        };
        if p == libc::MAP_FAILED {
            return None;
        }
        log::info!("hyprland plugin: reading workspace offsets");
        m.0 = p as usize;
        Some(p as *const u8)
    }

    /// Mapping of a plugin loaded in this Hyprland instance (version 4+:
    /// 256 window slots).
    fn live() -> Option<*const u8> {
        let p = map()?;
        unsafe {
            if (*(p as *const AtomicU32)).load(Ordering::Acquire) != MAGIC || *(p.add(4) as *const u32) < 4 {
                return None;
            }
            let inst = std::slice::from_raw_parts(p.add(8), 120);
            let inst = &inst[..inst.iter().position(|&b| b == 0).unwrap_or(120)];
            let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
            (inst == sig.as_bytes()).then_some(p)
        }
    }

    pub fn loaded() -> bool {
        live().is_some()
    }

    const WINDOWS: usize = 2048;
    const MAX_WINDOWS: usize = 256;

    /// Window record `actor` (see contrib/hyprland-plugin/main.cpp): where
    /// the plugin should keep our character window.
    pub fn write_window(actor: usize, workspace: i64, x: f32, y: f32, monitor: &str) {
        if actor >= MAX_WINDOWS {
            return;
        }
        let Some(p) = live() else { return };
        unsafe {
            let rec = (p as *mut u8).add(WINDOWS + actor * RECORD);
            let seq = &*(rec as *const AtomicU32);
            let s = seq.load(Ordering::Relaxed);
            seq.store(s.wrapping_add(1) | 1, Ordering::Relaxed);
            fence(Ordering::Release);
            std::ptr::write_volatile(rec.add(4) as *mut u32, std::process::id());
            std::ptr::write_volatile(rec.add(8) as *mut u32, actor as u32);
            std::ptr::write_volatile(rec.add(12) as *mut i32, workspace as i32);
            std::ptr::write_volatile(rec.add(16) as *mut f32, x);
            std::ptr::write_volatile(rec.add(20) as *mut f32, y);
            let mut name = [0u8; 40];
            let n = monitor.len().min(39);
            name[..n].copy_from_slice(&monitor.as_bytes()[..n]);
            std::ptr::copy_nonoverlapping(name.as_ptr(), rec.add(24), 40);
            seq.store((s.wrapping_add(1) | 1).wrapping_add(1), Ordering::Release);
        }
    }

    /// (active workspace id, render offset) of `monitor`, if the plugin is
    /// loaded in this Hyprland instance.
    pub fn read(monitor: &str) -> Option<(i64, f32)> {
        let p = live()?;
        unsafe {
            for i in 0..MONITORS {
                let rec = p.add(RECORDS + i * RECORD);
                let seq = &*(rec as *const AtomicU32);
                for _ in 0..64 {
                    let s1 = seq.load(Ordering::Acquire);
                    if s1 & 1 == 1 {
                        std::hint::spin_loop();
                        continue;
                    }
                    let active = std::ptr::read_volatile(rec.add(4) as *const i32);
                    let offset = std::ptr::read_volatile(rec.add(8) as *const f32);
                    let mut name = [0u8; 48];
                    std::ptr::copy_nonoverlapping(rec.add(16), name.as_mut_ptr(), 48);
                    fence(Ordering::Acquire);
                    if seq.load(Ordering::Relaxed) != s1 {
                        continue;
                    }
                    let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(48)];
                    if name.is_empty() {
                        return None; // records fill from the front
                    }
                    if name == monitor.as_bytes() {
                        return (active > 0).then_some((active as i64, offset));
                    }
                    break;
                }
            }
        }
        None
    }
}

// average per-frame travel of the swipe, kept past its release
static SWIPE_SPEED: std::sync::Mutex<Option<f32>> = std::sync::Mutex::new(None);

struct HyprOptions {
    distance: f32,
    invert: bool,
    forever: bool,
    cancel_ratio: f32,
    min_speed: f32,
    gaps: f32,
}

/// Gesture options and `general:gaps_workspaces`, read once.
fn hypr_options() -> &'static HyprOptions {
    static OPTS: std::sync::OnceLock<HyprOptions> = std::sync::OnceLock::new();
    OPTS.get_or_init(|| {
        let get = |name: &str| {
            let sock1 = hypr_socket1()?;
            let s = hypr_request(&sock1, format!("j/getoption {name}").as_bytes())?;
            let v: serde_json::Value = serde_json::from_str(&s).ok()?;
            v.get("int")
                .and_then(|i| i.as_f64())
                .or_else(|| v.get("float")?.as_f64())
                .or_else(|| v.get("bool")?.as_bool().map(|b| b as u8 as f64))
                .map(|x| x as f32)
        };
        HyprOptions {
            distance: get("gestures:workspace_swipe_distance").unwrap_or(300.0).max(1.0),
            invert: get("gestures:workspace_swipe_invert").unwrap_or(1.0) != 0.0,
            forever: get("gestures:workspace_swipe_forever").unwrap_or(0.0) != 0.0,
            cancel_ratio: get("gestures:workspace_swipe_cancel_ratio").unwrap_or(0.5),
            min_speed: get("gestures:workspace_swipe_min_speed_to_force").unwrap_or(30.0),
            gaps: get("general:gaps_workspaces").unwrap_or(0.0),
        }
    })
}

/// The `workspacesIn` animation (what the incoming workspace plays):
/// duration (speed is in 100 ms units) and bezier, inherited from
/// `workspaces`, then `global`, when not set.
struct HyprAnim {
    duration: f32,
    p: [f32; 4],
}

impl HyprAnim {
    /// Cubic bezier from (0,0) to (1,1): solve x(s) = t, return y(s).
    fn ease(&self, t: f32) -> f32 {
        let [x1, y1, x2, y2] = self.p;
        let bez = |a: f32, b: f32, s: f32| 3.0 * a * s * (1.0 - s).powi(2) + 3.0 * b * s * s * (1.0 - s) + s.powi(3);
        let (mut lo, mut hi) = (0.0f32, 1.0f32);
        for _ in 0..24 {
            let mid = (lo + hi) / 2.0;
            if bez(x1, x2, mid) < t { lo = mid } else { hi = mid }
        }
        bez(y1, y2, (lo + hi) / 2.0)
    }
}

fn hypr_workspace_anim() -> &'static HyprAnim {
    static ANIM: std::sync::OnceLock<HyprAnim> = std::sync::OnceLock::new();
    ANIM.get_or_init(|| {
        let fallback = HyprAnim { duration: 0.8, p: [0.0, 0.75, 0.15, 1.0] };
        let Some(v) = hypr_socket1()
            .and_then(|s| hypr_request(&s, b"j/animations"))
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        else {
            return fallback;
        };
        let find = |name: &str| v[0].as_array()?.iter().find(|a| a["name"].as_str() == Some(name)).cloned();
        let Some(anim) = ["workspacesIn", "workspaces", "global"]
            .iter()
            .filter_map(|n| find(n))
            .find(|a| a["overridden"].as_bool() == Some(true))
        else {
            return fallback;
        };
        if anim["enabled"].as_bool() == Some(false) {
            return HyprAnim { duration: 0.0, ..fallback };
        }
        let name = anim["bezier"].as_str().unwrap_or("default");
        let p = v[1]
            .as_array()
            .and_then(|b| b.iter().find(|b| b["name"].as_str() == Some(name)))
            .map(|b| ["X0", "Y0", "X1", "Y1"].map(|k| b[k].as_f64().unwrap_or(0.0) as f32))
            .unwrap_or(fallback.p);
        HyprAnim { duration: anim["speed"].as_f64().unwrap_or(8.0) as f32 * 0.1, p }
    })
}

/// Focused monitor: logical rect and connector name.
fn hypr_output_rect() -> Option<((f32, f32, f32, f32), String)> {
    let v: serde_json::Value = serde_json::from_str(&hypr_request(&hypr_socket1()?, b"j/monitors")?).ok()?;
    let m = v.as_array()?.iter().find(|m| m["focused"].as_bool() == Some(true))?;
    let f = |k: &str| m[k].as_f64().unwrap_or(0.0) as f32;
    let scale = f("scale").max(0.1);
    let rect = (f("x"), f("y"), f("width") / scale, f("height") / scale);
    Some((rect, m["name"].as_str()?.to_string()))
}

fn query_hypr_active_window() -> Option<(bool, bool, Option<WindowRect>)> {
    let sock1 = hypr_socket1()?;
    let json_str = hypr_request(&sock1, b"j/activewindow")?;
    let value: serde_json::Value = serde_json::from_str(&json_str).ok()?;
    let fs_mode = value.get("fullscreen").and_then(|v| v.as_i64()).unwrap_or(0);

    let at = value.get("at").and_then(|v| v.as_array());
    let size = value.get("size").and_then(|v| v.as_array());
    let rect = match (at, size) {
        (Some(at), Some(sz)) if at.len() >= 2 && sz.len() >= 2 => {
            let x = at[0].as_f64().unwrap_or(0.0) as f32;
            let y = at[1].as_f64().unwrap_or(0.0) as f32;
            let w = sz[0].as_f64().unwrap_or(0.0) as f32;
            let h = sz[1].as_f64().unwrap_or(0.0) as f32;
            if w > 0.0 && h > 0.0 {
                Some(WindowRect { x, y, w, h })
            } else {
                None
            }
        }
        _ => None,
    };

    Some((fs_mode >= 2, fs_mode == 1, rect))
}
