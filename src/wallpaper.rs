use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

/// What kind of renderer a wallpaper needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WallpaperKind {
    Image,
    Video,
    Shader,
    Scene,
    Web,
    Application,
}

impl WallpaperKind {
    pub fn as_str(self) -> &'static str {
        match self {
            WallpaperKind::Image => "image",
            WallpaperKind::Video => "video",
            WallpaperKind::Shader => "shader",
            WallpaperKind::Scene => "scene",
            WallpaperKind::Web => "web",
            WallpaperKind::Application => "application",
        }
    }
}

/// A selectable option for a `"combo"` wallpaper property.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PropertyOption {
    pub label: String,
    pub value: String,
}

/// A user-configurable property exposed by a wallpaper (`project.json` or `wallpaper.toml`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WallpaperProperty {
    pub key: String,
    pub text: String,
    pub prop_type: String,
    pub default_value: String,
    pub min: Option<f64>,
    pub max: Option<f64>,
    #[serde(default)]
    pub step: Option<f64>,
    #[serde(default)]
    pub fraction: bool,
    #[serde(default)]
    pub order: Option<i64>,
    #[serde(default)]
    pub index: Option<i64>,
    #[serde(default)]
    pub options: Vec<PropertyOption>,
    #[serde(default)]
    pub condition: Option<String>,
}

/// A resolved wallpaper. `root` is the folder (or parent) used to resolve
/// relative asset paths; `entry` is the file the renderer consumes.
#[derive(Debug, Clone)]
pub struct Wallpaper {
    pub root: PathBuf,
    pub entry: PathBuf,
    pub kind: WallpaperKind,
    pub title: String,
    pub workshop_id: Option<String>,
    pub preview: Option<PathBuf>,
    pub tags: Vec<String>,
    /// Raw Wallpaper Engine project metadata, when present. Kept so renderers
    /// can read extra properties such as `general.properties`.
    pub project: Option<serde_json::Value>,
    pub properties: Vec<WallpaperProperty>,
}

impl Wallpaper {
    /// Resolve a file, directory, Steam Workshop ID, or wallpaper name.
    pub fn resolve(path: &Path) -> Result<Self> {
        if !path.exists() {
            // Try matching a Steam Workshop ID or a name in standard directories.
            if let Some(s) = path.file_name().and_then(|n| n.to_str()) {
                if let Some(found) = find_by_id_or_name(s) {
                    return Self::resolve(&found);
                }
            }
        }

        let path = path
            .canonicalize()
            .with_context(|| format!("wallpaper not found: {}", path.display()))?;

        if path.is_dir() {
            return Self::from_dir(&path);
        }

        let kind = kind_from_extension(&path)?;
        Ok(Self {
            root: path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| path.clone()),
            entry: path.clone(),
            kind,
            title: path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("wallpaper")
                .to_string(),
            workshop_id: None,
            preview: None,
            tags: Vec::new(),
            project: None,
            properties: default_properties_for_kind(kind),
        })
    }

    fn from_dir(dir: &Path) -> Result<Self> {
        let workshop_id = dir
            .file_name()
            .and_then(|s| s.to_str())
            .filter(|s| s.chars().all(|c| c.is_ascii_digit()))
            .map(str::to_string);
        let preview = find_preview(dir);

        // 1. Native Anima manifest (`wallpaper.toml`).
        let manifest = dir.join("wallpaper.toml");
        if manifest.exists() {
            let text = std::fs::read_to_string(&manifest)?;
            let m: Manifest = toml::from_str(&text)
                .with_context(|| format!("parsing {}", manifest.display()))?;
            let entry = dir.join(&m.file);
            let kind = m.kind.unwrap_or_else(|| {
                kind_from_extension(&entry).unwrap_or(WallpaperKind::Scene)
            });
            let mut props = default_properties_for_kind(kind);
            for (k, v) in m.properties {
                props.push(WallpaperProperty {
                    key: k.clone(),
                    text: k,
                    prop_type: "textinput".into(),
                    default_value: v,
                    min: None,
                    max: None,
                    step: None,
                    fraction: false,
                    order: None,
                    index: None,
                    options: Vec::new(),
                    condition: None,
                });
            }
            sort_properties(&mut props);
            return Ok(Self {
                root: dir.to_path_buf(),
                entry,
                kind,
                title: m.title.unwrap_or_else(|| dir_name(dir)),
                workshop_id,
                preview,
                tags: m.tags,
                project: None,
                properties: props,
            });
        }

        // 2. Native Anima scene (`scene.toml`).
        let scene_toml = dir.join("scene.toml");
        if scene_toml.exists() {
            return Ok(Self {
                root: dir.to_path_buf(),
                entry: scene_toml,
                kind: WallpaperKind::Scene,
                title: dir_name(dir),
                workshop_id,
                preview,
                tags: vec!["scene".into()],
                project: None,
                properties: default_properties_for_kind(WallpaperKind::Scene),
            });
        }

        // 3. Wallpaper Engine `project.json`.
        let project_json = dir.join("project.json");
        if project_json.exists() {
            let text = std::fs::read_to_string(&project_json)?;
            let project: serde_json::Value = serde_json::from_str(&text)
                .with_context(|| format!("parsing {}", project_json.display()))?;
            let ty = project
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            let file = project
                .get("file")
                .and_then(|v| v.as_str())
                .unwrap_or("scene.json");
            let title = project
                .get("title")
                .and_then(|v| v.as_str())
                .map(str::to_string)
                .unwrap_or_else(|| dir_name(dir));

            if ty == "application" {
                bail!("Wallpaper Engine Windows .exe application wallpapers are not supported");
            }

            let entry = dir.join(file);
            let kind = match ty.as_str() {
                "video" => WallpaperKind::Video,
                "web" => WallpaperKind::Web,
                "scene" => WallpaperKind::Scene,
                _ => {
                    if dir.join("scene.pkg").exists() || dir.join("gifscene.pkg").exists() {
                        WallpaperKind::Scene
                    } else {
                        kind_from_extension(&entry).unwrap_or(WallpaperKind::Image)
                    }
                }
            };

            let tags = project
                .get("tags")
                .and_then(|t| t.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();

            let mut properties = extract_we_properties(&project);
            for def in default_properties_for_kind(kind) {
                if !properties.iter().any(|p| p.key.eq_ignore_ascii_case(&def.key)) {
                    properties.push(def);
                }
            }
            sort_properties(&mut properties);

            return Ok(Self {
                root: dir.to_path_buf(),
                entry,
                kind,
                title,
                workshop_id,
                preview,
                tags,
                project: Some(project),
                properties,
            });
        }

        // 4. Unpacked `scene.json` or `scene.pkg` folder without `project.json`.
        for candidate in ["scene.json", "scene.pkg", "gifscene.pkg"] {
            let p = dir.join(candidate);
            if p.exists() {
                return Ok(Self {
                    root: dir.to_path_buf(),
                    entry: p,
                    kind: WallpaperKind::Scene,
                    title: dir_name(dir),
                    workshop_id,
                    preview,
                    tags: vec!["scene".into()],
                    project: None,
                    properties: default_properties_for_kind(WallpaperKind::Scene),
                });
            }
        }

        // 5. Folder with a single supported media file.
        let mut candidates = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_file() && kind_from_extension(&p).is_ok() {
                    let fname = p
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    if fname != "preview" && fname != "thumb" {
                        candidates.push(p);
                    }
                }
            }
        }
        candidates.sort();
        if let Some(entry) = candidates.into_iter().next() {
            let kind = kind_from_extension(&entry)?;
            return Ok(Self {
                root: dir.to_path_buf(),
                entry,
                kind,
                title: dir_name(dir),
                workshop_id,
                preview,
                tags: Vec::new(),
                project: None,
                properties: default_properties_for_kind(kind),
            });
        }

        bail!(
            "no wallpaper found in {} (expected project.json, wallpaper.toml, scene.pkg or a media file)",
            dir.display()
        )
    }

    /// Human readable one line description used by `anima status`.
    pub fn describe(&self) -> String {
        format!(
            "{} [{}] {}",
            self.title,
            self.kind.as_str(),
            self.entry.display()
        )
    }
}

fn def_prop(
    key: &str,
    text: &str,
    prop_type: &str,
    default_value: &str,
    order: i64,
) -> WallpaperProperty {
    WallpaperProperty {
        key: key.into(),
        text: text.into(),
        prop_type: prop_type.into(),
        default_value: default_value.into(),
        min: None,
        max: None,
        step: None,
        fraction: false,
        order: Some(order),
        index: None,
        options: Vec::new(),
        condition: None,
    }
}

fn def_slider(
    key: &str,
    text: &str,
    default_value: &str,
    min: f64,
    max: f64,
    step: f64,
    order: i64,
) -> WallpaperProperty {
    WallpaperProperty {
        min: Some(min),
        max: Some(max),
        step: Some(step),
        fraction: true,
        ..def_prop(key, text, "slider", default_value, order)
    }
}

fn default_properties_for_kind(kind: WallpaperKind) -> Vec<WallpaperProperty> {
    let tr = is_turkish_locale();
    match kind {
        WallpaperKind::Image => vec![
            WallpaperProperty {
                options: vec![
                    PropertyOption {
                        label: if tr { "Kapla" } else { "Cover" }.into(),
                        value: "cover".into(),
                    },
                    PropertyOption {
                        label: if tr { "Sığdır" } else { "Contain" }.into(),
                        value: "contain".into(),
                    },
                    PropertyOption {
                        label: if tr { "Uzat" } else { "Stretch" }.into(),
                        value: "stretch".into(),
                    },
                ],
                ..def_prop(
                    "fit",
                    if tr { "Ölçekleme modu" } else { "Scaling mode" },
                    "combo",
                    "cover",
                    -84,
                )
            },
            def_slider("brightness", if tr { "Parlaklık" } else { "Brightness" }, "0.0", -1.0, 1.0, 0.02, -75),
            def_slider("contrast", if tr { "Karşıtlık" } else { "Contrast" }, "1.0", 0.0, 3.0, 0.02, -74),
            def_slider("saturation", if tr { "Canlılık" } else { "Saturation" }, "1.0", 0.0, 3.0, 0.02, -73),
            def_slider("hue", if tr { "Ton geçişi" } else { "Hue Shift" }, "0.0", -180.0, 180.0, 1.0, -72),
        ],
        WallpaperKind::Video => vec![
            def_slider("speed", if tr { "Oynatma hızı" } else { "Playback speed" }, "1.0", 0.1, 4.0, 0.05, -90),
            def_slider("volume", if tr { "Ses düzeyi" } else { "Audio volume" }, "0.0", 0.0, 1.0, 0.05, -86),
            def_prop("mute", if tr { "Sesi kapat" } else { "Mute audio" }, "bool", "true", -85),
            def_slider("ui_brightness", if tr { "Parlaklık" } else { "Brightness" }, "0.0", -1.0, 1.0, 0.02, -75),
            def_slider("ui_contrast", if tr { "Karşıtlık" } else { "Contrast" }, "1.0", 0.0, 3.0, 0.02, -74),
            def_slider("ui_saturation", if tr { "Canlılık" } else { "Saturation" }, "1.0", 0.0, 3.0, 0.02, -73),
            def_slider("ui_hue", if tr { "Ton geçişi" } else { "Hue Shift" }, "0.0", -180.0, 180.0, 1.0, -72),
        ],
        WallpaperKind::Shader => vec![
            def_slider("speed", if tr { "Animasyon hızı" } else { "Animation speed" }, "1.0", 0.0, 5.0, 0.05, -90),
            def_prop("schemecolor", if tr { "Şema rengi" } else { "Scheme Color" }, "color", "0.35 0.65 1.0", -80),
        ],
        WallpaperKind::Scene => vec![
            def_slider("speed", if tr { "Oynatma hızı" } else { "Playback speed" }, "1.0", 0.0, 5.0, 0.05, -90),
            def_slider("parallax", if tr { "Paralaks gücü" } else { "Parallax strength" }, "1.0", 0.0, 3.0, 0.05, -88),
            def_slider("volume", if tr { "Ses düzeyi" } else { "Audio volume" }, "0.0", 0.0, 1.0, 0.05, -86),
            def_prop("mute", if tr { "Sesi kapat" } else { "Mute audio" }, "bool", "true", -85),
            def_prop("schemecolor", if tr { "Şema rengi" } else { "Scheme Color" }, "color", "0.05 0.06 0.09", -80),
            def_slider("ui_brightness", if tr { "Parlaklık" } else { "Brightness" }, "0.0", -1.0, 1.0, 0.02, -75),
            def_slider("ui_contrast", if tr { "Karşıtlık" } else { "Contrast" }, "1.0", 0.0, 3.0, 0.02, -74),
            def_slider("ui_saturation", if tr { "Canlılık" } else { "Saturation" }, "1.0", 0.0, 3.0, 0.02, -73),
            def_slider("ui_hue", if tr { "Ton geçişi" } else { "Hue Shift" }, "0.0", -180.0, 180.0, 1.0, -72),
        ],
        WallpaperKind::Web => vec![
            def_prop("schemecolor", if tr { "Vurgu rengi" } else { "Accent color" }, "color", "0.22 0.62 0.98", -80),
            def_prop("clock", if tr { "Dijital saat HUD göster" } else { "Show digital clock HUD" }, "bool", "true", -69),
            def_prop("bars", if tr { "64 bant ses spektrumu göster" } else { "Show 64-band audio spectrum visualizer" }, "bool", "true", -68),
        ],
        WallpaperKind::Application => Vec::new(),
    }
}

fn system_locale_tags() -> (String, String) {
    let raw_lang = std::env::var("LC_ALL")
        .or_else(|_| std::env::var("LC_MESSAGES"))
        .or_else(|_| std::env::var("LANG"))
        .unwrap_or_default();
    let norm = raw_lang
        .split(['.', '@'])
        .next()
        .unwrap_or("")
        .replace('_', "-")
        .to_ascii_lowercase();
    let prefix = norm.split('-').next().unwrap_or("").to_string();
    (norm, prefix)
}

fn is_turkish_locale() -> bool {
    system_locale_tags().1 == "tr"
}

fn sort_properties(props: &mut [WallpaperProperty]) {
    props.sort_by(|a, b| {
        a.order
            .unwrap_or(i64::MAX / 2)
            .cmp(&b.order.unwrap_or(i64::MAX / 2))
            .then_with(|| a.index.unwrap_or(i64::MAX / 2).cmp(&b.index.unwrap_or(i64::MAX / 2)))
            .then_with(|| a.key.to_lowercase().cmp(&b.key.to_lowercase()))
    });
}

fn insert_locale_map(out: &mut BTreeMap<String, String>, map: &serde_json::Map<String, serde_json::Value>) {
    for (k, v) in map {
        if let Some(s) = v.as_str() {
            out.insert(k.clone(), s.to_string());
        }
    }
}

/// Build a localized string lookup table from Wallpaper Engine `general.localization`.
pub fn resolve_we_localization(project: &serde_json::Value) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Some(loc) = project
        .get("general")
        .and_then(|g| g.get("localization"))
        .and_then(|l| l.as_object())
    else {
        return out;
    };

    // Detect system locale preference (e.g. "tr_TR.UTF-8" -> "tr-tr" / "tr").
    let (norm, prefix) = system_locale_tags();

    // 1. Seed with first available locale as last-resort fallback.
    if let Some((_, first)) = loc.iter().next() {
        if let Some(m) = first.as_object() {
            insert_locale_map(&mut out, m);
        }
    }
    // 2. Overlay English variants if present.
    for en_key in ["en-gb", "en", "en-us"] {
        for (k, v) in loc {
            if k.eq_ignore_ascii_case(en_key) {
                if let Some(m) = v.as_object() {
                    insert_locale_map(&mut out, m);
                }
            }
        }
    }
    // 3. Overlay matching language prefix, then exact locale.
    if !prefix.is_empty() && prefix != "c" && prefix != "posix" {
        for (k, v) in loc {
            let kl = k.to_ascii_lowercase();
            if kl == prefix || kl.starts_with(&format!("{prefix}-")) {
                if let Some(m) = v.as_object() {
                    insert_locale_map(&mut out, m);
                }
            }
        }
        if !norm.is_empty() {
            for (k, v) in loc {
                if k.eq_ignore_ascii_case(&norm) {
                    if let Some(m) = v.as_object() {
                        insert_locale_map(&mut out, m);
                    }
                }
            }
        }
    }
    out
}

/// Strip HTML markup and decode entities from Wallpaper Engine property labels.
pub fn clean_we_label(raw: &str) -> String {
    // Normalize common break/paragraph tags into newlines before stripping HTML tags.
    let mut with_breaks = String::with_capacity(raw.len());
    let lower = raw.to_ascii_lowercase();
    let mut i = 0usize;
    while i < raw.len() {
        let rest_lower = &lower[i..];
        let br_len = if rest_lower.starts_with("<br/>") {
            Some(5)
        } else if rest_lower.starts_with("<br />") {
            Some(6)
        } else if rest_lower.starts_with("<br>") || rest_lower.starts_with("</p>") || rest_lower.starts_with("<hr>") {
            Some(4)
        } else if rest_lower.starts_with("<hr/>") {
            Some(5)
        } else {
            None
        };
        if let Some(len) = br_len {
            with_breaks.push('\n');
            i += len;
        } else if let Some(ch) = raw[i..].chars().next() {
            with_breaks.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }

    // Strip all `<...>` tags cleanly without touching UTF-8 boundaries.
    let mut stripped = String::with_capacity(with_breaks.len());
    let mut in_tag = false;
    for ch in with_breaks.chars() {
        if ch == '<' {
            in_tag = true;
        } else if ch == '>' {
            in_tag = false;
        } else if !in_tag {
            stripped.push(ch);
        }
    }

    // Decode common named & numeric HTML entities and normalize fancy Unicode styling.
    let decoded = normalize_fancy_unicode(&decode_html_entities(&stripped));

    // Collapse whitespace per line and drop blank lines.
    let lines: Vec<String> = decoded
        .lines()
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|l| !l.is_empty())
        .collect();
    lines.join("\n")
}

/// Normalize mathematical bold/italic/script Latin symbols, fullwidth ASCII,
/// and filler spaces into plain printable characters.
pub fn normalize_fancy_unicode(s: &str) -> String {
    const BLOCKS: &[(u32, u32, u8)] = &[
        (0x1D400, 26, b'A'), (0x1D41A, 26, b'a'),
        (0x1D434, 26, b'A'), (0x1D44E, 26, b'a'),
        (0x1D468, 26, b'A'), (0x1D482, 26, b'a'),
        (0x1D49C, 26, b'A'), (0x1D4B6, 26, b'a'),
        (0x1D4D0, 26, b'A'), (0x1D4EA, 26, b'a'),
        (0x1D504, 26, b'A'), (0x1D51E, 26, b'a'),
        (0x1D538, 26, b'A'), (0x1D552, 26, b'a'),
        (0x1D56C, 26, b'A'), (0x1D586, 26, b'a'),
        (0x1D5A0, 26, b'A'), (0x1D5BA, 26, b'a'),
        (0x1D5D4, 26, b'A'), (0x1D5EE, 26, b'a'),
        (0x1D608, 26, b'A'), (0x1D622, 26, b'a'),
        (0x1D63C, 26, b'A'), (0x1D656, 26, b'a'),
        (0x1D670, 26, b'A'), (0x1D68A, 26, b'a'),
        (0x1D7CE, 10, b'0'), (0x1D7D8, 10, b'0'),
        (0x1D7E2, 10, b'0'), (0x1D7EC, 10, b'0'),
        (0x1D7F6, 10, b'0'),
    ];
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        let cp = ch as u32;
        let mut mapped = None;
        for &(base, count, ascii_start) in BLOCKS {
            if cp >= base && cp < base + count {
                mapped = Some((ascii_start + (cp - base) as u8) as char);
                break;
            }
        }
        if let Some(c) = mapped {
            out.push(c);
            continue;
        }
        match ch {
            '\u{210E}' => out.push('h'),
            '\u{FF01}'..='\u{FF5E}' => {
                out.push(((cp - 0xFF01 + 0x21) as u8) as char);
            }
            '\u{3000}' | '\u{3164}' => out.push(' '),
            '、' => out.push_str(", "),
            _ => out.push(ch),
        }
    }
    out
}

fn decode_html_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let after = &rest[amp..];
        if let Some(semi) = after.find(';').filter(|&idx| idx <= 12) {
            let ent = &after[..=semi];
            let replacement = match ent.to_ascii_lowercase().as_str() {
                "&nbsp;" => Some(" ".to_string()),
                "&amp;" => Some("&".to_string()),
                "&lt;" => Some("<".to_string()),
                "&gt;" => Some(">".to_string()),
                "&quot;" => Some("\"".to_string()),
                "&#39;" | "&apos;" => Some("'".to_string()),
                _ if ent.starts_with("&#x") || ent.starts_with("&#X") => {
                    u32::from_str_radix(&ent[3..ent.len() - 1], 16)
                        .ok()
                        .and_then(char::from_u32)
                        .map(|c| c.to_string())
                }
                _ if ent.starts_with("&#") => ent[2..ent.len() - 1]
                    .parse::<u32>()
                    .ok()
                    .and_then(char::from_u32)
                    .map(|c| c.to_string()),
                _ => None,
            };
            if let Some(rep) = replacement {
                out.push_str(&rep);
                rest = &after[semi + 1..];
                continue;
            }
        }
        // Handle bare `&nbsp` without trailing semicolon common in Workshop properties.
        if after.get(..5).is_some_and(|p| p.eq_ignore_ascii_case("&nbsp")) {
            out.push(' ');
            rest = &after[5..];
            continue;
        }
        out.push('&');
        rest = &after[1..];
    }
    out.push_str(rest);
    out
}

fn lookup_localized<'a>(loc: &'a BTreeMap<String, String>, key: &str) -> Option<&'a str> {
    if let Some(v) = loc.get(key) {
        return Some(v.as_str());
    }
    loc.iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(key))
        .map(|(_, v)| v.as_str())
}

struct GlobalWeLocale {
    is_tr: bool,
    key_map: HashMap<String, String>,
    cjk_map: HashMap<String, String>,
}

static GLOBAL_WE_LOCALE: OnceLock<GlobalWeLocale> = OnceLock::new();

fn global_we_locale() -> &'static GlobalWeLocale {
    GLOBAL_WE_LOCALE.get_or_init(GlobalWeLocale::load)
}

fn find_we_locale_dir() -> Option<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(xdg) = std::env::var_os("XDG_DATA_HOME").map(PathBuf::from) {
        candidates.push(xdg.join("Steam/steamapps/common/wallpaper_engine/locale"));
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        candidates.push(home.join(".local/share/Steam/steamapps/common/wallpaper_engine/locale"));
        candidates.push(home.join(".steam/steam/steamapps/common/wallpaper_engine/locale"));
        candidates.push(
            home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam/steamapps/common/wallpaper_engine/locale"),
        );
    }
    candidates.into_iter().find(|p| p.is_dir())
}

fn load_locale_json(path: &Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let Ok(raw) = std::fs::read_to_string(path) else {
        return out;
    };
    let trimmed = raw.trim_start_matches('\u{feff}');
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(trimmed) else {
        return out;
    };
    for (k, v) in map {
        if let Some(s) = v.as_str() {
            let cleaned = s.replace("&&", "&");
            out.insert(k, cleaned);
        }
    }
    out
}

impl GlobalWeLocale {
    fn load() -> Self {
        let (norm, prefix) = system_locale_tags();
        let is_tr = prefix == "tr";
        let mut key_map = HashMap::new();

        // Seed common `ui_browse_properties_*` fallback strings so tokens resolve
        // cleanly even if Wallpaper Engine is not installed locally.
        const BUILTIN_UI_KEYS: &[(&str, &str, &str)] = &[
            ("ui_browse_properties_scheme_color", "Şema rengi", "Scheme color"),
            ("ui_browse_properties_accent_color", "Vurgu rengi", "Accent color"),
            ("ui_browse_properties_playback_rate", "Oynatma hızı", "Playback rate"),
            ("ui_browse_properties_brightness", "Parlaklık", "Brightness"),
            ("ui_browse_properties_contrast", "Karşıtlık", "Contrast"),
            ("ui_browse_properties_saturation", "Canlılık", "Saturation"),
            ("ui_browse_properties_hue_shift", "Ton geçişi", "Hue shift"),
            ("ui_browse_properties_volume", "Ses düzeyi", "Volume"),
            ("ui_browse_properties_alignment", "Hizalama", "Alignment"),
            ("ui_browse_properties_alignment_cover", "Kapla", "Cover"),
            ("ui_browse_properties_alignment_center", "Ortala", "Center"),
            ("ui_browse_properties_alignment_fill", "Doldur", "Fill"),
            ("ui_browse_properties_alignment_stretch", "Uzat", "Stretch"),
            ("ui_browse_properties_alignment_free", "Serbest", "Free"),
            ("ui_browse_properties_mouse_parallax", "Fare paralaksı", "Mouse parallax"),
            ("ui_browse_properties_parallax", "Harekete tepki", "React to motion"),
            ("ui_browse_properties_parallax_strength", "Hareket gücü", "Motion strength"),
            ("ui_browse_properties_glow", "Parıltı", "Glow"),
            ("ui_browse_properties_background_color", "Arka plan rengi", "Background color"),
            ("ui_browse_properties_count", "Sayı", "Count"),
            ("ui_browse_properties_style", "Stil", "Style"),
            ("ui_browse_properties_shading", "Gölgelendirme", "Shading"),
            ("ui_browse_properties_noise_fx", "Gürültü efektleri", "Noise effects"),
            ("ui_browse_properties_show_color_options", "Renk seçeneklerini göster", "Show color options"),
            ("ui_yes", "Evet", "Yes"),
            ("ui_no", "Hayır", "No"),
        ];
        for &(k, tr, en) in BUILTIN_UI_KEYS {
            key_map.insert(k.to_string(), if is_tr { tr } else { en }.to_string());
        }

        let mut cjk_map = HashMap::new();
        if let Some(dir) = find_we_locale_dir() {
            for fname in ["core_en-us.json", "ui_en-us.json"] {
                for (k, v) in load_locale_json(&dir.join(fname)) {
                    key_map.insert(k.to_ascii_lowercase(), v);
                }
            }
            if !prefix.is_empty() && !matches!(prefix.as_str(), "c" | "posix" | "en") {
                for kind in ["core", "ui"] {
                    let mut candidates: Vec<PathBuf> = Vec::new();
                    if let Ok(rd) = std::fs::read_dir(&dir) {
                        let pfx = format!("{kind}_{prefix}");
                        for ent in rd.flatten() {
                            let name = ent.file_name();
                            let ns = name.to_string_lossy().to_ascii_lowercase();
                            if ns.starts_with(&pfx) && ns.ends_with(".json") {
                                candidates.push(ent.path());
                            }
                        }
                    }
                    candidates.sort();
                    // Apply generic prefix match first, then exact `<kind>_<norm>.json` last.
                    let exact = dir.join(format!("{kind}_{norm}.json"));
                    for p in candidates {
                        if p != exact {
                            for (k, v) in load_locale_json(&p) {
                                key_map.insert(k.to_ascii_lowercase(), v);
                            }
                        }
                    }
                    if exact.exists() {
                        for (k, v) in load_locale_json(&exact) {
                            key_map.insert(k.to_ascii_lowercase(), v);
                        }
                    }
                }
            }

            // Build reverse CJK -> active-locale dictionary from official WE locales.
            // Prefer `ui_browse_properties_*` keys over generic dialog strings.
            let cjk_files = [
                "ui_zh-chs.json",
                "ui_zh-cht.json",
                "ui_ja-jp.json",
                "core_zh-chs.json",
                "core_zh-cht.json",
                "core_ja-jp.json",
            ];
            for prefer_prop_keys in [true, false] {
                for fname in cjk_files {
                    for (k, cjk_val) in load_locale_json(&dir.join(fname)) {
                        let kl = k.to_ascii_lowercase();
                        if prefer_prop_keys != kl.starts_with("ui_browse_properties_") {
                            continue;
                        }
                        if !contains_cjk(&cjk_val) || cjk_val.contains("{{") {
                            continue;
                        }
                        let Some(target_val) = key_map.get(&kl) else {
                            continue;
                        };
                        if target_val.contains("{{") || contains_cjk(target_val) {
                            continue;
                        }
                        let lookup = clean_we_label(&cjk_val);
                        let rep = clean_we_label(target_val);
                        if !lookup.is_empty() && !rep.is_empty() {
                            cjk_map.entry(lookup).or_insert(rep);
                        }
                    }
                }
            }
        }

        Self {
            is_tr,
            key_map,
            cjk_map,
        }
    }

    fn lookup_key(&self, key: &str) -> Option<&str> {
        self.key_map.get(&key.trim().to_ascii_lowercase()).map(String::as_str)
    }

    fn lookup_cjk(&self, text: &str) -> Option<&str> {
        self.cjk_map.get(text.trim()).map(String::as_str)
    }
}

pub fn is_cjk_char(ch: char) -> bool {
    matches!(
        ch as u32,
        0x2E80..=0x2EFF
            | 0x3040..=0x30FF
            | 0x31C0..=0x31EF
            | 0x31F0..=0x31FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xF900..=0xFAFF
            | 0xFF66..=0xFF9F
    )
}

pub fn contains_cjk(s: &str) -> bool {
    s.chars().any(is_cjk_char)
}

/// Translate Wallpaper Engine property/option labels (global `ui_*` tokens,
/// official WE CJK translations, bilingual labels, and Chinese/Japanese texts)
/// into Turkish (when system locale is `tr`) or English.
pub fn translate_we_label(cleaned: &str) -> String {
    translate_we_label_with_lang(cleaned, is_turkish_locale(), true)
}

fn translate_we_label_with_lang(cleaned: &str, is_tr: bool, allow_global_cjk: bool) -> String {
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    if (trimmed.starts_with("ui_") || trimmed.starts_with("core_")) && !trimmed.contains(' ') {
        if let Some(resolved) = global_we_locale().lookup_key(trimmed) {
            let cleaned_res = clean_we_label(resolved);
            if !cleaned_res.is_empty() && !contains_cjk(&cleaned_res) {
                return cleaned_res;
            }
        }
    }
    if !contains_cjk(trimmed) {
        return trimmed.to_string();
    }

    // Peel common leading bullet icon cleanly before lookup.
    let (prefix_icon, body) = if let Some(rest) = trimmed.strip_prefix('🔘') {
        ("🔘 ", rest.trim())
    } else {
        ("", trimmed)
    };

    let norm_body = body
        .replace('【', "[")
        .replace('】', "]")
        .replace("。", ". ")
        .replace('・', "·");
    let norm_body = norm_body.trim();

    if let Some(exact) = lookup_exact_cjk(norm_body, is_tr) {
        return format!("{prefix_icon}{exact}");
    }
    if allow_global_cjk {
        let gl = global_we_locale();
        if gl.is_tr == is_tr {
            if let Some(gl_hit) = gl.lookup_cjk(norm_body) {
                return format!("{prefix_icon}{gl_hit}");
            }
        }
    }

    // Handle bilingual separator labels (`CJK/English`, `English | CJK`, `CJK(English)`).
    if let Some(bilingual) = try_translate_bilingual(norm_body, is_tr, allow_global_cjk) {
        return format!("{prefix_icon}{bilingual}");
    }

    let translated = translate_cjk_phrases(norm_body, is_tr);
    format!("{prefix_icon}{translated}")
}

fn lookup_exact_cjk(s: &str, is_tr: bool) -> Option<String> {
    // Curated exact matches for option values, banners, notices, creator credits,
    // and common bilingual/idiomatic labels that read better with natural phrasing.
    const EXACT: &[(&str, &str, &str)] = &[
        // Common dropdown options
        ("关闭", "Kapalı", "Off"),
        ("开启", "Açık", "On"),
        ("不显示", "Gizli", "Hidden"),
        ("开 Open", "Açık", "Open"),
        ("关 Close", "Kapalı", "Close"),
        ("off 关", "Kapalı", "Off"),
        ("on 开", "Açık", "On"),
        ("pulse 闪", "Nabız", "Pulse"),
        ("高光", "Parlama", "Highlight"),
        ("阴影", "Gölge", "Shadow"),
        ("波形", "Dalga Formu", "Waveform"),
        ("柱形", "Sütun", "Bars"),
        ("简略", "Basit", "Simple"),
        ("精细", "Detaylı", "Detailed"),
        ("纯色", "Düz Renk", "Solid Color"),
        ("媒体集成", "Medya Entegrasyonu", "Media Integration"),
        ("合影CG背景", "CG Grup Arka Planı", "Group CG Background"),
        ("原背景&自定义背景", "Varsayılan ve Özel Arka Plan", "Default & Custom Background"),
        ("随音乐变化", "Müziğe Göre Değiş", "Reactive to Music"),
        ("正常渐变", "Normal Geçiş", "Smooth Gradient"),
        ("字体1", "Yazı Tipi 1", "Font 1"),
        ("字体2", "Yazı Tipi 2", "Font 2"),
        ("字体3", "Yazı Tipi 3", "Font 3"),
        ("底部 bottom", "Alt", "Bottom"),
        ("顶部 top", "Üst", "Top"),
        ("左 left", "Sol", "Left"),
        ("右 right", "Sağ", "Right"),

        // Section banners
        ("🔻🔻主要设置🔻🔻", "⚙️ Ana Ayarlar", "⚙️ Main Settings"),
        ("⚙️主要设置⚙️ ⚙️Main settings⚙️", "⚙️ Ana Ayarlar", "⚙️ Main Settings"),
        ("⚡音频响应⚡", "⚡ Ses Tepkisi", "⚡ Audio Reactivity"),
        ("-🔻背景设置🔻-", "🖼️ Arka Plan Ayarları", "🖼️ Background Settings"),
        ("-🔻组件设置🔻-", "🧩 Bileşen Ayarları", "🧩 Component Settings"),
        ("-🌐组件设置🌐- 🌐 Component Settings 🌐", "🌐 Bileşen Ayarları", "🌐 Component Settings"),
        ("-🔻流萤设置🔻-", "✨ Firefly Ayarları", "✨ Firefly Settings"),
        ("-🔻鼠标尾迹设置🔻-", "🖱️ İmleç İzi Ayarları", "🖱️ Cursor Trail Settings"),
        ("-🔻鼠标设置🔻- 🔻 Mouse settings🔻", "🖱️ Fare Ayarları", "🖱️ Mouse Settings"),
        ("-🔻时间组件设置🔻-", "⏰ Saat Bileşeni Ayarları", "⏰ Time Component Settings"),
        ("-⏰时间组件设置⏰- ⏰Time component settings⏰", "⏰ Saat Bileşeni Ayarları", "⏰ Time Component Settings"),
        ("-🔻音频感应设置🔻-", "🔊 Ses Tepki Ayarları", "🔊 Audio Reactivity Settings"),
        ("-🔊音频感应设置🔊- 🔻Audio induction settings🔻", "🔊 Ses Tepki Ayarları", "🔊 Audio Reactivity Settings"),
        ("-🔻音乐🔻-", "🎵 Müzik", "🎵 Music"),
        ("-♪♪音乐♪♪-", "🎵 Müzik", "🎵 Music"),
        ("-🔻关于赞助🔻-", "☕ Destek ve Sponsorluk", "☕ Support & Sponsors"),
        ("🎚️全局 Global", "🎚️ Genel Ayarlar", "🎚️ Global Settings"),
        ("设置", "Ayarlar", "Settings"),

        // Specific idiomatic & workshop labels
        ("Eye movement 眼睛移动力度大小", "Göz Hareketi Gücü", "Eye Movement Strength"),
        ("screen shaking 镜头抖动", "Kamera Titremesi", "Screen Shaking"),
        ("color of date and time 时间和日期颜色", "Tarih ve Saat Rengi", "Date and Time Color"),
        ("snow 雪花 1", "Kar Tanesi 1", "Snow 1"),
        ("snow 雪花 2", "Kar Tanesi 2", "Snow 2"),
        ("date 日期 1", "Tarih 1", "Date 1"),
        ("date 日期 2", "Tarih 2", "Date 2"),
        ("time 时间 1", "Saat 1", "Time 1"),
        ("time 时间 2", "Saat 2", "Time 2"),
        ("斩杀后续", "Bitiriş Sahnesi", "Execution Aftermath"),
        ("纸片人", "Karakter Katmanı", "Character Cutout"),
        ("封面", "Kapak", "Cover"),
        ("标题", "Başlık", "Title"),
        ("鸟", "Kuşlar", "Birds"),
        ("翅膀 wing", "Kanatlar", "Wings"),
        ("箭 arrow", "Ok", "Arrow"),
        ("遮罩 Cutout mask", "Kesim Maskesi", "Cutout Mask"),
        ("音频线", "Ses Çizgisi", "Audio Line"),
        ("粒子", "Parçacıklar", "Particles"),
        ("色差", "Renk Sapması", "Chromatic Aberration"),
        ("色差/Color difference", "Renk Sapması", "Chromatic Aberration"),
        ("音圈", "Ses Halkası", "Audio Ring"),
        ("音圈开关", "Ses Halkası", "Audio Ring"),
        ("音圈颜色", "Ses Halkası Rengi", "Audio Ring Color"),
        ("音圈亮度", "Ses Halkası Parlaklığı", "Audio Ring Brightness"),
        ("音圈位置 X", "Ses Halkası Konumu X", "Audio Ring Position X"),
        ("音圈位置 Y", "Ses Halkası Konumu Y", "Audio Ring Position Y"),
        ("音频条/Audio bar", "Ses Çubuğu", "Audio Bar"),
        ("音频条Audio bar", "Ses Çubuğu", "Audio Bar"),
        ("音频条 Music Bar", "Ses Çubuğu", "Audio Bar"),
        ("音频条开关Audio strip", "Ses Çubuğu", "Audio Strip"),
        ("音频条位置 Audio bar position", "Ses Çubuğu Konumu", "Audio Bar Position"),
        ("音频光环颜色 Audio Sensing Opacity Audio Sensing Color", "Ses Halkası Rengi", "Audio Ring Color"),
        ("音频光环开关 Audio induction switch", "Ses Halkası", "Audio Halo Switch"),
        ("组件随音频感应变化大小（拉至0取消）", "Bileşenlerin Sese Göre Ölçeklenmesi (0 = Kapalı)", "Audio-Reactive Component Scale (0 to disable)"),
        ("组件随音频感应变化大小(拉至0取消)", "Bileşenlerin Sese Göre Ölçeklenmesi (0 = Kapalı)", "Audio-Reactive Component Scale (0 to disable)"),
        ("底部模糊效果(高昂效果)", "Alt Bulanıklık Efekti (Yüksek Kalite)", "Bottom Blur Effect (High Cost)"),
        ("背景水波(自定义背景请关闭)", "Arka Plan Su Dalgası (Özel Arka Planda Kapatın)", "Background Water Ripple (Disable for Custom BG)"),
        ("自动隐藏时间(秒) Automatically hidetime (in seconds)", "Otomatik Gizleme Süresi (sn)", "Auto-Hide Delay (Seconds)"),
        ("悬停放大倍数 Hover magnification", "Üzerine Gelince Büyütme", "Hover Magnification"),
        ("光标影响半径 Cursor influence", "İmleç Etki Yarıçapı", "Cursor Influence Radius"),
        ("滚动速度 Rolling speed", "Kaydırma Hızı", "Scrolling Speed"),
        ("开场动画/Opening animation", "Açılış Animasyonu", "Opening Animation"),
        ("图标数量/Number of ICONS", "Simge Sayısı", "Number of Icons"),
        ("关闭作者营销词 Hide marketing words", "Yapımcı Tanıtım Yazılarını Gizle", "Hide Author Promo Text"),
        ("鼠标拖尾/Mouse trailing", "Fare İzi", "Mouse Trail"),
        ("鼠标拖尾Mouse tail", "Fare İzi", "Mouse Trail"),
        ("鼠标尾迹Mouse Trail", "Fare İzi", "Mouse Trail"),
        ("鼠标尾迹颜色Mouse Trail Color", "Fare İzi Rengi", "Mouse Trail Color"),
        ("鼠标影响 Mouse influence", "Fare Etkisi", "Mouse Influence"),
        ("可拖动/Dragable", "Sürüklenebilir", "Draggable"),
        ("底部花丛 Lens water stains", "Alt Çiçek Kümesi", "Bottom Flower Cluster"),
        ("q版流萤指针", "Chibi Firefly İmleci", "Chibi Firefly Cursor"),
        ("人物头发和礼服的飘动速度 The speed at which the character's hair and gown flutter.", "Karakter Saç ve Elbise Dalgalanma Hızı", "Character Hair & Gown Flutter Speed"),
        ("人物肢体循环速度 Character limb cycling speed", "Karakter Hareket Döngü Hızı", "Character Limb Cycle Speed"),
        ("[点击骷髅头 进行交互动画] Click the skull to trigger an interactive animation.", "[Etkileşim animasyonu için kurukafaya tıklayın]", "[Click the skull to trigger an interactive animation.]"),
        ("提示框 prompt box", "İpucu Kutusu", "Prompt Box"),
        ("投喂", "Destek / Bağış", "Donate / Support"),
        ("爱发电", "Afdian Bağış", "Afdian Support"),
        ("微信", "WeChat", "WeChat"),
        ("淘宝", "Taobao", "Taobao"),
        ("UnclePanda 淘宝", "UnclePanda Taobao", "UnclePanda Taobao"),
        ("显示赞助信息", "Destek Bilgisini Göster", "Show Sponsor Info"),
        ("手机壁纸获取方式(点击跳转)", "Mobil Duvar Kağıdı Bağlantısı (Açmak İçin Tıkla)", "Get Mobile Wallpaper (Click to Open)"),

        // Creator credits & notices
        ("动态制作:-夜莺Night", "Animasyon: YeYing (Night)", "Animated by: YeYing (Night)"),
        ("🔻我的个人主页🔻 夜莺Night的个人空间 (点击跳转)", "🔻 Yapımcı Profili: YeYing (Night) (Tıkla)", "🔻 Creator Profile: YeYing (Night) (Click)"),
        ("🔻原画师的个人主页🔻 原画师:微糖去冰加珍珠(点击跳转)", "🔻 Çizer Profili: WeitangQubing (Tıkla)", "🔻 Illustrator Profile: WeitangQubing (Click)"),
        ("🔻原画师的个人主页🔻 X:muli @genv117(AI生成)的个人空间 (点击跳转)", "🔻 Çizer Profili: X @genv117 (Yapay Zeka) (Tıkla)", "🔻 Illustrator: X @genv117 (AI Generated) (Click)"),
        ("原画师:X:muli @genv117(AI生成)", "Çizer: X: @genv117 (Yapay Zeka)", "Illustrator: X: @genv117 (AI Generated)"),
        ("❤我的个人主页❤ AL_汤缘的个人空间(点击跳转)", "❤ Yapımcı Profili: AL_TangYuan (Tıkla)", "❤ Creator Profile: AL_TangYuan (Click)"),
        ("喜欢的话,可不可以求个三连呢? BV16x4y1i7px(点击跳转)", "Beğendiyseniz destek olabilirsiniz: BV16x4y1i7px (Tıkla)", "Support the creator: BV16x4y1i7px (Click)"),
        ("喜欢的话,可不可以求个三连呢? 阿罗娜 融于暖春(点击跳转)", "Beğendiyseniz destek olabilirsiniz: Arona Bahar (Tıkla)", "Support the creator: Arona Spring (Click)"),
        ("喜欢的话,可以小小支持一下吗?你们的支持就是我更新最大的动力", "Beğendiyseniz ufak bir destek olabilirsiniz; desteğiniz güncelleme motivasyonumdur.", "Consider supporting the creator; your support keeps updates coming!"),
        ("可以给个好评收藏支持一下吗? Thank You!", "Olumlu oy ve favori ile destek olur musunuz? Teşekkürler!", "Please like and favorite to support! Thank You!"),
        ("请点赞和收藏❤️ Please like and favorite❤️", "Lütfen beğenin ve favorilere ekleyin ❤️", "Please like and favorite ❤️"),
        ("还不来点个关注???", "Takip etmeyi unutmayın!", "Consider following the creator!"),
        ("你的支持就是up创作的动力 如果你愿意投喂我我会很开心.", "Desteğiniz yeni içerikler üretme motivasyonumdur. Teşekkürler!", "Your support motivates me to create more!"),
        ("如果觉得壁纸做的不错请给作者一个好评,十分感谢!!(*´∀人) If you think the wallpaper is good, please give the author a good review, thank you very much!!(*´∀人)", "Duvar kağıdını beğendiyseniz olumlu değerlendirme bırakabilirsiniz, teşekkürler!", "If you like the wallpaper, please leave a positive review, thank you!"),
        ("-----作者:秋枫·塔诺卡----- 未经许可请勿转载到其他平台", "----- Yapımcı: QiuFeng·Tanoka ----- İzinsiz başka platformlarda paylaşmayın", "----- Author: QiuFeng·Tanoka ----- Do not repost without permission"),
        ("动 态 壁 纸 制 作 者 ™🔖 🔻🔻Dynamic made by HopeMaFei ™", "🔖 Hareketli Duvar Kağıdı Yapımcısı: HopeMaFei ™", "🔖 Dynamic Wallpaper Maker: HopeMaFei ™"),
        ("稿 主 ™🏷️ 🔻🔻Manuscript Provider ™", "🏷️ Tasarım Sağlayıcısı ™", "🏷️ Artwork Provider ™"),
        ("稿 主", "Tasarım Sağlayıcısı", "Artwork Provider"),
        ("此壁纸由[Hope麻匪]制作 Steam 版 Wallpaper Engine 专用 壁纸免费 禁止售卖 禁止 中国大陆内 任何壁纸软件使用 包括:软件,App,小程序,网页,视频带货笔记本作为显示宣传 This Wallpaper is dedicated to the Steam edition of the Wallpaper Engine Wallpapers are completely free and are not allowed to be sold 感 谢 您 的 使 用 Thank you for using", "Steam Wallpaper Engine için HopeMaFei tarafından yapılmıştır. Ücretsizdir, satılamaz. Kullandığınız için teşekkürler!", "Made by HopeMaFei for Steam Wallpaper Engine. Completely free and not for sale. Thank you for using!"),
        ("功能简要展示 : 人物设置: 人物动效开关,人物有两种动效选择:精细或简单.简单占用低,动效简单一些;精细动效占用高,动效精致许多 背景设置: 背景样式选择,媒体集成背景,鼠标深度视差 环境设置: 流星,粒子效果自定义 组件设置: 时间组件,音频可视化,鼠标尾迹,多种字体选择等等 若带不动可以将动效由精细更改至简略或关闭动效,适当关闭一些效果,同时进入wallpaper设置纹理分辨率为高性能,降低帧率.", "Kısa Rehber — Karakter Ayarları: Basit (düşük kaynak) veya Detaylı (yüksek kalite) animasyon seçin. Arka Plan: Stil, medya entegrasyonu ve fare paralaksı. Ortam: Akan yıldızlar ve parçacıklar. Bileşenler: Saat, ses görselleştirici, imleç izi ve yazı tipleri. Performans düşerse animasyonu Basit/Kapalı yapıp kare hızını düşürün.", "Quick Guide — Character: choose Simple (low load) or Detailed animation. Background: style, media integration, mouse parallax. Environment: meteors & particles. Components: clock, audio visualizer, cursor trail & fonts. For better performance, switch animation to Simple/Off or lower FPS."),
    ];

    let single_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if single_line.starts_with("功能简要展示") && single_line.contains("降低帧率") {
        return Some(if is_tr {
            "Kısa Rehber — Karakter Ayarları: Basit (düşük kaynak) veya Detaylı (yüksek kalite) animasyon seçin. Arka Plan: Stil, medya entegrasyonu ve fare paralaksı. Ortam: Akan yıldızlar ve parçacıklar. Bileşenler: Saat, ses görselleştirici, imleç izi ve yazı tipleri. Performans düşerse animasyonu Basit/Kapalı yapıp kare hızını düşürün."
        } else {
            "Quick Guide — Character: choose Simple (low load) or Detailed animation. Background: style, media integration, mouse parallax. Environment: meteors & particles. Components: clock, audio visualizer, cursor trail & fonts. For better performance, switch animation to Simple/Off or lower FPS."
        }.to_string());
    }
    for &(k, tr, en) in EXACT {
        if s == k || s.eq_ignore_ascii_case(k) || single_line == k || single_line.eq_ignore_ascii_case(k) {
            return Some(if is_tr { tr } else { en }.to_string());
        }
    }
    None
}

fn try_translate_bilingual(s: &str, is_tr: bool, allow_global_cjk: bool) -> Option<String> {
    // 1. Top-level `/` or `|` where one side has CJK and the other side is purely ASCII/English.
    for sep in ['/', '|'] {
        if let Some(idx) = find_top_level_char(s, sep) {
            let left = s[..idx].trim();
            let right = s[idx + 1..].trim();
            if !left.is_empty() && !right.is_empty() {
                if contains_cjk(left) && !contains_cjk(right) {
                    let t = translate_cjk_only_or_global(left, is_tr, allow_global_cjk);
                    if !contains_cjk(&t) && !t.is_empty() {
                        return Some(t);
                    }
                    if !is_tr {
                        return Some(right.to_string());
                    }
                } else if contains_cjk(right) && !contains_cjk(left) {
                    let t = translate_cjk_only_or_global(right, is_tr, allow_global_cjk);
                    if !contains_cjk(&t) && !t.is_empty() {
                        return Some(t);
                    }
                    if !is_tr {
                        return Some(left.to_string());
                    }
                }
            }
        }
    }

    // 2. `CJK(English)` duplicate like `时间(Clock)`, `辉光(Glow)`, `雨(Rain)`, `音乐(Music)`.
    if let Some(open) = s.rfind('(') {
        if s.ends_with(')') {
            let before = s[..open].trim();
            let inner = s[open + 1..s.len() - 1].trim();
            if contains_cjk(before)
                && !contains_cjk(inner)
                && inner.chars().any(|c| c.is_ascii_alphabetic())
                && !matches!(inner.to_ascii_lowercase().as_str(), "ms" | "fps" | "px" | "x" | "y" | "ai")
            {
                let t = translate_cjk_only_or_global(before, is_tr, allow_global_cjk);
                if !contains_cjk(&t) && !t.is_empty() {
                    return Some(t);
                }
            }
        }
    }

    // 3. Adjacent bilingual label `<English words> <CJK>` or `<CJK> <English words>`
    //    such as `Background 背景`, `声音sound`, `Date component display`.
    if let Some(adj) = try_strip_adjacent_english(s, is_tr, allow_global_cjk) {
        return Some(adj);
    }

    None
}

fn is_preserved_identifier(word: &str) -> bool {
    const KEEP: &[&str] = &[
        "x", "y", "z", "cg", "bgm", "fps", "hud", "rgb", "hsv", "hdr", "ai", "ui",
        "led", "q", "live2d", "qq", "wechat", "taobao", "afdian", "unclepanda",
        "hopemafei", "night", "arona", "firefly", "jinx", "x1", "x2", "y1", "y2",
    ];
    let clean = word.trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
    if clean.is_empty() {
        return true;
    }
    let low = clean.to_ascii_lowercase();
    if KEEP.contains(&low.as_str()) {
        return true;
    }
    if clean.chars().any(|c| c.is_ascii_digit()) {
        return true;
    }
    if clean.len() <= 2 && clean.chars().all(|c| c.is_ascii_uppercase()) {
        return true;
    }
    // Internal CamelCase or underscores (e.g. `UnclePanda`, `AL_TangYuan`)
    clean.contains('_')
        || clean
            .chars()
            .skip(1)
            .any(|c| c.is_ascii_uppercase())
}

fn try_strip_adjacent_english(s: &str, is_tr: bool, allow_global_cjk: bool) -> Option<String> {
    if s.len() > 96 || !contains_cjk(s) {
        return None;
    }
    // Extract contiguous CJK-or-emoji/number segments vs purely ASCII English segments.
    let mut cjk_side = String::new();
    let mut ascii_words: Vec<String> = Vec::new();
    let mut cur_ascii = String::new();
    for ch in s.chars() {
        if ch.is_ascii_alphabetic() {
            cur_ascii.push(ch);
        } else {
            if !cur_ascii.is_empty() {
                if is_preserved_identifier(&cur_ascii) {
                    cjk_side.push(' ');
                    cjk_side.push_str(&cur_ascii);
                    cjk_side.push(' ');
                } else {
                    ascii_words.push(cur_ascii.clone());
                }
                cur_ascii.clear();
            }
            cjk_side.push(ch);
        }
    }
    if !cur_ascii.is_empty() {
        if is_preserved_identifier(&cur_ascii) {
            cjk_side.push(' ');
            cjk_side.push_str(&cur_ascii);
        } else {
            ascii_words.push(cur_ascii);
        }
    }
    if ascii_words.is_empty() || !contains_cjk(&cjk_side) {
        return None;
    }
    let cjk_trimmed = cjk_side.split_whitespace().collect::<Vec<_>>().join(" ");
    let translated = translate_cjk_only_or_global(&cjk_trimmed, is_tr, allow_global_cjk);
    if !contains_cjk(&translated) && translated.chars().any(|c| c.is_alphabetic()) {
        // Check we didn't drop trailing numbers already embedded or lost
        return Some(translated);
    }
    None
}

fn find_top_level_char(s: &str, needle: char) -> Option<usize> {
    let mut depth = 0i32;
    let mut found = None;
    for (idx, ch) in s.char_indices() {
        match ch {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = (depth - 1).max(0),
            c if c == needle && depth == 0 => {
                if found.is_some() {
                    return None;
                }
                found = Some(idx);
            }
            _ => {}
        }
    }
    found
}

fn translate_cjk_only_or_global(s: &str, is_tr: bool, allow_global_cjk: bool) -> String {
    if let Some(exact) = lookup_exact_cjk(s, is_tr) {
        return exact;
    }
    if allow_global_cjk {
        let gl = global_we_locale();
        if gl.is_tr == is_tr {
            if let Some(gl_hit) = gl.lookup_cjk(s) {
                return gl_hit.to_string();
            }
        }
    }
    translate_cjk_phrases(s, is_tr)
}

fn translate_cjk_phrases(s: &str, is_tr: bool) -> String {
    // Longest-match dictionary ordered greedily at scan time (longest prefix wins).
    // Covers Workshop property & option vocabulary plus common Japanese/Chinese UI terms.
    const PHRASES: &[(&str, &str, &str)] = &[
        ("组件随音频感应变化大小", "Ses Tepkili Bileşen Ölçeği", "Audio-Reactive Component Scale"),
        ("音频感应透明度和颜色", "Ses Halkası Rengi", "Audio Halo Color"),
        ("颜色自动变化", "Otomatik Renk Değişimi", "Auto Color Cycling"),
        ("三角组件鼠标视差大小", "Üçgen Bileşen Fare Paralaksı", "Triangle Component Mouse Parallax"),
        ("总组件鼠标视差大小", "Genel Bileşen Fare Paralaksı", "Global Component Mouse Parallax"),
        ("人物头发和礼服的飘动速度", "Saç ve Elbise Dalgalanma Hızı", "Hair and Gown Flutter Speed"),
        ("人物肢体循环速度", "Karakter Hareket Döngü Hızı", "Character Limb Cycle Speed"),
        ("自定义背景请关闭", "Özel Arka Planda Kapatın", "Disable for Custom BG"),
        ("自定义文字不透明度", "Özel Metin Opaklığı", "Custom Text Opacity"),
        ("自定义文本透明度", "Özel Metin Opaklığı", "Custom Text Opacity"),
        ("自定义文本位置", "Özel Metin Konumu", "Custom Text Position"),
        ("自定义文本大小", "Özel Metin Boyutu", "Custom Text Size"),
        ("自定义文本开关", "Özel Metni Göster", "Custom Text Switch"),
        ("自定义文本颜色", "Özel Metin Rengi", "Custom Text Color"),
        ("自定义文字1大小", "Özel Metin 1 Boyutu", "Custom Text 1 Size"),
        ("自定义文字1颜色", "Özel Metin 1 Rengi", "Custom Text 1 Color"),
        ("自定义文字2大小", "Özel Metin 2 Boyutu", "Custom Text 2 Size"),
        ("自定义文字2颜色", "Özel Metin 2 Rengi", "Custom Text 2 Color"),
        ("自定义文字", "Özel Metin", "Custom Text"),
        ("自定义文本", "Özel Metin", "Custom Text"),
        ("音频响应条透明度", "Ses Çubuğu Opaklığı", "Audio Bar Opacity"),
        ("音频响应条开关", "Ses Tepki Çubuğu", "Audio Bar Switch"),
        ("音频响应条颜色", "Ses Çubuğu Rengi", "Audio Bar Color"),
        ("音频响应条", "Ses Tepki Çubuğu", "Audio Visualizer Bar"),
        ("音频条部分不显示", "Ses Çubuğunu Kısmen Gizle", "Partial Audio Bar"),
        ("音频条不透明度", "Ses Çubuğu Opaklığı", "Audio Bar Opacity"),
        ("音频条位置", "Ses Çubuğu Konumu", "Audio Bar Position"),
        ("音频条开关", "Ses Çubuğu", "Audio Bar"),
        ("音频条滚动", "Ses Çubuğu Kaydırması", "Audio Bar Scroll"),
        ("音频条视差", "Ses Çubuğu Paralaksı", "Audio Bar Parallax"),
        ("音频条颜色", "Ses Çubuğu Rengi", "Audio Bar Color"),
        ("音频光环亮度", "Ses Halkası Parlaklığı", "Audio Halo Brightness"),
        ("音频光环位置", "Ses Halkası Konumu", "Audio Halo Position"),
        ("音频光环半径", "Ses Halkası Yarıçapı", "Audio Halo Radius"),
        ("音频光环开关", "Ses Halkası", "Audio Halo"),
        ("音频光环颜色", "Ses Halkası Rengi", "Audio Halo Color"),
        ("动效状态文字开关", "Durum Yazısı", "Animation Status Text"),
        ("鼠标样式自定义", "Özel İmleç Stili", "Custom Cursor Style"),
        ("前花丛1大小", "Ön Çiçek Kümesi 1 Boyutu", "Front Flower Cluster 1 Size"),
        ("前花丛1", "Ön Çiçek Kümesi 1", "Front Flower Cluster 1"),
        ("前花丛2", "Ön Çiçek Kümesi 2", "Front Flower Cluster 2"),
        ("后花丛1", "Arka Çiçek Kümesi 1", "Back Flower Cluster 1"),
        ("后花丛2", "Arka Çiçek Kümesi 2", "Back Flower Cluster 2"),
        ("底部花丛", "Alt Çiçek Kümesi", "Bottom Flower Cluster"),
        ("背景闪烁星星大小", "Parıldayan Yıldız Boyutu", "Twinkling Stars Size"),
        ("背景闪烁星星数量", "Parıldayan Yıldız Sayısı", "Twinkling Stars Count"),
        ("背景闪烁星星", "Parıldayan Yıldızlar", "Twinkling Stars"),
        ("背景飘落粒子", "Düşen Arka Plan Parçacıkları", "Falling Background Particles"),
        ("背景字符下落", "Düşen Karakter Yağmuru", "Falling Characters"),
        ("背景流星速度", "Akan Yıldız Hızı", "Meteor Speed"),
        ("背景流星大小", "Akan Yıldız Boyutu", "Meteor Size"),
        ("背景流星数量", "Akan Yıldız Sayısı", "Meteor Count"),
        ("背景流星", "Akan Yıldızlar", "Meteors"),
        ("背景日期", "Arka Plan Tarihi", "Background Date"),
        ("背景时间", "Arka Plan Saati", "Background Time"),
        ("背景选择", "Arka Plan Seçimi", "Background Selection"),
        ("背景自选", "Özel Arka Plan", "Custom Background"),
        ("背景水波", "Arka Plan Su Dalgası", "Background Water Ripple"),
        ("背景颜色", "Arka Plan Rengi", "Background Color"),
        ("下三角组件显示", "Alt Üçgen Bileşeni", "Bottom Triangle Component"),
        ("主三角组件显示", "Ana Üçgen Bileşeni", "Main Triangle Component"),
        ("左三角组件显示", "Sol Üçgen Bileşeni", "Left Triangle Component"),
        ("右三角组件显示", "Sağ Üçgen Bileşeni", "Right Triangle Component"),
        ("三角组件背景", "Üçgen Bileşen Arka Planı", "Triangle Component Background"),
        ("三角组件位置", "Üçgen Bileşen Konumu", "Triangle Component Position"),
        ("三角组件大小", "Üçgen Bileşen Boyutu", "Triangle Component Size"),
        ("媒体集成组件显示", "Medya Entegrasyonu Bileşeni", "Media Integration Component"),
        ("媒体集成组件", "Medya Bileşeni", "Media Component"),
        ("媒体集成", "Medya Entegrasyonu", "Media Integration"),
        ("时间组件透明度", "Saat Bileşeni Opaklığı", "Time Component Opacity"),
        ("时间组件位置", "Saat Bileşeni Konumu", "Time Component Position"),
        ("时间组件大小", "Saat Bileşeni Boyutu", "Time Component Size"),
        ("时间组件字体", "Saat Yazı Tipi", "Time Font"),
        ("时间组件开关", "Saat Bileşeni", "Time Component Switch"),
        ("时间组件显示", "Saat Bileşenini Göster", "Show Time Component"),
        ("时间组件颜色", "Saat Bileşeni Rengi", "Time Component Color"),
        ("时间组件设置", "Saat Bileşeni Ayarları", "Time Component Settings"),
        ("时间组件", "Saat Bileşeni", "Time Component"),
        ("时间变化开关", "Zamana Bağlı Değişim", "Time-of-Day Transition"),
        ("时间显示大小", "Saat Boyutu", "Time Size"),
        ("时间显示颜色", "Saat Rengi", "Time Color"),
        ("时间位置", "Saat Konumu", "Time Position"),
        ("时间视差", "Saat Paralaksı", "Time Parallax"),
        ("时间设置", "Saat", "Time"),
        ("时间总", "Saat Genel", "Time Overall"),
        ("日期组件字体", "Tarih Yazı Tipi", "Date Font"),
        ("日期组件显示", "Tarih Bileşenini Göster", "Show Date Component"),
        ("日期显示大小", "Tarih Boyutu", "Date Size"),
        ("日期显示颜色", "Tarih Rengi", "Date Color"),
        ("日期位置", "Tarih Konumu", "Date Position"),
        ("星期组件字体", "Gün Yazı Tipi", "Weekday Font"),
        ("星期组件显示", "Haftanın Gününü Göster", "Show Weekday Component"),
        ("星期显示大小", "Gün Boyutu", "Weekday Size"),
        ("星期显示颜色", "Gün Rengi", "Weekday Color"),
        ("星期位置", "Gün Konumu", "Weekday Position"),
        ("早中晚英文显示", "İngilizce Günün Vakti", "English Time-of-Day Label"),
        ("早中晚显示", "Günün Vakti Etiketi", "Time-of-Day Greeting"),
        ("使用24小时格式", "24 Saat Biçimi", "24-Hour Format"),
        ("使用24小时制", "24 Saat Biçimi", "24-Hour Format"),
        ("当前帧率显示", "FPS Göstergesi", "Show FPS"),
        ("帧率位置", "FPS Konumu", "FPS Position"),
        ("显示到秒", "Saniyeyi Göster", "Show Seconds"),
        ("秒数显示", "Saniyeyi Göster", "Show Seconds"),
        ("秒数颜色", "Saniye Rengi", "Seconds Color"),
        ("组件自定义文字", "Özel Bileşen Metni", "Custom Component Text"),
        ("组件阴影不透明度", "Bileşen Gölge Opaklığı", "Component Shadow Opacity"),
        ("组件阴影颜色", "Bileşen Gölge Rengi", "Component Shadow Color"),
        ("组件高光不透明度", "Bileşen Parlama Opaklığı", "Component Highlight Opacity"),
        ("组件高光颜色", "Bileşen Parlama Rengi", "Component Highlight Color"),
        ("部件高光颜色变化", "Parlama Rengi Değişimi", "Highlight Color Mode"),
        ("组件镜像开关", "Bileşen Yansıtma", "Component Mirror"),
        ("组件音频条", "Bileşen Ses Çubuğu", "Component Audio Bar"),
        ("组件底部", "Bileşen Alt Efekti", "Component Base"),
        ("人物动效开关", "Karakter Hareketi", "Character Animation"),
        ("人物动效选择", "Karakter Animasyon Modu", "Character Animation Mode"),
        ("人物摆动速度", "Karakter Salınım Hızı", "Character Sway Speed"),
        ("人物位置", "Karakter Konumu", "Character Position"),
        ("人物大小", "Karakter Boyutu", "Character Size"),
        ("眼睛移动力度大小", "Göz Hareketi Gücü", "Eye Movement Strength"),
        ("光效音频脉动", "Sese Bağlı Işık Nabzı", "Audio-Reactive Glow Pulse"),
        ("镜头大小", "Lens Boyutu", "Lens Size"),
        ("镜头位置", "Lens Konumu", "Lens Position"),
        ("镜头抖动", "Kamera Titremesi", "Camera Shake"),
        ("自动隐藏时间", "Otomatik Gizleme Süresi", "Auto-Hide Delay"),
        ("自动隐藏", "Otomatik Gizle", "Auto-Hide"),
        ("快捷绑定", "Hızlı Kısayollar", "Quick Shortcuts"),
        ("悬停放大倍数", "Üzerine Gelince Büyütme", "Hover Magnification"),
        ("光标影响半径", "İmleç Etki Yarıçapı", "Cursor Influence Radius"),
        ("鼠标尾迹大小", "Fare İzi Boyutu", "Mouse Trail Size"),
        ("鼠标尾迹颜色", "Fare İzi Rengi", "Mouse Trail Color"),
        ("鼠标尾迹", "Fare İzi", "Mouse Trail"),
        ("鼠标拖尾", "Fare İzi", "Mouse Trail"),
        ("鼠标指针大小", "Fare İmleci Boyutu", "Cursor Size"),
        ("鼠标深度视差", "Fare Derinlik Paralaksı", "Mouse Depth Parallax"),
        ("鼠标视差", "Fare Paralaksı", "Mouse Parallax"),
        ("鼠标影响", "Fare Etkisi", "Mouse Influence"),
        ("流萤大小", "Firefly Boyutu", "Firefly Size"),
        ("萤火光效", "Ateşböceği Parıltısı", "Firefly Glow"),
        ("落花大小", "Düşen Çiçek Boyutu", "Falling Flower Size"),
        ("落花", "Düşen Çiçekler", "Falling Flowers"),
        ("花瓣飘舞", "Uçuşan Çiçek Yaprakları", "Floating Petals"),
        ("纯色层", "Düz Renk Katmanı", "Solid Layer"),
        ("纯色颜色", "Düz Renk", "Solid Color"),
        ("底部模糊效果", "Alt Bulanıklık Efekti", "Bottom Blur Effect"),
        ("高昂效果", "Yüksek Kalite", "High Cost"),
        ("模式/开关", "Mod / Anahtar", "Mode / Switch"),
        ("拉至0取消", "0 = Kapalı", "0 to disable"),
        ("点击跳转", "Açmak İçin Tıkla", "Click to Open"),
        ("阳光强度", "Güneş Işığı Şiddeti", "Sunlight Intensity"),
        ("文字描边", "Metin Dış Çizgisi", "Text Outline"),
        ("画师签名", "Çizer İmzası", "Artist Signature"),
        ("图标数量", "Simge Sayısı", "Icon Count"),
        ("滚动速度", "Kaydırma Hızı", "Scroll Speed"),
        ("开场动画", "Açılış Animasyonu", "Opening Animation"),
        ("壁纸视差", "Duvar Kağıdı Paralaksı", "Wallpaper Parallax"),
        ("音频组件", "Ses Bileşenleri", "Audio Component"),
        ("音频响应", "Ses Tepkisi", "Audio Reactivity"),
        ("音频条", "Ses Çubuğu", "Audio Bar"),
        ("音圈", "Ses Halkası", "Audio Ring"),
        ("音乐名", "Şarkı Adı", "Track Title"),
        ("音乐大小", "Müzik Ses Düzeyi", "Music Volume"),
        ("动作速度", "Animasyon Hızı", "Animation Speed"),
        ("恶魔脸", "Şeytan Yüzü", "Devil Face"),
        ("金克斯", "Jinx", "Jinx"),
        ("阿罗娜", "Arona", "Arona"),
        ("流萤", "Firefly", "Firefly"),
        ("字体", "Yazı Tipi", "Font"),
        ("音量", "Ses Düzeyi", "Volume"),
        ("声音", "Ses", "Sound"),
        ("音乐", "Müzik", "Music"),
        ("背景", "Arka Plan", "Background"),
        ("日期", "Tarih", "Date"),
        ("时间", "Saat", "Time"),
        ("星期", "Haftanın Günü", "Weekday"),
        ("颜色", "Renk", "Color"),
        ("脉动", "Nabız", "Pulse"),
        ("开", "Açık", "On"),
        ("关", "Kapalı", "Off"),
        ("闪", "Parlama", "Flash"),
        ("大小", "Boyut", "Size"),
        ("强度", "Şiddet", "Strength"),
        ("速度", "Hız", "Speed"),
        ("位置", "Konum", "Position"),
        ("不透明度", "Opaklık", "Opacity"),
        ("透明度", "Opaklık", "Opacity"),
        ("亮度", "Parlaklık", "Brightness"),
        ("对比度", "Karşıtlık", "Contrast"),
        ("饱和度", "Canlılık", "Saturation"),
        ("辉光", "Parıltı", "Glow"),
        ("色差", "Renk Sapması", "Chromatic"),
        ("间距", "Aralık", "Spacing"),
        ("条上限", "Maksimum Çubuk", "Max Bars"),
        ("条数", "Çubuk Sayısı", "Bar Count"),
        ("雨量", "Yağmur Yoğunluğu", "Rain Intensity"),
        ("雪花", "Kar Tanesi", "Snow"),
        ("雨", "Yağmur", "Rain"),
        ("头发", "Saç", "Hair"),
        ("光标", "İmleç", "Cursor"),
        ("开关", "Anahtar", "Switch"),
        ("设置", "Ayarlar", "Settings"),
        ("组件", "Bileşen", "Component"),
        ("效果", "Efekt", "Effect"),
        ("显示", "Göster", "Show"),
        ("隐藏", "Gizle", "Hide"),
        ("关闭", "Kapalı", "Off"),
        ("打开", "Açık", "On"),
        ("半径", "Yarıçap", "Radius"),
        ("轴", "Ekseni", "Axis"),
        ("秒", "sn", "s"),
        // Japanese common UI vocabulary
        ("オーディオスペクトラム", "Ses Spektrumu", "Audio Spectrum"),
        ("パーティクル", "Parçacıklar", "Particles"),
        ("エフェクト", "Efektler", "Effects"),
        ("アニメーション", "Animasyon", "Animation"),
        ("バックグラウンド", "Arka Plan", "Background"),
        ("マウス", "Fare", "Mouse"),
        ("カラー", "Renk", "Color"),
        ("サイズ", "Boyut", "Size"),
        ("スピード", "Hız", "Speed"),
        ("明るさ", "Parlaklık", "Brightness"),
        ("非表示", "Gizli", "Hidden"),
        ("表示", "Göster", "Show"),
        ("音量", "Ses Düzeyi", "Volume"),
        ("音楽", "Müzik", "Music"),
        ("時計", "Saat", "Clock"),
        ("日付", "Tarih", "Date"),
        ("日本語", "Japonca", "Japanese"),
        ("有効", "Etkin", "Enabled"),
        ("無効", "Devre Dışı", "Disabled"),
        ("オン", "Açık", "On"),
        ("オフ", "Kapalı", "Off"),
        ("速度", "Hız", "Speed"),
        ("色", "Renk", "Color"),
    ];

    let mut out = String::with_capacity(s.len() * 2);
    let mut rest = s;
    while !rest.is_empty() {
        let mut best: Option<(&str, &str, &str)> = None;
        for &entry in PHRASES {
            if rest.starts_with(entry.0) && best.is_none_or(|b| entry.0.len() > b.0.len()) {
                best = Some(entry);
            }
        }
        if let Some((pat, tr, en)) = best {
            let word = if is_tr { tr } else { en };
            if !out.is_empty() && !out.ends_with([' ', '(', '[', '/', '-', ':']) {
                out.push(' ');
            }
            out.push_str(word);
            rest = &rest[pat.len()..];
            if !rest.is_empty()
                && !rest.starts_with([' ', ')', ']', '/', '-', ':', ',', '.'])
                && !out.ends_with(' ')
            {
                out.push(' ');
            }
            continue;
        }
        let ch = rest.chars().next().unwrap();
        if is_cjk_char(ch) {
            // Unknown solitary CJK character: skip rather than leaking unreadable glyph,
            // only if surrounded by translated context; keep structure tidy.
            rest = &rest[ch.len_utf8()..];
            continue;
        }
        out.push(ch);
        rest = &rest[ch.len_utf8()..];
    }

    // Tidy punctuation spacing produced during phrase replacement.
    let tidy = out
        .replace("( ", "(")
        .replace(" )", ")")
        .replace(" :", ":")
        .replace(" 1", " 1")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if tidy.is_empty() {
        if is_tr { "Özellik" } else { "Property" }.to_string()
    } else {
        tidy
    }
}

fn extract_we_properties(project: &serde_json::Value) -> Vec<WallpaperProperty> {
    let mut out = Vec::new();
    let Some(props) = project
        .get("general")
        .and_then(|g| g.get("properties"))
        .and_then(|p| p.as_object())
    else {
        return out;
    };

    let loc = resolve_we_localization(project);

    for (key, obj) in props {
        let prop_type = obj
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("text")
            .trim()
            .to_ascii_lowercase();

        let default_value = match obj.get("value") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Bool(b)) => b.to_string(),
            Some(serde_json::Value::Number(n)) => n.to_string(),
            Some(v) => v.to_string(),
            None => String::new(),
        };

        // Skip internal non-UI types (`usershortcut` keybinds and empty `scenetexture` slots).
        if prop_type == "usershortcut"
            || (prop_type == "scenetexture" && default_value.trim().is_empty())
        {
            continue;
        }

        let raw_text = obj.get("text").and_then(|v| v.as_str()).unwrap_or(key);
        let raw_trimmed = raw_text.trim();
        let localized = lookup_localized(&loc, raw_trimmed)
            .or_else(|| global_we_locale().lookup_key(raw_trimmed))
            .unwrap_or(raw_text);
        let mut text = translate_we_label(&clean_we_label(localized));
        if text.is_empty() {
            if prop_type == "group" || prop_type == "text" {
                continue;
            }
            text = key.clone();
        }

        let options = obj
            .get("options")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|item| {
                        let val = match item.get("value") {
                            Some(serde_json::Value::String(s)) => s.clone(),
                            Some(serde_json::Value::Bool(b)) => b.to_string(),
                            Some(serde_json::Value::Number(n)) => n.to_string(),
                            Some(v) => v.to_string(),
                            None => return None,
                        };
                        let raw_lbl = item
                            .get("label")
                            .and_then(|v| v.as_str())
                            .unwrap_or(&val);
                        let raw_lbl_trimmed = raw_lbl.trim();
                        let loc_lbl = lookup_localized(&loc, raw_lbl_trimmed)
                            .or_else(|| global_we_locale().lookup_key(raw_lbl_trimmed))
                            .unwrap_or(raw_lbl);
                        let mut label = translate_we_label(&clean_we_label(loc_lbl));
                        if label.is_empty() {
                            label = val.clone();
                        }
                        Some(PropertyOption { label, value: val })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let min = obj.get("min").and_then(|v| v.as_f64());
        let max = obj.get("max").and_then(|v| v.as_f64());
        let step = obj.get("step").and_then(|v| v.as_f64());
        let fraction = obj
            .get("fraction")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let order = obj.get("order").and_then(|v| v.as_i64());
        let index = obj.get("index").and_then(|v| v.as_i64());
        let condition = obj
            .get("condition")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);

        out.push(WallpaperProperty {
            key: key.clone(),
            text,
            prop_type,
            default_value,
            min,
            max,
            step,
            fraction,
            order,
            index,
            options,
            condition,
        });
    }
    sort_properties(&mut out);
    out
}

/// Evaluate a Wallpaper Engine `project.json` property `condition` expression
/// against current property values (e.g. `effects.value == true && noise.value == true`).
/// Unparseable conditions fail-open (`true`) so controls remain accessible.
pub fn eval_property_condition(cond: &str, values: &BTreeMap<String, String>) -> bool {
    let trimmed = cond.trim();
    if trimmed.is_empty() {
        return true;
    }
    let Ok(tokens) = tokenize_cond(trimmed) else {
        return true;
    };
    if tokens.is_empty() {
        return true;
    }
    let mut parser = CondParser {
        tokens: &tokens,
        pos: 0,
        values,
    };
    match parser.parse_or() {
        Ok(val) if parser.pos == tokens.len() => cond_truthy(&val),
        _ => true,
    }
}

#[derive(Debug, Clone)]
enum CondTok {
    Or,
    And,
    Eq,
    Ne,
    Ge,
    Le,
    Gt,
    Lt,
    Bang,
    Minus,
    LParen,
    RParen,
    LBrack,
    RBrack,
    Comma,
    Dot,
    Ident(String),
    Str(String),
    Num(f64),
}

#[derive(Debug, Clone)]
enum CondVal {
    Bool(bool),
    Num(f64),
    Str(String),
}

fn tokenize_cond(input: &str) -> std::result::Result<Vec<CondTok>, ()> {
    let bytes = input.as_bytes();
    let mut i = 0usize;
    let mut out = Vec::new();
    while i < bytes.len() {
        let ch = input[i..].chars().next().ok_or(())?;
        if ch.is_whitespace() {
            i += ch.len_utf8();
            continue;
        }
        let rest = &input[i..];
        if rest.starts_with("||") {
            out.push(CondTok::Or);
            i += 2;
        } else if rest.starts_with("&&") {
            out.push(CondTok::And);
            i += 2;
        } else if rest.starts_with("===") || rest.starts_with("!==") {
            out.push(if rest.starts_with('!') { CondTok::Ne } else { CondTok::Eq });
            i += 3;
        } else if rest.starts_with("==") {
            out.push(CondTok::Eq);
            i += 2;
        } else if rest.starts_with("!=") {
            out.push(CondTok::Ne);
            i += 2;
        } else if rest.starts_with(">=") {
            out.push(CondTok::Ge);
            i += 2;
        } else if rest.starts_with("<=") {
            out.push(CondTok::Le);
            i += 2;
        } else {
            match ch {
                '=' => {
                    // Some WE authors write single `=` for equality (`raineffects.value=true`).
                    out.push(CondTok::Eq);
                    i += 1;
                }
                '>' => {
                    out.push(CondTok::Gt);
                    i += 1;
                }
                '<' => {
                    out.push(CondTok::Lt);
                    i += 1;
                }
                '!' => {
                    out.push(CondTok::Bang);
                    i += 1;
                }
                '-' => {
                    out.push(CondTok::Minus);
                    i += 1;
                }
                '(' => {
                    out.push(CondTok::LParen);
                    i += 1;
                }
                ')' => {
                    out.push(CondTok::RParen);
                    i += 1;
                }
                '[' => {
                    out.push(CondTok::LBrack);
                    i += 1;
                }
                ']' => {
                    out.push(CondTok::RBrack);
                    i += 1;
                }
                ',' => {
                    out.push(CondTok::Comma);
                    i += 1;
                }
                '.' => {
                    out.push(CondTok::Dot);
                    i += 1;
                }
                '\'' | '"' => {
                    let quote = ch;
                    i += 1;
                    let mut s = String::new();
                    let mut closed = false;
                    while i < input.len() {
                        let c = input[i..].chars().next().ok_or(())?;
                        i += c.len_utf8();
                        if c == '\\' && i < input.len() {
                            if let Some(esc) = input[i..].chars().next() {
                                s.push(esc);
                                i += esc.len_utf8();
                                continue;
                            }
                        }
                        if c == quote {
                            closed = true;
                            break;
                        }
                        s.push(c);
                    }
                    if !closed {
                        return Err(());
                    }
                    out.push(CondTok::Str(s));
                }
                c if c.is_ascii_digit() => {
                    let start = i;
                    while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                        i += 1;
                    }
                    let n = input[start..i].parse::<f64>().map_err(|_| ())?;
                    out.push(CondTok::Num(n));
                }
                c if c.is_ascii_alphabetic() || c == '_' => {
                    let start = i;
                    while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_')
                    {
                        i += 1;
                    }
                    out.push(CondTok::Ident(input[start..i].to_string()));
                }
                _ => return Err(()),
            }
        }
    }
    Ok(out)
}

struct CondParser<'a> {
    tokens: &'a [CondTok],
    pos: usize,
    values: &'a BTreeMap<String, String>,
}

impl<'a> CondParser<'a> {
    fn peek(&self) -> Option<&'a CondTok> {
        self.tokens.get(self.pos)
    }

    fn parse_or(&mut self) -> std::result::Result<CondVal, ()> {
        let mut lhs = self.parse_and()?;
        while matches!(self.peek(), Some(CondTok::Or)) {
            self.pos += 1;
            let rhs = self.parse_and()?;
            lhs = CondVal::Bool(cond_truthy(&lhs) || cond_truthy(&rhs));
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> std::result::Result<CondVal, ()> {
        let mut lhs = self.parse_cmp()?;
        while matches!(self.peek(), Some(CondTok::And)) {
            self.pos += 1;
            let rhs = self.parse_cmp()?;
            lhs = CondVal::Bool(cond_truthy(&lhs) && cond_truthy(&rhs));
        }
        Ok(lhs)
    }

    fn parse_cmp(&mut self) -> std::result::Result<CondVal, ()> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = match self.peek() {
                Some(CondTok::Eq) => 0,
                Some(CondTok::Ne) => 1,
                Some(CondTok::Gt) => 2,
                Some(CondTok::Ge) => 3,
                Some(CondTok::Lt) => 4,
                Some(CondTok::Le) => 5,
                _ => break,
            };
            self.pos += 1;
            let rhs = self.parse_unary()?;
            let res = match op {
                0 => cond_eq(&lhs, &rhs),
                1 => !cond_eq(&lhs, &rhs),
                2 => cond_cmp(&lhs, &rhs).is_some_and(|o| o.is_gt()),
                3 => cond_cmp(&lhs, &rhs).is_some_and(|o| !o.is_lt()),
                4 => cond_cmp(&lhs, &rhs).is_some_and(|o| o.is_lt()),
                _ => cond_cmp(&lhs, &rhs).is_some_and(|o| !o.is_gt()),
            };
            lhs = CondVal::Bool(res);
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> std::result::Result<CondVal, ()> {
        match self.peek() {
            Some(CondTok::Bang) => {
                self.pos += 1;
                let v = self.parse_unary()?;
                Ok(CondVal::Bool(!cond_truthy(&v)))
            }
            Some(CondTok::Minus) => {
                self.pos += 1;
                let v = self.parse_unary()?;
                let n = cond_to_num(&v).ok_or(())?;
                Ok(CondVal::Num(-n))
            }
            _ => self.parse_primary(),
        }
    }

    fn parse_primary(&mut self) -> std::result::Result<CondVal, ()> {
        let Some(tok) = self.peek().cloned() else {
            return Err(());
        };
        match tok {
            CondTok::LParen => {
                self.pos += 1;
                let v = self.parse_or()?;
                if !matches!(self.peek(), Some(CondTok::RParen)) {
                    return Err(());
                }
                self.pos += 1;
                Ok(v)
            }
            CondTok::LBrack => {
                self.pos += 1;
                let mut items = Vec::new();
                if !matches!(self.peek(), Some(CondTok::RBrack)) {
                    loop {
                        items.push(self.parse_or()?);
                        if matches!(self.peek(), Some(CondTok::Comma)) {
                            self.pos += 1;
                        } else {
                            break;
                        }
                    }
                }
                if !matches!(self.peek(), Some(CondTok::RBrack)) {
                    return Err(());
                }
                self.pos += 1;
                if !matches!(self.peek(), Some(CondTok::Dot)) {
                    return Err(());
                }
                self.pos += 1;
                let Some(CondTok::Ident(method)) = self.peek() else {
                    return Err(());
                };
                if method != "includes" {
                    return Err(());
                }
                self.pos += 1;
                if !matches!(self.peek(), Some(CondTok::LParen)) {
                    return Err(());
                }
                self.pos += 1;
                let needle = self.parse_or()?;
                if !matches!(self.peek(), Some(CondTok::RParen)) {
                    return Err(());
                }
                self.pos += 1;
                Ok(CondVal::Bool(items.iter().any(|x| cond_eq(x, &needle))))
            }
            CondTok::Num(n) => {
                self.pos += 1;
                Ok(CondVal::Num(n))
            }
            CondTok::Str(s) => {
                self.pos += 1;
                Ok(CondVal::Str(s))
            }
            CondTok::Ident(first) => {
                self.pos += 1;
                let mut parts = vec![first];
                while matches!(self.peek(), Some(CondTok::Dot)) {
                    self.pos += 1;
                    match self.peek() {
                        Some(CondTok::Ident(seg)) => {
                            parts.push(seg.clone());
                            self.pos += 1;
                        }
                        _ => return Err(()),
                    }
                }
                if parts.len() == 1 {
                    if parts[0].eq_ignore_ascii_case("true") {
                        return Ok(CondVal::Bool(true));
                    }
                    if parts[0].eq_ignore_ascii_case("false") {
                        return Ok(CondVal::Bool(false));
                    }
                }
                let key = if parts.last().is_some_and(|s| s == "value") && parts.len() >= 2 {
                    parts[..parts.len() - 1].join(".")
                } else {
                    parts.join(".")
                };
                let val = self
                    .values
                    .get(&key)
                    .or_else(|| {
                        self.values
                            .iter()
                            .find(|(k, _)| k.eq_ignore_ascii_case(&key))
                            .map(|(_, v)| v)
                    })
                    .cloned()
                    .unwrap_or_default();
                Ok(CondVal::Str(val))
            }
            _ => Err(()),
        }
    }
}

fn cond_to_num(v: &CondVal) -> Option<f64> {
    match v {
        CondVal::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        CondVal::Num(n) => Some(*n),
        CondVal::Str(s) => {
            let t = s.trim();
            if t.eq_ignore_ascii_case("true") {
                Some(1.0)
            } else if t.eq_ignore_ascii_case("false") {
                Some(0.0)
            } else if t.is_empty() {
                None
            } else {
                t.parse::<f64>().ok()
            }
        }
    }
}

fn cond_truthy(v: &CondVal) -> bool {
    match v {
        CondVal::Bool(b) => *b,
        CondVal::Num(n) => *n != 0.0 && !n.is_nan(),
        CondVal::Str(s) => {
            let t = s.trim();
            !t.is_empty()
                && !t.eq_ignore_ascii_case("false")
                && t != "0"
                && t.parse::<f64>().map(|n| n != 0.0).unwrap_or(true)
        }
    }
}

fn cond_eq(a: &CondVal, b: &CondVal) -> bool {
    if let (Some(na), Some(nb)) = (cond_to_num(a), cond_to_num(b)) {
        return (na - nb).abs() < 1e-9;
    }
    match (a, b) {
        (CondVal::Str(sa), CondVal::Str(sb)) => sa.trim() == sb.trim(),
        (CondVal::Bool(ba), CondVal::Bool(bb)) => ba == bb,
        _ => false,
    }
}

fn cond_cmp(a: &CondVal, b: &CondVal) -> Option<std::cmp::Ordering> {
    let na = cond_to_num(a)?;
    let nb = cond_to_num(b)?;
    na.partial_cmp(&nb)
}

fn find_preview(dir: &Path) -> Option<PathBuf> {
    for name in ["preview.jpg", "preview.gif", "preview.png", "preview.webp", "thumb.jpg"] {
        let p = dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

#[derive(Debug, Deserialize)]
struct Manifest {
    file: String,
    title: Option<String>,
    #[serde(rename = "type")]
    kind: Option<WallpaperKind>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    properties: BTreeMap<String, String>,
}

fn dir_name(dir: &Path) -> String {
    dir.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("wallpaper")
        .to_string()
}

pub fn kind_from_extension(path: &Path) -> Result<WallpaperKind> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let kind = match ext.as_str() {
        "png" | "jpg" | "jpeg" | "webp" | "bmp" | "gif" | "avif" | "tga" | "tex" => {
            WallpaperKind::Image
        }
        "mp4" | "webm" | "mkv" | "mov" | "avi" | "m4v" | "ogv" => WallpaperKind::Video,
        "frag" | "glsl" => WallpaperKind::Shader,
        "anima" | "scene" | "pkg" | "toml" => WallpaperKind::Scene,
        "html" | "htm" => WallpaperKind::Web,
        other => bail!("unsupported wallpaper extension: .{other}"),
    };
    Ok(kind)
}

/// Scan a directory for wallpapers (used by `anima list`).
pub fn scan(dir: &Path) -> BTreeMap<String, PathBuf> {
    let mut found = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        if path.is_dir() {
            if path.join("project.json").exists()
                || path.join("wallpaper.toml").exists()
                || path.join("scene.toml").exists()
                || path.join("scene.json").exists()
                || path.join("scene.pkg").exists()
            {
                found.insert(name, path);
            }
        } else if kind_from_extension(&path).is_ok() {
            found.insert(name, path);
        }
    }
    found
}

/// Discover all local Steam Workshop directories for Wallpaper Engine (AppID 431960).
pub fn steam_workshop_dirs() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let candidates = [
        home.join(".local/share/Steam/steamapps/workshop/content/431960"),
        home.join(".steam/steam/steamapps/workshop/content/431960"),
        home.join(".steam/root/steamapps/workshop/content/431960"),
        home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam/steamapps/workshop/content/431960"),
        home.join("snap/steam/common/.local/share/Steam/steamapps/workshop/content/431960"),
    ];
    let mut dirs = Vec::new();
    for c in candidates {
        if let Ok(canon) = c.canonicalize() {
            if canon.is_dir() && !dirs.contains(&canon) {
                dirs.push(canon);
            }
        }
    }
    dirs
}

/// Scan all Steam Workshop Wallpaper Engine items installed on the machine.
pub fn scan_workshop() -> Vec<Wallpaper> {
    let mut items = Vec::new();
    for dir in steam_workshop_dirs() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                if let Ok(wp) = Wallpaper::resolve(&p) {
                    items.push(wp);
                }
            }
        }
    }
    items.sort_by(|a, b| a.title.to_lowercase().cmp(&b.title.to_lowercase()));
    items
}

fn find_by_id_or_name(query: &str) -> Option<PathBuf> {
    // 1. Check Steam Workshop directories by exact folder ID or case-insensitive title substring.
    let q_lower = query.to_ascii_lowercase();
    for ws_dir in steam_workshop_dirs() {
        let exact = ws_dir.join(query);
        if exact.is_dir() {
            return Some(exact);
        }
        if let Ok(entries) = std::fs::read_dir(&ws_dir) {
            for e in entries.flatten() {
                let p = e.path();
                if let Ok(wp) = Wallpaper::from_dir(&p) {
                    if wp.title.to_ascii_lowercase() == q_lower
                        || wp.title.to_ascii_lowercase().contains(&q_lower)
                    {
                        return Some(p);
                    }
                }
            }
        }
    }

    // 2. Check configured wallpapers_dir and ./examples/wallpapers.
    let cfg = crate::config::Config::load(&crate::util::config_path()).unwrap_or_default();
    let search_dirs = [
        cfg.wallpapers_dir(),
        PathBuf::from("examples/wallpapers"),
    ];
    for d in search_dirs {
        let exact = d.join(query);
        if exact.exists() {
            return Some(exact);
        }
        for (name, path) in scan(&d) {
            if name.to_ascii_lowercase() == q_lower || name.to_ascii_lowercase().contains(&q_lower)
            {
                return Some(path);
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn clean_we_label_strips_html_and_decodes_entities() {
        assert_eq!(
            clean_we_label("<img src='banner.png' width='200'><h3>🎚️&nbsp;Global &amp; Effects</h3>"),
            "🎚️ Global & Effects"
        );
        assert_eq!(
            clean_we_label("<font color='#ff0'>Line 1</font><br/><small>Line 2 &lt;tag&gt;</small>"),
            "Line 1\nLine 2 <tag>"
        );
        assert_eq!(clean_we_label("&nbsp;  <hr/> "), "");
    }

    #[test]
    fn extract_we_properties_sorts_localizes_and_parses_options() {
        let project = json!({
            "general": {
                "localization": {
                    "en-us": {
                        "ui_grp": "<b>Visuals</b>",
                        "ui_mode": "Quality Mode",
                        "ui_low": "Low",
                        "ui_high": "High"
                    }
                },
                "properties": {
                    "z_last": {
                        "type": "slider",
                        "text": "Glow",
                        "value": 0.5,
                        "min": 0.0,
                        "max": 2.0,
                        "step": 0.1,
                        "fraction": true,
                        "order": 103
                    },
                    "mode": {
                        "type": "combo",
                        "text": "ui_mode",
                        "value": "2",
                        "options": [
                            { "label": "ui_low", "value": "1" },
                            { "label": "ui_high", "value": "2" }
                        ],
                        "order": 101,
                        "condition": "enable.value == true"
                    },
                    "hdr": {
                        "type": "group",
                        "text": "ui_grp",
                        "order": 100
                    },
                    "shortcut": {
                        "type": "usershortcut",
                        "text": "Hotkey",
                        "order": 1
                    },
                    "tex": {
                        "type": "scenetexture",
                        "text": "Custom Texture",
                        "value": "",
                        "order": 2
                    },
                    "custom_name": {
                        "type": "textinput",
                        "text": "Clock Title",
                        "value": "Anima",
                        "order": 102
                    }
                }
            }
        });

        let props = extract_we_properties(&project);
        let keys: Vec<&str> = props.iter().map(|p| p.key.as_str()).collect();
        assert_eq!(keys, vec!["hdr", "mode", "custom_name", "z_last"]);

        assert_eq!(props[0].prop_type, "group");
        assert_eq!(props[0].text, "Visuals");

        assert_eq!(props[1].text, "Quality Mode");
        assert_eq!(props[1].options.len(), 2);
        assert_eq!(props[1].options[0].label, "Low");
        assert_eq!(props[1].options[1].value, "2");
        assert_eq!(props[1].condition.as_deref(), Some("enable.value == true"));

        assert_eq!(props[2].prop_type, "textinput");
        assert_eq!(props[2].default_value, "Anima");

        assert_eq!(props[3].step, Some(0.1));
        assert!(props[3].fraction);
    }

    #[test]
    fn eval_property_condition_handles_we_expressions() {
        let mut vals = BTreeMap::new();
        vals.insert("dibu".into(), "1".into());
        vals.insert("day1".into(), "true".into());
        vals.insert("newproperty".into(), "2".into());
        vals.insert("raineffects".into(), "true".into());
        vals.insert("effects".into(), "true".into());
        vals.insert("noise".into(), "false".into());
        vals.insert("newproperty19".into(), "true".into());
        vals.insert("numberoficons".into(), "5".into());
        vals.insert("h4background".into(), "true".into());
        vals.insert("usecustombg".into(), "true".into());
        vals.insert("background".into(), "0".into());

        assert!(eval_property_condition("!dibu.value==0", &vals));
        vals.insert("dibu".into(), "0".into());
        assert!(!eval_property_condition("!dibu.value==0", &vals));

        assert!(eval_property_condition("day1.value", &vals));
        assert!(eval_property_condition("newproperty.value == 1 || newproperty.value == 2", &vals));
        assert!(eval_property_condition("raineffects.value=true", &vals));
        assert!(!eval_property_condition("effects.value == true && noise.value == true", &vals));
        assert!(eval_property_condition(
            "newproperty19.value == true && [3, 4, 5].includes(numberoficons.value)",
            &vals
        ));
        assert!(eval_property_condition(
            "h4background.value == true && usecustombg.value == true && background.value == \"0\"",
            &vals
        ));
        assert!(eval_property_condition("??? unparseable", &vals));
    }

    #[test]
    fn clean_we_label_decodes_bare_nbsp_and_fancy_unicode() {
        assert_eq!(
            clean_we_label("&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp;&nbsp音频条颜色"),
            "音频条颜色"
        );
        assert_eq!(
            clean_we_label("𝑩𝒂𝒄𝒌𝒈𝒓𝒐𝒖𝒏𝒅 背景（测试）"),
            "Background 背景(测试)"
        );
    }

    #[test]
    fn translate_we_label_resolves_ui_tokens_and_cjk_labels() {
        let project = json!({
            "general": {
                "properties": {
                    "schemecolor": {
                        "type": "color",
                        "text": "ui_browse_properties_scheme_color",
                        "value": "0 0 0",
                        "order": 0
                    },
                    "clock": {
                        "type": "bool",
                        "text": "时间(Clock)",
                        "value": true,
                        "order": 1
                    },
                    "mode": {
                        "type": "combo",
                        "text": "人物动效选择",
                        "value": "1",
                        "options": [
                            { "label": "简略", "value": "1" },
                            { "label": "精细", "value": "2" }
                        ],
                        "order": 2
                    }
                }
            }
        });
        let props = extract_we_properties(&project);
        assert_eq!(props.len(), 3);
        assert!(!props[0].text.starts_with("ui_browse_properties_"));
        assert!(!contains_cjk(&props[1].text));
        assert!(!contains_cjk(&props[2].text));
        assert!(!contains_cjk(&props[2].options[0].label));
        assert!(!contains_cjk(&props[2].options[1].label));

        // Explicit Turkish & English CJK translation checks
        assert_eq!(translate_we_label_with_lang("时间(Clock)", true, false), "Saat");
        assert_eq!(translate_we_label_with_lang("时间(Clock)", false, false), "Time");
        assert_eq!(translate_we_label_with_lang("Background 背景", true, false), "Arka Plan");
        assert_eq!(translate_we_label_with_lang("颜色/Color", true, false), "Renk");
        assert_eq!(translate_we_label_with_lang("🔘 日期组件显示Date component display", true, false), "🔘 Tarih Bileşenini Göster");
    }

    #[test]
    fn workshop_properties_contain_no_untranslated_cjk_or_ui_tokens() {
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return;
        };
        let workshop = home.join(".local/share/Steam/steamapps/workshop/content/431960");
        if !workshop.is_dir() {
            return;
        }
        let Ok(entries) = std::fs::read_dir(&workshop) else {
            return;
        };
        for entry in entries.flatten() {
            let pjson = entry.path().join("project.json");
            if !pjson.is_file() {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&pjson) else {
                continue;
            };
            let Ok(project) = serde_json::from_str::<serde_json::Value>(&text) else {
                continue;
            };
            let props = extract_we_properties(&project);
            for prop in props {
                assert!(
                    !contains_cjk(&prop.text),
                    "CJK left in {:?} prop {} -> {:?}",
                    entry.file_name(),
                    prop.key,
                    prop.text
                );
                assert!(
                    !prop.text.starts_with("ui_") && !prop.text.starts_with("core_"),
                    "raw WE token left in {:?} prop {} -> {:?}",
                    entry.file_name(),
                    prop.key,
                    prop.text
                );
                for opt in prop.options {
                    assert!(
                        !contains_cjk(&opt.label),
                        "CJK left in {:?} prop {} opt -> {:?}",
                        entry.file_name(),
                        prop.key,
                        opt.label
                    );
                }
            }
        }
    }
}
