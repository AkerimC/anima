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
    /// Snapshot rate for web (HTML/WebGL) wallpapers, in frames per second.
    /// WebKit renders these in software, so this is the main CPU dial.
    pub web_fps: u32,
    /// Internal render scale for web wallpapers, 0.25..=1.0 of the output.
    /// Lower renders fewer pixels and upscales; 1.0 is unscaled.
    pub web_scale: f32,
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
            web_fps: 30,
            web_scale: 1.0,
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

/// Coalesces config writes: a burst of changes (a GUI slider drag sends a
/// property update every 35 ms) costs one file write after the burst ends,
/// instead of a full serialise + fsync per change.
#[derive(Debug, Default)]
pub struct SaveDebounce {
    due: Option<std::time::Instant>,
}

impl SaveDebounce {
    /// Mark a change that should be written at `at`.
    pub fn note(&mut self, at: std::time::Instant) {
        self.due = Some(at);
    }

    /// Whether a write is still pending.
    pub fn is_pending(&self) -> bool {
        self.due.is_some()
    }

    /// How long until the pending write is due, if any.
    pub fn wait(&self, now: std::time::Instant) -> Option<std::time::Duration> {
        self.due.map(|at| at.saturating_duration_since(now))
    }

    /// True when the write is due; the pending flag is cleared then.
    pub fn due_now(&mut self, now: std::time::Instant) -> bool {
        match self.due {
            Some(at) if now >= at => {
                self.due = None;
                true
            }
            _ => false,
        }
    }

    /// Forget a pending write (it has just been written).
    pub fn clear(&mut self) {
        self.due = None;
    }
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

    /// Snapshot rate for web wallpapers, clamped to something usable.
    pub fn web_fps(&self) -> u32 {
        self.general.web_fps.clamp(1, 120)
    }

    /// Internal render scale for web wallpapers, clamped to 0.25..=1.0.
    pub fn web_scale(&self) -> f32 {
        let s = self.general.web_scale;
        if s.is_finite() {
            s.clamp(0.25, 1.0)
        } else {
            1.0
        }
    }

    /// Drop saved property overrides whose wallpaper no longer exists: a
    /// deleted local folder, or a removed Steam Workshop item.  Keys are
    /// `"<monitor>::<wallpaper_id>"`, where a workshop wallpaper uses its id
    /// and a local one its absolute entry path.
    ///
    /// Workshop ids are only pruned when some workshop directory exists, so a
    /// Steam library that is currently unmounted cannot wipe the overrides.
    /// Returns the number of entries removed.
    pub fn prune_missing_wallpapers(&mut self) -> usize {
        let workshop_dirs: Vec<PathBuf> = crate::wallpaper::steam_workshop_dirs()
            .into_iter()
            .filter(|d| d.is_dir())
            .collect();
        let mut removed = 0;
        self.wallpaper_properties.retain(|key, props| {
            if props.is_empty() {
                return false;
            }
            let Some((_monitor, wp_id)) = key.split_once("::") else {
                // not a key this version wrote: leave it be
                return true;
            };
            let is_workshop_id =
                !wp_id.is_empty() && wp_id.bytes().all(|b| b.is_ascii_digit());
            let exists = if is_workshop_id {
                workshop_dirs.is_empty() || workshop_dirs.iter().any(|d| d.join(wp_id).is_dir())
            } else {
                Path::new(wp_id).exists()
            };
            if !exists {
                removed += 1;
                log::info!("config: forgetting properties of missing wallpaper {wp_id}");
            }
            exists
        });
        removed
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

#[cfg(test)]
mod config_tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "anima-config-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn prune_keeps_live_wallpapers_and_drops_dead_paths() {
        let dir = temp_dir("prune");
        let live = dir.join("live");
        std::fs::create_dir_all(&live).unwrap();
        let live_id = live.to_string_lossy().into_owned();
        let dead_id = dir.join("gone").to_string_lossy().into_owned();

        let mut cfg = Config::default();
        cfg.set_property_for("DP-1", &live_id, "speed", "1.5");
        cfg.set_property_for("DP-1", &dead_id, "speed", "2.0");
        cfg.set_property_for("DP-1", "999999999999", "speed", "3.0");

        let removed = cfg.prune_missing_wallpapers();
        cfg.set_property_for("DP-1", &live_id, "speed", "1.5");

        assert!(!cfg.wallpaper_properties.contains_key(&format!("DP-1::{dead_id}")));
        assert!(cfg.wallpaper_properties.contains_key(&format!("DP-1::{live_id}")));
        // a workshop id survives when no workshop directory is mounted
        let workshop_key = format!("DP-1::999999999999");
        if crate::wallpaper::steam_workshop_dirs().iter().all(|d| !d.is_dir()) {
            assert!(cfg.wallpaper_properties.contains_key(&workshop_key));
            assert_eq!(removed, 1, "only the dead local path is pruned");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_override_sets_are_dropped() {
        let mut cfg = Config::default();
        cfg.wallpaper_properties
            .insert("DP-1::/nowhere".into(), BTreeMap::new());
        cfg.prune_missing_wallpapers();
        assert!(cfg.wallpaper_properties.is_empty());
    }

    #[test]
    fn web_settings_are_clamped() {
        let mut cfg = Config::default();
        cfg.general.web_fps = 0;
        assert_eq!(cfg.web_fps(), 1);
        cfg.general.web_fps = 500;
        assert_eq!(cfg.web_fps(), 120);
        cfg.general.web_scale = 0.0;
        assert_eq!(cfg.web_scale(), 0.25);
        cfg.general.web_scale = 4.0;
        assert_eq!(cfg.web_scale(), 1.0);
        cfg.general.web_scale = f32::NAN;
        assert_eq!(cfg.web_scale(), 1.0);
    }
}

#[cfg(test)]
mod debounce_tests {
    use super::SaveDebounce;
    use std::time::{Duration, Instant};

    #[test]
    fn writes_once_the_burst_is_over() {
        let mut d = SaveDebounce::default();
        let t0 = Instant::now();
        d.note(t0 + Duration::from_millis(500));
        assert!(d.is_pending());
        assert!(!d.due_now(t0 + Duration::from_millis(200)));
        // a second change pushes the deadline out
        d.note(t0 + Duration::from_millis(700));
        assert!(!d.due_now(t0 + Duration::from_millis(600)));
        assert!(d.due_now(t0 + Duration::from_millis(750)));
        assert!(!d.is_pending());
        assert!(!d.due_now(t0 + Duration::from_millis(900)));
    }

    #[test]
    fn clear_forgets_a_pending_write() {
        let mut d = SaveDebounce::default();
        d.note(Instant::now());
        d.clear();
        assert!(!d.is_pending());
        assert!(d.wait(Instant::now()).is_none());
    }
}
