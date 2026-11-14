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
use super::we_model::{parse_mdl, Mesh, MeshVertex, ParsedMdl, PuppetAnimLayer};
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
uniform vec4 u_uv_rect;
out vec2 v_uv;
void main() {
    vec2 world = u_origin + a_pos.x * u_axis_x + a_pos.y * u_axis_y;
    vec2 local = world - u_min;
    gl_Position = vec4(local.x / u_canvas.x * 2.0 - 1.0,
                       1.0 - local.y / u_canvas.y * 2.0, 0.0, 1.0);
    v_uv = u_uv_rect.xy + a_uv * u_uv_rect.zw;
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

/// Composite shader for Wallpaper Engine `colorBlendMode` layers.
///
/// The scene is snapshotted first, so the pass can read the composed
/// background while writing the result back into the scene framebuffer.
/// `a_uv` addresses the layer canvas; the scene texture is sampled from the
/// quad's own screen position.
const BLEND_VS: &str = r#"#version 300 es
layout(location = 0) in vec2 a_pos;
layout(location = 1) in vec2 a_uv;
uniform vec2 u_origin;
uniform vec2 u_axis_x;
uniform vec2 u_axis_y;
uniform vec2 u_ortho;
out vec2 v_uv;
out vec2 v_scene_uv;
void main() {
    vec2 world = u_origin + a_pos.x * u_axis_x + a_pos.y * u_axis_y;
    vec2 ndc = vec2(world.x / u_ortho.x * 2.0 - 1.0,
                    1.0 - world.y / u_ortho.y * 2.0);
    gl_Position = vec4(ndc, 0.0, 1.0);
    v_uv = a_uv;
    // GL render-target textures have v = 0 at the bottom, so screen UVs map
    // straight from NDC.  Flipping here would sample the snapshot upside
    // down and paint a mirrored copy of the scene inside the layer's AABB.
    v_scene_uv = ndc * 0.5 + 0.5;
}
"#;

/// Wallpaper Engine layer blending, ported one-to-one from the engine's
/// `assets/shaders/common_blending.h`.  `b` is the background (composed scene),
/// `s` the source layer and `o` the source opacity, exactly like the
/// engine's `ApplyBlending(blendMode, background, source, opacity)`.
const BLEND_FS: &str = r#"#version 300 es
precision highp float;
in vec2 v_uv;
in vec2 v_scene_uv;
uniform sampler2D u_layer;
uniform sampler2D u_scene;
uniform int u_blend_mode;
out vec4 fragColor;

vec3 c3(float v) { return vec3(v); }

float f_linear_burn(float b, float s) { return max(b + s - 1.0, 0.0); }
float f_linear_dodge(float b, float s) { return b + s; }
float f_color_dodge(float b, float s) { return (s == 1.0) ? s : min(b / (1.0 - s), 1.0); }
float f_color_burn(float b, float s) { return (s == 0.0) ? s : max(1.0 - ((1.0 - b) / s), 0.0); }
float f_linear_light(float b, float s) { return (s < 0.5) ? f_linear_burn(b, 2.0 * s) : f_linear_dodge(b, 2.0 * (s - 0.5)); }
float f_screen(float b, float s) { return 1.0 - ((1.0 - b) * (1.0 - s)); }
float f_overlay(float b, float s) { return (b < 0.5) ? (2.0 * b * s) : (1.0 - 2.0 * (1.0 - b) * (1.0 - s)); }
float f_soft_light(float b, float s) {
    return (s < 0.5) ? (2.0 * b * s + b * b * (1.0 - 2.0 * s))
                     : (sqrt(b) * (2.0 * s - 1.0) + 2.0 * b * (1.0 - s));
}
float f_vivid_light(float b, float s) { return (s < 0.5) ? f_color_burn(b, 2.0 * s) : f_color_dodge(b, 2.0 * (s - 0.5)); }
float f_pin_light(float b, float s) { return (s < 0.5) ? min(b, 2.0 * s) : max(b, 2.0 * (s - 0.5)); }
float f_hard_mix(float b, float s) { return (f_vivid_light(b, s) < 0.5) ? 0.0 : 1.0; }
float f_reflect(float b, float s) { return (s == 1.0) ? s : min(b * b / (1.0 - s), 1.0); }

vec3 map_color_dodge(vec3 b, vec3 s) { return vec3(f_color_dodge(b.r, s.r), f_color_dodge(b.g, s.g), f_color_dodge(b.b, s.b)); }
vec3 map_color_burn(vec3 b, vec3 s) { return vec3(f_color_burn(b.r, s.r), f_color_burn(b.g, s.g), f_color_burn(b.b, s.b)); }
vec3 map_linear_light(vec3 b, vec3 s) { return vec3(f_linear_light(b.r, s.r), f_linear_light(b.g, s.g), f_linear_light(b.b, s.b)); }
vec3 map_screen(vec3 b, vec3 s) { return vec3(f_screen(b.r, s.r), f_screen(b.g, s.g), f_screen(b.b, s.b)); }
vec3 map_overlay(vec3 b, vec3 s) { return vec3(f_overlay(b.r, s.r), f_overlay(b.g, s.g), f_overlay(b.b, s.b)); }
vec3 map_soft_light(vec3 b, vec3 s) { return vec3(f_soft_light(b.r, s.r), f_soft_light(b.g, s.g), f_soft_light(b.b, s.b)); }
vec3 map_vivid_light(vec3 b, vec3 s) { return vec3(f_vivid_light(b.r, s.r), f_vivid_light(b.g, s.g), f_vivid_light(b.b, s.b)); }
vec3 map_pin_light(vec3 b, vec3 s) { return vec3(f_pin_light(b.r, s.r), f_pin_light(b.g, s.g), f_pin_light(b.b, s.b)); }
vec3 map_hard_mix(vec3 b, vec3 s) { return vec3(f_hard_mix(b.r, s.r), f_hard_mix(b.g, s.g), f_hard_mix(b.b, s.b)); }
vec3 map_reflect(vec3 b, vec3 s) { return vec3(f_reflect(b.r, s.r), f_reflect(b.g, s.g), f_reflect(b.b, s.b)); }

vec3 rgb_to_hsl(vec3 color) {
    float fmin = min(min(color.r, color.g), color.b);
    float fmax = max(max(color.r, color.g), color.b);
    float delta = fmax - fmin;
    vec3 hsl;
    hsl.z = (fmax + fmin) / 2.0;
    if (delta == 0.0) {
        hsl.x = 0.0;
        hsl.y = 0.0;
    } else {
        if (hsl.z < 0.5) hsl.y = delta / (fmax + fmin);
        else hsl.y = delta / (2.0 - fmax - fmin);
        float delta_r = (((fmax - color.r) / 6.0) + (delta / 2.0)) / delta;
        float delta_g = (((fmax - color.g) / 6.0) + (delta / 2.0)) / delta;
        float delta_b = (((fmax - color.b) / 6.0) + (delta / 2.0)) / delta;
        if (color.r == fmax) hsl.x = delta_b - delta_g;
        else if (color.g == fmax) hsl.x = (1.0 / 3.0) + delta_r - delta_b;
        else if (color.b == fmax) hsl.x = (2.0 / 3.0) + delta_g - delta_r;
        if (hsl.x < 0.0) hsl.x += 1.0;
        else if (hsl.x > 1.0) hsl.x -= 1.0;
    }
    return hsl;
}

float hue_to_rgb(float f1, float f2, float hue) {
    if (hue < 0.0) hue += 1.0;
    else if (hue > 1.0) hue -= 1.0;
    float res;
    if ((6.0 * hue) < 1.0) res = f1 + (f2 - f1) * 6.0 * hue;
    else if ((2.0 * hue) < 1.0) res = f2;
    else if ((3.0 * hue) < 2.0) res = f1 + (f2 - f1) * ((2.0 / 3.0) - hue) * 6.0;
    else res = f1;
    return res;
}

vec3 hsl_to_rgb(vec3 hsl) {
    if (hsl.y == 0.0) return c3(hsl.z);
    float f2 = (hsl.z < 0.5) ? (hsl.z * (1.0 + hsl.y)) : ((hsl.z + hsl.y) - (hsl.y * hsl.z));
    float f1 = 2.0 * hsl.z - f2;
    return vec3(hue_to_rgb(f1, f2, hsl.x + (1.0 / 3.0)),
                hue_to_rgb(f1, f2, hsl.x),
                hue_to_rgb(f1, f2, hsl.x - (1.0 / 3.0)));
}

vec3 blend_hue(vec3 b, vec3 s) {
    vec3 bh = rgb_to_hsl(b);
    return hsl_to_rgb(vec3(rgb_to_hsl(s).r, bh.g, bh.b));
}
vec3 blend_saturation(vec3 b, vec3 s) {
    vec3 bh = rgb_to_hsl(b);
    return hsl_to_rgb(vec3(bh.r, rgb_to_hsl(s).g, bh.b));
}
vec3 blend_color(vec3 b, vec3 s) {
    vec3 sh = rgb_to_hsl(s);
    return hsl_to_rgb(vec3(sh.r, sh.g, rgb_to_hsl(b).b));
}
vec3 blend_luminosity(vec3 b, vec3 s) {
    vec3 bh = rgb_to_hsl(b);
    return hsl_to_rgb(vec3(bh.r, bh.g, rgb_to_hsl(s).b));
}

vec3 we_blend(int m, vec3 b, vec3 s) {
    if (m == 1) return min(s, b);
    if (m == 2) return b * s;
    if (m == 3) return map_color_burn(b, s);
    if (m == 4) return max(b + s - 1.0, 0.0);
    if (m == 5) return min(b, s);
    if (m == 6) return max(s, b);
    if (m == 7) return map_screen(b, s);
    if (m == 8) return map_color_dodge(b, s);
    if (m == 9) return min(b + s, c3(1.0));
    if (m == 10) return max(b, s);
    if (m == 11) return map_overlay(b, s);
    if (m == 12) return map_soft_light(b, s);
    if (m == 13) return map_overlay(s, b);
    if (m == 14) return map_vivid_light(b, s);
    if (m == 15) return map_linear_light(b, s);
    if (m == 16) return map_pin_light(b, s);
    if (m == 17) return map_hard_mix(b, s);
    if (m == 18) return abs(b - s);
    if (m == 19) return b + s - 2.0 * b * s;
    if (m == 20) return max(b + s - 1.0, 0.0);
    if (m == 21) return map_reflect(b, s);
    if (m == 22) return map_reflect(s, b);
    if (m == 23) return min(b, s) - max(b, s) + c3(1.0);
    if (m == 24) return (b + s) * 0.5;
    if (m == 25) return c3(1.0) - abs(c3(1.0) - b - s);
    if (m == 26) return blend_hue(b, s);
    if (m == 27) return blend_saturation(b, s);
    if (m == 28) return blend_color(b, s);
    if (m == 29) return blend_luminosity(b, s);
    if (m == 30) return c3(max(b.r, max(b.g, b.b))) * s;
    if (m == 31) return b + s;
    if (m == 32) return b + b * s;
    return s;
}

vec3 apply_blending(int m, vec3 b, vec3 s, float o) {
    // Modes 5 and 10 ignore the opacity in the engine's header.
    if (m == 5 || m == 10) return we_blend(m, b, s);
    if (m == 31) return b + s * o;
    return mix(b, we_blend(m, b, s), o);
}

void main() {
    vec4 source = texture(u_layer, v_uv);
    vec4 background = texture(u_scene, v_scene_uv);
    vec3 rgb = apply_blending(u_blend_mode, background.rgb, source.rgb, source.a);
    fragColor = vec4(rgb, background.a);
}
"#;

const COPY_VS: &str = r#"#version 300 es
layout(location = 0) in vec2 a_pos;
layout(location = 1) in vec2 a_uv;
uniform vec2 u_uv_scale;
uniform vec2 u_uv_offset;
uniform float u_flip_y;
out vec2 v_uv;
void main() {
    v_uv = a_uv * u_uv_scale + u_uv_offset;
    gl_Position = vec4(a_pos.x, a_pos.y * u_flip_y, 0.0, 1.0);
}
"#;

const COPY_FS: &str = r#"#version 300 es
precision highp float;
in vec2 v_uv;
uniform sampler2D u_tex;
uniform vec4 u_bcs_hue; // x=brightness(-1..1), y=contrast(1), z=saturation(1), w=hue(rad)
out vec4 fragColor;
void main() {
    vec4 c = texture(u_tex, v_uv);
    c.rgb += u_bcs_hue.x;
    c.rgb = (c.rgb - 0.5) * u_bcs_hue.y + 0.5;
    float luma = dot(c.rgb, vec3(0.2126, 0.7152, 0.0722));
    c.rgb = mix(vec3(luma), c.rgb, u_bcs_hue.z);
    if (abs(u_bcs_hue.w) > 1e-4) {
        const vec3 k = vec3(0.577350269);
        float cosA = cos(u_bcs_hue.w);
        float sinA = sin(u_bcs_hue.w);
        c.rgb = c.rgb * cosA + cross(k, c.rgb) * sinA + k * dot(k, c.rgb) * (1.0 - cosA);
    }
    c.rgb = clamp(c.rgb, 0.0, 1.0);
    fragColor = c;
}
"#;

const PARTICLE_VS: &str = r#"#version 300 es
precision highp float;
layout(location = 0) in vec2 a_quad;
layout(location = 1) in vec2 a_center;  // world x, y
layout(location = 2) in vec4 a_axes;    // right.x, right.y, up.x, up.y
layout(location = 3) in vec4 a_color;
layout(location = 4) in vec4 a_uv_rect; // u0, v0, u_size, v_size
uniform vec2 u_ortho;
out vec2 v_uv;
out vec2 v_local_uv;
out vec4 v_color;
out vec2 v_screen_uv;
out vec2 v_tangent_right;
out vec2 v_tangent_up;
void main() {
    vec2 world = a_center + a_quad.x * a_axes.xy + a_quad.y * a_axes.zw;
    vec2 ndc = vec2(world.x / u_ortho.x * 2.0 - 1.0,
                    1.0 - world.y / u_ortho.y * 2.0);
    gl_Position = vec4(ndc, 0.0, 1.0);
    vec2 quad_uv = vec2(a_quad.x * 0.5 + 0.5, 0.5 - a_quad.y * 0.5);
    v_local_uv = quad_uv;
    v_uv = a_uv_rect.xy + quad_uv * a_uv_rect.zw;
    v_color = a_color;
    v_screen_uv = ndc * 0.5 + 0.5;
    float lx = max(length(a_axes.xy), 1e-4);
    float ly = max(length(a_axes.zw), 1e-4);
    v_tangent_right = a_axes.xy / lx;
    v_tangent_up = a_axes.zw / ly;
}
"#;

const PARTICLE_FS: &str = r#"#version 300 es
precision highp float;
in vec2 v_uv;
in vec2 v_local_uv;
in vec4 v_color;
in vec2 v_screen_uv;
in vec2 v_tangent_right;
in vec2 v_tangent_up;
uniform int u_shape;
uniform int u_has_tex;
uniform int u_is_r8;
uniform int u_refract;
uniform int u_has_normal;
uniform float u_refract_amount;
uniform float u_overbright;
uniform int u_premul;
uniform sampler2D u_tex;
uniform sampler2D u_scene;
uniform sampler2D u_normal;
out vec4 fragColor;
void main() {
    if (u_has_tex == 1) {
        vec4 color = texture(u_tex, v_uv);
        if (u_is_r8 == 1) {
            color = vec4(1.0, 1.0, 1.0, color.r);
        }
        color *= v_color;
        if (u_refract == 1) {
            vec2 off = vec2(0.0);
            if (u_has_normal == 1) {
                vec4 n = texture(u_normal, v_uv);
                n.xw = n.wx;
                n.xy = n.xy * 2.0 - 1.0;
                vec2 disp = v_tangent_right * n.x + v_tangent_up * n.y;
                off = vec2(disp.x, -disp.y) * u_refract_amount * n.a * v_color.a;
            }
            vec2 ruv = clamp(v_screen_uv + off, vec2(0.001), vec2(0.999));
            color.rgb *= texture(u_scene, ruv).rgb;
        }
        color.rgb *= u_overbright;
        if (u_premul == 1) {
            color.rgb *= color.a;
        }
        fragColor = color;
        return;
    }
    vec2 p = v_local_uv * 2.0 - 1.0;
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
    vec4 out_c = vec4(v_color.rgb * u_overbright, v_color.a * alpha);
    if (u_premul == 1) {
        out_c.rgb *= out_c.a;
    }
    fragColor = out_c;
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
    /// Own rule plus every ancestor's rule: all must hold.
    All(Vec<VisibleRule>),
}

fn matches_condition(actual: &serde_json::Value, condition: &str) -> bool {
    let s_actual = value_to_string(actual);
    let a_trim = s_actual.trim();
    let c_trim = condition.trim();
    if a_trim == c_trim {
        return true;
    }
    if let (Ok(na), Ok(nc)) = (a_trim.parse::<f64>(), c_trim.parse::<f64>()) {
        return (na - nc).abs() < 1e-6;
    }
    false
}

fn prop_get<'a>(
    props: &'a HashMap<String, serde_json::Value>,
    name: &str,
) -> Option<&'a serde_json::Value> {
    props.get(name).or_else(|| {
        props
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v)
    })
}

impl VisibleRule {
    fn eval(&self, props: &HashMap<String, serde_json::Value>) -> bool {
        match self {
            VisibleRule::Always => true,
            VisibleRule::Script(v) => *v,
            VisibleRule::All(rules) => rules.iter().all(|r| r.eval(props)),
            VisibleRule::User { name, condition, value } => {
                let Some(actual) = prop_get(props, name) else {
                    return *value;
                };
                if let Some(condition) = condition {
                    return matches_condition(actual, condition);
                }
                value_bool(actual).unwrap_or(*value)
            }
        }
    }
}

fn resolve_prop(
    v: &serde_json::Value,
    props: &HashMap<String, serde_json::Value>,
) -> serde_json::Value {
    if let Some(obj) = v.as_object() {
        if let Some(user) = obj.get("user") {
            match user {
                serde_json::Value::String(name) => {
                    if let Some(actual) = prop_get(props, name) {
                        return actual.clone();
                    }
                }
                serde_json::Value::Object(uobj) => {
                    if let Some(name) = uobj.get("name").and_then(|n| n.as_str()) {
                        if let Some(actual) = prop_get(props, name) {
                            if let Some(cond) = uobj.get("condition").and_then(|c| c.as_str()) {
                                return serde_json::Value::Bool(matches_condition(actual, cond));
                            }
                            return actual.clone();
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(inner) = obj.get("value") {
            if inner.is_object() {
                let resolved_inner = resolve_prop(inner, props);
                let mut out = obj.clone();
                out.insert("value".to_string(), resolved_inner);
                return serde_json::Value::Object(out);
            }
        }
    }
    v.clone()
}

fn sp_f32(
    sp: Option<&serde_json::Map<String, serde_json::Value>>,
    key: &str,
    props: &HashMap<String, serde_json::Value>,
) -> Option<f32> {
    sp?.get(key).and_then(|v| value_f32(&resolve_prop(v, props)))
}

fn sp_bool(
    sp: Option<&serde_json::Map<String, serde_json::Value>>,
    key: &str,
    props: &HashMap<String, serde_json::Value>,
) -> Option<bool> {
    sp?.get(key).and_then(|v| value_bool(&resolve_prop(v, props)))
}

fn strip_line_comments(script: &str) -> String {
    script
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn const_number(s: &str, name: &str) -> Option<f32> {
    let idx = s.find(name)? + name.len();
    let rest = s[idx..].trim_start().strip_prefix('=')?.trim_start();
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let d = edge1 - edge0;
    if d.abs() < 1e-6 {
        return if x >= edge1 { 1.0 } else { 0.0 };
    }
    let t = ((x - edge0) / d).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn parse_sp_axis_assign<'a>(line: &'a str, lhs: &str, canvas_suffix: &str) -> Option<(&'a str, bool)> {
    let code = line.split("//").next()?.trim();
    let rhs = code.strip_prefix(lhs)?.trim_start().strip_prefix('=')?.trim();
    let rhs = rhs.trim_end_matches(';').trim();
    let after = rhs.strip_prefix("scriptProperties.")?;
    let name_len = after
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '$'))
        .unwrap_or(after.len());
    if name_len == 0 {
        return None;
    }
    let prop = &after[..name_len];
    let rest = after[name_len..].trim();
    if rest.is_empty() {
        return Some((prop, false));
    }
    if let Some(mul_rhs) = rest.strip_prefix('*') {
        if mul_rhs.trim() == canvas_suffix {
            return Some((prop, true));
        }
    }
    None
}

/// Resolves static `origin` properties, including user-bound `scriptProperties`
/// assignments (`value.x = scriptProperties.x [* engine.canvasSize.x]`).
fn eval_origin_field(
    v: &serde_json::Value,
    default: [f32; 3],
    ortho: [f32; 2],
    props: &HashMap<String, serde_json::Value>,
) -> [f32; 3] {
    let mut out = value_vec(&resolve_prop(v, props), 3, default);
    let Some(script) = v.get("script").and_then(|s| s.as_str()) else {
        return out;
    };
    let sp = v.get("scriptproperties").and_then(|s| s.as_object());
    if sp.is_none() {
        return out;
    }
    let clean = strip_line_comments(script);
    let body = clean.split("function update").nth(1).unwrap_or(&clean);
    for line in body.lines() {
        if let Some((prop, mult_canvas)) = parse_sp_axis_assign(line, "value.x", "engine.canvasSize.x") {
            if let Some(val) = sp_f32(sp, prop, props) {
                out[0] = if mult_canvas { val * ortho[0] } else { val };
            }
        }
        if let Some((prop, mult_canvas)) = parse_sp_axis_assign(line, "value.y", "engine.canvasSize.y") {
            if let Some(val) = sp_f32(sp, prop, props) {
                out[1] = if mult_canvas { val * ortho[1] } else { val };
            }
        }
    }
    out
}

/// Evaluates `alpha` properties, including time-of-day fade/brightness scripts.
fn eval_alpha_field(
    v: Option<&serde_json::Value>,
    props: &HashMap<String, serde_json::Value>,
    now: text::LocalTime,
) -> f32 {
    let Some(v) = v else {
        return 1.0;
    };
    if let Some(script) = v.get("script").and_then(|s| s.as_str()) {
        let clean = strip_line_comments(script);
        let tod_hours = now.hour as f32 + now.minute as f32 / 60.0 + now.second as f32 / 3600.0;
        let engine_tod = tod_hours / 24.0;
        if let (Some(in_s), Some(in_e), Some(out_s), Some(out_e)) = (
            const_number(&clean, "FADE_IN_START_HOUR"),
            const_number(&clean, "FADE_IN_END_HOUR"),
            const_number(&clean, "FADE_OUT_START_HOUR"),
            const_number(&clean, "FADE_OUT_END_HOUR"),
        ) {
            let blend = const_number(&clean, "BLEND_DURATION").unwrap_or(0.3);
            let fallback = if clean
                .rsplit("return ")
                .next()
                .and_then(|r| r.trim_start().strip_prefix('1'))
                .is_some()
            {
                1.0
            } else {
                0.0
            };
            let a = if tod_hours >= in_e && tod_hours < out_s {
                1.0
            } else if tod_hours >= out_e && tod_hours < in_s {
                0.0
            } else if tod_hours >= in_s && tod_hours <= in_e {
                smoothstep((in_s - blend) / 24.0, in_e / 24.0, engine_tod)
            } else if tod_hours >= out_s && tod_hours <= out_e {
                1.0 - smoothstep((out_s - blend) / 24.0, out_e / 24.0, engine_tod)
            } else {
                fallback
            };
            return a.clamp(0.0, 1.0);
        }
        if let (Some(sr_s), Some(sr_e), Some(ss_s), Some(ss_e)) = (
            const_number(&clean, "SUNRISE_START"),
            const_number(&clean, "SUNRISE_END"),
            const_number(&clean, "SUNSET_START"),
            const_number(&clean, "SUNSET_END"),
        ) {
            let day_b = const_number(&clean, "DAY_BRIGHTNESS").unwrap_or(1.0);
            let night_b = const_number(&clean, "NIGHT_BRIGHTNESS").unwrap_or(0.45);
            let brightness = if tod_hours < sr_s {
                night_b
            } else if tod_hours < sr_e {
                night_b + (day_b - night_b) * smoothstep(sr_s, sr_e, tod_hours)
            } else if tod_hours < ss_s {
                day_b
            } else if tod_hours < ss_e {
                day_b - (day_b - night_b) * smoothstep(ss_s, ss_e, tod_hours)
            } else {
                night_b
            };
            let alpha = if clean.contains("darkness") || clean.contains("1.0 - brightness") {
                1.0 - brightness
            } else {
                brightness
            };
            return alpha.clamp(0.0, 1.0);
        }
    }
    let resolved = resolve_prop(v, props);
    value_f32(&resolved).unwrap_or(1.0).clamp(0.0, 1.0)
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

/// Temporary verification hook: `ANIMA_MOUSE_NDC="x y"` pins the camera mouse
/// position (NDC, -1..1) so parallax/attachment behaviour can be checked in a
/// headless capture rig.
fn debug_mouse_ndc() -> Option<(f32, f32)> {
    let raw = std::env::var("ANIMA_MOUSE_NDC").ok()?;
    let mut it = raw.split_whitespace();
    let x = it.next()?.parse::<f32>().ok()?.clamp(-1.0, 1.0);
    let y = it.next()?.parse::<f32>().ok()?.clamp(-1.0, 1.0);
    Some((x, y))
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
    if let serde_json::Value::String(s) = v {
        if s.trim().starts_with('#') {
            return parse_vec3_str(s, default);
        }
    }
    if let serde_json::Value::Object(o) = v {
        if let Some(inner) = o.get("value") {
            return parse_color(inner, default);
        }
    }
    let mut c = value_vec(v, 3, default);
    if c.iter().any(|x| *x > 1.0) {
        for x in &mut c {
            *x = (*x / 255.0).clamp(0.0, 1.0);
        }
    }
    c
}

fn eval_color_field(
    v: Option<&serde_json::Value>,
    props: &HashMap<String, serde_json::Value>,
    default: [f32; 3],
) -> [f32; 3] {
    match v {
        Some(raw) => parse_color(&resolve_prop(raw, props), default),
        None => default,
    }
}

fn eval_brightness_field(
    v: Option<&serde_json::Value>,
    props: &HashMap<String, serde_json::Value>,
    default: f32,
) -> f32 {
    match v {
        Some(raw) => value_f32(&resolve_prop(raw, props)).unwrap_or(default),
        None => default,
    }
}

fn eval_parallax_field(
    v: Option<&serde_json::Value>,
    props: &HashMap<String, serde_json::Value>,
) -> [f32; 2] {
    let Some(raw) = v else {
        return [0.0, 0.0];
    };
    let p = value_vec(&resolve_prop(raw, props), 3, [0.0, 0.0, 0.0]);
    [p[0], p[1]]
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
    rot_x: f32,
    rot_y: f32,
    ang_vel: f32,
    age: f32,
    lifetime: f32,
    frame_seed: u32,
    base_color: [f32; 3],
    color: [f32; 3],
}

#[derive(Debug, Clone)]
struct WeParticleSpec {
    transform: Affine,
    emitter_origin: [f32; 2],
    dist_min: [f32; 2],
    dist_max: [f32; 2],
    life_min: f32,
    life_max: f32,
    size_min: f32,
    size_max: f32,
    vel_min: [f32; 2],
    vel_max: [f32; 2],
    color_min: [f32; 3],
    color_max: [f32; 3],
    color_mult: [f32; 3],
    alpha_min: f32,
    alpha_max: f32,
    rot_min: [f32; 3],
    rot_max: [f32; 3],
    ang_vel_min: f32,
    ang_vel_max: f32,
    gravity: [f32; 2],
    drag: f32,
    fade_in_time: f32,
    fade_out_time: f32,
    size_change: Option<(f32, f32, f32, f32)>,
    osc_freq: f32,
    osc_scale: f32,
    trail_length: Option<(f32, f32)>,
    random_frame: bool,
    lock_to_pointer: bool,
    rng_state: u32,
}

impl WeParticleSpec {
    fn next_f32(&mut self) -> f32 {
        self.rng_state ^= self.rng_state << 13;
        self.rng_state ^= self.rng_state >> 17;
        self.rng_state ^= self.rng_state << 5;
        if self.rng_state == 0 {
            self.rng_state = 0x9E37_79B9;
        }
        (self.rng_state as f32) / (u32::MAX as f32)
    }

    fn spawn(&mut self, initial_age_frac: Option<f32>) -> Particle {
        let rx = self.next_f32() * 2.0 - 1.0;
        let ry = self.next_f32() * 2.0 - 1.0;
        let sx = if rx >= 0.0 { 1.0 } else { -1.0 };
        let sy = if ry >= 0.0 { 1.0 } else { -1.0 };
        let dx = self.dist_min[0] + rx.abs() * (self.dist_max[0] - self.dist_min[0]).max(0.0);
        let dy = self.dist_min[1] + ry.abs() * (self.dist_max[1] - self.dist_min[1]).max(0.0);
        let x = self.emitter_origin[0] + sx * dx;
        let y = self.emitter_origin[1] + sy * dy;
        let vx = self.vel_min[0] + self.next_f32() * (self.vel_max[0] - self.vel_min[0]);
        let vy = self.vel_min[1] + self.next_f32() * (self.vel_max[1] - self.vel_min[1]);
        let size = self.size_min + self.next_f32() * (self.size_max - self.size_min);
        let alpha = self.alpha_min + self.next_f32() * (self.alpha_max - self.alpha_min);
        let rot_x = self.rot_min[0] + self.next_f32() * (self.rot_max[0] - self.rot_min[0]);
        let rot_y = self.rot_min[1] + self.next_f32() * (self.rot_max[1] - self.rot_min[1]);
        let rot = self.rot_min[2] + self.next_f32() * (self.rot_max[2] - self.rot_min[2]);
        let ang_vel = self.ang_vel_min + self.next_f32() * (self.ang_vel_max - self.ang_vel_min);
        let lifetime = (self.life_min + self.next_f32() * (self.life_max - self.life_min)).max(0.1);
        let tc = self.next_f32();
        let base_color = [
            self.color_min[0] + tc * (self.color_max[0] - self.color_min[0]),
            self.color_min[1] + tc * (self.color_max[1] - self.color_min[1]),
            self.color_min[2] + tc * (self.color_max[2] - self.color_min[2]),
        ];
        let color = [
            base_color[0] * self.color_mult[0],
            base_color[1] * self.color_mult[1],
            base_color[2] * self.color_mult[2],
        ];
        let phase = self.next_f32() * std::f32::consts::TAU;
        let frame_seed = (self.next_f32() * 1024.0) as u32;
        let age = initial_age_frac.unwrap_or(0.0) * lifetime;
        Particle {
            x: x + vx * age * 0.35,
            y: y + vy * age * 0.35,
            vx,
            vy,
            size,
            alpha,
            phase,
            rot: rot + ang_vel * age,
            rot_x,
            rot_y,
            ang_vel,
            age,
            lifetime,
            frame_seed,
            base_color,
            color,
        }
    }
}

struct Emitter {
    kind: ParticleKind,
    center: [f32; 2],
    spread: [f32; 2],
    blend: LayerBlend,
    audio_reactive: bool,
    texture: Option<glow::Texture>,
    tex_is_r8: bool,
    refract: bool,
    normal_tex: Option<glow::Texture>,
    refract_amount: f32,
    overbright: f32,
    spritesheet: Option<crate::pkg::SpritesheetMeta>,
    we_spec: Option<WeParticleSpec>,
    particles: Vec<Particle>,
}

impl Emitter {
    fn from_we_preset(
        gl: &glow::Context,
        pkg: &PackageFs,
        textures: &mut TextureCache,
        preset: &serde_json::Value,
        obj: &serde_json::Value,
        transform: Affine,
        hint_kind: ParticleKind,
        props: &HashMap<String, serde_json::Value>,
    ) -> Self {
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
            .map(|c| parse_color(&resolve_prop(c, props), [1.0, 1.0, 1.0]))
            .unwrap_or([1.0, 1.0, 1.0]);

        let mut emitter_origin = [0.0f32, 0.0];
        let mut dist_min = [0.0f32, 0.0];
        let mut dist_max = [256.0f32, 256.0];
        let mut rate = 15.0f32;
        if let Some(em) = preset
            .get("emitter")
            .and_then(|e| e.as_array())
            .and_then(|a| a.first())
        {
            if let Some(orig) = em.get("origin") {
                let v = value_vec(orig, 3, [0.0, 0.0, 0.0]);
                emitter_origin = [v[0], v[1]];
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
            if let Some(r) = em.get("rate").and_then(value_f32) {
                rate = r.max(0.1);
            }
        }
        rate *= ov_rate.max(0.05);

        let mut life_min = 4.0f32;
        let mut life_max = 6.0f32;
        let mut size_min = 24.0f32;
        let mut size_max = 36.0f32;
        let mut vel_min = [-20.0f32, -20.0];
        let mut vel_max = [20.0f32, 20.0];
        let mut color_min = [1.0f32, 1.0, 1.0];
        let mut color_max = [1.0f32, 1.0, 1.0];
        let mut alpha_min = 0.6f32;
        let mut alpha_max = 1.0f32;
        let mut rot_min = [0.0f32; 3];
        let mut rot_max = [0.0f32; 3];
        let mut ang_vel_min = 0.0f32;
        let mut ang_vel_max = 0.0f32;

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
                        vel_min = [fx * avg - spread - px.abs() * spread, fy * avg - spread - py.abs() * spread];
                        vel_max = [fx * avg + spread + px.abs() * spread, fy * avg + spread + py.abs() * spread];
                    }
                    "colorrandom" => {
                        if let Some(cmin) = init.get("min") {
                            color_min = parse_color(cmin, [1.0, 1.0, 1.0]);
                        }
                        if let Some(cmax) = init.get("max") {
                            color_max = parse_color(cmax, color_min);
                        }
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
        alpha_min = (alpha_min * ov_alpha).clamp(0.0, 1.0);
        alpha_max = (alpha_max * ov_alpha).clamp(0.0, 1.0).max(alpha_min);

        let mut gravity = [0.0f32, 0.0];
        let mut drag = 0.0f32;
        let mut fade_in_time = 0.1f32;
        let mut fade_out_time = 0.8f32;
        let mut size_change = None;
        let mut osc_freq = 0.0f32;
        let mut osc_scale = 0.0f32;

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
                        fade_in_time = op.get("fadeintime").and_then(value_f32).unwrap_or(0.1).clamp(0.0, 1.0);
                        fade_out_time = op.get("fadeouttime").and_then(value_f32).unwrap_or(0.8).clamp(0.0, 1.0);
                    }
                    "sizechange" => {
                        let st = op.get("starttime").and_then(value_f32).unwrap_or(0.0);
                        let et = op.get("endtime").and_then(value_f32).unwrap_or(1.0);
                        let sv = op.get("startvalue").and_then(value_f32).unwrap_or(1.0);
                        let ev = op.get("endvalue").and_then(value_f32).unwrap_or(1.0);
                        size_change = Some((st, et, sv, ev));
                    }
                    "oscillateposition" => {
                        osc_freq = op
                            .get("frequencymax")
                            .or_else(|| op.get("frequencymin"))
                            .and_then(value_f32)
                            .unwrap_or(1.5);
                        osc_scale = op
                            .get("scalemax")
                            .or_else(|| op.get("scalemin"))
                            .and_then(value_f32)
                            .unwrap_or(8.0)
                            * ov_speed;
                    }
                    "turbulence" => {
                        let smin = op.get("speedmin").and_then(value_f32).unwrap_or(20.0);
                        let smax = op.get("speedmax").and_then(value_f32).unwrap_or(50.0);
                        osc_freq = osc_freq.max(1.2);
                        osc_scale = osc_scale.max((smin + smax) * 0.12 * ov_speed);
                    }
                    _ => {}
                }
            }
        }

        let mut trail_length = None;
        if let Some(rends) = preset.get("renderer").and_then(|r| r.as_array()) {
            for r in rends {
                if r.get("name").and_then(|v| v.as_str()) == Some("spritetrail") {
                    let len = r.get("length").and_then(value_f32).unwrap_or(0.05);
                    let maxlen = r.get("maxlength").and_then(value_f32).unwrap_or(100.0);
                    trail_length = Some((len, maxlen));
                }
            }
        }

        let random_frame = preset
            .get("animationmode")
            .and_then(|v| v.as_str())
            .map(|s| s.eq_ignore_ascii_case("randomframe"))
            .unwrap_or(false);
        let lock_to_pointer = preset
            .get("controlpoint")
            .and_then(|c| c.as_array())
            .is_some_and(|cps| {
                cps.iter().any(|cp| {
                    let id = cp.get("id").and_then(|v| v.as_i64()).unwrap_or(-1);
                    let flags = cp.get("flags").and_then(|v| v.as_u64()).unwrap_or(0);
                    id == 0 && (flags & 1) != 0
                })
            });

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
        if color_mult[0] + color_mult[1] + color_mult[2] < 0.25 && blend == LayerBlend::Additive {
            blend = LayerBlend::Normal;
        }

        let raw_max = preset
            .get("maxcount")
            .and_then(value_f32)
            .unwrap_or(60.0);
        let max_count = ((raw_max * ov_count).round() as usize).clamp(1, 500);
        let avg_life = (life_min + life_max) * 0.5;
        let steady_count = ((rate * avg_life).ceil() as usize).clamp(1, max_count);
        let count = if hint_kind == ParticleKind::Fog {
            steady_count.min(32)
        } else {
            steady_count
        };

        let mut spec = WeParticleSpec {
            transform,
            emitter_origin,
            dist_min,
            dist_max,
            life_min,
            life_max,
            size_min,
            size_max,
            vel_min,
            vel_max,
            color_min,
            color_max,
            color_mult,
            alpha_min,
            alpha_max,
            rot_min,
            rot_max,
            ang_vel_min,
            ang_vel_max,
            gravity,
            drag,
            fade_in_time,
            fade_out_time,
            size_change,
            osc_freq,
            osc_scale,
            trail_length,
            random_frame,
            lock_to_pointer,
            rng_state: 0x9E37_79B9u32
                ^ (count as u32)
                ^ (transform.tx.to_bits())
                ^ (transform.ty.to_bits()),
        };

        let mut particles = Vec::with_capacity(count);
        for i in 0..count {
            let frac = (i as f32 + 0.5) / (count as f32);
            particles.push(spec.spawn(Some(frac)));
        }

        Self {
            kind: hint_kind,
            center: [transform.tx, transform.ty],
            spread: dist_max,
            blend,
            audio_reactive: false,
            texture,
            tex_is_r8,
            refract,
            normal_tex,
            refract_amount,
            overbright,
            spritesheet,
            we_spec: Some(spec),
            particles,
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
                rot_x: 0.0,
                rot_y: 0.0,
                ang_vel: 0.0,
                age: 0.0,
                lifetime: 1.0,
                frame_seed: 0,
                base_color: color.unwrap_or(default_col),
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
            texture: None,
            tex_is_r8: false,
            refract: false,
            normal_tex: None,
            refract_amount: 0.05,
            overbright: 1.0,
            spritesheet: None,
            we_spec: None,
            particles,
        }
    }

    fn update_color_mult(&mut self, color_mult: [f32; 3]) {
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

    fn update_and_pack(
        &mut self,
        dt: f32,
        time: f32,
        ortho: [f32; 2],
        audio: [f32; 4],
        offset: [f32; 2],
        pointer_world: [f32; 2],
        out: &mut Vec<f32>,
    ) {
        out.clear();
        let boost = if self.audio_reactive {
            1.0 + audio[0] * 2.0 + audio[3]
        } else {
            1.0
        };
        let dtc = dt.clamp(0.0, 0.1) * boost;

        if let Some(spec) = &mut self.we_spec {
            if spec.lock_to_pointer {
                spec.transform.tx = pointer_world[0];
                spec.transform.ty = pointer_world[1];
            }
            let t_scale_x = spec.transform.a.hypot(spec.transform.b);
            let t_scale_y = spec.transform.c.hypot(spec.transform.d);
            let t_scale = ((t_scale_x + t_scale_y) * 0.5).max(0.01);
            let t_rot = spec.transform.b.atan2(spec.transform.a);

            for p in &mut self.particles {
                p.age += dtc;
                if p.age >= p.lifetime {
                    *p = spec.spawn(None);
                }
                let damp = (1.0 - spec.drag * dtc).clamp(0.0, 1.0);
                p.vx = p.vx * damp + spec.gravity[0] * dtc;
                p.vy = p.vy * damp + spec.gravity[1] * dtc;
                let osc_x = if spec.osc_scale > 0.0 {
                    (time * spec.osc_freq + p.phase).sin() * spec.osc_scale
                } else {
                    0.0
                };
                let osc_y = if spec.osc_scale > 0.0 {
                    (time * spec.osc_freq * 0.8 + p.phase).cos() * spec.osc_scale * 0.5
                } else {
                    0.0
                };
                p.x += (p.vx + osc_x) * dtc;
                p.y += (p.vy + osc_y) * dtc;
                p.rot += p.ang_vel * dtc;

                let t_norm = (p.age / p.lifetime).clamp(0.0, 1.0);
                let mut alpha = p.alpha;
                if spec.fade_in_time > 0.0 && t_norm < spec.fade_in_time {
                    alpha *= t_norm / spec.fade_in_time;
                }
                if spec.fade_out_time < 1.0 && t_norm > spec.fade_out_time {
                    alpha *= (1.0 - t_norm) / (1.0 - spec.fade_out_time).max(1e-4);
                }

                let mut size_mult = 1.0f32;
                if let Some((st, et, sv, ev)) = spec.size_change {
                    if t_norm <= st {
                        size_mult = sv;
                    } else if t_norm >= et {
                        size_mult = ev;
                    } else {
                        let u = (t_norm - st) / (et - st).max(1e-4);
                        size_mult = sv + u * (ev - sv);
                    }
                }

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
                        let col = fidx % cols;
                        let row = fidx / cols;
                        let u_size = 1.0 / (cols as f32);
                        let v_size = 1.0 / (rows as f32);
                        let u0 = (col as f32) * u_size;
                        let v0 = (row as f32) * v_size;
                        ([u0, v0, u_size, v_size], 1.0f32)
                    } else {
                        ([0.0, 0.0, 1.0, 1.0], 1.0f32)
                    }
                } else {
                    ([0.0, 0.0, 1.0, 1.0], 1.0f32)
                };

                let (axis_x, axis_y) = if let Some((t_len, t_max)) = spec.trail_length {
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
                    let r_local = [(cz * cy - sz * sx * sy) * unscaled_r * aspect, -sz * cx * unscaled_r];
                    let u_local = [(sz * cy + cz * sx * sy) * unscaled_r, cz * cx * unscaled_r];
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
                    p.color[0],
                    p.color[1],
                    p.color[2],
                    alpha.clamp(0.0, 1.0),
                    uv_rect[0],
                    uv_rect[1],
                    uv_rect[2],
                    uv_rect[3],
                ]);
            }
            return;
        }

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
            let world_x = self.center[0] + p.x * sx + offset[0];
            let world_y = self.center[1] + p.y * sy + offset[1];
            let (w, h) = match self.kind {
                ParticleKind::Rain => (p.size * 0.35 * ortho[1], p.size * 4.8 * ortho[1]),
                ParticleKind::Sakura => (p.size * 1.3 * ortho[1], p.size * 0.8 * ortho[1]),
                ParticleKind::Fog => (p.size * ortho[0] * 0.5, p.size * ortho[1] * 0.5),
                _ => (p.size * ortho[1], p.size * ortho[1]),
            };
            let (s, c) = p.rot.sin_cos();
            let hw = w * 0.5;
            let hh = h * 0.5;
            out.extend_from_slice(&[
                world_x,
                world_y,
                c * hw,
                s * hw,
                -s * hh,
                c * hh,
                p.color[0],
                p.color[1],
                p.color[2],
                (p.alpha * twinkle).clamp(0.0, 1.0),
                0.0,
                0.0,
                1.0,
                1.0,
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
}

/// Wallpaper Engine `colorBlendMode` of an object (`0` = material blend only).
fn object_color_blend(obj: &serde_json::Value) -> i32 {
    obj.get("colorBlendMode")
        .and_then(|v| v.as_i64())
        .unwrap_or(0) as i32
}

/// Material-level blend for an object.
///
/// `colorBlendMode` 31 is plain additive (`background + source * alpha`) and is
/// exactly what fixed-function additive blending does, so it stays on the fast
/// path.  Every other colour blend mode composites the finished layer through
/// the Wallpaper Engine blending pass instead (see `draw_layer_canvas`), so the
/// material blend only describes how the object draws into its own canvas.
fn object_blend(material: &str, color_blend: i32) -> LayerBlend {
    if color_blend == 31 {
        LayerBlend::Additive
    } else {
        LayerBlend::from_material(material)
    }
}

struct TextLayer {
    script: Option<String>,
    authored: String,
    raw_text_def: serde_json::Value,
    props: HashMap<String, serde_json::Value>,
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
        tex_size: [u32; 2],
        bounds: ([f32; 2], [f32; 2]),
    },
    Text(Box<TextLayer>),
    Solid,
    Particle(usize),
    Compose {
        copy_background: bool,
    },
    CompositeRef(i64),
    Post,
}

struct SceneObject {
    id: Option<i64>,
    name: String,
    transform: Affine,
    /// Local (unscaled) layer size in scene units.
    size: [f32; 2],
    /// Normalized anchor shift (`-0.5..=0.5`) from Wallpaper Engine's
    /// `alignment` field: non-zero shifts the quad relative to `origin`.
    align: [f32; 2],
    color: [f32; 4],
    brightness: f32,
    blend: LayerBlend,
    /// Wallpaper Engine `colorBlendMode`: how the finished layer is composited
    /// onto the scene.  `0` keeps the material blend, `31` is additive, and
    /// every other value runs through the generic WE blending pass.
    color_blend: i32,
    parallax: [f32; 2],
    visible: VisibleRule,
    dependencies: Vec<i64>,
    capture_composite: bool,
    effects: Vec<EffectInstance>,
    kind: ObjectKind,
    /// Keyframe / script driven transform and alpha.
    anim: Option<Box<ObjAnim>>,
    /// Animated texture (TEXS spritesheet or decoded MP4) for image layers.
    sprite: Option<crate::pkg::SpritesheetMeta>,
    /// Set when this layer is parented to a puppet bone (`attachment`).
    attach: Option<AttachPoint>,
    /// Index into `SceneRenderer::raw_nodes` and raw `scene.json` object definition
    /// so user property changes can re-evaluate transforms, colors, and text live.
    raw_idx: Option<usize>,
    raw_obj: serde_json::Value,
}

/// A layer that follows a bone of its parent puppet, possibly animated.
#[derive(Clone)]
struct AttachPoint {
    /// Index of the parent puppet object in `SceneRenderer::objects`.
    parent: usize,
    /// Bone name from the scene's `attachment` field.
    bone: String,
    /// The layer's local transform (translation * rotation * scale) in
    /// attachment space.
    local: Affine,
}

// ---------------------------------------------------------------------------
// Object property animation (keyframes + simple time scripts)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct KeyTrack {
    fps: f32,
    length: f32,
    mode: u8, // 0 loop, 1 mirror, 2 single
    relative: bool,
    curves: Vec<Vec<(f32, f32)>>,
}

impl KeyTrack {
    fn parse(v: &serde_json::Value) -> Option<Self> {
        let anim = v.get("animation")?.as_object()?;
        let opts = anim.get("options").and_then(|o| o.as_object());
        let fps = opts.and_then(|o| o.get("fps")).and_then(value_f32).unwrap_or(30.0).max(1.0);
        let length = opts.and_then(|o| o.get("length")).and_then(value_f32).unwrap_or(60.0).max(1.0);
        let mode = match opts.and_then(|o| o.get("mode")).and_then(|m| m.as_str()) {
            Some("mirror") => 1,
            Some("single") => 2,
            _ => 0,
        };
        let relative = anim.get("relative").and_then(value_bool).unwrap_or(false);
        let mut curves = Vec::new();
        for key in ["c0", "c1", "c2"] {
            let mut pts: Vec<(f32, f32)> = anim
                .get(key)
                .and_then(|c| c.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|k| Some((k.get("frame").and_then(value_f32)?, k.get("value").and_then(value_f32)?)))
                        .collect()
                })
                .unwrap_or_default();
            if pts.is_empty() {
                break;
            }
            pts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
            curves.push(pts);
        }
        if curves.is_empty() {
            return None;
        }
        Some(Self { fps, length, mode, relative, curves })
    }

    fn eval(&self, time: f32) -> [f32; 3] {
        let raw = time * self.fps;
        let frame = match self.mode {
            1 => {
                let period = self.length * 2.0;
                let m = raw.rem_euclid(period);
                if m > self.length { period - m } else { m }
            }
            2 => raw.clamp(0.0, self.length),
            _ => raw.rem_euclid(self.length),
        };
        let mut out = [0.0f32; 3];
        for (i, pts) in self.curves.iter().enumerate().take(3) {
            out[i] = if frame <= pts[0].0 {
                pts[0].1
            } else {
                let mut v = pts.last().map(|p| p.1).unwrap_or(0.0);
                for w in pts.windows(2) {
                    let ((f0, v0), (f1, v1)) = (w[0], w[1]);
                    if frame <= f1 {
                        let t = ((frame - f0) / (f1 - f0).max(1e-5)).clamp(0.0, 1.0);
                        let s = t * t * (3.0 - 2.0 * t);
                        v = v0 + (v1 - v0) * s;
                        break;
                    }
                }
                v
            };
        }
        if self.curves.len() == 1 {
            out[1] = out[0];
            out[2] = out[0];
        }
        out
    }

    fn apply(&self, base: [f32; 3], time: f32) -> [f32; 3] {
        let v = self.eval(time);
        if self.relative {
            [base[0] + v[0], base[1] + v[1], base[2] + v[2]]
        } else {
            v
        }
    }
}

/// Tiny evaluator for common time-driven property scripts (clock hands,
/// second progress bars, continuous spin, and wave oscillations).
#[derive(Debug, Clone)]
enum PropScript {
    /// `value.<c> = [Math.floor](engine.timeOfDay * mul) % modulo * factor`
    TimeOfDay { comp: usize, mul: f32, modulo: f32, factor: f32, floor: bool },
    /// `value.<c> = new Date().getSeconds() / div`
    Seconds { comp: usize, div: f32 },
    /// `value.<c> = [0 -] (engine.runtime * speed)` (degrees/sec)
    SpinZ { comp: usize, speed: f32 },
    /// Horizontal sinusoidal / triangular oscillation (`WaveOriginX`).
    WaveOriginX { intensity: f32, speed: f32, smoothness: f32, invert: bool },
}

fn number_after(s: &str, pat: &str) -> Option<f32> {
    let idx = s.find(pat)? + pat.len();
    let rest = s[idx..].trim_start();
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

impl PropScript {
    fn parse(field: &serde_json::Value, props: &HashMap<String, serde_json::Value>) -> Option<Self> {
        let script = field.get("script").and_then(|s| s.as_str())?;
        let clean = strip_line_comments(script);
        let body = clean.split("function update").nth(1).unwrap_or(&clean);
        let sp = field.get("scriptproperties").and_then(|s| s.as_object());
        if body.contains("offsetX")
            && body.contains("Math.sin")
            && clean.contains("smoothness")
            && clean.contains("intensity")
        {
            let intensity = sp_f32(sp, "intensity", props).unwrap_or(50.0);
            let speed = sp_f32(sp, "speed", props).unwrap_or(1.0);
            let smoothness = sp_f32(sp, "smoothness", props).unwrap_or(1.0).clamp(0.0, 1.0);
            let invert = sp_bool(sp, "invert", props).unwrap_or(false);
            return Some(PropScript::WaveOriginX {
                intensity,
                speed,
                smoothness,
                invert,
            });
        }
        let comp = if body.contains("value.x") {
            0
        } else if body.contains("value.y") {
            1
        } else if body.contains("value.z") {
            2
        } else {
            return None;
        };
        if body.contains("engine.timeOfDay") {
            let mul = number_after(body, "engine.timeOfDay *")?;
            let tail = &body[body.find("engine.timeOfDay")?..];
            let modulo = number_after(tail, "%").unwrap_or(0.0);
            let factor = tail
                .find('%')
                .and_then(|p| {
                    let after = &tail[p + 1..];
                    let star = after.find('*')?;
                    let semi = after.find(';').unwrap_or(after.len());
                    (star < semi).then(|| number_after(after, "*")).flatten()
                })
                .unwrap_or(1.0);
            return Some(PropScript::TimeOfDay {
                comp,
                mul,
                modulo,
                factor,
                floor: body.contains("Math.floor"),
            });
        }
        if body.contains("getSeconds()") {
            let div = number_after(body, "getSeconds() /").unwrap_or(1.0).max(1e-3);
            return Some(PropScript::Seconds { comp, div });
        }
        if body.contains("engine.runtime *")
            && !clean.contains("MediaPlaybackEvent")
            && !clean.contains("mediaPlaybackChanged")
            && !clean.contains("frametime")
        {
            let rt_pos = body.find("engine.runtime *")?;
            let mut speed = number_after(&body[rt_pos..], "engine.runtime *")?;
            let before = body[..rt_pos].trim_end().trim_end_matches('(').trim_end();
            if before.ends_with('-') {
                speed = -speed;
            }
            return Some(PropScript::SpinZ { comp, speed });
        }
        None
    }

    fn apply(&self, mut v: [f32; 3], time: f32, daytime: f32, now: text::LocalTime) -> [f32; 3] {
        match *self {
            PropScript::TimeOfDay { comp, mul, modulo, factor, floor } => {
                let mut x = daytime * mul + 1e-6;
                if floor {
                    x = x.floor();
                }
                if modulo > 0.0 {
                    x = x.rem_euclid(modulo);
                }
                v[comp] = x * factor;
            }
            PropScript::Seconds { comp, div } => v[comp] = now.second as f32 / div,
            PropScript::SpinZ { comp, speed } => v[comp] = (speed * time).rem_euclid(360.0),
            PropScript::WaveOriginX {
                intensity,
                speed,
                smoothness,
                invert,
            } => {
                let elapsed = time * speed;
                let phase = (elapsed / std::f32::consts::TAU).rem_euclid(1.0);
                let triangle = if phase < 0.5 {
                    phase * 4.0 - 1.0
                } else {
                    3.0 - phase * 4.0
                };
                let sine = elapsed.sin();
                let mut wave = triangle + (sine - triangle) * smoothness;
                if invert {
                    wave = -wave;
                }
                v[0] += wave * intensity;
            }
        }
        v
    }
}

struct ObjAnim {
    parent: Affine,
    origin: [f32; 3],
    scale: [f32; 3],
    angles: [f32; 3],
    origin_track: Option<KeyTrack>,
    scale_track: Option<KeyTrack>,
    angles_track: Option<KeyTrack>,
    alpha_track: Option<KeyTrack>,
    base_alpha: f32,
    origin_script: Option<PropScript>,
    scale_script: Option<PropScript>,
    angles_script: Option<PropScript>,
}

impl ObjAnim {
    fn parse(
        obj: &serde_json::Value,
        parent: Affine,
        origin: [f32; 3],
        scale: [f32; 3],
        angles: [f32; 3],
        props: &HashMap<String, serde_json::Value>,
        now: text::LocalTime,
    ) -> Option<Box<Self>> {
        let track = |k: &str| obj.get(k).and_then(KeyTrack::parse);
        let script = |k: &str| obj.get(k).and_then(|v| PropScript::parse(v, props));
        let a = ObjAnim {
            parent,
            origin,
            scale,
            angles,
            origin_track: track("origin"),
            scale_track: track("scale"),
            angles_track: track("angles"),
            alpha_track: track("alpha"),
            base_alpha: eval_alpha_field(obj.get("alpha"), props, now),
            origin_script: script("origin"),
            scale_script: script("scale"),
            angles_script: script("angles"),
        };
        let any = a.origin_track.is_some()
            || a.scale_track.is_some()
            || a.angles_track.is_some()
            || a.alpha_track.is_some()
            || a.origin_script.is_some()
            || a.scale_script.is_some()
            || a.angles_script.is_some();
        any.then(|| Box::new(a))
    }

    fn eval_local(&self, time: f32, daytime: f32, now: text::LocalTime) -> (Affine, Option<f32>) {
        let mut origin = self.origin;
        let mut scale = self.scale;
        let mut angles = self.angles;
        if let Some(t) = &self.origin_track {
            origin = t.apply(origin, time);
        }
        if let Some(t) = &self.scale_track {
            scale = t.apply(scale, time);
        }
        if let Some(t) = &self.angles_track {
            angles = t.apply(angles, time);
        }
        if let Some(s) = &self.origin_script {
            origin = s.apply(origin, time, daytime, now);
        }
        if let Some(s) = &self.scale_script {
            scale = s.apply(scale, time, daytime, now);
        }
        if let Some(s) = &self.angles_script {
            // Scripts use degrees; scene.json stores radians.
            let deg = s.apply([0.0; 3], time, daytime, now);
            angles[2] = deg[2].to_radians();
        }
        let local = Affine::translate(origin[0], origin[1])
            .mul(Affine::rotate(angles[2]))
            .mul(Affine::scale(scale[0], scale[1]));
        let alpha = self
            .alpha_track
            .as_ref()
            .map(|t| t.apply([self.base_alpha; 3], time)[0].clamp(0.0, 1.0));
        (local, alpha)
    }

    fn eval(&self, time: f32, daytime: f32, now: text::LocalTime) -> (Affine, Option<f32>) {
        let (local, alpha) = self.eval_local(time, daytime, now);
        (self.parent.mul(local), alpha)
    }
}

impl SceneObject {
    fn is_visible(&self, props: &HashMap<String, serde_json::Value>) -> bool {
        self.visible.eval(props)
    }

    fn local_center(&self) -> [f32; 2] {
        let mut cx = self.align[0] * self.size[0];
        let mut cy = self.align[1] * self.size[1];
        if let ObjectKind::Text(layer) = &self.kind {
            if !layer.limit_width {
                let dx = layer.world_size[0] - layer.box_size[0];
                let dy = layer.world_size[1] - layer.box_size[1];
                match layer.h_align {
                    text::HAlign::Left => cx += dx * 0.5,
                    text::HAlign::Right => cx -= dx * 0.5,
                    text::HAlign::Center => {}
                }
                match layer.v_align {
                    text::VAlign::Bottom => cy += dy * 0.5,
                    text::VAlign::Top => cy -= dy * 0.5,
                    text::VAlign::Center => {}
                }
            }
        }
        [cx, cy]
    }

    fn quad_transform(&self, mut world_t: Affine) -> Affine {
        let [cx, cy] = self.local_center();
        if cx != 0.0 || cy != 0.0 {
            let [wx, wy] = world_t.apply_vec([cx, cy]);
            world_t.tx += wx;
            world_t.ty += wy;
        }
        world_t
    }

    fn aabb(&self, offset: [f32; 2]) -> ([f32; 2], [f32; 2]) {
        let transform = self.quad_transform(Affine {
            tx: self.transform.tx + offset[0],
            ty: self.transform.ty + offset[1],
            ..self.transform
        });
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
            ObjectKind::Particle(_)
            | ObjectKind::Compose { .. }
            | ObjectKind::CompositeRef(_)
            | ObjectKind::Post => DrawKind::None,
        };
        DrawInfo {
            kind,
            size: self.size,
            color: self.color,
            brightness: self.brightness,
            uv_rect: [0.0, 0.0, 1.0, 1.0],
            premul: false,
        }
    }

    /// Current frame rectangle of an animated texture.
    fn sprite_rect(&self, time: f32) -> [f32; 4] {
        let Some(ss) = self.sprite else {
            return [0.0, 0.0, 1.0, 1.0];
        };
        let frames = ss.frames.max(1);
        let cols = ss.cols.max(1);
        let rows = ss.rows.max(1);
        let t = (time / ss.duration.max(0.01)).rem_euclid(1.0);
        let idx = ((t * frames as f32) as u32).min(frames - 1);
        let (us, vs) = (1.0 / cols as f32, 1.0 / rows as f32);
        [(idx % cols) as f32 * us, (idx / cols) as f32 * vs, us, vs]
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
    /// Texture sub-rectangle (u0, v0, u_size, v_size) for animated textures.
    uv_rect: [f32; 4],
    /// Premultiply colour by alpha (screen / multiply blending).
    premul: bool,
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

/// A Wallpaper Engine scene `sound` layer waiting to be started.
struct SoundSpec {
    files: Vec<std::path::PathBuf>,
    base_volume: f32,
    shuffle: bool,
}

/// A running scene sound (one libmpv instance per sound layer).
struct SceneSound {
    player: super::video::AudioPlayer,
    base_volume: f32,
}

pub struct SceneRenderer {
    content_prog: Program,
    puppet_prog: Program,
    composite_prog: Program,
    blend_prog: Program,
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
    /// Read-only snapshot of the composed scene, used by `colorBlendMode`
    /// layers that need to sample the background they are composited onto.
    scene_copy: Option<Canvas>,
    composites: HashMap<i64, Canvas>,
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
    /// Horizontal offset applied when a portrait scene is widened to the
    /// output aspect ratio (see `load_we_scene`).  Reset for every scene so a
    /// landscape wallpaper loaded afterwards is not shifted.
    view_shift: f32,

    objects: Vec<SceneObject>,
    emitters: Vec<Emitter>,
    /// Wallpaper Engine scene `sound` layers, started on the first frame so the
    /// backend's mute/volume settings are applied before playback begins.
    sound_specs: Vec<SoundSpec>,
    sounds: Vec<SceneSound>,
    sound_temp_dir: Option<std::path::PathBuf>,
    sounds_started: bool,
    volume: f32,
    muted: bool,
    bcs_hue: [f32; 4],
    scene_general: Option<serde_json::Value>,
    authored_ortho: [f32; 2],
    authored_prop_keys: std::collections::HashSet<String>,
    raw_nodes: Vec<(Option<usize>, serde_json::Value)>,
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
        let blend_prog = Program::new(gl, BLEND_VS, BLEND_FS)?;
        let copy_prog = Program::new(gl, COPY_VS, COPY_FS)?;
        let particle_prog = Program::new(gl, PARTICLE_VS, PARTICLE_FS)?;
        let (quad_vao, quad_vbo) = create_quad(gl)?;
        let (particle_vao, particle_quad_vbo, particle_inst_vbo) = create_particle_buffers(gl)?;

        let mut this = Self {
            content_prog,
            puppet_prog,
            composite_prog,
            blend_prog,
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
            scene_copy: None,
            composites: HashMap::new(),
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
            view_shift: 0.0,
            objects: Vec::new(),
            emitters: Vec::new(),
            sound_specs: Vec::new(),
            sounds: Vec::new(),
            sound_temp_dir: None,
            sounds_started: false,
            volume: 1.0,
            muted: false,
            bcs_hue: [0.0, 1.0, 1.0, 0.0],
            scene_general: None,
            authored_ortho: [1920.0, 1080.0],
            authored_prop_keys: std::collections::HashSet::new(),
            raw_nodes: Vec::new(),
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
                    this.authored_prop_keys.insert(k.to_ascii_lowercase());
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
    ) -> Option<(glow::Texture, u32, u32, Option<crate::pkg::SpritesheetMeta>)> {
        let mut candidates = Vec::new();
        // Texture names may legitimately contain (and even end with) spaces —
        // e.g. `materials/Miles & Gwen Parts 1 .tex`.  Try the raw name first,
        // then the trimmed spelling for sources that pad names.
        let raw = path.trim_start_matches('/');
        let mut names = vec![raw.to_string()];
        let trimmed = raw.trim();
        if trimmed != raw {
            names.push(trimmed.to_string());
        }
        for name in names {
            candidates.push(name.clone());
            let no_ext = name
                .strip_suffix(".tex")
                .or_else(|| name.strip_suffix(".png"))
                .or_else(|| name.strip_suffix(".jpg"))
                .unwrap_or(&name);
            candidates.push(format!("materials/{no_ext}.tex"));
            candidates.push(format!("{no_ext}.tex"));
            if !no_ext.starts_with("materials/") {
                candidates.push(format!("materials/{no_ext}.tex"));
            }
        }
        for candidate in candidates {
            if candidate.is_empty() {
                continue;
            }
            if let Some((t, w, h, ss, _)) = textures.get_spritesheet(gl, pkg, &candidate) {
                return Some((t, w, h, ss));
            }
        }
        None
    }

    fn load_we_scene(&mut self, gl: &glow::Context, pkg: &PackageFs, json_text: &str) -> Result<()> {
        let root: serde_json::Value =
            serde_json::from_str(json_text).context("parsing Wallpaper Engine scene.json")?;

        // Per-scene camera state: `view_shift` is only set by the portrait
        // widening branch below, so it must never leak in from a previous load.
        self.view_shift = 0.0;
        self.sounds.clear();
        self.sound_specs.clear();
        self.sounds_started = false;
        self.scene_general = root.get("general").cloned();
        self.raw_nodes.clear();

        if let Some(general) = root.get("general") {
            if let Some(ortho) = general.get("orthogonalprojection") {
                let w = ortho.get("width").and_then(value_f32).unwrap_or(1920.0);
                let h = ortho.get("height").and_then(value_f32).unwrap_or(1080.0);
                self.ortho_size = [w.max(1.0), h.max(1.0)];
            }
            if let Some(cc) = general.get("clearcolor") {
                self.clear_color = eval_color_field(Some(cc), &self.properties, [0.04, 0.05, 0.08]);
            }
            if let Some(v) = general
                .get("cameraparallax")
                .map(|v| resolve_prop(v, &self.properties))
                .and_then(|v| value_bool(&v))
            {
                self.parallax_enabled = v;
            }
            if let Some(v) = general
                .get("cameraparallaxamount")
                .map(|v| resolve_prop(v, &self.properties))
                .and_then(|v| value_f32(&v))
            {
                self.parallax_amount = v;
            }
            if let Some(v) = general
                .get("cameraparallaxmouseinfluence")
                .map(|v| resolve_prop(v, &self.properties))
                .and_then(|v| value_f32(&v))
            {
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
            origin: [f32; 3],
            scale: [f32; 3],
            angles: [f32; 3],
        }
        let mut raws = Vec::with_capacity(objects.len());
        let mut referenced_composites = std::collections::HashSet::new();
        for obj in &objects {
            if let Some(deps) = obj.get("dependencies").and_then(|d| d.as_array()) {
                for d in deps {
                    if let Some(id) = d.as_i64() {
                        referenced_composites.insert(id);
                    }
                }
            }
            collect_composite_refs(obj, &mut referenced_composites);

            let parent = obj
                .get("parent")
                .and_then(|p| {
                    p.as_str()
                        .map(str::to_string)
                        .or_else(|| p.as_i64().map(|n| n.to_string()))
                });
            let default_origin = if parent.is_some() {
                [0.0, 0.0, 0.0]
            } else {
                [self.ortho_size[0] * 0.5, self.ortho_size[1] * 0.5, 0.0]
            };
            let origin = obj
                .get("origin")
                .map(|v| eval_origin_field(v, default_origin, self.ortho_size, &self.properties))
                .unwrap_or(default_origin);
            let scale = obj
                .get("scale")
                .map(|v| value_vec(&resolve_prop(v, &self.properties), 3, [1.0, 1.0, 1.0]))
                .unwrap_or([1.0, 1.0, 1.0]);
            let angles = obj
                .get("angles")
                .map(|v| value_vec(&resolve_prop(v, &self.properties), 3, [0.0, 0.0, 0.0]))
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
                origin,
                scale,
                angles,
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

        // Portrait ("phone") scenes on a landscape output: instead of cropping
        // ~70% of the height away, keep the full authored height and widen the
        // camera around the scene centre.  Authors of such wallpapers often
        // place side backdrops / pillarbox layers outside `0..width` for this.
        let authored_ortho = self.ortho_size;
        self.authored_ortho = authored_ortho;
        if authored_ortho[1] > authored_ortho[0] && self.width > self.height && self.height > 0 {
            let win_aspect = self.width as f32 / self.height as f32;
            let view_w = authored_ortho[1] * win_aspect;
            let shift = (view_w - authored_ortho[0]) * 0.5;
            let root = Affine::translate(shift, 0.0);
            for t in resolved.iter_mut().flatten() {
                *t = root.mul(*t);
            }
            self.view_shift = shift;
            self.ortho_size = [view_w, authored_ortho[1]];
            self.clear_color = [0.0, 0.0, 0.0];
        }
        let root_transform = Affine::translate(self.view_shift, 0.0);

        let now = text::local_time();
        let parent_index = |i: usize| -> Option<usize> {
            raws[i].parent.as_ref().and_then(|p| id_index.get(p).copied())
        };
        self.raw_nodes = raws
            .iter()
            .enumerate()
            .map(|(idx, r)| (parent_index(idx), r.obj.clone()))
            .collect();
        // Visibility is inherited: a hidden group hides all of its children
        // (WE groups are often empty "image": null containers).
        let own_visible: Vec<VisibleRule> = raws
            .iter()
            .map(|r| parse_visible(r.obj.get("visible"), &self.properties, now))
            .collect();
        let inherited_visible = |i: usize| -> VisibleRule {
            let mut rules: Vec<VisibleRule> = Vec::new();
            let mut cur = Some(i);
            let mut depth = 0;
            while let Some(c) = cur {
                if depth > 32 {
                    break;
                }
                if !matches!(own_visible[c], VisibleRule::Always) {
                    rules.push(own_visible[c].clone());
                }
                cur = parent_index(c);
                depth += 1;
            }
            match rules.len() {
                0 => VisibleRule::Always,
                1 => rules.pop().unwrap(),
                _ => VisibleRule::All(rules),
            }
        };
        let mut anims: HashMap<i64, Box<ObjAnim>> = HashMap::new();
        let mut sprites: HashMap<i64, crate::pkg::SpritesheetMeta> = HashMap::new();
        for (i, raw) in raws.iter().enumerate() {
            let parent = parent_index(i)
                .and_then(|p| resolved[p])
                .unwrap_or(root_transform);
            if let (Some(id), Some(a)) = (
                raw.obj.get("id").and_then(|v| v.as_i64()),
                ObjAnim::parse(
                    &raw.obj,
                    parent,
                    raw.origin,
                    raw.scale,
                    raw.angles,
                    &self.properties,
                    now,
                ),
            ) {
                anims.insert(id, a);
            }
        }
        self.collect_sounds(pkg, &raws.iter().map(|r| &r.obj).collect::<Vec<_>>(), now);
        for (i, raw) in raws.iter().enumerate() {
            let obj = &raw.obj;
            let transform = resolved[i].unwrap_or_else(Affine::identity);
            let obj_id = obj
                .get("id")
                .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())));
            let dependencies: Vec<i64> = obj
                .get("dependencies")
                .and_then(|d| d.as_array())
                .map(|arr| arr.iter().filter_map(|v| v.as_i64()).collect())
                .unwrap_or_default();
            let capture_composite = obj_id
                .map(|id| referenced_composites.contains(&id))
                .unwrap_or(false);
            let name = obj
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("object")
                .to_string();

            let visible = inherited_visible(i);
            let align = parse_alignment(obj);

            if let Some(particle_path) = obj
                .get("particle")
                .or_else(|| obj.get("particlesrc"))
                .and_then(|v| v.as_str())
            {
                let kind = ParticleKind::from_hint(&format!("{name} {particle_path}"));
                let preset = read_scene_text(pkg, particle_path)
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
                let emitter = if let Some(preset_json) = &preset {
                    Emitter::from_we_preset(
                        gl,
                        pkg,
                        &mut self.textures,
                        preset_json,
                        obj,
                        transform,
                        kind,
                        &self.properties,
                    )
                } else {
                    let size = obj
                        .get("size")
                        .map(|v| value_vec(&resolve_prop(v, &self.properties), 3, [600.0, 600.0, 0.0]))
                        .unwrap_or([1200.0, 900.0, 0.0]);
                    let scale = obj
                        .get("scale")
                        .map(|v| value_vec(&resolve_prop(v, &self.properties), 3, [1.0, 1.0, 1.0]))
                        .unwrap_or([1.0, 1.0, 1.0]);
                    let center = [transform.tx, transform.ty];
                    let spread = [
                        size[0].abs() * scale[0].abs() * 0.5,
                        size[1].abs() * scale[1].abs() * 0.5,
                    ];
                    let color = Some(eval_color_field(obj.get("color"), &self.properties, [1.0, 1.0, 1.0]));
                    let count = if kind == ParticleKind::Fog { 20 } else { 80 };
                    Emitter::new(kind, count, center, spread, color, true)
                };
                let idx = self.emitters.len();
                self.emitters.push(emitter);
                self.objects.push(SceneObject {
                    id: obj_id,
                    name,
                    transform,
                    size: [1.0, 1.0],
                    align: [0.0, 0.0],
                    color: [1.0, 1.0, 1.0, 0.0],
                    brightness: 1.0,
                    blend: LayerBlend::Normal,
                    parallax: eval_parallax_field(obj.get("parallaxDepth"), &self.properties),
                    visible,
                    dependencies,
                    capture_composite: false,
                    effects: Vec::new(),
                    color_blend: object_color_blend(obj),
                    kind: ObjectKind::Particle(idx),
                    anim: None,
                    sprite: None,
                    attach: None,
                    raw_idx: Some(i),
                    raw_obj: obj.clone(),
                });
                continue;
            }

            if let Some(text_def) = obj.get("text") {
                if let Some(layer) = Self::load_text(pkg, obj, text_def, &self.properties, now) {
                    let size = obj
                        .get("size")
                        .map(|v| value_vec(&resolve_prop(v, &self.properties), 3, [layer.box_size[0], layer.box_size[1], 0.0]))
                        .map(|v| [v[0], v[1]])
                        .unwrap_or(layer.box_size);
                    let c = eval_color_field(obj.get("color"), &self.properties, [1.0, 1.0, 1.0]);
                    self.objects.push(SceneObject {
                        id: obj_id,
                        name,
                        transform,
                        size,
                        align,
                        color: [
                            c[0],
                            c[1],
                            c[2],
                            eval_alpha_field(obj.get("alpha"), &self.properties, now),
                        ],
                        brightness: eval_brightness_field(obj.get("brightness"), &self.properties, 1.0),
                        blend: object_blend("normal", object_color_blend(obj)),
                        color_blend: object_color_blend(obj),
                        parallax: eval_parallax_field(obj.get("parallaxDepth"), &self.properties),
                        visible,
                        dependencies,
                        capture_composite,
                        effects: load_effects(gl, pkg, obj, &self.properties, now),
                        kind: ObjectKind::Text(Box::new(layer)),
                        anim: None,
                        sprite: None,
                        attach: None,
                        raw_idx: Some(i),
                        raw_obj: obj.clone(),
                    });
                }
                continue;
            }

            let Some(image_ref) = obj.get("image").and_then(|v| v.as_str()) else {
                continue;
            };
            let lower = image_ref.to_ascii_lowercase();
            if lower.contains("util/fullscreenlayer") {
                let effects = load_effects(gl, pkg, obj, &self.properties, now);
                if effects.is_empty() && !capture_composite {
                    continue;
                }
                self.objects.push(SceneObject {
                    id: obj_id,
                    name,
                    transform: Affine::translate(
                        self.ortho_size[0] * 0.5,
                        self.ortho_size[1] * 0.5,
                    ),
                    size: [self.ortho_size[0], self.ortho_size[1]],
                    align: [0.0, 0.0],
                    color: [1.0, 1.0, 1.0, 1.0],
                    brightness: 1.0,
                    blend: LayerBlend::Normal,
                    parallax: [0.0, 0.0],
                    visible,
                    dependencies,
                    capture_composite,
                    effects,
                    color_blend: object_color_blend(obj),
                    kind: ObjectKind::Post,
                    anim: None,
                    sprite: None,
                    attach: None,
                    raw_idx: Some(i),
                    raw_obj: obj.clone(),
                });
                continue;
            }
            if lower.contains("util/projectlayer") || lower.contains("util/composelayer") {
                let effects = load_effects(gl, pkg, obj, &self.properties, now);
                let default_copy_bg = lower.contains("util/projectlayer");
                let copy_background = obj
                    .get("copybackground")
                    .and_then(value_bool)
                    .unwrap_or(default_copy_bg);
                if effects.is_empty() && !capture_composite {
                    continue;
                }
                let size = obj
                    .get("size")
                    .map(|v| value_vec(&resolve_prop(v, &self.properties), 3, [self.ortho_size[0], self.ortho_size[1], 0.0]))
                    .map(|v| [v[0].abs().max(1.0), v[1].abs().max(1.0)])
                    .unwrap_or(self.ortho_size);
                self.objects.push(SceneObject {
                    id: obj_id,
                    name,
                    transform,
                    size,
                    align,
                    color: [
                        1.0,
                        1.0,
                        1.0,
                        eval_alpha_field(obj.get("alpha"), &self.properties, now),
                    ],
                    brightness: eval_brightness_field(obj.get("brightness"), &self.properties, 1.0),
                    blend: object_blend("normal", object_color_blend(obj)),
                    color_blend: object_color_blend(obj),
                    parallax: eval_parallax_field(obj.get("parallaxDepth"), &self.properties),
                    visible,
                    dependencies,
                    capture_composite,
                    effects,
                    kind: ObjectKind::Compose { copy_background },
                    anim: None,
                    sprite: None,
                    attach: None,
                    raw_idx: Some(i),
                    raw_obj: obj.clone(),
                });
                continue;
            }
            if lower.contains("util/solidlayer") {
                let size = obj
                    .get("size")
                    .map(|v| value_vec(&resolve_prop(v, &self.properties), 3, [100.0, 100.0, 0.0]))
                    .unwrap_or([self.ortho_size[0], self.ortho_size[1], 0.0]);
                let c = eval_color_field(obj.get("color"), &self.properties, [1.0, 1.0, 1.0]);
                self.objects.push(SceneObject {
                    id: obj_id,
                    name,
                    transform,
                    size: [size[0], size[1]],
                    align,
                    color: [
                        c[0],
                        c[1],
                        c[2],
                        eval_alpha_field(obj.get("alpha"), &self.properties, now),
                    ],
                    brightness: eval_brightness_field(obj.get("brightness"), &self.properties, 1.0),
                    blend: object_blend("normal", object_color_blend(obj)),
                    color_blend: object_color_blend(obj),
                    parallax: eval_parallax_field(obj.get("parallaxDepth"), &self.properties),
                    visible,
                    dependencies,
                    capture_composite,
                    effects: load_effects(gl, pkg, obj, &self.properties, now),
                    kind: ObjectKind::Solid,
                    anim: None,
                    sprite: None,
                    attach: None,
                    raw_idx: Some(i),
                    raw_obj: obj.clone(),
                });
                continue;
            }

            // Model / image.
            let mut model_size: Option<[f32; 2]> = None;
            let mut material_blend = String::from("normal");
            let mut texture_path: Option<String> = None;
            let mut puppet_path: Option<String> = None;
            let mut solid_model = false;
            if lower.ends_with(".json") {
                if let Some(text) = read_scene_text(pkg, image_ref) {
                    if let Ok(model) = serde_json::from_str::<serde_json::Value>(&text) {
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
                            if let Some(text) = read_scene_text(pkg, material_ref) {
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
                let inst_tex_ref = obj
                    .get("instance")
                    .or_else(|| obj.get("instanceoverride"))
                    .and_then(|inst| inst.get("textures"))
                    .and_then(|t| t.as_array())
                    .and_then(|a| a.first())
                    .and_then(|v| v.as_str());
                let inst_composite_ref = inst_tex_ref
                    .and_then(parse_composite_tex_id)
                    .or_else(|| dependencies.first().copied());
                let mut loaded_tex: Option<(glow::Texture, [f32; 2])> = None;
                if inst_composite_ref.is_none() {
                    if let Some(tex_name) = inst_tex_ref {
                        if let Some((t, tw, th, _)) =
                            Self::load_texture(gl, pkg, &mut self.textures, tex_name)
                        {
                            loaded_tex = Some((t, [tw as f32, th as f32]));
                        }
                    }
                }
                let default_size = loaded_tex
                    .map(|(_, s)| [s[0], s[1], 0.0])
                    .unwrap_or([self.ortho_size[0], self.ortho_size[1], 0.0]);
                let size = obj
                    .get("size")
                    .map(|v| value_vec(v, 3, default_size))
                    .unwrap_or(default_size);
                let effects = load_effects(gl, pkg, obj, &self.properties, now);
                let kind = if let Some(ref_id) = inst_composite_ref {
                    ObjectKind::CompositeRef(ref_id)
                } else if let Some((tex, _)) = loaded_tex {
                    ObjectKind::Image(tex)
                } else if obj.get("copybackground").and_then(value_bool) == Some(true) {
                    if effects.is_empty() && !capture_composite {
                        continue;
                    }
                    ObjectKind::Compose {
                        copy_background: true,
                    }
                } else {
                    ObjectKind::Solid
                };
                let c = eval_color_field(obj.get("color"), &self.properties, [1.0, 1.0, 1.0]);
                self.objects.push(SceneObject {
                    id: obj_id,
                    name,
                    transform,
                    size: [size[0], size[1]],
                    align,
                    color: [
                        c[0],
                        c[1],
                        c[2],
                        eval_alpha_field(obj.get("alpha"), &self.properties, now),
                    ],
                    brightness: eval_brightness_field(obj.get("brightness"), &self.properties, 1.0),
                    blend: object_blend("normal", object_color_blend(obj)),
                    color_blend: object_color_blend(obj),
                    parallax: eval_parallax_field(obj.get("parallaxDepth"), &self.properties),
                    visible,
                    dependencies,
                    capture_composite,
                    effects,
                    kind,
                    anim: None,
                    sprite: None,
                    attach: None,
                    raw_idx: Some(i),
                    raw_obj: obj.clone(),
                });
                continue;
            }

            let Some(texture_path) = texture_path else {
                log::debug!("scene object {name}: no usable texture in {image_ref}");
                continue;
            };
            let Some((texture, tex_w, tex_h)) =
                Self::load_texture(gl, pkg, &mut self.textures, &texture_path)
                    .map(|(t, w, h, ss)| {
                        if let (Some(ss), Some(id)) = (ss, obj_id) {
                            sprites.insert(id, ss);
                        }
                        let (w, h) = match ss {
                            Some(ss) => (w / ss.cols.max(1), h / ss.rows.max(1)),
                            None => (w, h),
                        };
                        (t, w, h)
                    })
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
                                        let vis = parse_visible(
                                            layer.get("visible"),
                                            &self.properties,
                                            now,
                                        )
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
                                bounds: mdl_bounds(&parsed.vertices),
                                mdl: parsed,
                                anim_layers,
                                texture,
                                tex_size: [tex_w, tex_h],
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

            if std::env::var_os("ANIMA_TRACE_SCENE").is_some() {
                log::info!(
                    "load obj {:?} '{}' image='{}' tex='{}' {}x{} sprite={:?}",
                    obj_id,
                    name,
                    image_ref,
                    texture_path,
                    tex_w,
                    tex_h,
                    obj_id.and_then(|id| sprites.get(&id).copied())
                );
            }
            let c = eval_color_field(obj.get("color"), &self.properties, [1.0, 1.0, 1.0]);
            self.objects.push(SceneObject {
                id: obj_id,
                name,
                transform,
                size,
                align,
                color: [
                    c[0],
                    c[1],
                    c[2],
                    eval_alpha_field(obj.get("alpha"), &self.properties, now),
                ],
                brightness: eval_brightness_field(obj.get("brightness"), &self.properties, 1.0),
                blend: object_blend(&material_blend, object_color_blend(obj)),
                color_blend: object_color_blend(obj),
                parallax: eval_parallax_field(obj.get("parallaxDepth"), &self.properties),
                visible,
                dependencies,
                capture_composite,
                effects: load_effects(gl, pkg, obj, &self.properties, now),
                kind,
                anim: None,
                sprite: None,
                attach: None,
                raw_idx: Some(i),
                raw_obj: obj.clone(),
            });
        }

        // Wallpaper Engine attachments: a layer parented to a puppet is placed
        // at a named bone (or an `MDAT` attachment point) rather than at the
        // model origin.  The bone matrix lives in the puppet's model space, so
        // it composes directly with the parent object's world transform.
        let obj_id_of = |j: usize| -> Option<String> {
            raws[j]
                .obj
                .get("id")
                .and_then(|v| v.as_i64())
                .map(|n| n.to_string())
        };
        let mut obj_by_id: HashMap<String, usize> = HashMap::new();
        for (j, o) in self.objects.iter().enumerate() {
            if let Some(id) = o.id {
                obj_by_id.insert(id.to_string(), j);
            }
        }
        for i in 0..raws.len() {
            let Some(att) = raws[i].obj.get("attachment").and_then(|v| v.as_str()) else {
                continue;
            };
            let Some(p) = parent_index(i) else { continue };
            let (Some(child_id), Some(parent_id)) = (obj_id_of(i), obj_id_of(p)) else {
                continue;
            };
            let (Some(&child_obj), Some(&parent_obj)) =
                (obj_by_id.get(&child_id), obj_by_id.get(&parent_id))
            else {
                continue;
            };
            let bone = match &self.objects[parent_obj].kind {
                ObjectKind::Puppet { mdl, .. } => mdl.attachment_world(att),
                _ => None,
            };
            let Some(bone) = bone else {
                log::debug!(
                    "attachment '{att}' on '{}': parent puppet has no such bone",
                    raws[i].obj.get("name").and_then(|v| v.as_str()).unwrap_or("?")
                );
                continue;
            };
            let bone_aff = Affine {
                a: bone[0],
                b: bone[1],
                c: bone[2],
                d: bone[3],
                tx: bone[4],
                ty: bone[5],
            };
            let parent_transform = self.objects[parent_obj].transform;
            self.objects[child_obj].transform =
                parent_transform.mul(bone_aff).mul(raws[i].local);
            // Keep the binding so the layer can follow the animated bone pose.
            self.objects[child_obj].attach = Some(AttachPoint {
                parent: parent_obj,
                bone: att.to_string(),
                local: raws[i].local,
            });
        }

        // Attach keyframe/script animations and animated textures.  Particle
        // emitters bake their transform at load time and post layers cover
        // the whole view, so they are skipped.
        for o in self.objects.iter_mut() {
            let Some(id) = o.id else { continue };
            if matches!(o.kind, ObjectKind::Particle(_) | ObjectKind::Post) {
                continue;
            }
            if matches!(o.kind, ObjectKind::Image(_)) {
                o.sprite = sprites.get(&id).copied();
            }
            if let Some(a) = anims.remove(&id) {
                o.anim = Some(a);
            }
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
                    ObjectKind::Compose { .. } => "compose",
                    ObjectKind::CompositeRef(_) => "composite_ref",
                    ObjectKind::Post => "post",
                };
                log::info!(
                    "obj {i}: '{}' kind={} size={:?} blend={:?} alpha={} pos=({:.1},{:.1}) vis={}",
                    o.name,
                    kind,
                    o.size,
                    o.blend,
                    o.color[3],
                    o.transform.tx,
                    o.transform.ty,
                    o.is_visible(&self.properties)
                );
            }
        }
        Ok(())
    }

    /// Collect Wallpaper Engine `sound` layer objects.  They are not started
    /// here: the backend applies the configured mute/volume right after the
    /// renderer is constructed, so playback is deferred to the first frame
    /// (`start_sounds`).
    fn collect_sounds(
        &mut self,
        pkg: &PackageFs,
        objects: &[&serde_json::Value],
        _now: text::LocalTime,
    ) {
        for obj in objects {
            let Some(list) = obj.get("sound").and_then(|v| v.as_array()) else {
                continue;
            };
            // `startsilent` sounds wait for a SceneScript trigger we cannot run
            // yet; starting them eagerly would play unrelated audio.
            if obj
                .get("startsilent")
                .and_then(value_bool)
                .unwrap_or(false)
            {
                continue;
            }
            let base_volume = obj
                .get("volume")
                .and_then(value_f32)
                .unwrap_or(1.0)
                .clamp(0.0, 1.0);
            if base_volume <= 0.0 {
                continue;
            }
            let shuffle = obj
                .get("playbackmode")
                .and_then(|v| v.as_str())
                .map(|m| m.eq_ignore_ascii_case("random"))
                .unwrap_or(false);
            let mut files = Vec::new();
            for sound in list {
                let Some(rel) = sound.as_str() else {
                    continue;
                };
                match self.resolve_sound_file(pkg, rel) {
                    Some(path) => files.push(path),
                    None => log::warn!("scene sound '{rel}' not found in package"),
                }
            }
            if files.is_empty() {
                continue;
            }
            self.sound_specs.push(SoundSpec {
                files,
                base_volume,
                shuffle,
            });
        }
        if !self.sound_specs.is_empty() {
            log::debug!("scene sounds: {} layer(s)", self.sound_specs.len());
        }
    }

    /// Resolve a `sound` entry to a real file path.  Directory wallpapers are
    /// used in place; archive-backed wallpapers extract the audio next to the
    /// other temporary files, since libmpv cannot read from a `.pkg`.
    fn resolve_sound_file(
        &mut self,
        pkg: &PackageFs,
        rel: &str,
    ) -> Option<std::path::PathBuf> {
        let clean = rel.replace('\\', "/");
        let clean = clean
            .trim_start_matches("./")
            .trim_start_matches('/')
            .to_string();
        let disk = pkg.root().join(&clean);
        if disk.is_file() {
            return Some(disk);
        }
        let bytes = pkg.read(&clean).ok()?;
        let dir = self.sound_temp_dir(pkg);
        let name = clean
            .replace(['/', '\\'], "_")
            .replace("..", "_");
        let path = dir.join(name);
        std::fs::write(&path, &bytes).ok()?;
        Some(path)
    }

    fn sound_temp_dir(&mut self, pkg: &PackageFs) -> std::path::PathBuf {
        if let Some(dir) = &self.sound_temp_dir {
            return dir.clone();
        }
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        pkg.root().hash(&mut hasher);
        // Distinguish renderers of the same wallpaper on different outputs so
        // one output's cleanup cannot delete files another is still playing.
        (self as *const Self as usize).hash(&mut hasher);
        let dir = std::env::temp_dir().join(format!("anima-sounds-{:016x}", hasher.finish()));
        let _ = std::fs::create_dir_all(&dir);
        self.sound_temp_dir = Some(dir.clone());
        dir
    }

    /// Start deferred scene sounds with the current volume/mute state.
    fn start_sounds(&mut self) {
        if self.sounds_started {
            return;
        }
        self.sounds_started = true;
        let specs = std::mem::take(&mut self.sound_specs);
        for spec in specs {
            let volume = (self.volume * spec.base_volume).clamp(0.0, 1.0);
            match super::video::AudioPlayer::new(
                &spec.files,
                spec.shuffle,
                volume,
                self.muted,
            ) {
                Ok(mut player) => {
                    if self.paused {
                        player.set_paused(true);
                    }
                    self.sounds.push(SceneSound {
                        player,
                        base_volume: spec.base_volume,
                    });
                }
                Err(err) => log::warn!("scene sound playback disabled: {err}"),
            }
        }
    }

    fn load_text(
        pkg: &PackageFs,
        obj: &serde_json::Value,
        text_def: &serde_json::Value,
        props_map: &HashMap<String, serde_json::Value>,
        now: text::LocalTime,
    ) -> Option<TextLayer> {
        let (script, authored) = match text_def {
            serde_json::Value::String(s) => (None, s.clone()),
            serde_json::Value::Object(o) => {
                let authored = if text_def.get("user").is_some() {
                    value_to_string(&resolve_prop(text_def, props_map))
                } else {
                    o.get("value")
                        .map(value_to_string)
                        .unwrap_or_else(|| "Text".to_string())
                };
                (
                    o.get("script").and_then(|v| v.as_str()).map(str::to_string),
                    authored,
                )
            }
            _ => (None, String::new()),
        };
        let props = text_def
            .get("scriptproperties")
            .and_then(|v| v.as_object())
            .map(|o| o.clone().into_iter().collect::<HashMap<_, _>>())
            .unwrap_or_default();

        let mut point_size = obj
            .get("pointsize")
            .map(|v| resolve_prop(v, props_map))
            .and_then(|v| value_f32(&v))
            .unwrap_or(32.0);
        if point_size <= 0.0 {
            point_size = 32.0;
        }
        let box_size = obj
            .get("size")
            .map(|v| {
                let p = value_vec(&resolve_prop(v, props_map), 3, [512.0, 128.0, 0.0]);
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
            raw_text_def: text_def.clone(),
            props,
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
            let Some((texture, _w, _h, _)) = Self::load_texture(gl, pkg, &mut self.textures, path)
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
                id: None,
                name: l.name.unwrap_or_else(|| "layer".into()),
                transform,
                size,
                align: [0.0, 0.0],
                color: [c[0], c[1], c[2], l.alpha.unwrap_or(1.0)],
                brightness: 1.0,
                blend: LayerBlend::from_material(l.blend.as_deref().unwrap_or("normal")),
                color_blend: 0,
                parallax: l.parallax_depth.unwrap_or([0.0, 0.0]),
                visible: VisibleRule::Script(parse_visible_script(&serde_json::Value::Null, now)),
                dependencies: Vec::new(),
                capture_composite: false,
                effects: Vec::new(),
                kind: ObjectKind::Image(texture),
                anim: None,
                sprite: None,
                attach: None,
                raw_idx: None,
                raw_obj: serde_json::Value::Null,
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
                id: None,
                name: "particles".into(),
                transform: Affine::identity(),
                size: [1.0, 1.0],
                align: [0.0, 0.0],
                color: [1.0, 1.0, 1.0, 0.0],
                brightness: 1.0,
                blend: LayerBlend::Normal,
                parallax: p.parallax_depth.unwrap_or([0.0, 0.0]),
                visible: VisibleRule::Always,
                dependencies: Vec::new(),
                capture_composite: false,
                effects: Vec::new(),
                color_blend: 0,
                kind: ObjectKind::Particle(idx),
                anim: None,
                sprite: None,
                attach: None,
                raw_idx: None,
                raw_obj: serde_json::Value::Null,
            });
        }
        Ok(())
    }
}

fn recompute_raw_transforms(
    raw_nodes: &[(Option<usize>, serde_json::Value)],
    authored_ortho: [f32; 2],
    view_shift: f32,
    props: &HashMap<String, serde_json::Value>,
) -> Vec<(Affine, Affine, [f32; 3], [f32; 3], [f32; 3])> {
    let root_transform = Affine::translate(view_shift, 0.0);
    let mut locals = Vec::with_capacity(raw_nodes.len());
    let mut origins = Vec::with_capacity(raw_nodes.len());
    let mut scales = Vec::with_capacity(raw_nodes.len());
    let mut angles_vec = Vec::with_capacity(raw_nodes.len());
    for (parent, obj) in raw_nodes {
        let default_origin = if parent.is_some() {
            [0.0, 0.0, 0.0]
        } else {
            [authored_ortho[0] * 0.5, authored_ortho[1] * 0.5, 0.0]
        };
        let origin = obj
            .get("origin")
            .map(|v| eval_origin_field(v, default_origin, authored_ortho, props))
            .unwrap_or(default_origin);
        let scale = obj
            .get("scale")
            .map(|v| value_vec(&resolve_prop(v, props), 3, [1.0, 1.0, 1.0]))
            .unwrap_or([1.0, 1.0, 1.0]);
        let angles = obj
            .get("angles")
            .map(|v| value_vec(&resolve_prop(v, props), 3, [0.0, 0.0, 0.0]))
            .unwrap_or([0.0, 0.0, 0.0]);
        let local = Affine::translate(origin[0], origin[1])
            .mul(Affine::rotate(angles[2]))
            .mul(Affine::scale(
                if scale[0] == 0.0 { 1.0 } else { scale[0] },
                if scale[1] == 0.0 { 1.0 } else { scale[1] },
            ));
        locals.push(local);
        origins.push(origin);
        scales.push(scale);
        angles_vec.push(angles);
    }
    let mut worlds: Vec<Option<Affine>> = vec![None; raw_nodes.len()];
    fn resolve_w(
        i: usize,
        raw_nodes: &[(Option<usize>, serde_json::Value)],
        locals: &[Affine],
        root: Affine,
        out: &mut [Option<Affine>],
        depth: usize,
    ) -> Affine {
        if let Some(t) = out[i] {
            return t;
        }
        if depth > 32 {
            return root;
        }
        let parent_t = raw_nodes[i]
            .0
            .map(|p| resolve_w(p, raw_nodes, locals, root, out, depth + 1))
            .unwrap_or(root);
        let w = parent_t.mul(locals[i]);
        out[i] = Some(w);
        w
    }
    (0..raw_nodes.len())
        .map(|i| {
            let world = resolve_w(i, raw_nodes, &locals, root_transform, &mut worlds, 0);
            let parent_world = raw_nodes[i]
                .0
                .and_then(|p| worlds[p])
                .unwrap_or(root_transform);
            (world, parent_world, origins[i], scales[i], angles_vec[i])
        })
        .collect()
}

impl SceneRenderer {
    pub fn apply_property(&mut self, key: &str, value: &str) {
        let parsed = if value.eq_ignore_ascii_case("true") {
            serde_json::Value::Bool(true)
        } else if value.eq_ignore_ascii_case("false") {
            serde_json::Value::Bool(false)
        } else if let Ok(i) = value.parse::<i64>() {
            serde_json::Value::Number(serde_json::Number::from(i))
        } else if let Ok(n) = value.parse::<f64>() {
            serde_json::Value::Number(
                serde_json::Number::from_f64(n).unwrap_or_else(|| serde_json::Number::from(0)),
            )
        } else {
            serde_json::Value::String(value.to_string())
        };
        let existing_key = self
            .properties
            .keys()
            .find(|k| k.eq_ignore_ascii_case(key))
            .cloned()
            .unwrap_or_else(|| key.to_string());
        self.properties.insert(existing_key, parsed);

        let kl = key.to_ascii_lowercase();
        let is_authored = self.authored_prop_keys.contains(&kl);
        match kl.as_str() {
            "clearcolor" => {
                self.clear_color =
                    parse_color(&serde_json::Value::String(value.to_string()), self.clear_color);
            }
            "speed" => {
                if let Ok(v) = value.parse::<f32>() {
                    self.speed_multiplier = v.clamp(0.0, 5.0);
                }
            }
            "parallax" | "cameraparallax" => {
                if value.eq_ignore_ascii_case("true") || value.eq_ignore_ascii_case("false") {
                    self.parallax_enabled = value.eq_ignore_ascii_case("true");
                } else if let Ok(v) = value.parse::<f32>() {
                    self.parallax_enabled = v > 0.0;
                    self.parallax_amount = v;
                }
            }
            "ui_brightness" => {
                if let Ok(v) = value.parse::<f32>() {
                    self.bcs_hue[0] = v.clamp(-1.0, 1.0);
                }
            }
            "brightness" if !is_authored => {
                if let Ok(v) = value.parse::<f32>() {
                    self.bcs_hue[0] = v.clamp(-1.0, 1.0);
                }
            }
            "ui_contrast" => {
                if let Ok(v) = value.parse::<f32>() {
                    self.bcs_hue[1] = v.clamp(0.0, 3.0);
                }
            }
            "contrast" if !is_authored => {
                if let Ok(v) = value.parse::<f32>() {
                    self.bcs_hue[1] = v.clamp(0.0, 3.0);
                }
            }
            "ui_saturation" => {
                if let Ok(v) = value.parse::<f32>() {
                    self.bcs_hue[2] = v.clamp(0.0, 3.0);
                }
            }
            "saturation" if !is_authored => {
                if let Ok(v) = value.parse::<f32>() {
                    self.bcs_hue[2] = v.clamp(0.0, 3.0);
                }
            }
            "ui_hue" => {
                if let Ok(v) = value.parse::<f32>() {
                    self.bcs_hue[3] = v.clamp(-180.0, 180.0).to_radians();
                }
            }
            "hue" | "hueshift" if !is_authored => {
                if let Ok(v) = value.parse::<f32>() {
                    self.bcs_hue[3] = v.clamp(-180.0, 180.0).to_radians();
                }
            }
            "schemecolor" | _ => {}
        }
        self.reapply_scene_properties();
    }

    fn reapply_scene_properties(&mut self) {
        let now = text::local_time();
        if let Some(general) = &self.scene_general {
            if let Some(cc) = general.get("clearcolor").filter(|v| v.get("user").is_some()) {
                self.clear_color = eval_color_field(Some(cc), &self.properties, self.clear_color);
            }
            if let Some(cp) = general.get("cameraparallax").filter(|v| v.get("user").is_some()) {
                if let Some(b) = value_bool(&resolve_prop(cp, &self.properties)) {
                    self.parallax_enabled = b;
                }
            }
            if let Some(cpa) = general
                .get("cameraparallaxamount")
                .filter(|v| v.get("user").is_some())
            {
                if let Some(v) = value_f32(&resolve_prop(cpa, &self.properties)) {
                    self.parallax_amount = v;
                }
            }
            if let Some(cpmi) = general
                .get("cameraparallaxmouseinfluence")
                .filter(|v| v.get("user").is_some())
            {
                if let Some(v) = value_f32(&resolve_prop(cpmi, &self.properties)) {
                    self.parallax_mouse_influence = v;
                }
            }
        }

        let transforms = if self.raw_nodes.is_empty() {
            Vec::new()
        } else {
            recompute_raw_transforms(
                &self.raw_nodes,
                self.authored_ortho,
                self.view_shift,
                &self.properties,
            )
        };

        for obj in &mut self.objects {
            if obj.raw_obj.is_null() {
                continue;
            }
            let raw = &obj.raw_obj;
            let alpha = eval_alpha_field(raw.get("alpha"), &self.properties, now);
            match &mut obj.kind {
                ObjectKind::Post => {}
                ObjectKind::Compose { .. } => {
                    obj.color[3] = alpha;
                    obj.brightness =
                        eval_brightness_field(raw.get("brightness"), &self.properties, 1.0);
                    obj.parallax = eval_parallax_field(raw.get("parallaxDepth"), &self.properties);
                }
                ObjectKind::Particle(idx) => {
                    obj.parallax = eval_parallax_field(raw.get("parallaxDepth"), &self.properties);
                    if let Some(color_mult) = raw
                        .get("instanceoverride")
                        .and_then(|o| o.get("colorn").or_else(|| o.get("color")))
                        .or_else(|| raw.get("color"))
                        .map(|c| parse_color(&resolve_prop(c, &self.properties), [1.0, 1.0, 1.0]))
                    {
                        if let Some(em) = self.emitters.get_mut(*idx) {
                            em.update_color_mult(color_mult);
                        }
                    }
                }
                ObjectKind::Text(layer) => {
                    let c = eval_color_field(raw.get("color"), &self.properties, [1.0, 1.0, 1.0]);
                    obj.color = [c[0], c[1], c[2], alpha];
                    obj.brightness =
                        eval_brightness_field(raw.get("brightness"), &self.properties, 1.0);
                    obj.parallax = eval_parallax_field(raw.get("parallaxDepth"), &self.properties);
                    let ps = raw
                        .get("pointsize")
                        .map(|v| resolve_prop(v, &self.properties))
                        .and_then(|v| value_f32(&v))
                        .unwrap_or(layer.point_size)
                        .max(1.0);
                    if (ps - layer.point_size).abs() > 1e-3 {
                        layer.point_size = ps;
                        layer.raster_ppw = -1.0;
                    }
                }
                ObjectKind::Image(_)
                | ObjectKind::Puppet { .. }
                | ObjectKind::Solid
                | ObjectKind::CompositeRef(_) => {
                    let c = eval_color_field(raw.get("color"), &self.properties, [1.0, 1.0, 1.0]);
                    obj.color = [c[0], c[1], c[2], alpha];
                    obj.brightness =
                        eval_brightness_field(raw.get("brightness"), &self.properties, 1.0);
                    obj.parallax = eval_parallax_field(raw.get("parallaxDepth"), &self.properties);
                }
            }

            if let Some(ridx) = obj.raw_idx {
                if let Some(&(world, parent_world, origin, scale, angles)) = transforms.get(ridx) {
                    if let Some(att) = &mut obj.attach {
                        att.local = Affine::translate(origin[0], origin[1])
                            .mul(Affine::rotate(angles[2]))
                            .mul(Affine::scale(
                                if scale[0] == 0.0 { 1.0 } else { scale[0] },
                                if scale[1] == 0.0 { 1.0 } else { scale[1] },
                            ));
                    } else if !matches!(obj.kind, ObjectKind::Post | ObjectKind::Particle(_)) {
                        obj.anim = ObjAnim::parse(
                            raw,
                            parent_world,
                            origin,
                            scale,
                            angles,
                            &self.properties,
                            now,
                        );
                        if obj.anim.is_none() {
                            obj.transform = world;
                        }
                    }
                }
            }
        }
    }

    fn cover_uv_transform(&self) -> ([f32; 2], [f32; 2]) {
        let ortho_w = self.ortho_size[0].max(1.0);
        let ortho_h = self.ortho_size[1].max(1.0);
        let scene_aspect = ortho_w / ortho_h;
        let win_aspect = (self.width.max(1) as f32) / (self.height.max(1) as f32);
        if (win_aspect - scene_aspect).abs() < 1e-3 {
            ([1.0, 1.0], [0.0, 0.0])
        } else if win_aspect > scene_aspect {
            let sy = (scene_aspect / win_aspect).clamp(0.05, 1.0);
            ([1.0, sy], [0.0, (1.0 - sy) * 0.5])
        } else {
            let sx = (win_aspect / scene_aspect).clamp(0.05, 1.0);
            ([sx, 1.0], [(1.0 - sx) * 0.5, 0.0])
        }
    }

    fn ensure_scene(&mut self, gl: &glow::Context) -> Result<()> {
        let (uv_scale, _) = self.cover_uv_transform();
        let w = ((self.width.max(1) as f32 / uv_scale[0]).round() as i32).clamp(1, 4096);
        let h = ((self.height.max(1) as f32 / uv_scale[1]).round() as i32).clamp(1, 4096);
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
            if let Some(old) = self.scene_copy.take() {
                old.destroy(gl);
            }
            for (_, old) in self.composites.drain() {
                old.destroy(gl);
            }
            self.scene = Some(Canvas::new(gl, w, h)?);
            self.scene_alt = Some(Canvas::new(gl, w, h)?);
        }
        Ok(())
    }

    fn blit_subrect_to_canvas(
        &mut self,
        gl: &glow::Context,
        src_tex: glow::Texture,
        dst: &Canvas,
        uv_offset: [f32; 2],
        uv_scale: [f32; 2],
    ) {
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(dst.fbo));
            gl.viewport(0, 0, dst.w, dst.h);
            gl.disable(glow::BLEND);
            gl.disable(glow::DEPTH_TEST);
            let p = &mut self.copy_prog;
            p.bind(gl);
            p.set_vec2(gl, "u_uv_scale", uv_scale[0], uv_scale[1]);
            p.set_vec2(gl, "u_uv_offset", uv_offset[0], uv_offset[1]);
            p.set_f32(gl, "u_flip_y", -1.0);
            p.set_vec4(gl, "u_bcs_hue", [0.0, 1.0, 1.0, 0.0]);
            p.set_i32(gl, "u_tex", 0);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(src_tex));
            gl.bind_vertex_array(Some(self.quad_vao));
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        }
    }

    /// Parallax depth an object is actually drawn with.
    ///
    /// Bone-attached layers (eyes, bangs, ornaments) usually have no
    /// `parallaxDepth` of their own: their transform is bone-relative, so they
    /// belong to their parent's camera plane.  Using their own (zero) depth
    /// would pin them to the camera while the body slides underneath, which
    /// makes attached parts drift off the model when the camera moves.
    fn effective_parallax(&self, index: usize) -> [f32; 2] {
        let mut cur = index;
        for _ in 0..64 {
            let obj = &self.objects[cur];
            match &obj.attach {
                Some(att) if att.parent < self.objects.len() && att.parent != cur => {
                    cur = att.parent;
                }
                _ => return obj.parallax,
            }
        }
        self.objects[index].parallax
    }

    fn parallax_offset(&self, depth: [f32; 2], time: f32) -> [f32; 2] {
        let mut offset = [0.0f32; 2];
        if self.parallax_enabled {
            // In Wallpaper Engine, camera parallax shifts the camera by
            // `mouse_ndc * depth * ortho * 0.5 * amount * mouse_influence`, so
            // layers shift in the opposite direction (`-s`).
            let s = -self.parallax_amount * self.parallax_mouse_influence * 0.5;
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
                    p.set_vec4(gl, "u_uv_rect", info.uv_rect);
                    p.set_i32(gl, "u_premul", i32::from(info.premul));
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
                    p.set_i32(gl, "u_premul", i32::from(info.premul));
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
                    p.set_vec4(gl, "u_uv_rect", [0.0, 0.0, 1.0, 1.0]);
                    p.set_i32(gl, "u_premul", i32::from(info.premul));
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
        color_blend: i32,
    ) -> Result<()> {
        let origin = [(min[0] + max[0]) * 0.5, (min[1] + max[1]) * 0.5];
        let axis_x = [(max[0] - min[0]) * 0.5, 0.0];
        let axis_y = [0.0, (max[1] - min[1]) * 0.5];
        self.draw_layer_canvas_transformed(gl, canvas, origin, axis_x, axis_y, blend, color_blend)
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_layer_canvas_transformed(
        &mut self,
        gl: &glow::Context,
        canvas: &Canvas,
        origin: [f32; 2],
        axis_x: [f32; 2],
        axis_y: [f32; 2],
        blend: LayerBlend,
        color_blend: i32,
    ) -> Result<()> {
        if color_blend != 0 && color_blend != 31 {
            return self.draw_layer_canvas_blended_transformed(
                gl,
                canvas,
                origin,
                axis_x,
                axis_y,
                color_blend,
            );
        }
        unsafe {
            apply_blend(gl, blend);
            let p = &mut self.composite_prog;
            p.bind(gl);
            p.set_vec2(gl, "u_origin", origin[0], origin[1]);
            p.set_vec2(gl, "u_axis_x", axis_x[0], axis_x[1]);
            p.set_vec2(gl, "u_axis_y", axis_y[0], axis_y[1]);
            p.set_vec2(gl, "u_ortho", self.ortho_size[0], self.ortho_size[1]);
            p.set_i32(gl, "u_premul", i32::from(is_premul_blend(blend)));
            p.set_i32(gl, "u_tex", 0);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(canvas.tex));
            gl.bind_vertex_array(Some(self.quad_vao));
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        }
        Ok(())
    }

    fn snapshot_scene_copy(&mut self, gl: &glow::Context) -> Result<glow::Texture> {
        let (scene_w, scene_h, scene_fbo) = {
            let scene = self.scene.as_ref().unwrap();
            (scene.w, scene.h, scene.fbo)
        };
        let recreate = match &self.scene_copy {
            Some(c) => c.w != scene_w || c.h != scene_h,
            None => true,
        };
        if recreate {
            if let Some(old) = self.scene_copy.take() {
                old.destroy(gl);
            }
            self.scene_copy = Some(Canvas::new(gl, scene_w, scene_h)?);
        }
        let copy_fbo = self.scene_copy.as_ref().unwrap().fbo;
        let copy_tex = self.scene_copy.as_ref().unwrap().tex;
        unsafe {
            // Snapshot the scene.  A framebuffer blit keeps the row order
            // identical, which is what shader screen-derived UVs expect.
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(scene_fbo));
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(copy_fbo));
            gl.blit_framebuffer(
                0,
                0,
                scene_w,
                scene_h,
                0,
                0,
                scene_w,
                scene_h,
                glow::COLOR_BUFFER_BIT,
                glow::NEAREST,
            );
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(scene_fbo));
            gl.viewport(0, 0, scene_w, scene_h);
        }
        Ok(copy_tex)
    }

    /// Composite a finished layer canvas with a Wallpaper Engine
    /// `colorBlendMode` (`common_blending.h` semantics).
    ///
    /// Those modes read the background they are blended onto, so the composed
    /// scene is snapshotted into `scene_copy` first and both textures are fed
    /// to the blending shader.  The quad covers the layer's oriented bounds,
    /// so everything outside it is left untouched.
    fn draw_layer_canvas_blended_transformed(
        &mut self,
        gl: &glow::Context,
        canvas: &Canvas,
        origin: [f32; 2],
        axis_x: [f32; 2],
        axis_y: [f32; 2],
        mode: i32,
    ) -> Result<()> {
        let copy_tex = self.snapshot_scene_copy(gl)?;
        unsafe {
            gl.disable(glow::BLEND);
            gl.disable(glow::DEPTH_TEST);

            let p = &mut self.blend_prog;
            p.bind(gl);
            p.set_vec2(gl, "u_origin", origin[0], origin[1]);
            p.set_vec2(gl, "u_axis_x", axis_x[0], axis_x[1]);
            p.set_vec2(gl, "u_axis_y", axis_y[0], axis_y[1]);
            p.set_vec2(gl, "u_ortho", self.ortho_size[0], self.ortho_size[1]);
            p.set_i32(gl, "u_blend_mode", mode);
            p.set_i32(gl, "u_layer", 0);
            p.set_i32(gl, "u_scene", 1);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(canvas.tex));
            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(glow::TEXTURE_2D, Some(copy_tex));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_vertex_array(Some(self.quad_vao));
            gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn run_object_effects(
        &mut self,
        gl: &glow::Context,
        obj_idx: usize,
        canvas: &Canvas,
        tmp: &Canvas,
        time: f32,
        frametime: f32,
        daytime: f32,
        now: text::LocalTime,
        mouse: [f32; 2],
        audio: [f32; 4],
        spectrum: &[f32; 64],
        projection_inverse: [f32; 16],
        self_id: Option<i64>,
    ) -> bool {
        let mut effects = std::mem::take(&mut self.objects[obj_idx].effects);
        let deps = std::mem::take(&mut self.objects[obj_idx].dependencies);
        let mut flip = false;
        {
            let ctx = EffectContext {
                time,
                frametime,
                daytime,
                mouse,
                audio,
                spectrum,
                projection_inverse,
                composites: &self.composites,
                self_id,
                dependencies: &deps,
                props: &self.properties,
            };
            for effect in effects.iter_mut() {
                if !parse_visible(effect.visible_raw.as_ref(), ctx.props, now).eval(ctx.props) {
                    continue;
                }
                let (src, dst) = if flip { (tmp, canvas) } else { (canvas, tmp) };
                if effect
                    .apply(gl, &self.pkg, &mut self.textures, src, dst, &ctx)
                    .is_ok()
                {
                    flip = !flip;
                }
            }
        }
        self.objects[obj_idx].effects = effects;
        self.objects[obj_idx].dependencies = deps;
        flip
    }

    fn capture_layer_composite(
        &mut self,
        gl: &glow::Context,
        id: i64,
        final_canvas: &Canvas,
    ) -> Result<()> {
        let snap = match self.composites.remove(&id) {
            Some(c) if c.w == final_canvas.w && c.h == final_canvas.h => c,
            Some(old) => {
                old.destroy(gl);
                Canvas::new(gl, final_canvas.w, final_canvas.h)?
            }
            None => Canvas::new(gl, final_canvas.w, final_canvas.h)?,
        };
        self.blit_subrect_to_canvas(gl, final_canvas.tex, &snap, [0.0, 0.0], [1.0, 1.0]);
        self.composites.insert(id, snap);
        Ok(())
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
            let resolved = resolve_prop(v, props);
            let value = if resolved.is_object() {
                resolved
                    .get("value")
                    .cloned()
                    .unwrap_or(serde_json::Value::Null)
            } else {
                resolved
            };
            merged.insert(k.clone(), value);
        }
        if let Some(script) = &self.script {
            if let Some(value) = text::eval_script(script, &merged, now) {
                return value;
            }
        }
        if self.raw_text_def.get("user").is_some() {
            return value_to_string(&resolve_prop(&self.raw_text_def, props));
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
            true,
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

fn parse_alignment(obj: &serde_json::Value) -> [f32; 2] {
    let s = obj
        .get("alignment")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let x = if s.contains("left") {
        0.5
    } else if s.contains("right") {
        -0.5
    } else {
        0.0
    };
    let y = if s.contains("bottom") {
        0.5
    } else if s.contains("top") {
        -0.5
    } else {
        0.0
    };
    [x, y]
}

fn eval_visible_script(
    v: &serde_json::Value,
    clean_script: &str,
    props: &HashMap<String, serde_json::Value>,
    now: text::LocalTime,
) -> Option<bool> {
    if clean_script.contains("mediaPlaybackChanged")
        && clean_script.contains("PLAYBACK_STOPPED")
        && clean_script.contains("thisLayer.visible")
    {
        return Some(false);
    }
    if clean_script.contains("cursorEnter")
        && clean_script.contains("cursorLeave")
        && clean_script.contains("thisLayer.visible")
    {
        return Some(false);
    }
    if clean_script.contains("isLayerVisible")
        && clean_script.contains("visibleStart")
        && clean_script.contains("visibleEnd")
    {
        let sp = v.get("scriptproperties").and_then(|s| s.as_object());
        let is_enabled = sp_bool(sp, "isEnabled", props).unwrap_or(true);
        let reverse_mode = sp_bool(sp, "reverseMode", props).unwrap_or(false);
        if !is_enabled {
            return Some(reverse_mode);
        }
        let start = sp_f32(sp, "visibleStart", props)?;
        let end = sp_f32(sp, "visibleEnd", props)?;
        let current_time =
            now.hour as f32 + now.minute as f32 / 60.0 + now.second as f32 / 3600.0;
        let visible = if start < end {
            current_time >= start && current_time < end
        } else {
            current_time >= start || current_time < end
        };
        return Some(visible);
    }
    None
}

fn parse_visible(
    v: Option<&serde_json::Value>,
    props: &HashMap<String, serde_json::Value>,
    now: text::LocalTime,
) -> VisibleRule {
    let Some(v) = v else {
        return VisibleRule::Always;
    };
    if let Some(script) = v.get("script").and_then(|s| s.as_str()) {
        let clean = strip_line_comments(script);
        // Only date-range (holiday) or handled helper scripts decide visibility;
        // other visibility scripts are side-effect helpers (shared animation
        // cores) where the authored user/value still applies.
        if is_date_script(&clean) {
            return VisibleRule::Script(script_is_currently_true(&clean, now));
        }
        if let Some(b) = eval_visible_script(v, &clean, props, now) {
            return VisibleRule::Script(b);
        }
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
    match v.get("script").and_then(|s| s.as_str()) {
        Some(script) if is_date_script(script) => script_is_currently_true(script, now),
        _ => v.get("value").and_then(value_bool).unwrap_or(true),
    }
}

fn is_date_script(script: &str) -> bool {
    script.contains("getMonth()") || script.contains("getDate()") || script.contains("new Date(now.getFullYear()")
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

fn read_scene_text(pkg: &PackageFs, path: &str) -> Option<String> {
    let clean = path.trim_start_matches('/');
    if let Ok(s) = pkg.read_to_string(clean) {
        return Some(s);
    }
    let resolved = crate::pkg::resolve_we_asset(clean)?;
    std::fs::read_to_string(resolved).ok()
}

fn parse_composite_tex_id(s: &str) -> Option<i64> {
    let rest = s.trim().strip_prefix("_rt_imageLayerComposite_")?;
    let id_str = rest.split('_').next()?;
    id_str.parse::<i64>().ok()
}

fn collect_composite_refs(v: &serde_json::Value, out: &mut std::collections::HashSet<i64>) {
    match v {
        serde_json::Value::String(s) => {
            if let Some(id) = parse_composite_tex_id(s) {
                out.insert(id);
            }
        }
        serde_json::Value::Array(arr) => {
            for item in arr {
                collect_composite_refs(item, out);
            }
        }
        serde_json::Value::Object(map) => {
            for val in map.values() {
                collect_composite_refs(val, out);
            }
        }
        _ => {}
    }
}

fn load_effects(
    gl: &glow::Context,
    pkg: &PackageFs,
    obj: &serde_json::Value,
    _props: &HashMap<String, serde_json::Value>,
    _now: text::LocalTime,
) -> Vec<EffectInstance> {
    if std::env::var_os("ANIMA_NO_EFFECTS").is_some() {
        return Vec::new();
    }
    let Some(effects) = obj.get("effects").and_then(|e| e.as_array()) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for effect in effects {
        // Keep effects whose visibility is script- or user-property-driven so toggling
        // properties live in the inspector enables/disables them without reloading.
        if effect.get("visible").and_then(|v| v.as_bool()) == Some(false) {
            continue;
        }
        let Some(file) = effect.get("file").and_then(|v| v.as_str()) else {
            continue;
        };
        if let Some(instance) = EffectInstance::load(gl, pkg, file, effect) {
            out.push(instance);
        }
    }
    out
}

impl Renderer for SceneRenderer {
    fn render(&mut self, gl: &glow::Context, info: &FrameInfo) -> Result<bool> {
        self.ensure_scene(gl)?;
        self.start_sounds();
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
        let (target_mx, target_my) = debug_mouse_ndc().unwrap_or((target_mx, target_my));
        let lerp = (dt * 8.0).clamp(0.02, 1.0);
        self.smooth_mouse[0] += (target_mx - self.smooth_mouse[0]) * lerp;
        self.smooth_mouse[1] += (target_my - self.smooth_mouse[1]) * lerp;
        let (cover_scale, cover_offset) = self.cover_uv_transform();
        let win_mouse_uv = [
            (self.smooth_mouse[0] + 1.0) * 0.5,
            (1.0 - self.smooth_mouse[1]) * 0.5,
        ];
        let mouse_uv = [
            (cover_offset[0] + win_mouse_uv[0] * cover_scale[0]).clamp(0.0, 1.0),
            (cover_offset[1] + win_mouse_uv[1] * cover_scale[1]).clamp(0.0, 1.0),
        ];

        let ortho_w = self.ortho_size[0].max(1.0);
        let ortho_h = self.ortho_size[1].max(1.0);
        let ortho = [ortho_w, ortho_h];
        let pointer_world = [
            mouse_uv[0] * ortho_w,
            (1.0 - mouse_uv[1]) * ortho_h,
        ];
        let (scene_w, scene_h) = {
            let s = self.scene.as_ref().unwrap();
            (s.w, s.h)
        };
        let px_per_world = (scene_w.max(1) as f32 / ortho_w)
            .max(scene_h.max(1) as f32 / ortho_h);

        let now = text::local_time();
        let daytime = ((now.hour as f32 * 3600.0 + now.minute as f32 * 60.0 + now.second as f32)
            / 86400.0)
            .clamp(0.0, 1.0);

        // Refresh text and skeletal puppet meshes before computing layer bounds.
        {
            let props = &self.properties;
            for obj in self.objects.iter_mut() {
                if !obj.visible.eval(props) && !obj.capture_composite {
                    continue;
                }
                if let Some(anim) = &obj.anim {
                    let (t, alpha) = anim.eval(time, daytime, now);
                    if obj.attach.is_none() {
                        obj.transform = t;
                    }
                    if let Some(a) = alpha {
                        obj.color[3] = a;
                    }
                }
                match &mut obj.kind {
                    ObjectKind::Text(layer) => {
                        let value = layer.evaluate(props, now);
                        // Wallpaper Engine rasterises text at the layer's
                        // authored scale, so a group that is scaled up (e.g. a
                        // 20pt headline stretched 1.5x) keeps full glyph
                        // density.  Our output density therefore has to
                        // include the object's world scale as well, otherwise
                        // large text is rasterised at half resolution and gets
                        // upscaled (soft, blurry edges).
                        let t = &obj.transform;
                        let world_scale = t.a.hypot(t.b).max(t.c.hypot(t.d)).clamp(0.01, 64.0);
                        layer.update(gl, value, (px_per_world * world_scale).max(1.0));
                        obj.size = layer.world_size;
                    }
                    ObjectKind::Puppet {
                        mesh,
                        mdl,
                        anim_layers,
                        bounds,
                        ..
                    } => {
                        if mdl.has_animations() {
                            let verts = mdl.evaluate_vertices(time, anim_layers);
                            *bounds = mdl_bounds(&verts);
                            mesh.update_vertices(gl, &verts);
                        }
                    }
                    _ => {}
                }
            }
        }

        // Attached layers follow the animated bone of their puppet parent, so
        // eyes, brows and hair ornaments move with the head instead of staying
        // at the rest pose.
        for i in 0..self.objects.len() {
            let Some(att) = self.objects[i].attach.clone() else {
                continue;
            };
            let parent_world = self.objects[att.parent].transform;
            let bone = match &self.objects[att.parent].kind {
                ObjectKind::Puppet {
                    mdl, anim_layers, ..
                } => mdl.animated_attachment_world(time, anim_layers, &att.bone),
                _ => None,
            };
            let Some(bone) = bone else { continue };
            let bone_aff = Affine {
                a: bone[0],
                b: bone[1],
                c: bone[2],
                d: bone[3],
                tx: bone[4],
                ty: bone[5],
            };
            let local = match &self.objects[i].anim {
                Some(anim) => anim.eval_local(time, daytime, now).0,
                None => att.local,
            };
            self.objects[i].transform = parent_world.mul(bone_aff).mul(local);
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
            let is_vis = self.objects[i].is_visible(&self.properties);
            let capture_comp = self.objects[i].capture_composite;
            if !is_vis && !capture_comp {
                continue;
            }
            let obj_id = self.objects[i].id;
            let depth = self.effective_parallax(i);
            let offset = self.parallax_offset(depth, time);
            let blend = self.objects[i].blend;
            let color_blend = self.objects[i].color_blend;

            if let ObjectKind::Particle(idx) = self.objects[i].kind {
                if !is_vis {
                    continue;
                }
                let scene_copy_tex = if self.emitters[idx].refract {
                    Some(self.snapshot_scene_copy(gl)?)
                } else {
                    let scene = self.scene.as_ref().unwrap();
                    unsafe {
                        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(scene.fbo));
                        gl.viewport(0, 0, scene.w, scene.h);
                    }
                    None
                };
                let emitter = &mut self.emitters[idx];
                apply_blend(gl, emitter.blend);
                emitter.update_and_pack(
                    dt,
                    time,
                    ortho,
                    info.audio,
                    offset,
                    pointer_world,
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
                    p.set_i32(gl, "u_is_r8", i32::from(emitter.tex_is_r8));
                    p.set_i32(gl, "u_premul", i32::from(is_premul_blend(emitter.blend)));
                    p.set_f32(gl, "u_refract_amount", emitter.refract_amount);
                    p.set_f32(gl, "u_overbright", emitter.overbright);
                    if let Some(tex) = emitter.texture {
                        p.set_i32(gl, "u_has_tex", 1);
                        p.set_i32(gl, "u_tex", 0);
                        gl.active_texture(glow::TEXTURE0);
                        gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                    } else {
                        p.set_i32(gl, "u_has_tex", 0);
                    }
                    if let Some(sc_tex) = scene_copy_tex {
                        p.set_i32(gl, "u_refract", 1);
                        p.set_i32(gl, "u_scene", 1);
                        gl.active_texture(glow::TEXTURE1);
                        gl.bind_texture(glow::TEXTURE_2D, Some(sc_tex));
                        if let Some(ntex) = emitter.normal_tex {
                            p.set_i32(gl, "u_has_normal", 1);
                            p.set_i32(gl, "u_normal", 2);
                            gl.active_texture(glow::TEXTURE2);
                            gl.bind_texture(glow::TEXTURE_2D, Some(ntex));
                        } else {
                            p.set_i32(gl, "u_has_normal", 0);
                        }
                        gl.active_texture(glow::TEXTURE0);
                    } else {
                        p.set_i32(gl, "u_refract", 0);
                        p.set_i32(gl, "u_has_normal", 0);
                    }
                    gl.bind_vertex_array(Some(self.particle_vao));
                    gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.particle_inst_vbo));
                    gl.buffer_data_u8_slice(
                        glow::ARRAY_BUFFER,
                        bytemuck_cast(&self.instance_scratch),
                        glow::STREAM_DRAW,
                    );
                    let count = (self.instance_scratch.len() / 14) as i32;
                    gl.draw_arrays_instanced(glow::TRIANGLE_STRIP, 0, 4, count);
                }
                continue;
            }

            if matches!(self.objects[i].kind, ObjectKind::Post) {
                let mut effects = std::mem::take(&mut self.objects[i].effects);
                let deps = std::mem::take(&mut self.objects[i].dependencies);
                if effects.is_empty() {
                    self.objects[i].dependencies = deps;
                    continue;
                }
                // Run the chain on a copy of the scene and composite the
                // result back with its alpha.  Effects such as `pulse` with
                // PULSEALPHA fade the whole fullscreen layer in and out, which a
                // plain buffer swap would turn into an opaque (often black)
                // frame.
                let (sw, sh, scene_tex) = {
                    let s = self.scene.as_ref().unwrap();
                    (s.w, s.h, s.tex)
                };
                let canvas = self.pool.take(gl, sw, sh)?;
                let tmp = self.pool.take(gl, sw, sh)?;
                self.blit_subrect_to_canvas(gl, scene_tex, &canvas, [0.0, 0.0], [1.0, 1.0]);
                let mut flip = false;
                {
                    let ctx = EffectContext {
                        time,
                        frametime: dt,
                        daytime,
                        mouse: mouse_uv,
                        audio: info.audio,
                        spectrum: &info.spectrum,
                        projection_inverse: identity4(),
                        composites: &self.composites,
                        self_id: obj_id,
                        dependencies: &deps,
                        props: &self.properties,
                    };
                    for effect in effects.iter_mut() {
                        if !parse_visible(effect.visible_raw.as_ref(), ctx.props, now).eval(ctx.props) {
                            continue;
                        }
                        let (src, dst) = if flip { (&tmp, &canvas) } else { (&canvas, &tmp) };
                        if effect
                            .apply(gl, &self.pkg, &mut self.textures, src, dst, &ctx)
                            .is_ok()
                        {
                            flip = !flip;
                        }
                    }
                }
                self.objects[i].effects = effects;
                self.objects[i].dependencies = deps;
                if flip && is_vis {
                    let final_canvas: &Canvas = &tmp;
                    let scene = self.scene.as_ref().unwrap();
                    unsafe {
                        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(scene.fbo));
                        gl.viewport(0, 0, scene.w, scene.h);
                    }
                    self.draw_layer_canvas(gl, final_canvas, [0.0, 0.0], ortho, blend, color_blend)?;
                } else if is_vis && !flip {
                    // Even number of passes: the result is in `canvas`.
                    let scene = self.scene.as_ref().unwrap();
                    unsafe {
                        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(scene.fbo));
                        gl.viewport(0, 0, scene.w, scene.h);
                    }
                    self.draw_layer_canvas(gl, &canvas, [0.0, 0.0], ortho, blend, color_blend)?;
                }
                self.pool.put(gl, canvas);
                self.pool.put(gl, tmp);
                continue;
            }

            if let ObjectKind::Compose { copy_background } = self.objects[i].kind {
                let (min, max) = self.objects[i].aabb(offset);
                let aabb_size = [(max[0] - min[0]).max(1.0), (max[1] - min[1]).max(1.0)];
                let cw = (aabb_size[0] * px_per_world).ceil().clamp(1.0, 4096.0) as i32;
                let ch = (aabb_size[1] * px_per_world).ceil().clamp(1.0, 4096.0) as i32;
                let canvas = self.pool.take(gl, cw, ch)?;
                let tmp = self.pool.take(gl, cw, ch)?;

                if copy_background {
                    let scene_tex = self.scene.as_ref().unwrap().tex;
                    let u0 = min[0] / ortho_w;
                    let v0 = 1.0 - max[1] / ortho_h;
                    let us = (max[0] - min[0]) / ortho_w;
                    let vs = (max[1] - min[1]) / ortho_h;
                    self.blit_subrect_to_canvas(gl, scene_tex, &canvas, [u0, v0], [us, vs]);
                }

                let mut effects = std::mem::take(&mut self.objects[i].effects);
                let deps = std::mem::take(&mut self.objects[i].dependencies);
                let mut flip = false;
                {
                    let ctx = EffectContext {
                        time,
                        frametime: dt,
                        daytime,
                        mouse: mouse_uv,
                        audio: info.audio,
                        spectrum: &info.spectrum,
                        projection_inverse: layer_projection_inverse(ortho, aabb_size),
                        composites: &self.composites,
                        self_id: obj_id,
                        dependencies: &deps,
                        props: &self.properties,
                    };
                    for effect in effects.iter_mut() {
                        if !parse_visible(effect.visible_raw.as_ref(), ctx.props, now).eval(ctx.props) {
                            continue;
                        }
                        let (src, dst) = if flip { (&tmp, &canvas) } else { (&canvas, &tmp) };
                        if effect
                            .apply(gl, &self.pkg, &mut self.textures, src, dst, &ctx)
                            .is_ok()
                        {
                            flip = !flip;
                        }
                    }
                }
                self.objects[i].effects = effects;
                self.objects[i].dependencies = deps;

                let final_canvas: &Canvas = if flip { &tmp } else { &canvas };
                if capture_comp {
                    if let Some(id) = obj_id {
                        let snap = match self.composites.remove(&id) {
                            Some(c) if c.w == final_canvas.w && c.h == final_canvas.h => c,
                            Some(old) => {
                                old.destroy(gl);
                                Canvas::new(gl, final_canvas.w, final_canvas.h)?
                            }
                            None => Canvas::new(gl, final_canvas.w, final_canvas.h)?,
                        };
                        self.blit_subrect_to_canvas(
                            gl,
                            final_canvas.tex,
                            &snap,
                            [0.0, 0.0],
                            [1.0, 1.0],
                        );
                        self.composites.insert(id, snap);
                    }
                }

                if is_vis {
                    let scene = self.scene.as_ref().unwrap();
                    unsafe {
                        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(scene.fbo));
                        gl.viewport(0, 0, scene.w, scene.h);
                    }
                    self.draw_layer_canvas(gl, final_canvas, min, max, blend, color_blend)?;
                }

                self.pool.put(gl, canvas);
                self.pool.put(gl, tmp);
                continue;
            }

            if let ObjectKind::CompositeRef(ref_id) = self.objects[i].kind {
                let Some(ref_tex) = self.composites.get(&ref_id).map(|c| c.tex) else {
                    continue;
                };
                let (min, max) = self.objects[i].aabb(offset);
                let aabb_size = [(max[0] - min[0]).max(1.0), (max[1] - min[1]).max(1.0)];
                let has_effects = !self.objects[i].effects.is_empty();
                if !has_effects {
                    if is_vis {
                        let scene = self.scene.as_ref().unwrap();
                        unsafe {
                            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(scene.fbo));
                            gl.viewport(0, 0, scene.w, scene.h);
                        }
                        let mut transform = self.objects[i].transform;
                        transform.tx += offset[0];
                        transform.ty += offset[1];
                        let transform = self.objects[i].quad_transform(transform);
                        let draw = DrawInfo {
                            kind: DrawKind::Image(ref_tex),
                            size: self.objects[i].size,
                            color: self.objects[i].color,
                            brightness: self.objects[i].brightness,
                            uv_rect: [0.0, 0.0, 1.0, 1.0],
                            premul: is_premul_blend(blend),
                        };
                        apply_blend(gl, blend);
                        self.draw_content(gl, &draw, transform, [0.0, 0.0], ortho);
                    }
                    continue;
                }

                let cw = (aabb_size[0] * px_per_world).ceil().clamp(1.0, 4096.0) as i32;
                let ch = (aabb_size[1] * px_per_world).ceil().clamp(1.0, 4096.0) as i32;
                let canvas = self.pool.take(gl, cw, ch)?;
                let tmp = self.pool.take(gl, cw, ch)?;
                self.blit_subrect_to_canvas(gl, ref_tex, &canvas, [0.0, 0.0], [1.0, 1.0]);

                let mut effects = std::mem::take(&mut self.objects[i].effects);
                let deps = std::mem::take(&mut self.objects[i].dependencies);
                let mut flip = false;
                {
                    let ctx = EffectContext {
                        time,
                        frametime: dt,
                        daytime,
                        mouse: mouse_uv,
                        audio: info.audio,
                        spectrum: &info.spectrum,
                        projection_inverse: layer_projection_inverse(ortho, aabb_size),
                        composites: &self.composites,
                        self_id: obj_id,
                        dependencies: &deps,
                        props: &self.properties,
                    };
                    for effect in effects.iter_mut() {
                        if !parse_visible(effect.visible_raw.as_ref(), ctx.props, now).eval(ctx.props) {
                            continue;
                        }
                        let (src, dst) = if flip { (&tmp, &canvas) } else { (&canvas, &tmp) };
                        if effect
                            .apply(gl, &self.pkg, &mut self.textures, src, dst, &ctx)
                            .is_ok()
                        {
                            flip = !flip;
                        }
                    }
                }
                self.objects[i].effects = effects;
                self.objects[i].dependencies = deps;

                let final_canvas: &Canvas = if flip { &tmp } else { &canvas };
                if is_vis {
                    let scene = self.scene.as_ref().unwrap();
                    unsafe {
                        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(scene.fbo));
                        gl.viewport(0, 0, scene.w, scene.h);
                    }
                    self.draw_layer_canvas(gl, final_canvas, min, max, blend, color_blend)?;
                }
                self.pool.put(gl, canvas);
                self.pool.put(gl, tmp);
                continue;
            }

            let mut transform = self.objects[i].transform;
            transform.tx += offset[0];
            transform.ty += offset[1];
            let quad_transform = self.objects[i].quad_transform(transform);
            let has_effects = !self.objects[i].effects.is_empty();
            let mut draw = self.objects[i].draw_info();
            draw.uv_rect = self.objects[i].sprite_rect(time);
            if std::env::var_os("ANIMA_TRACE_SPRITE").is_some()
                && self.objects[i].sprite.is_some()
            {
                log::info!(
                    "sprite '{}' t={:.3} uv={:?}",
                    self.objects[i].name,
                    time,
                    draw.uv_rect
                );
            }
            let puppet_info = match &self.objects[i].kind {
                ObjectKind::Puppet {
                    texture,
                    tex_size,
                    bounds,
                    ..
                } => Some((*texture, *tex_size, *bounds)),
                _ => None,
            };

            if let Some((raw_tex, tex_size, bounds)) = puppet_info {
                let mut effect_canvases: Option<(Canvas, Canvas, bool)> = None;
                if has_effects {
                    let tex_world = [tex_size[0].max(1) as f32, tex_size[1].max(1) as f32];
                    let eff_sx = (transform.a * transform.a + transform.b * transform.b)
                        .sqrt()
                        .max(1.0);
                    let eff_sy = (transform.c * transform.c + transform.d * transform.d)
                        .sqrt()
                        .max(1.0);
                    let cw = (tex_world[0] * eff_sx * px_per_world)
                        .ceil()
                        .clamp(1.0, 4096.0) as i32;
                    let ch = (tex_world[1] * eff_sy * px_per_world)
                        .ceil()
                        .clamp(1.0, 4096.0) as i32;
                    let canvas = self.pool.take(gl, cw, ch)?;
                    let tmp = self.pool.take(gl, cw, ch)?;
                    unsafe {
                        canvas.bind(gl);
                        gl.clear_color(0.0, 0.0, 0.0, 0.0);
                        gl.clear(glow::COLOR_BUFFER_BIT);
                        gl.disable(glow::BLEND);
                    }
                    let raw_draw = DrawInfo {
                        kind: DrawKind::Image(raw_tex),
                        size: tex_world,
                        color: [1.0, 1.0, 1.0, 1.0],
                        brightness: 1.0,
                        uv_rect: [0.0, 0.0, 1.0, 1.0],
                        premul: false,
                    };
                    self.draw_content(
                        gl,
                        &raw_draw,
                        Affine::identity(),
                        [-tex_world[0] * 0.5, -tex_world[1] * 0.5],
                        tex_world,
                    );
                    let flip = self.run_object_effects(
                        gl,
                        i,
                        &canvas,
                        &tmp,
                        time,
                        dt,
                        daytime,
                        now,
                        mouse_uv,
                        info.audio,
                        &info.spectrum,
                        layer_projection_inverse(ortho, tex_world),
                        obj_id,
                    );
                    let eff_tex = if flip { tmp.tex } else { canvas.tex };
                    if let DrawKind::Puppet {
                        ref mut texture, ..
                    } = draw.kind
                    {
                        *texture = eff_tex;
                    }
                    effect_canvases = Some((canvas, tmp, flip));
                }

                if !capture_comp && (color_blend == 0 || color_blend == 31) {
                    if is_vis {
                        let scene = self.scene.as_ref().unwrap();
                        unsafe {
                            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(scene.fbo));
                            gl.viewport(0, 0, scene.w, scene.h);
                        }
                        apply_blend(gl, blend);
                        draw.premul = is_premul_blend(blend);
                        self.draw_content(gl, &draw, transform, [0.0, 0.0], ortho);
                    }
                } else {
                    let (pmin_raw, pmax_raw) = transformed_aabb(transform, bounds.0, bounds.1);
                    let pmin = [pmin_raw[0] - 2.0, pmin_raw[1] - 2.0];
                    let pmax = [pmax_raw[0] + 2.0, pmax_raw[1] + 2.0];
                    let psize = [(pmax[0] - pmin[0]).max(1.0), (pmax[1] - pmin[1]).max(1.0)];
                    let cw = (psize[0] * px_per_world).ceil().clamp(1.0, 4096.0) as i32;
                    let ch = (psize[1] * px_per_world).ceil().clamp(1.0, 4096.0) as i32;
                    let out_canvas = self.pool.take(gl, cw, ch)?;
                    unsafe {
                        out_canvas.bind(gl);
                        gl.clear_color(0.0, 0.0, 0.0, 0.0);
                        gl.clear(glow::COLOR_BUFFER_BIT);
                        gl.enable(glow::BLEND);
                        gl.blend_func_separate(
                            glow::SRC_ALPHA,
                            glow::ONE_MINUS_SRC_ALPHA,
                            glow::ONE,
                            glow::ONE_MINUS_SRC_ALPHA,
                        );
                    }
                    draw.premul = false;
                    self.draw_content(gl, &draw, transform, pmin, psize);
                    if capture_comp {
                        if let Some(id) = obj_id {
                            self.capture_layer_composite(gl, id, &out_canvas)?;
                        }
                    }
                    if is_vis {
                        let scene = self.scene.as_ref().unwrap();
                        unsafe {
                            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(scene.fbo));
                            gl.viewport(0, 0, scene.w, scene.h);
                        }
                        self.draw_layer_canvas(gl, &out_canvas, pmin, pmax, blend, color_blend)?;
                    }
                    self.pool.put(gl, out_canvas);
                }

                if let Some((canvas, tmp, _)) = effect_canvases {
                    self.pool.put(gl, canvas);
                    self.pool.put(gl, tmp);
                }
                continue;
            }

            if !has_effects && !capture_comp && color_blend == 0 {
                if is_vis {
                    let scene = self.scene.as_ref().unwrap();
                    unsafe {
                        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(scene.fbo));
                        gl.viewport(0, 0, scene.w, scene.h);
                    }
                    apply_blend(gl, blend);
                    draw.premul = is_premul_blend(blend);
                    self.draw_content(gl, &draw, quad_transform, [0.0, 0.0], ortho);
                }
                continue;
            }

            let local_size = [
                self.objects[i].size[0].abs().max(1.0),
                self.objects[i].size[1].abs().max(1.0),
            ];
            // Layer effects run at the layer's own (texture) resolution in
            // Wallpaper Engine.  Our canvas is sized in world units, so the
            // output density must also include the object's world scale, or a
            // scaled-up layer gets an undersized effect buffer and every blur
            // radius ends up twice as soft as authored.
            let t = &self.objects[i].transform;
            let density_x = (t.a.hypot(t.b) * px_per_world).clamp(1.0, 64.0);
            let density_y = (t.c.hypot(t.d) * px_per_world).clamp(1.0, 64.0);
            let cw = (local_size[0] * density_x)
                .ceil()
                .clamp(1.0, 4096.0) as i32;
            let ch = (local_size[1] * density_y)
                .ceil()
                .clamp(1.0, 4096.0) as i32;
            let canvas = self.pool.take(gl, cw, ch)?;
            let tmp = self.pool.take(gl, cw, ch)?;

            unsafe {
                canvas.bind(gl);
                gl.clear_color(0.0, 0.0, 0.0, 0.0);
                gl.clear(glow::COLOR_BUFFER_BIT);
                gl.disable(glow::BLEND);
            }
            self.draw_content(
                gl,
                &draw,
                Affine::identity(),
                [-local_size[0] * 0.5, -local_size[1] * 0.5],
                local_size,
            );

            let flip = if has_effects {
                self.run_object_effects(
                    gl,
                    i,
                    &canvas,
                    &tmp,
                    time,
                    dt,
                    daytime,
                    now,
                    mouse_uv,
                    info.audio,
                    &info.spectrum,
                    layer_projection_inverse(ortho, local_size),
                    obj_id,
                )
            } else {
                false
            };

            let final_canvas: &Canvas = if flip { &tmp } else { &canvas };
            if capture_comp {
                if let Some(id) = obj_id {
                    self.capture_layer_composite(gl, id, final_canvas)?;
                }
            }

            if is_vis {
                let scene = self.scene.as_ref().unwrap();
                unsafe {
                    gl.bind_framebuffer(glow::FRAMEBUFFER, Some(scene.fbo));
                    gl.viewport(0, 0, scene.w, scene.h);
                }
                let origin = [quad_transform.tx, quad_transform.ty];
                let axis_x = quad_transform.apply_vec([local_size[0] * 0.5, 0.0]);
                let axis_y = quad_transform.apply_vec([0.0, local_size[1] * 0.5]);
                self.draw_layer_canvas_transformed(
                    gl,
                    final_canvas,
                    origin,
                    axis_x,
                    axis_y,
                    blend,
                    color_blend,
                )?;
            }
            self.pool.put(gl, canvas);
            self.pool.put(gl, tmp);
        }

        // Blit the composed scene to the surface with aspect-ratio cover scaling.
        {
            let scene = self.scene.as_ref().unwrap();
            unsafe {
                gl.bind_framebuffer(glow::FRAMEBUFFER, None);
                gl.viewport(0, 0, self.width.max(1), self.height.max(1));
                gl.disable(glow::BLEND);
                gl.disable(glow::DEPTH_TEST);
                let p = &mut self.copy_prog;
                p.bind(gl);
                p.set_vec2(gl, "u_uv_scale", cover_scale[0], cover_scale[1]);
                p.set_vec2(gl, "u_uv_offset", cover_offset[0], cover_offset[1]);
                p.set_f32(gl, "u_flip_y", 1.0);
                p.set_vec4(gl, "u_bcs_hue", self.bcs_hue);
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
        for sound in &mut self.sounds {
            sound.player.set_paused(paused);
        }
    }

    fn set_volume(&mut self, volume: f32) {
        self.volume = volume.clamp(0.0, 1.0);
        for sound in &mut self.sounds {
            sound
                .player
                .set_volume((self.volume * sound.base_volume).clamp(0.0, 1.0));
        }
    }

    fn set_property(&mut self, key: &str, value: &str) -> Result<()> {
        match key.to_ascii_lowercase().as_str() {
            "mute" => {
                self.muted = matches!(
                    value.trim().to_ascii_lowercase().as_str(),
                    "1" | "true" | "yes" | "on"
                );
                for sound in &mut self.sounds {
                    sound.player.set_muted(self.muted);
                }
            }
            "volume" => {
                if let Ok(v) = value.parse::<f32>() {
                    let norm = if v > 1.0 { v / 100.0 } else { v };
                    self.set_volume(norm);
                }
            }
            _ => self.apply_property(key, value),
        }
        Ok(())
    }

    fn uses_audio(&self) -> bool {
        self.emitters.iter().any(|e| e.audio_reactive)
            || self
                .objects
                .iter()
                .any(|o| o.effects.iter().any(|e| e.uses_audio()))
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
        // Stop scene audio before releasing GL resources.
        self.sounds.clear();
        self.sound_specs.clear();
        if let Some(canvas) = self.scene.take() {
            canvas.destroy(gl);
        }
        if let Some(canvas) = self.scene_alt.take() {
            canvas.destroy(gl);
        }
        if let Some(canvas) = self.scene_copy.take() {
            canvas.destroy(gl);
        }
        for (_, canvas) in self.composites.drain() {
            canvas.destroy(gl);
        }
        self.pool.clear(gl);
        self.textures.clear(gl);
        for obj in &mut self.objects {
            for effect in std::mem::take(&mut obj.effects) {
                effect.destroy(gl);
            }
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
        self.blend_prog.destroy(gl);
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
            LayerBlend::Normal => gl.blend_func_separate(
                glow::SRC_ALPHA,
                glow::ONE_MINUS_SRC_ALPHA,
                glow::ONE,
                glow::ONE_MINUS_SRC_ALPHA,
            ),
            LayerBlend::Additive => {
                gl.blend_func_separate(glow::SRC_ALPHA, glow::ONE, glow::ZERO, glow::ONE)
            }
            LayerBlend::Multiply => gl.blend_func_separate(
                glow::DST_COLOR,
                glow::ONE_MINUS_SRC_ALPHA,
                glow::ZERO,
                glow::ONE,
            ),
            LayerBlend::Screen => gl.blend_func_separate(
                glow::ONE,
                glow::ONE_MINUS_SRC_COLOR,
                glow::ZERO,
                glow::ONE,
            ),
        }
    }
}

fn is_premul_blend(mode: LayerBlend) -> bool {
    matches!(mode, LayerBlend::Screen | LayerBlend::Multiply)
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
        let stride = 14 * 4;
        gl.enable_vertex_attrib_array(1);
        gl.vertex_attrib_pointer_f32(1, 2, glow::FLOAT, false, stride, 0);
        gl.vertex_attrib_divisor(1, 1);
        gl.enable_vertex_attrib_array(2);
        gl.vertex_attrib_pointer_f32(2, 4, glow::FLOAT, false, stride, 8);
        gl.vertex_attrib_divisor(2, 1);
        gl.enable_vertex_attrib_array(3);
        gl.vertex_attrib_pointer_f32(3, 4, glow::FLOAT, false, stride, 24);
        gl.vertex_attrib_divisor(3, 1);
        gl.enable_vertex_attrib_array(4);
        gl.vertex_attrib_pointer_f32(4, 4, glow::FLOAT, false, stride, 40);
        gl.vertex_attrib_divisor(4, 1);
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

fn layer_projection_inverse(ortho: [f32; 2], layer_size: [f32; 2]) -> [f32; 16] {
    let sx = ortho[0] / layer_size[0].max(1.0);
    let sy = ortho[1] / layer_size[1].max(1.0);
    [
        sx,  0.0, 0.0, 0.0,
        0.0, sy,  0.0, 0.0,
        0.0, 0.0, 1.0, 0.0,
        0.0, 0.0, 0.0, 1.0,
    ]
}

fn mdl_bounds(verts: &[MeshVertex]) -> ([f32; 2], [f32; 2]) {
    let Some(first) = verts.first() else {
        return ([0.0, 0.0], [0.0, 0.0]);
    };
    let mut bmin = first.pos;
    let mut bmax = first.pos;
    for v in &verts[1..] {
        bmin[0] = bmin[0].min(v.pos[0]);
        bmin[1] = bmin[1].min(v.pos[1]);
        bmax[0] = bmax[0].max(v.pos[0]);
        bmax[1] = bmax[1].max(v.pos[1]);
    }
    (bmin, bmax)
}

fn transformed_aabb(transform: Affine, bmin: [f32; 2], bmax: [f32; 2]) -> ([f32; 2], [f32; 2]) {
    let corners = [
        transform.apply([bmin[0], bmin[1]]),
        transform.apply([bmax[0], bmin[1]]),
        transform.apply([bmin[0], bmax[1]]),
        transform.apply([bmax[0], bmax[1]]),
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

impl Drop for SceneRenderer {
    fn drop(&mut self) {
        // Removes audio files extracted from a `.pkg`, if any.
        if let Some(dir) = self.sound_temp_dir.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
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
#[allow(dead_code)]
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample_time(hour: u32, minute: u32) -> text::LocalTime {
        text::LocalTime {
            year: 2026,
            month: 11,
            day: 14,
            weekday: 6,
            hour,
            minute,
            second: 0,
        }
    }

    #[test]
    fn resolve_prop_handles_string_conditional_and_nested_value() {
        let mut props = HashMap::new();
        props.insert("color".to_string(), json!("0 1 0"));
        props.insert("format".to_string(), json!("2"));
        props.insert("inner_speed".to_string(), json!(4.5));

        // Plain user string binding
        assert_eq!(
            resolve_prop(&json!({"user": "color", "value": "1 1 1"}), &props),
            json!("0 1 0")
        );

        // Conditional user object binding
        let cond_prop = json!({"user": {"condition": "1", "name": "format"}, "value": true});
        assert_eq!(resolve_prop(&cond_prop, &props), json!(false));
        props.insert("format".to_string(), json!("1"));
        assert_eq!(resolve_prop(&cond_prop, &props), json!(true));

        // Nested value object with inner user binding
        let nested = json!({"value": {"user": "inner_speed", "value": 1.0}});
        let resolved = resolve_prop(&nested, &props);
        assert_eq!(resolved.get("value"), Some(&json!(4.5)));
    }

    #[test]
    fn eval_origin_field_resolves_canvas_normalized_and_slider_scripts() {
        let mut props = HashMap::new();
        props.insert("x8".to_string(), json!(0.279));
        props.insert("y6".to_string(), json!(0.623));
        props.insert("x1".to_string(), json!(2045.0));
        props.insert("y".to_string(), json!(1794.0));
        props.insert("xfps".to_string(), json!(1170.0));
        props.insert("fps".to_string(), json!(280.0));

        // Canvas-normalized origin (e.g. 3243449890)
        let norm = json!({
            "script": "export function update(value) {\n  value.x = scriptProperties.x * engine.canvasSize.x;\n  value.y = scriptProperties.y * engine.canvasSize.y;\n  return value;\n}",
            "scriptproperties": {
                "x": {"user": "x8", "value": 0.5},
                "y": {"user": "y6", "value": 0.5}
            },
            "value": "0 0 0"
        });
        let p = eval_origin_field(&norm, [0.0; 3], [3840.0, 2160.0], &props);
        assert!((p[0] - 0.279 * 3840.0).abs() < 0.01);
        assert!((p[1] - 0.623 * 2160.0).abs() < 0.01);

        // Direct pixel sliders (e.g. 2802243144 & 3432999904)
        let slider = json!({
            "script": "export function update(value) {\n  value.x = scriptProperties.newSlider\n  value.y = scriptProperties.newSlider2\n  return value;\n}",
            "scriptproperties": {
                "newSlider": {"user": "x1", "value": 2157},
                "newSlider2": {"user": "y", "value": 50}
            },
            "value": "2045 1794 0"
        });
        let p2 = eval_origin_field(&slider, [0.0; 3], [3840.0, 2160.0], &props);
        assert!((p2[0] - 2045.0).abs() < 0.01);
        assert!((p2[1] - 1794.0).abs() < 0.01);

        let direct_xy = json!({
            "script": "export function update(value) {\n  value.x = scriptProperties.x\n  value.y = scriptProperties.y\n  return value;\n}",
            "scriptproperties": {
                "x": {"user": "xfps", "value": 50},
                "y": {"user": "fps", "value": 50}
            },
            "value": "1170 300 0"
        });
        let p3 = eval_origin_field(&direct_xy, [0.0; 3], [2340.0, 1080.0], &props);
        assert!((p3[0] - 1170.0).abs() < 0.01);
        assert!((p3[1] - 280.0).abs() < 0.01);
    }

    #[test]
    fn eval_alpha_field_handles_fade_hours_and_sunrise_darkness() {
        let props = HashMap::new();
        // 3340707146 night background: wraps around midnight (18..19 fade in, 6..7 fade out)
        let night_fade = json!({
            "script": "'use strict';\nconst FADE_IN_START_HOUR = 18;\nconst FADE_IN_END_HOUR = 19;\nconst FADE_OUT_START_HOUR = 6;\nconst FADE_OUT_END_HOUR = 7;\nconst BLEND_DURATION = 0.3;\nexport function update(value) { return 1.0; }\n",
            "value": 1.0
        });
        assert!((eval_alpha_field(Some(&night_fade), &props, sample_time(12, 0)) - 0.0).abs() < 1e-4);
        assert!((eval_alpha_field(Some(&night_fade), &props, sample_time(22, 0)) - 1.0).abs() < 1e-4);
        assert!((eval_alpha_field(Some(&night_fade), &props, sample_time(2, 0)) - 1.0).abs() < 1e-4);
        let dusk = eval_alpha_field(Some(&night_fade), &props, sample_time(18, 30));
        assert!(dusk > 0.1 && dusk < 0.99);

        // 3806256988 night darkness overlay
        let sunrise_dark = json!({
            "script": "const DAY_BRIGHTNESS = 1.00;\nconst NIGHT_BRIGHTNESS = 0.45;\nconst SUNRISE_START = 6.0;\nconst SUNRISE_END   = 8.0;\nconst SUNSET_START = 18.0;\nconst SUNSET_END   = 22.0;\nconst darkness = 1.0 - brightness;",
            "value": 0.0
        });
        assert!((eval_alpha_field(Some(&sunrise_dark), &props, sample_time(13, 0)) - 0.0).abs() < 1e-4);
        assert!((eval_alpha_field(Some(&sunrise_dark), &props, sample_time(23, 0)) - 0.55).abs() < 1e-4);
    }

    #[test]
    fn parse_visible_handles_time_range_media_hover_and_comments() {
        let mut props = HashMap::new();
        props.insert("time_ctrl".to_string(), json!(true));

        let day_layer = json!({
            "script": "function isLayerVisible(currentTime, startTime, endTime) { return true; }\nlet visibleStart = scriptProperties.visibleStart;\nlet visibleEnd = scriptProperties.visibleEnd;",
            "scriptproperties": {
                "isEnabled": {"user": "time_ctrl", "value": true},
                "reverseMode": true,
                "visibleStart": 9.0,
                "visibleEnd": 18.0
            },
            "value": true
        });
        assert!(parse_visible(Some(&day_layer), &props, sample_time(12, 0)).eval(&props));
        assert!(!parse_visible(Some(&day_layer), &props, sample_time(21, 0)).eval(&props));
        // When isEnabled=false, falls back to reverseMode (true)
        props.insert("time_ctrl".to_string(), json!(false));
        assert!(parse_visible(Some(&day_layer), &props, sample_time(21, 0)).eval(&props));

        // Active media handler hides layer when no session is playing
        let media_active = json!({
            "script": "export function mediaPlaybackChanged(event) {\n  thisLayer.visible = event.state !== MediaPlaybackEvent.PLAYBACK_STOPPED;\n}",
            "value": true
        });
        assert!(!parse_visible(Some(&media_active), &props, sample_time(12, 0)).eval(&props));

        // Commented-out handler (e.g. 3243449890 Album Cover) preserves authored value
        let media_commented = json!({
            "script": "// 'use strict';\n// export function mediaPlaybackChanged(event) {\n//   thisLayer.visible = event.state !== MediaPlaybackEvent.PLAYBACK_STOPPED;\n// }",
            "value": true
        });
        assert!(parse_visible(Some(&media_commented), &props, sample_time(12, 0)).eval(&props));

        // Hover-only UI button stays hidden when cursor isn't hovering
        let hover_btn = json!({
            "script": "export function init() { thisLayer.visible = false; }\nexport function cursorEnter(e) { thisLayer.visible = true; }\nexport function cursorLeave(e) { thisLayer.visible = false; }",
            "value": true
        });
        assert!(!parse_visible(Some(&hover_btn), &props, sample_time(12, 0)).eval(&props));
    }

    #[test]
    fn prop_script_parses_and_evals_spin_z_and_wave_origin_x() {
        let props = HashMap::new();
        let now = sample_time(12, 0);

        let spin = json!({
            "script": "'use strict';\nexport function update(value) {\n  value.z = 0 - (engine.runtime * 20);\n  value.z = value.z % 360;\n  return value;\n}"
        });
        let ps_spin = PropScript::parse(&spin, &props).expect("SpinZ parsed");
        let a = ps_spin.apply([0.0; 3], 1.5, 0.5, now);
        assert!((a[2] - 330.0).abs() < 0.01);

        let wave = json!({
            "script": "export function update(value) {\n  let sine = Math.sin(elapsed);\n  let wave = triangle + (sine - triangle) * scriptProperties.smoothness;\n  if (scriptProperties.invert) wave = -wave;\n  const offsetX = wave * scriptProperties.intensity;\n  return new Vec3(baseOrigin.x + offsetX, baseOrigin.y, baseOrigin.z);\n}",
            "scriptproperties": {
                "intensity": 60.0,
                "speed": 1.0,
                "smoothness": 1.0,
                "invert": true
            }
        });
        let ps_wave = PropScript::parse(&wave, &props).expect("WaveOriginX parsed");
        let o0 = ps_wave.apply([1920.0, 1000.0, 0.0], 0.0, 0.5, now);
        let oq = ps_wave.apply([1920.0, 1000.0, 0.0], std::f32::consts::FRAC_PI_2, 0.5, now);
        assert!((o0[0] - 1920.0).abs() < 0.01);
        assert!((oq[0] - 1860.0).abs() < 0.1);
    }

    #[test]
    fn user_bound_color_and_text_evaluate_dynamically() {
        let color_def = json!({
            "user": "accent_color",
            "value": "0.1 0.2 0.3"
        });
        let mut props = HashMap::new();
        let c_default = eval_color_field(Some(&color_def), &props, [1.0, 1.0, 1.0]);
        assert!((c_default[0] - 0.1).abs() < 1e-4);

        props.insert("accent_color".into(), json!("#ff8000"));
        let c_override = eval_color_field(Some(&color_def), &props, [1.0, 1.0, 1.0]);
        assert!((c_override[0] - 1.0).abs() < 1e-3);
        assert!((c_override[1] - 128.0 / 255.0).abs() < 1e-2);
        assert!((c_override[2] - 0.0).abs() < 1e-4);

        let text_def = json!({
            "user": "greeting",
            "value": "Default Text"
        });
        let obj = json!({ "text": text_def });
        let pkg = PackageFs::default();
        let now = sample_time(12, 0);
        let layer = SceneRenderer::load_text(&pkg, &obj, &text_def, &props, now).unwrap();
        assert_eq!(layer.evaluate(&props, now), "Default Text");

        props.insert("greeting".into(), json!("Updated Label"));
        assert_eq!(layer.evaluate(&props, now), "Updated Label");
    }
}
