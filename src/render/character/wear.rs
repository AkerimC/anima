//! Things worn and held: hats, glasses, a scarf, wings, an umbrella in the
//! rain, a Santa hat at Christmas, snow on the head. Each is placed on the
//! sprite's own anchors (head, eyes, neck, back, hand) and drawn with the
//! sprite's own transform, so it turns, flips, hops, breathes and lies down
//! with the character.
//!
//! The built-in set is drawn by the prop shader (rope.rs). Anyone can add
//! more: a PNG in `~/.local/share/anima/accessories/`, with an optional
//! TOML of the same name:
//!
//! ```toml
//! anchor = "crown"    # crown, eyes, neck, back or hand
//! size = [0.6, 0.5]   # width and height, in head widths and heights
//! pivot = [0.5, 1.0]  # the point of the picture that sits on the anchor
//! offset = [0.0, 0.1] # moved by this much (head widths, head heights down)
//! behind = false      # drawn behind the character (wings, backpacks)
//! ```
//!
//! Who wears what is kept in `wardrobe.json` (character name -> item ids).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime};

use super::walk::Act;
use super::CharacterOverlay;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Anchor {
    Crown,
    Eyes,
    Neck,
    Back,
    Hand,
}

impl Anchor {
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "crown" | "head" => Self::Crown,
            "eyes" | "face" => Self::Eyes,
            "neck" => Self::Neck,
            "back" => Self::Back,
            "hand" => Self::Hand,
            _ => return None,
        })
    }
}

/// One kind of thing to wear.
#[derive(Clone, Debug)]
pub struct Item {
    pub id: String,
    pub label: String,
    /// prop shader kind, or 100 for a picture
    kind: i32,
    image: Option<PathBuf>,
    anchor: Anchor,
    size: [f32; 2],
    pivot: [f32; 2],
    offset: [f32; 2],
    behind: bool,
}

/// What the wardrobe offers: id, label, kind, anchor, size, pivot, offset, behind.
const BUILT_IN: [(&str, &str, i32, Anchor, [f32; 2], [f32; 2], [f32; 2], bool); 12] = [
    ("party_hat", "Party hat", 27, Anchor::Crown, [0.5, 0.62], [0.5, 1.0], [0.1, 0.18], false),
    ("top_hat", "Top hat", 28, Anchor::Crown, [0.62, 0.55], [0.5, 1.0], [0.0, 0.16], false),
    ("crown", "Crown", 37, Anchor::Crown, [0.5, 0.32], [0.5, 1.0], [0.0, 0.14], false),
    ("bow", "Big bow", 29, Anchor::Crown, [0.44, 0.24], [0.5, 0.5], [0.3, 0.14], false),
    ("flower", "Flower", 32, Anchor::Crown, [0.28, 0.25], [0.5, 0.5], [-0.3, 0.16], false),
    ("headphones", "Headphones", 36, Anchor::Crown, [1.08, 0.62], [0.5, 0.12], [0.0, 0.0], false),
    ("glasses", "Round glasses", 30, Anchor::Eyes, [0.8, 0.22], [0.5, 0.5], [0.0, 0.0], false),
    ("sunglasses", "Sunglasses", 31, Anchor::Eyes, [0.82, 0.24], [0.5, 0.5], [0.0, 0.0], false),
    ("sleep_mask", "Sleep mask (asleep)", 26, Anchor::Eyes, [0.95, 0.3], [0.5, 0.5], [0.0, 0.0], false),
    ("scarf", "Scarf", 33, Anchor::Neck, [0.8, 0.32], [0.5, 0.3], [0.0, 0.0], false),
    ("backpack", "Backpack", 34, Anchor::Back, [0.62, 0.62], [0.5, 0.5], [0.0, 0.05], true),
    ("wings", "Angel wings", 35, Anchor::Back, [1.9, 0.9], [0.5, 0.5], [0.0, 0.0], true),
];

pub fn accessories_dir() -> PathBuf {
    crate::util::data_dir().join("accessories")
}

pub fn wardrobe_path() -> PathBuf {
    crate::util::data_dir().join("wardrobe.json")
}

/// Everything there is to wear: the built-in set and the pictures added.
pub fn catalog() -> Vec<Item> {
    let mut v: Vec<Item> = BUILT_IN
        .iter()
        .map(|&(id, label, kind, anchor, size, pivot, offset, behind)| Item {
            id: id.into(),
            label: label.into(),
            kind,
            image: None,
            anchor,
            size,
            pivot,
            offset,
            behind,
        })
        .collect();
    let Ok(dir) = std::fs::read_dir(accessories_dir()) else { return v };
    let mut pngs: Vec<PathBuf> = dir.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("png"))).collect();
    pngs.sort();
    for png in pngs {
        let stem = png.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        let spec: toml::Table = std::fs::read_to_string(png.with_extension("toml")).ok().and_then(|t| t.parse().ok()).unwrap_or_default();
        let pair = |k: &str, d: [f32; 2]| {
            spec.get(k).and_then(|v| v.as_array()).and_then(|a| Some([a.first()?.as_float()? as f32, a.get(1)?.as_float()? as f32])).unwrap_or(d)
        };
        // a square-ish hat by default, as wide as the head, sitting on it
        let (iw, ih) = image::image_dimensions(&png).unwrap_or((1, 1));
        let default_size = [0.8, 0.8 * ih as f32 / iw.max(1) as f32];
        v.push(Item {
            id: format!("custom:{stem}"),
            label: spec.get("label").and_then(|v| v.as_str()).map(String::from).unwrap_or_else(|| stem.replace(['_', '-'], " ")),
            kind: 100,
            image: Some(png.clone()),
            anchor: spec.get("anchor").and_then(|v| v.as_str()).and_then(Anchor::parse).unwrap_or(Anchor::Crown),
            size: pair("size", default_size),
            pivot: pair("pivot", [0.5, 1.0]),
            offset: pair("offset", [0.0, 0.12]),
            behind: spec.get("behind").and_then(|v| v.as_bool()).unwrap_or(false),
        });
    }
    v
}

/// The wardrobe as saved, read again whenever the file changes; the catalog
/// with it, so a picture dropped in shows up.
fn wardrobe() -> (HashMap<String, Vec<String>>, Vec<Item>) {
    type State = (Option<Instant>, Option<SystemTime>, HashMap<String, Vec<String>>, Vec<Item>);
    static STATE: std::sync::LazyLock<Mutex<State>> = std::sync::LazyLock::new(|| Mutex::new((None, None, HashMap::new(), Vec::new())));
    let mut s = STATE.lock().unwrap();
    if s.0.is_none_or(|t| t.elapsed() > Duration::from_secs(2)) {
        s.0 = Some(Instant::now());
        let stamp = std::fs::metadata(wardrobe_path()).and_then(|m| m.modified()).ok();
        let dir_stamp = std::fs::metadata(accessories_dir()).and_then(|m| m.modified()).ok();
        let both = stamp.max(dir_stamp);
        if both != s.1 || s.3.is_empty() {
            s.1 = both;
            s.2 = std::fs::read_to_string(wardrobe_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
            s.3 = catalog();
        }
    }
    (s.2.clone(), s.3.clone())
}

/// One thing to draw this frame: prop kind, its box in sprite space
/// (x0, y0, x1, y1 from -1 to 1, y up), mirrored, behind the character,
/// and the picture for kind 100.
#[derive(Clone, Debug)]
pub(super) struct Worn {
    pub(super) kind: i32,
    pub(super) rect: [f32; 4],
    pub(super) flip: bool,
    pub(super) behind: bool,
    pub(super) image: Option<PathBuf>,
}

impl CharacterOverlay {
    fn own_name(&self) -> String {
        let path = std::path::Path::new(&self.config.source);
        path.parent().and_then(|p| p.file_name()).or(path.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }

    /// What it has on right now: its wardrobe, and what the weather and the
    /// season put on it.
    pub(super) fn worn(&self) -> Vec<Worn> {
        let Some(a) = self.anchors else { return Vec::new() };
        let flip = self.pose.flip;
        let (hw, hh) = (a.head[2] - a.head[0], a.head[3] - a.head[1]);
        if hw <= 0.0 || hh <= 0.0 {
            return Vec::new();
        }
        let crown = [(a.head[0] + a.head[2]) * 0.5, a.head[1]];
        let eyes = [(a.eyes[0][0] + a.eyes[1][0]) * 0.5, (a.eyes[0][1] + a.eyes[1][1]) * 0.5];
        // the hand on the side it faces holds things
        let hand = if self.faces_right { a.hands[1] } else { a.hands[0] };
        let at = |anchor: Anchor| match anchor {
            Anchor::Crown => crown,
            Anchor::Eyes => eyes,
            Anchor::Neck => a.neck,
            Anchor::Back => a.back,
            Anchor::Hand => hand,
        };
        // texture units -> sprite space, mirrored as the sprite is
        let sx = |u: f32| (if flip { 1.0 - u } else { u }) * 2.0 - 1.0;
        let sy = |v: f32| 1.0 - 2.0 * v;
        let place = |anchor: Anchor, size: [f32; 2], pivot: [f32; 2], offset: [f32; 2]| {
            let p = at(anchor);
            let (w, h) = (size[0] * hw, size[1] * hh);
            let left = p[0] + offset[0] * hw - pivot[0] * w;
            let top = p[1] + offset[1] * hh - pivot[1] * h;
            let (a, b) = (sx(left), sx(left + w));
            [a.min(b), sy(top + h), a.max(b), sy(top)]
        };
        let mut out = Vec::new();
        let wk = &self.walk;
        let asleep = wk.act == Act::Rest && wk.rest.is_some_and(|r| r.kind == super::moods::RestKind::Sleep);
        let (wardrobe, items) = wardrobe();
        if let Some(ids) = wardrobe.get(&self.own_name()) {
            for id in ids {
                let Some(it) = items.iter().find(|i| &i.id == id) else { continue };
                // the sleep mask only goes on in its sleep
                if it.id == "sleep_mask" && !asleep {
                    continue;
                }
                out.push(Worn { kind: it.kind, rect: place(it.anchor, it.size, it.pivot, it.offset), flip, behind: it.behind, image: it.image.clone() });
            }
        }
        let weather = super::weather::now();
        // Christmas: a Santa hat, the tip falling away from where it looks
        let xmas = super::weather::christmas();
        let hatted = xmas || out.iter().any(|w| matches!(w.kind, 27 | 28 | 37 | 36) || (w.kind == 100));
        if xmas {
            out.push(Worn { kind: 25, rect: place(Anchor::Crown, [0.95, 0.72], [0.5, 1.0], [0.0, 0.2]), flip: !flip, behind: false, image: None });
        }
        // snow that has settled on its head (or hat)
        let pile = wk.snow_pile;
        if pile >= 0.05 {
            let (size, pivot, offset) = if hatted {
                ([0.45 * (0.7 + 0.3 * pile), 0.1 + 0.2 * pile], [0.5, 0.8], [0.0, -0.3])
            } else {
                ([0.6 + 0.3 * pile, 0.12 + 0.42 * pile], [0.5, 0.8], [0.0, 0.1])
            };
            out.push(Worn { kind: 24, rect: place(Anchor::Crown, size, pivot, offset), flip: false, behind: false, image: None });
        }
        // rain: an umbrella in the hand, unless it's busy playing or resting
        let busy = wk.race.is_some() || wk.in_game;
        let free = !matches!(wk.act, Act::Held | Act::Fall | Act::Travel | Act::Planted | Act::Rest);
        if weather.rain && !busy && free {
            // the top a set size just clear of the head, centred over the hand;
            // the stick from it down to the hand, however far that is
            let tex_aspect = self.sprite_tex.map_or(1.0, |(_, w, h)| w as f32 / h.max(1) as f32);
            let wide = 1.3 * hw;
            let tall = wide * 0.5 * tex_aspect;
            let rim = crown[1] - 0.015;
            let (x0, x1) = (sx(hand[0] - wide * 0.5), sx(hand[0] + wide * 0.5));
            out.push(Worn { kind: 38, rect: [x0.min(x1), sy(rim), x0.max(x1), sy(rim - tall)], flip: false, behind: false, image: None });
            let thin = wide * 0.04;
            // the hook turns towards the character
            let (a0, a1) = (sx(hand[0] - thin * 0.5), sx(hand[0] + thin * 0.5));
            let stick = [a0.min(a1) - (a1 - a0).abs() * 2.5, sy(hand[1] + hh * 0.04), a0.max(a1) + (a1 - a0).abs() * 2.5, sy(rim - tall * 0.9)];
            out.push(Worn { kind: 39, rect: stick, flip: self.faces_right == flip, behind: false, image: None });
        }
        out
    }
}
