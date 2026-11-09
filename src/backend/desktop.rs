//! What's going on at the desktop, for the characters to react to: has the
//! user gone quiet, how busy is the processor, did a window come or go.

use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::time::Duration;

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry::WlRegistry, wl_seat::WlSeat};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1::{self, ExtIdleNotificationV1},
    ext_idle_notifier_v1::ExtIdleNotifierV1,
};

/// No keyboard or pointer input for `IDLE_AFTER_MS`.
pub static IDLE: AtomicBool = AtomicBool::new(false);
pub const IDLE_AFTER_MS: u32 = 20_000;

/// Processor load, thousandths.
pub static CPU: AtomicU32 = AtomicU32::new(0);

/// Bumped when a new window opens / one closes.
pub static OPENED: AtomicU32 = AtomicU32::new(0);
pub static CLOSED: AtomicU32 = AtomicU32::new(0);
pub static LAST_OPENED: Mutex<Option<u64>> = Mutex::new(None);

static KNOWN: Mutex<Option<HashSet<u64>>> = Mutex::new(None);

pub fn spawn() {
    let _ = std::thread::Builder::new().name("anima-idle".into()).spawn(|| {
        if watch_idle().is_none() {
            log::info!("desktop: no idle notifications from the compositor");
        }
    });
    let _ = std::thread::Builder::new().name("anima-cpu".into()).spawn(watch_cpu);
}

struct Idle;

impl Dispatch<WlRegistry, GlobalListContents> for Idle {
    fn event(_: &mut Self, _: &WlRegistry, _: <WlRegistry as wayland_client::Proxy>::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ExtIdleNotificationV1, ()> for Idle {
    fn event(_: &mut Self, _: &ExtIdleNotificationV1, event: ext_idle_notification_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            ext_idle_notification_v1::Event::Idled => IDLE.store(true, Relaxed),
            ext_idle_notification_v1::Event::Resumed => IDLE.store(false, Relaxed),
            _ => {}
        }
    }
}

delegate_noop!(Idle: ignore WlSeat);
delegate_noop!(Idle: ignore ExtIdleNotifierV1);

fn watch_idle() -> Option<()> {
    let conn = Connection::connect_to_env().ok()?;
    let (globals, mut queue) = registry_queue_init::<Idle>(&conn).ok()?;
    let qh = queue.handle();
    let seat: WlSeat = globals.bind(&qh, 1..=1, ()).ok()?;
    let notifier: ExtIdleNotifierV1 = globals.bind(&qh, 1..=1, ()).ok()?;
    let _note = notifier.get_idle_notification(IDLE_AFTER_MS, &seat, &qh, ());
    loop {
        queue.blocking_dispatch(&mut Idle).ok()?;
    }
}

/// (busy, total) jiffies from the first line of /proc/stat.
fn parse_stat(text: &str) -> Option<(u64, u64)> {
    let mut f = text.lines().next()?.split_whitespace();
    if f.next()? != "cpu" {
        return None;
    }
    let v: Vec<u64> = f.filter_map(|x| x.parse().ok()).collect();
    let total: u64 = v.iter().take(8).sum();
    let idle = v.get(3)? + v.get(4).copied().unwrap_or(0);
    Some((total - idle, total))
}

fn watch_cpu() {
    let read = || std::fs::read_to_string("/proc/stat").ok().as_deref().and_then(parse_stat);
    let mut last = read();
    loop {
        std::thread::sleep(Duration::from_secs(2));
        let now = read();
        if let (Some((b0, t0)), Some((b1, t1))) = (last, now) {
            if t1 > t0 {
                CPU.store(((b1 - b0) * 1000 / (t1 - t0)) as u32, Relaxed);
            }
        }
        last = now;
    }
}

/// One line of niri's event stream.
pub fn window_event(line: &str) {
    let opened = line.starts_with("{\"WindowOpenedOrChanged\"");
    let closed = line.starts_with("{\"WindowClosed\"");
    let listed = line.starts_with("{\"WindowsChanged\"");
    if !(opened || closed || listed) {
        return;
    }
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { return };
    let mut known = KNOWN.lock().unwrap();
    if listed {
        let ids = v["WindowsChanged"]["windows"].as_array().map(|w| w.iter().filter_map(|x| x["id"].as_u64()).collect());
        *known = Some(ids.unwrap_or_default());
        return;
    }
    let Some(set) = known.as_mut() else { return };
    if opened {
        if let Some(id) = v["WindowOpenedOrChanged"]["window"]["id"].as_u64() {
            if set.insert(id) {
                *LAST_OPENED.lock().unwrap() = Some(id);
                OPENED.fetch_add(1, Relaxed);
            }
        }
    } else if let Some(id) = v["WindowClosed"]["id"].as_u64() {
        if set.remove(&id) {
            CLOSED.fetch_add(1, Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_busy_time() {
        let (busy, total) = parse_stat("cpu  100 0 50 800 50 0 0 0 0 0\ncpu0 1 2 3").unwrap();
        assert_eq!((busy, total), (150, 1000));
    }

    #[test]
    fn only_new_windows_count() {
        let (o, c) = (OPENED.load(Relaxed), CLOSED.load(Relaxed));
        window_event(r#"{"WindowsChanged":{"windows":[{"id":1}]}}"#);
        window_event(r#"{"WindowOpenedOrChanged":{"window":{"id":1}}}"#);
        assert_eq!(OPENED.load(Relaxed), o, "a retitled window isn't new");
        window_event(r#"{"WindowOpenedOrChanged":{"window":{"id":2}}}"#);
        assert_eq!(OPENED.load(Relaxed), o + 1);
        window_event(r#"{"WindowClosed":{"id":2}}"#);
        assert_eq!(CLOSED.load(Relaxed), c + 1);
    }
}
