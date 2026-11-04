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
