//! The rope in a tug of war: a braided strip sagging a little between the
//! two ends of the line.

use super::{ActorTarget, CharacterOverlay};
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
        let ropes = self.rope_ends();
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
        for [x0, y0, x1, y1] in ropes {
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
            r.program.set_vec4(gl, "u_clip", target.clip_xform());
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

// tip down, y up: an orange body with a few grooves, a tuft of leaves on top
const CARROT_FS: &str = r#"#version 300 es
precision highp float;
in vec2 v_pos;
out vec4 fragColor;
float blade(vec2 p, float x, float lean) {
    float t = clamp((p.y - 0.25) / 0.7, 0.0, 1.0);
    float d = abs(p.x - x - lean * t) - 0.17 * (1.0 - t);
    return (1.0 - smoothstep(-0.02, 0.03, d)) * step(0.25, p.y) * (1.0 - smoothstep(0.9, 0.95, p.y));
}
void main() {
    vec2 p = v_pos;
    float t = clamp((p.y + 1.0) / 1.45, 0.0, 1.0);
    float half_w = 0.1 + 0.62 * pow(t, 0.75);
    float d = abs(p.x) - half_w;
    float body = (1.0 - smoothstep(-0.04, 0.02, d)) * step(p.y, 0.45) * step(-1.0, p.y);
    float groove = smoothstep(0.82, 1.0, sin(p.y * 15.0 + p.x * 1.6));
    vec3 orange = mix(vec3(1.0, 0.55, 0.08), vec3(0.8, 0.3, 0.02), groove * 0.7);
    orange *= 1.0 - 0.35 * smoothstep(0.1, 1.0, p.x / max(half_w, 0.01));
    float leaf = max(blade(p, 0.0, 0.0), max(blade(p, -0.14, -0.35), blade(p, 0.14, 0.35)));
    vec3 col = body > 0.0 ? orange : vec3(0.2, 0.68, 0.18);
    float alpha = max(body, leaf);
    fragColor = vec4(col * alpha, alpha);
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
        let rects = self.carrot_rects();
        if rects.is_empty() {
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
        for rect in rects {
            if !target.begin(gl, 0, rect) {
                continue;
            }
            let [x0, y0, x1, y1] = rect;
            c.program.bind(gl);
            c.program.set_vec4(gl, "u_rect", [x0 / w * 2.0 - 1.0, 1.0 - y1 / h * 2.0, x1 / w * 2.0 - 1.0, 1.0 - y0 / h * 2.0]);
            c.program.set_vec4(gl, "u_clip", target.clip_xform());
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
