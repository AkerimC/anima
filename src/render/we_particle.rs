//! Wallpaper Engine particle preset parser, CPU simulator, and instanced ribbbon/sprite packer.

use std::collections::HashMap;

use crate::pkg::PackageFs;

use super::scene::{
    parse_color, read_scene_text, resolve_prop, value_bool, value_f32, value_vec, Affine,
    LayerBlend,
};
use super::we_effect::TextureCache;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ParticleKind {
    Snow,
    Rain,
    Embers,
    Fireflies,
    Stars,
    Sakura,
    Fog,
}

impl ParticleKind {
    pub(super) fn from_hint(s: &str) -> Self {
        let l = s.to_ascii_lowercase();
        if l.contains("fog") || l.contains("mist") || l.contains("smoke") || l.contains("cloud") {
            ParticleKind::Fog
        } else if l.contains("rain") || l.contains("storm") || l.contains("splash") {
            ParticleKind::Rain
        } else if l.contains("ember")
            || l.contains("fire")
            || l.contains("spark")
            || l.contains("lightning")
        {
            ParticleKind::Embers
        } else if l.contains("fly") || l.contains("glow") || l.contains("orb") || l.contains("bubble")
        {
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

    pub(super) fn shape_id(self) -> i32 {
        match self {
            ParticleKind::Rain => 1,
            ParticleKind::Stars | ParticleKind::Fireflies => 2,
            _ => 0,
        }
    }
}

const TRAIL_CAP: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq)]
enum ParticleRendererKind {
    Sprite,
    SpriteTrail { length: f32, max_length: f32 },
    RopeTrail { length: f32 },
    Rope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EmitterShape {
    Sphere,
    Box,
    LayerImage,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ChildSpawnMode {
    Static,
    EventFollow,
    EventSpawn,
}

#[derive(Debug, Clone)]
pub(super) struct LayerImageMask {
    pub(super) points: Vec<[f32; 2]>,
}

/// Sample active emission points from an `emitterimage` layer into the particle system's local space.
pub(super) fn sample_layer_image_mask(
    rgba: &[u8],
    width: u32,
    height: u32,
    mask_size: [f32; 2],
    mask_world: Affine,
    particle_world: Affine,
) -> Option<LayerImageMask> {
    let needed = (width as usize).checked_mul(height as usize)?.checked_mul(4)?;
    if width == 0 || height == 0 || rgba.len() < needed {
        return None;
    }
    let px_count = (width as usize) * (height as usize);
    let step = (((px_count / 4096).max(1)) as f32).sqrt().ceil().max(1.0) as u32;

    let mut has_transparent = false;
    let mut y = 0u32;
    'alpha_scan: while y < height {
        let mut x = 0u32;
        while x < width {
            let idx = ((y * width + x) as usize) * 4;
            if rgba[idx + 3] < 250 {
                has_transparent = true;
                break 'alpha_scan;
            }
            x += step;
        }
        y += step;
    }

    let mut raw_points: Vec<[f32; 2]> = Vec::new();
    let inv_w = 1.0 / width as f32;
    let inv_h = 1.0 / height as f32;
    let mut py = 0u32;
    while py < height {
        let mut px = 0u32;
        while px < width {
            let idx = ((py * width + px) as usize) * 4;
            let active = if has_transparent {
                rgba[idx + 3] > 24
            } else {
                rgba[idx].max(rgba[idx + 1]).max(rgba[idx + 2]) > 24
            };
            if active {
                let u = (px as f32 + 0.5) * inv_w;
                let v = (py as f32 + 0.5) * inv_h;
                let lx = (u - 0.5) * mask_size[0];
                let ly = (0.5 - v) * mask_size[1];
                let world = mask_world.apply([lx, ly]);
                if let Some(local) = particle_world.invert_point(world) {
                    raw_points.push(local);
                }
            }
            px += step;
        }
        py += step;
    }

    if raw_points.is_empty() {
        return None;
    }
    const MAX_POINTS: usize = 2048;
    let points = if raw_points.len() > MAX_POINTS {
        let mut down = Vec::with_capacity(MAX_POINTS);
        let len = raw_points.len();
        for i in 0..MAX_POINTS {
            let src_idx = (i * len) / MAX_POINTS;
            down.push(raw_points[src_idx]);
        }
        down
    } else {
        raw_points
    };
    Some(LayerImageMask { points })
}

#[derive(Debug, Clone, Copy, Default)]
struct ControlPointDef {
    offset: [f32; 2],
    flags: u32,
}

#[derive(Debug, Clone, Copy)]
struct AudioMod {
    mode: u32,
    bounds: [f32; 2],
    exponent: f32,
    freq_start: usize,
    freq_end: usize,
}

impl AudioMod {
    fn parse(val: &serde_json::Value) -> Option<Self> {
        let has_bounds = val.get("audioprocessingbounds").is_some();
        let mode_raw = val
            .get("audioprocessingmode")
            .and_then(|v| v.as_i64().or_else(|| value_f32(v).map(|f| f.round() as i64)));
        let mode = match mode_raw {
            Some(m) if m > 0 => m as u32,
            Some(_) => return None,
            None if has_bounds => 3,
            None => return None,
        };
        let bounds_v = val
            .get("audioprocessingbounds")
            .map(|b| value_vec(b, 3, [0.0, 1.0, 0.0]))
            .unwrap_or([0.0, 1.0, 0.0]);
        let bounds = [bounds_v[0], bounds_v[1]];
        let exponent = val
            .get("audioprocessingexponent")
            .and_then(value_f32)
            .unwrap_or(1.0);
        let freq_start = val
            .get("audioprocessingfrequencystart")
            .and_then(|v| v.as_i64().or_else(|| value_f32(v).map(|f| f.round() as i64)))
            .unwrap_or(0)
            .clamp(0, 63) as usize;
        let freq_end = val
            .get("audioprocessingfrequencyend")
            .and_then(|v| v.as_i64().or_else(|| value_f32(v).map(|f| f.round() as i64)))
            .unwrap_or(15)
            .clamp(freq_start as i64, 63) as usize;
        Some(Self {
            mode,
            bounds,
            exponent,
            freq_start,
            freq_end,
        })
    }

    fn eval(&self, spectrum: &[f32; 64]) -> f32 {
        let (lo_bin, hi_bin) = if self.freq_end <= 15 {
            ((self.freq_start * 4).min(63), (self.freq_end * 4 + 3).min(63))
        } else {
            (self.freq_start.min(63), self.freq_end.min(63))
        };
        let slice = &spectrum[lo_bin..=hi_bin.max(lo_bin)];
        let val = if self.mode == 2 {
            slice.iter().copied().fold(0.0f32, f32::max)
        } else {
            let sum: f32 = slice.iter().sum();
            sum / slice.len().max(1) as f32
        };
        let lo = self.bounds[0];
        let hi = self.bounds[1].max(lo + 1e-4);
        ((val - lo) / (hi - lo))
            .clamp(0.0, 1.0)
            .powf(self.exponent.max(0.05))
    }
}

#[derive(Debug, Clone, Copy)]
struct OscillateOp {
    freq_min: f32,
    freq_max: f32,
    scale_min: f32,
    scale_max: f32,
    phase_min: f32,
    phase_max: f32,
    mask: [f32; 2],
}

#[derive(Debug, Clone, Copy)]
struct TurbulenceOp {
    speed_min: f32,
    speed_max: f32,
    scale: f32,
    timescale: f32,
    mask: [f32; 2],
    phase_min: f32,
    phase_max: f32,
    audio: Option<AudioMod>,
}

#[derive(Debug, Clone, Copy)]
struct ScalarChangeOp {
    start_time: f32,
    end_time: f32,
    start_value: f32,
    end_value: f32,
}

#[derive(Debug, Clone, Copy)]
struct ColorChangeOp {
    start_time: f32,
    end_time: f32,
    start_value: [f32; 3],
    end_value: [f32; 3],
}

#[derive(Debug, Clone, Copy)]
struct AngularMoveOp {
    force_z: f32,
    drag: f32,
}

#[derive(Debug, Clone, Copy)]
struct CpAttractOp {
    cp_idx: usize,
    origin: [f32; 2],
    scale: f32,
    threshold: f32,
    audio: Option<AudioMod>,
}

#[derive(Debug, Clone, Copy)]
struct VortexOp {
    cp_idx: usize,
    offset: [f32; 2],
    axis_sign: f32,
    dist_inner: f32,
    dist_outer: f32,
    speed_inner: f32,
    speed_outer: f32,
    ring_radius: f32,
    ring_width: f32,
    audio: Option<AudioMod>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CollisionBehavior {
    Bounce,
    Slide,
    Stop,
}

#[derive(Debug, Clone, Copy)]
struct CollisionSphereOp {
    cp_idx: usize,
    origin: [f32; 2],
    radius: f32,
    behavior: CollisionBehavior,
}

#[derive(Debug, Clone, Copy)]
enum SequenceInit {
    AroundCp {
        cp_idx: usize,
        count: f32,
        speed_min: [f32; 2],
        speed_max: [f32; 2],
        bounds: [f32; 2],
    },
    BetweenCps {
        start_cp: usize,
        end_cp: usize,
        count: f32,
        mirror: bool,
    },
}

#[derive(Debug, Clone, Copy)]
struct HsvRange {
    hue_min: f32,
    hue_max: f32,
    sat_min: f32,
    sat_max: f32,
    val_min: f32,
    val_max: f32,
}

fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [f32; 3] {
    let h = h.rem_euclid(1.0) * 6.0;
    let s = s.clamp(0.0, 1.0);
    let v = v.clamp(0.0, 2.0);
    let i = h.floor() as i32;
    let f = h - i as f32;
    let p = v * (1.0 - s);
    let q = v * (1.0 - s * f);
    let t = v * (1.0 - s * (1.0 - f));
    match i.rem_euclid(6) {
        0 => [v, t, p],
        1 => [q, v, p],
        2 => [p, v, t],
        3 => [p, q, v],
        4 => [t, p, v],
        _ => [v, p, q],
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct Particle {
    pub(super) x: f32,
    pub(super) y: f32,
    vx: f32,
    vy: f32,
    size: f32,
    alpha: f32,
    phase: f32,
    osc_seed: f32,
    rot: f32,
    rot_x: f32,
    rot_y: f32,
    ang_vel: f32,
    age: f32,
    lifetime: f32,
    frame_seed: u32,
    base_color: [f32; 3],
    color: [f32; 3],
    trail: [[f32; 2]; TRAIL_CAP],
    trail_len: u8,
    trail_timer: f32,
}

#[derive(Debug, Clone)]
pub(super) struct WeParticleSpec {
    pub(super) transform: Affine,
    emitter_shape: EmitterShape,
    emitter_origin: [f32; 2],
    emitter_cp: usize,
    dist_min: [f32; 2],
    dist_max: [f32; 2],
    emitter_directions: [f32; 2],
    emitter_sign: [i8; 2],
    radial_speed_min: f32,
    radial_speed_max: f32,
    emitter_audio: Option<AudioMod>,
    layer_image_points: Vec<[f32; 2]>,
    control_points: [ControlPointDef; 8],
    spawn_rate: f32,
    instantaneous: usize,
    max_count: usize,
    spawn_accum: f32,
    life_min: f32,
    life_max: f32,
    size_min: f32,
    size_max: f32,
    vel_min: [f32; 2],
    vel_max: [f32; 2],
    color_min: [f32; 3],
    color_max: [f32; 3],
    color_mult: [f32; 3],
    hsv_color: Option<HsvRange>,
    alpha_min: f32,
    alpha_max: f32,
    rot_min: [f32; 3],
    rot_max: [f32; 3],
    ang_vel_min: f32,
    ang_vel_max: f32,
    sequence_init: Option<SequenceInit>,
    seq_counter: u32,
    gravity: [f32; 2],
    drag: f32,
    fade_in_time: f32,
    fade_out_time: f32,
    size_change: Option<ScalarChangeOp>,
    alpha_change: Option<ScalarChangeOp>,
    color_change: Option<ColorChangeOp>,
    osc_pos: Option<OscillateOp>,
    osc_alpha: Option<OscillateOp>,
    osc_size: Option<OscillateOp>,
    turbulence: Option<TurbulenceOp>,
    angular_move: Option<AngularMoveOp>,
    cp_attracts: Vec<CpAttractOp>,
    vortices: Vec<VortexOp>,
    collision_spheres: Vec<CollisionSphereOp>,
    renderer: ParticleRendererKind,
    random_frame: bool,
    rng_state: u32,
}

impl WeParticleSpec {
    fn uses_audio(&self) -> bool {
        self.emitter_audio.is_some()
            || self.turbulence.as_ref().is_some_and(|t| t.audio.is_some())
            || self.cp_attracts.iter().any(|c| c.audio.is_some())
            || self.vortices.iter().any(|v| v.audio.is_some())
    }

    fn resolve_cps(&self, pointer_world: [f32; 2]) -> [[f32; 2]; 8] {
        let mut out = [[0.0f32; 2]; 8];
        let mut ptr_local: Option<[f32; 2]> = None;
        for (idx, cp) in self.control_points.iter().enumerate() {
            out[idx] = if (cp.flags & 1) != 0 {
                let ptr = *ptr_local.get_or_insert_with(|| {
                    self.transform.invert_point(pointer_world).unwrap_or([0.0, 0.0])
                });
                [ptr[0] + cp.offset[0], ptr[1] + cp.offset[1]]
            } else if (cp.flags & 2) != 0 {
                self.transform.invert_point(cp.offset).unwrap_or(cp.offset)
            } else {
                cp.offset
            };
        }
        out
    }

    fn next_f32(&mut self) -> f32 {
        self.rng_state ^= self.rng_state << 13;
        self.rng_state ^= self.rng_state >> 17;
        self.rng_state ^= self.rng_state << 5;
        if self.rng_state == 0 {
            self.rng_state = 0x9E37_79B9;
        }
        (self.rng_state as f32) / (u32::MAX as f32)
    }

    fn sample_box_offset(&mut self) -> (f32, f32, f32, f32) {
        let mut rx = self.next_f32() * 2.0 - 1.0;
        let mut ry = self.next_f32() * 2.0 - 1.0;
        if self.emitter_sign[0] > 0 {
            rx = rx.abs();
        } else if self.emitter_sign[0] < 0 {
            rx = -rx.abs();
        }
        if self.emitter_sign[1] > 0 {
            ry = ry.abs();
        } else if self.emitter_sign[1] < 0 {
            ry = -ry.abs();
        }
        let sx = if rx >= 0.0 { 1.0 } else { -1.0 };
        let sy = if ry >= 0.0 { 1.0 } else { -1.0 };
        let dx = self.dist_min[0] + rx.abs() * (self.dist_max[0] - self.dist_min[0]).max(0.0);
        let dy = self.dist_min[1] + ry.abs() * (self.dist_max[1] - self.dist_min[1]).max(0.0);
        let px = sx * dx * self.emitter_directions[0];
        let py = sy * dy * self.emitter_directions[1];
        let len = px.hypot(py).max(1e-4);
        (px, py, px / len, py / len)
    }

    #[allow(clippy::too_many_arguments)]
    fn parse(
        preset: &serde_json::Value,
        obj: &serde_json::Value,
        transform: Affine,
        props: &HashMap<String, serde_json::Value>,
        count_cap_override: Option<usize>,
        layer_mask: Option<LayerImageMask>,
    ) -> (Self, usize, f32, f32, f32, f32, f32) {
        let ov = obj.get("instanceoverride");
        let ov_val = |k: &str| {
            ov.and_then(|o| o.get(k))
                .map(|v| resolve_prop(v, props))
                .and_then(|v| value_f32(&v))
                .unwrap_or(1.0)
        };
        let ov_alpha = ov_val("alpha");
        let ov_size = ov_val("size");
        let ov_speed = ov_val("speed");
        let ov_rate = ov_val("rate");
        let ov_count = ov_val("count");
        let ov_life = ov_val("lifetime");
        let color_mult = ov
            .and_then(|o| o.get("colorn").or_else(|| o.get("color")))
            .or_else(|| obj.get("color"))
            .map(|v| parse_color(&resolve_prop(v, props), [1.0, 1.0, 1.0]))
            .unwrap_or([1.0, 1.0, 1.0]);

        let mut control_points = [ControlPointDef::default(); 8];
        if let Some(cps) = preset.get("controlpoint").and_then(|c| c.as_array()) {
            for (fallback_idx, cp) in cps.iter().enumerate() {
                let idx = cp
                    .get("id")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(fallback_idx as i64)
                    .clamp(0, 7) as usize;
                if let Some(off) = cp.get("offset") {
                    let o = value_vec(off, 3, [0.0, 0.0, 0.0]);
                    control_points[idx].offset = [o[0], o[1]];
                }
                if let Some(fl) = cp.get("flags").and_then(|v| v.as_u64()) {
                    control_points[idx].flags = fl as u32;
                }
                if cp.get("locktopointer").and_then(value_bool) == Some(true) {
                    control_points[idx].flags |= 1;
                }
            }
        }
        if let Some(cps) = obj
            .get("controlpoints")
            .or_else(|| obj.get("controlpoint"))
            .and_then(|c| c.as_array())
        {
            for (fallback_idx, cp) in cps.iter().enumerate() {
                let idx = cp
                    .get("id")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(fallback_idx as i64)
                    .clamp(0, 7) as usize;
                if let Some(off) = cp.get("offset") {
                    let o = value_vec(off, 3, [0.0, 0.0, 0.0]);
                    control_points[idx].offset = [o[0], o[1]];
                }
                if let Some(fl) = cp.get("flags").and_then(|v| v.as_u64()) {
                    control_points[idx].flags = fl as u32;
                }
                if cp.get("locktopointer").and_then(value_bool) == Some(true) {
                    control_points[idx].flags |= 1;
                }
            }
        }

        let mut emitter_shape = EmitterShape::Sphere;
        let mut emitter_origin = [0.0f32, 0.0];
        let mut emitter_cp = 0usize;
        let mut dist_min = [0.0f32, 0.0];
        let mut dist_max = [256.0f32, 256.0];
        let mut emitter_directions = [1.0f32, 1.0];
        let mut emitter_sign = [0i8, 0i8];
        let mut radial_speed_min = 0.0f32;
        let mut radial_speed_max = 0.0f32;
        let mut rate = 15.0f32;
        let mut instantaneous = 0usize;
        let mut emitter_audio = None;
        if let Some(em) = preset
            .get("emitter")
            .and_then(|e| e.as_array())
            .and_then(|a| a.first())
        {
            if let Some(en) = em.get("name").and_then(|v| v.as_str()) {
                if en.eq_ignore_ascii_case("boxrandom") {
                    emitter_shape = EmitterShape::Box;
                } else if en.eq_ignore_ascii_case("layerimage") {
                    emitter_shape = EmitterShape::LayerImage;
                }
            }
            if let Some(orig) = em.get("origin") {
                let v = value_vec(orig, 3, [0.0, 0.0, 0.0]);
                emitter_origin = [v[0], v[1]];
            }
            if let Some(cp) = em.get("controlpoint").and_then(|v| v.as_i64()) {
                emitter_cp = cp.clamp(0, 7) as usize;
            }
            if let Some(dmin) = em.get("distancemin") {
                if let Some(s) = value_f32(dmin) {
                    dist_min = [s.abs(), s.abs()];
                } else {
                    let v = value_vec(dmin, 3, [0.0, 0.0, 0.0]);
                    dist_min = [v[0].abs(), v[1].abs()];
                }
            }
            if let Some(dmax) = em.get("distancemax") {
                if let Some(s) = value_f32(dmax) {
                    dist_max = [s.abs(), s.abs()];
                } else {
                    let v = value_vec(dmax, 3, [256.0, 256.0, 0.0]);
                    dist_max = [v[0].abs(), v[1].abs()];
                }
            }
            if let Some(dirs) = em.get("directions") {
                let v = value_vec(dirs, 3, [1.0, 1.0, 1.0]);
                emitter_directions = [v[0], v[1]];
            }
            if let Some(sig) = em.get("sign") {
                let v = value_vec(sig, 3, [0.0, 0.0, 0.0]);
                emitter_sign = [v[0].clamp(-1.0, 1.0) as i8, v[1].clamp(-1.0, 1.0) as i8];
            }
            radial_speed_min = em.get("speedmin").and_then(value_f32).unwrap_or(0.0);
            radial_speed_max = em
                .get("speedmax")
                .and_then(value_f32)
                .unwrap_or(radial_speed_min);
            if let Some(r) = em.get("rate").and_then(value_f32) {
                rate = r.max(0.0);
            }
            if let Some(inst) = em
                .get("instantaneous")
                .and_then(|v| v.as_u64().or_else(|| value_f32(v).map(|f| f.max(0.0) as u64)))
            {
                instantaneous = inst.min(500) as usize;
            }
            emitter_audio = AudioMod::parse(em);
        }
        let spawn_rate = rate.max(0.0);

        let mut life_min = 4.0f32;
        let mut life_max = 6.0f32;
        let mut size_min = 24.0f32;
        let mut size_max = 36.0f32;
        let mut vel_min = [0.0f32, 0.0];
        let mut vel_max = [0.0f32, 0.0];
        let mut color_min = [1.0f32, 1.0, 1.0];
        let mut color_max = [1.0f32, 1.0, 1.0];
        let mut hsv_color: Option<HsvRange> = None;
        let mut alpha_min = 0.6f32;
        let mut alpha_max = 1.0f32;
        let mut rot_min = [0.0f32; 3];
        let mut rot_max = [0.0f32; 3];
        let mut ang_vel_min = 0.0f32;
        let mut ang_vel_max = 0.0f32;
        let mut sequence_init: Option<SequenceInit> = None;

        if let Some(inits) = preset.get("initializer").and_then(|i| i.as_array()) {
            for init in inits {
                let iname = init
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                match iname.as_str() {
                    "lifetimerandom" => {
                        life_min = init.get("min").and_then(value_f32).unwrap_or(life_min);
                        life_max = init.get("max").and_then(value_f32).unwrap_or(life_max);
                    }
                    "sizerandom" => {
                        size_min = init.get("min").and_then(value_f32).unwrap_or(size_min);
                        size_max = init.get("max").and_then(value_f32).unwrap_or(size_max);
                    }
                    "velocityrandom" => {
                        if let Some(vmin) = init.get("min") {
                            let v = value_vec(vmin, 3, [0.0, 0.0, 0.0]);
                            vel_min = [v[0], v[1]];
                        }
                        if let Some(vmax) = init.get("max") {
                            let v = value_vec(vmax, 3, [0.0, 0.0, 0.0]);
                            vel_max = [v[0], v[1]];
                        }
                    }
                    "turbulentvelocityrandom" => {
                        let smin = init.get("speedmin").and_then(value_f32).unwrap_or(100.0);
                        let smax = init.get("speedmax").and_then(value_f32).unwrap_or(250.0);
                        let fwd = init
                            .get("forward")
                            .map(|v| value_vec(v, 3, [0.0, -1.0, 0.0]))
                            .unwrap_or([0.0, -1.0, 0.0]);
                        let len = (fwd[0] * fwd[0] + fwd[1] * fwd[1]).sqrt().max(1e-4);
                        let fx = fwd[0] / len;
                        let fy = fwd[1] / len;
                        let px = -fy;
                        let py = fx;
                        let avg = (smin + smax) * 0.5;
                        let spread = (smax - smin).abs() * 0.5 + avg * 0.35;
                        vel_min = [
                            fx * avg - spread - px.abs() * spread,
                            fy * avg - spread - py.abs() * spread,
                        ];
                        vel_max = [
                            fx * avg + spread + px.abs() * spread,
                            fy * avg + spread + py.abs() * spread,
                        ];
                    }
                    "colorrandom" => {
                        if let Some(cmin) = init.get("min") {
                            color_min = parse_color(cmin, [1.0, 1.0, 1.0]);
                        }
                        if let Some(cmax) = init.get("max") {
                            color_max = parse_color(cmax, color_min);
                        }
                    }
                    "hsvcolorrandom" => {
                        let hue_min = init.get("huemin").and_then(value_f32).unwrap_or(0.0);
                        let hue_max = init.get("huemax").and_then(value_f32).unwrap_or(hue_min);
                        let sat_min = init.get("saturationmin").and_then(value_f32).unwrap_or(1.0);
                        let sat_max = init
                            .get("saturationmax")
                            .and_then(value_f32)
                            .unwrap_or(sat_min);
                        let val_min = init.get("valuemin").and_then(value_f32).unwrap_or(1.0);
                        let val_max = init.get("valuemax").and_then(value_f32).unwrap_or(val_min);
                        hsv_color = Some(HsvRange {
                            hue_min,
                            hue_max,
                            sat_min,
                            sat_max,
                            val_min,
                            val_max,
                        });
                    }
                    "alpharandom" => {
                        alpha_min = init.get("min").and_then(value_f32).unwrap_or(alpha_min);
                        alpha_max = init.get("max").and_then(value_f32).unwrap_or(alpha_max);
                    }
                    "rotationrandom" => {
                        let has_min = init.get("min").is_some();
                        let has_max = init.get("max").is_some();
                        if let Some(rmin) = init.get("min") {
                            rot_min = value_vec(rmin, 3, [0.0, 0.0, 0.0]);
                        } else if has_max {
                            rot_min = [0.0, 0.0, 0.0];
                        }
                        if let Some(rmax) = init.get("max") {
                            rot_max = value_vec(rmax, 3, [0.0, 0.0, std::f32::consts::TAU]);
                        } else if !has_min {
                            rot_max = [0.0, 0.0, std::f32::consts::TAU];
                        } else {
                            rot_max = rot_min;
                        }
                    }
                    "angularvelocityrandom" => {
                        if let Some(wmin) = init.get("min") {
                            ang_vel_min = value_vec(wmin, 3, [0.0, 0.0, -1.0])[2];
                        }
                        if let Some(wmax) = init.get("max") {
                            ang_vel_max = value_vec(wmax, 3, [0.0, 0.0, 1.0])[2];
                        }
                    }
                    "mapsequencearoundcontrolpoint" => {
                        let cp_idx = init
                            .get("controlpoint")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0)
                            .clamp(0, 7) as usize;
                        let count = init.get("count").and_then(value_f32).unwrap_or(8.0).max(1.0);
                        let smin = init
                            .get("speedmin")
                            .map(|v| value_vec(v, 3, [0.0, 0.0, 0.0]))
                            .unwrap_or([0.0, 0.0, 0.0]);
                        let smax = init
                            .get("speedmax")
                            .map(|v| value_vec(v, 3, smin))
                            .unwrap_or(smin);
                        let bounds_v = init
                            .get("bounds")
                            .map(|v| value_vec(v, 3, [0.0, 1.0, 0.0]))
                            .unwrap_or([0.0, 1.0, 0.0]);
                        let bounds = [bounds_v[0], bounds_v[1]];
                        sequence_init = Some(SequenceInit::AroundCp {
                            cp_idx,
                            count,
                            speed_min: [smin[0], smin[1]],
                            speed_max: [smax[0], smax[1]],
                            bounds,
                        });
                    }
                    "mapsequencebetweencontrolpoints" => {
                        let start_cp = init
                            .get("startcontrolpoint")
                            .or_else(|| init.get("controlpoint"))
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0)
                            .clamp(0, 7) as usize;
                        let end_cp = init
                            .get("endcontrolpoint")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(1)
                            .clamp(0, 7) as usize;
                        let count = init.get("count").and_then(value_f32).unwrap_or(10.0).max(1.0);
                        let mirror = init
                            .get("limitbehavior")
                            .and_then(|v| v.as_str())
                            .is_some_and(|s| s.eq_ignore_ascii_case("mirror"));
                        sequence_init = Some(SequenceInit::BetweenCps {
                            start_cp,
                            end_cp,
                            count,
                            mirror,
                        });
                    }
                    _ => {}
                }
            }
        }

        life_min = (life_min * ov_life).max(0.1);
        life_max = (life_max * ov_life).max(life_min);
        size_min = (size_min * ov_size).max(1.0);
        size_max = (size_max * ov_size).max(size_min);
        vel_min[0] *= ov_speed;
        vel_min[1] *= ov_speed;
        vel_max[0] *= ov_speed;
        vel_max[1] *= ov_speed;
        radial_speed_min *= ov_speed;
        radial_speed_max *= ov_speed;
        alpha_min = (alpha_min * ov_alpha).clamp(0.0, 1.0);
        alpha_max = (alpha_max * ov_alpha).clamp(0.0, 1.0).max(alpha_min);

        let mut gravity = [0.0f32, 0.0];
        let mut drag = 0.0f32;
        let mut fade_in_time = 0.1f32;
        let mut fade_out_time = 0.8f32;
        let mut size_change: Option<ScalarChangeOp> = None;
        let mut alpha_change: Option<ScalarChangeOp> = None;
        let mut color_change: Option<ColorChangeOp> = None;
        let mut osc_pos: Option<OscillateOp> = None;
        let mut osc_alpha: Option<OscillateOp> = None;
        let mut osc_size: Option<OscillateOp> = None;
        let mut turbulence: Option<TurbulenceOp> = None;
        let mut angular_move: Option<AngularMoveOp> = None;
        let mut cp_attracts: Vec<CpAttractOp> = Vec::new();
        let mut vortices: Vec<VortexOp> = Vec::new();
        let mut collision_spheres: Vec<CollisionSphereOp> = Vec::new();

        let parse_osc = |op: &serde_json::Value,
                         def_fmin: f32,
                         def_fmax: f32,
                         def_smin: f32,
                         def_smax: f32| {
            let fmin = op.get("frequencymin").and_then(value_f32).unwrap_or(def_fmin);
            let fmax = op
                .get("frequencymax")
                .and_then(value_f32)
                .unwrap_or(fmin.max(def_fmax));
            let smin = op.get("scalemin").and_then(value_f32).unwrap_or(def_smin);
            let smax = op.get("scalemax").and_then(value_f32).unwrap_or(def_smax);
            let pmin = op.get("phasemin").and_then(value_f32).unwrap_or(0.0);
            let pmax = op
                .get("phasemax")
                .and_then(value_f32)
                .unwrap_or(std::f32::consts::TAU);
            let m = op
                .get("mask")
                .map(|v| value_vec(v, 3, [1.0, 1.0, 1.0]))
                .unwrap_or([1.0, 1.0, 1.0]);
            OscillateOp {
                freq_min: fmin,
                freq_max: fmax.max(fmin),
                scale_min: smin,
                scale_max: smax,
                phase_min: pmin,
                phase_max: pmax.max(pmin),
                mask: [m[0], m[1]],
            }
        };

        if let Some(ops) = preset.get("operator").and_then(|o| o.as_array()) {
            for op in ops {
                let oname = op
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                match oname.as_str() {
                    "movement" => {
                        if let Some(g) = op.get("gravity") {
                            let v = value_vec(g, 3, [0.0, 0.0, 0.0]);
                            gravity = [v[0] * ov_speed, v[1] * ov_speed];
                        }
                        drag = op.get("drag").and_then(value_f32).unwrap_or(0.0).max(0.0);
                    }
                    "alphafade" => {
                        fade_in_time = op
                            .get("fadeintime")
                            .and_then(value_f32)
                            .unwrap_or(0.1)
                            .clamp(0.0, 1.0);
                        fade_out_time = op
                            .get("fadeouttime")
                            .and_then(value_f32)
                            .unwrap_or(0.8)
                            .clamp(0.0, 1.0);
                    }
                    "sizechange" => {
                        size_change = Some(ScalarChangeOp {
                            start_time: op.get("starttime").and_then(value_f32).unwrap_or(0.0),
                            end_time: op.get("endtime").and_then(value_f32).unwrap_or(1.0),
                            start_value: op.get("startvalue").and_then(value_f32).unwrap_or(1.0),
                            end_value: op.get("endvalue").and_then(value_f32).unwrap_or(1.0),
                        });
                    }
                    "alphachange" => {
                        alpha_change = Some(ScalarChangeOp {
                            start_time: op.get("starttime").and_then(value_f32).unwrap_or(0.0),
                            end_time: op.get("endtime").and_then(value_f32).unwrap_or(1.0),
                            start_value: op.get("startvalue").and_then(value_f32).unwrap_or(1.0),
                            end_value: op.get("endvalue").and_then(value_f32).unwrap_or(0.0),
                        });
                    }
                    "colorchange" => {
                        let start_value = op
                            .get("startvalue")
                            .map(|v| parse_color(v, [1.0, 1.0, 1.0]))
                            .unwrap_or([1.0, 1.0, 1.0]);
                        let end_value = op
                            .get("endvalue")
                            .map(|v| parse_color(v, start_value))
                            .unwrap_or(start_value);
                        color_change = Some(ColorChangeOp {
                            start_time: op.get("starttime").and_then(value_f32).unwrap_or(0.0),
                            end_time: op.get("endtime").and_then(value_f32).unwrap_or(1.0),
                            start_value,
                            end_value,
                        });
                    }
                    "oscillateposition" => {
                        let mut parsed = parse_osc(op, 1.0, 1.5, 5.0, 8.0);
                        parsed.scale_min *= ov_speed;
                        parsed.scale_max *= ov_speed;
                        osc_pos = Some(parsed);
                    }
                    "oscillatealpha" => {
                        osc_alpha = Some(parse_osc(op, 1.0, 2.0, 0.5, 1.0));
                    }
                    "oscillatesize" => {
                        osc_size = Some(parse_osc(op, 1.0, 2.0, 0.8, 1.2));
                    }
                    "turbulence" => {
                        let smin = op.get("speedmin").and_then(value_f32).unwrap_or(20.0) * ov_speed;
                        let smax = op.get("speedmax").and_then(value_f32).unwrap_or(50.0) * ov_speed;
                        let m = op
                            .get("mask")
                            .map(|v| value_vec(v, 3, [1.0, 1.0, 1.0]))
                            .unwrap_or([1.0, 1.0, 1.0]);
                        turbulence = Some(TurbulenceOp {
                            speed_min: smin,
                            speed_max: smax.max(smin),
                            scale: op.get("scale").and_then(value_f32).unwrap_or(0.01),
                            timescale: op.get("timescale").and_then(value_f32).unwrap_or(1.0),
                            mask: [m[0], m[1]],
                            phase_min: op.get("phasemin").and_then(value_f32).unwrap_or(0.0),
                            phase_max: op
                                .get("phasemax")
                                .and_then(value_f32)
                                .unwrap_or(std::f32::consts::TAU),
                            audio: AudioMod::parse(op),
                        });
                    }
                    "angularmovement" => {
                        let force_z = op
                            .get("force")
                            .map(|v| value_vec(v, 3, [0.0, 0.0, 0.0])[2])
                            .unwrap_or(0.0);
                        let adrag = op.get("drag").and_then(value_f32).unwrap_or(0.0).max(0.0);
                        angular_move = Some(AngularMoveOp {
                            force_z,
                            drag: adrag,
                        });
                    }
                    "controlpointattract" => {
                        let cp_idx = op
                            .get("controlpoint")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0)
                            .clamp(0, 7) as usize;
                        let origin = op
                            .get("origin")
                            .map(|v| {
                                let o = value_vec(v, 3, [0.0, 0.0, 0.0]);
                                [o[0], o[1]]
                            })
                            .unwrap_or([0.0, 0.0]);
                        let scale = op.get("scale").and_then(value_f32).unwrap_or(512.0) * ov_speed;
                        let threshold = op.get("threshold").and_then(value_f32).unwrap_or(256.0).max(0.0);
                        cp_attracts.push(CpAttractOp {
                            cp_idx,
                            origin,
                            scale,
                            threshold,
                            audio: AudioMod::parse(op),
                        });
                    }
                    "vortex" => {
                        let cp_idx = op
                            .get("controlpoint")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0)
                            .clamp(0, 7) as usize;
                        let offset = op
                            .get("offset")
                            .map(|v| {
                                let o = value_vec(v, 3, [0.0, 0.0, 0.0]);
                                [o[0], o[1]]
                            })
                            .unwrap_or([0.0, 0.0]);
                        let axis_sign = op
                            .get("axis")
                            .map(|v| if value_vec(v, 3, [0.0, 0.0, 1.0])[2] < 0.0 { -1.0 } else { 1.0 })
                            .unwrap_or(1.0);
                        let dist_inner = op
                            .get("distanceinner")
                            .and_then(value_f32)
                            .unwrap_or(0.0)
                            .max(0.0);
                        let dist_outer = op
                            .get("distanceouter")
                            .and_then(value_f32)
                            .unwrap_or(500.0)
                            .max(dist_inner);
                        let has_si = op.get("speedinner").is_some();
                        let has_so = op.get("speedouter").is_some();
                        let speed_inner = op.get("speedinner").and_then(value_f32).unwrap_or(0.0) * ov_speed;
                        let speed_outer = op
                            .get("speedouter")
                            .and_then(value_f32)
                            .unwrap_or(if !has_si && !has_so { 200.0 } else { 0.0 })
                            * ov_speed;
                        let ring_radius = op.get("ringradius").and_then(value_f32).unwrap_or(0.0).max(0.0);
                        let ring_width = op.get("ringwidth").and_then(value_f32).unwrap_or(0.0).max(0.0);
                        vortices.push(VortexOp {
                            cp_idx,
                            offset,
                            axis_sign,
                            dist_inner,
                            dist_outer,
                            speed_inner,
                            speed_outer,
                            ring_radius,
                            ring_width,
                            audio: AudioMod::parse(op),
                        });
                    }
                    "collisionsphere" => {
                        let cp_idx = op
                            .get("controlpoint")
                            .and_then(|v| v.as_i64())
                            .unwrap_or(0)
                            .clamp(0, 7) as usize;
                        let origin = op
                            .get("origin")
                            .map(|v| {
                                let o = value_vec(v, 3, [0.0, 0.0, 0.0]);
                                [o[0], o[1]]
                            })
                            .unwrap_or([0.0, 0.0]);
                        let radius = op.get("radius").and_then(value_f32).unwrap_or(100.0).max(1.0);
                        let behavior = match op
                            .get("collisionbehavior")
                            .and_then(|v| v.as_str())
                            .unwrap_or("bounce")
                            .to_ascii_lowercase()
                            .as_str()
                        {
                            "slide" => CollisionBehavior::Slide,
                            "stop" => CollisionBehavior::Stop,
                            _ => CollisionBehavior::Bounce,
                        };
                        collision_spheres.push(CollisionSphereOp {
                            cp_idx,
                            origin,
                            radius,
                            behavior,
                        });
                    }
                    _ => {}
                }
            }
        }

        let mut renderer = ParticleRendererKind::Sprite;
        if let Some(rends) = preset.get("renderer").and_then(|r| r.as_array()) {
            for r in rends {
                match r
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_ascii_lowercase()
                    .as_str()
                {
                    "spritetrail" => {
                        let len = r.get("length").and_then(value_f32).unwrap_or(0.05);
                        let maxlen = r.get("maxlength").and_then(value_f32).unwrap_or(100.0);
                        renderer = ParticleRendererKind::SpriteTrail {
                            length: len,
                            max_length: maxlen,
                        };
                    }
                    "ropetrail" => {
                        let len = r.get("length").and_then(value_f32).unwrap_or(0.2).max(0.02);
                        renderer = ParticleRendererKind::RopeTrail { length: len };
                    }
                    "rope" => {
                        renderer = ParticleRendererKind::Rope;
                    }
                    _ => {}
                }
            }
        }

        let random_frame = preset
            .get("animationmode")
            .and_then(|v| v.as_str())
            .map(|s| s.eq_ignore_ascii_case("randomframe"))
            .unwrap_or(false);

        let raw_max = preset.get("maxcount").and_then(value_f32).unwrap_or(60.0);
        let max_count = count_cap_override
            .unwrap_or_else(|| (raw_max.round() as usize).clamp(1, 500))
            .clamp(1, 500);
        let avg_life = (life_min + life_max) * 0.5;
        let scaled_max = ((max_count as f32) * ov_count.max(0.0))
            .round()
            .clamp(0.0, 500.0) as usize;
        let scaled_rate = (spawn_rate * ov_rate.max(0.0)).max(0.0);
        let steady_count = if scaled_max == 0 {
            0
        } else if instantaneous > 0 {
            instantaneous.clamp(1, scaled_max)
        } else if scaled_rate <= 0.0 {
            0
        } else {
            ((scaled_rate * avg_life).ceil() as usize).clamp(1, scaled_max)
        };
        let layer_image_points = layer_mask.map(|m| m.points).unwrap_or_default();

        let spec = Self {
            transform,
            emitter_shape,
            emitter_origin,
            emitter_cp,
            dist_min,
            dist_max,
            emitter_directions,
            emitter_sign,
            radial_speed_min,
            radial_speed_max,
            emitter_audio,
            layer_image_points,
            control_points,
            spawn_rate,
            instantaneous,
            max_count,
            spawn_accum: 0.0,
            life_min,
            life_max,
            size_min,
            size_max,
            vel_min,
            vel_max,
            color_min,
            color_max,
            color_mult,
            hsv_color,
            alpha_min,
            alpha_max,
            rot_min,
            rot_max,
            ang_vel_min,
            ang_vel_max,
            sequence_init,
            seq_counter: 0,
            gravity,
            drag,
            fade_in_time,
            fade_out_time,
            size_change,
            alpha_change,
            color_change,
            osc_pos,
            osc_alpha,
            osc_size,
            turbulence,
            angular_move,
            cp_attracts,
            vortices,
            collision_spheres,
            renderer,
            random_frame,
            rng_state: 0x9E37_79B9u32
                ^ (steady_count as u32)
                ^ transform.tx.to_bits()
                ^ transform.ty.to_bits(),
        };

        (spec, steady_count, ov_rate, ov_count, ov_alpha, ov_size, ov_speed)
    }

    fn spawn(
        &mut self,
        initial_age_frac: Option<f32>,
        cps: &[[f32; 2]; 8],
        anchor_override: Option<[f32; 2]>,
    ) -> Particle {
        let mut x;
        let mut y;
        let mut vx = self.vel_min[0] + self.next_f32() * (self.vel_max[0] - self.vel_min[0]);
        let mut vy = self.vel_min[1] + self.next_f32() * (self.vel_max[1] - self.vel_min[1]);

        if let Some(anchor) = anchor_override {
            let jitter_x = (self.next_f32() * 2.0 - 1.0) * (self.dist_max[0] * 0.15).min(24.0);
            let jitter_y = (self.next_f32() * 2.0 - 1.0) * (self.dist_max[1] * 0.15).min(24.0);
            x = anchor[0] + jitter_x;
            y = anchor[1] + jitter_y;
        } else if let Some(seq) = self.sequence_init {
            let idx = self.seq_counter;
            self.seq_counter = self.seq_counter.wrapping_add(1);
            match seq {
                SequenceInit::AroundCp {
                    cp_idx,
                    count,
                    speed_min,
                    speed_max,
                    bounds,
                } => {
                    let frac = ((idx as f32 % count) / count).clamp(0.0, 1.0);
                    let u = bounds[0] + frac * (bounds[1] - bounds[0]);
                    let theta = u * std::f32::consts::TAU;
                    let (st, ct) = theta.sin_cos();
                    let r_u = self.next_f32();
                    let rx = self.dist_min[0] + r_u * (self.dist_max[0] - self.dist_min[0]).max(0.0);
                    let ry = self.dist_min[1] + r_u * (self.dist_max[1] - self.dist_min[1]).max(0.0);
                    let cp = cps[cp_idx.min(7)];
                    x = self.emitter_origin[0] + cp[0] + ct * rx * self.emitter_directions[0];
                    y = self.emitter_origin[1] + cp[1] + st * ry * self.emitter_directions[1];
                    let tang = speed_min[1] + self.next_f32() * (speed_max[1] - speed_min[1]);
                    let rad = speed_min[0] + self.next_f32() * (speed_max[0] - speed_min[0]);
                    vx += ct * rad - st * tang;
                    vy += st * rad + ct * tang;
                }
                SequenceInit::BetweenCps {
                    start_cp,
                    end_cp,
                    count,
                    mirror,
                } => {
                    let p0 = cps[start_cp.min(7)];
                    let p1 = cps[end_cp.min(7)];
                    let t = if mirror {
                        let period = (count * 2.0).max(2.0);
                        let ph = (idx as f32 % period) / count.max(1.0);
                        if ph <= 1.0 { ph } else { (2.0 - ph).clamp(0.0, 1.0) }
                    } else if count > 1.0 {
                        (idx as f32 % count) / (count - 1.0)
                    } else {
                        0.0
                    };
                    x = self.emitter_origin[0] + p0[0] + t * (p1[0] - p0[0]);
                    y = self.emitter_origin[1] + p0[1] + t * (p1[1] - p0[1]);
                }
            }
        } else {
            let cp = cps[self.emitter_cp.min(7)];
            let (ox, oy, dx_dir, dy_dir) = match self.emitter_shape {
                EmitterShape::Sphere => {
                    let theta = self.next_f32() * std::f32::consts::TAU;
                    let (mut st, mut ct) = theta.sin_cos();
                    if self.emitter_sign[0] > 0 {
                        ct = ct.abs();
                    } else if self.emitter_sign[0] < 0 {
                        ct = -ct.abs();
                    }
                    if self.emitter_sign[1] > 0 {
                        st = st.abs();
                    } else if self.emitter_sign[1] < 0 {
                        st = -st.abs();
                    }
                    let u = self.next_f32();
                    let sample_r = |r0: f32, r1: f32| {
                        let r_max = r1.max(r0);
                        let sq = r0 * r0 + u * (r_max * r_max - r0 * r0);
                        sq.max(0.0).sqrt()
                    };
                    let rx = sample_r(self.dist_min[0], self.dist_max[0]);
                    let ry = sample_r(self.dist_min[1], self.dist_max[1]);
                    (
                        ct * rx * self.emitter_directions[0],
                        st * ry * self.emitter_directions[1],
                        ct,
                        st,
                    )
                }
                EmitterShape::Box => self.sample_box_offset(),
                EmitterShape::LayerImage => {
                    if !self.layer_image_points.is_empty() {
                        let len = self.layer_image_points.len();
                        let idx = (self.next_f32() * len as f32) as usize % len;
                        let pt = self.layer_image_points[idx];
                        let p_len = pt[0].hypot(pt[1]).max(1e-4);
                        (pt[0], pt[1], pt[0] / p_len, pt[1] / p_len)
                    } else {
                        self.sample_box_offset()
                    }
                }
            };
            x = self.emitter_origin[0] + cp[0] + ox;
            y = self.emitter_origin[1] + cp[1] + oy;
            if self.radial_speed_max != 0.0 || self.radial_speed_min != 0.0 {
                let spd = self.radial_speed_min
                    + self.next_f32() * (self.radial_speed_max - self.radial_speed_min);
                vx += dx_dir * spd;
                vy += dy_dir * spd;
            }
        }

        let size = self.size_min + self.next_f32() * (self.size_max - self.size_min);
        let alpha = self.alpha_min + self.next_f32() * (self.alpha_max - self.alpha_min);
        let rot_x = self.rot_min[0] + self.next_f32() * (self.rot_max[0] - self.rot_min[0]);
        let rot_y = self.rot_min[1] + self.next_f32() * (self.rot_max[1] - self.rot_min[1]);
        let rot = self.rot_min[2] + self.next_f32() * (self.rot_max[2] - self.rot_min[2]);
        let ang_vel = self.ang_vel_min + self.next_f32() * (self.ang_vel_max - self.ang_vel_min);
        let lifetime = (self.life_min + self.next_f32() * (self.life_max - self.life_min)).max(0.1);
        let tc = self.next_f32();
        let base_color = if let Some(hsv) = self.hsv_color {
            let h = hsv.hue_min + self.next_f32() * (hsv.hue_max - hsv.hue_min);
            let s = hsv.sat_min + self.next_f32() * (hsv.sat_max - hsv.sat_min);
            let v = hsv.val_min + self.next_f32() * (hsv.val_max - hsv.val_min);
            hsv_to_rgb(h, s, v)
        } else {
            [
                self.color_min[0] + tc * (self.color_max[0] - self.color_min[0]),
                self.color_min[1] + tc * (self.color_max[1] - self.color_min[1]),
                self.color_min[2] + tc * (self.color_max[2] - self.color_min[2]),
            ]
        };
        let color = [
            base_color[0] * self.color_mult[0],
            base_color[1] * self.color_mult[1],
            base_color[2] * self.color_mult[2],
        ];
        let phase = self.next_f32() * std::f32::consts::TAU;
        let osc_seed = self.next_f32();
        let frame_seed = (self.next_f32() * 1024.0) as u32;
        let age = initial_age_frac.unwrap_or(0.0) * lifetime;
        x += vx * age * 0.35;
        y += vy * age * 0.35;
        Particle {
            x,
            y,
            vx,
            vy,
            size,
            alpha,
            phase,
            osc_seed,
            rot: rot + ang_vel * age,
            rot_x,
            rot_y,
            ang_vel,
            age,
            lifetime,
            frame_seed,
            base_color,
            color,
            trail: [[x, y]; TRAIL_CAP],
            trail_len: 1,
            trail_timer: 0.0,
        }
    }
}

pub(super) struct Emitter {
    pub(super) kind: ParticleKind,
    pub(super) center: [f32; 2],
    pub(super) spread: [f32; 2],
    pub(super) blend: LayerBlend,
    pub(super) audio_reactive: bool,
    pub(super) texture: Option<glow::Texture>,
    pub(super) tex_is_r8: bool,
    pub(super) refract: bool,
    pub(super) normal_tex: Option<glow::Texture>,
    pub(super) refract_amount: f32,
    pub(super) overbright: f32,
    spritesheet: Option<crate::pkg::SpritesheetMeta>,
    pub(super) we_spec: Option<WeParticleSpec>,
    pub(super) particles: Vec<Particle>,
    pub(super) child_indices: Vec<(usize, ChildSpawnMode)>,
    pub(super) child_local: Option<Affine>,
    child_probability: f32,
    base_ov_rate: f32,
    base_ov_count: f32,
    base_ov_alpha: f32,
    base_ov_size: f32,
    base_ov_speed: f32,
    live_rate_mult: f32,
    live_count_mult: f32,
    live_alpha_mult: f32,
    live_size_mult: f32,
    live_speed_mult: f32,
    live_brightness: f32,
}

impl Emitter {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn from_we_preset(
        gl: &glow::Context,
        pkg: &PackageFs,
        textures: &mut TextureCache,
        preset: &serde_json::Value,
        obj: &serde_json::Value,
        transform: Affine,
        hint_kind: ParticleKind,
        props: &HashMap<String, serde_json::Value>,
        child_local: Option<Affine>,
        child_maxcount: Option<usize>,
        child_probability: f32,
        layer_mask: Option<LayerImageMask>,
    ) -> Self {
        let (mut spec, steady_count, ov_rate, ov_count, ov_alpha, ov_size, ov_speed) =
            WeParticleSpec::parse(preset, obj, transform, props, child_maxcount, layer_mask);

        let mut texture = None;
        let mut tex_is_r8 = false;
        let mut refract = false;
        let mut normal_tex = None;
        let mut refract_amount = 0.05f32;
        let mut overbright = 1.0f32;
        let mut spritesheet = None;
        let mut blend = match hint_kind {
            ParticleKind::Embers | ParticleKind::Fireflies | ParticleKind::Stars => {
                LayerBlend::Additive
            }
            _ => LayerBlend::Normal,
        };
        if let Some(mat_path) = preset.get("material").and_then(|v| v.as_str()) {
            if let Some(mat_txt) = read_scene_text(pkg, mat_path) {
                if let Ok(mat_json) = serde_json::from_str::<serde_json::Value>(&mat_txt) {
                    if let Some(pass) = mat_json
                        .get("passes")
                        .and_then(|p| p.as_array())
                        .and_then(|a| a.first())
                    {
                        if let Some(b) = pass.get("blending").and_then(|v| v.as_str()) {
                            blend = LayerBlend::from_material(b);
                        }
                        if let Some(v) = pass
                            .get("combos")
                            .and_then(|c| c.get("REFRACT"))
                            .and_then(value_f32)
                        {
                            refract = v != 0.0;
                        }
                        if let Some(csv) = pass.get("constantshadervalues") {
                            if let Some(ob) = csv
                                .get("ui_editor_properties_overbright")
                                .or_else(|| csv.get("overbright"))
                                .and_then(value_f32)
                            {
                                overbright = ob.max(0.0);
                            }
                            if let Some(ra) = csv
                                .get("ui_editor_properties_refract_amount")
                                .or_else(|| csv.get("refractamount"))
                                .and_then(value_f32)
                            {
                                refract_amount = ra;
                            }
                        }
                        if let Some(texs) = pass.get("textures").and_then(|t| t.as_array()) {
                            if let Some(tex_name) = texs.first().and_then(|v| v.as_str()) {
                                if let Some((tex, _w, _h, ss, is_r8)) =
                                    Self::load_particle_texture(gl, pkg, textures, tex_name)
                                {
                                    texture = Some(tex);
                                    spritesheet = ss;
                                    tex_is_r8 = is_r8;
                                }
                            }
                            if refract {
                                if let Some(norm_name) = texs.get(1).and_then(|v| v.as_str()) {
                                    if let Some((ntex, ..)) =
                                        Self::load_particle_texture(gl, pkg, textures, norm_name)
                                    {
                                        normal_tex = Some(ntex);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        // Dark-tinted particles (e.g. `colorn: "0 0 0"` on the Death half of
        // `Life / Death`) must alpha-blend rather than add zero RGB.
        if spec.color_mult[0] + spec.color_mult[1] + spec.color_mult[2] < 0.25
            && blend == LayerBlend::Additive
        {
            blend = LayerBlend::Normal;
        }

        if hint_kind == ParticleKind::Fog {
            spec.max_count = spec.max_count.min(32);
        }
        let count = if hint_kind == ParticleKind::Fog {
            steady_count.min(32)
        } else {
            steady_count
        };
        let cps = spec.resolve_cps([transform.tx, transform.ty]);
        let mut particles = Vec::with_capacity(count);
        if spec.instantaneous > 0 {
            for _ in 0..count {
                particles.push(spec.spawn(Some(0.0), &cps, None));
            }
        } else {
            for i in 0..count {
                let frac = (i as f32 + 0.5) / (count.max(1) as f32);
                particles.push(spec.spawn(Some(frac), &cps, None));
            }
        }
        let audio_reactive = spec.uses_audio();
        let spread = spec.dist_max;

        Self {
            kind: hint_kind,
            center: [transform.tx, transform.ty],
            spread,
            blend,
            audio_reactive,
            texture,
            tex_is_r8,
            refract,
            normal_tex,
            refract_amount,
            overbright,
            spritesheet,
            we_spec: Some(spec),
            particles,
            child_indices: Vec::new(),
            child_local,
            child_probability: child_probability.clamp(0.0, 1.0),
            base_ov_rate: ov_rate,
            base_ov_count: ov_count,
            base_ov_alpha: ov_alpha,
            base_ov_size: ov_size,
            base_ov_speed: ov_speed,
            live_rate_mult: ov_rate.max(0.0),
            live_count_mult: ov_count.max(0.0),
            live_alpha_mult: 1.0,
            live_size_mult: 1.0,
            live_speed_mult: 1.0,
            live_brightness: 1.0,
        }
    }

    fn load_particle_texture(
        gl: &glow::Context,
        pkg: &PackageFs,
        textures: &mut TextureCache,
        path: &str,
    ) -> Option<(glow::Texture, u32, u32, Option<crate::pkg::SpritesheetMeta>, bool)> {
        let raw = path.trim_start_matches('/');
        let no_ext = raw
            .strip_suffix(".tex")
            .or_else(|| raw.strip_suffix(".png"))
            .unwrap_or(raw);
        let candidates = [
            raw.to_string(),
            format!("materials/{no_ext}.tex"),
            format!("{no_ext}.tex"),
        ];
        for cand in &candidates {
            if let Some(found) = textures.get_spritesheet(gl, pkg, cand) {
                return Some(found);
            }
        }
        None
    }

    pub(super) fn new(
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
                osc_seed: rng(),
                rot,
                rot_x: 0.0,
                rot_y: 0.0,
                ang_vel: 0.0,
                age: 0.0,
                lifetime: 1.0,
                frame_seed: 0,
                base_color: color.unwrap_or(default_col),
                color: color.unwrap_or(default_col),
                trail: [[x, y]; TRAIL_CAP],
                trail_len: 1,
                trail_timer: 0.0,
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
            texture: None,
            tex_is_r8: false,
            refract: false,
            normal_tex: None,
            refract_amount: 0.05,
            overbright: 1.0,
            spritesheet: None,
            we_spec: None,
            particles,
            child_indices: Vec::new(),
            child_local: None,
            child_probability: 1.0,
            base_ov_rate: 1.0,
            base_ov_count: 1.0,
            base_ov_alpha: 1.0,
            base_ov_size: 1.0,
            base_ov_speed: 1.0,
            live_rate_mult: 1.0,
            live_count_mult: 1.0,
            live_alpha_mult: 1.0,
            live_size_mult: 1.0,
            live_speed_mult: 1.0,
            live_brightness: 1.0,
        }
    }

    pub(super) fn reset_script_overrides(&mut self) {
        self.live_rate_mult = self.base_ov_rate.max(0.0);
        self.live_count_mult = self.base_ov_count.max(0.0);
        self.live_alpha_mult = 1.0;
        self.live_size_mult = 1.0;
        self.live_speed_mult = 1.0;
        self.live_brightness = 1.0;
    }

    pub(super) fn update_instance_override(&mut self, key: &str, value: &serde_json::Value) {
        match key {
            "color" | "colorn" => {
                let c = super::scenescript::prop_vec3(value, [1.0, 1.0, 1.0]);
                self.update_color_mult(c);
            }
            "rate" => {
                if let Some(v) = value_f32(value) {
                    self.live_rate_mult = v.clamp(0.0, 20.0);
                }
            }
            "count" => {
                if let Some(v) = value_f32(value) {
                    self.live_count_mult = v.clamp(0.0, 20.0);
                }
            }
            "alpha" => {
                if let Some(v) = value_f32(value) {
                    self.live_alpha_mult = (v / self.base_ov_alpha.max(1e-4)).clamp(0.0, 10.0);
                }
            }
            "size" => {
                if let Some(v) = value_f32(value) {
                    self.live_size_mult = (v / self.base_ov_size.max(1e-4)).clamp(0.0, 10.0);
                }
            }
            "speed" => {
                if let Some(v) = value_f32(value) {
                    self.live_speed_mult = (v / self.base_ov_speed.max(1e-4)).clamp(0.0, 10.0);
                }
            }
            "brightness" => {
                if let Some(v) = value_f32(value) {
                    self.live_brightness = v.clamp(0.0, 10.0);
                }
            }
            "lifetime" => {
                if let Some(spec) = &mut self.we_spec {
                    if let Some(v) = value_f32(value) {
                        let ratio = (v / spec.life_max.max(0.1)).clamp(0.05, 10.0);
                        for p in &mut self.particles {
                            p.lifetime = (spec.life_max * ratio).max(0.1);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    pub(super) fn update_color_mult(&mut self, color_mult: [f32; 3]) {
        if let Some(spec) = &mut self.we_spec {
            spec.color_mult = color_mult;
            for p in &mut self.particles {
                p.color = [
                    p.base_color[0] * color_mult[0],
                    p.base_color[1] * color_mult[1],
                    p.base_color[2] * color_mult[2],
                ];
            }
        } else {
            for p in &mut self.particles {
                p.color = color_mult;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn update_and_pack(
        &mut self,
        dt: f32,
        time: f32,
        ortho: [f32; 2],
        audio: [f32; 4],
        spectrum: &[f32; 64],
        offset: [f32; 2],
        pointer_world: [f32; 2],
        spawn_anchors: Option<(&[[f32; 2]], bool)>,
        out: &mut Vec<f32>,
    ) {
        out.clear();
        let boost = if self.audio_reactive {
            1.0 + audio[0] * 1.2 + audio[3] * 0.5
        } else {
            1.0
        };
        let dt_clamped = dt.clamp(0.0, 0.1);
        let dtc = dt_clamped * boost * self.live_speed_mult.clamp(0.0, 10.0);
        let alpha_mult = self.live_alpha_mult.clamp(0.0, 4.0);
        let size_mult_ov = self.live_size_mult.clamp(0.0, 10.0);
        let brightness_mult = self.live_brightness.clamp(0.0, 10.0);

        if let Some(spec) = &mut self.we_spec {
            let cps = spec.resolve_cps(pointer_world);
            let emitter_audio_mult = spec
                .emitter_audio
                .map(|a| 0.15 + 1.85 * a.eval(spectrum))
                .unwrap_or(1.0);

            // 1. Advance age and retire expired particles.
            let mut pi = 0usize;
            while pi < self.particles.len() {
                self.particles[pi].age += dtc;
                if self.particles[pi].age >= self.particles[pi].lifetime {
                    self.particles.swap_remove(pi);
                } else {
                    pi += 1;
                }
            }

            // 2. Spawn new particles continuously via rate accumulator (or top up instantaneous).
            let effective_max =
                ((spec.max_count as f32) * self.live_count_mult * self.child_probability)
                    .round()
                    .clamp(0.0, 2000.0) as usize;
            let effective_rate = (spec.spawn_rate
                * self.live_rate_mult
                * self.child_probability
                * emitter_audio_mult)
                .max(0.0);
            let missing_anchor = spawn_anchors.is_some_and(|(ancs, _)| ancs.is_empty());
            if missing_anchor {
                spec.spawn_accum = 0.0;
            } else if spec.instantaneous > 0 {
                if self.live_rate_mult > 0.0 {
                    let target = spec.instantaneous.min(effective_max);
                    let need = target.saturating_sub(self.particles.len()).min(64);
                    for k in 0..need {
                        let anchor = spawn_anchors.and_then(|(ancs, _)| {
                            (!ancs.is_empty()).then(|| ancs[(self.particles.len() + k) % ancs.len()])
                        });
                        self.particles.push(spec.spawn(Some(0.0), &cps, anchor));
                    }
                }
            } else if effective_rate <= 0.0 {
                spec.spawn_accum = 0.0;
            } else {
                spec.spawn_accum = (spec.spawn_accum + effective_rate * dt_clamped)
                    .min(effective_max as f32);
                let mut spawned = 0usize;
                while spec.spawn_accum >= 1.0
                    && self.particles.len() < effective_max
                    && spawned < 64
                {
                    spec.spawn_accum -= 1.0;
                    let anchor = spawn_anchors.and_then(|(ancs, _)| {
                        (!ancs.is_empty())
                            .then(|| ancs[(self.particles.len() + spawned) % ancs.len()])
                    });
                    self.particles.push(spec.spawn(Some(0.0), &cps, anchor));
                    spawned += 1;
                }
            }

            let t_scale_x = spec.transform.a.hypot(spec.transform.b);
            let t_scale_y = spec.transform.c.hypot(spec.transform.d);
            let t_scale = ((t_scale_x + t_scale_y) * 0.5).max(0.01);
            let t_rot = spec.transform.b.atan2(spec.transform.a);
            let max_bridge = ortho[0].hypot(ortho[1]).max(100.0) * 0.45;
            let rope_mode = spec.renderer == ParticleRendererKind::Rope;
            let mut rope_nodes: Vec<(f32, [f32; 2], f32, [f32; 3], f32, [f32; 4])> = if rope_mode
            {
                Vec::with_capacity(self.particles.len())
            } else {
                Vec::new()
            };

            // 3. Simulate operators and pack vertices for all live particles.
            for (idx_live, p) in self.particles.iter_mut().enumerate() {
                let damp = (1.0 - spec.drag * dtc).clamp(0.0, 1.0);
                p.vx = p.vx * damp + spec.gravity[0] * dtc;
                p.vy = p.vy * damp + spec.gravity[1] * dtc;

                if let Some(turb) = spec.turbulence {
                    let am = turb.audio.map(|a| a.eval(spectrum)).unwrap_or(1.0);
                    let spd = turb.speed_min + p.osc_seed * (turb.speed_max - turb.speed_min);
                    let s = turb.scale.max(1e-4);
                    let ph = turb.phase_min + p.osc_seed * (turb.phase_max - turb.phase_min);
                    let t = time * turb.timescale + ph + p.phase;
                    let fx = (p.y * s + t).sin() + 0.5 * (p.x * s * 1.7 - t * 1.3).cos();
                    let fy = (p.x * s - t * 0.9).cos() + 0.5 * (p.y * s * 1.7 + t * 1.1).sin();
                    let dv = (spd * am * dtc).clamp(-800.0, 800.0);
                    p.vx += fx * dv * turb.mask[0];
                    p.vy += fy * dv * turb.mask[1];
                }

                for op in &spec.cp_attracts {
                    let cpos = cps[op.cp_idx.min(7)];
                    let cx = cpos[0] + op.origin[0];
                    let cy = cpos[1] + op.origin[1];
                    let dx = cx - p.x;
                    let dy = cy - p.y;
                    let dist = dx.hypot(dy);
                    if dist > 1e-3 && (op.threshold <= 0.0 || dist < op.threshold) {
                        let falloff = if op.threshold > 0.0 {
                            (1.0 - dist / op.threshold).clamp(0.0, 1.0)
                        } else {
                            1.0
                        };
                        let am = op.audio.map(|a| a.eval(spectrum)).unwrap_or(1.0);
                        let dv = (op.scale * falloff * am * dtc).clamp(-800.0, 800.0);
                        p.vx += (dx / dist) * dv;
                        p.vy += (dy / dist) * dv;
                    }
                }

                for op in &spec.vortices {
                    let cpos = cps[op.cp_idx.min(7)];
                    let cx = cpos[0] + op.offset[0];
                    let cy = cpos[1] + op.offset[1];
                    let dx = p.x - cx;
                    let dy = p.y - cy;
                    let dist = dx.hypot(dy);
                    if dist > 1e-3 {
                        let t = if op.dist_outer > op.dist_inner {
                            ((dist - op.dist_inner) / (op.dist_outer - op.dist_inner))
                                .clamp(0.0, 1.0)
                        } else {
                            0.0
                        };
                        let mut spd = op.speed_inner + t * (op.speed_outer - op.speed_inner);
                        if op.ring_radius > 0.0 && op.ring_width > 0.0 {
                            let ring_f = (1.0 - ((dist - op.ring_radius).abs() / op.ring_width))
                                .clamp(0.0, 1.0);
                            spd *= ring_f;
                        }
                        let am = op.audio.map(|a| a.eval(spectrum)).unwrap_or(1.0);
                        let step = (spd * am * op.axis_sign * dtc).clamp(-400.0, 400.0);
                        p.x += (-dy / dist) * step;
                        p.y += (dx / dist) * step;
                    }
                }

                if let Some(am) = spec.angular_move {
                    let adamp = (1.0 - am.drag * dtc).clamp(0.0, 1.0);
                    p.ang_vel = p.ang_vel * adamp + am.force_z * dtc;
                }

                let (osc_x, osc_y) = if let Some(op) = spec.osc_pos {
                    let f = op.freq_min + p.osc_seed * (op.freq_max - op.freq_min);
                    let amp = op.scale_min + p.osc_seed * (op.scale_max - op.scale_min);
                    let ph = op.phase_min + p.osc_seed * (op.phase_max - op.phase_min) + p.phase;
                    (
                        (time * f + ph).sin() * amp * op.mask[0],
                        (time * f * 0.87 + ph).cos() * amp * op.mask[1],
                    )
                } else {
                    (0.0, 0.0)
                };

                p.x += (p.vx + osc_x) * dtc;
                p.y += (p.vy + osc_y) * dtc;
                p.rot += p.ang_vel * dtc;

                if let Some((ancs, follow)) = spawn_anchors {
                    if follow && !ancs.is_empty() {
                        let [ax, ay] = ancs[idx_live % ancs.len()];
                        p.x = ax + (p.x - ax) * 0.9;
                        p.y = ay + (p.y - ay) * 0.9;
                    }
                }

                for col in &spec.collision_spheres {
                    let cpos = cps[col.cp_idx.min(7)];
                    let cx = cpos[0] + col.origin[0];
                    let cy = cpos[1] + col.origin[1];
                    let dx = p.x - cx;
                    let dy = p.y - cy;
                    let dist = dx.hypot(dy);
                    if dist < col.radius {
                        let (nx, ny) = if dist > 1e-4 {
                            (dx / dist, dy / dist)
                        } else {
                            (1.0, 0.0)
                        };
                        p.x = cx + nx * col.radius;
                        p.y = cy + ny * col.radius;
                        let vn = p.vx * nx + p.vy * ny;
                        if vn < 0.0 {
                            match col.behavior {
                                CollisionBehavior::Bounce => {
                                    p.vx -= 1.8 * vn * nx;
                                    p.vy -= 1.8 * vn * ny;
                                }
                                CollisionBehavior::Slide => {
                                    p.vx -= vn * nx;
                                    p.vy -= vn * ny;
                                }
                                CollisionBehavior::Stop => {
                                    p.vx = 0.0;
                                    p.vy = 0.0;
                                }
                            }
                        }
                    }
                }

                if let ParticleRendererKind::RopeTrail { length } = spec.renderer {
                    let sample_dt = (length / (TRAIL_CAP.saturating_sub(1) as f32)).max(0.016);
                    p.trail_timer += dtc;
                    if p.trail_len <= 1 && (p.x - p.trail[0][0]).hypot(p.y - p.trail[0][1]) > 0.5 {
                        p.trail[1] = p.trail[0];
                        p.trail_len = 2;
                        p.trail[0] = [p.x, p.y];
                    } else if p.trail_timer >= sample_dt {
                        p.trail_timer %= sample_dt;
                        let keep = (p.trail_len as usize).min(TRAIL_CAP - 1);
                        p.trail.copy_within(0..keep, 1);
                        p.trail_len = ((p.trail_len as usize + 1).min(TRAIL_CAP)) as u8;
                        p.trail[0] = [p.x, p.y];
                    } else {
                        p.trail[0] = [p.x, p.y];
                    }
                }

                let t_norm = (p.age / p.lifetime).clamp(0.0, 1.0);
                let mut alpha = p.alpha * alpha_mult;
                if spec.fade_in_time > 0.0 && t_norm < spec.fade_in_time {
                    alpha *= t_norm / spec.fade_in_time;
                }
                if spec.fade_out_time < 1.0 && t_norm > spec.fade_out_time {
                    alpha *= (1.0 - t_norm) / (1.0 - spec.fade_out_time).max(1e-4);
                }
                if let Some(ac) = spec.alpha_change {
                    let u = ((t_norm - ac.start_time) / (ac.end_time - ac.start_time).max(1e-4))
                        .clamp(0.0, 1.0);
                    alpha *= ac.start_value + u * (ac.end_value - ac.start_value);
                }
                if let Some(oa) = spec.osc_alpha {
                    let f = oa.freq_min + p.osc_seed * (oa.freq_max - oa.freq_min);
                    let ph = oa.phase_min + p.osc_seed * (oa.phase_max - oa.phase_min) + p.phase;
                    let w = 0.5 + 0.5 * (time * f + ph).sin();
                    alpha *= oa.scale_min + w * (oa.scale_max - oa.scale_min);
                }

                let mut size_mult = size_mult_ov;
                if let Some(sc) = spec.size_change {
                    let s = if t_norm <= sc.start_time {
                        sc.start_value
                    } else if t_norm >= sc.end_time {
                        sc.end_value
                    } else {
                        let u =
                            (t_norm - sc.start_time) / (sc.end_time - sc.start_time).max(1e-4);
                        sc.start_value + u * (sc.end_value - sc.start_value)
                    };
                    size_mult *= s;
                }
                if let Some(os) = spec.osc_size {
                    let f = os.freq_min + p.osc_seed * (os.freq_max - os.freq_min);
                    let ph = os.phase_min + p.osc_seed * (os.phase_max - os.phase_min) + p.phase;
                    let w = 0.5 + 0.5 * (time * f + ph).sin();
                    size_mult *= os.scale_min + w * (os.scale_max - os.scale_min);
                }

                let col = if let Some(cc) = spec.color_change {
                    let u = ((t_norm - cc.start_time) / (cc.end_time - cc.start_time).max(1e-4))
                        .clamp(0.0, 1.0);
                    [
                        (cc.start_value[0] + u * (cc.end_value[0] - cc.start_value[0]))
                            * spec.color_mult[0],
                        (cc.start_value[1] + u * (cc.end_value[1] - cc.start_value[1]))
                            * spec.color_mult[1],
                        (cc.start_value[2] + u * (cc.end_value[2] - cc.start_value[2]))
                            * spec.color_mult[2],
                    ]
                } else {
                    p.color
                };
                p.color = col;

                let world_pos = spec.transform.apply([p.x, p.y]);
                let world_x = world_pos[0] + offset[0];
                let world_y = world_pos[1] + offset[1];
                let base_r = (p.size * size_mult * t_scale * 0.5).max(0.5);

                let (uv_rect, aspect) = if let Some(ss) = &self.spritesheet {
                    let n_frames = ss.frames.max(1);
                    let cols = ss.cols.max(1);
                    let rows = ss.rows.max(1);
                    if n_frames > 1 {
                        let fidx = if spec.random_frame {
                            p.frame_seed % n_frames
                        } else {
                            ((t_norm * n_frames as f32).floor() as u32).min(n_frames - 1)
                        };
                        let col_i = fidx % cols;
                        let row_i = fidx / cols;
                        let u_size = 1.0 / (cols as f32);
                        let v_size = 1.0 / (rows as f32);
                        let u0 = (col_i as f32) * u_size;
                        let v0 = (row_i as f32) * v_size;
                        ([u0, v0, u_size, v_size], 1.0f32)
                    } else {
                        ([0.0, 0.0, 1.0, 1.0], 1.0f32)
                    }
                } else {
                    ([0.0, 0.0, 1.0, 1.0], 1.0f32)
                };

                let col_out = [
                    col[0] * brightness_mult,
                    col[1] * brightness_mult,
                    col[2] * brightness_mult,
                ];
                let alpha_out = alpha.clamp(0.0, 1.0);

                match spec.renderer {
                    ParticleRendererKind::Rope => {
                        rope_nodes.push((
                            p.age,
                            [world_x, world_y],
                            base_r,
                            col_out,
                            alpha_out,
                            uv_rect,
                        ));
                    }
                    ParticleRendererKind::RopeTrail { .. } if p.trail_len >= 2 => {
                        let seg_total = (p.trail_len as usize) - 1;
                        let mut prev_w = {
                            let wp = spec.transform.apply(p.trail[0]);
                            [wp[0] + offset[0], wp[1] + offset[1]]
                        };
                        for k in 0..seg_total {
                            let next_w = {
                                let wp = spec.transform.apply(p.trail[k + 1]);
                                [wp[0] + offset[0], wp[1] + offset[1]]
                            };
                            let frac0 = k as f32 / seg_total as f32;
                            let frac1 = (k + 1) as f32 / seg_total as f32;
                            let w_mid = base_r * (1.0 - 0.85 * ((frac0 + frac1) * 0.5));
                            let a_mid = alpha_out * (1.0 - 0.85 * ((frac0 + frac1) * 0.5));
                            pack_ribbon_segment(
                                out,
                                prev_w,
                                next_w,
                                w_mid.max(0.4),
                                col_out,
                                a_mid,
                                uv_rect,
                                frac0,
                                frac1,
                                max_bridge,
                            );
                            prev_w = next_w;
                        }
                    }
                    _ => {
                        let (axis_x, axis_y) =
                            if let ParticleRendererKind::SpriteTrail {
                                length: t_len,
                                max_length: t_max,
                            } = spec.renderer
                            {
                                let w_vel = spec.transform.apply_vec([p.vx, p.vy]);
                                let speed = w_vel[0].hypot(w_vel[1]);
                                let max_h = if t_max <= 10.0 {
                                    (base_r * t_max.max(1.0)).max(base_r)
                                } else {
                                    (t_max * t_scale * 0.5).max(base_r)
                                };
                                let trail_h = (base_r + speed * t_len * t_scale)
                                    .max(base_r)
                                    .min(max_h);
                                let trail_w = (base_r * 0.35).max(1.0);
                                let angle = if speed > 1e-3 {
                                    (-w_vel[0]).atan2(w_vel[1])
                                } else {
                                    t_rot
                                };
                                let (s, c) = angle.sin_cos();
                                ([c * trail_w, s * trail_w], [-s * trail_h, c * trail_h])
                            } else {
                                let unscaled_r = (p.size * size_mult * 0.5).max(0.5);
                                let (sx, cx) = p.rot_x.sin_cos();
                                let (sy, cy) = p.rot_y.sin_cos();
                                let (sz, cz) = p.rot.sin_cos();
                                let r_local = [
                                    (cz * cy - sz * sx * sy) * unscaled_r * aspect,
                                    -sz * cx * unscaled_r,
                                ];
                                let u_local =
                                    [(sz * cy + cz * sx * sy) * unscaled_r, cz * cx * unscaled_r];
                                (
                                    spec.transform.apply_vec(r_local),
                                    spec.transform.apply_vec(u_local),
                                )
                            };

                        out.extend_from_slice(&[
                            world_x,
                            world_y,
                            axis_x[0],
                            axis_x[1],
                            axis_y[0],
                            axis_y[1],
                            col_out[0],
                            col_out[1],
                            col_out[2],
                            alpha_out,
                            uv_rect[0],
                            uv_rect[1],
                            uv_rect[2],
                            uv_rect[3],
                        ]);
                    }
                }
            }

            if rope_mode && rope_nodes.len() >= 2 {
                rope_nodes.sort_by(|a, b| a.0.total_cmp(&b.0));
                let total = (rope_nodes.len() - 1) as f32;
                for k in 0..rope_nodes.len() - 1 {
                    let (_, p0, r0, c0, a0, uv0) = rope_nodes[k];
                    let (_, p1, r1, c1, a1, _) = rope_nodes[k + 1];
                    let avg_col = [
                        (c0[0] + c1[0]) * 0.5,
                        (c0[1] + c1[1]) * 0.5,
                        (c0[2] + c1[2]) * 0.5,
                    ];
                    pack_ribbon_segment(
                        out,
                        p0,
                        p1,
                        (r0 + r1) * 0.5,
                        avg_col,
                        (a0 + a1) * 0.5,
                        uv0,
                        k as f32 / total,
                        (k + 1) as f32 / total,
                        max_bridge,
                    );
                }
            }
            return;
        }

        let sx = self.spread[0].max(1.0);
        let sy = self.spread[1].max(1.0);
        let active_limit = ((self.particles.len() as f32) * self.live_rate_mult * self.live_count_mult)
            .round()
            .clamp(0.0, self.particles.len() as f32) as usize;
        for p in self.particles.iter_mut().take(active_limit) {
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
            let world_x = self.center[0] + p.x * sx + offset[0];
            let world_y = self.center[1] + p.y * sy + offset[1];
            let (w, h) = match self.kind {
                ParticleKind::Rain => (p.size * 0.35 * ortho[1], p.size * 4.8 * ortho[1]),
                ParticleKind::Sakura => (p.size * 1.3 * ortho[1], p.size * 0.8 * ortho[1]),
                ParticleKind::Fog => (p.size * ortho[0] * 0.5, p.size * ortho[1] * 0.5),
                _ => (p.size * ortho[1], p.size * ortho[1]),
            };
            let (s, c) = p.rot.sin_cos();
            let hw = w * 0.5 * size_mult_ov;
            let hh = h * 0.5 * size_mult_ov;
            out.extend_from_slice(&[
                world_x,
                world_y,
                c * hw,
                s * hw,
                -s * hh,
                c * hh,
                p.color[0] * brightness_mult,
                p.color[1] * brightness_mult,
                p.color[2] * brightness_mult,
                (p.alpha * twinkle * alpha_mult).clamp(0.0, 1.0),
                0.0,
                0.0,
                1.0,
                1.0,
            ]);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn pack_ribbon_segment(
    out: &mut Vec<f32>,
    p0: [f32; 2],
    p1: [f32; 2],
    half_w: f32,
    color: [f32; 3],
    alpha: f32,
    uv_rect: [f32; 4],
    frac0: f32,
    frac1: f32,
    max_bridge: f32,
) {
    let dx = p1[0] - p0[0];
    let dy = p1[1] - p0[1];
    let len = dx.hypot(dy);
    if len < 0.5 || len > max_bridge {
        return;
    }
    let nx = -dy / len;
    let ny = dx / len;
    let hw = half_w.max(0.4);
    let cx = (p0[0] + p1[0]) * 0.5;
    let cy = (p0[1] + p1[1]) * 0.5;
    let seg_v0 = uv_rect[1] + frac0 * uv_rect[3];
    let seg_vh = (frac1 - frac0) * uv_rect[3];
    out.extend_from_slice(&[
        cx,
        cy,
        nx * hw,
        ny * hw,
        dx * 0.5,
        dy * 0.5,
        color[0],
        color[1],
        color[2],
        alpha.clamp(0.0, 1.0),
        uv_rect[0],
        seg_v0,
        uv_rect[2],
        seg_vh,
    ]);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_emitter_with_mask(
        preset: serde_json::Value,
        obj: serde_json::Value,
        xform: Affine,
        mask: Option<LayerImageMask>,
    ) -> Emitter {
        let props = HashMap::new();
        let (mut spec, count, ov_rate, ov_count, ov_alpha, ov_size, ov_speed) =
            WeParticleSpec::parse(&preset, &obj, xform, &props, None, mask);
        let cps = spec.resolve_cps([xform.tx, xform.ty]);
        let mut particles = Vec::with_capacity(count);
        for i in 0..count {
            particles.push(spec.spawn(Some((i as f32 + 0.5) / count.max(1) as f32), &cps, None));
        }
        let audio_reactive = spec.uses_audio();
        let spread = spec.dist_max;
        Emitter {
            kind: ParticleKind::Fireflies,
            center: [xform.tx, xform.ty],
            spread,
            blend: LayerBlend::Normal,
            audio_reactive,
            texture: None,
            tex_is_r8: false,
            refract: false,
            normal_tex: None,
            refract_amount: 0.05,
            overbright: 1.0,
            spritesheet: None,
            we_spec: Some(spec),
            particles,
            child_indices: Vec::new(),
            child_local: None,
            child_probability: 1.0,
            base_ov_rate: ov_rate,
            base_ov_count: ov_count,
            base_ov_alpha: ov_alpha,
            base_ov_size: ov_size,
            base_ov_speed: ov_speed,
            live_rate_mult: ov_rate.max(0.0),
            live_count_mult: ov_count.max(0.0),
            live_alpha_mult: 1.0,
            live_size_mult: 1.0,
            live_speed_mult: 1.0,
            live_brightness: 1.0,
        }
    }

    fn make_test_emitter(preset: serde_json::Value, obj: serde_json::Value, xform: Affine) -> Emitter {
        make_test_emitter_with_mask(preset, obj, xform, None)
    }

    #[test]
    fn we_particle_operators_turbulence_vortex_attract_color_alpha_oscillate_and_collision() {
        let preset = serde_json::json!({
            "maxcount": 16,
            "controlpoint": [
                { "id": 0, "offset": "0 0 0", "flags": 0 },
                { "id": 1, "offset": "0 0 0", "flags": 1 }
            ],
            "emitter": [{
                "name": "sphererandom",
                "distancemin": 0,
                "distancemax": 40,
                "rate": 16,
                "audioprocessingmode": 3,
                "audioprocessingbounds": "0.2 0.9",
                "audioprocessingfrequencystart": 0,
                "audioprocessingfrequencyend": 3
            }],
            "initializer": [
                { "name": "lifetimerandom", "min": 2.0, "max": 2.0 },
                { "name": "sizerandom", "min": 20.0, "max": 20.0 },
                { "name": "hsvcolorrandom", "huemin": 0.1, "huemax": 0.2, "saturationmin": 1.0, "valuemin": 1.0 }
            ],
            "operator": [
                { "name": "movement", "drag": 0.5, "gravity": "0 0 0" },
                { "name": "turbulence", "speedmin": 30, "speedmax": 60, "scale": 0.01, "timescale": 2.0, "mask": "1 1 0" },
                { "name": "vortex", "controlpoint": 0, "distanceinner": 0, "distanceouter": 200, "speedinner": 40, "speedouter": 120 },
                { "name": "controlpointattract", "controlpoint": 1, "origin": "0 0 0", "scale": -800, "threshold": 150 },
                { "name": "collisionsphere", "controlpoint": 1, "origin": "0 0 0", "radius": 60, "collisionbehavior": "slide" },
                { "name": "oscillateposition", "frequencymin": 1.0, "frequencymax": 2.0, "scalemin": 4.0, "scalemax": 6.0 },
                { "name": "oscillatealpha", "frequencymin": 2.0, "frequencymax": 3.0, "scalemin": 0.6, "scalemax": 0.9 },
                { "name": "oscillatesize", "frequencymin": 2.0, "frequencymax": 3.0, "scalemin": 0.8, "scalemax": 1.2 },
                { "name": "alphachange", "starttime": 0.0, "endtime": 1.0, "startvalue": 1.0, "endvalue": 0.2 },
                { "name": "colorchange", "starttime": 0.0, "endtime": 1.0, "startvalue": "0 0 1", "endvalue": "1 0 0" },
                { "name": "angularmovement", "force": "0 0 2.0", "drag": 0.1 }
            ]
        });
        let mut em = make_test_emitter(
            preset,
            serde_json::json!({}),
            Affine::translate(500.0, 500.0),
        );
        assert!(em.audio_reactive);
        assert!(em.we_spec.as_ref().unwrap().hsv_color.is_some());

        let spectrum = [0.6f32; 64];
        let mut scratch = Vec::new();
        for step in 0..8 {
            em.update_and_pack(
                0.03,
                step as f32 * 0.03,
                [1920.0, 1080.0],
                [0.6, 0.4, 0.2, 0.5],
                &spectrum,
                [0.0, 0.0],
                [500.0, 500.0],
                None,
                &mut scratch,
            );
        }
        assert!(!scratch.is_empty());
        assert_eq!(scratch.len() % 14, 0);
        for v in &scratch {
            assert!(v.is_finite(), "non-finite particle output: {v}");
        }
        // Colorchange transitioned toward red-purple relative to initial pure blue.
        let any_red_shift = scratch.chunks_exact(14).any(|c| c[6] > 0.15);
        assert!(any_red_shift);
        // Collision sphere at (500, 500) radius 60 must push every particle outside radius 59.5.
        for c in scratch.chunks_exact(14) {
            let d = (c[0] - 500.0).hypot(c[1] - 500.0);
            assert!(d >= 59.5, "particle inside collision sphere: d={d}");
        }
    }

    #[test]
    fn we_particle_ropetrail_and_rope_pack_connected_ribbon_segments() {
        let ropetrail_preset = serde_json::json!({
            "maxcount": 1,
            "emitter": [{ "name": "sphererandom", "distancemax": 0, "rate": 1 }],
            "initializer": [
                { "name": "lifetimerandom", "min": 4.0, "max": 4.0 },
                { "name": "sizerandom", "min": 16.0, "max": 16.0 },
                { "name": "velocityrandom", "min": "120 0 0", "max": "120 0 0" }
            ],
            "renderer": [{ "name": "ropetrail", "length": 0.2 }]
        });
        let mut rt_em = make_test_emitter(
            ropetrail_preset,
            serde_json::json!({}),
            Affine::translate(200.0, 200.0),
        );
        let spec0 = [0.0f32; 64];
        let mut scratch = Vec::new();
        for step in 0..10 {
            rt_em.update_and_pack(
                0.025,
                step as f32 * 0.025,
                [1920.0, 1080.0],
                [0.0; 4],
                &spec0,
                [0.0, 0.0],
                [0.0, 0.0],
                None,
                &mut scratch,
            );
        }
        let segs = scratch.len() / 14;
        assert!(segs >= 3, "expected multiple connected ribbon quads for ropetrail, got {segs}");

        // Rope connects alive particles ordered by age along cursor trail, skipping huge jumps.
        let rope_preset = serde_json::json!({
            "maxcount": 4,
            "controlpoint": [{ "id": 0, "offset": "0 0 0", "flags": 1 }],
            "emitter": [{ "name": "sphererandom", "distancemax": 0, "rate": 4 }],
            "initializer": [
                { "name": "lifetimerandom", "min": 2.0, "max": 2.0 },
                { "name": "sizerandom", "min": 12.0, "max": 12.0 }
            ],
            "renderer": [{ "name": "rope" }]
        });
        let mut rope_em = make_test_emitter(
            rope_preset,
            serde_json::json!({}),
            Affine::identity(),
        );
        rope_em.particles.clear();
        for (i, pos) in [(100.0f32, 100.0f32), (120.0, 100.0), (140.0, 100.0), (1800.0, 100.0)]
            .into_iter()
            .enumerate()
        {
            let mut p = rope_em
                .we_spec
                .as_mut()
                .unwrap()
                .spawn(Some(0.0), &[[0.0; 2]; 8], None);
            p.x = pos.0;
            p.y = pos.1;
            p.age = (i as f32) * 0.1;
            rope_em.particles.push(p);
        }
        rope_em.update_and_pack(
            0.016,
            0.016,
            [1920.0, 1080.0],
            [0.0; 4],
            &spec0,
            [0.0, 0.0],
            [100.0, 100.0],
            None,
            &mut scratch,
        );
        // 4 nodes sorted by age -> first 2 segments are 20px each (packed), last jump is 1660px (> 45% diag, skipped!).
        assert_eq!(scratch.len() / 14, 2);
    }

    #[test]
    fn we_particle_mapsequence_places_particles_around_and_between_cps() {
        let ring_preset = serde_json::json!({
            "maxcount": 4,
            "emitter": [{ "name": "sphererandom", "distancemin": 100, "distancemax": 100, "rate": 4 }],
            "initializer": [{ "name": "mapsequencearoundcontrolpoint", "controlpoint": 0, "count": 4, "bounds": "0 1" }]
        });
        let em_ring = make_test_emitter(ring_preset, serde_json::json!({}), Affine::identity());
        assert_eq!(em_ring.particles.len(), 4);
        for p in &em_ring.particles {
            let r = p.x.hypot(p.y);
            assert!((r - 100.0).abs() < 1e-2, "expected radius 100, got {r}");
        }
        // Check angular separation (~90 deg steps around circle).
        assert!((em_ring.particles[0].x - 100.0).abs() < 1e-2);
        assert!((em_ring.particles[1].y - 100.0).abs() < 1e-2);

        let line_preset = serde_json::json!({
            "maxcount": 3,
            "controlpoint": [
                { "id": 0, "offset": "0 0 0", "flags": 0 },
                { "id": 1, "offset": "200 100 0", "flags": 0 }
            ],
            "emitter": [{ "name": "sphererandom", "rate": 3 }],
            "initializer": [{ "name": "mapsequencebetweencontrolpoints", "startcontrolpoint": 0, "endcontrolpoint": 1, "count": 3 }]
        });
        let em_line = make_test_emitter(line_preset, serde_json::json!({}), Affine::identity());
        assert_eq!(em_line.particles.len(), 3);
        assert!((em_line.particles[0].x - 0.0).abs() < 1e-3);
        assert!((em_line.particles[1].x - 100.0).abs() < 1e-3);
        assert!((em_line.particles[1].y - 50.0).abs() < 1e-3);
        assert!((em_line.particles[2].x - 200.0).abs() < 1e-3);
    }

    #[test]
    fn we_particle_layerimage_samples_mask_points_and_spawns_inside_mask() {
        // 4x4 RGBA image: only top-left 2x2 quadrant is opaque.
        let mut rgba = vec![0u8; 4 * 4 * 4];
        for py in 0..2usize {
            for px in 0..2usize {
                let idx = (py * 4 + px) * 4;
                rgba[idx] = 255;
                rgba[idx + 1] = 255;
                rgba[idx + 2] = 255;
                rgba[idx + 3] = 255;
            }
        }
        let mask = sample_layer_image_mask(
            &rgba,
            4,
            4,
            [200.0, 200.0],
            Affine::identity(),
            Affine::identity(),
        )
        .expect("mask points sampled");
        assert_eq!(mask.points.len(), 4);

        let preset = serde_json::json!({
            "maxcount": 12,
            "emitter": [{ "name": "layerimage", "rate": 12 }],
            "initializer": [{ "name": "lifetimerandom", "min": 2.0, "max": 2.0 }]
        });
        let em = make_test_emitter_with_mask(
            preset,
            serde_json::json!({}),
            Affine::identity(),
            Some(mask),
        );
        assert_eq!(em.particles.len(), 12);
        for p in &em.particles {
            assert!(
                p.x < 0.0 && p.y > 0.0,
                "expected particle inside top-left quadrant, got ({}, {})",
                p.x,
                p.y
            );
        }
    }

    #[test]
    fn we_particle_rate_accumulator_emits_smoothly_and_drains_at_zero_rate() {
        let preset = serde_json::json!({
            "maxcount": 50,
            "emitter": [{ "name": "sphererandom", "distancemax": 20, "rate": 10 }],
            "initializer": [{ "name": "lifetimerandom", "min": 2.0, "max": 2.0 }]
        });
        let mut em = make_test_emitter(preset, serde_json::json!({}), Affine::identity());
        em.particles.clear();

        let spec0 = [0.0f32; 64];
        let mut scratch = Vec::new();
        for step in 0..10 {
            em.update_and_pack(
                0.05,
                step as f32 * 0.05,
                [1920.0, 1080.0],
                [0.0; 4],
                &spec0,
                [0.0, 0.0],
                [0.0, 0.0],
                None,
                &mut scratch,
            );
        }
        assert_eq!(em.particles.len(), 5, "expected smooth 10/s * 0.5s = 5 spawns");

        // Setting live_rate_mult = 0 stops new births while letting alive particles expire naturally.
        em.live_rate_mult = 0.0;
        for step in 0..30 {
            em.update_and_pack(
                0.1,
                0.5 + step as f32 * 0.1,
                [1920.0, 1080.0],
                [0.0; 4],
                &spec0,
                [0.0, 0.0],
                [0.0, 0.0],
                None,
                &mut scratch,
            );
        }
        assert_eq!(em.particles.len(), 0, "expected alive particles to drain to 0");

        // Authored ov_rate == 0 / ov_count == 0 must still recover when script raises overrides.
        let mut paused = make_test_emitter(
            serde_json::json!({
                "maxcount": 20,
                "emitter": [{ "name": "sphererandom", "distancemax": 20, "rate": 20 }],
                "initializer": [{ "name": "lifetimerandom", "min": 2.0, "max": 2.0 }]
            }),
            serde_json::json!({
                "instanceoverride": { "rate": 0.0, "count": 0.0 }
            }),
            Affine::identity(),
        );
        assert_eq!(paused.particles.len(), 0);
        paused.update_instance_override("rate", &serde_json::json!(1.0));
        paused.update_instance_override("count", &serde_json::json!(1.0));
        paused.update_and_pack(
            0.05,
            0.05,
            [1920.0, 1080.0],
            [0.0; 4],
            &spec0,
            [0.0, 0.0],
            [0.0, 0.0],
            Some((&[], false)),
            &mut scratch,
        );
        assert_eq!(paused.particles.len(), 0, "empty spawn_anchors must suppress spawns");
        paused.update_and_pack(
            0.05,
            0.10,
            [1920.0, 1080.0],
            [0.0; 4],
            &spec0,
            [0.0, 0.0],
            [0.0, 0.0],
            None,
            &mut scratch,
        );
        assert!(paused.particles.len() >= 1, "ov_rate=0/ov_count=0 must recover after override");
    }
}
