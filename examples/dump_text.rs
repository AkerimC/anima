//! Print the evaluated value of every Wallpaper Engine text layer in scenes.
//!
//! Usage: `cargo run --example dump_text -- <scene.json>...`

use std::collections::HashMap;

use anima::render::text;

fn main() -> anyhow::Result<()> {
    for scene in std::env::args().skip(1) {
        let root: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(&scene)?)?;
        let now = text::local_time();
        println!("=== {scene}");
        for obj in root["objects"].as_array().cloned().unwrap_or_default() {
            let Some(text_def) = obj.get("text").and_then(|t| t.as_object()) else {
                continue;
            };
            let Some(script) = text_def.get("script").and_then(|s| s.as_str()) else {
                continue;
            };
            let props: HashMap<String, serde_json::Value> = text_def
                .get("scriptproperties")
                .and_then(|p| p.as_object())
                .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default();
            println!(
                "{:?}: authored={:?} -> {:?}",
                obj.get("name").and_then(|n| n.as_str()).unwrap_or("?"),
                text_def.get("value"),
                text::eval_script(script, &props, now)
            );
        }
    }
    Ok(())
}
