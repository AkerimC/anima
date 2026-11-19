//! Real-time MPRIS2 media session tracker, album artwork decoder, and palette extractor.
//!
//! Feeds Wallpaper Engine SceneScript media hooks (`mediaStatusChanged`,
//! `mediaPlaybackChanged`, `mediaPropertiesChanged`, `mediaTimelineChanged`,
//! `mediaThumbnailChanged`) as well as the `$mediaThumbnail` and
//! `$mediaPreviousThumbnail` system textures.
//!
//! Sleeps on a `Condvar` at 0% CPU whenever no active output uses media features.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

const ART_CACHE_CAP: usize = 8;
const MAX_ART_DIM: u32 = 512;
const MAX_LOCAL_ART_BYTES: u64 = 16 * 1024 * 1024;
const MAX_HTTP_ART_BYTES: usize = 8 * 1024 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Playback state mapped directly to Wallpaper Engine `MediaPlaybackEvent` constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MediaPlaybackState {
    #[default]
    Stopped = 0,
    Playing = 1,
    Paused = 2,
}

impl MediaPlaybackState {
    /// Returns the numeric `MediaPlaybackEvent.PLAYBACK_*` code (`0 / 1 / 2`).
    pub fn as_we_code(self) -> u8 {
        match self {
            Self::Stopped => 0,
            Self::Playing => 1,
            Self::Paused => 2,
        }
    }

    fn from_mpris(status: &str) -> Self {
        if status.eq_ignore_ascii_case("Playing") {
            Self::Playing
        } else if status.eq_ignore_ascii_case("Paused") {
            Self::Paused
        } else {
            Self::Stopped
        }
    }
}

/// Normalized `0.0..=1.0` RGB palette extracted from the current album art.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MediaPalette {
    pub primary: [f32; 3],
    pub secondary: [f32; 3],
    pub tertiary: [f32; 3],
    pub text: [f32; 3],
    pub high_contrast: [f32; 3],
}

impl Default for MediaPalette {
    fn default() -> Self {
        Self {
            primary: [0.2, 0.2, 0.2],
            secondary: [0.35, 0.35, 0.35],
            tertiary: [0.5, 0.5, 0.5],
            text: [1.0, 1.0, 1.0],
            high_contrast: [1.0, 1.0, 1.0],
        }
    }
}

/// Decoded album artwork buffer ready for OpenGL upload (top-down RGBA8).
#[derive(Debug)]
pub struct MediaArt {
    pub rev: u64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// Frame-coherent snapshot of the active MPRIS2 session and album cover.
#[derive(Debug, Clone)]
pub struct MediaSnapshot {
    pub enabled: bool,
    pub state: MediaPlaybackState,
    pub title: String,
    pub artist: String,
    pub album_title: String,
    pub album_artist: String,
    pub sub_title: String,
    pub genres: String,
    pub content_type: String,
    pub position: f32,
    pub duration: f32,
    pub has_thumbnail: bool,
    pub palette: MediaPalette,
    pub art: Option<Arc<MediaArt>>,
    pub prev_art: Option<Arc<MediaArt>>,
    pub props_rev: u64,
    pub thumb_rev: u64,
}

struct SharedMedia {
    enabled: bool,
    quit: bool,
    state: MediaPlaybackState,
    title: String,
    artist: String,
    album_title: String,
    album_artist: String,
    sub_title: String,
    genres: String,
    content_type: String,
    base_pos_sec: f32,
    base_instant: Instant,
    duration_sec: f32,
    art_url: String,
    has_thumbnail: bool,
    palette: MediaPalette,
    art: Option<Arc<MediaArt>>,
    prev_art: Option<Arc<MediaArt>>,
    props_rev: u64,
    thumb_rev: u64,
}

impl Default for SharedMedia {
    fn default() -> Self {
        Self {
            enabled: false,
            quit: false,
            state: MediaPlaybackState::Stopped,
            title: String::new(),
            artist: String::new(),
            album_title: String::new(),
            album_artist: String::new(),
            sub_title: String::new(),
            genres: String::new(),
            content_type: "music".to_string(),
            base_pos_sec: 0.0,
            base_instant: Instant::now(),
            duration_sec: 0.0,
            art_url: String::new(),
            has_thumbnail: false,
            palette: MediaPalette::default(),
            art: None,
            prev_art: None,
            props_rev: 0,
            thumb_rev: 0,
        }
    }
}

/// Asynchronous MPRIS2 media tracker backed by a dedicated background thread.
#[derive(Clone)]
pub struct MediaTracker {
    inner: Arc<(Mutex<SharedMedia>, Condvar)>,
}

impl MediaTracker {
    /// Spawns the `"anima-media"` background worker thread.
    pub fn spawn(initially_enabled: bool) -> Self {
        let mut state = SharedMedia::default();
        state.enabled = initially_enabled;
        let inner = Arc::new((Mutex::new(state), Condvar::new()));
        let worker_inner = Arc::clone(&inner);
        std::thread::Builder::new()
            .name("anima-media".into())
            .spawn(move || media_worker(worker_inner))
            .ok();
        Self { inner }
    }

    /// Enables or disables MPRIS polling; disabled trackers sleep at 0% CPU.
    pub fn set_enabled(&self, enabled: bool) {
        let (lock, cvar) = &*self.inner;
        if let Ok(mut g) = lock.lock() {
            if g.enabled != enabled {
                g.enabled = enabled;
                cvar.notify_all();
            }
        }
    }

    /// Signals the background worker thread to exit promptly.
    pub fn stop(&self) {
        let (lock, cvar) = &*self.inner;
        if let Ok(mut g) = lock.lock() {
            g.quit = true;
            g.enabled = false;
            cvar.notify_all();
        }
    }

    /// Returns `None` when disabled or a snapshot with smoothly interpolated position.
    pub fn snapshot(&self) -> Option<MediaSnapshot> {
        let (lock, _) = &*self.inner;
        let Ok(g) = lock.lock() else {
            return None;
        };
        if !g.enabled {
            return None;
        }
        let mut position = match g.state {
            MediaPlaybackState::Playing => g.base_pos_sec + g.base_instant.elapsed().as_secs_f32(),
            MediaPlaybackState::Paused => g.base_pos_sec,
            MediaPlaybackState::Stopped => 0.0,
        };
        if position < 0.0 {
            position = 0.0;
        }
        if g.duration_sec > 0.0 {
            position = position.min(g.duration_sec);
        }
        Some(MediaSnapshot {
            enabled: g.enabled,
            state: g.state,
            title: g.title.clone(),
            artist: g.artist.clone(),
            album_title: g.album_title.clone(),
            album_artist: g.album_artist.clone(),
            sub_title: g.sub_title.clone(),
            genres: g.genres.clone(),
            content_type: g.content_type.clone(),
            position,
            duration: g.duration_sec,
            has_thumbnail: g.has_thumbnail,
            palette: g.palette,
            art: g.art.clone(),
            prev_art: g.prev_art.clone(),
            props_rev: g.props_rev,
            thumb_rev: g.thumb_rev,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
struct MprisSample {
    state: MediaPlaybackState,
    title: String,
    artist: String,
    album_title: String,
    album_artist: String,
    genres: String,
    art_url: String,
    position_sec: f32,
    duration_sec: f32,
}

type CachedArtEntry = (String, Option<(Arc<MediaArt>, MediaPalette)>);

fn media_worker(inner: Arc<(Mutex<SharedMedia>, Condvar)>) {
    let (lock, cvar) = &*inner;
    let mut art_cache: Vec<CachedArtEntry> = Vec::with_capacity(ART_CACHE_CAP);
    let mut next_art_rev: u64 = 1;
    let mut last_track_sig = String::new();
    let mut last_reported_pos: f32 = -1.0;

    loop {
        {
            let mut g = match lock.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            while !g.enabled && !g.quit {
                g = match cvar.wait(g) {
                    Ok(g) => g,
                    Err(_) => return,
                };
            }
            if g.quit {
                return;
            }
        }

        let sample = poll_mpris();
        let art_url = sample.as_ref().map(|s| s.art_url.trim().to_string()).unwrap_or_default();
        let cached_art = if art_url.is_empty() {
            None
        } else {
            load_or_cached_art(&art_url, &mut art_cache, &mut next_art_rev)
        };

        let now = Instant::now();
        {
            let Ok(mut g) = lock.lock() else {
                return;
            };
            if g.quit {
                return;
            }
            if !g.enabled {
                continue;
            }

            if let Some(sample) = sample {
                let props_changed = g.title != sample.title
                    || g.artist != sample.artist
                    || g.album_title != sample.album_title
                    || g.album_artist != sample.album_artist
                    || g.genres != sample.genres;
                if props_changed {
                    g.title = sample.title;
                    g.artist = sample.artist;
                    g.album_title = sample.album_title;
                    g.album_artist = sample.album_artist;
                    g.genres = sample.genres;
                    g.props_rev = g.props_rev.saturating_add(1);
                }

                let track_sig = format!("{}\n{}\n{}", g.title, g.artist, sample.art_url);
                let track_changed = track_sig != last_track_sig;
                let state_changed = g.state != sample.state;
                let expected_pos = match g.state {
                    MediaPlaybackState::Playing => {
                        g.base_pos_sec + now.saturating_duration_since(g.base_instant).as_secs_f32()
                    }
                    MediaPlaybackState::Paused => g.base_pos_sec,
                    MediaPlaybackState::Stopped => 0.0,
                };

                let pos_jumped = (sample.position_sec - expected_pos).abs() > 1.2
                    || (last_reported_pos >= 0.0 && (sample.position_sec - last_reported_pos).abs() > 2.5);
                if track_changed || state_changed || pos_jumped || sample.state != MediaPlaybackState::Playing {
                    g.base_pos_sec = sample.position_sec.max(0.0);
                    g.base_instant = now;
                }
                last_reported_pos = sample.position_sec;
                last_track_sig = track_sig;

                g.state = sample.state;
                g.duration_sec = sample.duration_sec.max(0.0);

                if g.art_url != art_url || (g.has_thumbnail != cached_art.is_some()) {
                    let prev = g.art.clone();
                    match cached_art {
                        Some((new_art, pal)) => {
                            let same_rev = prev.as_ref().is_some_and(|p| p.rev == new_art.rev);
                            if !same_rev {
                                if let Some(p) = prev {
                                    g.prev_art = Some(p);
                                }
                                g.art = Some(new_art);
                                g.palette = pal;
                                g.has_thumbnail = true;
                                g.thumb_rev = g.thumb_rev.saturating_add(1);
                            }
                        }
                        None => {
                            if let Some(p) = prev {
                                g.prev_art = Some(p);
                            }
                            if g.has_thumbnail || !g.art_url.is_empty() {
                                g.art = None;
                                g.palette = MediaPalette::default();
                                g.has_thumbnail = false;
                                g.thumb_rev = g.thumb_rev.saturating_add(1);
                            }
                        }
                    }
                    g.art_url = art_url;
                }
            } else {
                let had_props = !g.title.is_empty()
                    || !g.artist.is_empty()
                    || !g.album_title.is_empty()
                    || !g.album_artist.is_empty()
                    || !g.genres.is_empty();
                if had_props {
                    g.title.clear();
                    g.artist.clear();
                    g.album_title.clear();
                    g.album_artist.clear();
                    g.genres.clear();
                    g.props_rev = g.props_rev.saturating_add(1);
                }
                if g.has_thumbnail || !g.art_url.is_empty() {
                    if let Some(p) = g.art.clone() {
                        g.prev_art = Some(p);
                    }
                    g.art = None;
                    g.palette = MediaPalette::default();
                    g.has_thumbnail = false;
                    g.art_url.clear();
                    g.thumb_rev = g.thumb_rev.saturating_add(1);
                }
                g.state = MediaPlaybackState::Stopped;
                g.base_pos_sec = 0.0;
                g.base_instant = now;
                g.duration_sec = 0.0;
                last_track_sig.clear();
                last_reported_pos = -1.0;
            }

            if g.enabled && !g.quit {
                let _ = cvar.wait_timeout(g, POLL_INTERVAL);
            }
        }
    }
}

fn load_or_cached_art(
    art_url: &str,
    cache: &mut Vec<CachedArtEntry>,
    next_rev: &mut u64,
) -> Option<(Arc<MediaArt>, MediaPalette)> {
    if let Some(idx) = cache.iter().position(|(u, _)| u == art_url) {
        let entry = cache.remove(idx);
        let res = entry.1.clone();
        cache.push(entry);
        return res;
    }

    let decoded = fetch_and_decode_art(art_url, next_rev);
    if cache.len() >= ART_CACHE_CAP {
        cache.remove(0);
    }
    cache.push((art_url.to_string(), decoded.clone()));
    decoded
}

fn fetch_and_decode_art(art_url: &str, next_rev: &mut u64) -> Option<(Arc<MediaArt>, MediaPalette)> {
    let bytes = if art_url.starts_with("file://") {
        let path = decode_file_uri(art_url)?;
        let meta = std::fs::metadata(&path).ok()?;
        if !meta.is_file() || meta.len() > MAX_LOCAL_ART_BYTES {
            return None;
        }
        std::fs::read(&path).ok()?
    } else if art_url.starts_with("data:") {
        decode_data_uri(art_url)?
    } else if art_url.starts_with("http://") || art_url.starts_with("https://") {
        fetch_http_art(art_url)?
    } else {
        return None;
    };

    let mut img = image::load_from_memory(&bytes).ok()?;
    if img.width() > MAX_ART_DIM || img.height() > MAX_ART_DIM {
        img = img.thumbnail(MAX_ART_DIM, MAX_ART_DIM);
    }
    let rgba = img.to_rgba8();
    let width = rgba.width();
    let height = rgba.height();
    if width == 0 || height == 0 {
        return None;
    }
    let palette = extract_palette(&rgba);
    let rev = *next_rev;
    *next_rev = next_rev.saturating_add(1);
    Some((
        Arc::new(MediaArt {
            rev,
            width,
            height,
            rgba: rgba.into_raw(),
        }),
        palette,
    ))
}

fn fetch_http_art(url: &str) -> Option<Vec<u8>> {
    let max_size_str = MAX_HTTP_ART_BYTES.to_string();
    let out = Command::new("curl")
        .args(["-fsSL", "--max-time", "5", "--max-filesize", &max_size_str, "--", url])
        .output()
        .ok()?;
    if !out.status.success() || out.stdout.is_empty() || out.stdout.len() > MAX_HTTP_ART_BYTES {
        return None;
    }
    Some(out.stdout)
}

fn decode_file_uri(uri: &str) -> Option<PathBuf> {
    let mut rest = uri.strip_prefix("file://")?;
    if let Some(after_host) = rest.strip_prefix("localhost") {
        if after_host.starts_with('/') {
            rest = after_host;
        }
    }
    if !rest.starts_with('/') {
        return None;
    }
    let raw = rest.as_bytes();
    let mut decoded = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%' && i + 2 < raw.len() {
            if let (Some(h), Some(l)) = (hex_val(raw[i + 1]), hex_val(raw[i + 2])) {
                decoded.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        decoded.push(raw[i]);
        i += 1;
    }
    let s = String::from_utf8(decoded).ok()?;
    let path = PathBuf::from(&s);
    if path.is_absolute() && Path::new(&s).as_os_str().len() > 1 {
        Some(path)
    } else {
        None
    }
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

fn decode_data_uri(uri: &str) -> Option<Vec<u8>> {
    let rest = uri.strip_prefix("data:")?;
    let (header, body) = rest.split_once(',')?;
    if !header.split(';').any(|p| p.eq_ignore_ascii_case("base64")) {
        return None;
    }
    decode_base64(body)
}

fn decode_base64(input: &str) -> Option<Vec<u8>> {
    let mut vals = Vec::with_capacity(input.len());
    let mut pad = 0usize;
    for b in input.bytes() {
        if b.is_ascii_whitespace() {
            continue;
        }
        if b == b'=' {
            pad += 1;
            vals.push(0u8);
            continue;
        }
        if pad > 0 {
            return None;
        }
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        vals.push(v);
    }
    if vals.len() % 4 != 0 || pad > 2 {
        return None;
    }
    let mut out = Vec::with_capacity((vals.len() / 4) * 3);
    for (idx, chunk) in vals.chunks_exact(4).enumerate() {
        let is_last = (idx + 1) * 4 == vals.len();
        let n = ((chunk[0] as u32) << 18)
            | ((chunk[1] as u32) << 12)
            | ((chunk[2] as u32) << 6)
            | (chunk[3] as u32);
        out.push(((n >> 16) & 0xFF) as u8);
        if !is_last || pad < 2 {
            out.push(((n >> 8) & 0xFF) as u8);
        }
        if !is_last || pad == 0 {
            out.push((n & 0xFF) as u8);
        }
    }
    Some(out)
}

fn poll_mpris() -> Option<MprisSample> {
    let list_out = Command::new("busctl")
        .args(["--user", "list", "--no-pager", "--no-legend"])
        .output()
        .ok()?;
    if !list_out.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&list_out.stdout);
    let mut services = Vec::new();
    for line in stdout.lines() {
        if let Some(svc) = line.split_whitespace().next() {
            if svc.starts_with("org.mpris.MediaPlayer2.") {
                services.push(svc.to_string());
            }
        }
    }
    if services.is_empty() {
        return None;
    }

    let mut best: Option<(u8, MprisSample)> = None;
    for svc in services {
        let Some(sample) = query_mpris_service(&svc) else {
            continue;
        };
        let rank = match (sample.state, !sample.title.is_empty()) {
            (MediaPlaybackState::Playing, true) => 4,
            (MediaPlaybackState::Playing, false) => 3,
            (MediaPlaybackState::Paused, true) => 2,
            (MediaPlaybackState::Paused, false) => 1,
            (MediaPlaybackState::Stopped, _) => 0,
        };
        if rank == 0 {
            continue;
        }
        if rank == 4 {
            return Some(sample);
        }
        if best.as_ref().is_none_or(|(r, _)| rank > *r) {
            best = Some((rank, sample));
        }
    }
    best.map(|(_, s)| s)
}

fn query_mpris_service(service: &str) -> Option<MprisSample> {
    let fast = Command::new("busctl")
        .args([
            "--user",
            "get-property",
            service,
            "/org/mpris/MediaPlayer2",
            "org.mpris.MediaPlayer2.Player",
            "PlaybackStatus",
            "Position",
            "Metadata",
            "--json=short",
        ])
        .output()
        .ok();
    if let Some(out) = fast {
        if out.status.success() {
            let text = String::from_utf8_lossy(&out.stdout);
            if let Some(sample) = parse_busctl_player_output(&text, true) {
                return Some(sample);
            }
        }
    }

    let fallback = Command::new("busctl")
        .args([
            "--user",
            "get-property",
            service,
            "/org/mpris/MediaPlayer2",
            "org.mpris.MediaPlayer2.Player",
            "PlaybackStatus",
            "Metadata",
            "--json=short",
        ])
        .output()
        .ok()?;
    if !fallback.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&fallback.stdout);
    parse_busctl_player_output(&text, false)
}

fn parse_busctl_player_output(stdout: &str, has_position: bool) -> Option<MprisSample> {
    let mut lines = stdout.lines().filter(|l| !l.trim().is_empty());
    let status_json: Value = serde_json::from_str(lines.next()?).ok()?;
    let pos_us = if has_position {
        let pos_json: Value = serde_json::from_str(lines.next()?).ok()?;
        json_microseconds(pos_json.get("data"))
    } else {
        0.0
    };
    let meta_json: Value = serde_json::from_str(lines.next()?).ok()?;

    let status_str = status_json.get("data").and_then(|v| v.as_str()).unwrap_or("");
    let state = MediaPlaybackState::from_mpris(status_str);
    let data = meta_json.get("data").unwrap_or(&Value::Null);

    let title = meta_text(data, "xesam:title");
    let artist = meta_text(data, "xesam:artist");
    let album_title = meta_text(data, "xesam:album");
    let album_artist = meta_text(data, "xesam:albumArtist");
    let genres = meta_text(data, "xesam:genre");
    let art_url = meta_text(data, "mpris:artUrl");

    let len_us = data.get("mpris:length").and_then(|v| json_microseconds_opt(v.get("data"))).unwrap_or(0.0);
    let position_sec = (pos_us.max(0.0) / 1_000_000.0) as f32;
    let duration_sec = (len_us.max(0.0) / 1_000_000.0) as f32;

    Some(MprisSample {
        state,
        title,
        artist,
        album_title,
        album_artist,
        genres,
        art_url,
        position_sec,
        duration_sec,
    })
}

fn json_microseconds(v: Option<&Value>) -> f64 {
    json_microseconds_opt(v).unwrap_or(0.0)
}

fn json_microseconds_opt(v: Option<&Value>) -> Option<f64> {
    let v = v?;
    v.as_f64()
        .or_else(|| v.as_i64().map(|n| n as f64))
        .or_else(|| v.as_u64().map(|n| n as f64))
}

fn meta_text(data: &Value, key: &str) -> String {
    let Some(v) = data.get(key).and_then(|entry| entry.get("data")) else {
        return String::new();
    };
    if let Some(arr) = v.as_array() {
        arr.iter()
            .filter_map(|x| x.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", ")
    } else {
        v.as_str().map(str::trim).unwrap_or("").to_string()
    }
}

/// Extracts a 5-color Wallpaper Engine media palette from RGBA8 artwork.
pub fn extract_palette(img: &image::RgbaImage) -> MediaPalette {
    let w = img.width();
    let h = img.height();
    if w == 0 || h == 0 {
        return MediaPalette::default();
    }

    let step_x = (w / 32).max(1);
    let step_y = (h / 32).max(1);
    let mut buckets: HashMap<u16, (u32, f32, f32, f32)> = HashMap::new();

    for y in (0..h).step_by(step_y as usize) {
        for x in (0..w).step_by(step_x as usize) {
            let p = img.get_pixel(x, y).0;
            if p[3] < 128 {
                continue;
            }
            let r = p[0] as f32 / 255.0;
            let g = p[1] as f32 / 255.0;
            let b = p[2] as f32 / 255.0;
            let key = ((p[0] as u16 >> 3) << 10) | ((p[1] as u16 >> 3) << 5) | (p[2] as u16 >> 3);
            let entry = buckets.entry(key).or_insert((0, 0.0, 0.0, 0.0));
            entry.0 += 1;
            entry.1 += r;
            entry.2 += g;
            entry.3 += b;
        }
    }

    if buckets.is_empty() {
        return MediaPalette::default();
    }

    let mut ranked: Vec<([f32; 3], f32)> = Vec::with_capacity(buckets.len());
    for (_, (count, sr, sg, sb)) in buckets {
        let inv = 1.0 / count as f32;
        let rgb = [sr * inv, sg * inv, sb * inv];
        let max_c = rgb[0].max(rgb[1]).max(rgb[2]);
        let min_c = rgb[0].min(rgb[1]).min(rgb[2]);
        let sat = if max_c > 1e-5 { (max_c - min_c) / max_c } else { 0.0 };
        let lum_mid = 0.5 * (max_c + min_c);
        let midtone_weight = if max_c < 0.08 || lum_mid > 0.94 { 0.15 } else { 1.0 };
        let score = (count as f32) * (0.35 + 1.65 * sat) * midtone_weight;
        ranked.push((rgb, score));
    }
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));

    let primary = ranked[0].0;
    let secondary = ranked
        .iter()
        .map(|&(rgb, _)| rgb)
        .find(|&rgb| rgb_dist(rgb, primary) >= 0.25)
        .unwrap_or_else(|| shift_tone(primary, 0.28));
    let tertiary = ranked
        .iter()
        .map(|&(rgb, _)| rgb)
        .find(|&rgb| rgb_dist(rgb, primary) >= 0.22 && rgb_dist(rgb, secondary) >= 0.22)
        .unwrap_or_else(|| shift_tone(secondary, -0.24));

    let primary_l = rel_luminance(primary);
    let (high_contrast, mut text) = if primary_l < 0.35 {
        (
            [1.0, 1.0, 1.0],
            [
                (primary[0] * 0.12 + 0.88).clamp(0.0, 1.0),
                (primary[1] * 0.12 + 0.88).clamp(0.0, 1.0),
                (primary[2] * 0.12 + 0.88).clamp(0.0, 1.0),
            ],
        )
    } else {
        (
            [0.0, 0.0, 0.0],
            [
                (primary[0] * 0.15).clamp(0.0, 1.0),
                (primary[1] * 0.15).clamp(0.0, 1.0),
                (primary[2] * 0.15).clamp(0.0, 1.0),
            ],
        )
    };
    if wcag_contrast(primary, text) < 4.5 {
        text = high_contrast;
    }

    MediaPalette {
        primary,
        secondary,
        tertiary,
        text,
        high_contrast,
    }
}

fn rgb_dist(a: [f32; 3], b: [f32; 3]) -> f32 {
    let dr = a[0] - b[0];
    let dg = a[1] - b[1];
    let db = a[2] - b[2];
    (dr * dr + dg * dg + db * db).sqrt()
}

fn shift_tone(rgb: [f32; 3], delta: f32) -> [f32; 3] {
    let l = rel_luminance(rgb);
    let dir = if l < 0.4 { delta.abs() } else { -delta.abs() };
    [
        (rgb[0] + dir).clamp(0.0, 1.0),
        (rgb[1] + dir * 0.85).clamp(0.0, 1.0),
        (rgb[2] + dir * 0.7).clamp(0.0, 1.0),
    ]
}

fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

fn rel_luminance(rgb: [f32; 3]) -> f32 {
    0.2126 * srgb_to_linear(rgb[0])
        + 0.7152 * srgb_to_linear(rgb[1])
        + 0.0722 * srgb_to_linear(rgb[2])
}

fn wcag_contrast(a: [f32; 3], b: [f32; 3]) -> f32 {
    let la = rel_luminance(a);
    let lb = rel_luminance(b);
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{Rgba, RgbaImage};

    #[test]
    fn parse_mpris_busctl_json() {
        let three_line = concat!(
            "{\"type\":\"s\",\"data\":\"Playing\"}\n",
            "{\"type\":\"x\",\"data\":45500000}\n",
            "{\"type\":\"a{sv}\",\"data\":{",
            "\"xesam:title\":{\"type\":\"s\",\"data\":\"Night Dancer\"},",
            "\"xesam:artist\":{\"type\":\"as\",\"data\":[\"imase\",\"Guest\"]},",
            "\"xesam:album\":{\"type\":\"s\",\"data\":\"Bonsai\"},",
            "\"xesam:albumArtist\":{\"type\":\"as\",\"data\":[\"imase\"]},",
            "\"xesam:genre\":{\"type\":\"as\",\"data\":[\"J-Pop\",\"Pop\"]},",
            "\"mpris:length\":{\"type\":\"x\",\"data\":210000000},",
            "\"mpris:artUrl\":{\"type\":\"s\",\"data\":\"file:///tmp/cover.png\"}",
            "}}\n"
        );
        let parsed = parse_busctl_player_output(three_line, true).expect("3-line parse");
        assert_eq!(parsed.state, MediaPlaybackState::Playing);
        assert_eq!(parsed.title, "Night Dancer");
        assert_eq!(parsed.artist, "imase, Guest");
        assert_eq!(parsed.album_title, "Bonsai");
        assert_eq!(parsed.album_artist, "imase");
        assert_eq!(parsed.genres, "J-Pop, Pop");
        assert_eq!(parsed.art_url, "file:///tmp/cover.png");
        assert!((parsed.position_sec - 45.5).abs() < 1e-3);
        assert!((parsed.duration_sec - 210.0).abs() < 1e-3);

        let two_line = concat!(
            "{\"type\":\"s\",\"data\":\"Paused\"}\n",
            "{\"type\":\"a{sv}\",\"data\":{\"xesam:title\":{\"type\":\"s\",\"data\":\"Radio\"}}}\n"
        );
        let fallback = parse_busctl_player_output(two_line, false).expect("2-line fallback");
        assert_eq!(fallback.state, MediaPlaybackState::Paused);
        assert_eq!(fallback.state.as_we_code(), 2);
        assert_eq!(fallback.title, "Radio");
        assert_eq!(fallback.position_sec, 0.0);
    }

    #[test]
    fn decode_file_uri_and_base64() {
        let p = decode_file_uri("file:///tmp/My%20Song%20%231.png").expect("valid file URI");
        assert_eq!(p, PathBuf::from("/tmp/My Song #1.png"));
        let p_host = decode_file_uri("file://localhost/tmp/cover.jpg").expect("localhost URI");
        assert_eq!(p_host, PathBuf::from("/tmp/cover.jpg"));
        assert!(decode_file_uri("http://example.com/a.png").is_none());

        // Minimal 1x1 red RGBA PNG encoded as base64 data URI.
        let mut img = RgbaImage::new(1, 1);
        img.put_pixel(0, 0, Rgba([220, 40, 60, 255]));
        let mut png_bytes = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut png_bytes), image::ImageFormat::Png)
            .expect("encode png");
        let b64 = encode_base64_for_test(&png_bytes);
        let uri = format!("data:image/png;base64,{b64}");
        let mut rev = 10u64;
        let (art, _) = fetch_and_decode_art(&uri, &mut rev).expect("decode data URI art");
        assert_eq!(art.rev, 10);
        assert_eq!(rev, 11);
        assert_eq!(art.width, 1);
        assert_eq!(art.height, 1);
        assert_eq!(&art.rgba[..4], &[220, 40, 60, 255]);
    }

    #[test]
    fn extract_palette_dark_and_bright_artwork() {
        // Dark deep-blue background with warm orange accent.
        let mut dark = RgbaImage::new(32, 32);
        for y in 0..32 {
            for x in 0..32 {
                let px = if x < 24 { [18, 42, 110, 255] } else { [225, 115, 20, 255] };
                dark.put_pixel(x, y, Rgba(px));
            }
        }
        let pal_dark = extract_palette(&dark);
        assert!(pal_dark.primary[2] > pal_dark.primary[0]);
        assert!(wcag_contrast(pal_dark.primary, pal_dark.text) >= 4.5);
        assert_eq!(pal_dark.high_contrast, [1.0, 1.0, 1.0]);
        assert!(rgb_dist(pal_dark.primary, pal_dark.secondary) >= 0.25);

        // Bright pastel cyan cover with coral highlight.
        let mut bright = RgbaImage::new(32, 32);
        for y in 0..32 {
            for x in 0..32 {
                let px = if y < 22 { [140, 235, 240, 255] } else { [245, 150, 165, 255] };
                bright.put_pixel(x, y, Rgba(px));
            }
        }
        let pal_bright = extract_palette(&bright);
        assert!(wcag_contrast(pal_bright.primary, pal_bright.text) >= 4.5);
        assert_eq!(pal_bright.high_contrast, [0.0, 0.0, 0.0]);
        assert!(rgb_dist(pal_bright.primary, pal_bright.secondary) >= 0.2);
    }

    fn encode_base64_for_test(bytes: &[u8]) -> String {
        const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = *chunk.get(1).unwrap_or(&0) as u32;
            let b2 = *chunk.get(2).unwrap_or(&0) as u32;
            let n = (b0 << 16) | (b1 << 8) | b2;
            out.push(TABLE[((n >> 18) & 63) as usize] as char);
            out.push(TABLE[((n >> 12) & 63) as usize] as char);
            out.push(if chunk.len() > 1 { TABLE[((n >> 6) & 63) as usize] as char } else { '=' });
            out.push(if chunk.len() > 2 { TABLE[(n & 63) as usize] as char } else { '=' });
        }
        out
    }
}
