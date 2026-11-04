use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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

/// A user-configurable property exposed by a wallpaper (`project.json` or `wallpaper.toml`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WallpaperProperty {
    pub key: String,
    pub text: String,
    pub prop_type: String,
    pub default_value: String,
    pub min: Option<f64>,
    pub max: Option<f64>,
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
                    prop_type: "text".into(),
                    default_value: v,
                    min: None,
                    max: None,
                });
            }
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

fn default_properties_for_kind(kind: WallpaperKind) -> Vec<WallpaperProperty> {
    match kind {
        WallpaperKind::Image => vec![
            WallpaperProperty {
                key: "fit".into(),
                text: "Scaling mode (cover, contain, stretch)".into(),
                prop_type: "combo".into(),
                default_value: "cover".into(),
                min: None,
                max: None,
            },
            WallpaperProperty {
                key: "brightness".into(),
                text: "Brightness (-1.0 .. 1.0)".into(),
                prop_type: "slider".into(),
                default_value: "0.0".into(),
                min: Some(-1.0),
                max: Some(1.0),
            },
            WallpaperProperty {
                key: "contrast".into(),
                text: "Contrast (0.0 .. 3.0)".into(),
                prop_type: "slider".into(),
                default_value: "1.0".into(),
                min: Some(0.0),
                max: Some(3.0),
            },
            WallpaperProperty {
                key: "saturation".into(),
                text: "Saturation (0.0 .. 3.0)".into(),
                prop_type: "slider".into(),
                default_value: "1.0".into(),
                min: Some(0.0),
                max: Some(3.0),
            },
        ],
        WallpaperKind::Video => vec![
            WallpaperProperty {
                key: "speed".into(),
                text: "Playback speed (0.1 .. 4.0)".into(),
                prop_type: "slider".into(),
                default_value: "1.0".into(),
                min: Some(0.1),
                max: Some(4.0),
            },
            WallpaperProperty {
                key: "volume".into(),
                text: "Audio volume (0.0 .. 1.0)".into(),
                prop_type: "slider".into(),
                default_value: "0.0".into(),
                min: Some(0.0),
                max: Some(1.0),
            },
            WallpaperProperty {
                key: "mute".into(),
                text: "Mute audio (true/false)".into(),
                prop_type: "bool".into(),
                default_value: "true".into(),
                min: None,
                max: None,
            },
        ],
        WallpaperKind::Shader => vec![
            WallpaperProperty {
                key: "speed".into(),
                text: "Animation speed multiplier".into(),
                prop_type: "slider".into(),
                default_value: "1.0".into(),
                min: Some(0.0),
                max: Some(5.0),
            },
            WallpaperProperty {
                key: "schemecolor".into(),
                text: "Primary theme color (R G B or #RRGGBB)".into(),
                prop_type: "color".into(),
                default_value: "0.35 0.65 1.0".into(),
                min: None,
                max: None,
            },
        ],
        WallpaperKind::Scene => vec![
            WallpaperProperty {
                key: "speed".into(),
                text: "Scene animation speed".into(),
                prop_type: "slider".into(),
                default_value: "1.0".into(),
                min: Some(0.0),
                max: Some(5.0),
            },
            WallpaperProperty {
                key: "parallax".into(),
                text: "Mouse parallax strength".into(),
                prop_type: "slider".into(),
                default_value: "1.0".into(),
                min: Some(0.0),
                max: Some(3.0),
            },
            WallpaperProperty {
                key: "schemecolor".into(),
                text: "Background scheme color".into(),
                prop_type: "color".into(),
                default_value: "0.05 0.06 0.09".into(),
                min: None,
                max: None,
            },
        ],
        WallpaperKind::Web => vec![
            WallpaperProperty {
                key: "schemecolor".into(),
                text: "Accent color (R G B or #RRGGBB)".into(),
                prop_type: "color".into(),
                default_value: "0.22 0.62 0.98".into(),
                min: None,
                max: None,
            },
            WallpaperProperty {
                key: "clock".into(),
                text: "Show digital clock HUD".into(),
                prop_type: "bool".into(),
                default_value: "true".into(),
                min: None,
                max: None,
            },
            WallpaperProperty {
                key: "bars".into(),
                text: "Show 64-band audio spectrum visualizer".into(),
                prop_type: "bool".into(),
                default_value: "true".into(),
                min: None,
                max: None,
            },
        ],
        WallpaperKind::Application => Vec::new(),
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

    for (key, obj) in props {
        let prop_type = obj
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("text")
            .to_string();
        if prop_type == "group" {
            continue;
        }
        let text = obj
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or(key)
            .to_string();
        let default_value = match obj.get("value") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(v) => v.to_string(),
            None => String::new(),
        };
        let min = obj.get("min").and_then(|v| v.as_f64());
        let max = obj.get("max").and_then(|v| v.as_f64());
        out.push(WallpaperProperty {
            key: key.clone(),
            text,
            prop_type,
            default_value,
            min,
            max,
        });
    }
    out
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
