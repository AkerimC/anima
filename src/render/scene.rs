//! Wallpaper Engine scene renderer.
//!
//! This renderer implements the generic Wallpaper Engine scene model instead of
//! special-casing individual wallpapers:
//!
//! * objects are drawn in scene order with full parent transforms,
//! * image layers are decoded from the package (`.tex`) and tinted/brightness
//!   adjusted like WE,
//! * puppet (`.mdl`) meshes are parsed and drawn as static rest-pose geometry,
//! * text layers evaluate WE clock/date/day-night scripts and are rasterised
//!   with `ab_glyph`,
//! * layer effects run through the generic WE effect-pass pipeline
//!   (`we_effect.rs`), including masks, named FBOs and multi-pass chains,
//! * `util/fullscreenlayer`, `util/projectlayer` and `util/composelayer`
//!   objects apply their effects to the composed scene so far,
//! * `util/solidlayer` objects draw solid colour geometry,
//! * particle objects are approximated with the built-in sprite emitters.
//!
//! The scene is always composed into an offscreen framebuffer and then blitted
//! to the Wayland surface.  Content is rendered with a top-left UV convention
//! (row 0 = top) so that effect masks, framebuffers and textures agree.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use glow::HasContext;
use serde::Deserialize;

use crate::pkg::PackageFs;

use super::gl::{bytemuck_cast, create_texture, Filter, Program};
use super::text;
use super::we_effect::{Canvas, EffectContext, EffectInstance, TextureCache};
use super::we_model::{parse_mdl, Mesh, ParsedMdl, PuppetAnimLayer};
use super::{FrameInfo, Renderer};

// ---------------------------------------------------------------------------
// Shaders
// ---------------------------------------------------------------------------

const CONTENT_VS: &str = r#"#version 300 es
layout(location = 0) in vec2 a_pos;
layout(location = 1) in vec2 a_uv;
uniform vec2 u_min;
uniform vec2 u_canvas;
uniform vec2 u_origin;
uniform vec2 u_axis_x;
uniform vec2 u_axis_y;
out vec2 v_uv;
void main() {
    vec2 world = u_origin + a_pos.x * u_axis_x + a_pos.y * u_axis_y;
    vec2 local = world - u_min;
    gl_Position = vec4(local.x / u_canvas.x * 2.0 - 1.0,
                       1.0 - local.y / u_canvas.y * 2.0, 0.0, 1.0);
    v_uv = a_uv;
}
"#;

const CONTENT_FS: &str = r#"#version 300 es
precision highp float;
in vec2 v_uv;
uniform sampler2D u_tex;
uniform vec4 u_color;
uniform float u_brightness;
out vec4 fragColor;
void main() {
    vec4 c = texture(u_tex, v_uv);
    c.rgb *= u_color.rgb * u_brightness;
    c.a *= u_color.a;
    fragColor = c;
}
"#;

const PUPPET_VS: &str = r#"#version 300 es
layout(location = 0) in vec2 a_pos;
layout(location = 1) in vec2 a_uv;
uniform vec2 u_min;
uniform vec2 u_canvas;
uniform vec2 u_origin;
uniform vec2 u_axis_x;
uniform vec2 u_axis_y;
out vec2 v_uv;
void main() {
    // Puppet vertices are corrected to a y-up local space when parsed.
    vec2 world = u_origin + a_pos.x * u_axis_x + a_pos.y * u_axis_y;
    vec2 local = world - u_min;
    gl_Position = vec4(local.x / u_canvas.x * 2.0 - 1.0,
                       1.0 - local.y / u_canvas.y * 2.0, 0.0, 1.0);
    v_uv = a_uv;
}
"#;

const COMPOSITE_VS: &str = r#"#version 300 es
layout(location = 0) in vec2 a_pos;
layout(location = 1) in vec2 a_uv;
uniform vec2 u_origin;
uniform vec2 u_axis_x;
uniform vec2 u_axis_y;
uniform vec2 u_ortho;
out vec2 v_uv;
void main() {
    vec2 world = u_origin + a_pos.x * u_axis_x + a_pos.y * u_axis_y;
    vec2 ndc = vec2(world.x / u_ortho.x * 2.0 - 1.0,
                    1.0 - world.y / u_ortho.y * 2.0);
    gl_Position = vec4(ndc, 0.0, 1.0);
    v_uv = a_uv;
}
"#;

const COMPOSITE_FS: &str = r#"#version 300 es
precision highp float;
in vec2 v_uv;
uniform sampler2D u_tex;
out vec4 fragColor;
void main() {
    fragColor = texture(u_tex, v_uv);
}
"#;

const COPY_VS: &str = r#"#version 300 es
layout(location = 0) in vec2 a_pos;
layout(location = 1) in vec2 a_uv;
out vec2 v_uv;
void main() {
    v_uv = a_uv;
    gl_Position = vec4(a_pos, 0.0, 1.0);
}
"#;

const COPY_FS: &str = r#"#version 300 es
precision highp float;
in vec2 v_uv;
uniform sampler2D u_tex;
out vec4 fragColor;
void main() {
    fragColor = texture(u_tex, v_uv);
}
"#;

const PARTICLE_VS: &str = r#"#version 300 es
precision highp float;
layout(location = 0) in vec2 a_quad;
layout(location = 1) in vec4 a_pos_size; // world x, y, half width, half height
layout(location = 2) in vec4 a_color;
layout(location = 3) in float a_rot;
uniform vec2 u_ortho;
out vec2 v_uv;
out vec4 v_color;
void main() {
    float c = cos(a_rot);
    float s = sin(a_rot);
    vec2 scaled = a_quad * a_pos_size.zw;
    vec2 rot = vec2(scaled.x * c - scaled.y * s, scaled.x * s + scaled.y * c);
    vec2 world = a_pos_size.xy + rot;
    vec2 ndc = vec2(world.x / u_ortho.x * 2.0 - 1.0,
                    1.0 - world.y / u_ortho.y * 2.0);
    gl_Position = vec4(ndc, 0.0, 1.0);
    v_uv = a_quad * 0.5 + 0.5;
    v_color = a_color;
}
"#;

const PARTICLE_FS: &str = r#"#version 300 es
precision highp float;
in vec2 v_uv;
in vec4 v_color;
uniform int u_shape;
out vec4 fragColor;
void main() {
    vec2 p = v_uv * 2.0 - 1.0;
    float alpha = 1.0;
    if (u_shape == 1) {
        float d = abs(p.x) * 3.5 + abs(p.y);
        alpha = smoothstep(1.0, 0.1, d);
    } else if (u_shape == 2) {
        float r = length(p);
        float rays = max(0.0, 1.0 - abs(p.x * p.y * 6.0));
        alpha = smoothstep(1.0, 0.0, r) * 0.7 + rays * smoothstep(1.0, 0.0, r) * 0.5;
    } else {
        float r = length(p);
        alpha = smoothstep(1.0, 0.05, r);
    }
    fragColor = vec4(v_color.rgb, v_color.a * alpha);
}
"#;

// ---------------------------------------------------------------------------
// Small affine math (world space: y-up, origin bottom-left)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
struct Affine {
    a: f32,
    b: f32,
    c: f32,
    d: f32,
    tx: f32,
    ty: f32,
}

impl Affine {
    fn identity() -> Self {
        Self { a: 1.0, b: 0.0, c: 0.0, d: 1.0, tx: 0.0, ty: 0.0 }
    }
    fn translate(x: f32, y: f32) -> Self {
        Self { tx: x, ty: y, ..Self::identity() }
    }
    fn rotate(r: f32) -> Self {
        let (s, c) = r.sin_cos();
        Self { a: c, b: s, c: -s, d: c, tx: 0.0, ty: 0.0 }
    }
    fn scale(x: f32, y: f32) -> Self {
        Self { a: x, b: 0.0, c: 0.0, d: y, tx: 0.0, ty: 0.0 }
    }
    /// `self * rhs`: apply `rhs` first, then `self`.
    fn mul(self, rhs: Self) -> Self {
        Self {
            a: self.a * rhs.a + self.c * rhs.b,
            b: self.b * rhs.a + self.d * rhs.b,
            c: self.a * rhs.c + self.c * rhs.d,
            d: self.b * rhs.c + self.d * rhs.d,
            tx: self.a * rhs.tx + self.c * rhs.ty + self.tx,
            ty: self.b * rhs.tx + self.d * rhs.ty + self.ty,
        }
    }
    fn apply(self, p: [f32; 2]) -> [f32; 2] {
        [
            self.a * p[0] + self.c * p[1] + self.tx,
            self.b * p[0] + self.d * p[1] + self.ty,
        ]
    }
    fn apply_vec(self, v: [f32; 2]) -> [f32; 2] {
        [self.a * v[0] + self.c * v[1], self.b * v[0] + self.d * v[1]]
    }
}

// ---------------------------------------------------------------------------
// Visibility rules and property helpers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum VisibleRule {
    Always,
    User {
        name: String,
        condition: Option<String>,
        value: bool,
    },
    Script(bool),
}

impl VisibleRule {
    fn eval(&self, props: &HashMap<String, serde_json::Value>) -> bool {
        match self {
            VisibleRule::Always => true,
            VisibleRule::Script(v) => *v,
            VisibleRule::User { name, condition, value } => {
                let Some(actual) = props.get(name) else {
                    return *value;
                };
                if let Some(condition) = condition {
                    return value_to_string(actual).trim() == condition.trim();
                }
                value_bool(actual).unwrap_or(*value)
            }
        }
    }
}

fn value_bool(v: &serde_json::Value) -> Option<bool> {
    match v {
        serde_json::Value::Bool(b) => Some(*b),
        serde_json::Value::Number(n) => Some(n.as_f64().unwrap_or(0.0) != 0.0),
        serde_json::Value::String(s) => {
            Some(s.eq_ignore_ascii_case("true") || s == "1" || s.eq_ignore_ascii_case("on"))
        }
        serde_json::Value::Object(o) => o.get("value").and_then(value_bool),
        _ => None,
    }
}

fn value_to_string(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Object(o) => o
            .get("value")
            .map(value_to_string)
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn value_f32(v: &serde_json::Value) -> Option<f32> {
    match v {
        serde_json::Value::Number(n) => n.as_f64().map(|f| f as f32),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        serde_json::Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        serde_json::Value::Object(o) => o.get("value").and_then(value_f32),
        _ => None,
    }
}

fn value_vec(v: &serde_json::Value, n: usize, default: [f32; 3]) -> [f32; 3] {
    match v {
        serde_json::Value::String(s) => parse_vec_str_n(s, n, default),
        serde_json::Value::Array(a) => {
            let mut out = default;
            for (i, item) in a.iter().take(3).enumerate() {
                out[i] = value_f32(item).unwrap_or(default[i]);
            }
            out
        }
        serde_json::Value::Object(o) => o
            .get("value")
            .map(|inner| value_vec(inner, n, default))
            .unwrap_or(default),
        _ => default,
    }
}

fn parse_vec_str_n(s: &str, n: usize, default: [f32; 3]) -> [f32; 3] {
    let mut out = default;
    let mut count = 0;
    for part in s
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|p| !p.is_empty())
    {
        if count >= n || count >= 3 {
            break;
        }
        if let Ok(v) = part.parse::<f32>() {
            out[count] = v;
            count += 1;
        }
    }
    out
}

fn parse_color(v: &serde_json::Value, default: [f32; 3]) -> [f32; 3] {
    let mut c = value_vec(v, 3, default);
    if c.iter().any(|x| *x > 1.0) {
        for x in &mut c {
            *x = (*x / 255.0).clamp(0.0, 1.0);
        }
    }
    c
}

/// Parse a `"r g b"` (or `#rrggbb`) colour string.  Values above 1.0 are
/// treated as 0..255 components, matching Wallpaper Engine's own property
/// serialisation.
pub fn parse_vec3_str(s: &str, default: [f32; 3]) -> [f32; 3] {
    let trimmed = s.trim();
    if let Some(hex) = trimmed.strip_prefix('#') {
        if hex.len() == 6 {
            if let (Ok(r), Ok(g), Ok(b)) = (
                u8::from_str_radix(&hex[0..2], 16),
                u8::from_str_radix(&hex[2..4], 16),
                u8::from_str_radix(&hex[4..6], 16),
            ) {
                return [r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0];
            }
        }
    }
    let mut vals: Vec<f32> = trimmed
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|p| !p.is_empty())
        .filter_map(|p| p.parse::<f32>().ok())
        .collect();
    if vals.len() >= 3 {
        if vals.iter().any(|&x| x > 1.0) {
            for x in &mut vals {
                *x = (*x / 255.0).clamp(0.0, 1.0);
            }
        }
        [vals[0], vals[1], vals[2]]
    } else {
        default
    }
}

// ---------------------------------------------------------------------------
// Particle emitters (approximation of WE sprite particle systems)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ParticleKind {
    Snow,
    Rain,
    Embers,
    Fireflies,
    Stars,
    Sakura,
    Fog,
}

impl ParticleKind {
    fn from_hint(s: &str) -> Self {
        let l = s.to_ascii_lowercase();
        if l.contains("fog") || l.contains("mist") || l.contains("smoke") || l.contains("cloud") {
            ParticleKind::Fog
        } else if l.contains("rain") || l.contains("storm") || l.contains("splash") {
            ParticleKind::Rain
        } else if l.contains("ember") || l.contains("fire") || l.contains("spark") || l.contains("lightning") {
            ParticleKind::Embers
        } else if l.contains("fly") || l.contains("glow") || l.contains("orb") || l.contains("bubble") {
            ParticleKind::Fireflies
        } else if l.contains("star") || l.contains("twinkle") {
            ParticleKind::Stars
        } else if l.contains("sakura")
            || l.contains("petal")
            || l.contains("blossom")
            || l.contains("flower")
            || l.contains("leaf")
            || l.contains("leave")
        {
            ParticleKind::Sakura
        } else {
            ParticleKind::Snow
        }
    }

    fn shape_id(self) -> i32 {
        match self {
            ParticleKind::Rain => 1,
            ParticleKind::Stars | ParticleKind::Fireflies => 2,
            _ => 0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Particle {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
    size: f32,
    alpha: f32,
    phase: f32,
    rot: f32,
    color: [f32; 3],
}

struct Emitter {
    kind: ParticleKind,
    center: [f32; 2],
    spread: [f32; 2],
    blend: LayerBlend,
    audio_reactive: bool,
    particles: Vec<Particle>,
}

impl Emitter {
    fn new(
        kind: ParticleKind,
        count: usize,
        center: [f32; 2],
        spread: [f32; 2],
        color: Option<[f32; 3]>,
        audio_reactive: bool,
    ) -> Self {
        let mut seed = 0x9E37_79B9u32 ^ (count as u32) ^ (center[0] as u32);
        let mut rng = || -> f32 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed as f32) / (u32::MAX as f32)
        };
        let mut particles = Vec::with_capacity(count);
        for _ in 0..count {
            let x = rng() * 2.0 - 1.0;
            let y = rng() * 2.0 - 1.0;
            let phase = rng() * std::f32::consts::TAU;
            let (vx, vy, size, alpha, default_col, rot) = match kind {
                ParticleKind::Snow => (
                    (rng() - 0.5) * 0.12,
                    -(0.16 + rng() * 0.36),
                    0.008 + rng() * 0.018,
                    0.35 + rng() * 0.55,
                    [0.92, 0.96, 1.0],
                    0.0,
                ),
                ParticleKind::Rain => (
                    -0.24 - rng() * 0.10,
                    -(1.70 + rng() * 1.30),
                    0.006 + rng() * 0.008,
                    0.25 + rng() * 0.35,
                    [0.70, 0.82, 0.96],
                    -0.15,
                ),
                ParticleKind::Embers => (
                    (rng() - 0.5) * 0.24,
                    0.20 + rng() * 0.50,
                    0.008 + rng() * 0.018,
                    0.45 + rng() * 0.55,
                    [1.0, 0.45 + rng() * 0.35, 0.12],
                    0.0,
                ),
                ParticleKind::Fireflies => (
                    (rng() - 0.5) * 0.10,
                    (rng() - 0.5) * 0.10,
                    0.012 + rng() * 0.020,
                    0.30 + rng() * 0.65,
                    [0.65, 1.0, 0.38],
                    0.0,
                ),
                ParticleKind::Stars => (
                    0.0,
                    0.0,
                    0.006 + rng() * 0.010,
                    0.25 + rng() * 0.75,
                    [0.88, 0.93, 1.0],
                    0.0,
                ),
                ParticleKind::Sakura => (
                    -0.16 - rng() * 0.20,
                    -(0.16 + rng() * 0.28),
                    0.010 + rng() * 0.016,
                    0.55 + rng() * 0.40,
                    [1.0, 0.72 + rng() * 0.15, 0.82],
                    rng() * std::f32::consts::PI,
                ),
                ParticleKind::Fog => (
                    (rng() - 0.5) * 0.04,
                    (rng() - 0.5) * 0.012,
                    0.40 + rng() * 0.30,
                    0.012 + rng() * 0.028,
                    [0.78, 0.81, 0.86],
                    0.0,
                ),
            };
            particles.push(Particle {
                x,
                y,
                vx,
                vy,
                size,
                alpha,
                phase,
                rot,
                color: color.unwrap_or(default_col),
            });
        }
        let blend = match kind {
            ParticleKind::Embers | ParticleKind::Fireflies | ParticleKind::Stars => {
                LayerBlend::Additive
            }
            _ => LayerBlend::Normal,
        };
        Self {
            kind,
            center,
            spread,
            blend,
            audio_reactive,
            particles,
        }
    }

    fn update_and_pack(
        &mut self,
        dt: f32,
        time: f32,
        ortho: [f32; 2],
        audio: [f32; 4],
        out: &mut Vec<f32>,
    ) {
        out.clear();
        let boost = if self.audio_reactive {
            1.0 + audio[0] * 2.0 + audio[3]
        } else {
            1.0
        };
        let dtc = dt.clamp(0.0, 0.1) * boost;
        let sx = self.spread[0].max(1.0);
        let sy = self.spread[1].max(1.0);
        for p in &mut self.particles {
            match self.kind {
                ParticleKind::Snow => {
                    p.x += (p.vx + (time * 1.3 + p.phase).sin() * 0.08) * dtc;
                    p.y += p.vy * dtc;
                }
                ParticleKind::Rain => {
                    p.x += p.vx * dtc;
                    p.y += p.vy * dtc;
                }
                ParticleKind::Embers => {
                    p.x += (p.vx + (time * 2.5 + p.phase).sin() * 0.16) * dtc;
                    p.y += p.vy * dtc;
                }
                ParticleKind::Fireflies => {
                    p.x += (p.vx + (time * 0.9 + p.phase).cos() * 0.08) * dtc;
                    p.y += (p.vy + (time * 1.1 + p.phase).sin() * 0.08) * dtc;
                }
                ParticleKind::Stars => {}
                ParticleKind::Fog => {
                    p.x += (p.vx + (time * 0.25 + p.phase).sin() * 0.012) * dtc;
                    p.y += p.vy * dtc;
                }
                ParticleKind::Sakura => {
                    p.x += (p.vx + (time * 1.7 + p.phase).sin() * 0.12) * dtc;
                    p.y += p.vy * dtc;
                    p.rot += dtc * 1.5;
                }
            }
            if p.y < -1.1 {
                p.y = 1.1;
            } else if p.y > 1.1 {
                p.y = -1.1;
            }
            if p.x < -1.1 {
                p.x = 1.1;
            } else if p.x > 1.1 {
                p.x = -1.1;
            }
            let twinkle = match self.kind {
                ParticleKind::Fireflies | ParticleKind::Stars | ParticleKind::Embers => {
                    0.55 + 0.45 * (time * 3.0 + p.phase).sin()
                }
                _ => 1.0,
            };
            let world_x = self.center[0] + p.x * sx;
            let world_y = self.center[1] + p.y * sy;
            let (w, h) = match self.kind {
                ParticleKind::Rain => (p.size * 0.35 * ortho[1], p.size * 4.8 * ortho[1]),
                ParticleKind::Sakura => (p.size * 1.3 * ortho[1], p.size * 0.8 * ortho[1]),
                ParticleKind::Fog => (p.size * ortho[0] * 0.5, p.size * ortho[1] * 0.5),
                _ => (p.size * ortho[1], p.size * ortho[1]),
            };
            out.extend_from_slice(&[
                world_x,
                world_y,
                w * 0.5,
                h * 0.5,
                p.color[0],
                p.color[1],
                p.color[2],
                (p.alpha * twinkle).clamp(0.0, 1.0),
                p.rot,
            ]);
        }
    }
}

// ---------------------------------------------------------------------------
// Layers
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayerBlend {
    Normal,
    Additive,
    Multiply,
    Screen,
}

impl LayerBlend {
    fn from_material(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "additive" | "add" | "light" => LayerBlend::Additive,
            "multiply" | "mult" => LayerBlend::Multiply,
            "screen" => LayerBlend::Screen,
            _ => LayerBlend::Normal,
        }
    }
    fn from_color_mode(mode: i64) -> Self {
        match mode {
            31 => LayerBlend::Additive,
            2 => LayerBlend::Multiply,
            6 => LayerBlend::Screen,
            _ => LayerBlend::Normal,
        }
    }
}

struct TextLayer {
    script: Option<String>,
    authored: String,
    props: HashMap<String, serde_json::Value>,
    font_path: String,
    font: Option<ab_glyph::FontVec>,
    point_size: f32,
    spacing: [f32; 2],
    padding: [f32; 2],
    max_width: f32,
    limit_width: bool,
    h_align: text::HAlign,
    v_align: text::VAlign,
    box_size: [f32; 2],
    /// Text extent in local scene units (before the object transform).
    world_size: [f32; 2],
    current: String,
    texture: Option<glow::Texture>,
    raster_size: [i32; 2],
    raster_ppw: f32,
}

enum ObjectKind {
    Image(glow::Texture),
    Puppet {
        mesh: Mesh,
        mdl: ParsedMdl,
        anim_layers: Vec<PuppetAnimLayer>,
        texture: glow::Texture,
    },
    Text(Box<TextLayer>),
    Solid,
    Particle(usize),
    Post,
}

struct SceneObject {
    name: String,
    transform: Affine,
    /// Local (unscaled) layer size in scene units.
    size: [f32; 2],
    color: [f32; 4],
    brightness: f32,
    blend: LayerBlend,
    parallax: [f32; 2],
    visible: VisibleRule,
    effects: Vec<EffectInstance>,
    kind: ObjectKind,
}

impl SceneObject {
    fn is_visible(&self, props: &HashMap<String, serde_json::Value>) -> bool {
        self.visible.eval(props)
    }

    fn aabb(&self, offset: [f32; 2]) -> ([f32; 2], [f32; 2]) {
        let mut transform = self.transform;
        transform.tx += offset[0];
        transform.ty += offset[1];
        let size = self.size;
        let hw = size[0].abs() * 0.5;
        let hh = size[1].abs() * 0.5;
        let corners = [
            transform.apply([-hw, -hh]),
            transform.apply([hw, -hh]),
            transform.apply([-hw, hh]),
            transform.apply([hw, hh]),
        ];
        let mut min = corners[0];
        let mut max = corners[0];
        for c in &corners[1..] {
            min[0] = min[0].min(c[0]);
            min[1] = min[1].min(c[1]);
            max[0] = max[0].max(c[0]);
            max[1] = max[1].max(c[1]);
        }
        (min, max)
    }

    /// Copy the subset of data needed to draw the object.
    ///
    /// Keeping this separate from `self` lets the renderer mutate its GL
    /// programs while the object list stays borrowed.
    fn draw_info(&self) -> DrawInfo {
        let kind = match &self.kind {
            ObjectKind::Image(tex) => DrawKind::Image(*tex),
            ObjectKind::Puppet {
                mesh, texture, ..
            } => DrawKind::Puppet {
                vao: mesh.vao,
                index_count: mesh.index_count,
                texture: *texture,
            },
            ObjectKind::Text(layer) => DrawKind::Text(layer.texture),
            ObjectKind::Solid => DrawKind::Solid,
            ObjectKind::Particle(_) | ObjectKind::Post => DrawKind::None,
        };
        DrawInfo {
            kind,
            size: self.size,
            color: self.color,
            brightness: self.brightness,
        }
    }
}

#[derive(Clone, Copy)]
enum DrawKind {
    Image(glow::Texture),
    Puppet {
        vao: glow::VertexArray,
        index_count: i32,
        texture: glow::Texture,
    },
    Text(Option<glow::Texture>),
    Solid,
    None,
}

#[derive(Clone, Copy)]
struct DrawInfo {
    kind: DrawKind,
    size: [f32; 2],
    color: [f32; 4],
    brightness: f32,
}

// ---------------------------------------------------------------------------
// Canvas pool
// ---------------------------------------------------------------------------

struct CanvasPool {
    free: Vec<Canvas>,
}

impl CanvasPool {
    fn new() -> Self {
        Self { free: Vec::new() }
    }

    fn take(&mut self, gl: &glow::Context, w: i32, h: i32) -> Result<Canvas> {
        let w = w.max(1);
        let h = h.max(1);
        if let Some(idx) = self
            .free
            .iter()
            .position(|c| c.w == w && c.h == h)
        {
            let c = self.free.swap_remove(idx);
            unsafe {
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(c.fbo));
                gl.viewport(0, 0, w, h);
                gl.disable(glow::BLEND);
                gl.clear_color(0.0, 0.0, 0.0, 0.0);
                gl.clear(glow::COLOR_BUFFER_BIT);
            }
            return Ok(c);
        }
        Canvas::new(gl, w, h)
    }

    fn put(&mut self, gl: &glow::Context, canvas: Canvas) {
        // Keep a small working set of canvases; delete anything beyond it so
        // scenes with many distinct effect sizes do not leak GPU memory.
        if self.free.len() < 16 {
            self.free.push(canvas);
        } else {
            canvas.destroy(gl);
        }
    }

    fn clear(&mut self, gl: &glow::Context) {
        for canvas in self.free.drain(..) {
            canvas.destroy(gl);
        }
    }
}

// ---------------------------------------------------------------------------
// Renderer
// ---------------------------------------------------------------------------

pub struct SceneRenderer {
    content_prog: Program,
    puppet_prog: Program,
    composite_prog: Program,
    copy_prog: Program,
    particle_prog: Program,

    quad_vao: glow::VertexArray,
    #[allow(dead_code)]
    quad_vbo: glow::Buffer,
    particle_vao: glow::VertexArray,
    particle_quad_vbo: glow::Buffer,
    particle_inst_vbo: glow::Buffer,
    instance_scratch: Vec<f32>,

    scene: Option<Canvas>,
    scene_alt: Option<Canvas>,
    pool: CanvasPool,
    textures: TextureCache,

    ortho_size: [f32; 2],
    clear_color: [f32; 3],
    parallax_enabled: bool,
    parallax_amount: f32,
    parallax_mouse_influence: f32,
    camera_shake: bool,
    camera_shake_amp: f32,
    camera_shake_speed: f32,
    smooth_mouse: [f32; 2],
    speed_multiplier: f32,

    objects: Vec<SceneObject>,
    emitters: Vec<Emitter>,
    properties: HashMap<String, serde_json::Value>,
    pkg: PackageFs,

    width: i32,
    height: i32,
    paused: bool,
    title: String,
}

impl SceneRenderer {
    pub fn new(
        gl: &glow::Context,
        root: &Path,
        entry: &Path,
        width: i32,
        height: i32,
        project: Option<&serde_json::Value>,
    ) -> Result<Self> {
        let pkg = PackageFs::open(root)?;
        let content_prog = Program::new(gl, CONTENT_VS, CONTENT_FS)?;
        let puppet_prog = Program::new(gl, PUPPET_VS, CONTENT_FS)?;
        let composite_prog = Program::new(gl, COMPOSITE_VS, COMPOSITE_FS)?;
        let copy_prog = Program::new(gl, COPY_VS, COPY_FS)?;
        let particle_prog = Program::new(gl, PARTICLE_VS, PARTICLE_FS)?;
        let (quad_vao, quad_vbo) = create_quad(gl)?;
        let (particle_vao, particle_quad_vbo, particle_inst_vbo) = create_particle_buffers(gl)?;

        let mut this = Self {
            content_prog,
            puppet_prog,
            composite_prog,
            copy_prog,
            particle_prog,
            quad_vao,
            quad_vbo,
            particle_vao,
            particle_quad_vbo,
            particle_inst_vbo,
            instance_scratch: Vec::with_capacity(4096),
            scene: None,
            scene_alt: None,
            pool: CanvasPool::new(),
            textures: TextureCache::new(),
            ortho_size: [1920.0, 1080.0],
            clear_color: [0.04, 0.05, 0.08],
            parallax_enabled: false,
            parallax_amount: 0.5,
            parallax_mouse_influence: 0.4,
            camera_shake: false,
            camera_shake_amp: 0.01,
            camera_shake_speed: 1.5,
            smooth_mouse: [0.0, 0.0],
            speed_multiplier: 1.0,
            objects: Vec::new(),
            emitters: Vec::new(),
            properties: HashMap::new(),
            pkg: PackageFs::default(),
            width,
            height,
            paused: false,
            title: entry
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("scene")
                .to_string(),
        };

        if let Some(proj) = project {
            if let Some(props) = proj
                .get("general")
                .and_then(|g| g.get("properties"))
                .and_then(|p| p.as_object())
            {
                for (k, v) in props {
                    let value = v.get("value").cloned().unwrap_or(serde_json::Value::Null);
                    this.properties.insert(k.clone(), value);
                }
            }
        }

        let entry_name = entry
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("scene.json");
        if entry_name.ends_with(".toml") || entry_name.ends_with(".anima") {
            let s = std::fs::read_to_string(entry)
                .with_context(|| format!("reading native scene {}", entry.display()))?;
            this.load_native_toml(gl, &pkg, &s)?;
        } else {
            let json_text = if entry.is_file() && !entry_name.ends_with(".pkg") {
                std::fs::read_to_string(entry)?
            } else if pkg.exists("scene.json") {
                pkg.read_to_string("scene.json")?
            } else if pkg.exists("gifscene.json") {
                pkg.read_to_string("gifscene.json")?
            } else {
                anyhow::bail!("no scene.json found in {}", root.display());
            };
            this.load_we_scene(gl, &pkg, &json_text)?;
        }

        this.pkg = pkg;
        Ok(this)
    }

    fn load_texture(
        gl: &glow::Context,
        pkg: &PackageFs,
        textures: &mut TextureCache,
        path: &str,
    ) -> Option<(glow::Texture, u32, u32)> {
        let mut candidates = Vec::new();
        let clean = path.trim().trim_start_matches('/');
        candidates.push(clean.to_string());
        let no_ext = clean
            .strip_suffix(".tex")
            .or_else(|| clean.strip_suffix(".png"))
            .or_else(|| clean.strip_suffix(".jpg"))
            .unwrap_or(clean);
        candidates.push(format!("materials/{no_ext}.tex"));
        candidates.push(format!("{no_ext}.tex"));
        if !no_ext.starts_with("materials/") {
            candidates.push(format!("materials/{no_ext}.tex"));
        }
        for candidate in candidates {
            if candidate.is_empty() {
                continue;
            }
            if let Some(found) = textures.get(gl, pkg, &candidate) {
                return Some(found);
            }
        }
        None
    }

    fn material_texture(pkg: &PackageFs, model: &serde_json::Value) -> Option<String> {
        let material_ref = model.get("material")?.as_str()?;
        let material: serde_json::Value = serde_json::from_str(&pkg.read_to_string(material_ref).ok()?).ok()?;
        let pass = material.get("passes")?.as_array()?.first()?;
        let textures = pass.get("textures")?.as_array()?;
        textures.first()?.as_str().map(str::to_string)
    }

    fn load_we_scene(&mut self, gl: &glow::Context, pkg: &PackageFs, json_text: &str) -> Result<()> {
        let root: serde_json::Value =
            serde_json::from_str(json_text).context("parsing Wallpaper Engine scene.json")?;

        if let Some(general) = root.get("general") {
            if let Some(ortho) = general.get("orthogonalprojection") {
                let w = ortho.get("width").and_then(value_f32).unwrap_or(1920.0);
                let h = ortho.get("height").and_then(value_f32).unwrap_or(1080.0);
                self.ortho_size = [w.max(1.0), h.max(1.0)];
            }
            if let Some(cc) = general.get("clearcolor") {
                self.clear_color = parse_color(cc, [0.04, 0.05, 0.08]);
            }
            if let Some(v) = general.get("cameraparallax").and_then(value_bool) {
                self.parallax_enabled = v;
            }
            if let Some(v) = general.get("cameraparallaxamount").and_then(value_f32) {
                self.parallax_amount = v;
            }
            if let Some(v) = general.get("cameraparallaxmouseinfluence").and_then(value_f32) {
                self.parallax_mouse_influence = v;
            }
            if let Some(v) = general.get("camerashake").and_then(value_bool) {
                self.camera_shake = v;
            }
            if let Some(v) = general.get("camerashakeamplitude").and_then(value_f32) {
                self.camera_shake_amp = v;
            }
            if let Some(v) = general.get("camerashakespeed").and_then(value_f32) {
                self.camera_shake_speed = v;
            }
        }

        let objects = root
            .get("objects")
            .and_then(|o| o.as_array())
            .cloned()
            .unwrap_or_default();

        // First pass: raw local transforms and parent ids.
        struct Raw {
            obj: serde_json::Value,
            parent: Option<String>,
            local: Affine,
        }
        let mut raws = Vec::with_capacity(objects.len());
        for obj in &objects {
            let parent = obj
                .get("parent")
                .and_then(|p| {
                    p.as_str()
                        .map(str::to_string)
                        .or_else(|| p.as_i64().map(|n| n.to_string()))
                });
            let origin = obj
                .get("origin")
                .map(|v| value_vec(v, 3, [self.ortho_size[0] * 0.5, self.ortho_size[1] * 0.5, 0.0]))
                .unwrap_or([self.ortho_size[0] * 0.5, self.ortho_size[1] * 0.5, 0.0]);
            let scale = obj
                .get("scale")
                .map(|v| value_vec(v, 3, [1.0, 1.0, 1.0]))
                .unwrap_or([1.0, 1.0, 1.0]);
            let angles = obj
                .get("angles")
                .map(|v| value_vec(v, 3, [0.0, 0.0, 0.0]))
                .unwrap_or([0.0, 0.0, 0.0]);
            let local = Affine::translate(origin[0], origin[1])
                .mul(Affine::rotate(angles[2]))
                .mul(Affine::scale(
                    if scale[0] == 0.0 { 1.0 } else { scale[0] },
                    if scale[1] == 0.0 { 1.0 } else { scale[1] },
                ));
            raws.push(Raw {
                obj: obj.clone(),
                parent,
                local,
            });
        }

        // Resolve world transforms with parent chains.
        let mut id_index: HashMap<String, usize> = HashMap::new();
        for (i, r) in raws.iter().enumerate() {
            if let Some(id) = r.obj.get("id") {
                id_index.insert(value_to_string(id), i);
            }
        }
        let mut resolved: Vec<Option<Affine>> = vec![None; raws.len()];
        fn resolve(
            i: usize,
            raws: &[impl RawLike],
            index: &HashMap<String, usize>,
            out: &mut Vec<Option<Affine>>,
            depth: usize,
        ) -> Affine {
            if let Some(t) = out[i] {
                return t;
            }
            if depth > 32 {
                return Affine::identity();
            }
            let parent = raws[i]
                .parent_id()
                .and_then(|id| index.get(&id).copied())
                .map(|p| resolve(p, raws, index, out, depth + 1))
                .unwrap_or_else(Affine::identity);
            let t = parent.mul(raws[i].local_transform());
            out[i] = Some(t);
            t
        }
        trait RawLike {
            fn parent_id(&self) -> Option<String>;
            fn local_transform(&self) -> Affine;
        }
        impl RawLike for Raw {
            fn parent_id(&self) -> Option<String> {
                self.parent.clone()
            }
            fn local_transform(&self) -> Affine {
                self.local
            }
        }
        for i in 0..raws.len() {
            resolved[i] = Some(resolve(i, &raws, &id_index, &mut resolved, 0));
        }

        let now = text::local_time();
        for (i, raw) in raws.iter().enumerate() {
            let obj = &raw.obj;
            let transform = resolved[i].unwrap_or_else(Affine::identity);
            let name = obj
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("object")
                .to_string();

            let visible = parse_visible(obj.get("visible"), now);

            if let Some(particle_path) = obj
                .get("particle")
                .or_else(|| obj.get("particlesrc"))
                .and_then(|v| v.as_str())
            {
                let kind = ParticleKind::from_hint(&format!("{name} {particle_path}"));
                let preset = pkg
                    .read_to_string(particle_path)
                    .ok()
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
                let count = preset
                    .as_ref()
                    .and_then(|p| p.get("maxcount"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(80)
                    .clamp(1, 800) as usize;
                let count = if kind == ParticleKind::Fog { count.min(20) } else { count };
                let size = obj
                    .get("size")
                    .map(|v| value_vec(v, 3, [600.0, 600.0, 0.0]))
                    .unwrap_or([1200.0, 900.0, 0.0]);
                let scale = obj
                    .get("scale")
                    .map(|v| value_vec(v, 3, [1.0, 1.0, 1.0]))
                    .unwrap_or([1.0, 1.0, 1.0]);
                let center = [transform.tx, transform.ty];
                let spread = [
                    size[0].abs() * scale[0].abs() * 0.5,
                    size[1].abs() * scale[1].abs() * 0.5,
                ];
                let color = obj.get("color").map(|c| parse_color(c, [1.0, 1.0, 1.0]));
                let idx = self.emitters.len();
                self.emitters.push(Emitter::new(
                    kind,
                    count,
                    center,
                    spread,
                    color,
                    true,
                ));
                self.objects.push(SceneObject {
                    name,
                    transform,
                    size: [1.0, 1.0],
                    color: [1.0, 1.0, 1.0, 0.0],
                    brightness: 1.0,
                    blend: LayerBlend::Normal,
                    parallax: [0.0, 0.0],
                    visible,
                    effects: Vec::new(),
                    kind: ObjectKind::Particle(idx),
                });
                continue;
            }

            if let Some(text_def) = obj.get("text") {
                if let Some(layer) = Self::load_text(pkg, obj, text_def, now) {
                    let size = obj
                        .get("size")
                        .map(|v| value_vec(v, 3, [layer.box_size[0], layer.box_size[1], 0.0]))
                        .map(|v| [v[0], v[1]])
                        .unwrap_or(layer.box_size);
                    self.objects.push(SceneObject {
                        name,
                        transform,
                        size,
                        color: [
                            parse_color(
                                obj.get("color").unwrap_or(&serde_json::Value::Null),
                                [1.0, 1.0, 1.0],
                            )[0],
                            parse_color(
                                obj.get("color").unwrap_or(&serde_json::Value::Null),
                                [1.0, 1.0, 1.0],
                            )[1],
                            parse_color(
                                obj.get("color").unwrap_or(&serde_json::Value::Null),
                                [1.0, 1.0, 1.0],
                            )[2],
                            obj.get("alpha").and_then(value_f32).unwrap_or(1.0),
                        ],
                        brightness: obj.get("brightness").and_then(value_f32).unwrap_or(1.0),
                        blend: LayerBlend::from_color_mode(
                            obj.get("colorBlendMode").and_then(|v| v.as_i64()).unwrap_or(0),
                        ),
                        parallax: obj
                            .get("parallaxDepth")
                            .map(|v| {
                                let p = value_vec(v, 3, [0.0, 0.0, 0.0]);
                                [p[0], p[1]]
                            })
                            .unwrap_or([0.0, 0.0]),
                        visible,
                        effects: load_effects(gl, pkg, obj, &self.properties, now),
                        kind: ObjectKind::Text(Box::new(layer)),
                    });
                }
                continue;
            }

            let Some(image_ref) = obj.get("image").and_then(|v| v.as_str()) else {
                continue;
            };
            let lower = image_ref.to_ascii_lowercase();
            let is_post = lower.contains("util/fullscreenlayer")
                || lower.contains("util/projectlayer")
                || lower.contains("util/composelayer");
            if is_post {
                let effects = load_effects(gl, pkg, obj, &self.properties, now);
                if effects.is_empty() {
                    continue;
                }
                self.objects.push(SceneObject {
                    name,
                    transform,
                    size: [self.ortho_size[0], self.ortho_size[1]],
                    color: [1.0, 1.0, 1.0, 1.0],
                    brightness: 1.0,
                    blend: LayerBlend::Normal,
                    parallax: [0.0, 0.0],
                    visible,
                    effects,
                    kind: ObjectKind::Post,
                });
                continue;
            }
            if lower.contains("util/solidlayer") {
                let size = obj
                    .get("size")
                    .map(|v| value_vec(v, 3, [100.0, 100.0, 0.0]))
                    .unwrap_or([self.ortho_size[0], self.ortho_size[1], 0.0]);
                self.objects.push(SceneObject {
                    name,
                    transform,
                    size: [size[0], size[1]],
                    color: [
                        parse_color(
                            obj.get("color").unwrap_or(&serde_json::Value::Null),
                            [1.0, 1.0, 1.0],
                        )[0],
                        parse_color(
                            obj.get("color").unwrap_or(&serde_json::Value::Null),
                            [1.0, 1.0, 1.0],
                        )[1],
                        parse_color(
                            obj.get("color").unwrap_or(&serde_json::Value::Null),
                            [1.0, 1.0, 1.0],
                        )[2],
                        obj.get("alpha").and_then(value_f32).unwrap_or(1.0),
                    ],
                    brightness: obj.get("brightness").and_then(value_f32).unwrap_or(1.0),
                    blend: LayerBlend::from_color_mode(
                        obj.get("colorBlendMode").and_then(|v| v.as_i64()).unwrap_or(0),
                    ),
                    parallax: obj
                        .get("parallaxDepth")
                        .map(|v| {
                            let p = value_vec(v, 3, [0.0, 0.0, 0.0]);
                            [p[0], p[1]]
                        })
                        .unwrap_or([0.0, 0.0]),
                    visible,
                    effects: load_effects(gl, pkg, obj, &self.properties, now),
                    kind: ObjectKind::Solid,
                });
                continue;
            }

            // Model / image.
            let mut model_size: Option<[f32; 2]> = None;
            let mut material_blend = String::from("normal");
            let mut texture_path: Option<String> = None;
            let mut puppet_path: Option<String> = None;
            let mut solid_model = false;
            let mut model_json: Option<serde_json::Value> = None;
            if lower.ends_with(".json") {
                if let Ok(text) = pkg.read_to_string(image_ref) {
                    if let Ok(model) = serde_json::from_str::<serde_json::Value>(&text) {
                        model_json = Some(model.clone());
                        if let Some(p) = model.get("puppet").and_then(|v| v.as_str()) {
                            puppet_path = Some(p.to_string());
                        }
                        if model.get("solidlayer").and_then(value_bool) == Some(true)
                            || model.get("instanced").and_then(value_bool) == Some(true)
                        {
                            solid_model = true;
                        }
                        if let Some(w) = model.get("width").and_then(value_f32) {
                            let h = model.get("height").and_then(value_f32).unwrap_or(w);
                            model_size = Some([w, h]);
                        }
                        if let Some(material_ref) = model.get("material").and_then(|v| v.as_str()) {
                            if let Ok(text) = pkg.read_to_string(material_ref) {
                                if let Ok(material) =
                                    serde_json::from_str::<serde_json::Value>(&text)
                                {
                                    if let Some(pass) = material
                                        .get("passes")
                                        .and_then(|p| p.as_array())
                                        .and_then(|a| a.first())
                                    {
                                        material_blend = pass
                                            .get("blending")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("normal")
                                            .to_string();
                                        texture_path = pass
                                            .get("textures")
                                            .and_then(|t| t.as_array())
                                            .and_then(|a| a.first())
                                            .and_then(|v| v.as_str())
                                            .map(str::to_string);
                                    }
                                }
                            }
                        }
                    }
                }
            } else {
                texture_path = Some(image_ref.to_string());
            }

            if solid_model {
                // Instanced solid layers with `copybackground` render the
                // already-composited scene (and often reference Wallpaper
                // Engine render-target dependencies).  Drawing one as a flat
                // solid would paint an opaque white rectangle over everything,
                // so leave it out until render-target composition is
                // supported.
                if obj.get("copybackground").and_then(value_bool) == Some(true) {
                    continue;
                }
                let size = obj
                    .get("size")
                    .map(|v| value_vec(v, 3, [self.ortho_size[0], self.ortho_size[1], 0.0]))
                    .unwrap_or([self.ortho_size[0], self.ortho_size[1], 0.0]);
                self.objects.push(SceneObject {
                    name,
                    transform,
                    size: [size[0], size[1]],
                    color: [
                        parse_color(obj.get("color").unwrap_or(&serde_json::Value::Null), [1.0, 1.0, 1.0])[0],
                        parse_color(obj.get("color").unwrap_or(&serde_json::Value::Null), [1.0, 1.0, 1.0])[1],
                        parse_color(obj.get("color").unwrap_or(&serde_json::Value::Null), [1.0, 1.0, 1.0])[2],
                        obj.get("alpha").and_then(value_f32).unwrap_or(1.0),
                    ],
                    brightness: obj.get("brightness").and_then(value_f32).unwrap_or(1.0),
                    blend: LayerBlend::from_color_mode(
                        obj.get("colorBlendMode").and_then(|v| v.as_i64()).unwrap_or(0),
                    ),
                    parallax: obj
                        .get("parallaxDepth")
                        .map(|v| {
                            let p = value_vec(v, 3, [0.0, 0.0, 0.0]);
                            [p[0], p[1]]
                        })
                        .unwrap_or([0.0, 0.0]),
                    visible,
                    effects: load_effects(gl, pkg, obj, &self.properties, now),
                    kind: ObjectKind::Solid,
                });
                continue;
            }

            let Some(texture_path) = texture_path else {
                log::debug!("scene object {name}: no usable texture in {image_ref}");
                continue;
            };
            let Some((texture, tex_w, tex_h)) =
                Self::load_texture(gl, pkg, &mut self.textures, &texture_path)
            else {
                log::debug!("scene object {name}: missing texture {texture_path}");
                continue;
            };

            let mut default_size = model_size.unwrap_or([tex_w as f32, tex_h as f32]);
            let kind = if let Some(puppet_path) = puppet_path {
                match pkg.read(&puppet_path).ok().and_then(|b| parse_mdl(&b)) {
                    Some(mut parsed) => {
                        let sym_bounds = [
                            (parsed.bounds_min[0].abs().max(parsed.bounds_max[0].abs()) * 2.0).max(1.0),
                            (parsed.bounds_min[1].abs().max(parsed.bounds_max[1].abs()) * 2.0).max(1.0),
                        ];
                        if model_size.is_none() {
                            default_size = [
                                default_size[0].max(sym_bounds[0]),
                                default_size[1].max(sym_bounds[1]),
                            ];
                        }
                        let anim_layers: Vec<PuppetAnimLayer> = obj
                            .get("animationlayers")
                            .and_then(|v| v.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|layer| {
                                        let vis = parse_visible(layer.get("visible"), now)
                                            .eval(&self.properties);
                                        if !vis {
                                            return None;
                                        }
                                        let anim_id = layer
                                            .get("animation")
                                            .and_then(|v| v.as_i64())
                                            .map(|v| v as u32);
                                        let rate = layer.get("rate").and_then(value_f32).unwrap_or(1.0);
                                        let blend = layer.get("blend").and_then(value_f32).unwrap_or(1.0);
                                        let additive = layer
                                            .get("additive")
                                            .and_then(value_bool)
                                            .unwrap_or(false);
                                        Some(PuppetAnimLayer {
                                            anim_id,
                                            rate,
                                            blend,
                                            additive,
                                        })
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        if parsed.has_animations() {
                            parsed.vertices = parsed.evaluate_vertices(0.0, &anim_layers);
                        }
                        match Mesh::new(gl, &parsed) {
                            Ok(mesh) => ObjectKind::Puppet {
                                mesh,
                                mdl: parsed,
                                anim_layers,
                                texture,
                            },
                            Err(err) => {
                                log::warn!("puppet {puppet_path}: {err}");
                                continue;
                            }
                        }
                    }
                    None => {
                        log::warn!("puppet {puppet_path}: unreadable model");
                        continue;
                    }
                }
            } else {
                ObjectKind::Image(texture)
            };

            let size = obj
                .get("size")
                .map(|v| value_vec(v, 3, [default_size[0], default_size[1], 0.0]))
                .map(|v| [v[0].abs().max(1.0), v[1].abs().max(1.0)])
                .unwrap_or(default_size);

            self.objects.push(SceneObject {
                name,
                transform,
                size,
                color: [
                    parse_color(obj.get("color").unwrap_or(&serde_json::Value::Null), [1.0, 1.0, 1.0])[0],
                    parse_color(obj.get("color").unwrap_or(&serde_json::Value::Null), [1.0, 1.0, 1.0])[1],
                    parse_color(obj.get("color").unwrap_or(&serde_json::Value::Null), [1.0, 1.0, 1.0])[2],
                    obj.get("alpha").and_then(value_f32).unwrap_or(1.0),
                ],
                brightness: obj.get("brightness").and_then(value_f32).unwrap_or(1.0),
                blend: {
                    let mode = LayerBlend::from_color_mode(
                        obj.get("colorBlendMode").and_then(|v| v.as_i64()).unwrap_or(0),
                    );
                    if mode == LayerBlend::Normal {
                        LayerBlend::from_material(&material_blend)
                    } else {
                        mode
                    }
                },
                parallax: obj
                    .get("parallaxDepth")
                    .map(|v| {
                        let p = value_vec(v, 3, [0.0, 0.0, 0.0]);
                        [p[0], p[1]]
                    })
                    .unwrap_or([0.0, 0.0]),
                visible,
                effects: load_effects(gl, pkg, obj, &self.properties, now),
                kind,
            });
            let _ = model_json;
        }

        log::debug!(
            "scene: ortho={:?} objects={} emitters={}",
            self.ortho_size,
            self.objects.len(),
            self.emitters.len()
        );
        if std::env::var_os("ANIMA_TRACE_SCENE").is_some() {
            for (i, o) in self.objects.iter().enumerate() {
                let kind = match &o.kind {
                    ObjectKind::Image(_) => "image",
                    ObjectKind::Puppet { .. } => "puppet",
                    ObjectKind::Text(_) => "text",
                    ObjectKind::Solid => "solid",
                    ObjectKind::Particle(_) => "particle",
                    ObjectKind::Post => "post",
                };
                log::info!(
                    "obj {i}: '{}' kind={} size={:?} blend={:?} alpha={} pos=({:.1},{:.1})",
                    o.name,
                    kind,
                    o.size,
                    o.blend,
                    o.color[3],
                    o.transform.tx,
                    o.transform.ty
                );
            }
        }
        Ok(())
    }

    fn load_text(
        pkg: &PackageFs,
        obj: &serde_json::Value,
        text_def: &serde_json::Value,
        now: text::LocalTime,
    ) -> Option<TextLayer> {
        let (script, authored) = match text_def {
            serde_json::Value::String(s) => (None, s.clone()),
            serde_json::Value::Object(o) => (
                o.get("script").and_then(|v| v.as_str()).map(str::to_string),
                o.get("value")
                    .map(value_to_string)
                    .unwrap_or_else(|| "Text".to_string()),
            ),
            _ => (None, String::new()),
        };
        let props = text_def
            .get("scriptproperties")
            .and_then(|v| v.as_object())
            .map(|o| o.clone().into_iter().collect::<HashMap<_, _>>())
            .unwrap_or_default();

        let mut point_size = obj
            .get("pointsize")
            .and_then(value_f32)
            .unwrap_or(32.0);
        // Some scripts express the size in points.
        if let Some(scale) = obj.get("scale") {
            let s = value_vec(scale, 3, [1.0, 1.0, 1.0]);
            let _ = s;
        }
        if point_size <= 0.0 {
            point_size = 32.0;
        }
        let box_size = obj
            .get("size")
            .map(|v| {
                let p = value_vec(v, 3, [512.0, 128.0, 0.0]);
                [p[0].abs().max(1.0), p[1].abs().max(1.0)]
            })
            .unwrap_or([512.0, 128.0]);
        let padding = obj
            .get("padding")
            .map(|v| {
                if v.is_number() {
                    let n = value_f32(v).unwrap_or(0.0).abs();
                    [n, n]
                } else {
                    let p = value_vec(v, 2, [0.0, 0.0, 0.0]);
                    [p[0].abs(), p[1].abs()]
                }
            })
            .unwrap_or([0.0, 0.0]);
        let max_width = obj.get("maxwidth").and_then(value_f32).unwrap_or(0.0);
        let h_align = match obj
            .get("horizontalalign")
            .and_then(|v| v.as_str())
            .unwrap_or("center")
        {
            "left" => text::HAlign::Left,
            "right" => text::HAlign::Right,
            _ => text::HAlign::Center,
        };
        let v_align = match obj
            .get("verticalalign")
            .and_then(|v| v.as_str())
            .unwrap_or("center")
        {
            "top" => text::VAlign::Top,
            "bottom" => text::VAlign::Bottom,
            _ => text::VAlign::Center,
        };
        let font_path = obj
            .get("font")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let font = text::load_font(pkg, &font_path);
        let layer = TextLayer {
            script,
            authored,
            props,
            font_path,
            font,
            point_size,
            spacing: [0.0, 0.0],
            padding,
            max_width,
            limit_width: obj.get("limitwidth").and_then(value_bool).unwrap_or(false),
            h_align,
            v_align,
            box_size,
            world_size: box_size,
            current: String::new(),
            texture: None,
            raster_size: [0, 0],
            raster_ppw: 0.0,
        };
        let _ = now;
        Some(layer)
    }

    fn load_native_toml(
        &mut self,
        gl: &glow::Context,
        pkg: &PackageFs,
        text_src: &str,
    ) -> Result<()> {
        let doc: NativeSceneDoc = toml::from_str(text_src).context("parsing native scene TOML")?;
        if let Some(title) = doc.title {
            self.title = title;
        }
        if let Some(res) = doc.resolution {
            self.ortho_size = [res[0].max(1.0), res[1].max(1.0)];
        }
        if let Some(cc) = doc.clear_color {
            self.clear_color = cc;
        }
        if let Some(p) = doc.parallax {
            self.parallax_enabled = p > 0.0;
            self.parallax_amount = p;
        }
        let now = text::local_time();
        for l in doc.layers {
            let Some(path) = &l.image else { continue };
            let Some((texture, _w, _h)) = Self::load_texture(gl, pkg, &mut self.textures, path)
            else {
                continue;
            };
            let size = l.size.unwrap_or(self.ortho_size);
            let origin = l.origin.unwrap_or([
                self.ortho_size[0] * 0.5,
                self.ortho_size[1] * 0.5,
                0.0,
            ]);
            let scale = l.scale.unwrap_or([1.0, 1.0]);
            let transform = Affine::translate(origin[0], origin[1])
                .mul(Affine::rotate(l.rotation.unwrap_or(0.0)))
                .mul(Affine::scale(scale[0], scale[1]));
            let c = l.color.unwrap_or([1.0, 1.0, 1.0]);
            self.objects.push(SceneObject {
                name: l.name.unwrap_or_else(|| "layer".into()),
                transform,
                size,
                color: [c[0], c[1], c[2], l.alpha.unwrap_or(1.0)],
                brightness: 1.0,
                blend: LayerBlend::from_material(l.blend.as_deref().unwrap_or("normal")),
                parallax: l.parallax_depth.unwrap_or([0.0, 0.0]),
                visible: VisibleRule::Script(parse_visible_script(&serde_json::Value::Null, now)),
                effects: Vec::new(),
                kind: ObjectKind::Image(texture),
            });
        }
        for p in doc.particles {
            let kind = ParticleKind::from_hint(&p.preset);
            let count = p.count.unwrap_or(120).clamp(1, 2000);
            let idx = self.emitters.len();
            self.emitters.push(Emitter::new(
                kind,
                count,
                [self.ortho_size[0] * 0.5, self.ortho_size[1] * 0.5],
                [self.ortho_size[0] * 0.5, self.ortho_size[1] * 0.5],
                p.color,
                p.audio_reactive.unwrap_or(true),
            ));
            self.objects.push(SceneObject {
                name: "particles".into(),
                transform: Affine::identity(),
                size: [1.0, 1.0],
                color: [1.0, 1.0, 1.0, 0.0],
                brightness: 1.0,
                blend: LayerBlend::Normal,
                parallax: p.parallax_depth.unwrap_or([0.0, 0.0]),
                visible: VisibleRule::Always,
                effects: Vec::new(),
                kind: ObjectKind::Particle(idx),
            });
        }
        Ok(())
    }

    pub fn apply_property(&mut self, key: &str, value: &str) {
        let parsed = if value.eq_ignore_ascii_case("true") {
            serde_json::Value::Bool(true)
        } else if value.eq_ignore_ascii_case("false") {
            serde_json::Value::Bool(false)
        } else if let Ok(n) = value.parse::<f64>() {
            serde_json::Value::Number(serde_json::Number::from_f64(n).unwrap())
        } else {
            serde_json::Value::String(value.to_string())
        };
        self.properties.insert(key.to_string(), parsed);
        match key.to_ascii_lowercase().as_str() {
            "clearcolor" => {
                self.clear_color = parse_color(&serde_json::Value::String(value.to_string()), self.clear_color);
            }
            "speed" => {
                if let Ok(v) = value.parse::<f32>() {
                    self.speed_multiplier = v.clamp(0.0, 5.0);
                }
            }
            "parallax" | "cameraparallax" => {
                if value == "true" || value == "false" {
                    self.parallax_enabled = value == "true";
                } else if let Ok(v) = value.parse::<f32>() {
                    self.parallax_enabled = v > 0.0;
                    self.parallax_amount = v;
                }
            }
            _ => {}
        }
    }

    fn ensure_scene(&mut self, gl: &glow::Context) -> Result<()> {
        let w = self.width.max(1);
        let h = self.height.max(1);
        let recreate = match &self.scene {
            Some(c) => c.w != w || c.h != h,
            None => true,
        };
        if recreate {
            if let Some(old) = self.scene.take() {
                old.destroy(gl);
            }
            if let Some(old) = self.scene_alt.take() {
                old.destroy(gl);
            }
            self.scene = Some(Canvas::new(gl, w, h)?);
            self.scene_alt = Some(Canvas::new(gl, w, h)?);
        }
        Ok(())
    }

    fn parallax_offset(&self, depth: [f32; 2], time: f32) -> [f32; 2] {
        let mut offset = [0.0f32; 2];
        if self.parallax_enabled {
            // WE parallax depths are small (0.01..2).  Map one unit of depth
            // to roughly 2% of the screen so mouse movement remains subtle.
            let s = self.parallax_amount * self.parallax_mouse_influence * 0.02;
            offset[0] = self.smooth_mouse[0] * depth[0] * self.ortho_size[0] * s;
            offset[1] = self.smooth_mouse[1] * depth[1] * self.ortho_size[1] * s;
        }
        if self.camera_shake {
            let t = time * self.camera_shake_speed;
            offset[0] += (t * 1.7).sin() * self.camera_shake_amp * self.ortho_size[0] * 0.01;
            offset[1] += (t * 2.3).cos() * self.camera_shake_amp * self.ortho_size[1] * 0.01;
        }
        offset
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_content(
        &mut self,
        gl: &glow::Context,
        info: &DrawInfo,
        transform: Affine,
        canvas_min: [f32; 2],
        canvas_size: [f32; 2],
    ) {
        unsafe {
            gl.disable(glow::DEPTH_TEST);
            match info.kind {
                DrawKind::Image(texture) | DrawKind::Text(Some(texture)) => {
                    let axis_x = transform.apply_vec([info.size[0] * 0.5, 0.0]);
                    let axis_y = transform.apply_vec([0.0, info.size[1] * 0.5]);
                    let p = &mut self.content_prog;
                    p.bind(gl);
                    p.set_vec2(gl, "u_min", canvas_min[0], canvas_min[1]);
                    p.set_vec2(gl, "u_canvas", canvas_size[0], canvas_size[1]);
                    p.set_vec2(gl, "u_origin", transform.tx, transform.ty);
                    p.set_vec2(gl, "u_axis_x", axis_x[0], axis_x[1]);
                    p.set_vec2(gl, "u_axis_y", axis_y[0], axis_y[1]);
                    p.set_vec4(gl, "u_color", info.color);
                    p.set_f32(gl, "u_brightness", info.brightness);
                    p.set_i32(gl, "u_tex", 0);
                    gl.active_texture(glow::TEXTURE0);
                    gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                    gl.bind_vertex_array(Some(self.quad_vao));
                    gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                }
                DrawKind::Text(None) | DrawKind::None => {}
                DrawKind::Puppet {
                    vao,
                    index_count,
                    texture,
                } => {
                    let p = &mut self.puppet_prog;
                    p.bind(gl);
                    p.set_vec2(gl, "u_min", canvas_min[0], canvas_min[1]);
                    p.set_vec2(gl, "u_canvas", canvas_size[0], canvas_size[1]);
                    p.set_vec2(gl, "u_origin", transform.tx, transform.ty);
                    p.set_vec2(gl, "u_axis_x", transform.a, transform.b);
                    p.set_vec2(gl, "u_axis_y", transform.c, transform.d);
                    p.set_vec4(gl, "u_color", info.color);
                    p.set_f32(gl, "u_brightness", info.brightness);
                    p.set_i32(gl, "u_tex", 0);
                    gl.active_texture(glow::TEXTURE0);
                    gl.bind_texture(glow::TEXTURE_2D, Some(texture));
                    gl.bind_vertex_array(Some(vao));
                    gl.draw_elements(glow::TRIANGLES, index_count, glow::UNSIGNED_SHORT, 0);
                    gl.bind_vertex_array(None);
                }
                DrawKind::Solid => {
                    let Some((white, _, _)) = self.textures.get(gl, &self.pkg, "util/white")
                    else {
                        return;
                    };
                    let axis_x = transform.apply_vec([info.size[0] * 0.5, 0.0]);
                    let axis_y = transform.apply_vec([0.0, info.size[1] * 0.5]);
                    let p = &mut self.content_prog;
                    p.bind(gl);
                    p.set_vec2(gl, "u_min", canvas_min[0], canvas_min[1]);
                    p.set_vec2(gl, "u_canvas", canvas_size[0], canvas_size[1]);
                    p.set_vec2(gl, "u_origin", transform.tx, transform.ty);
                    p.set_vec2(gl, "u_axis_x", axis_x[0], axis_x[1]);
                    p.set_vec2(gl, "u_axis_y", axis_y[0], axis_y[1]);
                    p.set_vec4(gl, "u_color", info.color);
                    p.set_f32(gl, "u_brightness", info.brightness);
                    p.set_i32(gl, "u_tex", 0);
                    gl.active_texture(glow::TEXTURE0);
                    gl.bind_texture(glow::TEXTURE_2D, Some(white));
                    gl.bind_vertex_array(Some(self.quad_vao));
                    gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                }
            }
        }
    }

    fn draw_layer_canvas(
        &mut self,
        gl: &glow::Context,
        canvas: &Canvas,
        min: [f32; 2],
        max: [f32; 2],
        blend: LayerBlend,
    ) {
        let origin = [(min[0] + max[0]) * 0.5, (min[1] + max[1]) * 0.5];
        let axis_x = [(max[0] - min[0]) * 0.5, 0.0];
        let axis_y = [0.0, (max[1] - min[1]) * 0.5];
        unsafe {
            apply_blend(gl, blend);
            let p = &mut self.composite_prog;
            p.bind(gl);
            p.set_vec2(gl, "u_origin", origin[0], origin[1]);
            p.set_vec2(gl, "u_axis_x", axis_x[0], axis_x[1]);
            p.set_vec2(gl, "u_axis_y", axis_y[0], axis_y[1]);
            p.set_vec2(gl, "u_ortho", self.ortho_size[0], self.ortho_size[1]);
            p.set_i32(gl, "u_tex", 0);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(canvas.tex));
            gl.bind_vertex_array(Some(self.quad_vao));
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        }
    }
}

impl TextLayer {
    fn evaluate(
        &self,
        props: &HashMap<String, serde_json::Value>,
        now: text::LocalTime,
    ) -> String {
        let mut merged: HashMap<String, serde_json::Value> = props.clone();
        for (k, v) in &self.props {
            let value = if let Some(user) = v.get("user").and_then(|u| u.as_str()) {
                props.get(user).cloned().unwrap_or_else(|| {
                    v.get("value").cloned().unwrap_or(serde_json::Value::Null)
                })
            } else {
                v.clone()
            };
            merged.insert(k.clone(), value);
        }
        if let Some(script) = &self.script {
            if let Some(value) = text::eval_script(script, &merged, now) {
                return value;
            }
        }
        self.authored.clone()
    }

    /// Re-rasterise the layer when its text or the output density changes.
    fn update(&mut self, gl: &glow::Context, value: String, px_per_world: f32) {
        let ppw = px_per_world.max(0.01);
        if value == self.current
            && (self.raster_ppw - ppw).abs() < 1e-3
            && self.texture.is_some()
        {
            return;
        }
        let Some(font) = self.font.as_ref() else {
            return;
        };
        let font_px =
            (self.point_size.max(1.0) * text::UNITS_PER_POINT * ppw).clamp(1.0, 8192.0);
        let pad_px = self.padding[0] * ppw;
        let spacing_px = [self.spacing[0] * ppw, self.spacing[1] * ppw];
        let (max_width_px, fixed) = if self.limit_width {
            let w = if self.max_width > 0.0 {
                self.max_width
            } else {
                self.box_size[0]
            };
            (
                Some((w * ppw).max(1.0)),
                Some([
                    (self.box_size[0] * ppw).round().clamp(1.0, 8192.0) as u32,
                    (self.box_size[1] * ppw).round().clamp(1.0, 8192.0) as u32,
                ]),
            )
        } else {
            (None, None)
        };
        let out = text::rasterize(
            font,
            &value,
            font_px,
            pad_px,
            spacing_px,
            self.h_align,
            self.v_align,
            max_width_px,
            0,
            fixed,
        );
        if let Some(old) = self.texture.take() {
            unsafe { gl.delete_texture(old) };
        }
        self.texture = create_texture(
            gl,
            out.width,
            out.height,
            &out.pixels,
            Filter::Linear,
            false,
        )
        .ok();
        self.raster_size = [out.width as i32, out.height as i32];
        self.raster_ppw = ppw;
        self.world_size = if self.limit_width {
            self.box_size
        } else {
            [out.width as f32 / ppw, out.height as f32 / ppw]
        };
        self.current = value;
    }
}


// ---------------------------------------------------------------------------
// Loading helpers
// ---------------------------------------------------------------------------

fn parse_visible(v: Option<&serde_json::Value>, now: text::LocalTime) -> VisibleRule {
    let Some(v) = v else {
        return VisibleRule::Always;
    };
    if let Some(script) = v.get("script").and_then(|s| s.as_str()) {
        return VisibleRule::Script(script_is_currently_true(script, now));
    }
    if let Some(user) = v.get("user") {
        let (name, condition) = match user {
            serde_json::Value::String(s) => (s.clone(), None),
            serde_json::Value::Object(o) => (
                o.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string(),
                o.get("condition").map(value_to_string),
            ),
            _ => (String::new(), None),
        };
        let value = v.get("value").and_then(value_bool).unwrap_or(true);
        return VisibleRule::User {
            name,
            condition,
            value,
        };
    }
    match v.as_bool() {
        Some(true) => VisibleRule::Always,
        Some(false) => VisibleRule::Script(false),
        None => VisibleRule::Script(v.get("value").and_then(value_bool).unwrap_or(true)),
    }
}

fn parse_visible_script(v: &serde_json::Value, now: text::LocalTime) -> bool {
    if let Some(script) = v.get("script").and_then(|s| s.as_str()) {
        script_is_currently_true(script, now)
    } else {
        v.get("value").and_then(value_bool).unwrap_or(true)
    }
}

/// Minimal evaluator for the date-range visibility scripts shipped with WE
/// holiday effects (`start of January` etc.).
fn script_is_currently_true(script: &str, now: text::LocalTime) -> bool {
    if let Some(start) = script.find("new Date(now.getFullYear(), 0, 1)") {
        let _ = start;
        if script.contains("0, 2") {
            // January 1st only.
            return now.month == 1 && now.day == 1;
        }
    }
    if script.contains("getMonth()") && script.contains("getDate()") {
        // Unknown date script: hide rather than showing holiday effects all year.
        return false;
    }
    false
}

fn load_effects(
    gl: &glow::Context,
    pkg: &PackageFs,
    obj: &serde_json::Value,
    props: &HashMap<String, serde_json::Value>,
    now: text::LocalTime,
) -> Vec<EffectInstance> {
    if std::env::var_os("ANIMA_NO_EFFECTS").is_some() {
        return Vec::new();
    }
    let Some(effects) = obj.get("effects").and_then(|e| e.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for effect in effects {
        if !parse_visible(effect.get("visible"), now).eval(props) {
            continue;
        }
        let Some(file) = effect.get("file").and_then(|v| v.as_str()) else {
            continue;
        };
        let mut resolved_effect = effect.clone();
        if let Some(passes) = resolved_effect
            .get_mut("passes")
            .and_then(|p| p.as_array_mut())
        {
            for pass in passes {
                if let Some(csv) = pass
                    .get_mut("constantshadervalues")
                    .and_then(|c| c.as_object_mut())
                {
                    for val in csv.values_mut() {
                        if let Some(user_prop) = val
                            .as_object()
                            .and_then(|o| o.get("user"))
                            .and_then(|u| u.as_str())
                            .map(str::to_string)
                        {
                            if let Some(user_val) = props.get(&user_prop) {
                                *val = user_val.clone();
                            }
                        }
                    }
                }
            }
        }
        if let Some(instance) = EffectInstance::load(gl, pkg, file, &resolved_effect) {
            out.push(instance);
        }
    }
    out
}

impl Renderer for SceneRenderer {
    fn render(&mut self, gl: &glow::Context, info: &FrameInfo) -> Result<bool> {
        self.ensure_scene(gl)?;
        let time = (info.time as f32) * self.speed_multiplier;
        let dt = (info.delta as f32) * self.speed_multiplier;

        let target_mx = if info.width > 0 {
            (info.mouse[0] / info.width as f32) * 2.0 - 1.0
        } else {
            0.0
        };
        let target_my = if info.height > 0 {
            1.0 - (info.mouse[1] / info.height as f32) * 2.0
        } else {
            0.0
        };
        let lerp = (dt * 8.0).clamp(0.02, 1.0);
        self.smooth_mouse[0] += (target_mx - self.smooth_mouse[0]) * lerp;
        self.smooth_mouse[1] += (target_my - self.smooth_mouse[1]) * lerp;
        let mouse_uv = [
            (self.smooth_mouse[0] + 1.0) * 0.5,
            (self.smooth_mouse[1] + 1.0) * 0.5,
        ];

        let ortho_w = self.ortho_size[0].max(1.0);
        let ortho_h = self.ortho_size[1].max(1.0);
        let ortho = [ortho_w, ortho_h];
        let px_per_world = (self.width.max(1) as f32 / ortho_w)
            .max(self.height.max(1) as f32 / ortho_h);

        // Refresh text and skeletal puppet meshes before computing layer bounds.
        {
            let now = text::local_time();
            let props = &self.properties;
            for obj in self.objects.iter_mut() {
                if !obj.visible.eval(props) {
                    continue;
                }
                match &mut obj.kind {
                    ObjectKind::Text(layer) => {
                        let value = layer.evaluate(props, now);
                        layer.update(gl, value, px_per_world);
                        obj.size = layer.world_size;
                    }
                    ObjectKind::Puppet {
                        mesh,
                        mdl,
                        anim_layers,
                        ..
                    } => {
                        if mdl.has_animations() {
                            let verts = mdl.evaluate_vertices(time, anim_layers);
                            mesh.update_vertices(gl, &verts);
                        }
                    }
                    _ => {}
                }
            }
        }

        // Clear the scene framebuffer.
        {
            let scene = self.scene.as_ref().unwrap();
            let (w, h) = (scene.w, scene.h);
            unsafe {
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(scene.fbo));
                gl.viewport(0, 0, w, h);
                gl.disable(glow::DEPTH_TEST);
                gl.disable(glow::SCISSOR_TEST);
                gl.clear_color(
                    self.clear_color[0],
                    self.clear_color[1],
                    self.clear_color[2],
                    1.0,
                );
                gl.clear(glow::COLOR_BUFFER_BIT);
            }
        }

        for i in 0..self.objects.len() {
            if !self.objects[i].is_visible(&self.properties) {
                continue;
            }
            let depth = self.objects[i].parallax;
            let offset = self.parallax_offset(depth, time);
            let blend = self.objects[i].blend;

            if let ObjectKind::Particle(idx) = self.objects[i].kind {
                let scene = self.scene.as_ref().unwrap();
                unsafe {
                    gl.bind_framebuffer(glow::FRAMEBUFFER, Some(scene.fbo));
                    gl.viewport(0, 0, scene.w, scene.h);
                }
                let emitter = &mut self.emitters[idx];
                apply_blend(gl, emitter.blend);
                emitter.update_and_pack(
                    dt,
                    time,
                    ortho,
                    info.audio,
                    &mut self.instance_scratch,
                );
                if self.instance_scratch.is_empty() {
                    continue;
                }
                unsafe {
                    let p = &mut self.particle_prog;
                    p.bind(gl);
                    p.set_vec2(gl, "u_ortho", ortho_w, ortho_h);
                    p.set_i32(gl, "u_shape", emitter.kind.shape_id());
                    gl.bind_vertex_array(Some(self.particle_vao));
                    gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.particle_inst_vbo));
                    gl.buffer_data_u8_slice(
                        glow::ARRAY_BUFFER,
                        bytemuck_cast(&self.instance_scratch),
                        glow::STREAM_DRAW,
                    );
                    let count = (self.instance_scratch.len() / 9) as i32;
                    gl.draw_arrays_instanced(glow::TRIANGLE_STRIP, 0, 4, count);
                }
                continue;
            }

            if matches!(self.objects[i].kind, ObjectKind::Post) {
                let mut effects = std::mem::take(&mut self.objects[i].effects);
                if effects.is_empty() {
                    continue;
                }
                let ctx = EffectContext {
                    time,
                    mouse: mouse_uv,
                    audio: info.audio,
                    spectrum: &info.spectrum,
                    projection_inverse: identity4(),
                };
                let mut flip = false;
                {
                    let scene = self.scene.as_ref().unwrap();
                    let alt = self.scene_alt.as_ref().unwrap();
                    for effect in effects.iter_mut() {
                        let (src, dst) = if flip { (alt, scene) } else { (scene, alt) };
                        if effect
                            .apply(gl, &self.pkg, &mut self.textures, src, dst, &ctx)
                            .is_ok()
                        {
                            flip = !flip;
                        }
                    }
                }
                if flip {
                    std::mem::swap(&mut self.scene, &mut self.scene_alt);
                }
                self.objects[i].effects = effects;
                continue;
            }

            let mut transform = self.objects[i].transform;
            transform.tx += offset[0];
            transform.ty += offset[1];
            let (min, max) = self.objects[i].aabb(offset);
            let aabb_size = [(max[0] - min[0]).max(1.0), (max[1] - min[1]).max(1.0)];
            let has_effects = !self.objects[i].effects.is_empty();
            let draw = self.objects[i].draw_info();

            if !has_effects {
                let scene = self.scene.as_ref().unwrap();
                unsafe {
                    gl.bind_framebuffer(glow::FRAMEBUFFER, Some(scene.fbo));
                    gl.viewport(0, 0, scene.w, scene.h);
                }
                apply_blend(gl, blend);
                self.draw_content(gl, &draw, transform, [0.0, 0.0], ortho);
                continue;
            }

            let cw = (aabb_size[0] * px_per_world).ceil().clamp(1.0, 4096.0) as i32;
            let ch = (aabb_size[1] * px_per_world).ceil().clamp(1.0, 4096.0) as i32;
            let mut canvas = self.pool.take(gl, cw, ch)?;
            let tmp = self.pool.take(gl, cw, ch)?;

            unsafe {
                canvas.bind(gl);
                gl.disable(glow::BLEND);
                gl.clear_color(0.0, 0.0, 0.0, 0.0);
                gl.clear(glow::COLOR_BUFFER_BIT);
            }
            self.draw_content(gl, &draw, transform, min, aabb_size);

            let ctx = EffectContext {
                time,
                mouse: mouse_uv,
                audio: info.audio,
                spectrum: &info.spectrum,
                projection_inverse: identity4(),
            };
            let mut effects = std::mem::take(&mut self.objects[i].effects);
            let mut flip = false;
            for effect in effects.iter_mut() {
                let (src, dst) = if flip { (&tmp, &canvas) } else { (&canvas, &tmp) };
                if effect
                    .apply(gl, &self.pkg, &mut self.textures, src, dst, &ctx)
                    .is_ok()
                {
                    flip = !flip;
                }
            }
            self.objects[i].effects = effects;

            {
                let scene = self.scene.as_ref().unwrap();
                unsafe {
                    gl.bind_framebuffer(glow::FRAMEBUFFER, Some(scene.fbo));
                    gl.viewport(0, 0, scene.w, scene.h);
                }
            }
            let final_canvas: &Canvas = if flip { &tmp } else { &canvas };
            self.draw_layer_canvas(gl, final_canvas, min, max, blend);
            self.pool.put(gl, canvas);
            self.pool.put(gl, tmp);
        }

        // Blit the composed scene to the surface.
        {
            let scene = self.scene.as_ref().unwrap();
            unsafe {
                gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                gl.viewport(0, 0, self.width.max(1), self.height.max(1));
                gl.disable(glow::BLEND);
                gl.disable(glow::DEPTH_TEST);
                let p = &mut self.copy_prog;
                p.bind(gl);
                p.set_i32(gl, "u_tex", 0);
                gl.active_texture(glow::TEXTURE0);
                gl.bind_texture(glow::TEXTURE_2D, Some(scene.tex));
                gl.bind_vertex_array(Some(self.quad_vao));
                gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            }
        }
        gl_disable_blend(gl);

        Ok(true)
    }

    fn resize(&mut self, _gl: &glow::Context, width: i32, height: i32) -> Result<()> {
        self.width = width.max(1);
        self.height = height.max(1);
        Ok(())
    }

    fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
    }

    fn set_volume(&mut self, _volume: f32) {}

    fn set_property(&mut self, key: &str, value: &str) -> Result<()> {
        self.apply_property(key, value);
        Ok(())
    }

    fn uses_audio(&self) -> bool {
        self.emitters.iter().any(|e| e.audio_reactive)
    }

    fn describe(&self) -> String {
        format!(
            "scene '{}' ({} objects, {} emitters)",
            self.title,
            self.objects.len(),
            self.emitters.len()
        )
    }

    fn destroy(&mut self, gl: &glow::Context) {
        if let Some(canvas) = self.scene.take() {
            canvas.destroy(gl);
        }
        if let Some(canvas) = self.scene_alt.take() {
            canvas.destroy(gl);
        }
        self.pool.clear(gl);
        self.textures.clear(gl);
        for obj in &mut self.objects {
            match &mut obj.kind {
                ObjectKind::Text(layer) => {
                    if let Some(tex) = layer.texture.take() {
                        unsafe { gl.delete_texture(tex) };
                    }
                }
                ObjectKind::Puppet { mesh, .. } => mesh.destroy(gl),
                _ => {}
            }
        }
        self.content_prog.destroy(gl);
        self.puppet_prog.destroy(gl);
        self.composite_prog.destroy(gl);
        self.copy_prog.destroy(gl);
        self.particle_prog.destroy(gl);
        unsafe {
            gl.delete_vertex_array(self.quad_vao);
            gl.delete_buffer(self.quad_vbo);
            gl.delete_vertex_array(self.particle_vao);
            gl.delete_buffer(self.particle_quad_vbo);
            gl.delete_buffer(self.particle_inst_vbo);
        }
    }
}

// ---------------------------------------------------------------------------
// GL helpers
// ---------------------------------------------------------------------------

fn apply_blend(gl: &glow::Context, mode: LayerBlend) {
    unsafe {
        gl.enable(glow::BLEND);
        match mode {
            LayerBlend::Normal => gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA),
            LayerBlend::Additive => gl.blend_func(glow::SRC_ALPHA, glow::ONE),
            LayerBlend::Multiply => gl.blend_func(glow::DST_COLOR, glow::ONE_MINUS_SRC_ALPHA),
            LayerBlend::Screen => gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_COLOR),
        }
    }
}

fn gl_disable_blend(gl: &glow::Context) {
    unsafe { gl.disable(glow::BLEND) }
}

fn create_quad(gl: &glow::Context) -> Result<(glow::VertexArray, glow::Buffer)> {
    // pos.xy + uv.xy; a_pos.y = +1 is the top and samples uv.y = 0.
    const VERTS: [f32; 16] = [
        -1.0, 1.0, 0.0, 0.0,
         1.0, 1.0, 1.0, 0.0,
        -1.0, -1.0, 0.0, 1.0,
         1.0, -1.0, 1.0, 1.0,
    ];
    unsafe {
        let vao = gl.create_vertex_array().map_err(|e| anyhow::anyhow!(e))?;
        let vbo = gl.create_buffer().map_err(|e| anyhow::anyhow!(e))?;
        gl.bind_vertex_array(Some(vao));
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
        gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytemuck_cast(&VERTS), glow::STATIC_DRAW);
        gl.enable_vertex_attrib_array(0);
        gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 16, 0);
        gl.enable_vertex_attrib_array(1);
        gl.vertex_attrib_pointer_f32(1, 2, glow::FLOAT, false, 16, 8);
        gl.bind_vertex_array(None);
        Ok((vao, vbo))
    }
}

fn create_particle_buffers(
    gl: &glow::Context,
) -> Result<(glow::VertexArray, glow::Buffer, glow::Buffer)> {
    const QUAD: [f32; 8] = [-1.0, -1.0, 1.0, -1.0, -1.0, 1.0, 1.0, 1.0];
    unsafe {
        let vao = gl.create_vertex_array().map_err(|e| anyhow::anyhow!(e))?;
        let q_vbo = gl.create_buffer().map_err(|e| anyhow::anyhow!(e))?;
        let i_vbo = gl.create_buffer().map_err(|e| anyhow::anyhow!(e))?;
        gl.bind_vertex_array(Some(vao));
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(q_vbo));
        gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytemuck_cast(&QUAD), glow::STATIC_DRAW);
        gl.enable_vertex_attrib_array(0);
        gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 8, 0);
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(i_vbo));
        let stride = 9 * 4;
        gl.enable_vertex_attrib_array(1);
        gl.vertex_attrib_pointer_f32(1, 4, glow::FLOAT, false, stride, 0);
        gl.vertex_attrib_divisor(1, 1);
        gl.enable_vertex_attrib_array(2);
        gl.vertex_attrib_pointer_f32(2, 4, glow::FLOAT, false, stride, 16);
        gl.vertex_attrib_divisor(2, 1);
        gl.enable_vertex_attrib_array(3);
        gl.vertex_attrib_pointer_f32(3, 1, glow::FLOAT, false, stride, 32);
        gl.vertex_attrib_divisor(3, 1);
        gl.bind_vertex_array(None);
        Ok((vao, q_vbo, i_vbo))
    }
}

fn identity4() -> [f32; 16] {
    [
        1.0, 0.0, 0.0, 0.0,
        0.0, 1.0, 0.0, 0.0,
        0.0, 0.0, 1.0, 0.0,
        0.0, 0.0, 0.0, 1.0,
    ]
}

// Native scene TOML (kept for backwards compatibility with `scene.toml`).

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct NativeSceneDoc {
    title: Option<String>,
    resolution: Option<[f32; 2]>,
    clear_color: Option<[f32; 3]>,
    parallax: Option<f32>,
    #[serde(rename = "layer")]
    layers: Vec<NativeLayerDef>,
    #[serde(rename = "particles")]
    particles: Vec<NativeParticleDef>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct NativeLayerDef {
    name: Option<String>,
    image: Option<String>,
    origin: Option<[f32; 3]>,
    size: Option<[f32; 2]>,
    scale: Option<[f32; 2]>,
    rotation: Option<f32>,
    color: Option<[f32; 3]>,
    alpha: Option<f32>,
    parallax_depth: Option<[f32; 2]>,
    blend: Option<String>,
    wave_amp: Option<f32>,
    wave_freq: Option<f32>,
    sway_amp: Option<f32>,
    pulse_amp: Option<f32>,
    scroll: Option<[f32; 2]>,
    audio_react: Option<f32>,
    chromatic: Option<f32>,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct NativeParticleDef {
    preset: String,
    count: Option<usize>,
    color: Option<[f32; 3]>,
    parallax_depth: Option<[f32; 2]>,
    audio_reactive: Option<bool>,
}

struct SceneObjectTextProxy<'a> {
    size: [f32; 2],
    _marker: std::marker::PhantomData<&'a ()>,
}
