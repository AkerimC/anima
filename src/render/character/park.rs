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
