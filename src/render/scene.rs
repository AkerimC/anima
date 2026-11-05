//! 2D/3D Multi-Layer Scene & Particle Renderer (Wallpaper Engine `scene.json` /
//! `scene.pkg` + Native Anima Scene `.anima` / `scene.toml`).
//!
//! Features:
//! * Reads unpacked `scene.json` or directly streams from `scene.pkg` / `gifscene.pkg`
//!   and decodes `.tex` textures (RGBA8, DXT1/3/5, LZ4, embedded PNG/JPEG).
//! * Multi-layer compositing with Z-sorting, blend modes (`normal`, `additive`,
//!   `multiply`, `screen`), per-layer `parallaxDepth`, camera shake & smooth
//!   mouse parallax interpolation.
//! * Per-layer procedural GPU shader effects: water waves, foliage sway, UV
//!   scroll, pulse, chromatic aberration, and audio-reactive bounce/glow.
//! * Built-in GPU-batched particle systems (`rain`, `snow`, `embers`,
//!   `fireflies`, `dust`, `stars`, `sakura`) with wind, turbulence, mouse
//!   repulsion, and audio-beat acceleration.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use glow::HasContext;
use serde::Deserialize;

use crate::pkg::PackageFs;
use super::gl::{Filter, Program, Triangle, bytemuck_cast, create_texture};
use super::{FrameInfo, Renderer};

const LAYER_VS: &str = r#"#version 300 es
precision highp float;
layout(location = 0) in vec2 a_pos;
layout(location = 1) in vec2 a_uv;

uniform vec2 u_center;
uniform vec2 u_half_size;
uniform float u_rotation;

out vec2 v_uv;

void main() {
    float c = cos(u_rotation);
    float s = sin(u_rotation);
    vec2 local = a_pos * u_half_size;
    vec2 rotated = vec2(local.x * c - local.y * s, local.x * s + local.y * c);
    vec2 clip = u_center + rotated;
    v_uv = a_uv;
    gl_Position = vec4(clip, 0.0, 1.0);
}
"#;

const LAYER_FS: &str = r#"#version 300 es
precision highp float;
in vec2 v_uv;

uniform sampler2D u_tex;
uniform vec4 u_color;
uniform float u_time;
uniform vec4 u_audio;
// x = wave_amp, y = wave_freq, z = sway_amp, w = pulse_amp
uniform vec4 u_fx1;
// x = scroll_x, y = scroll_y, z = audio_react, w = chromatic
uniform vec4 u_fx2;

out vec4 fragColor;

void main() {
    vec2 uv = v_uv;

    // Foliage / top-anchored sway
    if (u_fx1.z > 0.0001) {
        float weight = 1.0 - uv.y;
        uv.x += sin(u_time * 2.1 + uv.y * 4.0) * u_fx1.z * weight;
    }

    // Water wave distortion
    if (u_fx1.x > 0.0001) {
        uv.x += sin(uv.y * u_fx1.y + u_time * 2.6) * u_fx1.x;
        uv.y += cos(uv.x * u_fx1.y + u_time * 2.1) * (u_fx1.x * 0.6);
    }

    // Continuous UV scroll
    uv += vec2(u_fx2.x, u_fx2.y) * u_time;
    uv = fract(uv);

    vec4 texel;
    if (u_fx2.w > 0.0001) {
        float shift = u_fx2.w * (1.0 + u_audio.x * 2.0);
        float r = texture(u_tex, clamp(uv + vec2(shift, 0.0), 0.0, 1.0)).r;
        vec4 gba = texture(u_tex, uv);
        float b = texture(u_tex, clamp(uv - vec2(shift, 0.0), 0.0, 1.0)).b;
        texel = vec4(r, gba.g, b, gba.a);
    } else {
        texel = texture(u_tex, uv);
    }

    float pulse = 1.0;
    if (u_fx1.w > 0.0001) {
        pulse += sin(u_time * 3.0) * u_fx1.w;
    }
    if (u_fx2.z > 0.0001) {
        pulse += u_audio.x * u_fx2.z;
    }

    vec4 col = texel * u_color;
    col.rgb *= pulse;
    fragColor = col;
}
"#;

const PARTICLE_VS: &str = r#"#version 300 es
precision highp float;
layout(location = 0) in vec2 a_quad;
layout(location = 1) in vec4 a_inst_pos_size; // x, y (clip), w, h (clip)
layout(location = 2) in vec4 a_inst_color;
layout(location = 3) in float a_inst_rot;

out vec2 v_uv;
out vec4 v_color;

void main() {
    float c = cos(a_inst_rot);
    float s = sin(a_inst_rot);
    vec2 scaled = a_quad * a_inst_pos_size.zw;
    vec2 rot = vec2(scaled.x * c - scaled.y * s, scaled.x * s + scaled.y * c);
    gl_Position = vec4(a_inst_pos_size.xy + rot, 0.0, 1.0);
    v_uv = a_quad * 0.5 + 0.5;
    v_color = a_inst_color;
}
"#;

const PARTICLE_FS: &str = r#"#version 300 es
precision highp float;
in vec2 v_uv;
in vec4 v_color;
uniform int u_shape; // 0 = soft circle, 1 = streak/rain, 2 = star/sparkle
out vec4 fragColor;

void main() {
    vec2 p = v_uv * 2.0 - 1.0;
    float alpha = 1.0;
    if (u_shape == 1) {
        // Rain streak
        float d = abs(p.x) * 3.5 + abs(p.y);
        alpha = smoothstep(1.0, 0.1, d);
    } else if (u_shape == 2) {
        // 4-point star glow
        float r = length(p);
        float rays = max(0.0, 1.0 - abs(p.x * p.y * 6.0));
        alpha = smoothstep(1.0, 0.0, r) * 0.7 + rays * smoothstep(1.0, 0.0, r) * 0.5;
    } else {
        // Soft bokeh / snow / ember disc
        float r = length(p);
        alpha = smoothstep(1.0, 0.05, r);
    }
    fragColor = vec4(v_color.rgb, v_color.a * alpha);
}
"#;

const POST_VS: &str = r#"#version 300 es
layout(location = 0) in vec2 a_pos;
out vec2 v_uv;
void main() {
    v_uv = a_pos * 0.5 + 0.5;
    gl_Position = vec4(a_pos, 0.0, 1.0);
}
"#;

const POST_FS: &str = r#"#version 300 es
precision highp float;
in vec2 v_uv;
uniform float u_time;
uniform vec3 u_clear_color;
uniform float u_vignette;
out vec4 fragColor;
void main() {
    float vig = 1.0 - u_vignette * smoothstep(0.35, 1.05, length(v_uv - 0.5));
    fragColor = vec4(u_clear_color * vig, 1.0);
}
"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlendMode {
    Normal,
    Additive,
    Multiply,
    Screen,
}

impl BlendMode {
    fn from_str(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "additive" | "add" | "light" => BlendMode::Additive,
            "multiply" | "mult" => BlendMode::Multiply,
            "screen" => BlendMode::Screen,
            _ => BlendMode::Normal,
        }
    }
}

struct SceneLayer {
    #[allow(dead_code)]
    name: String,
    texture: glow::Texture,
    /// Normalized position in scene coordinates `(0..ortho_w, 0..ortho_h)`.
    origin: [f32; 3],
    size: [f32; 2],
    scale: [f32; 2],
    rotation: f32,
    color: [f32; 4],
    parallax_depth: [f32; 2],
    blend: BlendMode,
    wave_amp: f32,
    wave_freq: f32,
    sway_amp: f32,
    pulse_amp: f32,
    scroll: [f32; 2],
    audio_react: f32,
    chromatic: f32,
}

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
        let lower = s.to_ascii_lowercase();
        if lower.contains("fog")
            || lower.contains("mist")
            || lower.contains("smoke")
            || lower.contains("cloud")
        {
            ParticleKind::Fog
        } else if lower.contains("rain") || lower.contains("storm") {
            ParticleKind::Rain
        } else if lower.contains("ember") || lower.contains("fire") || lower.contains("spark") {
            ParticleKind::Embers
        } else if lower.contains("fly") || lower.contains("glow") || lower.contains("orb") {
            ParticleKind::Fireflies
        } else if lower.contains("star") || lower.contains("twinkle") {
            ParticleKind::Stars
        } else if lower.contains("sakura")
            || lower.contains("petal")
            || lower.contains("blossom")
            || lower.contains("flower")
            || lower.contains("leaf")
            || lower.contains("leave")
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

struct ParticleEmitter {
    kind: ParticleKind,
    blend: BlendMode,
    parallax: [f32; 2],
    audio_reactive: bool,
    particles: Vec<Particle>,
}

impl ParticleEmitter {
    fn new(kind: ParticleKind, count: usize, color_override: Option<[f32; 3]>, audio_reactive: bool) -> Self {
        let mut particles = Vec::with_capacity(count);
        let mut seed = 0x9E37_79B9u32 ^ (count as u32);
        let mut rng = || -> f32 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed as f32) / (u32::MAX as f32)
        };

        for _ in 0..count {
            let x = rng() * 2.4 - 1.2;
            let y = rng() * 2.4 - 1.2;
            let phase = rng() * std::f32::consts::TAU;
            let (vx, vy, size, alpha, default_col, rot) = match kind {
                ParticleKind::Snow => (
                    (rng() - 0.5) * 0.06,
                    -(0.08 + rng() * 0.18),
                    0.004 + rng() * 0.009,
                    0.35 + rng() * 0.55,
                    [0.92, 0.96, 1.0],
                    0.0,
                ),
                ParticleKind::Rain => (
                    -0.12 - rng() * 0.05,
                    -(0.85 + rng() * 0.65),
                    0.006 + rng() * 0.008,
                    0.25 + rng() * 0.35,
                    [0.70, 0.82, 0.96],
                    -0.15,
                ),
                ParticleKind::Embers => (
                    (rng() - 0.5) * 0.12,
                    0.10 + rng() * 0.25,
                    0.004 + rng() * 0.010,
                    0.45 + rng() * 0.55,
                    [1.0, 0.45 + rng() * 0.35, 0.12],
                    0.0,
                ),
                ParticleKind::Fireflies => (
                    (rng() - 0.5) * 0.05,
                    (rng() - 0.5) * 0.05,
                    0.008 + rng() * 0.016,
                    0.30 + rng() * 0.65,
                    [0.65, 1.0, 0.38],
                    0.0,
                ),
                ParticleKind::Stars => (
                    0.0,
                    0.0,
                    0.003 + rng() * 0.007,
                    0.25 + rng() * 0.75,
                    [0.88, 0.93, 1.0],
                    0.0,
                ),
                ParticleKind::Sakura => (
                    -0.08 - rng() * 0.10,
                    -(0.08 + rng() * 0.14),
                    0.007 + rng() * 0.011,
                    0.55 + rng() * 0.40,
                    [1.0, 0.72 + rng() * 0.15, 0.82],
                    rng() * std::f32::consts::PI,
                ),
                ParticleKind::Fog => (
                    (rng() - 0.5) * 0.02,
                    (rng() - 0.5) * 0.006,
                    0.30 + rng() * 0.35,
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
                color: color_override.unwrap_or(default_col),
            });
        }

        let blend = match kind {
            ParticleKind::Embers | ParticleKind::Fireflies | ParticleKind::Stars => {
                BlendMode::Additive
            }
            _ => BlendMode::Normal,
        };

        Self {
            kind,
            blend,
            parallax: [0.15, 0.15],
            audio_reactive,
            particles,
        }
    }

    fn update_and_pack(
        &mut self,
        dt: f32,
        time: f32,
        aspect: f32,
        cam_offset: [f32; 2],
        audio: [f32; 4],
        out: &mut Vec<f32>,
    ) {
        out.clear();
        let speed_boost = if self.audio_reactive {
            1.0 + audio[0] * 2.2 + audio[3] * 1.2
        } else {
            1.0
        };
        let dt_clamped = dt.clamp(0.0, 0.1) * speed_boost;

        for p in &mut self.particles {
            match self.kind {
                ParticleKind::Snow => {
                    p.x += (p.vx + (time * 1.3 + p.phase).sin() * 0.04) * dt_clamped;
                    p.y += p.vy * dt_clamped;
                }
                ParticleKind::Rain => {
                    p.x += p.vx * dt_clamped;
                    p.y += p.vy * dt_clamped;
                }
                ParticleKind::Embers => {
                    p.x += (p.vx + (time * 2.5 + p.phase).sin() * 0.08) * dt_clamped;
                    p.y += p.vy * dt_clamped;
                }
                ParticleKind::Fireflies => {
                    p.x += (p.vx + (time * 0.9 + p.phase).cos() * 0.04) * dt_clamped;
                    p.y += (p.vy + (time * 1.1 + p.phase).sin() * 0.04) * dt_clamped;
                }
                ParticleKind::Stars => {}
                ParticleKind::Fog => {
                    p.x += (p.vx + (time * 0.25 + p.phase).sin() * 0.006) * dt_clamped;
                    p.y += p.vy * dt_clamped;
                }
                ParticleKind::Sakura => {
                    p.x += (p.vx + (time * 1.7 + p.phase).sin() * 0.06) * dt_clamped;
                    p.y += p.vy * dt_clamped;
                    p.rot += dt_clamped * 1.5;
                }
            }

            if p.y < -1.25 {
                p.y = 1.22;
                p.x = ((p.x + 3.2) % 2.4) - 1.2;
            } else if p.y > 1.25 {
                p.y = -1.22;
                p.x = ((p.x + 3.2) % 2.4) - 1.2;
            }
            if p.x < -1.25 {
                p.x += 2.5;
            } else if p.x > 1.25 {
                p.x -= 2.5;
            }

            let twinkle = match self.kind {
                ParticleKind::Fireflies | ParticleKind::Stars | ParticleKind::Embers => {
                    0.55 + 0.45 * (time * 3.0 + p.phase).sin()
                }
                _ => 1.0,
            };
            let audio_glow = if self.audio_reactive {
                1.0 + audio[0] * 0.8
            } else {
                1.0
            };

            let (w, h) = match self.kind {
                ParticleKind::Rain => (p.size * 0.35 / aspect.max(0.5), p.size * 4.8),
                ParticleKind::Sakura => (p.size * 1.3 / aspect.max(0.5), p.size * 0.8),
                _ => (p.size / aspect.max(0.5), p.size),
            };

            let px = p.x - cam_offset[0] * self.parallax[0];
            let py = p.y - cam_offset[1] * self.parallax[1];

            // 9 floats per instance: x, y, w, h, r, g, b, a, rot
            out.extend_from_slice(&[
                px,
                py,
                w * audio_glow,
                h * audio_glow,
                p.color[0],
                p.color[1],
                p.color[2],
                (p.alpha * twinkle).clamp(0.0, 1.0),
                p.rot,
            ]);
        }
    }
}

pub struct SceneRenderer {
    bg_triangle: Triangle,
    bg_prog: Program,
    layer_prog: Program,
    quad_vao: glow::VertexArray,
    #[allow(dead_code)]
    quad_vbo: glow::Buffer,
    particle_prog: Program,
    particle_vao: glow::VertexArray,
    #[allow(dead_code)]
    particle_quad_vbo: glow::Buffer,
    particle_inst_vbo: glow::Buffer,
    instance_scratch: Vec<f32>,

    ortho_size: [f32; 2],
    clear_color: [f32; 3],
    vignette: f32,
    parallax_enabled: bool,
    parallax_amount: f32,
    parallax_mouse_influence: f32,
    camera_shake: bool,
    camera_shake_amp: f32,
    camera_shake_speed: f32,
    smooth_mouse: [f32; 2],
    speed_multiplier: f32,

    layers: Vec<SceneLayer>,
    emitters: Vec<ParticleEmitter>,
    properties: HashMap<String, String>,

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
        let bg_triangle = Triangle::new(gl)?;
        let bg_prog = Program::new(gl, POST_VS, POST_FS)?;
        let layer_prog = Program::new(gl, LAYER_VS, LAYER_FS)?;
        let particle_prog = Program::new(gl, PARTICLE_VS, PARTICLE_FS)?;

        let (quad_vao, quad_vbo) = create_layer_quad(gl)?;
        let (particle_vao, particle_quad_vbo, particle_inst_vbo) = create_particle_buffers(gl)?;

        let mut this = Self {
            bg_triangle,
            bg_prog,
            layer_prog,
            quad_vao,
            quad_vbo,
            particle_prog,
            particle_vao,
            particle_quad_vbo,
            particle_inst_vbo,
            instance_scratch: Vec::with_capacity(4096),
            ortho_size: [1920.0, 1080.0],
            clear_color: [0.04, 0.05, 0.08],
            vignette: 0.25,
            parallax_enabled: true,
            parallax_amount: 1.0,
            parallax_mouse_influence: 0.12,
            camera_shake: false,
            camera_shake_amp: 0.01,
            camera_shake_speed: 1.5,
            smooth_mouse: [0.0, 0.0],
            speed_multiplier: 1.0,
            layers: Vec::new(),
            emitters: Vec::new(),
            properties: HashMap::new(),
            width,
            height,
            paused: false,
            title: entry
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("scene")
                .to_string(),
        };

        // Load initial properties from project.json if available.
        if let Some(proj) = project {
            if let Some(props) = proj
                .get("general")
                .and_then(|g| g.get("properties"))
                .and_then(|p| p.as_object())
            {
                for (k, v) in props {
                    if let Some(val) = v.get("value") {
                        let s = match val {
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        this.properties.insert(k.clone(), s);
                    }
                }
            }
        }

        // Load either native Anima TOML scene or Wallpaper Engine JSON scene.
        let entry_name = entry
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("scene.json");

        if entry_name.ends_with(".toml") || entry_name.ends_with(".anima") {
            let text = std::fs::read_to_string(entry)
                .with_context(|| format!("reading native scene {}", entry.display()))?;
            this.load_native_toml(gl, &pkg, &text)?;
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
            this.load_we_scene_json(gl, &pkg, &json_text)?;
        }

        // Apply initial property overrides (such as schemecolor).
        let props_clone = this.properties.clone();
        for (k, v) in props_clone {
            this.apply_property(&k, &v);
        }

        Ok(this)
    }

    fn load_native_toml(&mut self, gl: &glow::Context, pkg: &PackageFs, text: &str) -> Result<()> {
        let doc: NativeSceneDoc = toml::from_str(text).context("parsing native scene TOML")?;
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

        for l in doc.layers {
            let tex = if let Some(path) = &l.image {
                let decoded = pkg.load_rgba(path)?;
                create_texture(
                    gl,
                    decoded.image.width(),
                    decoded.image.height(),
                    decoded.image.as_raw(),
                    Filter::Linear,
                    true,
                )?
            } else {
                create_texture(gl, 1, 1, &[255, 255, 255, 255], Filter::Nearest, false)?
            };

            let c = l.color.unwrap_or([1.0, 1.0, 1.0]);
            let a = l.alpha.unwrap_or(1.0);
            self.layers.push(SceneLayer {
                name: l.name.unwrap_or_else(|| "layer".into()),
                texture: tex,
                origin: l.origin.unwrap_or([
                    self.ortho_size[0] * 0.5,
                    self.ortho_size[1] * 0.5,
                    0.0,
                ]),
                size: l.size.unwrap_or(self.ortho_size),
                scale: l.scale.unwrap_or([1.0, 1.0]),
                rotation: l.rotation.unwrap_or(0.0),
                color: [c[0], c[1], c[2], a],
                parallax_depth: l.parallax_depth.unwrap_or([0.0, 0.0]),
                blend: BlendMode::from_str(l.blend.as_deref().unwrap_or("normal")),
                wave_amp: l.wave_amp.unwrap_or(0.0),
                wave_freq: l.wave_freq.unwrap_or(12.0),
                sway_amp: l.sway_amp.unwrap_or(0.0),
                pulse_amp: l.pulse_amp.unwrap_or(0.0),
                scroll: l.scroll.unwrap_or([0.0, 0.0]),
                audio_react: l.audio_react.unwrap_or(0.0),
                chromatic: l.chromatic.unwrap_or(0.0),
            });
        }

        for p in doc.particles {
            let kind = ParticleKind::from_hint(&p.preset);
            let count = p.count.unwrap_or(120).clamp(1, 2000);
            let mut emitter =
                ParticleEmitter::new(kind, count, p.color, p.audio_reactive.unwrap_or(true));
            if let Some(par) = p.parallax_depth {
                emitter.parallax = par;
            }
            self.emitters.push(emitter);
        }

        Ok(())
    }

    fn load_we_scene_json(
        &mut self,
        gl: &glow::Context,
        pkg: &PackageFs,
        json_text: &str,
    ) -> Result<()> {
        let root: serde_json::Value =
            serde_json::from_str(json_text).context("parsing Wallpaper Engine scene.json")?;

        if let Some(general) = root.get("general") {
            if let Some(ortho) = general.get("orthogonalprojection") {
                let w = ortho.get("width").and_then(|v| v.as_f64()).unwrap_or(1920.0) as f32;
                let h = ortho.get("height").and_then(|v| v.as_f64()).unwrap_or(1080.0) as f32;
                self.ortho_size = [w.max(1.0), h.max(1.0)];
            }
            if let Some(cc) = general.get("clearcolor") {
                self.clear_color = parse_vec3_value(cc, [0.05, 0.06, 0.09]);
            }
            if let Some(p) = general.get("cameraparallax").and_then(value_bool) {
                self.parallax_enabled = p;
            }
            if let Some(a) = general.get("cameraparallaxamount").and_then(value_f32) {
                self.parallax_amount = a;
            }
            if let Some(m) = general.get("cameraparallaxmouseinfluence").and_then(value_f32) {
                self.parallax_mouse_influence = (m * 0.15).clamp(0.02, 0.45);
            }
            if let Some(cs) = general.get("camerashake").and_then(value_bool) {
                self.camera_shake = cs;
            }
            if let Some(amp) = general.get("camerashakeamplitude").and_then(value_f32) {
                self.camera_shake_amp = (amp * 0.02).clamp(0.0, 0.08);
            }
            if let Some(spd) = general.get("camerashakespeed").and_then(value_f32) {
                self.camera_shake_speed = spd.clamp(0.1, 10.0);
            }
        }

        let Some(objects) = root.get("objects").and_then(|o| o.as_array()) else {
            return Ok(());
        };

        for obj in objects {
            if obj.get("visible").and_then(value_bool) == Some(false) {
                continue;
            }

            let name = obj
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("object")
                .to_string();

            // Particle object? Wallpaper Engine stores these as references to a
            // particle preset (`particles/presets/*.json`).
            if let Some(particle_path) = obj.get("particle").and_then(|v| v.as_str()) {
                let kind = ParticleKind::from_hint(&format!("{name} {particle_path}"));

                // Pull the particle count (and a color hint) from the preset so
                // heavier effects (rain, fog) behave like the original.
                let preset = pkg
                    .read_to_string(particle_path)
                    .ok()
                    .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
                let count = preset
                    .as_ref()
                    .and_then(|p| p.get("maxcount"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(120)
                    .clamp(1, 1200) as usize;
                // Fog presets stack many huge billboards; keep them sparse.
                let count = if kind == ParticleKind::Fog {
                    count.min(24)
                } else {
                    count
                };

                let override_obj = obj.get("instanceoverride");
                let col = override_obj
                    .and_then(|o| o.get("colorn"))
                    .and_then(|v| v.as_str())
                    .map(|s| parse_vec3_str(s, [1.0, 1.0, 1.0]))
                    .or_else(|| {
                        obj.get("color")
                            .map(|c| parse_vec3_value(c, [1.0, 1.0, 1.0]))
                    });
                let par = obj
                    .get("parallaxDepth")
                    .map(|p| parse_vec2_value(p, [0.15, 0.15]))
                    .unwrap_or([0.15, 0.15]);
                let mut emitter = ParticleEmitter::new(kind, count, col, true);
                emitter.parallax = par;
                self.emitters.push(emitter);
                continue;
            }

            // Image / model object?
            let Some(image_ref) = obj.get("image").and_then(|v| v.as_str()) else {
                continue;
            };

            let resolved = resolve_we_image_model(pkg, image_ref);
            let Ok(decoded) = pkg.load_rgba(&resolved.material_or_tex) else {
                log::debug!("scene object {name}: skipping unresolved asset {}", resolved.material_or_tex);
                continue;
            };

            let tex_w = decoded.image.width() as f32;
            let tex_h = decoded.image.height() as f32;
            let texture = create_texture(
                gl,
                decoded.image.width(),
                decoded.image.height(),
                decoded.image.as_raw(),
                Filter::Linear,
                true,
            )?;

            let origin = obj
                .get("origin")
                .map(|v| parse_vec3_plain(v, [self.ortho_size[0] * 0.5, self.ortho_size[1] * 0.5, 0.0]))
                .unwrap_or([self.ortho_size[0] * 0.5, self.ortho_size[1] * 0.5, 0.0]);
            let size = obj
                .get("size")
                .map(|v| parse_vec2_value(v, [tex_w, tex_h]))
                .or(resolved.crop_size)
                .unwrap_or([tex_w, tex_h]);
            let scale3 = obj
                .get("scale")
                .map(|v| parse_vec3_plain(v, [1.0, 1.0, 1.0]))
                .unwrap_or([1.0, 1.0, 1.0]);
            let angles = obj
                .get("angles")
                .map(|v| parse_vec3_plain(v, [0.0, 0.0, 0.0]))
                .unwrap_or([0.0, 0.0, 0.0]);
            let rgb = obj
                .get("color")
                .map(|v| parse_vec3_value(v, [1.0, 1.0, 1.0]))
                .unwrap_or([1.0, 1.0, 1.0]);
            let alpha = obj.get("alpha").and_then(value_f32).unwrap_or(1.0);
            let parallax_depth = obj
                .get("parallaxDepth")
                .map(|v| parse_vec2_value(v, [0.0, 0.0]))
                .unwrap_or([0.0, 0.0]);

            let mut wave_amp = 0.0f32;
            let mut wave_freq = 14.0f32;
            let mut sway_amp = 0.0f32;
            let mut pulse_amp = 0.0f32;
            let mut scroll = [0.0f32, 0.0f32];
            let mut audio_react = 0.0f32;
            let mut chromatic = 0.0f32;

            if let Some(effects) = obj.get("effects").and_then(|e| e.as_array()) {
                for ef in effects {
                    if ef.get("visible").and_then(value_bool) == Some(false) {
                        continue;
                    }
                    let ef_file = ef
                        .get("file")
                        .or_else(|| ef.get("name"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    if ef_file.contains("water") || ef_file.contains("wave") || ef_file.contains("ripple") {
                        wave_amp = ef.get("strength").and_then(value_f32).unwrap_or(0.004);
                        wave_freq = ef.get("scale").and_then(value_f32).unwrap_or(16.0);
                    } else if ef_file.contains("foliage") || ef_file.contains("sway") {
                        sway_amp = ef.get("strength").and_then(value_f32).unwrap_or(0.008);
                    } else if ef_file.contains("pulse") || ef_file.contains("shimmer") {
                        pulse_amp = ef.get("strength").and_then(value_f32).unwrap_or(0.15);
                    } else if ef_file.contains("scroll") {
                        scroll = [
                            ef.get("speed_x").and_then(value_f32).unwrap_or(0.02),
                            ef.get("speed_y").and_then(value_f32).unwrap_or(0.0),
                        ];
                    } else if ef_file.contains("audio") {
                        audio_react = ef.get("strength").and_then(value_f32).unwrap_or(0.45);
                    } else if ef_file.contains("chrom") || ef_file.contains("glitch") {
                        chromatic = ef.get("strength").and_then(value_f32).unwrap_or(0.004);
                    }
                }
            }

            self.layers.push(SceneLayer {
                name,
                texture,
                origin,
                size,
                scale: [scale3[0], scale3[1]],
                rotation: angles[2].to_radians(),
                color: [rgb[0], rgb[1], rgb[2], alpha],
                parallax_depth,
                blend: resolved.blend,
                wave_amp,
                wave_freq,
                sway_amp,
                pulse_amp,
                scroll,
                audio_react,
                chromatic,
            });
        }

        log::debug!(
            "scene: ortho={:?} surface={}x{} layers={} emitters={}",
            self.ortho_size,
            self.width,
            self.height,
            self.layers.len(),
            self.emitters.len()
        );

        // Sort back-to-front by Z coordinate (`origin[2]`) if layers specify depth.
        self.layers.sort_by(|a, b| {
            a.origin[2]
                .partial_cmp(&b.origin[2])
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        // If a scene had zero decoded image layers and zero emitters, add a subtle star/ember emitter so it's never blank.
        if self.layers.is_empty() && self.emitters.is_empty() {
            self.emitters
                .push(ParticleEmitter::new(ParticleKind::Fireflies, 100, None, true));
        }

        Ok(())
    }

    pub fn apply_property(&mut self, key: &str, value: &str) {
        self.properties.insert(key.to_string(), value.to_string());
        match key.to_ascii_lowercase().as_str() {
            // `schemecolor` is the Workshop browser/loading accent color in
            // Wallpaper Engine, not the scene clear color. Only honor an
            // explicit `clearcolor` property here.
            "clearcolor" => {
                let c = parse_vec3_str(value, self.clear_color);
                self.clear_color = c;
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
}

impl Renderer for SceneRenderer {
    fn render(&mut self, gl: &glow::Context, info: &FrameInfo) -> Result<bool> {
        let time = (info.time as f32) * self.speed_multiplier;
        let dt = (info.delta as f32) * self.speed_multiplier;

        // Normalized cursor in `-1.0..=1.0`.
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

        let mut cam_offset = [0.0f32, 0.0f32];
        if self.parallax_enabled {
            let s = self.parallax_amount * self.parallax_mouse_influence;
            cam_offset[0] += self.smooth_mouse[0] * s;
            cam_offset[1] += self.smooth_mouse[1] * s;
        }
        if self.camera_shake {
            let t = time * self.camera_shake_speed;
            cam_offset[0] += (t * 1.7).sin() * self.camera_shake_amp;
            cam_offset[1] += (t * 2.3).cos() * self.camera_shake_amp;
        }

        let ortho_w = self.ortho_size[0].max(1.0);
        let ortho_h = self.ortho_size[1].max(1.0);
        let aspect = (self.width.max(1) as f32) / (self.height.max(1) as f32);

        unsafe {
            gl.viewport(0, 0, self.width, self.height);
            gl.disable(glow::DEPTH_TEST);

            // 1. Background gradient / clear color pass.
            gl.disable(glow::BLEND);
            self.bg_prog.bind(gl);
            self.bg_prog.set_f32(gl, "u_time", time);
            self.bg_prog.set_vec3(
                gl,
                "u_clear_color",
                self.clear_color[0],
                self.clear_color[1],
                self.clear_color[2],
            );
            self.bg_prog.set_f32(gl, "u_vignette", self.vignette);
            self.bg_triangle.draw(gl);

            // 2. Scene image layers.
            gl.enable(glow::BLEND);
            self.layer_prog.bind(gl);
            self.layer_prog.set_f32(gl, "u_time", time);
            self.layer_prog.set_vec4(gl, "u_audio", info.audio);
            self.layer_prog.set_i32(gl, "u_tex", 0);
            gl.bind_vertex_array(Some(self.quad_vao));

            for layer in &self.layers {
                apply_blend(gl, layer.blend);

                let cx = (layer.origin[0] / ortho_w) * 2.0 - 1.0
                    - cam_offset[0] * layer.parallax_depth[0];
                let cy = (layer.origin[1] / ortho_h) * 2.0 - 1.0
                    - cam_offset[1] * layer.parallax_depth[1];
                let hw = (layer.size[0] * layer.scale[0]) / ortho_w;
                let hh = (layer.size[1] * layer.scale[1]) / ortho_h;

                self.layer_prog.set_vec2(gl, "u_center", cx, cy);
                self.layer_prog.set_vec2(gl, "u_half_size", hw, hh);
                self.layer_prog.set_f32(gl, "u_rotation", layer.rotation);
                self.layer_prog.set_vec4(gl, "u_color", layer.color);
                self.layer_prog.set_vec4(
                    gl,
                    "u_fx1",
                    [layer.wave_amp, layer.wave_freq, layer.sway_amp, layer.pulse_amp],
                );
                self.layer_prog.set_vec4(
                    gl,
                    "u_fx2",
                    [
                        layer.scroll[0],
                        layer.scroll[1],
                        layer.audio_react,
                        layer.chromatic,
                    ],
                );

                gl.active_texture(glow::TEXTURE0);
                gl.bind_texture(glow::TEXTURE_2D, Some(layer.texture));
                gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
            }

            // 3. Instanced particle emitters.
            if !self.emitters.is_empty() {
                self.particle_prog.bind(gl);
                gl.bind_vertex_array(Some(self.particle_vao));
                gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.particle_inst_vbo));

                for emitter in &mut self.emitters {
                    emitter.update_and_pack(
                        dt,
                        time,
                        aspect,
                        cam_offset,
                        info.audio,
                        &mut self.instance_scratch,
                    );
                    if self.instance_scratch.is_empty() {
                        continue;
                    }
                    apply_blend(gl, emitter.blend);
                    self.particle_prog
                        .set_i32(gl, "u_shape", emitter.kind.shape_id());
                    gl.buffer_data_u8_slice(
                        glow::ARRAY_BUFFER,
                        bytemuck_cast(&self.instance_scratch),
                        glow::STREAM_DRAW,
                    );
                    let inst_count = (self.instance_scratch.len() / 9) as i32;
                    gl.draw_arrays_instanced(glow::TRIANGLE_STRIP, 0, 4, inst_count);
                }
            }

            gl.bind_vertex_array(None);
            gl.disable(glow::BLEND);
        }

        Ok(true)
    }

    fn resize(&mut self, _gl: &glow::Context, width: i32, height: i32) -> Result<()> {
        self.width = width;
        self.height = height;
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
        self.layers.iter().any(|l| l.audio_react > 0.0)
            || self.emitters.iter().any(|e| e.audio_reactive)
    }

    fn describe(&self) -> String {
        format!(
            "scene {} ({} layers, {} emitters)",
            self.title,
            self.layers.len(),
            self.emitters.len()
        )
    }
}

struct ResolvedModel {
    material_or_tex: String,
    crop_size: Option<[f32; 2]>,
    blend: BlendMode,
}

fn resolve_we_image_model(pkg: &PackageFs, image_ref: &str) -> ResolvedModel {
    let mut out = ResolvedModel {
        material_or_tex: image_ref.to_string(),
        crop_size: None,
        blend: BlendMode::Normal,
    };

    if image_ref.ends_with(".json") {
        if let Ok(model_str) = pkg.read_to_string(image_ref) {
            if let Ok(model_val) = serde_json::from_str::<serde_json::Value>(&model_str) {
                let w = model_val.get("width").and_then(value_f32);
                let h = model_val.get("height").and_then(value_f32);
                if let (Some(w), Some(h)) = (w, h) {
                    out.crop_size = Some([w, h]);
                }
                if let Some(mat_path) = model_val.get("material").and_then(|v| v.as_str()) {
                    out.material_or_tex = mat_path.to_string();
                    if let Ok(mat_str) = pkg.read_to_string(mat_path) {
                        if let Ok(mat_val) = serde_json::from_str::<serde_json::Value>(&mat_str) {
                            if let Some(pass) = mat_val
                                .get("passes")
                                .and_then(|p| p.as_array())
                                .and_then(|a| a.first())
                            {
                                if let Some(blending) =
                                    pass.get("blending").and_then(|v| v.as_str())
                                {
                                    out.blend = BlendMode::from_str(blending);
                                }
                                if let Some(tex) = pass
                                    .get("textures")
                                    .and_then(|t| t.as_array())
                                    .and_then(|a| a.first())
                                    .and_then(|v| v.as_str())
                                {
                                    out.material_or_tex = tex.to_string();
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    out
}

unsafe fn apply_blend(gl: &glow::Context, mode: BlendMode) {
    unsafe {
        match mode {
            BlendMode::Normal => gl.blend_func(glow::SRC_ALPHA, glow::ONE_MINUS_SRC_ALPHA),
            BlendMode::Additive => gl.blend_func(glow::SRC_ALPHA, glow::ONE),
            BlendMode::Multiply => gl.blend_func(glow::DST_COLOR, glow::ONE_MINUS_SRC_ALPHA),
            BlendMode::Screen => gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_COLOR),
        }
    }
}

fn create_layer_quad(gl: &glow::Context) -> Result<(glow::VertexArray, glow::Buffer)> {
    // Interleaved [pos.x, pos.y, uv.x, uv.y] with flipped V for top-down image coords.
    const VERTS: [f32; 16] = [
        -1.0, -1.0, 0.0, 1.0, // bottom-left
         1.0, -1.0, 1.0, 1.0, // bottom-right
        -1.0,  1.0, 0.0, 0.0, // top-left
         1.0,  1.0, 1.0, 0.0, // top-right
    ];
    unsafe {
        let vao = gl.create_vertex_array().map_err(|e| anyhow::anyhow!(e))?;
        let vbo = gl.create_buffer().map_err(|e| anyhow::anyhow!(e))?;
        gl.bind_vertex_array(Some(vao));
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
        gl.buffer_data_u8_slice(
            glow::ARRAY_BUFFER,
            bytemuck_cast(&VERTS),
            glow::STATIC_DRAW,
        );
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
        // location 1: vec4 (x, y, w, h)
        gl.enable_vertex_attrib_array(1);
        gl.vertex_attrib_pointer_f32(1, 4, glow::FLOAT, false, stride, 0);
        gl.vertex_attrib_divisor(1, 1);
        // location 2: vec4 (r, g, b, a)
        gl.enable_vertex_attrib_array(2);
        gl.vertex_attrib_pointer_f32(2, 4, glow::FLOAT, false, stride, 16);
        gl.vertex_attrib_divisor(2, 1);
        // location 3: float (rot)
        gl.enable_vertex_attrib_array(3);
        gl.vertex_attrib_pointer_f32(3, 1, glow::FLOAT, false, stride, 32);
        gl.vertex_attrib_divisor(3, 1);

        gl.bind_vertex_array(None);
        Ok((vao, q_vbo, i_vbo))
    }
}

fn value_f32(v: &serde_json::Value) -> Option<f32> {
    if let Some(f) = v.as_f64() {
        return Some(f as f32);
    }
    if let Some(s) = v.as_str() {
        return s.trim().parse::<f32>().ok();
    }
    if let Some(obj) = v.as_object() {
        if let Some(inner) = obj.get("value") {
            return value_f32(inner);
        }
    }
    None
}

fn value_bool(v: &serde_json::Value) -> Option<bool> {
    if let Some(b) = v.as_bool() {
        return Some(b);
    }
    if let Some(obj) = v.as_object() {
        if let Some(inner) = obj.get("value") {
            return value_bool(inner);
        }
    }
    None
}

fn parse_vec2_value(v: &serde_json::Value, default: [f32; 2]) -> [f32; 2] {
    if let Some(s) = v.as_str() {
        let mut it = s.split_whitespace().filter_map(|p| p.parse::<f32>().ok());
        return [
            it.next().unwrap_or(default[0]),
            it.next().unwrap_or(default[1]),
        ];
    }
    if let Some(obj) = v.as_object() {
        if let Some(inner) = obj.get("value") {
            return parse_vec2_value(inner, default);
        }
    }
    default
}

fn parse_vec3_value(v: &serde_json::Value, default: [f32; 3]) -> [f32; 3] {
    if let Some(s) = v.as_str() {
        return parse_vec3_str(s, default);
    }
    if let Some(obj) = v.as_object() {
        if let Some(inner) = obj.get("value") {
            return parse_vec3_value(inner, default);
        }
    }
    default
}

/// Parse a generic three-component vector (`origin`, `scale`, `angles`) without
/// the color normalization performed by [`parse_vec3_str`].
fn parse_vec3_plain(v: &serde_json::Value, default: [f32; 3]) -> [f32; 3] {
    if let Some(s) = v.as_str() {
        let mut it = s
            .split(|c: char| c.is_whitespace() || c == ',')
            .filter(|p| !p.is_empty())
            .filter_map(|p| p.parse::<f32>().ok());
        return [
            it.next().unwrap_or(default[0]),
            it.next().unwrap_or(default[1]),
            it.next().unwrap_or(default[2]),
        ];
    }
    if let Some(obj) = v.as_object() {
        if let Some(inner) = obj.get("value") {
            return parse_vec3_plain(inner, default);
        }
    }
    default
}

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

#[allow(dead_code)]
fn _keep_vbo(a: glow::Buffer, b: glow::Buffer) {
    let _ = (a, b);
}
