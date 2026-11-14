//! Anima: pick the desktop characters, start/stop the daemon, and change
//! every number behind how they behave.

mod config;
mod daemon;
mod pages;
mod park;
mod roster;
mod team;

use adw::prelude::*;
use gtk::{gio, glib};

const APP_ID: &str = "dev.anima.Anima";

fn main() -> glib::ExitCode {
    // plain lists and sliders: drawing them on the CPU saves ~80 MB and leaves
    // the GPU to the compositor. GSK_RENDERER from outside still wins.
    // a forced GTK theme (GTK_THEME=Breeze-Dark) has none of libadwaita's
    // styles: rows lose their spacing and look broken. This window uses its own.
    // SAFETY: before any thread exists
    unsafe { std::env::remove_var("GTK_THEME") };
    if std::env::var_os("GSK_RENDERER").is_none() {
        // SAFETY: before any thread exists
        unsafe { std::env::set_var("GSK_RENDERER", "cairo") };
    }
    // `anima-gui --park`: the park window, on its own
    if std::env::args().any(|a| a == "--park") {
        return park::run();
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
