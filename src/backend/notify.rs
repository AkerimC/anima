//! Count desktop notifications (org.freedesktop.Notifications.Notify calls on
//! the session bus) so the characters can react to them.

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};

/// Bumped on every notification.
pub static NOTIFICATIONS: AtomicU32 = AtomicU32::new(0);

pub fn spawn() {
    std::thread::Builder::new()
        .name("anima-notify".into())
        .spawn(|| {
            let child = Command::new("dbus-monitor")
                .args(["--session", "interface='org.freedesktop.Notifications',member='Notify'"])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();
            let Ok(mut child) = child else {
                log::info!("notify: dbus-monitor not available");
                return;
            };
            let Some(out) = child.stdout.take() else { return };
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                if line.starts_with("method call") && line.contains("member=Notify") {
                    log::debug!("notify: notification");
                    NOTIFICATIONS.fetch_add(1, Ordering::Relaxed);
                }
            }
        })
        .ok();
}
