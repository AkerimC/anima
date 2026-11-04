//! Real-time MPRIS media tracker, automatic synced lyrics (`.lrc` / LRCLIB) fetcher,
//! and multi-language (Turkish, English, Japanese) Phoneme-to-Viseme sequencer.
//!
//! Combines synced song lyrics with the real-time acoustic vocal formant analyzer
//! (`src/audio.rs`) so desktop characters articulate exact vowel shapes (`A, I, U, E, O`),
//! bilabial lip closures (`M, B, P`), and sibilants (`S, Ş, Ç, Z`) in sync with music.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

/// A single timestamped lyric line from a `.lrc` file or LRCLIB response.
#[derive(Debug, Clone)]
pub struct LyricLine {
    pub start_sec: f32,
    pub end_sec: f32,
    pub text: String,
    pub phonemes: Vec<PhonemeStep>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PhonemeStep {
    /// Target vertical mouth opening (`0.0 .. 1.0`).
    pub open: f32,
    /// Target horizontal lip form (`-1.0` = rounded O/U, `0.0` = open A, `+1.0` = spread I/E).
    pub form: f32,
    /// Target dental/fricative sibilance (`0.0 .. 1.0`).
    pub sibilance: f32,
    /// True if this step is a bilabial consonant closure (`M`, `B`, `P`) where lips must meet.
    pub closure: bool,
}

/// Snapshot of the current lyric & phoneme state at a given instant.
#[derive(Debug, Clone, Default)]
pub struct LyricSnapshot {
    pub playing: bool,
    pub track_title: String,
    pub track_artist: String,
    pub has_synced_lyrics: bool,
    pub active_line: Option<String>,
    pub in_vocal_line: bool,
    pub step: PhonemeStep,
}

impl Default for PhonemeStep {
    fn default() -> Self {
        Self {
            open: 0.0,
            form: 0.0,
            sibilance: 0.0,
            closure: false,
        }
    }
}

struct SharedLyrics {
    enabled: bool,
    quit: bool,
    playing: bool,
    track_key: String,
    track_title: String,
    track_artist: String,
    base_pos_sec: f32,
    base_instant: Instant,
    has_synced_lyrics: bool,
    lines: Vec<LyricLine>,
}

#[derive(Clone)]
pub struct LyricsEngine {
    inner: Arc<(Mutex<SharedLyrics>, Condvar)>,
}

impl LyricsEngine {
    pub fn spawn(initially_enabled: bool) -> Self {
        let inner = Arc::new((
            Mutex::new(SharedLyrics {
                enabled: initially_enabled,
                quit: false,
                playing: false,
                track_key: String::new(),
                track_title: String::new(),
                track_artist: String::new(),
                base_pos_sec: 0.0,
                base_instant: Instant::now(),
                has_synced_lyrics: false,
                lines: Vec::new(),
            }),
            Condvar::new(),
        ));
        let thread_inner = Arc::clone(&inner);
        std::thread::Builder::new()
            .name("anima-lyrics".into())
            .spawn(move || lyrics_worker(thread_inner))
            .ok();
        Self { inner }
    }

    pub fn set_enabled(&self, enabled: bool) {
        let (lock, cvar) = &*self.inner;
        if let Ok(mut g) = lock.lock() {
            if g.enabled != enabled {
                g.enabled = enabled;
                cvar.notify_all();
            }
        }
    }

    pub fn stop(&self) {
        let (lock, cvar) = &*self.inner;
        if let Ok(mut g) = lock.lock() {
            g.quit = true;
            g.enabled = false;
            cvar.notify_all();
        }
    }

    /// Evaluates the exact phoneme/viseme at the current interpolated playback timestamp.
    pub fn sample(&self) -> LyricSnapshot {
        let (lock, _) = &*self.inner;
        let Ok(g) = lock.lock() else {
            return LyricSnapshot::default();
        };
        if !g.enabled || !g.playing {
            return LyricSnapshot {
                playing: false,
                track_title: g.track_title.clone(),
                track_artist: g.track_artist.clone(),
                has_synced_lyrics: g.has_synced_lyrics,
                active_line: None,
                in_vocal_line: false,
                step: PhonemeStep::default(),
            };
        }

        let pos = g.base_pos_sec + g.base_instant.elapsed().as_secs_f32();
        if g.lines.is_empty() {
            return LyricSnapshot {
                playing: true,
                track_title: g.track_title.clone(),
                track_artist: g.track_artist.clone(),
                has_synced_lyrics: false,
                active_line: None,
                in_vocal_line: false,
                step: PhonemeStep::default(),
            };
        }

        // Binary search or scan for active line at `pos`
        for line in &g.lines {
            if pos >= line.start_sec && pos <= line.end_sec {
                if line.phonemes.is_empty() {
                    return LyricSnapshot {
                        playing: true,
                        track_title: g.track_title.clone(),
                        track_artist: g.track_artist.clone(),
                        has_synced_lyrics: g.has_synced_lyrics,
                        active_line: Some(line.text.clone()),
                        in_vocal_line: false,
                        step: PhonemeStep::default(),
                    };
                }
                let dur = (line.end_sec - line.start_sec).max(0.15);
                // Sing/articulate across the first 88% of the line window (leaving a natural breath at the end)
                let active_dur = (dur * 0.88).min(line.phonemes.len() as f32 * 0.13).max(0.25);
                let t = (pos - line.start_sec) / active_dur;
                if t <= 1.0 {
                    let idx_f = t * (line.phonemes.len() as f32);
                    let idx0 = (idx_f.floor() as usize).min(line.phonemes.len() - 1);
                    let idx1 = (idx0 + 1).min(line.phonemes.len() - 1);
                    let frac = idx_f.fract();
                    let p0 = line.phonemes[idx0];
                    let p1 = line.phonemes[idx1];
                    let step = PhonemeStep {
                        open: p0.open + (p1.open - p0.open) * frac,
                        form: p0.form + (p1.form - p0.form) * frac,
                        sibilance: p0.sibilance + (p1.sibilance - p0.sibilance) * frac,
                        closure: p0.closure,
                    };
                    return LyricSnapshot {
                        playing: true,
                        track_title: g.track_title.clone(),
                        track_artist: g.track_artist.clone(),
                        has_synced_lyrics: g.has_synced_lyrics,
                        active_line: Some(line.text.clone()),
                        in_vocal_line: true,
                        step,
                    };
                } else {
                    return LyricSnapshot {
                        playing: true,
                        track_title: g.track_title.clone(),
                        track_artist: g.track_artist.clone(),
                        has_synced_lyrics: g.has_synced_lyrics,
                        active_line: Some(line.text.clone()),
                        in_vocal_line: false,
                        step: PhonemeStep::default(),
                    };
                }
            }
        }

        LyricSnapshot {
            playing: true,
            track_title: g.track_title.clone(),
            track_artist: g.track_artist.clone(),
            has_synced_lyrics: g.has_synced_lyrics,
            active_line: None,
            in_vocal_line: false,
            step: PhonemeStep::default(),
        }
    }
}

/// Fuses the real-time acoustic vocal formant parameters (`audio_mouth: [open, form, sib, act]`)
/// with the synced lyric phoneme sequencer (`LyricSnapshot`).
pub fn fuse_mouth_visemes(audio_mouth: [f32; 4], lyric: &LyricSnapshot) -> [f32; 4] {
    let [ac_open, ac_form, ac_sib, ac_act] = audio_mouth;

    // Case 1: Synced lyrics are available and playing
    if lyric.playing && lyric.has_synced_lyrics {
        if lyric.in_vocal_line {
            let ph = lyric.step;
            if ph.closure {
                // Bilabial M / B / P closure: snap lips closed briefly so consonant articulation is crisp!
                return [ac_open * 0.08, ph.form * 0.5 + ac_form * 0.5, 0.0, 1.0];
            }
            // Gate lyric phoneme opening by actual audio presence so pauses in singing still respected,
            // while shaping the vowel opening and lip form according to the lyric phoneme!
            let audio_gate = (ac_act * 1.45 + ac_open * 0.65).clamp(0.28, 1.15);
            let fused_open = (ph.open * 0.62 * audio_gate + ac_open * 0.48).clamp(0.0, 1.0);
            let fused_form = (ph.form * 0.68 + ac_form * 0.32).clamp(-1.0, 1.0);
            let fused_sib = (ph.sibilance * 0.60 + ac_sib * 0.40).clamp(0.0, 1.0);
            return [fused_open, fused_form, fused_sib, 1.0];
        } else {
            // Between lyric lines (instrumental break): strongly damp mouth opening unless strong vocal formant
            let damped_open = if ac_act > 0.72 {
                (ac_open - 0.25).max(0.0) * 0.45
            } else {
                0.0
            };
            return [damped_open, ac_form * 0.5, ac_sib * 0.3, damped_open];
        }
    }

    // Case 2: Pure real-time acoustic vocal formant & syllabic viseme mode (works for all audio/video/streams)
    [ac_open, ac_form, ac_sib, ac_act]
}

fn lyrics_worker(inner: Arc<(Mutex<SharedLyrics>, Condvar)>) {
    let (lock, cvar) = &*inner;
    let mut last_reported_mpris_pos = -1.0f32;
    let mut smooth_pos_anchor = 0.0f32;
    let mut smooth_instant_anchor = Instant::now();

    loop {
        // Sleep at 0% CPU while disabled
        {
            let mut guard = match lock.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            while !guard.enabled && !guard.quit {
                guard = match cvar.wait(guard) {
                    Ok(g) => g,
                    Err(_) => return,
                };
            }
            if guard.quit {
                return;
            }
        }

        if let Some(mpris) = poll_mpris_player() {
            let key = format!("{} — {}", mpris.artist.trim(), mpris.title.trim());
            let need_fetch = {
                let Ok(g) = lock.lock() else {
                    return;
                };
                g.track_key != key && !mpris.title.trim().is_empty()
            };

            if need_fetch {
                log::info!("lyrics: detected track '{}'", key);
                let (lines, synced) = fetch_lyrics_for_track(&mpris);
                log::info!(
                    "lyrics: loaded {} lyric lines (synced={}) for '{}'",
                    lines.len(),
                    synced,
                    key
                );
                last_reported_mpris_pos = mpris.position_sec;
                smooth_pos_anchor = mpris.position_sec;
                smooth_instant_anchor = Instant::now();

                if let Ok(mut g) = lock.lock() {
                    g.track_key = key;
                    g.track_title = mpris.title.clone();
                    g.track_artist = mpris.artist.clone();
                    g.has_synced_lyrics = synced;
                    g.lines = lines;
                }
            }

            // Handle players whose MPRIS Position updates only periodically vs continuously
            let now = Instant::now();
            if (mpris.position_sec - last_reported_mpris_pos).abs() > 0.05 {
                last_reported_mpris_pos = mpris.position_sec;
                smooth_pos_anchor = mpris.position_sec;
                smooth_instant_anchor = now;
            } else if !mpris.playing {
                smooth_instant_anchor = now;
            }

            if let Ok(mut g) = lock.lock() {
                g.playing = mpris.playing;
                g.base_pos_sec = smooth_pos_anchor;
                g.base_instant = smooth_instant_anchor;
            }
        } else if let Ok(mut g) = lock.lock() {
            g.playing = false;
        }

        std::thread::sleep(Duration::from_millis(750));
    }
}

#[derive(Debug, Clone)]
struct MprisTrack {
    playing: bool,
    title: String,
    artist: String,
    url: String,
    embedded_lyrics: String,
    position_sec: f32,
    duration_sec: f32,
}

fn poll_mpris_player() -> Option<MprisTrack> {
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

    let mut fallback: Option<MprisTrack> = None;
    for svc in services {
        if let Some(track) = query_mpris_service(&svc) {
            if track.playing {
                return Some(track);
            }
            if fallback.is_none() {
                fallback = Some(track);
            }
        }
    }
    fallback
}

fn query_mpris_service(service: &str) -> Option<MprisTrack> {
    let out = Command::new("busctl")
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
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let status_json: Value = serde_json::from_str(lines.next()?) .ok()?;
    let pos_json: Value = serde_json::from_str(lines.next()?) .ok()?;
    let meta_json: Value = serde_json::from_str(lines.next()?) .ok()?;

    let status = status_json.get("data")?.as_str().unwrap_or("");
    let playing = status.eq_ignore_ascii_case("Playing");
    let pos_us = pos_json.get("data").and_then(|v| v.as_i64()).unwrap_or(0);
    let position_sec = (pos_us.max(0) as f64 / 1_000_000.0) as f32;

    let data = meta_json.get("data")?;
    let title = data
        .get("xesam:title")
        .and_then(|v| v.get("data"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if title.is_empty() {
        return None;
    }

    let artist = data
        .get("xesam:artist")
        .and_then(|v| v.get("data"))
        .and_then(|v| {
            if let Some(arr) = v.as_array() {
                arr.first().and_then(|x| x.as_str()).map(|s| s.to_string())
            } else {
                v.as_str().map(|s| s.to_string())
            }
        })
        .unwrap_or_default()
        .trim()
        .to_string();

    let url = data
        .get("xesam:url")
        .and_then(|v| v.get("data"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();

    let embedded_lyrics = data
        .get("xesam:asText")
        .and_then(|v| v.get("data"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let len_us = data
        .get("mpris:length")
        .and_then(|v| v.get("data"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let duration_sec = (len_us.max(0) as f64 / 1_000_000.0) as f32;

    Some(MprisTrack {
        playing,
        title,
        artist,
        url,
        embedded_lyrics,
        position_sec,
        duration_sec,
    })
}

fn cache_dir() -> Option<PathBuf> {
    let base = dirs::cache_dir()?;
    let dir = base.join("anima").join("lyrics");
    fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
}

/// Cleans YouTube / Spotify suffixes like `(Official Video)`, `[Lyrics]`, `feat. ...`, `- Topic`.
fn clean_track_metadata(raw_artist: &str, raw_title: &str) -> (String, String) {
    let mut artist = raw_artist
        .trim_end_matches(" - Topic")
        .trim_end_matches("VEVO")
        .trim()
        .to_string();
    let mut title = raw_title.trim().to_string();

    // If title is formatted as "Artist - Song Title", split it
    if let Some((left, right)) = title.split_once(" - ") {
        if artist.is_empty()
            || left
                .to_lowercase()
                .contains(&artist.to_lowercase())
            || artist
                .to_lowercase()
                .contains(&left.to_lowercase())
        {
            if artist.is_empty() {
                artist = left.trim().to_string();
            }
            title = right.trim().to_string();
        }
    }

    // Strip parenthetical / bracketed noise like (Official Music Video), [4K], (prod. ...)
    for (open, close) in [('(', ')'), ('[', ']'), ('【', '】')] {
        while let Some(start) = title.find(open) {
            if let Some(rel_end) = title[start..].find(close) {
                let end = start + rel_end;
                let inside = title[start + open.len_utf8()..end].to_lowercase();
                if inside.contains("official")
                    || inside.contains("video")
                    || inside.contains("audio")
                    || inside.contains("lyric")
                    || inside.contains("sözleri")
                    || inside.contains("klip")
                    || inside.contains("prod")
                    || inside.contains("feat")
                    || inside.contains("ft.")
                    || inside.contains("mv")
                    || inside.contains("hd")
                    || inside.contains("4k")
                {
                    title.replace_range(start..=end, "");
                    title = title.trim().to_string();
                    continue;
                }
            }
            break;
        }
    }

    for marker in [" feat.", " ft.", " Feat.", " FT."] {
        if let Some(idx) = title.find(marker) {
            title.truncate(idx);
        }
    }

    (artist.trim().to_string(), title.trim().to_string())
}

fn fetch_lyrics_for_track(track: &MprisTrack) -> (Vec<LyricLine>, bool) {
    // 1. Embedded MPRIS lyrics
    if !track.embedded_lyrics.trim().is_empty() {
        let parsed = parse_lrc_or_plain(&track.embedded_lyrics, track.duration_sec);
        if !parsed.0.is_empty() {
            return parsed;
        }
    }

    // 2. Local .lrc next to file:// media URL
    if let Some(local_path) = track.url.strip_prefix("file://") {
        let p = PathBuf::from(local_path).with_extension("lrc");
        if let Ok(content) = fs::read_to_string(&p) {
            let parsed = parse_lrc_or_plain(&content, track.duration_sec);
            if !parsed.0.is_empty() {
                return parsed;
            }
        }
    }

    let (clean_artist, clean_title) = clean_track_metadata(&track.artist, &track.title);
    let cache_slug = sanitize_filename(&format!("{clean_artist}_{clean_title}"));

    // 3. Local disk cache (~/.cache/anima/lyrics/)
    if let Some(dir) = cache_dir() {
        let lrc_path = dir.join(format!("{cache_slug}.lrc"));
        if let Ok(cached) = fs::read_to_string(&lrc_path) {
            if cached.trim() == "NONE" {
                return (Vec::new(), false);
            }
            let parsed = parse_lrc_or_plain(&cached, track.duration_sec);
            if !parsed.0.is_empty() {
                return parsed;
            }
        }
    }

    // 4. Query LRCLIB API via curl
    let queries = [
        format!("{clean_artist} {clean_title}"),
        clean_title.clone(),
        format!("{} {}", track.artist, track.title),
    ];

    for q in queries {
        let q = q.trim();
        if q.is_empty() {
            continue;
        }
        if let Some(raw_lrc) = query_lrclib(q, &clean_artist, &clean_title, track.duration_sec) {
            if let Some(dir) = cache_dir() {
                let _ = fs::write(dir.join(format!("{cache_slug}.lrc")), &raw_lrc);
            }
            return parse_lrc_or_plain(&raw_lrc, track.duration_sec);
        }
    }

    if let Some(dir) = cache_dir() {
        let _ = fs::write(dir.join(format!("{cache_slug}.lrc")), "NONE");
    }
    (Vec::new(), false)
}

fn query_lrclib(query: &str, artist: &str, title: &str, duration_sec: f32) -> Option<String> {
    let out = Command::new("curl")
        .args([
            "-sG",
            "--max-time",
            "4",
            "-A",
            "AnimaEngine/0.1.0 (https://github.com/anima)",
            "https://lrclib.net/api/search",
            "--data-urlencode",
            &format!("q={query}"),
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }

    let items: Vec<Value> = serde_json::from_slice(&out.stdout).ok()?;
    if items.is_empty() {
        return None;
    }

    let artist_lc = artist.to_lowercase();
    let title_lc = title.to_lowercase();

    // Score candidates preferring syncedLyrics + matching artist/title/duration
    let mut best_synced: Option<(i32, String)> = None;
    let mut best_plain: Option<(i32, String)> = None;

    for item in items {
        let item_title = item
            .get("trackName")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase();
        let item_artist = item
            .get("artistName")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase();
        let item_dur = item
            .get("duration")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0) as f32;

        let mut score = 0;
        if !title_lc.is_empty() && (item_title == title_lc || item_title.contains(&title_lc)) {
            score += 50;
        }
        if !artist_lc.is_empty() && (item_artist.contains(&artist_lc) || artist_lc.contains(&item_artist)) {
            score += 40;
        }
        if duration_sec > 10.0 && item_dur > 10.0 {
            let diff = (duration_sec - item_dur).abs();
            if diff < 3.0 {
                score += 30;
            } else if diff < 10.0 {
                score += 10;
            }
        }

        if let Some(synced) = item.get("syncedLyrics").and_then(|v| v.as_str()) {
            if !synced.trim().is_empty() {
                if best_synced.as_ref().map_or(true, |(s, _)| score > *s) {
                    best_synced = Some((score, synced.to_string()));
                }
            }
        }
        if let Some(plain) = item.get("plainLyrics").and_then(|v| v.as_str()) {
            if !plain.trim().is_empty() {
                if best_plain.as_ref().map_or(true, |(s, _)| score > *s) {
                    best_plain = Some((score, plain.to_string()));
                }
            }
        }
    }

    if let Some((score, s)) = best_synced {
        if score >= 30 {
            return Some(s);
        }
    }
    if let Some((score, p)) = best_plain {
        if score >= 40 {
            return Some(p);
        }
    }
    None
}

/// Parses `.lrc` timestamped lines (`[mm:ss.xx] text`) or plain lyrics into `Vec<LyricLine>`.
pub fn parse_lrc_or_plain(raw: &str, duration_sec: f32) -> (Vec<LyricLine>, bool) {
    let mut timed: Vec<(f32, String)> = Vec::new();
    let mut plain_lines: Vec<String> = Vec::new();

    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some((ts_sec, text)) = parse_lrc_timestamp_line(trimmed) {
            timed.push((ts_sec, text));
        } else if !trimmed.starts_with('[') {
            plain_lines.push(trimmed.to_string());
        }
    }

    if !timed.is_empty() {
        timed.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let mut out = Vec::with_capacity(timed.len());
        for i in 0..timed.len() {
            let start = timed[i].0;
            let next_start = timed
                .get(i + 1)
                .map(|x| x.0)
                .unwrap_or_else(|| start + 5.0);
            let text = timed[i].1.trim().to_string();
            let phonemes = text_to_phonemes(&text);
            let max_natural_dur = (phonemes.len() as f32 * 0.16).clamp(1.2, 6.5);
            let end = next_start.min(start + max_natural_dur);
            out.push(LyricLine {
                start_sec: start,
                end_sec: end.max(start + 0.25),
                text,
                phonemes,
            });
        }
        return (out, true);
    }

    if !plain_lines.is_empty() {
        let total = if duration_sec > 15.0 {
            duration_sec * 0.92
        } else {
            (plain_lines.len() as f32) * 3.2
        };
        let step = (total / plain_lines.len().max(1) as f32).clamp(1.5, 5.0);
        let mut out = Vec::with_capacity(plain_lines.len());
        for (i, text) in plain_lines.into_iter().enumerate() {
            let start = 2.0 + (i as f32) * step;
            let phonemes = text_to_phonemes(&text);
            out.push(LyricLine {
                start_sec: start,
                end_sec: start + step * 0.88,
                text,
                phonemes,
            });
        }
        return (out, false);
    }

    (Vec::new(), false)
}

fn parse_lrc_timestamp_line(line: &str) -> Option<(f32, String)> {
    let rest = line.strip_prefix('[')?;
    let close_idx = rest.find(']')?;
    let tag = &rest[..close_idx];
    let text = rest[close_idx + 1..].trim();

    let (min_str, sec_str) = tag.split_once(':')?;
    let mins: f32 = min_str.trim().parse().ok()?;
    let secs: f32 = sec_str.trim().replace(',', ".").parse().ok()?;
    Some((mins * 60.0 + secs, text.to_string()))
}

/// Converts a lyric line (Turkish, English, Japanese romaji/kana) into a sequence of
/// articulatory `PhonemeStep` targets (`open`, `form`, `sibilance`, `closure`).
pub fn text_to_phonemes(text: &str) -> Vec<PhonemeStep> {
    // Strip inline enhanced LRC tags `<00:12.34>` if present
    let mut cleaned = String::with_capacity(text.len());
    let mut in_angle = false;
    for ch in text.chars() {
        if ch == '<' {
            in_angle = true;
        } else if ch == '>' {
            in_angle = false;
        } else if !in_angle {
            cleaned.push(ch);
        }
    }

    let trimmed = cleaned.trim();
    if trimmed.is_empty() || trimmed == "♪" || trimmed.eq_ignore_ascii_case("instrumental") {
        return Vec::new();
    }

    let mut steps = Vec::with_capacity(trimmed.chars().count().min(64));
    let mut prev_was_space = false;

    for ch in trimmed.chars() {
        if ch.is_whitespace() || matches!(ch, ',' | '.' | '!' | '?' | ';' | ':' | '-' | '—') {
            if !prev_was_space && !steps.is_empty() {
                steps.push(PhonemeStep {
                    open: 0.08,
                    form: 0.0,
                    sibilance: 0.0,
                    closure: false,
                });
                prev_was_space = true;
            }
            continue;
        }
        prev_was_space = false;

        if let Some(step) = char_to_phoneme(ch) {
            steps.push(step);
        }
    }

    steps
}

fn char_to_phoneme(ch: char) -> Option<PhonemeStep> {
    let lower = ch.to_lowercase().next().unwrap_or(ch);
    match lower {
        // Open vowel A (wide jaw opening, neutral horizontal lip form)
        'a' | 'â' | 'á' | 'à' | 'ä'
        | 'あ' | 'ア' | 'か' | 'カ' | 'さ' | 'サ' | 'た' | 'タ' | 'な' | 'ナ'
        | 'は' | 'ハ' | 'ま' | 'マ' | 'や' | 'ヤ' | 'ら' | 'ラ' | 'わ' | 'ワ' => {
            Some(PhonemeStep {
                open: 0.92,
                form: 0.05,
                sibilance: 0.0,
                closure: false,
            })
        }
        // Mid-front spread vowel E (medium jaw opening, wide smiling corners)
        'e' | 'é' | 'è' | 'ê'
        | 'え' | 'エ' | 'け' | 'ケ' | 'せ' | 'セ' | 'て' | 'テ' | 'ね' | 'ネ'
        | 'へ' | 'ヘ' | 'め' | 'メ' | 'れ' | 'レ' => Some(PhonemeStep {
            open: 0.64,
            form: 0.58,
            sibilance: 0.12,
            closure: false,
        }),
        // Close-front spread vowel I / İ / I (narrow vertical opening, wide horizontal corners, teeth visible)
        'i' | 'ı' | 'î' | 'í' | 'y'
        | 'い' | 'イ' | 'き' | 'キ' | 'し' | 'シ' | 'ち' | 'チ' | 'に' | 'ニ'
        | 'ひ' | 'ヒ' | 'み' | 'ミ' | 'り' | 'リ' => Some(PhonemeStep {
            open: 0.36,
            form: 0.92,
            sibilance: 0.22,
            closure: false,
        }),
        // Mid-back rounded vowel O / Ö (rounded vertical oval)
        'o' | 'ö' | 'ô' | 'ó'
        | 'お' | 'オ' | 'こ' | 'コ' | 'そ' | 'ソ' | 'と' | 'ト' | 'の' | 'ノ'
        | 'ほ' | 'ホ' | 'も' | 'モ' | 'よ' | 'ヨ' | 'ろ' | 'ロ' => Some(PhonemeStep {
            open: 0.78,
            form: -0.76,
            sibilance: 0.0,
            closure: false,
        }),
        // Close-back puckered vowel U / Ü (small rounded puckered mouth)
        'u' | 'ü' | 'û' | 'ú' | 'w'
        | 'う' | 'ウ' | 'く' | 'ク' | 'す' | 'ス' | 'つ' | 'ツ' | 'ぬ' | 'ヌ'
        | 'ふ' | 'フ' | 'む' | 'ム' | 'ゆ' | 'ユ' | 'る' | 'ル' => Some(PhonemeStep {
            open: 0.42,
            form: -0.95,
            sibilance: 0.0,
            closure: false,
        }),
        // Bilabials M / B / P (lips press completely closed!)
        'm' | 'b' | 'p' | 'ん' | 'ン' => Some(PhonemeStep {
            open: 0.0,
            form: -0.08,
            sibilance: 0.0,
            closure: true,
        }),
        // Labiodentals F / V (nearly closed, slight lower lip tuck + fricative)
        'f' | 'v' => Some(PhonemeStep {
            open: 0.14,
            form: 0.28,
            sibilance: 0.55,
            closure: false,
        }),
        // Sibilants & Affricates S / Ş / Z / C / Ç (narrow slit, upper teeth clearly visible)
        's' | 'ş' | 'z' | 'c' | 'ç' | 'x' => Some(PhonemeStep {
            open: 0.22,
            form: 0.52,
            sibilance: 0.88,
            closure: false,
        }),
        // Alveolar / Velar / Liquid consonants T / D / N / L / R / K / G / Ğ / H / J
        't' | 'd' | 'n' | 'l' | 'r' | 'k' | 'g' | 'ğ' | 'h' | 'j' | 'q' => {
            Some(PhonemeStep {
                open: 0.28,
                form: 0.22,
                sibilance: 0.35,
                closure: false,
            })
        }
        _ if ch.is_alphabetic() => Some(PhonemeStep {
            open: 0.55,
            form: 0.0,
            sibilance: 0.1,
            closure: false,
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lrc_parsing_and_phonemes() {
        let sample = "[00:01.50] Merhaba dünya\n[00:04.20] Bomb Girl Reze\n";
        let (lines, synced) = parse_lrc_or_plain(sample, 120.0);
        assert!(synced);
        assert_eq!(lines.len(), 2);
        assert!((lines[0].start_sec - 1.5).abs() < 0.01);
        assert_eq!(lines[0].text, "Merhaba dünya");
        // First phoneme of "Merhaba" is 'm' -> bilabial closure!
        assert!(lines[0].phonemes[0].closure);
        assert_eq!(lines[0].phonemes[0].open, 0.0);
        // Second phoneme is 'e' -> spread vowel E (form > 0.4)
        assert!(lines[0].phonemes[1].form > 0.4);
        // Last phoneme of "Merhaba" is 'a' -> open vowel A (open > 0.85)
        assert!(lines[0].phonemes[6].open > 0.85);
    }

    #[test]
    fn test_clean_track_metadata() {
        let (artist, title) = clean_track_metadata(
            "Kenshi Yonezu - Topic",
            "Kenshi Yonezu - IRIS OUT (Official Music Video) [4K]",
        );
        assert_eq!(artist, "Kenshi Yonezu");
        assert_eq!(title, "IRIS OUT");
    }
}
