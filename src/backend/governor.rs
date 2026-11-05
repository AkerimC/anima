//! Smart Governor: compositor fullscreen/maximized detection, active window
//! geometry tracking (for `window-inside` / `window-outside` character placement),
//! and cursor tracking.
//!
//! Uses Hyprland's UNIX sockets (`.socket.sock` and `.socket2.sock`) directly
//! without spawning subprocesses, keeping CPU overhead at ~0.0%.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use calloop::channel::Sender;

use crate::render::character::WindowRect;

#[derive(Debug, Clone)]
pub enum GovernorEvent {
    WindowState {
        fullscreen: bool,
        maximized: bool,
        active_rect: Option<WindowRect>,
    },
    FocusedMonitor(String),
    CursorPos(f32, f32),
    Power,
}

pub fn spawn_governor(tx: Sender<GovernorEvent>) {
    super::notify::spawn();
    std::thread::Builder::new()
        .name("anima-governor".into())
        .spawn(move || run(tx))
        .ok();
}

fn run(tx: Sender<GovernorEvent>) {
    if let Some((fs, max, rect)) = query_window_state() {
        let _ = tx.send(GovernorEvent::WindowState {
            fullscreen: fs,
            maximized: max,
            active_rect: rect,
        });
    }

    if let Some(sock) = niri_socket() {
        run_niri(tx, &sock);
        return;
    }

    let Some(path) = hypr_socket2() else {
        poll_forever(tx);
        return;
    };

    let stream = match UnixStream::connect(&path) {
        Ok(stream) => stream,
        Err(err) => {
            log::debug!("cannot connect to Hyprland IPC ({}): {err}", path.display());
            poll_forever(tx);
            return;
        }
    };

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

    log::info!("listening to Hyprland events at {}", path.display());
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let Ok(line) = line else { break };

        if let Some(rest) = line.strip_prefix("focusedmon>>") {
            if let Some((name, _workspace)) = rest.split_once(',') {
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
            if let Some((fs, max, rect)) = query_window_state() {
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

fn poll_forever(tx: Sender<GovernorEvent>) {
    let mut last = None;
    loop {
        let state = query_window_state();
        if state != last {
            if let Some((fs, max, rect)) = state {
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
            last = state;
        }
        if tx.send(GovernorEvent::Power).is_err() {
            return;
        }
        std::thread::sleep(Duration::from_secs(2));
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

fn query_window_state() -> Option<(bool, bool, Option<WindowRect>)> {
    if let Some(res) = query_hypr_active_window() {
        return Some(res);
    }
    if let Some(sock) = niri_socket() {
        return query_niri_window(&sock);
    }

    if std::env::var_os("SWAYSOCK").is_some() {
        let output = Command::new("swaymsg")
            .args(["-t", "get_tree"])
            .output()
            .ok()?;
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout);
            let fs = text.contains("\"focused\": true") && text.contains("\"fullscreen_mode\": 1");
            return Some((fs, false, None));
        }
    }

    None
}

// niri: $NIRI_SOCKET, one JSON request per line

/// Workspaces on the focused niri output.
#[derive(Clone, Debug, Default)]
pub struct NiriWorkspaces {
    pub active: u64,
    pub all: Vec<u64>,
    /// workspaces that have at least one window
    pub busy: Vec<u64>,
}


/// niri's scrolling strip on the focused output, logical pixels.
#[derive(Clone, Copy, Debug, Default)]
pub struct NiriStrip {
    /// how far the strip is scrolled (left edge of the screen in strip coords)
    pub view_x: f32,
    /// total width of all columns
    pub width: f32,
    pub screen_w: f32,
    pub ws: u64,
    /// view_x comes straight from niri (no smoothing needed)
    pub exact: bool,
}

// tiled windows on the active workspace: (id, x of its column on our strip, tile width),
// so a WindowLayoutsChanged event alone gives the scroll position
static NIRI_WIN_X: std::sync::Mutex<Vec<(u64, f32, f32)>> = std::sync::Mutex::new(Vec::new());

/// Fast path for the per-frame layout events niri sends while scrolling.
/// False if the event needs a full refresh (unknown window, resize, ...).
fn niri_layouts_changed(line: &str) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { return false };
    let Some(changes) = v["WindowLayoutsChanged"]["changes"].as_array() else { return false };
    let wins = NIRI_WIN_X.lock().unwrap();
    let mut view = None;
    for c in changes {
        let (Some(id), layout) = (c[0].as_u64(), &c[1]) else { return false };
        let Some(&(_, col_x, width)) = wins.iter().find(|w| w.0 == id) else { continue };
        let w = layout["tile_size"][0].as_f64().unwrap_or(0.0) as f32;
        if (w - width).abs() > 0.5 {
            return false;
        }
        if let Some(x) = layout["tile_pos_in_workspace_view"][0].as_f64() {
            view.get_or_insert(col_x - x as f32);
        }
    }
    let Some(view) = view else { return false };
    if let Some(st) = NIRI_STRIP.lock().unwrap().as_mut() {
        st.view_x = view;
    }
    true
}

// niri reports tiled window positions (patched niri): no guessing needed
static NIRI_EXACT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

// a three-finger swipe is moving the strip; don't recompute it meanwhile
static NIRI_GESTURE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

// view when the swipe started, and whether it just ended (niri then snaps)
static GESTURE_START: std::sync::Mutex<Option<f32>> = std::sync::Mutex::new(None);
static GESTURE_ENDED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
// finger -> strip correction on top of niri's formula, measured on this
// touchpad (our libinput acceleration estimate runs a bit low)
const GESTURE_SCALE: f32 = 1.2;

pub fn niri_gesture(active: bool) {
    if active {
        *GESTURE_START.lock().unwrap() = NIRI_STRIP.lock().unwrap().map(|s| s.view_x);
    } else {
        GESTURE_ENDED.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    NIRI_GESTURE.store(active, std::sync::atomic::Ordering::Relaxed);
}

/// Follow niri's touchpad swipe: the strip moves by the (accelerated) finger
/// travel times output width / 1200, like niri's view_offset_gesture_update.
pub fn niri_gesture_scroll(dx: f32) {
    if !NIRI_GESTURE.load(std::sync::atomic::Ordering::Relaxed)
        || NIRI_EXACT.load(std::sync::atomic::Ordering::Relaxed)
    {
        return;
    }
    let mut strip = NIRI_STRIP.lock().unwrap();
    let Some(st) = strip.as_mut() else { return };
    st.view_x = (st.view_x + dx * st.screen_w / 1200.0 * GESTURE_SCALE).clamp(-st.screen_w * 0.2, st.width - st.screen_w * 0.8);
    if let Some(v) = NIRI_VIEWS.lock().unwrap().iter_mut().find(|v| v.0 == st.ws) {
        v.1 = st.view_x;
    }
}

pub static NIRI_STRIP: std::sync::Mutex<Option<NiriStrip>> = std::sync::Mutex::new(None);

// remembered scroll position per workspace
static NIRI_VIEWS: std::sync::Mutex<Vec<(u64, f32)>> = std::sync::Mutex::new(Vec::new());

/// niri doesn't report the scroll position, so follow its rule: scroll as
/// little as needed to show the focused column in full.
fn update_niri_strip(sock: &Path) {
    if NIRI_GESTURE.load(std::sync::atomic::Ordering::Relaxed)
        && !NIRI_EXACT.load(std::sync::atomic::Ordering::Relaxed)
    {
        return;
    }
    let Some(ws) = query_niri_workspaces(sock) else { return };
    let Some(windows) = niri_request(sock, "\"Windows\"")
        .and_then(|v| v.get("Windows").and_then(|w| w.as_array()).cloned())
    else {
        return;
    };
    let screen_w = niri_output_rect(sock).map(|r| r.2).unwrap_or(1920.0);
    let gap = niri_gaps();
    // column index -> width, and the focused column
    let mut cols: Vec<(u64, f32)> = Vec::new();
    let mut focused_col = None;
    // (column, x on screen) of a tiled window, when niri reports it
    let mut seen_pos: Option<(u64, f32)> = None;
    for w in &windows {
        if w.get("workspace_id").and_then(|v| v.as_u64()) != Some(ws.active)
            || w.get("is_floating").and_then(|v| v.as_bool()) == Some(true)
        {
            continue;
        }
        let layout = &w["layout"];
        let Some(col) = layout["pos_in_scrolling_layout"].get(0).and_then(|v| v.as_u64()) else { continue };
        let width = layout["tile_size"].get(0).and_then(|v| v.as_f64()).unwrap_or(0.0) as f32;
        match cols.iter_mut().find(|c| c.0 == col) {
            Some(c) => c.1 = c.1.max(width),
            None => cols.push((col, width)),
        }
        if w.get("is_focused").and_then(|v| v.as_bool()) == Some(true) {
            focused_col = Some(col);
        }
        if let Some(x) = layout["tile_pos_in_workspace_view"].get(0).and_then(|v| v.as_f64()) {
            seen_pos.get_or_insert((col, x as f32));
        }
    }
    cols.sort_by_key(|c| c.0);
    {
        let mut col_x = Vec::new();
        let mut x = gap;
        for (col, w) in &cols {
            col_x.push((*col, x, *w));
            x += w + gap;
        }
        let mut cache = NIRI_WIN_X.lock().unwrap();
        cache.clear();
        for w in &windows {
            if w.get("workspace_id").and_then(|v| v.as_u64()) != Some(ws.active) {
                continue;
            }
            let (Some(id), Some(col)) = (w["id"].as_u64(), w["layout"]["pos_in_scrolling_layout"][0].as_u64()) else {
                continue;
            };
            if let Some(&(_, x, cw)) = col_x.iter().find(|c| c.0 == col) {
                cache.push((id, x, cw));
            }
        }
    }
    let width = cols.iter().map(|c| c.1 + gap).sum::<f32>() + gap;
    let mut views = NIRI_VIEWS.lock().unwrap();
    let mut view = views.iter().find(|v| v.0 == ws.active).map(|v| v.1).unwrap_or(0.0);
    let ended = GESTURE_ENDED.swap(false, std::sync::atomic::Ordering::Relaxed);
    if let Some((pc, px)) = seen_pos {
        // exact: our x of that column minus where niri shows it
        NIRI_EXACT.store(true, std::sync::atomic::Ordering::Relaxed);
        let mut x = gap;
        for (col, w) in &cols {
            if *col == pc {
                view = x - px;
                break;
            }
            x += w + gap;
        }
    } else if let Some(fc) = focused_col {
        let mut x = gap;
        for (col, w) in &cols {
            if *col == fc {
                let (left, right) = (x - gap, x + w + gap - screen_w);
                if ended {
                    // after a swipe niri snaps the focused column to one edge:
                    // take the one nearest to our estimate
                    let est = view;
                    view = if (left - est).abs() <= (right - est).abs() { left } else { right };
                    if let Some(start) = GESTURE_START.lock().unwrap().take() {
                        log::debug!("niri swipe: est {:.0} real {:.0}", est - start, view - start);
                    }
                } else {
                    // smallest scroll that shows [x - gap, x + w + gap]
                    view = view.min(left).max(right);
                }
                break;
            }
            x += w + gap;
        }
    }
    if seen_pos.is_none() {
        view = view.clamp(0.0, (width - screen_w).max(0.0));
    }
    match views.iter_mut().find(|v| v.0 == ws.active) {
        Some(v) => v.1 = view,
        None => views.push((ws.active, view)),
    }
    let new = NiriStrip { view_x: view, width: width.max(screen_w), screen_w, ws: ws.active, exact: seen_pos.is_some() };
    let mut strip = NIRI_STRIP.lock().unwrap();
    if strip.is_none_or(|o| o.view_x != new.view_x || o.width != new.width) {
        log::debug!("niri strip: view {:.0} of {:.0}", new.view_x, new.width);
    }
    *strip = Some(new);
}

/// `layout { gaps N }` from the niri config (DMS keeps it in dms/layout.kdl).
fn niri_gaps() -> f32 {
    static GAPS: std::sync::OnceLock<f32> = std::sync::OnceLock::new();
    *GAPS.get_or_init(|| {
        let dir = dirs::config_dir().unwrap_or_default().join("niri");
        let mut files = vec![dir.join("config.kdl")];
        if let Ok(rd) = std::fs::read_dir(dir.join("dms")) {
            files.extend(rd.flatten().map(|e| e.path()));
        }
        files
            .iter()
            .filter_map(|f| std::fs::read_to_string(f).ok())
            .flat_map(|t| t.lines().map(str::to_owned).collect::<Vec<_>>())
            .filter(|l| !l.trim_start().starts_with("//"))
            .find_map(|l| l.trim().strip_prefix("gaps ").and_then(|v| v.trim().parse().ok()))
            .unwrap_or(16.0)
    })
}

fn query_niri_workspaces(sock: &Path) -> Option<NiriWorkspaces> {
    let out = niri_focused_output(sock)?.get("name")?.as_str()?.to_string();
    let list = niri_request(sock, "\"Workspaces\"")?.get("Workspaces")?.as_array()?.clone();
    let mut ws = NiriWorkspaces::default();
    for w in list.iter().filter(|w| w.get("output").and_then(|o| o.as_str()) == Some(out.as_str())) {
        let Some(id) = w.get("id").and_then(|v| v.as_u64()) else { continue };
        ws.all.push(id);
        if w.get("is_active").and_then(|v| v.as_bool()) == Some(true) {
            ws.active = id;
        }
        if w.get("active_window_id").is_some_and(|v| !v.is_null()) {
            ws.busy.push(id);
        }
    }
    Some(ws)
}

fn niri_socket() -> Option<PathBuf> {
    let p = PathBuf::from(std::env::var_os("NIRI_SOCKET")?);
    p.exists().then_some(p)
}

fn niri_request(sock: &Path, req: &str) -> Option<serde_json::Value> {
    let mut stream = UnixStream::connect(sock).ok()?;
    stream.set_read_timeout(Some(Duration::from_millis(200))).ok()?;
    stream.write_all(req.as_bytes()).ok()?;
    stream.write_all(b"\n").ok()?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).ok()?;
    let v: serde_json::Value = serde_json::from_str(&line).ok()?;
    v.get("Ok").cloned()
}

fn niri_focused_output(sock: &Path) -> Option<serde_json::Value> {
    niri_request(sock, "\"FocusedOutput\"")?.get("FocusedOutput").cloned()
}

fn query_niri_window(sock: &Path) -> Option<(bool, bool, Option<WindowRect>)> {
    let win = niri_request(sock, "\"FocusedWindow\"")?.get("FocusedWindow").cloned()?;
    if win.is_null() {
        return Some((false, false, None));
    }
    let logical = niri_focused_output(sock).and_then(|o| o.get("logical").cloned());
    let num = |v: Option<&serde_json::Value>, i: usize| {
        v.and_then(|a| a.get(i)).and_then(|x| x.as_f64()).unwrap_or(0.0) as f32
    };
    let layout = win.get("layout");
    let wsize = layout.and_then(|l| l.get("window_size"));
    let tsize = layout.and_then(|l| l.get("tile_size"));
    let (ox, oy, ow, oh) = match &logical {
        Some(l) => (
            l["x"].as_f64().unwrap_or(0.0) as f32,
            l["y"].as_f64().unwrap_or(0.0) as f32,
            l["width"].as_f64().unwrap_or(0.0) as f32,
            l["height"].as_f64().unwrap_or(0.0) as f32,
        ),
        None => (0.0, 0.0, 0.0, 0.0),
    };
    // niri has no fullscreen flag; a window covering the whole output counts
    let fullscreen = ow > 0.0 && num(wsize, 0) >= ow && num(wsize, 1) >= oh;
    let maximized = !fullscreen && ow > 0.0 && num(tsize, 0) >= ow * 0.95;

    let pos = layout.and_then(|l| l.get("tile_pos_in_workspace_view"));
    let off = layout.and_then(|l| l.get("window_offset_in_tile"));
    let rect = match pos {
        Some(p) if !p.is_null() => {
            let w = num(wsize, 0);
            let h = num(wsize, 1);
            (w > 0.0 && h > 0.0).then(|| WindowRect {
                x: ox + num(Some(p), 0) + num(off, 0),
                y: oy + num(Some(p), 1) + num(off, 1),
                w,
                h,
            })
        }
        _ => None,
    };
    Some((fullscreen, maximized, rect))
}

fn run_niri(tx: Sender<GovernorEvent>, sock: &Path) {
    let Ok(mut stream) = UnixStream::connect(sock) else {
        poll_forever(tx);
        return;
    };
    if stream.write_all(b"\"EventStream\"\n").is_err() {
        poll_forever(tx);
        return;
    }

    // power checks + window geometry (niri emits no event on resize/scroll)
    let tx_poll = tx.clone();
    let sock_poll = sock.to_path_buf();
    std::thread::Builder::new()
        .name("anima-niri-poll".into())
        .spawn(move || {
            let mut last = None;
            let mut ticks = 0u32;
            loop {
                // with exact positions the event stream keeps it current
                if !NIRI_EXACT.load(std::sync::atomic::Ordering::Relaxed) {
                    update_niri_strip(&sock_poll);
                }
                let state = query_niri_window(&sock_poll);
                if state != last {
                    last = state;
                    if let Some((fs, max, rect)) = state {
                        if tx_poll
                            .send(GovernorEvent::WindowState { fullscreen: fs, maximized: max, active_rect: rect })
                            .is_err()
                        {
                            return;
                        }
                    }
                }
                ticks = ticks.wrapping_add(1);
                if ticks % 10 == 0 && tx_poll.send(GovernorEvent::Power).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        })
        .ok();

    // cursor: niri has no IPC for it, approximate from evdev
    let bounds: super::pointer::Bounds = std::sync::Arc::new(std::sync::Mutex::new((
        niri_output_rect(sock).unwrap_or((0.0, 0.0, 1920.0, 1080.0)),
        niri_focused_output(sock)
            .and_then(|o| o.get("name").and_then(|n| n.as_str()).map(String::from))
            .unwrap_or_default(),
    )));
    super::pointer::spawn(tx.clone(), bounds.clone());

    log::info!("listening to niri events at {}", sock.display());
    let mut last_output = String::new();
    for line in BufReader::new(stream).lines() {
        let Ok(line) = line else { break };
        let relevant = ["WindowFocusChanged", "WindowOpenedOrChanged", "WindowClosed",
            "WorkspaceActivated", "WindowLayoutsChanged", "WindowsChanged"]
            .iter()
            .any(|k| line.starts_with(&format!("{{\"{k}\"")));
        if !relevant {
            continue;
        }
        // while scrolling niri sends one of these per frame: read the
        // position straight from it instead of asking niri again
        if line.starts_with("{\"WindowLayoutsChanged\"") && niri_layouts_changed(&line) {
            continue;
        }
        update_niri_strip(sock);
        if let Some(name) = niri_focused_output(sock)
            .and_then(|o| o.get("name").and_then(|n| n.as_str()).map(String::from))
        {
            if name != last_output {
                last_output = name.clone();
                if let Some(r) = niri_output_rect(sock) {
                    *bounds.lock().unwrap() = (r, name.clone());
                }
                if tx.send(GovernorEvent::FocusedMonitor(name)).is_err() {
                    return;
                }
            }
        }
        if let Some((fs, max, rect)) = query_niri_window(sock) {
            if tx
                .send(GovernorEvent::WindowState { fullscreen: fs, maximized: max, active_rect: rect })
                .is_err()
            {
                return;
            }
        }
    }
    poll_forever(tx);
}

fn niri_output_rect(sock: &Path) -> Option<(f32, f32, f32, f32)> {
    let l = niri_focused_output(sock)?.get("logical")?.clone();
    let f = |k: &str| l.get(k).and_then(|v| v.as_f64()).map(|v| v as f32);
    Some((f("x")?, f("y")?, f("width")?, f("height")?))
}
