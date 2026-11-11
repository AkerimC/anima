//! The park: a window of its own where the characters can live, and the
//! chest they fill with what they find (wood, rope, nails, paint) for the
//! user to build things with.

use std::collections::BTreeMap;
use std::path::PathBuf;

/// app_id of the park window.
pub const APP_ID: &str = "dev.anima.Park";

/// What can go in the chest: id, label. Pictures are props/materials/<id>.png.
pub const MATERIALS: [(&str, &str); 4] = [("wood", "Wood"), ("rope", "Rope"), ("nail", "Nails"), ("paint", "Paint")];

/// material -> who brought how many
pub type Chest = BTreeMap<String, BTreeMap<String, u32>>;

pub fn chest_path() -> PathBuf {
    crate::util::data_dir().join("chest.json")
}

pub fn read_chest() -> Chest {
    std::fs::read_to_string(chest_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

/// How many of `id` there are in all.
pub fn count(chest: &Chest, id: &str) -> u32 {
    chest.get(id).map_or(0, |by| by.values().sum())
}

/// `name` puts one `id` in the chest.
pub fn add(name: &str, id: &str) {
    let mut chest = read_chest();
    *chest.entry(id.to_string()).or_default().entry(name.to_string()).or_default() += 1;
    if let Ok(text) = serde_json::to_string_pretty(&chest) {
        let _ = crate::util::atomic_write(&chest_path(), text.as_bytes());
    }
}

pub fn picture(id: &str) -> PathBuf {
    crate::util::data_dir().join("props").join("materials").join(format!("{id}.png"))
}

/// Something built in the park: what, where (share of the park's width,
/// its middle), turned round, and how many pieces (a slide grows with them).
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
pub struct Thing {
    pub kind: String,
    pub x: f32,
    #[serde(default)]
    pub flip: bool,
    #[serde(default = "one")]
    pub pieces: u32,
}

fn one() -> u32 {
    1
}

/// The park as saved: what's built, and whether building is free (trying out).
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Park {
    #[serde(default)]
    pub things: Vec<Thing>,
    #[serde(default)]
    pub free: bool,
}

pub fn park_path() -> PathBuf {
    crate::util::data_dir().join("park.json")
}

pub fn read_park() -> Park {
    std::fs::read_to_string(park_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

pub fn write_park(p: &Park) {
    if let Ok(text) = serde_json::to_string_pretty(p) {
        let _ = crate::util::atomic_write(&park_path(), text.as_bytes());
    }
}

/// What can be built: id, label, cost. "slide_piece" makes a slide bigger.
pub const RECIPES: [(&str, &str, &[(&str, u32)]); 12] = [
    ("chair", "Chair", &[("wood", 2), ("nail", 1)]),
    ("bench", "Bench", &[("wood", 3), ("nail", 2)]),
    ("table", "Table", &[("wood", 4), ("nail", 2)]),
    ("stove", "Stove", &[("wood", 1), ("nail", 3), ("paint", 1)]),
    ("swing", "Swing", &[("wood", 4), ("rope", 2), ("nail", 2)]),
    ("seesaw", "Seesaw", &[("wood", 3), ("nail", 2), ("paint", 1)]),
    ("slide", "Slide", &[("wood", 4), ("nail", 3), ("paint", 1)]),
    ("slide_piece", "Slide piece", &[("wood", 2), ("nail", 1)]),
    ("flowers", "Flower bed", &[("wood", 1), ("paint", 1)]),
    ("tree", "Tree", &[("wood", 3)]),
    ("lamp", "Lamp", &[("nail", 2), ("paint", 1)]),
    ("sandbox", "Sandbox", &[("wood", 3), ("paint", 1)]),
];

pub fn recipe(id: &str) -> Option<&'static (&'static str, &'static str, &'static [(&'static str, u32)])> {
    RECIPES.iter().find(|r| r.0 == id)
}

pub fn affordable(chest: &Chest, cost: &[(&str, u32)]) -> bool {
    cost.iter().all(|&(m, k)| count(chest, m) >= k)
}

/// Takes `cost` out of the chest, from whoever brought the most first.
pub fn spend(cost: &[(&str, u32)]) -> bool {
    let mut chest = read_chest();
    if !affordable(&chest, cost) {
        return false;
    }
    for &(m, k) in cost {
        let by = chest.entry(m.to_string()).or_default();
        for _ in 0..k {
            if let Some(name) = by.iter().filter(|e| *e.1 > 0).max_by_key(|e| *e.1).map(|e| e.0.clone()) {
                if let Some(v) = by.get_mut(&name) {
                    *v -= 1;
                }
            }
        }
        by.retain(|_, v| *v > 0);
    }
    chest.retain(|_, by| !by.is_empty());
    if let Ok(text) = serde_json::to_string_pretty(&chest) {
        let _ = crate::util::atomic_write(&chest_path(), text.as_bytes());
    }
    true
}

/// Puts `cost` back (something taken down), as the park's own.
pub fn refund(cost: &[(&str, u32)]) {
    for &(m, k) in cost {
        for _ in 0..k {
            add("Park", m);
        }
    }
}

// Sizes, in character heights (`unit`), measured from the ground.

/// A slide of `pieces`: top of the ladder, and how far the chute reaches.
pub fn slide_size(pieces: u32) -> (f32, f32) {
    let p = pieces.clamp(1, 5) as f32 - 1.0;
    (0.75 + 0.3 * p, 1.1 + 0.45 * p)
}
pub const SWING_TOP: f32 = 1.6;
pub const SWING_ROPE: f32 = 1.2;
pub const SEESAW_HALF: f32 = 1.1;
pub const SEESAW_PIVOT: f32 = 0.3;
pub const SEESAW_TILT: f32 = 0.22;
pub const SEAT: f32 = 0.36;

/// How wide a thing is drawn (character heights).
pub fn width(t: &Thing) -> f32 {
    match t.kind.as_str() {
        "chair" => 0.45,
        "bench" => 1.0,
        "table" => 1.3,
        "stove" => 0.75,
        "swing" => 1.3,
        "seesaw" => SEESAW_HALF * 2.0 + 0.2,
        "slide" => 0.35 + slide_size(t.pieces).1,
        "flowers" => 0.8,
        "tree" => 1.2,
        "lamp" => 0.3,
        "sandbox" => 1.4,
        _ => 0.5,
    }
}

/// How many can use it at once (0: just to look at).
pub fn room(kind: &str) -> usize {
    match kind {
        "chair" => 1,
        "bench" | "swing" | "seesaw" => 2,
        "stove" => 3,
        "table" | "slide" | "sandbox" => 4,
        _ => 0,
    }
}

/// Character height in the park window's pixels, as the daemon last saw it.
pub fn view_path() -> PathBuf {
    crate::util::data_dir().join("park-view.json")
}

/// A character's height as a share of the park's width, if known.
pub fn read_unit_frac() -> Option<f32> {
    std::fs::read_to_string(view_path()).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()).and_then(|v| v["unit_frac"].as_f64()).map(|u| u as f32)
}

/// Which things are in use right now (swings swinging).
pub fn live_path() -> PathBuf {
    crate::util::data_dir().join("park-live.json")
}

pub fn read_busy() -> Vec<usize> {
    std::fs::read_to_string(live_path()).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok()).and_then(|v| serde_json::from_value(v["busy"].clone()).ok()).unwrap_or_default()
}
