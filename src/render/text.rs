//! Text rasterisation and Wallpaper Engine text-script emulation.
//!
//! Wallpaper Engine text layers are JavaScript snippets that return a string
//! (clock, date, day/night greeting, media metadata or a user property).  We do
//! not execute JavaScript; the update templates shipped with the built-in text
//! presets are recognised and reproduced here.  Everything is rasterised with
//! `ab_glyph` into an RGBA bitmap that the scene renderer uploads as a texture.
//!
//! Font sizing follows Wallpaper Engine: a text layer's font is `pointsize`
//! points at 300 DPI, i.e. `pointsize * 300 / 96` scene units tall.  The layer
//! is then transformed (including its scale) like any other layer.

use std::collections::HashMap;
use std::path::Path;

use ab_glyph::{Font, FontVec, Glyph, PxScale, ScaleFont};
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

/// Scene units per point (300 DPI relative to 96 DPI scene units).
pub const UNITS_PER_POINT: f32 = 300.0 / 96.0;

pub struct RasterizedText {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// Load a font either from the wallpaper package or from the system.
pub fn load_font(pkg: &PackageFs, path: &str) -> Option<FontVec> {
    let path = path.trim();
    if !path.is_empty() && !path.starts_with("systemfont_") {
        for candidate in [path, path.trim_start_matches('/')] {
            if let Ok(bytes) = pkg.read(candidate) {
                if let Ok(font) = FontVec::try_from_vec(bytes) {
                    return Some(font);
                }
            }
            if let Some(we_file) = crate::pkg::resolve_we_asset(candidate) {
                if let Ok(bytes) = std::fs::read(we_file) {
                    if let Ok(font) = FontVec::try_from_vec(bytes) {
                        return Some(font);
                    }
                }
            }
        }
        // Some packages reference fonts relative to the package root or by basename.
        let lower = path.to_ascii_lowercase();
        let base_lower = Path::new(&lower)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(&lower);
        for entry in pkg.pkg_file_names() {
            let el = entry.to_ascii_lowercase();
            if el == lower || el.ends_with(&format!("/{base_lower}")) || el == base_lower {
                if let Ok(bytes) = pkg.read(entry) {
                    if let Ok(font) = FontVec::try_from_vec(bytes) {
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
                            if let Ok(bytes) = std::fs::read(entry.path()) {
                                if let Ok(font) = FontVec::try_from_vec(bytes) {
                                    return Some(font);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Try Wallpaper Engine's bundled fonts first (`NotoSans-Regular.ttf`, `RobotoMono-Regular.ttf`).
    for we_font in ["fonts/NotoSans-Regular.ttf", "fonts/RobotoMono-Regular.ttf"] {
        if let Some(p) = crate::pkg::resolve_we_asset(we_font) {
            if let Ok(bytes) = std::fs::read(p) {
                if let Ok(font) = FontVec::try_from_vec(bytes) {
                    return Some(font);
                }
            }
        }
    }

    // `systemfont_arial` and friends: use a reasonable sans-serif fallback.
    const CANDIDATES: &[&str] = &[
        "/usr/share/fonts/Adwaita/AdwaitaSans-Regular.ttf",
        "/usr/share/fonts/cantarell/Cantarell-VF.otf",
        "/usr/share/fonts/TTF/JetBrainsMonoNerdFont-Regular.ttf",
        "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/TTF/DejaVuSans.ttf",
        "/usr/share/fonts/dejavu/DejaVuSans.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/liberation/LiberationSans-Regular.ttf",
        "/usr/share/fonts/truetype/noto/NotoSans-Regular.ttf",
        "/usr/share/fonts/noto/NotoSans-Regular.ttf",
    ];
    for candidate in CANDIDATES {
        if Path::new(candidate).exists() {
            if let Ok(bytes) = std::fs::read(candidate) {
                if let Ok(font) = FontVec::try_from_vec(bytes) {
                    return Some(font);
                }
            }
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
            if measure_line(&scaled, line, spacing[0]) <= max_w || line.is_empty() {
                wrapped.push(line.clone());
                continue;
            }
            let mut current = String::new();
            for word in line.split_inclusive(' ') {
                if !current.is_empty()
                    && measure_line(&scaled, &format!("{current}{word}"), spacing[0]) > max_w
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
        .map(|l| measure_line(&scaled, l, spacing[0]))
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
        let line_w = measure_line(&scaled, line, spacing[0]);
        let line_x = match h_align {
            HAlign::Left => pad,
            HAlign::Right => w as f32 - pad - line_w,
            HAlign::Center => (w as f32 - line_w) * 0.5,
        };
        let baseline = top + line_idx as f32 * line_height + scaled.ascent();

        let mut pen_x = line_x;
        let mut prev: Option<ab_glyph::GlyphId> = None;
        for ch in line.chars() {
            if ch == ' ' {
                pen_x += scaled.h_advance(scaled.glyph_id(' ')) + spacing[0];
                prev = None;
                continue;
            }
            let id = scaled.glyph_id(ch);
            if let Some(prev_id) = prev {
                pen_x += scaled.kern(prev_id, id);
            }
            let glyph: Glyph = id.with_scale_and_position(px, ab_glyph::point(pen_x, baseline));
            if let Some(outline) = font.outline_glyph(glyph) {
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
            pen_x += scaled.h_advance(id) + spacing[0];
            prev = Some(id);
        }
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
    let scaled = font.as_scaled(px);
    text.split('\n')
        .map(|line| measure_line(&scaled, line, spacing))
        .fold(0.0, f32::max)
}

fn measure_line<F: Font>(scaled: &ab_glyph::PxScaleFont<&F>, line: &str, extra: f32) -> f32 {
    let mut width = 0.0;
    let mut prev: Option<ab_glyph::GlyphId> = None;
    for ch in line.chars() {
        let id = scaled.glyph_id(ch);
        if ch != ' ' {
            if let Some(prev_id) = prev {
                width += scaled.kern(prev_id, id);
            }
            width += scaled.h_advance(id) + extra;
            prev = Some(id);
        } else {
            width += scaled.h_advance(id) + extra;
            prev = None;
        }
    }
    width.max(0.0)
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

    if lower.contains("getmedia") || lower.contains("mediaartist") || lower.contains("mediatitle") {
        // Media title/artist layers have nothing sensible to show without a
        // media session; the authored placeholder is better than nothing.
        return None;
    }

    if lower.contains("gethours") && lower.contains("getminutes") {
        return Some(clock_text(script, props, now));
    }

    // Day/night greeting templates.
    if lower.contains("daytext") && lower.contains("nighttext") {
        let day_start = prop_f32(props, "dayStart", 8.0);
        let night_start = prop_f32(props, "nightStart", 20.0);
        let hour = now.hour as f32;
        let is_day = hour >= day_start && hour < night_start;
        let key = if is_day { "dayText" } else { "nightText" };
        return Some(prop_str(props, key, ""));
    }

    if lower.contains("getday") && lower.contains("getmonth") {
        return Some(date_text(script, props, now));
    }

    None
}

fn clock_text(script: &str, props: &HashMap<String, serde_json::Value>, now: LocalTime) -> String {
    let use_24h = prop_bool(props, "use24hFormat", true);
    let show_seconds = prop_bool(props, "showSeconds", false);
    let delimiter = prop_str(props, "delimiter", ":");

    let mut hours = now.hour;
    let mut period = "";
    if !use_24h {
        period = if now.hour >= 12 { "PM" } else { "AM" };
        hours %= 12;
        if hours == 0 {
            hours = 12;
        }
    }

    let uses_period = script.contains("timePeriod") || script.contains("AM");
    let wrap_dash = script.contains("value = \"-\" + hours") || script.contains("'-' + hours");
    let mut out = format!("{:02}{}{:02}", hours, delimiter, now.minute);
    if show_seconds {
        out.push_str(&format!("{}{:02}", delimiter, now.second));
    }
    if !use_24h && uses_period {
        out.push(' ');
        out.push_str(period);
    }
    if wrap_dash {
        out = format!("-{out}-");
    }

    if prop_bool(props, "displayDate", false) {
        out.push('\n');
        out.push_str(&format!(
            "{:02}/{:02}/{:04}",
            now.month, now.day, now.year
        ));
    }
    out
}

fn date_text(script: &str, props: &HashMap<String, serde_json::Value>, now: LocalTime) -> String {
    let month_format = prop_i32(props, "monthFormat", 1).clamp(1, 3);
    let day_format = prop_i32(props, "dayFormat", 1).clamp(1, 2);
    let show_day = prop_bool(props, "showDay", false);
    let use_delimiter = prop_bool(props, "useDelimiter", false);
    let delimiter = if use_delimiter {
        prop_str(props, "addDelimiter", "/")
    } else {
        " ".to_string()
    };
    let new_line = if prop_bool(props, "alignVertical", false) {
        "\n"
    } else {
        ""
    };

    let months = extract_array_for(script, "months", "monthFormat", month_format);
    let days = extract_array_for(script, "day", "dayFormat", day_format);

    let month = months
        .get((now.month as usize).saturating_sub(1))
        .map(|s| s.trim().to_string())
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

    let date = format!(
        "{} {}{} {}",
        now.day,
        month,
        delimiter.trim(),
        now.year
    );
    let date = date.replace("  ", " ");
    if !day_name.is_empty() && show_day && script.contains("+ ' ' + date.getDate()") {
        format!("{day_name}{new_line} {date}")
    } else if !day_name.is_empty() && show_day {
        format!("{day_name}{new_line} {date}")
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
                        value.push(match escaped {
                            'n' => '\n',
                            't' => '\t',
                            other => other,
                        });
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
