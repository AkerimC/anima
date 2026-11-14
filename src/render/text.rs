//! Text rasterisation and Wallpaper Engine text-script emulation.
//!
//! Wallpaper Engine text layers are JavaScript snippets that return a string
//! (clock, date, day/night greeting, media metadata or a user property).  We do
//! not execute JavaScript; the update templates shipped with the built-in text
//! presets are recognised and reproduced here.  Everything is rasterised with
//! `ab_glyph` into an RGBA bitmap that the scene renderer uploads as a texture.
//!
//! Font sizing follows Wallpaper Engine: a text layer's font is `pointsize`
//! points, i.e. `pointsize * 5` scene units tall (see `UNITS_PER_POINT`).  The
//! layer is then transformed (including its scale) like any other layer.

use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;

use ab_glyph::{Font, FontVec, Glyph, GlyphId, PxScale, ScaleFont};
use crate::pkg::PackageFs;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HAlign {
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VAlign {
    Top,
    Center,
    Bottom,
}

/// Scene units per point for Wallpaper Engine text layers.
///
/// Verified empirically against the workshop previews of several
/// 3840×2160 scenes (Anurati/Quicksand/Atami fonts, point sizes 20–60):
/// `pointsize * 5` scene units matches the authored glyph height, while the
/// previous `300/96` value rendered every text layer ~1.6× too small.
pub const UNITS_PER_POINT: f32 = 5.0;

pub struct RasterizedText {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// Load a font either from the wallpaper package or from the system.
fn font_from_bytes(bytes: Vec<u8>) -> Option<FontVec> {
    FontVec::try_from_vec(bytes.clone())
        .or_else(|_| FontVec::try_from_vec_and_index(bytes, 0))
        .ok()
}

fn load_font_file(path: &Path) -> Option<FontVec> {
    let bytes = std::fs::read(path).ok()?;
    font_from_bytes(bytes)
}

fn fallback_fonts() -> &'static [FontVec] {
    static FALLBACKS: OnceLock<Vec<FontVec>> = OnceLock::new();
    FALLBACKS.get_or_init(|| {
        let mut out = Vec::new();
        let system_paths = [
            "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/TTF/DejaVuSans.ttf",
            "/usr/share/fonts/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Regular.ttf",
        ];
        for path in system_paths {
            if let Some(f) = load_font_file(Path::new(path)) {
                out.push(f);
            }
        }
        if let Some(p) = crate::pkg::resolve_we_asset("fonts/NotoSans-Regular.ttf") {
            if let Some(f) = load_font_file(&p) {
                out.push(f);
            }
        }
        out
    })
}

/// Load a font either from the wallpaper package or from the system.
pub fn load_font(pkg: &PackageFs, path: &str) -> Option<FontVec> {
    let path = path.trim();
    let lower = path.to_ascii_lowercase();
    if !path.is_empty() && !path.starts_with("systemfont_") {
        for candidate in [path, path.trim_start_matches('/')] {
            if let Ok(bytes) = pkg.read(candidate) {
                if let Some(font) = font_from_bytes(bytes) {
                    return Some(font);
                }
            }
            if let Some(we_file) = crate::pkg::resolve_we_asset(candidate) {
                if let Some(font) = load_font_file(&we_file) {
                    return Some(font);
                }
            }
        }
        // Some packages reference fonts relative to the package root or by basename.
        let base_lower = Path::new(&lower)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&lower);
        for entry in pkg.pkg_file_names() {
            let el = entry.to_ascii_lowercase();
            if el == lower || el.ends_with(&format!("/{base_lower}")) || el == base_lower {
                if let Ok(bytes) = pkg.read(entry) {
                    if let Some(font) = font_from_bytes(bytes) {
                        return Some(font);
                    }
                }
            }
        }
        // Case-insensitive search inside Wallpaper Engine's `assets/fonts/` directory.
        if let Some(we_fonts_dir) = crate::pkg::resolve_we_asset("fonts/NotoSans-Regular.ttf")
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        {
            if let Ok(rd) = std::fs::read_dir(&we_fonts_dir) {
                for entry in rd.flatten() {
                    if let Some(name) = entry.file_name().to_str() {
                        if name.to_ascii_lowercase() == base_lower {
                            if let Some(font) = load_font_file(&entry.path()) {
                                return Some(font);
                            }
                        }
                    }
                }
            }
        }
    }

    // Preserve monospace metrics for `systemfont_consolas`, `systemfont_courier*`, etc.
    if lower.contains("consola") || lower.contains("mono") || lower.contains("courier") {
        if let Some(p) = crate::pkg::resolve_we_asset("fonts/RobotoMono-Regular.ttf") {
            if let Some(font) = load_font_file(&p) {
                return Some(font);
            }
        }
        for candidate in [
            "/usr/share/fonts/Adwaita/AdwaitaMono-Regular.ttf",
            "/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Regular.ttf",
            "/usr/share/fonts/liberation/LiberationMono-Regular.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
            "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
        ] {
            if let Some(font) = load_font_file(Path::new(candidate)) {
                return Some(font);
            }
        }
    }

    // CJK system fonts (`systemfont_microsoft yahei`, `simhei`, `simsun`, `msgothic`, etc.).
    if [
        "yahei", "simhei", "simsun", "kaiti", "fangsong", "pingfang", "hiragino", "meiryo",
        "yu gothic", "msgothic", "msmincho", "malgun", "cjk",
    ]
    .iter()
    .any(|kw| lower.contains(kw))
    {
        for candidate in [
            "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/noto/NotoSansCJK-Regular.ttc",
        ] {
            if let Some(font) = load_font_file(Path::new(candidate)) {
                return Some(font);
            }
        }
    }

    // Try Wallpaper Engine's bundled fonts first (`NotoSans-Regular.ttf`, `RobotoMono-Regular.ttf`).
    for we_font in ["fonts/NotoSans-Regular.ttf", "fonts/RobotoMono-Regular.ttf"] {
        if let Some(p) = crate::pkg::resolve_we_asset(we_font) {
            if let Some(font) = load_font_file(&p) {
                return Some(font);
            }
        }
    }

    // `systemfont_arial` and friends: use a reasonable sans-serif fallback.
    const CANDIDATES: &[&str] = &[
        "/usr/share/fonts/Adwaita/AdwaitaSans-Regular.ttf",
        "/usr/share/fonts/cantarell/Cantarell-VF.otf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Regular.ttf",
    ];
    for candidate in CANDIDATES {
        if let Some(font) = load_font_file(Path::new(candidate)) {
            return Some(font);
        }
    }
    None
}

/// Lay out and rasterise `text`.
///
/// `font_px` is the font size in *texture pixels*, `padding_px` is the padding
/// in the same space.  The returned bitmap is top-down (row 0 = top).
#[allow(clippy::too_many_arguments)]
pub fn rasterize(
    font: &FontVec,
    text: &str,
    font_px: f32,
    padding_px: f32,
    spacing: [f32; 2],
    h_align: HAlign,
    v_align: VAlign,
    max_width_px: Option<f32>,
    max_rows: usize,
    fixed_size: Option<[u32; 2]>,
) -> RasterizedText {
    let font_px = font_px.clamp(1.0, 4096.0);
    let px = PxScale::from(font_px);
    let scaled = font.as_scaled(px);
    let line_height = (scaled.ascent() - scaled.descent() + scaled.line_gap() + spacing[1])
        .max(font_px * 0.5);
    let pad = padding_px.max(0.0);

    let mut lines: Vec<String> = text
        .split('\n')
        .map(|l| l.trim_end_matches('\r').to_string())
        .collect();
    if lines.is_empty() {
        lines.push(String::new());
    }

    // Word-wrap when the layer has a width limit.
    if let Some(max_w) = max_width_px {
        let max_w = (max_w - 2.0 * pad).max(font_px * 0.5);
        let mut wrapped = Vec::new();
        for line in &lines {
            if measure_line(font, px, line, spacing[0]) <= max_w || line.is_empty() {
                wrapped.push(line.clone());
                continue;
            }
            let mut current = String::new();
            for word in line.split_inclusive(' ') {
                if !current.is_empty()
                    && measure_line(font, px, &format!("{current}{word}"), spacing[0]) > max_w
                {
                    wrapped.push(current.trim_end().to_string());
                    current.clear();
                }
                current.push_str(word);
            }
            if !current.is_empty() {
                wrapped.push(current.trim_end().to_string());
            }
        }
        lines = wrapped;
    }
    if max_rows > 0 && lines.len() > max_rows {
        lines.truncate(max_rows);
    }

    let text_w = lines
        .iter()
        .map(|l| measure_line(font, px, l, spacing[0]))
        .fold(0.0f32, f32::max);
    let text_h = line_height * lines.len() as f32;

    let w = match fixed_size {
        Some([fw, _]) => fw.max(1).min(8192),
        None => (text_w + 2.0 * pad).ceil().clamp(1.0, 8192.0) as u32,
    };
    let h = match fixed_size {
        Some([_, fh]) => fh.max(1).min(8192),
        None => (text_h + 2.0 * pad).ceil().clamp(1.0, 8192.0) as u32,
    };
    let mut pixels = vec![0u8; (w * h * 4) as usize];

    let top = match v_align {
        VAlign::Top => pad,
        VAlign::Bottom => h as f32 - pad - text_h,
        VAlign::Center => (h as f32 - text_h) * 0.5,
    };

    for (line_idx, line) in lines.iter().enumerate() {
        let line_w = measure_line(font, px, line, spacing[0]);
        let line_x = match h_align {
            HAlign::Left => pad,
            HAlign::Right => w as f32 - pad - line_w,
            HAlign::Center => (w as f32 - line_w) * 0.5,
        };
        let baseline = top + line_idx as f32 * line_height + scaled.ascent();

        layout_line(font, px, line, line_x, spacing[0], |resolved, id, pen_x| {
            let glyph: Glyph = id.with_scale_and_position(px, ab_glyph::point(pen_x, baseline));
            if let Some(outline) = resolved.outline_glyph(glyph) {
                let bounds = outline.px_bounds();
                outline.draw(|gx, gy, coverage| {
                    let x = bounds.min.x.round() as i32 + gx as i32;
                    let y = bounds.min.y.round() as i32 + gy as i32;
                    if x < 0 || y < 0 || x >= w as i32 || y >= h as i32 {
                        return;
                    }
                    let idx = ((y as u32 * w + x as u32) * 4) as usize;
                    let old = pixels[idx + 3];
                    let a = (coverage * 255.0).round().clamp(0.0, 255.0) as u8;
                    pixels[idx] = 255;
                    pixels[idx + 1] = 255;
                    pixels[idx + 2] = 255;
                    pixels[idx + 3] = old.max(a);
                });
            }
        });
    }

    RasterizedText {
        width: w,
        height: h,
        pixels,
    }
}

/// Width of the widest line at the given pixel size.
pub fn measure(font: &FontVec, text: &str, font_px: f32, spacing: f32) -> f32 {
    let px = PxScale::from(font_px.max(1.0));
    text.split('\n')
        .map(|line| measure_line(font, px, line, spacing))
        .fold(0.0, f32::max)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FontSource {
    Primary,
    Fallback(usize),
}

/// Shared line-advance and glyph-fallback walker used by both measuring and rasterizing.
fn layout_line<'a>(
    primary: &'a FontVec,
    px: PxScale,
    line: &str,
    start_x: f32,
    extra: f32,
    mut on_glyph: impl FnMut(&'a FontVec, GlyphId, f32),
) -> f32 {
    let scaled_primary = primary.as_scaled(px);
    let space_adv = scaled_primary.h_advance(scaled_primary.glyph_id(' '));
    let mut pen_x = start_x;
    let mut prev: Option<(FontSource, GlyphId)> = None;

    for ch in line.chars() {
        if matches!(ch, '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{FEFF}') || ch.is_control() {
            if ch == '\t' {
                pen_x += space_adv * 4.0 + extra;
            }
            prev = None;
            continue;
        }
        if ch == ' ' {
            pen_x += space_adv + extra;
            prev = None;
            continue;
        }
        if matches!(ch, '\u{2000}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{00A0}') {
            let id = scaled_primary.glyph_id(ch);
            let adv = if id.0 != 0 {
                let raw = scaled_primary.h_advance(id);
                if raw > 0.0 {
                    raw
                } else {
                    unicode_space_factor(ch) * space_adv
                }
            } else {
                unicode_space_factor(ch) * space_adv
            };
            pen_x += adv + extra;
            prev = None;
            continue;
        }

        let (src, resolved) = resolve_font_for_char(primary, ch);
        let id = resolved.glyph_id(ch);
        let resolved_scaled = resolved.as_scaled(px);
        if let Some((prev_src, prev_id)) = prev {
            if prev_src == src {
                pen_x += resolved_scaled.kern(prev_id, id);
            }
        }
        on_glyph(resolved, id, pen_x);
        pen_x += resolved_scaled.h_advance(id) + extra;
        prev = Some((src, id));
    }

    (pen_x - start_x).max(0.0)
}

fn unicode_space_factor(ch: char) -> f32 {
    match ch {
        '\u{200A}' => 0.25,
        '\u{2009}' | '\u{2006}' => 0.35,
        '\u{202F}' => 0.45,
        _ => 1.0,
    }
}

fn resolve_font_for_char<'a>(primary: &'a FontVec, ch: char) -> (FontSource, &'a FontVec) {
    if primary.glyph_id(ch).0 != 0 {
        return (FontSource::Primary, primary);
    }
    for (idx, fb) in fallback_fonts().iter().enumerate() {
        if fb.glyph_id(ch).0 != 0 {
            return (FontSource::Fallback(idx), fb);
        }
    }
    (FontSource::Primary, primary)
}

fn measure_line(font: &FontVec, px: PxScale, line: &str, extra: f32) -> f32 {
    layout_line(font, px, line, 0.0, extra, |_, _, _| {})
}

// ---------------------------------------------------------------------------
// Wallpaper Engine text scripts
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default)]
pub struct LocalTime {
    pub year: i32,
    pub month: u32,   // 1..=12
    pub day: u32,     // 1..=31
    pub weekday: u32, // 0 = Sunday
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

/// Read the local wall clock using libc so we do not pull in a time crate.
pub fn local_time() -> LocalTime {
    unsafe {
        let now = libc::time(std::ptr::null_mut());
        let mut tm: libc::tm = std::mem::zeroed();
        if libc::localtime_r(&now, &mut tm).is_null() {
            return LocalTime::default();
        }
        LocalTime {
            year: tm.tm_year + 1900,
            month: (tm.tm_mon + 1).clamp(1, 12) as u32,
            day: tm.tm_mday.clamp(1, 31) as u32,
            weekday: tm.tm_wday.clamp(0, 6) as u32,
            hour: tm.tm_hour.clamp(0, 23) as u32,
            minute: tm.tm_min.clamp(0, 59) as u32,
            second: tm.tm_sec.clamp(0, 59) as u32,
        }
    }
}

fn prop<'a>(
    props: &'a HashMap<String, serde_json::Value>,
    key: &str,
) -> Option<&'a serde_json::Value> {
    props.get(key)
}

fn prop_bool(props: &HashMap<String, serde_json::Value>, key: &str, default: bool) -> bool {
    match prop(props, key) {
        Some(serde_json::Value::Bool(b)) => *b,
        Some(serde_json::Value::String(s)) => s.eq_ignore_ascii_case("true") || s == "1",
        Some(serde_json::Value::Number(n)) => n.as_f64().unwrap_or(0.0) != 0.0,
        Some(serde_json::Value::Object(o)) => o
            .get("value")
            .map(|v| match v {
                serde_json::Value::Bool(b) => *b,
                serde_json::Value::String(s) => s.eq_ignore_ascii_case("true") || s == "1",
                serde_json::Value::Number(n) => n.as_f64().unwrap_or(0.0) != 0.0,
                _ => default,
            })
            .unwrap_or(default),
        _ => default,
    }
}

fn prop_f32(props: &HashMap<String, serde_json::Value>, key: &str, default: f32) -> f32 {
    match prop(props, key) {
        Some(serde_json::Value::Number(n)) => n.as_f64().unwrap_or(default as f64) as f32,
        Some(serde_json::Value::String(s)) => s.trim().parse().unwrap_or(default),
        Some(serde_json::Value::Object(o)) => o
            .get("value")
            .and_then(|v| match v {
                serde_json::Value::Number(n) => n.as_f64().map(|n| n as f32),
                serde_json::Value::String(s) => s.trim().parse().ok(),
                _ => None,
            })
            .unwrap_or(default),
        _ => default,
    }
}

fn prop_i32(props: &HashMap<String, serde_json::Value>, key: &str, default: i32) -> i32 {
    prop_f32(props, key, default as f32).round() as i32
}

fn prop_str(props: &HashMap<String, serde_json::Value>, key: &str, default: &str) -> String {
    match prop(props, key) {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        Some(serde_json::Value::Bool(b)) => b.to_string(),
        Some(serde_json::Value::Object(o)) => o
            .get("value")
            .map(|v| match v {
                serde_json::Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_else(|| default.to_string()),
        _ => default.to_string(),
    }
}

/// Return the text a WE script would produce, or `None` when the script is
/// something we do not understand (the caller then keeps the authored value).
pub fn eval_script(
    script: &str,
    props: &HashMap<String, serde_json::Value>,
    now: LocalTime,
) -> Option<String> {
    let lower = script.to_ascii_lowercase();

    if lower.contains("getmedia")
        || lower.contains("mediaartist")
        || lower.contains("mediatitle")
        || lower.contains("mediapropertieschanged")
        || lower.contains("shared.trackname")
    {
        // Without an active MPRIS/media backend, return empty string instead of
        // rendering authored placeholders ("Artist Name\nTHIS LAYER NAME HAS TO BE UNIQUE", etc.).
        return Some(String::new());
    }

    if (lower.contains("engine.frametime") || lower.contains("totfps"))
        && lower.contains("refreshinterval")
    {
        if !prop_bool(props, "active", true) {
            return Some(prop_str(props, "disabledText", ""));
        }
        let label = prop_str(props, "label", "FPS: &");
        if label.contains('&') {
            let decimals = (prop_i32(props, "resolution", 1).clamp(1, 3) - 1) as usize;
            let num = if decimals == 0 {
                "60".to_string()
            } else {
                format!("{:.*}", decimals, 60.0f32)
            };
            return Some(label.replace('&', &num));
        }
        return Some(format!("{label} 60"));
    }

    if script.contains("scriptProperties.format")
        && (script.contains("ampmlang") || script.contains("twelve"))
    {
        return Some(format_pattern_date_time(props, now));
    }

    if lower.contains("gethours") && lower.contains("getminutes") {
        return Some(clock_text(script, props, now));
    }

    // Day/night greeting templates (both hour threshold and shared.currentTODState).
    if lower.contains("daytext") && lower.contains("nighttext") {
        let day_start = prop_f32(props, "dayStart", 8.0);
        let night_start = prop_f32(props, "nightStart", 20.0);
        let hour = now.hour as f32;
        let is_day = hour >= day_start && hour < night_start;
        let key = if is_day { "dayText" } else { "nightText" };
        return Some(prop_str(props, key, ""));
    }

    // Chinese/English 8-period time-of-day script (`shichenC` / `shichenY`).
    if script.contains("shichenC") && script.contains("shichenY") {
        let bucket = if now.hour < 2 {
            0
        } else if now.hour < 6 {
            1
        } else if now.hour < 8 {
            2
        } else if now.hour < 11 {
            3
        } else if now.hour < 13 {
            4
        } else if now.hour < 17 {
            5
        } else if now.hour < 20 {
            6
        } else {
            7
        };
        let label = if script.contains("shichenY[") {
            [
                "Before dawn",
                "At night",
                "Morning",
                "Morning",
                "Noon",
                "Afternoon",
                "Evening",
                "Night",
            ][bucket]
        } else {
            ["凌晨", "深夜", "早晨", "上午", "中午", "下午", "傍晚", "晚上"][bucket]
        };
        return Some(label.to_string());
    }

    // Standalone seconds layer (`seconds + 's'`, hidden when `showSeconds`/`showCD` checked).
    if lower.contains("getseconds") && !lower.contains("gethours") && !lower.contains("getminutes") {
        if prop_bool(props, "showSeconds", false) || prop_bool(props, "showCD", false) {
            return Some(String::new());
        }
        return Some(format!("{:02}s", now.second));
    }

    // Standalone AM/PM layer (`meridiem`).
    if lower.contains("gethours")
        && !lower.contains("getminutes")
        && (lower.contains("meridiem") || (lower.contains("'am'") && lower.contains("'pm'")))
    {
        let meridiem = if now.hour < 12 { "AM" } else { "PM" };
        return Some(if script.contains("\\u200A") {
            format!("{}\u{200A}{}", &meridiem[0..1], &meridiem[1..2])
        } else {
            meridiem.to_string()
        });
    }

    // Standalone weekday-only layer (`days[today.getDay()]`).
    if lower.contains("getday") && !lower.contains("getmonth") && !lower.contains("getdate") {
        let days = parse_string_array(script);
        if let Some(name) = days.get(now.weekday as usize) {
            return Some(name.clone());
        }
    }

    if (lower.contains("getday") || lower.contains("getdate")) && lower.contains("getmonth") {
        return Some(date_text(script, props, now));
    }

    None
}

fn extract_braced(raw: &str) -> String {
    if let (Some(start), Some(end)) = (raw.find('{'), raw.rfind('}')) {
        if end > start {
            return raw[start + 1..end].to_string();
        }
    }
    raw.to_string()
}

fn romanize(mut num: u32) -> String {
    if num == 0 {
        return String::new();
    }
    const TABLE: &[(u32, &str)] = &[
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let mut out = String::new();
    for &(val, sym) in TABLE {
        while num >= val {
            out.push_str(sym);
            num -= val;
        }
    }
    out
}

fn format_pattern_date_time(
    props: &HashMap<String, serde_json::Value>,
    now: LocalTime,
) -> String {
    let lang = prop_str(props, "language", "en-US");
    let week_type = prop_str(props, "week", "long");
    let twelve = prop_bool(props, "twelve", false);
    let fmt = prop_str(props, "format", "yyyy/MM/dd hh:mm:ss");

    let wd_idx = (now.weekday as usize).min(6);
    let weekday = match (lang.as_str(), week_type.as_str()) {
        ("zh-TW", "narrow") => ["日", "一", "二", "三", "四", "五", "六"][wd_idx],
        ("zh-TW", "short") => ["週日", "週一", "週二", "週三", "週四", "週五", "週六"][wd_idx],
        ("zh-TW", _) => [
            "星期日",
            "星期一",
            "星期二",
            "星期三",
            "星期四",
            "星期五",
            "星期六",
        ][wd_idx],
        ("ja-JP", "narrow") => ["日", "月", "火", "水", "木", "金", "土"][wd_idx],
        ("ja-JP", "short") => ["日曜", "月曜", "火曜", "水曜", "木曜", "金曜", "土曜"][wd_idx],
        ("ja-JP", _) => [
            "日曜日",
            "月曜日",
            "火曜日",
            "水曜日",
            "木曜日",
            "金曜日",
            "土曜日",
        ][wd_idx],
        (_, "narrow") => ["S", "M", "T", "W", "T", "F", "S"][wd_idx],
        (_, "short") => ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"][wd_idx],
        _ => [
            "Sunday",
            "Monday",
            "Tuesday",
            "Wednesday",
            "Thursday",
            "Friday",
            "Saturday",
        ][wd_idx],
    };

    let ampm = if !twelve {
        ""
    } else {
        let pm = usize::from(now.hour >= 12);
        match lang.as_str() {
            "zh-TW" => ["上午", "下午"][pm],
            "ja-JP" => ["午前", "午後"][pm],
            _ => ["AM", "PM"][pm],
        }
    };

    let hour_val = if twelve {
        match now.hour % 12 {
            0 => 12,
            h => h,
        }
    } else {
        now.hour
    };

    let mut out = String::with_capacity(fmt.len() + 8);
    let chars: Vec<char> = fmt.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '\\' {
            if i + 1 < chars.len() {
                out.push(chars[i + 1]);
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        if matches!(c, 'y' | 'M' | 'd' | 'W' | 'h' | 'm' | 's' | 'S' | 'a') {
            let mut j = i + 1;
            while j < chars.len() && chars[j] == c {
                j += 1;
            }
            let count = j - i;
            match (c, count) {
                ('y', 2) => out.push_str(&format!("{:02}", now.year.rem_euclid(100))),
                ('y', 4) => out.push_str(&format!("{:04}", now.year)),
                ('M', 1) => out.push_str(&now.month.to_string()),
                ('M', 2) => out.push_str(&format!("{:02}", now.month)),
                ('d', 1) => out.push_str(&now.day.to_string()),
                ('d', 2) => out.push_str(&format!("{:02}", now.day)),
                ('W', 1) => out.push_str(weekday),
                ('h', 1) => out.push_str(&hour_val.to_string()),
                ('h', 2) => out.push_str(&format!("{:02}", hour_val)),
                ('m', 1) => out.push_str(&now.minute.to_string()),
                ('m', 2) => out.push_str(&format!("{:02}", now.minute)),
                ('s', 1) => out.push_str(&now.second.to_string()),
                ('s', 2) => out.push_str(&format!("{:02}", now.second)),
                ('S', 1) => out.push('0'),
                ('S', 2) => out.push_str("00"),
                ('S', 3) => out.push_str("000"),
                ('a', 1) => out.push_str(ampm),
                _ => {
                    for _ in 0..count {
                        out.push(c);
                    }
                }
            }
            i = j;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

fn clock_text(script: &str, props: &HashMap<String, serde_json::Value>, now: LocalTime) -> String {
    let use_24h = prop_bool(props, "use24hFormat", true);
    let show_hours = if script.contains("showHours") {
        prop_bool(props, "showHours", true)
    } else {
        true
    };
    let show_minutes = if script.contains("showMinutes") {
        prop_bool(props, "showMinutes", true)
    } else {
        true
    };
    let show_seconds = prop_bool(props, "showSeconds", false);
    let show_ms = script.contains("showMilliseconds") && prop_bool(props, "showMilliseconds", false);
    let is_roman = script.contains("romanNumerals") && prop_bool(props, "romanNumerals", false);

    let raw_delimiter = prop_str(props, "delimiter", ":");
    let delimiter = if script.contains("getDelimiter") {
        extract_braced(&raw_delimiter)
    } else {
        raw_delimiter
    };

    let mut hours = now.hour;
    let is_pm = now.hour >= 12;
    let lowercase_period = script.contains("'pm'") || script.contains("' pm'");
    let period = if !use_24h {
        hours %= 12;
        if hours == 0 {
            hours = 12;
        }
        if lowercase_period {
            if is_pm { "pm" } else { "am" }
        } else if is_pm {
            "PM"
        } else {
            "AM"
        }
    } else {
        ""
    };

    let hair_spaced = script.contains("spacedHours") && script.contains("beforeColon");
    let show_leading_zero = if script.contains("showLeadingZero") {
        use_24h
    } else if !use_24h && script.contains("// hours = hours.toString().padStart") {
        false
    } else {
        true
    };

    let hours_str = if is_roman {
        romanize(hours)
    } else if show_leading_zero {
        format!("{hours:02}")
    } else {
        hours.to_string()
    };
    let minutes_str = if is_roman {
        romanize(now.minute)
    } else {
        format!("{:02}", now.minute)
    };
    let seconds_str = if is_roman {
        romanize(now.second)
    } else {
        format!("{:02}", now.second)
    };

    let vertical_nl =
        script.contains("delimiter + \"\\n\"") || script.contains("delimiter + '\\n'");
    let uses_period = if script.contains("scriptProperties.ampm") {
        !use_24h && prop_bool(props, "ampm", false)
    } else if hair_spaced {
        false
    } else {
        script.contains("timePeriod")
            || script.contains("' AM'")
            || script.contains("\" AM\"")
            || script.contains("let period =")
            || script.contains("' pm'")
            || script.contains("'pm'")
    };
    let wrap_dash = script.contains("value = \"-\" + hours") || script.contains("'-' + hours");

    let mut out = String::new();
    if hair_spaced {
        out = format!("{hours_str}\u{200A}{delimiter}\u{200A}{minutes_str}");
    } else {
        if show_hours {
            out.push_str(&hours_str);
        }
        let sep = if vertical_nl {
            format!("{delimiter}\n")
        } else {
            delimiter.clone()
        };
        if show_minutes {
            out.push_str(&sep);
            out.push_str(&minutes_str);
        }
        if show_seconds {
            out.push_str(&sep);
            out.push_str(&seconds_str);
        }
        if show_ms {
            out.push_str(&delimiter);
            if !is_roman {
                out.push_str("00");
            }
        }
    }

    if !use_24h && uses_period {
        out.push(' ');
        out.push_str(period);
    }
    if wrap_dash {
        out = format!("-{out}-");
    }
    if script.contains("getDelimiter") && script.contains("whitespaces") {
        let ws = extract_braced(&prop_str(props, "whitespaces", ""));
        if !ws.is_empty() {
            let mut spaced = String::new();
            for ch in out.chars() {
                spaced.push(ch);
                spaced.push_str(&ws);
            }
            out = spaced;
        }
    }

    if prop_bool(props, "displayDate", false) {
        out.push('\n');
        let date_str = if prop_bool(props, "useYYYYMMDD", false) {
            format!("{:04}/{:02}/{:02}", now.year, now.month, now.day)
        } else if prop_bool(props, "useDDMMYYYY", false)
            || (script.contains("useDDMMYYYY") && !prop_bool(props, "useMMDDYYYY", false))
        {
            format!("{:02}/{:02}/{:04}", now.day, now.month, now.year)
        } else {
            format!("{:02}/{:02}/{:04}", now.month, now.day, now.year)
        };
        out.push_str(&date_str);
    }
    out
}

fn stack_vertical(s: &str) -> String {
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push(ch);
    }
    out
}

fn date_text(script: &str, props: &HashMap<String, serde_json::Value>, now: LocalTime) -> String {
    let lower = script.to_ascii_lowercase();

    // Spaced day + month without year or weekday (e.g. "2 5 M a y").
    if !lower.contains("getday") && lower.contains("getdate") && lower.contains("getmonth") {
        let months = extract_array(script, "months");
        let month = months
            .get((now.month as usize).saturating_sub(1))
            .cloned()
            .unwrap_or_else(|| now.month.to_string());
        let day_str = now.day.to_string();
        let spaced_day = if script.contains("spacedDay") {
            let mut out = String::new();
            for (i, ch) in day_str.chars().enumerate() {
                if i > 0 {
                    out.push('\u{200A}');
                }
                out.push(ch);
            }
            out
        } else {
            day_str
        };
        return format!("{spaced_day} {month}");
    }

    // Pipe / dash weekday scripts ("YYYY | MM | DD | Weekday", "DD | MM | YYYY | Weekday", "YYYY-MM-DD SUN").
    if script.contains("weekday") && !script.contains("showDay") {
        let weekdays = extract_array(script, "weekday");
        let wd = weekdays
            .get(now.weekday as usize)
            .cloned()
            .unwrap_or_default();
        if script.contains("day + \" | \" + month") || script.contains("day + ' | ' + month") {
            return format!("{:02} | {:02} | {:04} | {wd}", now.day, now.month, now.year);
        }
        if script.contains("\" | \"") || script.contains("' | '") {
            return format!("{:04} | {:02} | {:02} | {wd}", now.year, now.month, now.day);
        }
        return format!("{:04}-{:02}-{:02} {wd}", now.year, now.month, now.day);
    }

    let month_format = prop_i32(props, "monthFormat", 1).clamp(1, 3);
    let day_format = prop_i32(props, "dayFormat", 1).clamp(1, 2);
    let show_day = prop_bool(props, "showDay", false);
    let align_vertical = prop_bool(props, "alignVertical", false);
    let use_delimiter = prop_bool(props, "useDelimiter", false);

    // Vertically stacked character-by-character date preset (`dtt = ['', '0' + newLine + '1', ...]`).
    if script.contains("dtt = [") || script.contains("'0' + newLine + '1'") {
        let sep = if use_delimiter {
            prop_str(props, "addDelimiter", "/")
        } else if align_vertical {
            "\n\n".to_string()
        } else {
            " ".to_string()
        };
        let day_two = format!("{:02}", now.day);
        let month_raw = match month_format {
            1 => format!("{:02}", now.month),
            2 => [
                "JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC",
            ][(now.month as usize).saturating_sub(1).min(11)]
            .to_string(),
            _ => [
                "January",
                "February",
                "March",
                "April",
                "May",
                "June",
                "July",
                "August",
                "September",
                "October",
                "November",
                "December",
            ][(now.month as usize).saturating_sub(1).min(11)]
            .to_string(),
        };
        let wd_short = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"]
            [(now.weekday as usize).min(6)];
        let wd_full = [
            "Sunday",
            "Monday",
            "Tuesday",
            "Wednesday",
            "Thursday",
            "Friday",
            "Saturday",
        ][(now.weekday as usize).min(6)];
        let year_str = format!("{:04}", now.year);

        let (d_out, m_out, y_out, wd_out) = if align_vertical {
            (
                stack_vertical(&day_two),
                if month_format <= 2 {
                    stack_vertical(&month_raw)
                } else {
                    month_raw
                },
                stack_vertical(&year_str),
                if day_format == 1 {
                    stack_vertical(wd_short)
                } else {
                    wd_full.to_string()
                },
            )
        } else {
            (
                day_two,
                month_raw,
                year_str,
                if day_format == 1 {
                    wd_short.to_string()
                } else {
                    wd_full.to_string()
                },
            )
        };

        return if show_day {
            format!("{wd_out}{sep}{d_out}{sep}{m_out}{sep}{y_out}")
        } else {
            format!("{d_out}{sep}{m_out}{sep}{y_out}")
        };
    }

    let months = extract_array_for(script, "months", "monthFormat", month_format);
    let days = extract_array_for(script, "day", "dayFormat", day_format);

    let raw_month = months
        .get((now.month as usize).saturating_sub(1))
        .cloned()
        .unwrap_or_else(|| now.month.to_string());
    let day_name = days
        .get(now.weekday as usize)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();

    // The date presets use `showDay` to choose between a weekday-only output
    // and a full date.  Some Date layers also prepend the weekday.
    let day_only = show_day && is_day_only_branch(script);
    if day_only {
        return day_name;
    }

    let delimiter = if use_delimiter {
        prop_str(props, "addDelimiter", "/")
    } else {
        " ".to_string()
    };
    let month = if delimiter.is_empty() {
        raw_month
    } else {
        raw_month.trim().to_string()
    };
    let date = format!("{}{}{}{}{}", now.day, delimiter, month, delimiter, now.year);

    if !day_name.is_empty() && show_day {
        if align_vertical {
            format!("{day_name}\n{date}")
        } else {
            format!("{day_name} {date}")
        }
    } else {
        date
    }
}

/// Extract a JavaScript string array assigned to `name`, preferring the block
/// guarded by `scriptProperties.<format_prop> == <format>`.
fn extract_array_for(script: &str, name: &str, format_prop: &str, format: i32) -> Vec<String> {
    let needle = format!("{name} = [");
    let marker = format!(".{format_prop} ==");
    let mut found: Option<Vec<String>> = None;
    let mut from = 0usize;
    while let Some(pos) = script[from..].find(&needle) {
        let abs = from + pos;
        let ctx_start = abs.saturating_sub(320);
        let ctx = &script[ctx_start..abs];
        // Use the closest preceding format check, otherwise a later branch's
        // context still contains the earlier `== 2` test.
        if let Some(marker_pos) = ctx.rfind(&marker) {
            let tail = &ctx[marker_pos + marker.len()..];
            let value: i32 = tail
                .trim_start()
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .parse()
                .unwrap_or(-1);
            if value == format {
                found = Some(parse_string_array(&script[abs..]));
                break;
            }
        }
        from = abs + needle.len();
    }
    found.unwrap_or_else(|| extract_array(script, name))
}

/// Extract a JavaScript string array assigned to `name`.
fn extract_array(script: &str, name: &str) -> Vec<String> {
    let needle = format!("{name} = [");
    match script.find(&needle) {
        Some(pos) => parse_string_array(&script[pos..]),
        None => Vec::new(),
    }
}

fn parse_string_array(rest: &str) -> Vec<String> {
    let Some(start) = rest.find('[') else {
        return Vec::new();
    };
    let body = &rest[start..];
    let Some(end) = body.find(']') else {
        return Vec::new();
    };
    let body = &body[..end];
    let mut out = Vec::new();
    let mut chars = body.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\'' || ch == '"' {
            let quote = ch;
            let mut value = String::new();
            while let Some(c) = chars.next() {
                if c == '\\' {
                    if let Some(escaped) = chars.next() {
                        match escaped {
                            'n' => value.push('\n'),
                            't' => value.push('\t'),
                            'r' => value.push('\r'),
                            '0' => value.push('\0'),
                            'u' => {
                                let mut hex = String::with_capacity(4);
                                for _ in 0..4 {
                                    if let Some(&h) = chars.peek() {
                                        if h.is_ascii_hexdigit() {
                                            hex.push(h);
                                            chars.next();
                                        }
                                    }
                                }
                                if let Ok(code) = u32::from_str_radix(&hex, 16) {
                                    if let Some(ch) = char::from_u32(code) {
                                        value.push(ch);
                                    }
                                }
                            }
                            other => value.push(other),
                        }
                    }
                } else if c == quote {
                    break;
                } else {
                    value.push(c);
                }
            }
            out.push(value);
        }
    }
    out
}

fn is_day_only_branch(script: &str) -> bool {
    let Some(true_idx) = script.find("showDay == true") else {
        return false;
    };
    let Some(false_idx) = script[true_idx..].find("showDay == false") else {
        return false;
    };
    let branch = &script[true_idx..true_idx + false_idx];
    match branch.find("return") {
        Some(r) => {
            let tail = &branch[r..];
            // A `+` before the closing brace means a full date expression.
            let end = tail.find('}').unwrap_or(tail.len());
            !tail[..end].contains('+')
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_time() -> LocalTime {
        LocalTime {
            year: 2026,
            month: 11,
            day: 5,
            weekday: 4, // Thursday
            hour: 16,
            minute: 7,
            second: 9,
        }
    }

    fn props(pairs: &[(&str, serde_json::Value)]) -> HashMap<String, serde_json::Value> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect()
    }

    #[test]
    fn eval_media_returns_empty_without_session() {
        let script = "export function mediaPropertiesChanged(event) { mediaData = event.title; } export function update(v) { return mediaData; }";
        assert_eq!(eval_script(script, &props(&[]), sample_time()), Some(String::new()));

        let track = "export function update(value) { let customText = shared.trackName; return value; }";
        assert_eq!(eval_script(track, &props(&[]), sample_time()), Some(String::new()));
    }

    #[test]
    fn eval_fps_scripts() {
        let s1 = "export function update(value) { intervalTime += engine.frametime; if (time - oldTime > scriptProperties.refreshInterval) { value = scriptProperties.label.replace('&', fps.toFixed(scriptProperties.resolution - 1)); } return value; }";
        let p1 = props(&[("active", json!(true)), ("label", json!("FPS: &")), ("resolution", json!(1))]);
        assert_eq!(eval_script(s1, &p1, sample_time()), Some("FPS: 60".to_string()));

        let s2 = "export function update(value) { if (scriptProperties.active) { totFps /= n; if (currentFrame - oldTime >= parseInt(scriptProperties.refreshInterval)) { value = scriptProperties.label + ' ' + Math.round(totFps); } } }";
        let p2 = props(&[("active", json!(true)), ("label", json!("fps:"))]);
        assert_eq!(eval_script(s2, &p2, sample_time()), Some("fps: 60".to_string()));
    }

    #[test]
    fn eval_format_pattern_script() {
        let script = "const weekday = {}; const ampmlang = {}; export function update(e) { let o = { twelve: scriptProperties.twelve }; return scriptProperties.format; }";
        let p_date = props(&[
            ("format", json!("yyyy/MM/dd W")),
            ("language", json!("en-US")),
            ("week", json!("short")),
            ("twelve", json!(false)),
        ]);
        assert_eq!(
            eval_script(script, &p_date, sample_time()),
            Some("2026/11/05 Thu".to_string())
        );
        let p_time = props(&[
            ("format", json!("hh:mm a")),
            ("language", json!("en-US")),
            ("week", json!("long")),
            ("twelve", json!(true)),
        ]);
        assert_eq!(
            eval_script(script, &p_time, sample_time()),
            Some("04:07 PM".to_string())
        );
    }

    #[test]
    fn eval_shichen_seconds_ampm_and_weekday_only() {
        let shichen_c = "let shichenC = {1:'凌晨'}; let shichenY = {1:'Before dawn'}; export function update(v) { switch (t) { case 'xiawu': kong = shichenC[6]; } return kong; }";
        let shichen_y = "let shichenC = {1:'凌晨'}; let shichenY = {1:'Before dawn'}; export function update(v) { switch (t) { case 'xiawu': kong = shichenY[6]; } return kong; }";
        assert_eq!(eval_script(shichen_c, &props(&[]), sample_time()), Some("下午".to_string()));
        assert_eq!(eval_script(shichen_y, &props(&[]), sample_time()), Some("Afternoon".to_string()));

        let sec_script = "export function update(v) { let time = new Date(); let seconds = ('00' + time.getSeconds()).slice(-2) + 's'; return seconds; }";
        assert_eq!(eval_script(sec_script, &props(&[]), sample_time()), Some("09s".to_string()));
        assert_eq!(
            eval_script(sec_script, &props(&[("showSeconds", json!(true))]), sample_time()),
            Some(String::new())
        );

        let ampm_script = "export function update(v) { let time = new Date(); var hours = time.getHours(); const microSpace = '\\u200A'; const meridiem = hours < 12 ? 'AM' : 'PM'; return meridiem.split('').join(microSpace); }";
        assert_eq!(eval_script(ampm_script, &props(&[]), sample_time()), Some("P\u{200A}M".to_string()));

        let day_only = "export function update(v) { let today = new Date(); var days = [\"Sunday\", \"Monday\", \"Tuesday\", \"Wednesday\", \"Thursday\", \"Friday\", \"Saturday\"]; return days[today.getDay()]; }";
        assert_eq!(eval_script(day_only, &props(&[]), sample_time()), Some("Thursday".to_string()));
    }

    #[test]
    fn eval_clock_variants() {
        let braced = "function getDelimiter(s){} export function update(v){ let t = new Date(); let hours = t.getHours(); let minutes = t.getMinutes(); if (!scriptProperties.use24hFormat && scriptProperties.ampm) {} if (scriptProperties.romanNumerals) {} if (scriptProperties.showHours && scriptProperties.showMinutes && whitespaces != '') {} }";
        let p_braced = props(&[
            ("delimiter", json!("{:}")),
            ("whitespaces", json!("{}")),
            ("use24hFormat", json!(true)),
            ("ampm", json!(false)),
            ("romanNumerals", json!(false)),
            ("showHours", json!(true)),
            ("showMinutes", json!(true)),
        ]);
        assert_eq!(eval_script(braced, &p_braced, sample_time()), Some("16:07".to_string()));

        let vertical = "export function update(v) { let t = new Date(); var hours = t.getHours(); var minutes = t.getMinutes(); value = hours + scriptProperties.delimiter + \"\\n\" + minutes; if (scriptProperties.showSeconds) { value += scriptProperties.delimiter + \"\\n\" + seconds; } return value; }";
        let p_vert = props(&[("delimiter", json!("")), ("showSeconds", json!(true)), ("use24hFormat", json!(true))]);
        assert_eq!(eval_script(vertical, &p_vert, sample_time()), Some("16\n07\n09".to_string()));

        let spaced_clock = "export function update(v) { let t = new Date(); var hours = t.getHours(); let minutes = t.getMinutes(); let showLeadingZero = scriptProperties.use24hFormat; let spacedHours = hours; const beforeColon = '\\u200A'; return spacedHours; }";
        let p_spaced = props(&[("delimiter", json!(":")), ("use24hFormat", json!(false))]);
        assert_eq!(eval_script(spaced_clock, &p_spaced, sample_time()), Some("4\u{200A}:\u{200A}07".to_string()));

        let cn_clock = "export function update(v) { let time = new Date(); let hours = time.getHours(); let minutes = time.getMinutes(); let timePeriod = hours >= 12 ? ' pm' : ' am'; if (scriptProperties.useYYYYMMDD) {} else if (scriptProperties.useDDMMYYYY) {} }";
        let p_cn = props(&[
            ("delimiter", json!(":")),
            ("use24hFormat", json!(false)),
            ("displayDate", json!(true)),
            ("useYYYYMMDD", json!(true)),
        ]);
        assert_eq!(eval_script(cn_clock, &p_cn, sample_time()), Some("04:07 pm\n2026/11/05".to_string()));
    }

    #[test]
    fn eval_date_variants() {
        let pipe = "export function update(v) { let time = new Date(); let year = time.getFullYear(); let month = time.getMonth(); let day = time.getDate(); let weekday = [\"星期日\", \"星期一\", \"星期二\", \"星期三\", \"星期四\", \"星期五\", \"星期六\"]; return year + \" | \" + month + \" | \" + day + \" | \" + weekday[time.getDay()]; }";
        assert_eq!(eval_script(pipe, &props(&[]), sample_time()), Some("2026 | 11 | 05 | 星期四".to_string()));

        let spaced_date = "export function update(v) { let time = new Date(); let months = ['J\\u200Aa\\u200An','F\\u200Ae\\u200Ab','M\\u200Aa\\u200Ar','A\\u200Ap\\u200Ar','M\\u200Aa\\u200Ay','J\\u200Au\\u200An','J\\u200Au\\u200Al','A\\u200Au\\u200Ag','S\\u200Ae\\u200Ap','O\\u200Ac\\u200At','N\\u200Ao\\u200Av','D\\u200Ae\\u200Ac']; const day = time.getDate().toString(); const spacedDay = day; return spacedDay + ' ' + months[time.getMonth()]; }";
        assert_eq!(eval_script(spaced_date, &props(&[]), sample_time()), Some("5 N\u{200A}o\u{200A}v".to_string()));

        let stacked = "export function update(v) { dtt = ['', '0' + newLine + '1']; let date = new Date(); date.getDay(); date.getMonth(); }";
        let p_stack = props(&[("alignVertical", json!(true)), ("monthFormat", json!("2")), ("showDay", json!(false)), ("useDelimiter", json!(false))]);
        assert_eq!(
            eval_script(stacked, &p_stack, sample_time()),
            Some("0\n5\n\nN\nO\nV\n\n2\n0\n2\n6".to_string())
        );

        let delim_preset = "export function update(v) { if (scriptProperties.monthFormat == 1) { months = ['1','2','3','4','5','6','7','8','9','10','11','12']; } if (scriptProperties.dayFormat == 1) { day = ['Sun','Mon','Tue','Wed','Thu','Fri','Sat']; } let date = new Date(); if (scriptProperties.showDay == true) { return day[date.getDay()] + ' ' + date.getDate(); } if (scriptProperties.showDay == false) { return date.getDate() + delimiterValue + months[date.getMonth()] + delimiterValue + date.getFullYear(); } }";
        let p_slash = props(&[("useDelimiter", json!(true)), ("addDelimiter", json!("/")), ("showDay", json!(false)), ("monthFormat", json!("1"))]);
        assert_eq!(eval_script(delim_preset, &p_slash, sample_time()), Some("5/11/2026".to_string()));
    }

    #[test]
    fn rasterize_cjk_and_symbols_with_fallback() {
        let pkg = PackageFs::open(&std::env::temp_dir()).expect("open temp pkg");
        let font = load_font(&pkg, "systemfont_consolas").expect("load fallback/mono font");
        let raster = rasterize(
            &font,
            "星期一 下午 ▸",
            32.0,
            2.0,
            [0.0, 0.0],
            HAlign::Center,
            VAlign::Center,
            None,
            0,
            None,
        );
        let non_zero_alpha = raster.pixels.iter().skip(3).step_by(4).filter(|&&a| a > 0).count();
        assert!(non_zero_alpha > 50, "expected CJK/symbol glyphs to draw pixels, got {non_zero_alpha}");
    }
}
