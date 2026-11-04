//! Anima — a lightweight, native live wallpaper engine for Linux combining
//! Anima Engine and Wallpaper Engine features.
//!
//! The crate is split into a shared library and two binaries:
//!
//! * `animad` — the background daemon that draws wallpapers on the Wayland
//!   background layer using EGL / OpenGL ES 3.0 and hardware video decoding.
//! * `anima`  — the command-line and interactive TUI client used to control a
//!   running daemon, browse Steam Workshop wallpapers, and tune user properties.

pub mod audio;
pub mod backend;
pub mod config;
pub mod ipc;
pub mod lyrics;
pub mod pkg;
pub mod render;
pub mod util;
pub mod wallpaper;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
