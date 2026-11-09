//! The team (with each character's on/off state) and saved groups, in
//! ~/.config/anima/team.json.

use crate::config;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Serialize, Deserialize)]
pub struct Member {
    pub path: String,
    pub on: bool,
    /// no longer used (hand-made teams), kept so old files still load
    #[serde(default)]
    pub crew: String,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Team {
    #[serde(default)]
    pub team: Vec<Member>,
    /// saved line-ups, by name
    #[serde(default)]
    pub groups: BTreeMap<String, Vec<Member>>,
    /// no longer used, kept so old files still load
    #[serde(default)]
    pub crews: Vec<String>,
}

fn file() -> PathBuf {
    config::home().join(".config/anima/team.json")
}

impl Team {
    pub fn load() -> Self {
        if let Some(t) = std::fs::read_to_string(file()).ok().and_then(|s| serde_json::from_str(&s).ok()) {
            return t;
        }
        // first run: whoever the config already has
        let doc = config::read();
        let mut team = Vec::new();
        let source = config::text(&doc, "character", "source", "");
        if !source.is_empty() {
            team.push(Member { path: source, on: true, crew: String::new() });
        }
        if let Some(extra) = doc.get("character").and_then(|c| c.get("extra")).and_then(|e| e.as_array()) {
            team.extend(extra.iter().filter_map(|v| v.as_str()).map(|p| Member { path: p.to_string(), on: true, crew: String::new() }));
        }
        Team { team, groups: BTreeMap::new(), crews: Vec::new() }
    }

    pub fn save(&self) {
        if let Some(dir) = file().parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(s) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(file(), s);
        }
    }

    pub fn enabled(&self) -> Vec<String> {
        self.team.iter().filter(|m| m.on).map(|m| m.path.clone()).collect()
    }
}

pub struct Outfit {
    pub name: String,
    pub calm: PathBuf,
    pub happy: Option<PathBuf>,
}

fn sorted_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    files.sort();
    files
}

/// One character folder: sprite outfits, then 3D models.
pub fn outfits(dir: &Path) -> Vec<Outfit> {
    let mut out = Vec::new();
    let files = sorted_files(dir);
    for a in &files {
        let Some(file) = a.file_name().and_then(|n| n.to_str()) else { continue };
        if file.ends_with("_0010.png") {
            let b = a.with_file_name(file.replace("[A]", "[B]").replace("_0010.", "_0011."));
            let name = file.split("- Chibi").next().unwrap_or(file).trim().replace('_', "");
            out.push(Outfit { name, calm: a.clone(), happy: b.exists().then_some(b) });
        }
    }
    for m in files.iter().filter(|f| f.extension().is_some_and(|e| e == "glb")) {
        let name = m.file_stem().and_then(|n| n.to_str()).unwrap_or("").to_string();
        out.push(Outfit { name, calm: m.clone(), happy: None });
    }
    out
}

/// Every character folder, by name.
pub fn characters() -> Vec<PathBuf> {
    let data = config::home().join(".local/share/anima");
    let mut dirs: Vec<PathBuf> = ["uma", "ba"]
        .iter()
        .flat_map(|root| std::fs::read_dir(data.join(root)).into_iter().flatten().flatten())
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort_by_key(|p| p.file_name().map(|n| n.to_string_lossy().to_lowercase()));
    dirs
}
