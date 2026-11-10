//! The weather outside and the time of day, for the characters to react
//! to: umbrellas in the rain, catching snowflakes, some going to sleep at
//! night. Looked up with curl from wttr.in every half hour; `anima weather`
//! sets it by hand.

use std::sync::Mutex;
use std::time::Duration;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Weather {
    pub rain: bool,
    pub snow: bool,
    pub night: bool,
}

// what the sky is doing, and what was asked for by hand (None: the real thing)
static NOW: Mutex<Weather> = Mutex::new(Weather { rain: false, snow: false, night: false });
static FORCED: Mutex<Option<Weather>> = Mutex::new(None);
// sunrise and sunset, minutes after midnight
static SUN: Mutex<(u32, u32)> = Mutex::new((7 * 60, 19 * 60));

/// "rain", "snow", "night", "clear" or "auto".
pub fn force(name: &str) -> bool {
    let w = match name {
        "rain" => Some(Weather { rain: true, ..Default::default() }),
        "snow" => Some(Weather { snow: true, ..Default::default() }),
        "night" => Some(Weather { night: true, ..Default::default() }),
        "clear" => Some(Weather::default()),
        "auto" => None,
        _ => return false,
    };
    *FORCED.lock().unwrap() = w;
    true
}

pub(super) fn now() -> Weather {
    if let Some(w) = *FORCED.lock().unwrap() {
        return w;
    }
    let mut w = *NOW.lock().unwrap();
    let (rise, set) = *SUN.lock().unwrap();
    let m = minutes_now();
    w.night = m < rise || m >= set + 150;
    w
}

fn minutes_now() -> u32 {
    let t = unsafe { libc::time(std::ptr::null_mut()) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&t, &mut tm) };
    (tm.tm_hour * 60 + tm.tm_min) as u32
}

pub(super) fn spawn() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let _ = std::thread::Builder::new().name("anima-weather".into()).spawn(|| loop {
            if let Some((w, sun)) = look_up() {
                log::debug!("weather: {w:?}, sun {sun:?}");
                *NOW.lock().unwrap() = w;
                *SUN.lock().unwrap() = sun;
                std::thread::sleep(Duration::from_secs(1800));
            } else {
                std::thread::sleep(Duration::from_secs(300));
            }
        });
    });
}

fn look_up() -> Option<(Weather, (u32, u32))> {
    let out = std::process::Command::new("curl").args(["-s", "-m", "20", "https://wttr.in/?format=j1"]).output().ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let cur = &v["current_condition"][0];
    let code: u32 = cur["weatherCode"].as_str()?.parse().ok()?;
    let desc = cur["weatherDesc"][0]["value"].as_str().unwrap_or("").to_ascii_lowercase();
    let snow = desc.contains("snow") || desc.contains("sleet") || desc.contains("blizzard") || matches!(code, 179 | 227 | 230 | 323..=338 | 368..=377 | 392 | 395);
    let rain = !snow && (desc.contains("rain") || desc.contains("drizzle") || desc.contains("shower") || desc.contains("thunder") || matches!(code, 176 | 200 | 263..=314 | 353..=359 | 386 | 389));
    let astro = &v["weather"][0]["astronomy"][0];
    let clock = |s: &str| -> Option<u32> {
        // "07:12 AM"
        let (hm, ap) = s.trim().split_once(' ')?;
        let (h, m) = hm.split_once(':')?;
        let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
        Some((h % 12 + if ap.eq_ignore_ascii_case("PM") { 12 } else { 0 }) * 60 + m)
    };
    let sun = (clock(astro["sunrise"].as_str()?)?, clock(astro["sunset"].as_str()?)?);
    Some((Weather { rain, snow, night: false }, sun))
}
