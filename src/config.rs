use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::render::character::CharacterConfig;
use crate::util;

/// Top level configuration, stored as TOML in `~/.config/anima/config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub general: General,
    pub character: CharacterConfig,
    #[serde(rename = "monitor")]
    pub monitors: Vec<MonitorConfig>,
    /// Per-wallpaper saved property overrides keyed by `"<monitor>::<wallpaper_id>"`.
    #[serde(default)]
    pub wallpaper_properties: BTreeMap<String, BTreeMap<String, String>>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            general: General::default(),
            character: CharacterConfig::default(),
            monitors: Vec::new(),
            wallpaper_properties: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct General {
    /// Target frame rate for animated wallpapers. Static images idle at 0 fps.
    pub fps: u32,
    /// Mute all video audio by default.
    pub mute: bool,
    /// Default volume for video wallpapers, 0.0..=1.0.
    pub volume: f32,
    /// Automatically mute wallpaper audio when another application plays audio.
    pub automute: bool,
    /// Enable desktop audio spectrum & beat capture for audio-reactive wallpapers.
    /// Automatically idles at 0% CPU when the active wallpaper does not use audio.
    pub audio_capture: bool,
    /// Freeze animated wallpapers while a fullscreen window is focused.
    pub pause_on_fullscreen: bool,
    /// Freeze animated wallpapers when the focused window is maximized.
    pub pause_on_maximized: bool,
    /// Reduce work when running on battery.
    pub pause_on_battery: bool,
    /// Frame rate cap while on battery.
    pub battery_fps: u32,
    /// Pause when the battery drops below this percentage (0 disables).
    pub low_battery_pause: u8,
    /// mpv hardware decoding mode: `auto-safe`, `auto`, `vaapi`, `nvdec`, `no`...
    pub hwdec: String,
    /// Compositor backend: `auto` (detect), `hyprland`, `niri` or `generic`.
    /// `ANIMA_COMPOSITOR` overrides this. Changing it needs an animad restart.
    #[serde(default = "default_compositor")]
    pub compositor: String,
    /// Fallback wallpaper used for monitors without an explicit entry.
    pub default_wallpaper: Option<String>,
    /// Where `anima list` looks for wallpapers.
    pub wallpapers_dir: Option<String>,
    /// Automatic slideshow / playlist interval in seconds (0 = disabled).
    pub playlist_interval_secs: u64,
    /// Shuffle playlist order when advancing.
    pub playlist_shuffle: bool,
    /// Explicit playlist entries (paths, names or Workshop IDs). If empty, all
    /// wallpapers in `wallpapers_dir` are used for `anima next` / `anima prev`.
    pub playlist: Vec<String>,
}

fn default_compositor() -> String {
    "auto".into()
}

impl Default for General {
    fn default() -> Self {
        Self {
            fps: 60,
            mute: true,
            volume: 0.0,
            automute: true,
            audio_capture: true,
            pause_on_fullscreen: true,
            pause_on_maximized: false,
            pause_on_battery: false,
            battery_fps: 30,
            low_battery_pause: 15,
            hwdec: "auto-safe".into(),
            compositor: default_compositor(),
            default_wallpaper: None,
            wallpapers_dir: None,
            playlist_interval_secs: 0,
            playlist_shuffle: false,
            playlist: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MonitorConfig {
    /// Output name as reported by the compositor, e.g. `eDP-1` or `DP-1`.
    pub name: String,
    pub wallpaper: Option<String>,
    pub fps: Option<u32>,
    pub volume: Option<f32>,
    pub mute: Option<bool>,
    /// Per-monitor shader/parallax scale override.
    pub scale: Option<f32>,
    /// Persisted user property overrides (`schemecolor`, `speed`, `parallax`...).
    #[serde(default)]
    pub properties: BTreeMap<String, String>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        let cfg: Config =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        Ok(cfg)
    }

    /// Load the config, creating a commented default file on first run.
    pub fn load_or_create(path: &Path) -> Result<Self> {
        if path.exists() {
            return Self::load(path);
        }
        let cfg = Config::default();
        cfg.save(path)?;
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = toml::to_string_pretty(self).context("serializing config")?;
        util::atomic_write(path, text.as_bytes())?;
        Ok(())
    }

    pub fn monitor(&self, name: &str) -> Option<&MonitorConfig> {
        self.monitors.iter().find(|m| m.name == name)
    }

    pub fn monitor_mut(&mut self, name: &str) -> Option<&mut MonitorConfig> {
        self.monitors.iter_mut().find(|m| m.name == name)
    }

    pub fn ensure_monitor(&mut self, name: &str) -> &mut MonitorConfig {
        if let Some(idx) = self.monitors.iter().position(|m| m.name == name) {
            return &mut self.monitors[idx];
        }
        self.monitors.push(MonitorConfig {
            name: name.to_string(),
            ..Default::default()
        });
        self.monitors.last_mut().unwrap()
    }

    /// Effective wallpaper path for a monitor, with `~` expanded.
    pub fn wallpaper_for(&self, monitor: &str) -> Option<PathBuf> {
        let raw = self
            .monitor(monitor)
            .and_then(|m| m.wallpaper.clone())
            .or_else(|| self.general.default_wallpaper.clone())?;
        Some(util::expand_path(&raw, Some(&util::config_dir())))
    }

    pub fn fps_for(&self, monitor: &str) -> u32 {
        self.monitor(monitor)
            .and_then(|m| m.fps)
            .unwrap_or(self.general.fps)
            .clamp(1, 360)
    }

    pub fn volume_for(&self, monitor: &str) -> f32 {
        self.monitor(monitor)
            .and_then(|m| m.volume)
            .unwrap_or(self.general.volume)
            .clamp(0.0, 1.0)
    }

    pub fn mute_for(&self, monitor: &str) -> bool {
        self.monitor(monitor)
            .and_then(|m| m.mute)
            .unwrap_or(self.general.mute)
    }

    pub fn wallpapers_dir(&self) -> PathBuf {
        self.general
            .wallpapers_dir
            .as_deref()
            .map(|p| util::expand_path(p, Some(&util::config_dir())))
            .unwrap_or_else(|| {
                dirs::picture_dir()
                    .unwrap_or_else(|| PathBuf::from("/tmp"))
                    .join("Wallpapers")
            })
    }

    fn wp_prop_key(monitor: &str, wp_id: &str) -> String {
        format!("{monitor}::{wp_id}")
    }

    /// Saved property overrides for a specific `(monitor, wallpaper_id)` pair.
    pub fn properties_for(&self, monitor: &str, wp_id: &str) -> BTreeMap<String, String> {
        self.wallpaper_properties
            .get(&Self::wp_prop_key(monitor, wp_id))
            .cloned()
            .unwrap_or_default()
    }

    /// Persist one property override for a specific `(monitor, wallpaper_id)` pair.
    pub fn set_property_for(&mut self, monitor: &str, wp_id: &str, key: &str, value: &str) {
        let entry = self
            .wallpaper_properties
            .entry(Self::wp_prop_key(monitor, wp_id))
            .or_default();
        if let Some(dup) = entry
            .keys()
            .find(|k| k.eq_ignore_ascii_case(key) && k.as_str() != key)
            .cloned()
        {
            entry.remove(&dup);
        }
        entry.insert(key.to_string(), value.to_string());
    }

    /// Reset one (or all, when `key` is `None`) saved property overrides for `(monitor, wp_id)`.
    pub fn reset_properties_for(&mut self, monitor: &str, wp_id: &str, key: Option<&str>) {
        let outer = Self::wp_prop_key(monitor, wp_id);
        match key {
            Some(target) => {
                if let Some(map) = self.wallpaper_properties.get_mut(&outer) {
                    if let Some(found) = map
                        .keys()
                        .find(|k| k.eq_ignore_ascii_case(target))
                        .cloned()
                    {
                        map.remove(&found);
                    }
                    if map.is_empty() {
                        self.wallpaper_properties.remove(&outer);
                    }
                }
            }
            None => {
                self.wallpaper_properties.remove(&outer);
            }
        }
    }
}
