//! Anima Park: a window of its own where the characters can gather, play on
//! swings, slides, seesaws and benches across a 2.5D meadow.

use std::path::PathBuf;

/// app_id of the park window.
pub const APP_ID: &str = "dev.anima.Park";

/// How far up the park lawn things can be placed (share of window height).
pub const MEADOW: f32 = 0.70;

/// Depth perspective scale across `0.0..=MEADOW` (1.0 at the front, smaller towards the horizon).
pub fn depth_scale(y: f32) -> f32 {
    (1.0 - (y / MEADOW).clamp(0.0, 1.0) * 0.22).max(0.75)
}

/// One modular segment of a tube slide.
#[derive(serde::Serialize, serde::Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TubeSeg {
    Straight,
    Turn,
}

/// Something built in the park: what, where (x share of width, y share of
/// height from bottom edge), turned round, piece count, and tube slide segments.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq)]
pub struct Thing {
    pub kind: String,
    #[serde(default = "park_place")]
    pub place: String,
    #[serde(default)]
    pub id: u64,
    pub x: f32,
    #[serde(default)]
    pub y: f32,
    #[serde(default)]
    pub flip: bool,
    #[serde(default = "one")]
    pub pieces: u32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tubes: Vec<TubeSeg>,
}

fn one() -> u32 {
    1
}

fn park_place() -> String {
    "park".into()
}

/// The park window the characters live in: id, window title.
pub const PLACES: [(&str, &str); 1] = [("park", "Anima Park")];

pub fn place_title(place: &str) -> &'static str {
    PLACES.iter().find(|p| p.0 == place).map_or("Anima Park", |p| p.1)
}

/// The park layout as saved.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq)]
pub struct Park {
    #[serde(default)]
    pub things: Vec<Thing>,
}

pub fn park_path() -> PathBuf {
    crate::util::data_dir().join("park.json")
}

pub fn read_park() -> Park {
    let mut p: Park = std::fs::read_to_string(park_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    p.things.retain(|t| t.place == "park" && recipe(&t.kind).is_some());
    p
}

pub fn write_park(p: &Park) {
    if let Ok(text) = serde_json::to_string_pretty(p) {
        let _ = crate::util::atomic_write(&park_path(), text.as_bytes());
    }
}

/// What can be built in the park: id, label.
pub type Recipe = (&'static str, &'static str);

/// "tube_straight" and "tube_turn" snap onto a slide; "slide_piece" is kept for older saves.
pub const RECIPES: [Recipe; 13] = [
    ("chair", "Chair"),
    ("bench", "Bench"),
    ("table", "Table"),
    ("swing", "Swing"),
    ("seesaw", "Seesaw"),
    ("slide", "Slide"),
    ("tube_straight", "Tube: Straight"),
    ("tube_turn", "Tube: Turn"),
    ("slide_piece", "Slide piece"),
    ("flowers", "Flower bed"),
    ("tree", "Tree"),
    ("lamp", "Lamp"),
    ("sandbox", "Sandbox"),
];

pub fn recipe(id: &str) -> Option<&'static Recipe> {
    RECIPES.iter().find(|r| r.0 == id)
}

// Sizes, in character heights (`unit`), measured from the ground.

pub const MAX_TUBE_PIECES: usize = 10;
pub const TUBE_RADIUS: f32 = 0.28;
pub const TUBE_EXIT_H: f32 = 0.06;

/// One tube segment along a slide's centerline, in centered `unit` coords.
#[derive(Clone, Debug)]
pub struct SlideCurve {
    pub kind: TubeSeg,
    pub pts: [(f32, f32); 4],
    pub dir_in: f32,
    pub dir_out: f32,
}

/// One sample point along the rider floor path (`h` is height above ground).
#[derive(Clone, Copy, Debug)]
pub struct SlideSample {
    pub x: f32,
    pub h: f32,
    pub tx: f32,
    pub th: f32,
    pub dist: f32,
}

/// Full layout of a modular tube slide in centered `unit` coordinates (`h >= 0` up).
#[derive(Clone, Debug)]
pub struct SlideLayout {
    pub top: f32,
    pub ladder_x: f32,
    pub lip_x: f32,
    pub exit_x: f32,
    pub exit_h: f32,
    pub exit_dir: f32,
    pub width: f32,
    pub total_len: f32,
    pub curves: Vec<SlideCurve>,
    pub joints: Vec<(f32, f32, f32, f32)>,
    pub samples: Vec<SlideSample>,
}

impl SlideLayout {
    /// Sample `(x, h, tx, th)` at `frac` (`0.0` lip .. `1.0` exit) by arc length.
    pub fn at(&self, frac: f32) -> (f32, f32, f32, f32) {
        let target = frac.clamp(0.0, 1.0) * self.total_len;
        let idx = self.samples.partition_point(|s| s.dist < target);
        if idx == 0 {
            let s = self.samples[0];
            return (s.x, s.h, s.tx, s.th);
        }
        if idx >= self.samples.len() {
            let s = *self.samples.last().unwrap();
            return (s.x, s.h, s.tx, s.th);
        }
        let a = self.samples[idx - 1];
        let b = self.samples[idx];
        let span = (b.dist - a.dist).max(1e-5);
        let t = ((target - a.dist) / span).clamp(0.0, 1.0);
        let x = a.x + (b.x - a.x) * t;
        let h = a.h + (b.h - a.h) * t;
        let tx = a.tx + (b.tx - a.tx) * t;
        let th = a.th + (b.th - a.th) * t;
        let n = (tx * tx + th * th).sqrt().max(1e-5);
        (x, h, tx / n, th / n)
    }
}

/// Segments forming a slide, falling back to `pieces` straight segments for older saves.
pub fn slide_segments(t: &Thing) -> Vec<TubeSeg> {
    if !t.tubes.is_empty() {
        t.tubes.iter().copied().take(MAX_TUBE_PIECES).collect()
    } else {
        vec![TubeSeg::Straight; (t.pieces.clamp(1, 5) as usize).min(MAX_TUBE_PIECES)]
    }
}

/// Cubic Bézier points and outgoing horizontal direction (`+1.0` or `-1.0`) for one tube segment.
pub fn next_tube_curve(x: f32, h: f32, dir: f32, kind: TubeSeg) -> ([(f32, f32); 4], f32) {
    match kind {
        TubeSeg::Straight => (
            [(x, h), (x + dir * 0.40, h - 0.16), (x + dir * 0.68, h - 0.52), (x + dir * 1.05, h - 0.70)],
            dir,
        ),
        TubeSeg::Turn => (
            [(x, h), (x + dir * 0.88, h - 0.12), (x + dir * 0.88, h - 0.66), (x + dir * 0.14, h - 0.78)],
            -dir,
        ),
    }
}

/// Position and unit tangent on a cubic Bézier at `t` (`0.0..=1.0`).
pub fn eval_cubic(p: [(f32, f32); 4], t: f32) -> ((f32, f32), (f32, f32)) {
    let u = 1.0 - t;
    let x = u * u * u * p[0].0 + 3.0 * u * u * t * p[1].0 + 3.0 * u * t * t * p[2].0 + t * t * t * p[3].0;
    let y = u * u * u * p[0].1 + 3.0 * u * u * t * p[1].1 + 3.0 * u * t * t * p[2].1 + t * t * t * p[3].1;
    let dx = 3.0 * u * u * (p[1].0 - p[0].0) + 6.0 * u * t * (p[2].0 - p[1].0) + 3.0 * t * t * (p[3].0 - p[2].0);
    let dy = 3.0 * u * u * (p[1].1 - p[0].1) + 6.0 * u * t * (p[2].1 - p[1].1) + 3.0 * t * t * (p[3].1 - p[2].1);
    let len = (dx * dx + dy * dy).sqrt().max(1e-5);
    ((x, y), (dx / len, dy / len))
}

/// Computes the full centered geometry of a modular tube slide `t`.
pub fn slide_layout(t: &Thing) -> SlideLayout {
    let segs = slide_segments(t);
    let ladder_raw = 0.0_f32;
    let lip_raw = 0.30_f32;
    let (mut cur_x, mut cur_h, mut dir) = (lip_raw, 0.0_f32, 1.0_f32);
    let mut raw_curves = Vec::with_capacity(segs.len());
    for kind in segs {
        let (pts, next_dir) = next_tube_curve(cur_x, cur_h, dir, kind);
        cur_x = pts[3].0;
        cur_h = pts[3].1;
        raw_curves.push(SlideCurve { kind, pts, dir_in: dir, dir_out: next_dir });
        dir = next_dir;
    }
    let shift_h = TUBE_EXIT_H - cur_h;
    let mut min_x = (ladder_raw - 0.16).min(lip_raw - TUBE_RADIUS);
    let mut max_x = (ladder_raw + 0.16).max(lip_raw + TUBE_RADIUS);
    for c in &mut raw_curves {
        for p in &mut c.pts {
            p.1 += shift_h;
        }
        for k in 0..=16 {
            let ((sx, _), _) = eval_cubic(c.pts, k as f32 / 16.0);
            min_x = min_x.min(sx - TUBE_RADIUS);
            max_x = max_x.max(sx + TUBE_RADIUS);
        }
    }
    let center = (min_x + max_x) * 0.5;
    let flip = if t.flip { -1.0 } else { 1.0 };
    let map_x = |x: f32| (x - center) * flip;

    let mut curves = Vec::with_capacity(raw_curves.len());
    let mut joints = Vec::with_capacity(raw_curves.len() + 1);
    let mut samples = Vec::with_capacity(raw_curves.len() * 20 + 1);
    let mut total_len = 0.0_f32;

    for (si, c) in raw_curves.into_iter().enumerate() {
        let pts = [
            (map_x(c.pts[0].0), c.pts[0].1),
            (map_x(c.pts[1].0), c.pts[1].1),
            (map_x(c.pts[2].0), c.pts[2].1),
            (map_x(c.pts[3].0), c.pts[3].1),
        ];
        if si == 0 {
            let ((x0, h0), (tx0, th0)) = eval_cubic(pts, 0.0);
            joints.push((x0, h0, tx0, th0));
            samples.push(SlideSample { x: x0, h: h0, tx: tx0, th: th0, dist: 0.0 });
        }
        const STEPS: usize = 20;
        for k in 1..=STEPS {
            let ((sx, sh), (tx, th)) = eval_cubic(pts, k as f32 / STEPS as f32);
            let prev = *samples.last().unwrap();
            total_len += ((sx - prev.x).powi(2) + (sh - prev.h).powi(2)).sqrt();
            samples.push(SlideSample { x: sx, h: sh, tx, th, dist: total_len });
        }
        let ((jx, jh), (jtx, jth)) = eval_cubic(pts, 1.0);
        joints.push((jx, jh, jtx, jth));
        curves.push(SlideCurve { kind: c.kind, pts, dir_in: c.dir_in * flip, dir_out: c.dir_out * flip });
    }
    let last_pt = samples.last().copied().unwrap();
    SlideLayout {
        top: shift_h,
        ladder_x: map_x(ladder_raw),
        lip_x: map_x(lip_raw),
        exit_x: last_pt.x,
        exit_h: last_pt.h,
        exit_dir: dir * flip,
        width: (max_x - min_x).max(0.8),
        total_len,
        curves,
        joints,
        samples,
    }
}

/// Legacy size helper kept for callers inspecting straight slide bounds.
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
        "swing" => 1.3,
        "seesaw" => SEESAW_HALF * 2.0 + 0.2,
        "slide" => slide_layout(t).width,
        "tube_straight" => 1.05,
        "tube_turn" => 0.95,
        "flowers" => 0.8,
        "tree" => 1.2,
        "lamp" => 0.3,
        "sandbox" => 1.4,
        _ => 0.5,
    }
}

/// How tall a thing reaches (character heights), for picking / dropping onto it.
pub fn height(t: &Thing) -> f32 {
    match t.kind.as_str() {
        "chair" => 0.9,
        "bench" => 0.75,
        "table" => 0.7,
        "swing" | "lamp" => 1.65,
        "seesaw" => 0.6,
        "slide" => slide_layout(t).top + 0.6,
        "tube_straight" => 0.85,
        "tube_turn" => 1.05,
        "tree" => 1.9,
        _ => 0.45,
    }
}

/// How many can use it at once (0: decorative).
pub fn room(kind: &str) -> usize {
    match kind {
        "chair" => 1,
        "bench" | "swing" | "seesaw" => 2,
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

/// What the daemon writes for the park window: swings swinging, seesaws rocking.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, Default, PartialEq)]
pub struct World {
    #[serde(default)]
    pub busy: Vec<usize>,
}

pub fn world_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    dir.join("anima-world.json")
}

pub fn read_world() -> World {
    std::fs::read_to_string(world_path()).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_slide(tubes: Vec<TubeSeg>, pieces: u32, flip: bool) -> Thing {
        Thing {
            kind: "slide".into(),
            place: "park".into(),
            id: 1,
            x: 0.5,
            y: 0.2,
            flip,
            pieces,
            tubes,
        }
    }

    #[test]
    fn legacy_thing_json_deserializes_with_pieces_fallback() {
        let raw = r#"{"kind":"slide","x":0.4,"y":0.1,"flip":false,"pieces":3}"#;
        let t: Thing = serde_json::from_str(raw).unwrap();
        assert_eq!(slide_segments(&t), vec![TubeSeg::Straight; 3]);
    }

    #[test]
    fn turn_segment_flips_direction_and_connects_continuously() {
        let t = sample_slide(vec![TubeSeg::Straight, TubeSeg::Turn, TubeSeg::Straight], 1, false);
        let layout = slide_layout(&t);
        assert_eq!(layout.curves.len(), 3);
        assert!((layout.exit_dir + 1.0).abs() < 1e-4, "expected left exit after 1 turn");
        assert!((layout.exit_h - TUBE_EXIT_H).abs() < 1e-4);
        for w in layout.curves.windows(2) {
            let a = w[0].pts[3];
            let b = w[1].pts[0];
            assert!((a.0 - b.0).abs() < 1e-5 && (a.1 - b.1).abs() < 1e-5);
        }
        let (x0, h0, _, _) = layout.at(0.0);
        let (x1, h1, _, _) = layout.at(1.0);
        assert!((x0 - layout.lip_x).abs() < 1e-4 && (h0 - layout.top).abs() < 1e-4);
        assert!((x1 - layout.exit_x).abs() < 1e-4 && (h1 - layout.exit_h).abs() < 1e-4);
    }

    #[test]
    fn flip_mirrors_slide_symmetrically() {
        let a = slide_layout(&sample_slide(vec![TubeSeg::Straight, TubeSeg::Turn], 1, false));
        let b = slide_layout(&sample_slide(vec![TubeSeg::Straight, TubeSeg::Turn], 1, true));
        assert!((a.width - b.width).abs() < 1e-4);
        assert!((a.top - b.top).abs() < 1e-4);
        assert!((a.ladder_x + b.ladder_x).abs() < 1e-4);
        assert!((a.exit_x + b.exit_x).abs() < 1e-4);
        assert!((a.exit_dir + b.exit_dir).abs() < 1e-4);
    }
}
