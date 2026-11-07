//! niri behind [`Compositor`]. The implementation itself still lives in
//! governor.rs (`run_niri`, `STRIP`, `niri_gesture*`, ...) and is meant to
//! move here.

use std::path::PathBuf;

use calloop::channel::Sender;

use super::compositor::{Compositor, WindowState};
use super::governor::{self, GovernorEvent, Strip};

pub struct Niri {
    sock: PathBuf,
}

impl Niri {
    pub fn detect() -> Option<Self> {
        governor::niri_socket().map(|sock| Self { sock })
    }
}

impl Compositor for Niri {
    fn name(&self) -> &'static str {
        "niri"
    }

    fn run(&self, tx: Sender<GovernorEvent>) {
        governor::run_niri(tx, &self.sock);
    }

    fn window_state(&self) -> Option<WindowState> {
        governor::query_niri_window(&self.sock)
    }

    fn strip(&self) -> Option<Strip> {
        *governor::STRIP.lock().unwrap()
    }

    fn cursor(&self) -> Option<Option<(f32, f32)>> {
        governor::niri_cursor()
    }

    fn swipe(&self, active: bool) {
        governor::niri_gesture(active);
    }

    fn swipe_scroll(&self, dx: f32) {
        governor::niri_gesture_scroll(dx);
    }
}
