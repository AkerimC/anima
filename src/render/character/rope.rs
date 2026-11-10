//! The rope in a tug of war: a braided strip sagging a little between the
//! two ends of the line.

use super::{ActorTarget, CharacterOverlay};
use super::SceneZoom;
use crate::render::FrameInfo;
use crate::render::gl::{Program, bytemuck_cast};
use anyhow::{Result, anyhow};
use glow::HasContext;

const SEGMENTS: usize = 32;

const VS: &str = r#"#version 300 es
precision highp float;
layout(location = 0) in vec2 a_pos; // x: 0..1 along the rope, y: -1..1 across
out vec2 v_pos;
uniform vec2 u_a;
uniform vec2 u_b;
uniform float u_sag;
uniform float u_half;
uniform vec4 u_clip;
void main() {
    float t = a_pos.x;
    vec2 p = mix(u_a, u_b, t);
    p.y -= u_sag * 4.0 * t * (1.0 - t);
    p.y += a_pos.y * u_half;
    v_pos = a_pos;
    gl_Position = vec4(p * u_clip.xy + u_clip.zw, 0.0, 1.0);
}
"#;

const FS: &str = r#"#version 300 es
precision highp float;
in vec2 v_pos;
out vec4 fragColor;
uniform float u_twists;
void main() {
    // slanted strands, rounded off towards the edges
    float s = fract(v_pos.x * u_twists + v_pos.y * 0.35);
    float strand = smoothstep(0.0, 0.2, s) * (1.0 - smoothstep(0.7, 1.0, s));
    vec3 col = mix(vec3(0.42, 0.27, 0.13), vec3(0.82, 0.64, 0.38), strand);
    col *= 1.0 - 0.45 * v_pos.y * v_pos.y;
    float alpha = 1.0 - smoothstep(0.75, 1.0, abs(v_pos.y));
    fragColor = vec4(col * alpha, alpha);
}
"#;

pub(super) struct RopeGl {
    program: Program,
    vao: glow::VertexArray,
    #[allow(dead_code)]
    vbo: glow::Buffer,
}

impl RopeGl {
    fn new(gl: &glow::Context) -> Result<Self> {
        let program = Program::new(gl, VS, FS)?;
        let mut verts = Vec::with_capacity((SEGMENTS + 1) * 4);
        for k in 0..=SEGMENTS {
            let t = k as f32 / SEGMENTS as f32;
            verts.extend_from_slice(&[t, -1.0, t, 1.0]);
        }
        unsafe {
            let vao = gl.create_vertex_array().map_err(|e| anyhow!(e))?;
            let vbo = gl.create_buffer().map_err(|e| anyhow!(e))?;
            gl.bind_vertex_array(Some(vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytemuck_cast(&verts), glow::STATIC_DRAW);
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 8, 0);
            gl.bind_vertex_array(None);
            Ok(Self { program, vao, vbo })
        }
    }
}

impl CharacterOverlay {
    pub(super) fn draw_ropes(&mut self, gl: &glow::Context, info: &FrameInfo, target: &mut dyn ActorTarget) {
        let ropes: Vec<([f32; 4], Option<SceneZoom>)> = self.ropes_owned().into_iter().filter_map(|(r, o)| self.placed(o).map(|z| (r, z))).collect();
        if ropes.is_empty() {
            return;
        }
        if self.rope.is_none() {
            match RopeGl::new(gl) {
                Ok(r) => self.rope = Some(r),
                Err(e) => {
                    log::warn!("rope: {e:#}");
                    return;
                }
            }
        }
        let time = self.start.elapsed().as_secs_f32();
        let (w, h) = (info.width.max(1) as f32, info.height.max(1) as f32);
        let Some(r) = self.rope.as_mut() else { return };
        for ([x0, y0, x1, y1], zoom) in ropes {
            if !target.begin(gl, 0, [x0, y0.min(y1) - 40.0, x1, y0.max(y1) + 40.0]) {
                continue;
            }
            let clip = |x: f32, y: f32| [x / w * 2.0 - 1.0, 1.0 - y / h * 2.0];
            let (a, b) = (clip(x0, y0), clip(x1, y1));
            r.program.bind(gl);
            r.program.set_vec2(gl, "u_a", a[0], a[1]);
            r.program.set_vec2(gl, "u_b", b[0], b[1]);
            // pulled nearly tight, trembling a little
            r.program.set_f32(gl, "u_sag", (12.0 + (time * 9.0).sin() * 3.0) / h * 2.0);
            r.program.set_f32(gl, "u_half", 5.0 / h * 2.0);
            r.program.set_f32(gl, "u_twists", (x1 - x0).abs() / 9.0);
            r.program.set_vec4(gl, "u_clip", zoom.map_or_else(|| target.clip_xform(), |z| z.clip_xform()));
            unsafe {
                gl.enable(glow::BLEND);
                gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
                gl.bind_vertex_array(Some(r.vao));
                gl.draw_arrays(glow::TRIANGLE_STRIP, 0, ((SEGMENTS + 1) * 2) as i32);
                gl.bind_vertex_array(None);
                gl.disable(glow::BLEND);
            }
            target.end(gl, 0);
        }
    }
}

const CARROT_VS: &str = r#"#version 300 es
precision highp float;
layout(location = 0) in vec2 a_pos;
out vec2 v_pos;
uniform vec4 u_rect; // x0, y0, x1, y1 in clip space
uniform vec4 u_clip;
void main() {
    v_pos = a_pos;
    vec2 p = mix(u_rect.xy, u_rect.zw, a_pos * 0.5 + 0.5);
    gl_Position = vec4(p * u_clip.xy + u_clip.zw, 0.0, 1.0);
}
"#;

// y up: 0 a carrot, 1 a wrapped box, 2 a balloon, 3 a boxing glove on a spring,
// 4 a sleeping Z, 5 a drop of sweat, 6 a crumb of carrot, 7 the arrow over "it",
// 8 a blindfold, 9 a bush, 10 a barrel, 11 a crate, 12 a music note,
// 13 a fishing rod, 14 a line and float, 15 a star, 16 a rubber duck, 17 an old boot, 18 a tin can,
// 20 confetti, 21 a beast out of the ground, 22 an umbrella, 23 a snowflake
const CARROT_FS: &str = r#"#version 300 es
precision highp float;
in vec2 v_pos;
out vec4 fragColor;
uniform int u_kind;
uniform int u_flip;
float blade(vec2 p, float x, float lean) {
    float t = clamp((p.y - 0.25) / 0.7, 0.0, 1.0);
    float d = abs(p.x - x - lean * t) - 0.17 * (1.0 - t);
    return (1.0 - smoothstep(-0.02, 0.03, d)) * step(0.25, p.y) * (1.0 - smoothstep(0.9, 0.95, p.y));
}
float disc(vec2 p, vec2 c, float r) {
    return 1.0 - smoothstep(r - 0.03, r + 0.01, length(p - c));
}
float bar(vec2 p, vec2 lo, vec2 hi) {
    return step(lo.x, p.x) * step(p.x, hi.x) * step(lo.y, p.y) * step(p.y, hi.y);
}
vec4 carrot(vec2 p) {
    float t = clamp((p.y + 1.0) / 1.45, 0.0, 1.0);
    float half_w = 0.1 + 0.62 * pow(t, 0.75);
    float d = abs(p.x) - half_w;
    float body = (1.0 - smoothstep(-0.04, 0.02, d)) * step(p.y, 0.45) * step(-1.0, p.y);
    float groove = smoothstep(0.82, 1.0, sin(p.y * 15.0 + p.x * 1.6));
    vec3 orange = mix(vec3(1.0, 0.55, 0.08), vec3(0.8, 0.3, 0.02), groove * 0.7);
    orange *= 1.0 - 0.35 * smoothstep(0.1, 1.0, p.x / max(half_w, 0.01));
    float leaf = max(blade(p, 0.0, 0.0), max(blade(p, -0.14, -0.35), blade(p, 0.14, 0.35)));
    return vec4(body > 0.0 ? orange : vec3(0.2, 0.68, 0.18), max(body, leaf));
}
vec4 box(vec2 p) {
    float body = bar(p, vec2(-0.78, -1.0), vec2(0.78, 0.3));
    float lid = bar(p, vec2(-0.92, 0.3), vec2(0.92, 0.56));
    float ribbon = step(abs(p.x), 0.16);
    float bow = max(disc(p, vec2(-0.27, 0.74), 0.22), disc(p, vec2(0.27, 0.74), 0.22));
    float a = max(max(body, lid), bow);
    vec3 col = mix(lid > 0.0 ? vec3(0.9, 0.2, 0.26) : vec3(0.72, 0.13, 0.2), vec3(1.0, 0.82, 0.25), max(ribbon, bow));
    return vec4(col, a);
}
vec4 balloon(vec2 p) {
    float d = length((p - vec2(0.0, 0.25)) / vec2(0.62, 0.72));
    float body = 1.0 - smoothstep(0.96, 1.0, d);
    float knot = bar(p, vec2(-0.07, -0.56), vec2(0.07, -0.44));
    float string_ = step(abs(p.x - 0.06 * sin(p.y * 9.0)), 0.025) * step(p.y, -0.5);
    float shine = disc(p, vec2(-0.22, 0.5), 0.13);
    vec3 col = mix(vec3(0.95, 0.2, 0.4), vec3(1.0, 0.85, 0.9), shine);
    if (body <= 0.0) col = string_ > 0.0 ? vec3(0.85) : vec3(0.75, 0.15, 0.3);
    return vec4(col, max(max(body, knot), string_));
}
vec4 glove(vec2 p) {
    float fist = max(disc(p, vec2(0.58, 0.0), 0.42), disc(p, vec2(0.36, 0.34), 0.18));
    float cuff = bar(p, vec2(0.04, -0.34), vec2(0.2, 0.34));
    float spring = step(p.x, 0.06) * step(abs(p.y - 0.28 * sin(p.x * 22.0)), 0.09);
    vec3 col = cuff > 0.0 ? vec3(0.95) : (fist > 0.0 ? vec3(0.9, 0.12, 0.12) : vec3(0.75, 0.76, 0.8));
    return vec4(col, max(max(fist, cuff), spring));
}
vec4 zee(vec2 p) {
    float top = bar(p, vec2(-0.8, 0.5), vec2(0.8, 0.85));
    float bottom = bar(p, vec2(-0.8, -0.85), vec2(0.8, -0.5));
    float slash = step(abs(p.x - p.y * 1.2), 0.3) * step(abs(p.y), 0.55);
    return vec4(0.75, 0.85, 1.0, max(max(top, bottom), slash));
}
vec4 sweat(vec2 p) {
    float round_part = disc(p, vec2(0.0, -0.3), 0.55);
    float half_w = 0.5 * (1.0 - (p.y + 0.1) / 0.9);
    float tip = step(abs(p.x), half_w) * step(-0.1, p.y) * step(p.y, 0.8);
    float shine = disc(p, vec2(-0.16, -0.38), 0.14);
    return vec4(mix(vec3(0.45, 0.78, 1.0), vec3(1.0), shine), max(round_part, tip));
}
vec4 crumb(vec2 p) {
    float a = atan(p.y, p.x);
    float d = length(p * vec2(1.0, 1.25)) - (0.72 + 0.14 * sin(a * 5.0 + 1.3));
    vec3 col = mix(vec3(1.0, 0.6, 0.12), vec3(0.85, 0.35, 0.05), smoothstep(-0.4, 0.6, p.x - p.y));
    return vec4(col, 1.0 - smoothstep(-0.06, 0.04, d));
}
vec4 marker(vec2 p) {
    // a fat arrow pointing down, white rim round red
    float outer = step(p.y, 0.75) * step(-0.95 + abs(p.x) * 1.7, p.y) * step(abs(p.x), 0.95);
    float inner = step(p.y, 0.6) * step(-0.75 + abs(p.x) * 1.7, p.y) * step(abs(p.x), 0.8);
    vec3 red = mix(vec3(0.85, 0.12, 0.2), vec3(1.0, 0.4, 0.45), smoothstep(-0.6, 0.6, p.y));
    return vec4(inner > 0.0 ? red : vec3(1.0), outer);
}
vec4 blindfold(vec2 p) {
    float strip = step(abs(p.y), 0.55) * step(abs(p.x), 0.78);
    float knot = disc(p, vec2(0.84, 0.0), 0.28);
    float tail = step(abs(p.y + 0.35 + (p.x - 0.84) * 0.6), 0.16) * step(0.84, p.x);
    vec3 col = mix(vec3(0.7, 0.04, 0.12), vec3(0.98, 0.22, 0.28), smoothstep(-0.5, 0.5, p.y));
    return vec4(col, max(max(strip, knot), tail));
}
vec4 bush(vec2 p) {
    float a = max(max(disc(p, vec2(-0.45, -0.3), 0.52), disc(p, vec2(0.45, -0.3), 0.52)), max(disc(p, vec2(0.0, 0.15), 0.62), bar(p, vec2(-0.92, -1.0), vec2(0.92, -0.35))));
    float spot = disc(p, vec2(-0.2, 0.35), 0.16) + disc(p, vec2(0.4, -0.1), 0.12);
    vec3 col = mix(vec3(0.12, 0.4, 0.16), vec3(0.35, 0.72, 0.3), smoothstep(-0.9, 0.7, p.y));
    col = mix(col, vec3(0.5, 0.85, 0.4), min(spot, 1.0) * 0.6);
    return vec4(col, a);
}
vec4 barrel(vec2 p) {
    float w = 0.66 + 0.14 * (1.0 - p.y * p.y);
    float body = step(abs(p.x), w) * step(abs(p.y), 0.96);
    float hoop = step(abs(abs(p.y) - 0.55), 0.07);
    vec3 col = mix(vec3(0.42, 0.24, 0.11), vec3(0.68, 0.45, 0.24), smoothstep(-w, 0.3, -abs(p.x - 0.15)));
    col = mix(col, vec3(0.32, 0.32, 0.35), hoop);
    return vec4(col, body);
}
vec4 crate_box(vec2 p) {
    float body = bar(p, vec2(-0.92, -0.96), vec2(0.92, 0.86));
    float inner = bar(p, vec2(-0.74, -0.78), vec2(0.74, 0.68));
    float cross_ = step(abs(p.x - p.y * 0.97), 0.13) * inner;
    vec3 col = mix(vec3(0.58, 0.4, 0.22), vec3(0.78, 0.58, 0.34), smoothstep(-1.0, 0.9, p.y));
    col = mix(col, vec3(0.48, 0.32, 0.17), max(1.0 - inner, cross_));
    return vec4(col, body);
}
vec4 note(vec2 p) {
    float head = disc(p, vec2(-0.28, -0.5), 0.34);
    float stem = bar(p, vec2(-0.02, -0.5), vec2(0.12, 0.78));
    float flag = bar(p, vec2(-0.02, 0.52), vec2(0.5, 0.78));
    return vec4(vec3(1.0, 0.86, 0.35), max(max(head, stem), flag));
}
vec4 rod(vec2 p) {
    // bottom left (the hand) to top right (the tip)
    float t = clamp((p.x + 1.0) * 0.5, 0.0, 1.0);
    float y = -1.0 + 2.0 * t;
    float a = step(abs(p.y - y), 0.09 * (1.2 - t));
    vec3 col = mix(vec3(0.35, 0.22, 0.12), vec3(0.75, 0.6, 0.35), t);
    return vec4(col, a);
}
vec4 line_float(vec2 p) {
    float line = step(abs(p.x), 0.07) * step(-0.7, p.y);
    float top = disc(p, vec2(0.0, -0.78), 0.32) * step(-0.78, p.y);
    float bottom = disc(p, vec2(0.0, -0.78), 0.32) * step(p.y, -0.78);
    vec3 col = top > 0.0 ? vec3(0.95, 0.2, 0.2) : bottom > 0.0 ? vec3(1.0) : vec3(0.9);
    return vec4(col, max(line * 0.8, max(top, bottom)));
}
vec4 star(vec2 p) {
    float a = atan(p.y, p.x);
    float r = 0.55 + 0.35 * cos(a * 5.0 + 1.57);
    float body = 1.0 - smoothstep(r - 0.04, r + 0.02, length(p));
    return vec4(mix(vec3(1.0, 0.78, 0.15), vec3(1.0, 0.95, 0.6), smoothstep(0.4, -0.2, length(p))), body);
}
vec4 duck(vec2 p) {
    float body = 1.0 - smoothstep(0.92, 0.98, length((p - vec2(0.0, -0.35)) / vec2(0.8, 0.5)));
    float head = disc(p, vec2(0.35, 0.3), 0.32);
    float beak = bar(p, vec2(0.62, 0.2), vec2(0.9, 0.32));
    float eye = disc(p, vec2(0.42, 0.38), 0.06);
    vec3 col = beak > 0.0 ? vec3(1.0, 0.5, 0.1) : eye > 0.0 ? vec3(0.1) : vec3(1.0, 0.85, 0.2);
    return vec4(col, max(max(body, head), beak));
}
vec4 boot(vec2 p) {
    float leg = bar(p, vec2(-0.5, -0.4), vec2(0.05, 0.9));
    float foot = bar(p, vec2(-0.5, -0.9), vec2(0.85, -0.4)) * (1.0 - step(0.0, (p.x - 0.55) + (p.y + 0.4) * 0.7));
    float sole = bar(p, vec2(-0.55, -0.95), vec2(0.8, -0.8));
    vec3 col = sole > 0.0 ? vec3(0.2, 0.15, 0.1) : vec3(0.45, 0.32, 0.2);
    return vec4(col, max(max(leg, foot), sole));
}
vec4 can(vec2 p) {
    float body = bar(p, vec2(-0.5, -0.85), vec2(0.5, 0.75));
    float label = bar(p, vec2(-0.5, -0.4), vec2(0.5, 0.35));
    vec3 col = label > 0.0 ? vec3(0.3, 0.55, 0.85) : vec3(0.72, 0.74, 0.78);
    col *= 0.75 + 0.25 * smoothstep(0.5, -0.3, abs(p.x + 0.15));
    return vec4(col, body);
}
vec4 beast(vec2 p) {
    float body = 1.0 - smoothstep(0.86, 0.92, length((p - vec2(0.0, -0.25)) / vec2(0.62, 0.8)));
    float arm = max(bar(p, vec2(-0.92, -0.45), vec2(-0.55, 0.05)), bar(p, vec2(0.55, -0.45), vec2(0.92, 0.05)));
    float horn = max(disc(p, vec2(-0.3, 0.62), 0.13), disc(p, vec2(0.3, 0.62), 0.13));
    float eyes = max(disc(p, vec2(-0.24, 0.12), 0.15), disc(p, vec2(0.24, 0.12), 0.15));
    float pupils = max(disc(p, vec2(-0.2, 0.1), 0.06), disc(p, vec2(0.2, 0.1), 0.06));
    float mouth = bar(p, vec2(-0.3, -0.34), vec2(0.3, -0.2));
    float tooth = mouth * step(0.5, fract((p.x + 0.3) * 8.0)) * step(-0.27, p.y);
    vec3 col = vec3(0.25, 0.55, 0.42);
    if (horn > 0.0 && body <= 0.0) col = vec3(0.85, 0.8, 0.6);
    if (mouth > 0.0) col = tooth > 0.0 ? vec3(0.95) : vec3(0.2, 0.03, 0.06);
    if (eyes > 0.0) col = pupils > 0.0 ? vec3(0.8, 0.05, 0.05) : vec3(1.0, 0.95, 0.6);
    return vec4(col, max(max(body, arm), horn));
}
vec4 umbrella(vec2 p) {
    // canopy: a dome whose rim curves up between the ribs
    vec2 q = (p - vec2(0.0, 0.05)) / vec2(0.96, 0.8);
    float dome = step(0.0, q.y) * (1.0 - smoothstep(0.97, 1.0, length(q)));
    float seg = fract((p.x + 0.96) / 0.48);
    float edge = 0.05 + 0.1 * sin(3.14159 * seg);
    dome *= smoothstep(edge - 0.02, edge + 0.01, p.y);
    // ribs from the tip down to each point of the rim
    float ribs = 0.0;
    for (int k = 0; k < 5; k++) {
        float rx = -0.96 + 0.48 * float(k);
        vec2 a = vec2(0.0, 0.85), b = vec2(rx, 0.05);
        vec2 ab = b - a;
        float t = clamp(dot(p - a, ab) / dot(ab, ab), 0.0, 1.0);
        ribs = max(ribs, 1.0 - smoothstep(0.012, 0.03, length(p - a - ab * t)));
    }
    // soft shading: lighter towards the top
    float light = clamp(q.y * 0.6 + 0.4, 0.0, 1.0);
    vec3 cloth = mix(vec3(0.82, 0.33, 0.45), vec3(1.0, 0.62, 0.72), light);
    vec3 col = mix(cloth, vec3(0.62, 0.2, 0.3), ribs * 0.8);
    // the shaft, a little tip on top, and a hooked handle at the bottom
    float shaft = bar(p, vec2(-0.025, -0.82), vec2(0.025, 0.9));
    float tip = disc(p, vec2(0.0, 0.92), 0.04);
    vec2 h = p - vec2(0.1, -0.82);
    float hook = step(h.y, 0.0) * (1.0 - smoothstep(0.02, 0.04, abs(length(h) - 0.1)));
    float stick = max(max(shaft, tip), hook);
    col = mix(col, vec3(0.32, 0.22, 0.16), stick);
    return vec4(col, max(dome, stick));
}
vec4 snowflake(vec2 p) {
    float a = atan(p.y, p.x);
    float r = length(p);
    float arms = smoothstep(0.18, 0.05, abs(sin(a * 3.0)) * r) * step(r, 0.9);
    float core = disc(p, vec2(0.0), 0.2);
    return vec4(vec3(0.95, 0.97, 1.0), max(arms, core) * 0.9);
}
vec4 confetti(vec2 p) {
    vec2 cell = floor((p + 1.0) * 4.0);
    vec2 f = fract((p + 1.0) * 4.0) - 0.5;
    float h = fract(sin(dot(cell, vec2(12.9898, 78.233))) * 43758.5453);
    float on = step(0.55, h) * step(abs(f.x), 0.22) * step(abs(f.y), 0.12) * step(length(p), 1.0);
    vec3 col = h > 0.85 ? vec3(1.0, 0.4, 0.6) : h > 0.75 ? vec3(0.4, 0.8, 1.0) : h > 0.65 ? vec3(1.0, 0.85, 0.3) : vec3(0.5, 0.95, 0.5);
    return vec4(col, on);
}
void main() {
    vec2 p = v_pos;
    if (u_flip == 1) p.x = -p.x;
    vec4 c = u_kind == 1 ? box(p) : u_kind == 2 ? balloon(p) : u_kind == 3 ? glove(p) : u_kind == 4 ? zee(p) : u_kind == 5 ? sweat(p) : u_kind == 6 ? crumb(p) : u_kind == 7 ? marker(p) : u_kind == 8 ? blindfold(p) : u_kind == 9 ? bush(p) : u_kind == 10 ? barrel(p) : u_kind == 11 ? crate_box(p) : u_kind == 12 ? note(p) : u_kind == 13 ? rod(p) : u_kind == 14 ? line_float(p) : u_kind == 15 ? star(p) : u_kind == 16 ? duck(p) : u_kind == 17 ? boot(p) : u_kind == 18 ? can(p) : u_kind == 20 ? confetti(p) : u_kind == 21 ? beast(p) : u_kind == 22 ? umbrella(p) : u_kind == 23 ? snowflake(p) : carrot(p);
    fragColor = vec4(c.rgb * c.a, c.a);
}
"#;

pub(super) struct CarrotGl {
    program: Program,
    vao: glow::VertexArray,
    #[allow(dead_code)]
    vbo: glow::Buffer,
}

impl CarrotGl {
    fn new(gl: &glow::Context) -> Result<Self> {
        let program = Program::new(gl, CARROT_VS, CARROT_FS)?;
        let verts: [f32; 8] = [-1.0, -1.0, 1.0, -1.0, -1.0, 1.0, 1.0, 1.0];
        unsafe {
            let vao = gl.create_vertex_array().map_err(|e| anyhow!(e))?;
            let vbo = gl.create_buffer().map_err(|e| anyhow!(e))?;
            gl.bind_vertex_array(Some(vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytemuck_cast(&verts), glow::STATIC_DRAW);
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 8, 0);
            gl.bind_vertex_array(None);
            Ok(Self { program, vao, vbo })
        }
    }
}

impl CharacterOverlay {
    pub(super) fn draw_carrot(&mut self, gl: &glow::Context, info: &FrameInfo, target: &mut dyn ActorTarget) {
        let props: Vec<_> = self.props().into_iter().filter_map(|p| self.placed(p.3).map(|z| (p.0, p.1, p.2, z))).collect();
        if props.is_empty() {
            return;
        }
        if self.carrot_gl.is_none() {
            match CarrotGl::new(gl) {
                Ok(c) => self.carrot_gl = Some(c),
                Err(e) => {
                    log::warn!("carrot: {e:#}");
                    return;
                }
            }
        }
        let (w, h) = (info.width.max(1) as f32, info.height.max(1) as f32);
        let Some(c) = self.carrot_gl.as_mut() else { return };
        for (kind, rect, flip, zoom) in props {
            if !target.begin(gl, 0, rect) {
                continue;
            }
            let [x0, y0, x1, y1] = rect;
            c.program.bind(gl);
            c.program.set_vec4(gl, "u_rect", [x0 / w * 2.0 - 1.0, 1.0 - y1 / h * 2.0, x1 / w * 2.0 - 1.0, 1.0 - y0 / h * 2.0]);
            c.program.set_vec4(gl, "u_clip", zoom.map_or_else(|| target.clip_xform(), |z| z.clip_xform()));
            c.program.set_i32(gl, "u_kind", kind);
            c.program.set_i32(gl, "u_flip", i32::from(flip));
            unsafe {
                gl.enable(glow::BLEND);
                gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
                gl.bind_vertex_array(Some(c.vao));
                gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
                gl.bind_vertex_array(None);
                gl.disable(glow::BLEND);
            }
            target.end(gl, 0);
        }
    }
}
