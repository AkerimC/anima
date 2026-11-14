use std::io::{self, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use anima::ipc::{self, Request};
use anima::pkg::PackageFs;
use anima::util;
use anima::wallpaper::{self, Wallpaper};

#[derive(Parser, Debug)]
#[command(
    name = "anima",
    version,
    about = "Control the Anima + Wallpaper Engine live wallpaper daemon"
)]
struct Cli {
    /// Override the control socket path.
    #[arg(long, global = true)]
    socket: Option<PathBuf>,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Set a wallpaper (file, folder, name, or Steam Workshop ID) on a monitor.
    Set {
        path: String,
        /// Monitor name (e.g. eDP-1, DP-1, or `all`). Defaults to the focused monitor.
        #[arg(long, short)]
        monitor: Option<String>,
    },
    /// Advance to the next wallpaper in the playlist / wallpaper directory.
    Next {
        #[arg(long, short)]
        monitor: Option<String>,
    },
    /// Return to the previous wallpaper in the playlist / wallpaper directory.
    Prev {
        #[arg(long, short)]
        monitor: Option<String>,
    },
    /// Freeze animated wallpapers.
    Pause {
        #[arg(long, short)]
        monitor: Option<String>,
    },
    /// Resume animations.
    Resume {
        #[arg(long, short)]
        monitor: Option<String>,
    },
    /// Toggle pause.
    Toggle {
        #[arg(long, short)]
        monitor: Option<String>,
    },
    /// Adjust video/wallpaper audio volume (0.0..1.0) or mute state.
    Volume {
        /// Volume level (0.0..1.0 or 0..100).
        level: Option<f32>,
        #[arg(long)]
        mute: Option<bool>,
        #[arg(long, short)]
        monitor: Option<String>,
    },
    /// Inspect, modify, or reset Wallpaper Engine / Anima user properties at runtime.
    Prop {
        /// Property key (e.g. `schemecolor`, `speed`, `parallax`, `clock`, `bars`, `fit`).
        /// Omit to list all properties for the active wallpaper.
        key: Option<String>,
        /// New value to assign.
        value: Option<String>,
        /// Reset `key` (or all properties if `key` is omitted) back to defaults.
        #[arg(long, short)]
        reset: bool,
        #[arg(long, short)]
        monitor: Option<String>,
    },
    /// Launch the desktop GUI (`anima-gui`), focused on the wallpapers inspector.
    Gui,
    /// Configure automatic slideshow / playlist timer and shuffle mode.
    Playlist {
        /// Rotation interval in seconds (0 disables automatic timer).
        #[arg(long, short)]
        interval: Option<u64>,
        /// Enable or disable random shuffle.
        #[arg(long, short)]
        shuffle: Option<bool>,
        /// Optional explicit wallpaper paths/IDs for the playlist.
        items: Vec<String>,
    },
    /// Spawn, move, or customize the Anima Engine 2D desktop character / companion in a screen corner.
    Char {
        /// Preset (`reze`, `neko`, `cyber`, `spirit`), sprite file (`.png`, `.gif`, `.webp`), or `off`/`hide`/`on`.
        source: Option<String>,
        /// Screen or window corner (`bottom-right`, `bottom-left`, `top-right`, `top-left`, `bottom-center`).
        #[arg(long, short)]
        corner: Option<String>,
        /// Placement mode:
        /// `wallpaper` (behind windows),
        /// `front` / `screen-front` (in front of windows at screen corner),
        /// `inside` / `window-inside` (inside active window's corner),
        /// `outside` / `window-outside` (outside active window's corner/border).
        #[arg(long, short)]
        placement: Option<String>,
        /// Character scale (`0.2 .. 4.0`, default `1.0`).
        #[arg(long, short)]
        scale: Option<f32>,
        /// Horizontal pixel offset from corner.
        #[arg(long)]
        x: Option<f32>,
        /// Vertical pixel offset from corner.
        #[arg(long)]
        y: Option<f32>,
        /// Opacity (`0.0 .. 1.0`).
        #[arg(long)]
        opacity: Option<f32>,
        /// Mirror character horizontally (`true`/`false`).
        #[arg(long)]
        flip: Option<bool>,
        /// Track mouse cursor with eyes/head (`true`/`false`).
        #[arg(long)]
        mouse: Option<bool>,
        /// Bounce & lip-sync to desktop audio (`true`/`false`).
        #[arg(long)]
        audio: Option<bool>,
        /// Keep above wallpaper on the bottom desktop overlay layer (`true`/`false`).
        #[arg(long)]
        overlay: Option<bool>,
    },
    /// Discover and search installed Steam Workshop Wallpaper Engine items (AppID 431960).
    Workshop {
        /// Optional search filter by title, tag, or Workshop ID.
        query: Option<String>,
    },
    /// Unpack a Wallpaper Engine `.pkg` (`scene.pkg` / `gifscene.pkg`) archive or convert a `.tex` file to `.png`.
    Unpack {
        /// Path to `.pkg`, `.tex`, or wallpaper directory.
        input: PathBuf,
        /// Output directory (for `.pkg`) or output `.png` file (for `.tex`).
        output: Option<PathBuf>,
    },
    /// Interactive terminal UI browser & live controller.
    Tui,
    /// Reload the configuration from disk.
    Reload,
    /// Make the walking characters race each other.
    Race,
    /// Tell the characters the lock screen is up (on) or gone (off); the
    /// DMS lock screen does this to show them.
    Lockscreen {
        state: String,
    },
    /// Start a game now: contest, cursor, long, domino, tug, tag, samba, volley, carrot, gift, blind, fishing.
    Game {
        name: String,
    },
    /// Set the weather by hand: rain, snow, night, bedtime (everyone to bed now), clear, or auto.
    Weather {
        name: String,
    },
    /// Show daemon and monitor status.
    Status {
        #[arg(long)]
        json: bool,
        #[arg(long, short)]
        monitor: Option<String>,
    },
    /// List connected outputs.
    Monitors,
    /// List wallpapers found in the wallpaper directory (or a given folder).
    List {
        #[arg(default_value = None)]
        dir: Option<PathBuf>,
    },
    /// Check whether the daemon is running.
    Ping,
    /// Ask the daemon to exit.
    Quit,
}

fn main() {
    if let Err(err) = run() {
        eprintln!("anima: {err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let socket = cli.socket.clone().unwrap_or_else(util::socket_path);

    let json = matches!(&cli.command, Cmd::Status { json: true, .. });
    let is_status = matches!(&cli.command, Cmd::Status { .. });

    match &cli.command {
        Cmd::List { dir } => {
            let dir = dir.clone().unwrap_or_else(|| {
                let cfg = anima::config::Config::load(&util::config_path()).unwrap_or_default();
                cfg.wallpapers_dir()
            });
            let mut found = wallpaper::scan(&dir);
            if found.is_empty() && Path::new("examples/wallpapers").is_dir() {
                found = wallpaper::scan(Path::new("examples/wallpapers"));
            }
            if found.is_empty() {
                println!("no wallpapers in {}", dir.display());
            } else {
                for (name, path) in found {
                    let kind = Wallpaper::resolve(&path)
                        .map(|w| w.kind.as_str())
                        .unwrap_or("?");
                    println!("{name:<24} [{kind:<6}] {}", path.display());
                }
            }
            return Ok(());
        }
        Cmd::Workshop { query } => {
            let items = wallpaper::scan_workshop();
            let q = query.as_deref().unwrap_or("").to_ascii_lowercase();
            let mut shown = 0usize;
            for wp in &items {
                if !q.is_empty() {
                    let id_match = wp
                        .workshop_id
                        .as_deref()
                        .map(|id| id.contains(&q))
                        .unwrap_or(false);
                    let title_match = wp.title.to_ascii_lowercase().contains(&q);
                    let tag_match = wp
                        .tags
                        .iter()
                        .any(|t| t.to_ascii_lowercase().contains(&q));
                    if !id_match && !title_match && !tag_match {
                        continue;
                    }
                }
                let wid = wp.workshop_id.as_deref().unwrap_or("-");
                println!(
                    "{wid:<12} [{:<6}] {:<38} {}",
                    wp.kind.as_str(),
                    wp.title,
                    wp.root.display()
                );
                shown += 1;
            }
            if shown == 0 {
                println!(
                    "no Steam Workshop wallpapers matched (searched {:?})",
                    wallpaper::steam_workshop_dirs()
                );
            } else {
                println!("\n{shown} Workshop wallpaper(s) found. Apply any with: anima set <ID>");
            }
            return Ok(());
        }
        Cmd::Unpack { input, output } => {
            return run_unpack(input, output.as_deref());
        }
        Cmd::Tui => {
            return run_tui(&socket);
        }
        Cmd::Gui => {
            return launch_gui();
        }
        Cmd::Ping => {
            let response = ipc::request(&socket, &Request::Ping)?;
            println!("{}", response.message);
            return Ok(());
        }
        Cmd::Quit => {
            let response = ipc::request(&socket, &Request::Quit)?;
            println!("{}", response.message);
            return Ok(());
        }
        _ => {}
    }

    let request = match cli.command {
        Cmd::Set { path, monitor } => {
            let cwd = std::env::current_dir().ok();
            let expanded = util::expand_path(&path, cwd.as_deref());
            let final_path = if expanded.exists() {
                expanded.display().to_string()
            } else {
                path
            };
            Request::Set {
                monitor,
                path: final_path,
            }
        }
        Cmd::Next { monitor } => Request::Next { monitor },
        Cmd::Prev { monitor } => Request::Prev { monitor },
        Cmd::Pause { monitor } => Request::Pause { monitor },
        Cmd::Resume { monitor } => Request::Resume { monitor },
        Cmd::Toggle { monitor } => Request::Toggle { monitor },
        Cmd::Volume {
            level,
            mute,
            monitor,
        } => {
            let volume = level.map(|v| if v > 1.0 { (v / 100.0).clamp(0.0, 1.0) } else { v.clamp(0.0, 1.0) });
            Request::Volume {
                monitor,
                volume,
                mute,
            }
        }
        Cmd::Prop {
            reset: true,
            key,
            monitor,
            ..
        } => Request::ResetProperties { monitor, key },
        Cmd::Prop {
            key: Some(key),
            value: Some(value),
            monitor,
            reset: false,
        } => Request::SetProperty {
            monitor,
            key,
            value,
        },
        Cmd::Prop {
            key: None,
            monitor,
            reset: false,
            ..
        } => Request::ListProperties { monitor },
        Cmd::Prop {
            key: Some(key),
            value: None,
            monitor,
            reset: false,
        } => {
            let resp = ipc::request(&socket, &Request::ListProperties { monitor })?;
            if let Some(data) = &resp.data {
                if !print_single_property(data, &key) {
                    anyhow::bail!("property `{key}` not found on active wallpaper");
                }
            } else {
                println!("property {key}: {}", resp.message);
            }
            return Ok(());
        }
        Cmd::Playlist {
            interval,
            shuffle,
            items,
        } => Request::Playlist {
            interval_secs: interval,
            shuffle,
            items: if items.is_empty() { None } else { Some(items) },
        },
        Cmd::Char {
            source,
            corner,
            placement,
            scale,
            x,
            y,
            opacity,
            flip,
            mouse,
            audio,
            overlay,
        } => {
            let (enabled, clean_source) = match source.as_deref() {
                Some("off") | Some("hide") | Some("false") | Some("disable") => (Some(false), None),
                Some("on") | Some("show") | Some("true") | Some("enable") => (Some(true), None),
                Some(s) => (Some(true), Some(s.to_string())),
                None => (None, None),
            };
            Request::Character {
                enabled,
                source: clean_source,
                corner,
                placement,
                scale,
                offset_x: x,
                offset_y: y,
                opacity,
                flip_x: flip,
                look_at_mouse: mouse,
                audio_reactive: audio,
                always_on_top: overlay,
            }
        }
        Cmd::Reload => Request::Reload,
        Cmd::Race => Request::Race,
        Cmd::Lockscreen { state } => Request::LockScreen { on: state != "off" },
        Cmd::Game { name } => Request::Game { name },
        Cmd::Weather { name } => Request::Weather { name },
        Cmd::Status { monitor, .. } => Request::Status { monitor },
        Cmd::Monitors => Request::ListMonitors,
        Cmd::List { .. }
        | Cmd::Workshop { .. }
        | Cmd::Unpack { .. }
        | Cmd::Tui
        | Cmd::Gui
        | Cmd::Ping
        | Cmd::Quit => unreachable!(),
    };

    let response = ipc::request(&socket, &request)?;

    if json {
        if let Some(data) = &response.data {
            println!("{}", serde_json::to_string_pretty(data)?);
        } else {
            println!("{}", serde_json::to_string_pretty(&response)?);
        }
    } else if matches!(request, Request::ListProperties { .. }) {
        if let Some(data) = &response.data {
            print_properties_report(data, false);
        }
    } else {
        println!("{}", response.message);
        if is_status {
            if let Some(data) = &response.data {
                if let Some(compositor) = data.get("compositor").and_then(|v| v.as_str()) {
                    println!("  compositor: {compositor}");
                }
                if let Some(monitors) = data.get("monitors").and_then(|m| m.as_array()) {
                    for monitor in monitors {
                        let name = monitor.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                        let kind = monitor.get("kind").and_then(|v| v.as_str()).unwrap_or("-");
                        let size = monitor.get("size").and_then(|v| v.as_str()).unwrap_or("?");
                        let fps = monitor.get("fps").and_then(|v| v.as_u64()).unwrap_or(0);
                        let paused = monitor.get("paused").and_then(|v| v.as_bool()).unwrap_or(false);
                        let audio = monitor.get("uses_audio").and_then(|v| v.as_bool()).unwrap_or(false);
                        let wallpaper = monitor
                            .get("wallpaper")
                            .and_then(|v| v.as_str())
                            .unwrap_or("-");
                        println!(
                            "  {name}  {size}  {kind}  {fps}fps  {}{}{}",
                            if paused { "paused  " } else { "" },
                            if audio { "audio-reactive  " } else { "" },
                            wallpaper
                        );
                        if let Some(error) = monitor.get("error").and_then(|v| v.as_str()) {
                            println!("    error: {error}");
                        }
                    }
                }
            }
        }
    }

    if !response.ok {
        std::process::exit(1);
    }
    Ok(())
}

fn run_unpack(input: &Path, output: Option<&Path>) -> Result<()> {
    let ext = input
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    if ext == "tex" {
        let bytes = std::fs::read(input).with_context(|| format!("reading {}", input.display()))?;
        let decoded = anima::pkg::decode_tex(&bytes)?;
        let out = output
            .map(Path::to_path_buf)
            .unwrap_or_else(|| input.with_extension("png"));
        decoded
            .image
            .save(&out)
            .with_context(|| format!("saving {}", out.display()))?;
        println!(
            "decoded {} ({}x{}) -> {}",
            input.display(),
            decoded.image.width(),
            decoded.image.height(),
            out.display()
        );
        return Ok(());
    }

    let pkg = if input.is_dir() {
        PackageFs::open(input)?
    } else {
        PackageFs::from_pkg_file(input)?
    };
    let dest = output.map(Path::to_path_buf).unwrap_or_else(|| {
        if input.is_dir() {
            input.join("unpacked")
        } else {
            input
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join("unpacked")
        }
    });
    let count = pkg.unpack_to(&dest)?;
    println!("unpacked {count} file(s) into {}", dest.display());
    Ok(())
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

fn collect_monitor_values(
    monitor_entry: &serde_json::Value,
    schema: &[anima::wallpaper::WallpaperProperty],
) -> (
    std::collections::BTreeMap<String, String>,
    std::collections::HashSet<String>,
) {
    let mut values = std::collections::BTreeMap::new();
    let mut overridden = std::collections::HashSet::new();
    let ov = monitor_entry.get("overrides").and_then(|o| o.as_object());
    for p in schema {
        let mut cur = p.default_value.clone();
        if let Some(v) = ov.and_then(|m| m.get(&p.key)) {
            cur = json_val_to_string(v);
            if cur.trim() != p.default_value.trim() {
                overridden.insert(p.key.clone());
            }
        }
        values.insert(p.key.clone(), cur);
    }
    if let Some(m) = ov {
        for (k, v) in m {
            if !values.contains_key(k) {
                values.insert(k.clone(), json_val_to_string(v));
                overridden.insert(k.clone());
            }
        }
    }
    (values, overridden)
}

fn prop_type_hint(p: &anima::wallpaper::WallpaperProperty) -> String {
    match p.prop_type.as_str() {
        "slider" => match (p.min, p.max) {
            (Some(lo), Some(hi)) => format!("slider {lo}..{hi}"),
            _ => "slider".to_string(),
        },
        "combo" if !p.options.is_empty() => {
            let preview: Vec<String> = p
                .options
                .iter()
                .take(4)
                .map(|o| format!("{}={}", o.value, o.label))
                .collect();
            let suffix = if p.options.len() > 4 { ", …" } else { "" };
            format!("combo: {}{suffix}", preview.join(", "))
        }
        other => other.to_string(),
    }
}

fn print_properties_report(data: &serde_json::Value, show_indices: bool) -> Vec<String> {
    let mut keys = Vec::new();
    let Some(mons) = data.get("monitors").and_then(|m| m.as_array()) else {
        return keys;
    };
    for m in mons {
        let mname = m.get("monitor").and_then(|v| v.as_str()).unwrap_or("?");
        let kind = m.get("kind").and_then(|v| v.as_str()).unwrap_or("-");
        let wp = m.get("wallpaper").and_then(|v| v.as_str()).unwrap_or("-");
        println!("Monitor {mname} [{kind}] ({wp}):");

        let schema: Vec<anima::wallpaper::WallpaperProperty> = m
            .get("schema")
            .and_then(|s| serde_json::from_value(s.clone()).ok())
            .unwrap_or_default();
        if schema.is_empty() {
            println!("  (no configurable properties)");
            continue;
        }
        let (values, overridden) = collect_monitor_values(m, &schema);

        let mut in_playback = false;
        let mut printed_author_header = false;
        for p in &schema {
            let is_playback = p.order.unwrap_or(0) < 0;
            if is_playback && !in_playback {
                in_playback = true;
                println!("  -- Playback & Color Adjustments --");
            } else if !is_playback && in_playback {
                in_playback = false;
                if p.prop_type != "group" {
                    printed_author_header = true;
                    println!("  -- Wallpaper Properties --");
                }
            } else if !is_playback && !printed_author_header && p.prop_type != "group" {
                printed_author_header = true;
                println!("  -- Wallpaper Properties --");
            }

            if p.prop_type == "group" {
                printed_author_header = true;
                println!("  -- {} --", p.text);
                continue;
            }
            if p.prop_type == "text" {
                if !p.text.trim().is_empty() {
                    println!("    [note] {}", p.text);
                }
                continue;
            }

            let idx = keys.len();
            keys.push(p.key.clone());
            let cur = values
                .get(&p.key)
                .map(String::as_str)
                .unwrap_or(&p.default_value);
            let mark = if overridden.contains(&p.key) { "*" } else { " " };
            let hidden = p
                .condition
                .as_deref()
                .is_some_and(|c| !wallpaper::eval_property_condition(c, &values));
            let vis_tag = if hidden { " [hidden by condition]" } else { "" };
            let hint = prop_type_hint(p);
            let label = if p.text.is_empty() { "" } else { &p.text };
            if show_indices {
                println!(
                    " {mark}[p{idx:<2}] {:<18} = {:<16} [{hint}] {label}{vis_tag}",
                    p.key, cur
                );
            } else {
                println!(
                    " {mark}{:<20} = {:<16} [{hint}] {label}{vis_tag}",
                    p.key, cur
                );
            }
        }
    }
    keys
}

fn print_single_property(data: &serde_json::Value, key: &str) -> bool {
    let Some(mons) = data.get("monitors").and_then(|m| m.as_array()) else {
        return false;
    };
    let mut found = false;
    for m in mons {
        let mname = m.get("monitor").and_then(|v| v.as_str()).unwrap_or("?");
        let schema: Vec<anima::wallpaper::WallpaperProperty> = m
            .get("schema")
            .and_then(|s| serde_json::from_value(s.clone()).ok())
            .unwrap_or_default();
        let (values, overridden) = collect_monitor_values(m, &schema);
        if let Some(p) = schema.iter().find(|p| p.key.eq_ignore_ascii_case(key)) {
            found = true;
            let cur = values
                .get(&p.key)
                .map(String::as_str)
                .unwrap_or(&p.default_value);
            let modified = if overridden.contains(&p.key) {
                " (modified)"
            } else {
                " (default)"
            };
            println!("Monitor {mname}: {}", p.key);
            if !p.text.is_empty() {
                println!("  label    : {}", p.text);
            }
            println!("  type     : {}", p.prop_type);
            println!("  value    : {cur}{modified}");
            println!("  default  : {}", p.default_value);
            if let (Some(lo), Some(hi)) = (p.min, p.max) {
                let step = p
                    .step
                    .map(|s| format!(" step {s}"))
                    .unwrap_or_default();
                println!("  range    : {lo}..{hi}{step}");
            }
            if !p.options.is_empty() {
                let opts: Vec<String> = p
                    .options
                    .iter()
                    .map(|o| format!("{} ({})", o.value, o.label))
                    .collect();
                println!("  options  : {}", opts.join(", "));
            }
            if let Some(cond) = &p.condition {
                let vis = wallpaper::eval_property_condition(cond, &values);
                println!("  visible  : {vis} (condition: {cond})");
            }
        } else if let Some(val) = m
            .get("overrides")
            .and_then(|o| o.as_object())
            .and_then(|o| o.get(key))
        {
            found = true;
            println!("Monitor {mname}: {key}");
            println!("  value    : {} (override)", json_val_to_string(val));
        }
    }
    found
}

fn resolve_tui_prop_key(token: &str, prop_keys: &[String]) -> String {
    if let Some(num) = token.strip_prefix('p').or_else(|| token.strip_prefix('P')) {
        if !num.is_empty() && num.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(idx) = num.parse::<usize>() {
                if let Some(k) = prop_keys.get(idx) {
                    return k.clone();
                }
            }
        }
    }
    token.to_string()
}

fn launch_gui() -> Result<()> {
    let sibling = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("anima-gui")))
        .filter(|p| p.is_file());
    let bin = sibling
        .map(|p| p.into_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("anima-gui"));
    let mut cmd = std::process::Command::new(&bin);
    if std::env::var_os("ANIMA_GUI_TAB").is_none() {
        cmd.env("ANIMA_GUI_TAB", "wallpapers");
    }
    cmd.spawn()
        .with_context(|| "could not launch anima-gui; is it built or installed on PATH?")?;
    println!("launched anima-gui");
    Ok(())
}

fn run_tui(socket: &Path) -> Result<()> {
    let cfg = anima::config::Config::load(&util::config_path()).unwrap_or_default();
    let mut items: Vec<Wallpaper> = Vec::new();
    for path in wallpaper::scan(&cfg.wallpapers_dir()).into_values() {
        if let Ok(w) = Wallpaper::resolve(&path) {
            items.push(w);
        }
    }
    for path in wallpaper::scan(Path::new("examples/wallpapers")).into_values() {
        if let Ok(w) = Wallpaper::resolve(&path) {
            items.push(w);
        }
    }
    items.extend(wallpaper::scan_workshop());

    println!("==============================================================");
    println!("  ANIMA + WALLPAPER ENGINE — INTERACTIVE CONTROL CENTER");
    println!("==============================================================");
    if items.is_empty() {
        println!("  (No wallpapers discovered yet; pass a path with `s <path>`)");
    } else {
        for (i, wp) in items.iter().enumerate() {
            println!(
                "  [{i:>2}] {:<32} [{:<6}] {}",
                wp.title,
                wp.kind.as_str(),
                wp.entry.display()
            );
        }
    }
    println!("--------------------------------------------------------------");
    println!("  Commands: <number> apply | n next | p prev | space toggle");
    println!("            props | prop <k|pN> <v> | reset [k|pN]");
    println!("            vol <0..1> | status | gui | q quit");
    println!("--------------------------------------------------------------");

    let mut prop_keys: Vec<String> = Vec::new();
    let stdin = io::stdin();
    loop {
        print!("anima> ");
        io::stdout().flush()?;
        let mut line = String::new();
        if stdin.read_line(&mut line)? == 0 {
            break;
        }
        let cmd = line.trim();
        if cmd.is_empty() {
            continue;
        }
        if cmd == "q" || cmd == "quit" || cmd == "exit" {
            break;
        }
        if let Ok(idx) = cmd.parse::<usize>() {
            if let Some(wp) = items.get(idx) {
                let req = Request::Set {
                    monitor: None,
                    path: wp.root.display().to_string(),
                };
                match ipc::request(socket, &req) {
                    Ok(r) => println!("{}", r.message),
                    Err(e) => println!("error: {e}"),
                }
            } else {
                println!("index out of range (0..{})", items.len().saturating_sub(1));
            }
            continue;
        }
        let parts: Vec<&str> = cmd.split_whitespace().collect();
        match parts.as_slice() {
            ["props"] | ["pr"] => {
                match ipc::request(socket, &Request::ListProperties { monitor: None }) {
                    Ok(resp) => {
                        if let Some(data) = &resp.data {
                            prop_keys = print_properties_report(data, true);
                        } else {
                            println!("{}", resp.message);
                        }
                    }
                    Err(e) => println!("error: {e}"),
                }
                continue;
            }
            ["gui"] => {
                if let Err(e) = launch_gui() {
                    println!("error: {e}");
                }
                continue;
            }
            _ => {}
        }
        let req = match parts.as_slice() {
            ["n"] | ["next"] => Some(Request::Next { monitor: None }),
            ["p"] | ["prev"] => Some(Request::Prev { monitor: None }),
            ["space"] | ["toggle"] | ["t"] => Some(Request::Toggle { monitor: None }),
            ["status"] | ["st"] => Some(Request::Status { monitor: None }),
            ["vol", v] => v.parse::<f32>().ok().map(|lvl| Request::Volume {
                monitor: None,
                volume: Some(lvl),
                mute: Some(lvl <= 0.0),
            }),
            ["reset"] => Some(Request::ResetProperties {
                monitor: None,
                key: None,
            }),
            ["reset", k] => Some(Request::ResetProperties {
                monitor: None,
                key: Some(resolve_tui_prop_key(k, &prop_keys)),
            }),
            ["prop", k, rest @ ..] | ["set", k, rest @ ..] if !rest.is_empty() => {
                Some(Request::SetProperty {
                    monitor: None,
                    key: resolve_tui_prop_key(k, &prop_keys),
                    value: rest.join(" "),
                })
            }
            ["s", path] | ["set", path] => Some(Request::Set {
                monitor: None,
                path: (*path).to_string(),
            }),
            _ => {
                println!("unknown command: {cmd}");
                None
            }
        };
        if let Some(r) = req {
            match ipc::request(socket, &r) {
                Ok(resp) => println!("{}", resp.message),
                Err(err) => println!("error: {err}"),
            }
        }
    }
    Ok(())
}
