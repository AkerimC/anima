//! Anima: pick the desktop characters, start/stop the daemon, and change
//! every number behind how they behave.

mod config;
mod daemon;
mod pages;
mod team;

use adw::prelude::*;
use gtk::{gio, glib};

const APP_ID: &str = "dev.anima.Anima";

fn main() -> glib::ExitCode {
    // plain lists and sliders: drawing them on the CPU saves ~80 MB and leaves
    // the GPU to the compositor. GSK_RENDERER from outside still wins.
    if std::env::var_os("GSK_RENDERER").is_none() {
        // SAFETY: before any thread exists
        unsafe { std::env::set_var("GSK_RENDERER", "cairo") };
    }
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::default())
        .build();
    app.connect_activate(|app| {
        if let Some(win) = app.active_window() {
            win.present();
            return;
        }
        pages::build_window(app).present();
    });
    app.run()
}
