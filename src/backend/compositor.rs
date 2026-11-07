//! Everything compositor-specific in animad sits behind [`Compositor`]. One
//! implementation is picked at startup (niri, Hyprland, or a polling
//! fallback); drawing, the characters and the input devices are shared.
//! A new compositor is one more implementation; docs/compositors.md says
//! what each method is for.

use std::sync::OnceLock;

use calloop::channel::Sender;

use super::governor::{GovernorEvent, Strip};
use crate::render::character::WindowRect;

/// Fullscreen, maximized, and the active window's rectangle.
pub type WindowState = (bool, bool, Option<WindowRect>);

/// The row of workspaces the characters walk along, for window mode.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Row {
    pub count: usize,
    /// index of the active workspace in the row
    pub active: usize,
    /// distance between neighbouring workspaces, logical pixels
    pub step: f32,
    /// a swipe or workspace animation is moving the row right now
    pub sliding: bool,
}

pub trait Compositor: Send + Sync {
    fn name(&self) -> &'static str;

    /// Watch the compositor and report to `tx` until the channel closes.
    /// Runs on the governor thread; also starts pointer tracking
    /// (`pointer::spawn`) where the compositor needs it.
    fn run(&self, tx: Sender<GovernorEvent>);

    /// Window state, asked directly.
    fn window_state(&self) -> Option<WindowState>;

    /// Where the characters' strip of the focused output is drawn right now
    /// (logical pixels). None: the strip is just the screen.
    fn strip(&self) -> Option<Strip> {
        None
    }

    /// Ask the compositor where the cursor is: None if it can't be asked,
    /// Some(None) if it doesn't know right now.
    fn cursor(&self) -> Option<Option<(f32, f32)>> {
        None
    }

    /// Three/four touchpad fingers went down (true) or up.
    fn swipe(&self, _active: bool) {}

    /// Horizontal travel of that swipe since the last call, pixels after
    /// libinput's acceleration.
    fn swipe_scroll(&self, _dx: f32) {}

    // Window mode (backend/charwin.rs): one window per workspace of the row,
    // which the compositor slides with its workspace.

    /// Whether character windows can be placed on workspaces.
    fn can_place_windows(&self) -> bool {
        false
    }

    /// Window mode switched on or off; while on, `strip` stays on the
    /// active workspace instead of following the slide.
    fn set_window_mode(&self, _on: bool) {}

    fn row(&self) -> Option<Row> {
        None
    }

    /// Keep character window `slot` on the row's `slot`th workspace, at its
    /// top-left.
    fn place_workspace_window(&self, _slot: usize) {}
}

/// The compositor animad runs on, detected once.
pub fn current() -> &'static dyn Compositor {
    static CURRENT: OnceLock<Box<dyn Compositor>> = OnceLock::new();
    CURRENT
        .get_or_init(|| {
            let c: Box<dyn Compositor> = if let Some(n) = super::niri::Niri::detect() {
                Box::new(n)
            } else if let Some(h) = super::hyprland::Hyprland::detect() {
                Box::new(h)
            } else {
                Box::new(Fallback)
            };
            log::info!("compositor: {}", c.name());
            c
        })
        .as_ref()
}

/// No IPC: window state every couple of seconds (fullscreen on sway).
struct Fallback;

impl Compositor for Fallback {
    fn name(&self) -> &'static str {
        "generic"
    }

    fn run(&self, tx: Sender<GovernorEvent>) {
        super::governor::poll_forever(tx);
    }

    fn window_state(&self) -> Option<WindowState> {
        std::env::var_os("SWAYSOCK")?;
        let output = std::process::Command::new("swaymsg").args(["-t", "get_tree"]).output().ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let fs = text.contains("\"focused\": true") && text.contains("\"fullscreen_mode\": 1");
        Some((fs, false, None))
    }
}
